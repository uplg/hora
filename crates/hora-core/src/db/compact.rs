//! Offline maintenance behind `hora compact`: the exclusive lock the daemon
//! holds, the size report, retention on demand, and the `VACUUM INTO` + swap
//! that gives the free pages back to the filesystem.
//!
//! SQLite never shrinks its file on its own: rows deleted by retention leave
//! free pages that later inserts reuse, but the file keeps its high-water
//! mark. Rewriting it needs the database to itself, so the daemon holds an
//! advisory lock on `<db>.lock` for as long as it runs, and compaction takes
//! the same lock or refuses. CLI commands hold `<db>.writers.lock` shared
//! while they have the database open, and compaction takes it exclusive, so
//! a command never writes into a file that is being replaced.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use super::Store;
use crate::config::Config;

/// The exclusive advisory lock on `<db>.lock`: held by the daemon for its
/// whole life and by `hora compact` while it rewrites the file. Released when
/// dropped, and by the kernel if the process dies.
#[derive(Debug)]
pub struct DbLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl DbLock {
    /// The lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Why the lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it: a running daemon, or a compaction.
    Busy(PathBuf),
    /// The lock file could not be opened.
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(path) => write!(
                f,
                "{} is held by another Hora process (the daemon, or a running hora compact)",
                path.display()
            ),
            Self::Io(path, err) => write!(f, "cannot open {}: {err}", path.display()),
        }
    }
}

impl std::error::Error for LockError {}

/// Take the exclusive lock on `<database_path>.lock`, without waiting.
/// `Ok(None)` for an in-memory database or a `file:` URI, which no other
/// process can share.
///
/// # Errors
///
/// [`LockError::Busy`] when another process holds it; [`LockError::Io`] when
/// the lock file cannot be created.
pub fn lock_exclusive(database_path: &str) -> Result<Option<DbLock>, LockError> {
    take_lock(database_path, "lock", true)
}

/// Take the lock on `<database_path>.writers.lock`, without waiting: shared
/// for every CLI command that opens the database (several run at once, next
/// to the daemon), exclusive for `hora compact` while it rewrites the file.
/// The daemon's own lock keeps the daemon out of a compaction; this one keeps
/// out the commands that write beside it (`announce`, `event`, `silence`...),
/// whose rows would otherwise land in the file about to be swapped away.
///
/// # Errors
///
/// [`LockError::Busy`] when the other mode is held (a compaction runs, or a
/// command still has the database open); [`LockError::Io`] when the lock file
/// cannot be created.
pub fn lock_writers(database_path: &str, exclusive: bool) -> Result<Option<DbLock>, LockError> {
    take_lock(database_path, "writers.lock", exclusive)
}

fn take_lock(
    database_path: &str,
    suffix: &str,
    exclusive: bool,
) -> Result<Option<DbLock>, LockError> {
    if database_path == ":memory:" || database_path.starts_with("file:") {
        return Ok(None);
    }
    let path = PathBuf::from(format!("{database_path}.{suffix}"));
    let mut options = std::fs::OpenOptions::new();
    // Never truncated nor removed: deleting a lock file another process has
    // open is how two holders end up locking two different files.
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|err| LockError::Io(path.clone(), err))?;
    let taken = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    match taken {
        Ok(()) => Ok(Some(DbLock { _file: file, path })),
        Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Busy(path)),
        Err(std::fs::TryLockError::Error(err)) => Err(LockError::Io(path, err)),
    }
}

/// The database's size from its header counters: O(1), cheap enough for an
/// operator endpoint refreshed every few seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreSize {
    /// Pages in use plus free pages, in bytes (the file, without its WAL).
    pub db_bytes: u64,
    /// Free pages, in bytes: what `hora compact` gives back at the least.
    pub free_bytes: u64,
}

impl Store {
    /// The size of the database file and of its free pages.
    ///
    /// # Errors
    ///
    /// Whatever the pragmas return.
    pub async fn size(&self) -> super::Result<StoreSize> {
        let (page_size, page_count, freelist): (i64, i64, i64) = sqlx::query_as(
            "SELECT page_size, page_count, freelist_count \
             FROM pragma_page_size, pragma_page_count, pragma_freelist_count",
        )
        .fetch_one(self.sqlx())
        .await?;
        let bytes = |pages: i64| u64::try_from(pages.saturating_mul(page_size)).unwrap_or(0);
        Ok(StoreSize {
            db_bytes: bytes(page_count),
            free_bytes: bytes(freelist),
        })
    }
}

/// One table or index, as `dbstat` measures it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSize {
    pub name: String,
    /// The table it belongs to (itself for a table).
    pub table: String,
    pub index: bool,
    /// Bytes of the pages it occupies.
    pub bytes: u64,
    /// Bytes of those pages holding nothing (fragmentation).
    pub unused_bytes: u64,
}

/// What `hora compact --dry-run` reports.
#[derive(Debug, Clone)]
pub struct SizeReport {
    pub size: StoreSize,
    /// Per table and index, largest first; `None` when this SQLite has no
    /// `dbstat` (the estimate then counts the free pages only).
    pub objects: Option<Vec<ObjectSize>>,
    /// The file size a rewrite should come out at: the pages in use minus
    /// their unused space.
    pub estimated_bytes_after: u64,
}

impl SizeReport {
    /// What a rewrite should give back.
    #[must_use]
    pub fn estimated_gain(&self) -> u64 {
        self.size
            .db_bytes
            .saturating_sub(self.estimated_bytes_after)
    }
}

/// Measure every table and index with `dbstat`. Reads every page of the
/// file, so it takes a while on a large database (it is a read: the daemon
/// is not slowed beyond the I/O).
///
/// # Errors
///
/// Whatever the queries return.
pub async fn size_report(store: &Store) -> super::Result<SizeReport> {
    /// One `dbstat` row per b-tree, with its schema entry.
    #[derive(sqlx::FromRow)]
    struct Row {
        name: String,
        tbl_name: String,
        kind: String,
        pgsize: i64,
        unused: i64,
    }
    let size = store.size().await?;
    let rows: super::Result<Vec<Row>> = sqlx::query_as(
        "SELECT s.name AS name, COALESCE(m.tbl_name, s.name) AS tbl_name, \
                COALESCE(m.type, 'table') AS kind, s.pgsize AS pgsize, s.unused AS unused \
         FROM dbstat AS s LEFT JOIN sqlite_schema AS m ON m.name = s.name \
         WHERE s.aggregate = TRUE \
         ORDER BY s.pgsize DESC",
    )
    .fetch_all(store.sqlx())
    .await;
    let bytes = |n: i64| u64::try_from(n).unwrap_or(0);
    let objects = match rows {
        Ok(rows) => Some(
            rows.into_iter()
                .map(|row| ObjectSize {
                    name: row.name,
                    table: row.tbl_name,
                    index: row.kind == "index",
                    bytes: bytes(row.pgsize),
                    unused_bytes: bytes(row.unused),
                })
                .collect::<Vec<_>>(),
        ),
        // A SQLite built without SQLITE_ENABLE_DBSTAT_VTAB.
        Err(sqlx::Error::Database(err)) if err.message().contains("dbstat") => None,
        Err(err) => return Err(err),
    };
    let unused: u64 = objects
        .iter()
        .flatten()
        .map(|object| object.unused_bytes)
        .sum();
    Ok(SizeReport {
        size,
        estimated_bytes_after: size
            .db_bytes
            .saturating_sub(size.free_bytes)
            .saturating_sub(unused),
        objects,
    })
}

/// Free space on the filesystem holding `path`, when the platform can tell.
#[must_use]
#[allow(
    clippy::useless_conversion,
    reason = "the statvfs fields are 32-bit on some targets"
)]
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let stats = nix::sys::statvfs::statvfs(dir).ok()?;
        Some(u64::from(stats.blocks_available()).saturating_mul(u64::from(stats.fragment_size())))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Apply the retention now instead of at the next maintenance tick: roll the
/// ended hours and days up, prune every table to its window, and sweep the
/// rows of monitors removed from the config - after their 7-day grace, or
/// right away with `purge_removed`.
///
/// # Errors
///
/// Returns an error if a query fails.
pub async fn apply_retention(
    store: &Store,
    config: &Config,
    purge_removed: bool,
) -> anyhow::Result<()> {
    // Offline (the daemon is stopped): nothing waits on the write lock, so
    // the delete slices run back to back.
    super::retention::prune_with(
        store,
        config,
        purge_removed,
        &super::retention::Pace::offline(),
    )
    .await
}

/// How a compaction went.
#[derive(Debug, Clone)]
pub struct Compacted {
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Where the previous file was kept, with `--keep`.
    pub backup: Option<PathBuf>,
    pub elapsed: Duration,
}

/// Rewrite the database into a sibling file with `VACUUM INTO` and swap it in
/// place of the original (an atomic rename), keeping the original as
/// `<db>.bak` when `keep` is set. The caller must hold the [`DbLock`] (proof
/// that no daemon has the file open) and hands over the store, which is
/// closed before the swap.
///
/// The original stays untouched until the rename: a failure (a full disk, a
/// crash) leaves it as it was, plus at worst a stale `<db>.compact` that the
/// next run removes.
///
/// # Errors
///
/// When `<db>.bak` already exists with `keep`, or a step fails.
pub async fn vacuum_swap(
    _lock: &DbLock,
    store: Store,
    database_path: &str,
    keep: bool,
) -> anyhow::Result<Compacted> {
    let started = std::time::Instant::now();
    let path = PathBuf::from(database_path);
    let tmp = PathBuf::from(format!("{database_path}.compact"));
    let backup = PathBuf::from(format!("{database_path}.bak"));
    anyhow::ensure!(
        !keep || !backup.exists(),
        "{} already exists; move it away first",
        backup.display()
    );
    let bytes_before = store.size().await?.db_bytes;

    // Ours: a previous run that died mid-copy (the lock proves none runs now).
    if tmp.exists() {
        std::fs::remove_file(&tmp).with_context(|| format!("removing stale {}", tmp.display()))?;
    }
    let tmp_text = tmp.to_string_lossy().into_owned();
    super::create_private(&tmp_text).with_context(|| format!("creating {}", tmp.display()))?;
    // Everything in the WAL goes into the main file first, so the copy and
    // the file it replaces agree, and no WAL is left to pair with the wrong
    // file after the swap.
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(store.sqlx())
        .await?;
    let copied = sqlx::query("VACUUM INTO ?")
        .bind(&tmp_text)
        .execute(store.sqlx())
        .await
        .with_context(|| format!("copying into {}", tmp.display()));
    store.close().await;
    if let Err(err) = copied {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    check_copy(&tmp_text).await?;
    std::fs::File::open(&tmp)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("syncing {}", tmp.display()))?;

    if keep {
        std::fs::rename(&path, &backup)
            .with_context(|| format!("keeping the original as {}", backup.display()))?;
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {database_path}"))?;
    // The closed store checkpointed and removed its WAL; anything left
    // belongs to the old file and must not be replayed into the new one.
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{database_path}{suffix}"));
        if sidecar.exists() {
            std::fs::remove_file(&sidecar)
                .with_context(|| format!("removing {}", sidecar.display()))?;
        }
    }
    sync_dir(&path);
    let bytes_after = std::fs::metadata(&path).map_or(0, |meta| meta.len());
    Ok(Compacted {
        bytes_before,
        bytes_after,
        backup: keep.then_some(backup),
        elapsed: started.elapsed(),
    })
}

/// A cheap sanity check of the copy before it replaces the original: it
/// opens, and holds the same schema.
async fn check_copy(path: &str) -> anyhow::Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .with_context(|| format!("opening the copy {path}"))?;
    let tables: super::Result<i64> =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'checks'")
            .fetch_one(&pool)
            .await;
    pool.close().await;
    anyhow::ensure!(
        tables.with_context(|| format!("reading the copy {path}"))? == 1,
        "the copy {path} has no checks table; the original is untouched"
    );
    Ok(())
}

/// Make the renames durable (best effort: not every platform syncs a
/// directory).
fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh on-disk database in its own temp directory.
    fn temp_db(tag: &str) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "hora-compact-{tag}-{}-{}",
            std::process::id(),
            jiff::Timestamp::now().as_nanosecond()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hora.db").to_string_lossy().into_owned();
        (dir, path)
    }

    #[test]
    fn the_lock_is_exclusive_until_dropped() {
        let (dir, path) = temp_db("lock");
        let held = lock_exclusive(&path)
            .unwrap()
            .expect("a file database locks");
        assert!(matches!(lock_exclusive(&path), Err(LockError::Busy(_))));
        drop(held);
        assert!(lock_exclusive(&path).unwrap().is_some());
        assert!(lock_exclusive(":memory:").unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn commands_share_the_writers_lock_and_compaction_excludes_them() {
        let (dir, path) = temp_db("writers");
        // Two commands at once (an announce while an event is recorded).
        let first = lock_writers(&path, false).unwrap().unwrap();
        let second = lock_writers(&path, false).unwrap().unwrap();
        // A compaction waits for both to finish.
        assert!(matches!(lock_writers(&path, true), Err(LockError::Busy(_))));
        drop((first, second));
        let compacting = lock_writers(&path, true).unwrap().unwrap();
        // A command started during the rewrite refuses.
        assert!(matches!(
            lock_writers(&path, false),
            Err(LockError::Busy(_))
        ));
        // The daemon's lock is a different file: commands never wait for it.
        let daemon = lock_exclusive(&path).unwrap().unwrap();
        drop(compacting);
        assert!(lock_writers(&path, false).unwrap().is_some());
        drop(daemon);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn compaction_shrinks_the_file_and_keeps_every_row() {
        let (dir, path) = temp_db("vacuum");
        let store = super::super::connect(&path).await.unwrap();
        // Bulky rows, most then deleted: free pages the file keeps.
        sqlx::query(
            "WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM n \
                                         WHERE value < 4000) \
             INSERT INTO checks (time, monitor_id, status, error) \
             SELECT value, 'm', 0, printf('%.500c', 'x') FROM n",
        )
        .execute(store.sqlx())
        .await
        .unwrap();
        sqlx::query("DELETE FROM checks WHERE time > 100")
            .execute(store.sqlx())
            .await
            .unwrap();
        let report = size_report(&store).await.unwrap();
        assert!(report.size.free_bytes > 1_000_000, "{report:?}");
        assert!(report.objects.as_ref().is_some_and(|objects| {
            objects
                .iter()
                .any(|object| object.name == "checks" && !object.index)
        }));
        assert!(report.estimated_gain() >= report.size.free_bytes);

        let lock = lock_exclusive(&path).unwrap().unwrap();
        let done = vacuum_swap(&lock, store, &path, true).await.unwrap();
        assert!(done.bytes_after < done.bytes_before / 2, "{done:?}");
        let backup = done.backup.expect("--keep keeps the original");
        assert!(backup.exists());
        assert!(!PathBuf::from(format!("{path}.compact")).exists());
        assert!(!PathBuf::from(format!("{path}-wal")).exists());

        // The swapped-in file is the database, rows and schema intact.
        let store = super::super::connect(&path).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checks")
            .fetch_one(store.sqlx())
            .await
            .unwrap();
        assert_eq!(rows, 100);
        assert_eq!(store.size().await.unwrap().free_bytes, 0);
        store.close().await;
        // A second --keep would overwrite the first backup: refused.
        let again = super::super::connect(&path).await.unwrap();
        let error = vacuum_swap(&lock, again, &path, true).await.unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
