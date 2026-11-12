//! How a change to the config file reaches the supervisor.
//!
//! Three sources ask for a re-read, and the content comparison in
//! [`Reloader`] drops every request that changed nothing:
//!
//! - **SIGHUP**, for the operator who wants it now.
//! - **File notifications** (inotify, `FSEvents`, ... through `notify`). The
//!   watch is on the *directory*, never on the file: a deploy tool that writes
//!   a temporary file and renames it over the old one (Ansible's `template`,
//!   most editors) gives the name a new inode, and a watch on the old inode
//!   would hear nothing ever again. The directory keeps its inode, and the
//!   events are filtered by file name, so the `SQLite` files that live next to
//!   the config in the default layout wake nobody. A symlinked config (a
//!   Kubernetes `ConfigMap` is a link into a `..data` directory that is itself
//!   a link, swapped atomically) has every directory of its link chain
//!   watched, and the watches are moved when the chain changes.
//! - **A re-read every [`RECHECK`]**, because notifications cannot be trusted
//!   everywhere: a network filesystem says nothing about a change made on
//!   another machine, some FUSE and overlay setups say nothing at all, and an
//!   event queue can overflow. A missed event then costs at most that long,
//!   on every platform.
//!
//! What no source can fix is a config bind-mounted as a single file: the
//! container holds the inode, and a file replaced on the host is a different
//! one. [`caveats`] finds that (and a network filesystem) in the mount table
//! so the operator is told once, at startup, in `hora check` and in
//! `hora doctor`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, warn};

/// How often the config file is re-read whatever the notifications said. It
/// is the bound on a missed event, and the only way a change arrives where
/// there are no events at all. Reading a few kilobytes of TOML and comparing
/// them with the text last seen, once a minute, costs nothing worth a
/// cheaper test: an inode or a modification time would save the read and be
/// wrong on exactly the filesystems this is for (coarse or cached attributes
/// on NFS, an overlay that reports the lower inode).
pub const RECHECK: Duration = Duration::from_secs(60);

/// How long the requests must have stopped before the file is read: a save is
/// several events, and a file written in place is not read half-way.
const QUIET: Duration = Duration::from_millis(500);

/// The longest a burst of requests may postpone the read. Something that
/// keeps touching the file cannot hold a reload back for ever.
const SETTLE_LIMIT: Duration = Duration::from_secs(5);

/// The longest chain of symbolic links followed from the config path. The
/// kernel gives up at 40 as well; a loop of links ends here.
const MAX_LINKS: usize = 40;

/// The filesystem types on which a change made on another machine may send
/// no notification here. A FUSE filesystem in general is not one of them:
/// most (Unraid's `shfs`, `mergerfs`) pass local writes through.
const NETWORK_FILESYSTEMS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "9p",
    "ceph",
    "afs",
    "lustre",
    "virtiofs",
    "fuse.sshfs",
    "fuse.glusterfs",
    "fuse.rclone",
    "fuse.s3fs",
];

/// A reason live reload works less well than the documentation promises,
/// found in the mount table. Its `Display` is the whole message: what works,
/// what does not, what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caveat {
    /// The config file is a mount of its own (`./config.toml:/etc/hora/config.toml`).
    SingleFileMount { file: PathBuf },
    /// The config file is on a filesystem served by another machine.
    NetworkFilesystem { file: PathBuf, fstype: String },
}

impl fmt::Display for Caveat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleFileMount { file } => {
                let directory = file
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                write!(
                    f,
                    "live reload is limited: {} is bind-mounted as a single file. An edit made \
                     in place is still applied, but once the file is replaced on the host (an \
                     editor or a deploy tool that saves by renaming, such as Ansible's template \
                     module) this container keeps the old file until it restarts, and SIGHUP \
                     re-reads the old one too. Mount the directory instead of the file \
                     (./config:{} rather than ./config/config.toml:{})",
                    file.display(),
                    directory.display(),
                    file.display(),
                )
            }
            Self::NetworkFilesystem { file, fstype } => write!(
                f,
                "live reload is delayed: {} is on a network filesystem ({fstype}), where a \
                 change made on another machine may send no notification. Hora re-reads the \
                 file every {} s, so an edit is applied within about a minute; send SIGHUP to \
                 apply it at once",
                file.display(),
                RECHECK.as_secs(),
            ),
        }
    }
}

/// What the mount table says against live reload of `config_path`. Empty when
/// nothing is wrong, and when nothing can be known: the mount table is read
/// on Linux only, and one that cannot be read is no finding.
#[must_use]
pub fn caveats(config_path: &Path) -> Vec<Caveat> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let file = std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    // The file a symlinked config leads to is the one that is mounted.
    let resolved = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
    mount_caveats(&mountinfo, &file, &resolved)
}

/// Log each of [`caveats`] as a warning. The daemon calls it once at startup
/// and `hora check` while it collects the loader's warnings, so both say the
/// same thing.
pub fn warn_caveats(config_path: &Path) {
    for caveat in caveats(config_path) {
        warn!("{caveat}");
    }
}

/// [`caveats`] for a given mount table: `file` is the config path made
/// absolute, `resolved` the same with its symbolic links followed.
fn mount_caveats(mountinfo: &str, file: &Path, resolved: &Path) -> Vec<Caveat> {
    let mounts: Vec<Mount> = mountinfo.lines().filter_map(parse_mount).collect();
    let mut found = Vec::new();
    if mounts
        .iter()
        .any(|mount| mount.point == file || mount.point == resolved)
    {
        found.push(Caveat::SingleFileMount {
            file: file.to_path_buf(),
        });
    }
    // The filesystem the file is on is the one mounted at the longest mount
    // point that contains it; of several stacked on one point the last line
    // is the one on top (`max_by_key` keeps the last maximum).
    let fstype = mounts
        .iter()
        .filter(|mount| resolved.starts_with(&mount.point))
        .max_by_key(|mount| mount.point.components().count())
        .map(|mount| mount.fstype.as_str());
    if let Some(fstype) = fstype.filter(|fstype| NETWORK_FILESYSTEMS.contains(fstype)) {
        found.push(Caveat::NetworkFilesystem {
            file: file.to_path_buf(),
            fstype: fstype.to_owned(),
        });
    }
    found
}

/// The two fields of a mount this module reads.
#[derive(Debug, PartialEq, Eq)]
struct Mount {
    point: PathBuf,
    fstype: String,
}

/// One line of `/proc/self/mountinfo`:
/// `36 35 98:0 /root /mount/point rw,noatime master:1 - ext4 /dev/sda1 rw`.
/// The mount point is the fifth field; the filesystem type is the first after
/// the lone `-` that ends a variable number of optional fields. A line that
/// does not look like that is skipped.
fn parse_mount(line: &str) -> Option<Mount> {
    let mut fields = line.split(' ');
    let point = fields.nth(4)?;
    // Mount options, then the optional fields, then the separator.
    fields.next()?;
    let fstype = fields.by_ref().skip_while(|field| *field != "-").nth(1)?;
    if fstype.is_empty() {
        return None;
    }
    Some(Mount {
        point: PathBuf::from(unescape(point)),
        fstype: fstype.to_owned(),
    })
}

/// The kernel writes a space, a tab, a newline and a backslash in a mount
/// point as a backslash and three octal digits (`\040`); anything else that
/// follows a backslash is left as it is.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let escaped = (byte == b'\\')
            .then(|| bytes.get(at + 1..at + 4))
            .flatten()
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        if let Some(escaped) = escaped {
            out.push(escaped);
            at += 4;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The supervisor's view of the config file: it waits here for a text that
/// differs from the last one it saw.
pub(crate) struct Reloader {
    path: PathBuf,
    requests: mpsc::Receiver<()>,
    /// The text last handed out (or read at startup). A request that finds
    /// the same text - a touch, a spurious event, the periodic re-read - is
    /// nothing to do, so a noisy source can never spin the supervisor. A text
    /// that failed validation is remembered too: it is refused once, not
    /// every minute.
    seen: String,
    /// When the burst of requests being waited out began. It lives here and
    /// not in [`next_change`](Self::next_change) so that a caller that drops
    /// that future in the middle (a `select!` whose other branch won) does
    /// not lose the request.
    burst: Option<Instant>,
    /// Whether the last read failed, so a file that stays unreadable is
    /// reported once.
    unreadable: bool,
    quiet: Duration,
    settle_limit: Duration,
    /// How many times the file was read after startup.
    #[cfg(test)]
    reads: usize,
}

impl Reloader {
    /// A reloader fed by `requests`, starting from the text the file has now.
    fn new(path: &Path, requests: mpsc::Receiver<()>) -> Self {
        Self {
            path: path.to_path_buf(),
            requests,
            seen: std::fs::read_to_string(path).unwrap_or_default(),
            burst: None,
            unreadable: false,
            quiet: QUIET,
            settle_limit: SETTLE_LIMIT,
            #[cfg(test)]
            reads: 0,
        }
    }

    /// Wait until the config file holds a text that differs from the last one
    /// seen, and return it. `None` once nothing can ask for a reload any more.
    ///
    /// A burst of requests is one read: the file is read when the requests
    /// have been quiet for a moment, so one save (several events) is one
    /// reload and a file being written in place is read when it is whole.
    /// Dropping the future loses nothing; the next call takes over.
    pub(crate) async fn next_change(&mut self) -> Option<String> {
        loop {
            let began = if let Some(began) = self.burst {
                began
            } else {
                self.requests.recv().await?;
                *self.burst.insert(Instant::now())
            };
            while began.elapsed() < self.settle_limit {
                match tokio::time::timeout(self.quiet, self.requests.recv()).await {
                    Ok(Some(())) => {}
                    Ok(None) => return None,
                    Err(_quiet) => break,
                }
            }
            // The read is off the runtime's threads: on a network mount whose
            // server went away it can hang for minutes.
            let path = self.path.clone();
            let read = tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await;
            self.burst = None;
            #[cfg(test)]
            {
                self.reads += 1;
            }
            match read {
                Ok(Ok(raw)) => {
                    self.unreadable = false;
                    if raw != self.seen {
                        self.seen.clone_from(&raw);
                        return Some(raw);
                    }
                }
                Ok(Err(err)) => {
                    if !self.unreadable {
                        warn!(
                            "config reload skipped, cannot read {}: {err}",
                            self.path.display()
                        );
                    }
                    self.unreadable = true;
                }
                // The runtime is shutting down; the caller is about to stop.
                Err(_cancelled) => {}
            }
        }
    }
}

/// Start everything that can ask for a reload of `config_path` - SIGHUP, the
/// file notifications, the periodic re-read - and return where the supervisor
/// waits for the outcome. All of it stops when the [`Reloader`] is dropped.
/// What the mount table says against live reload is logged here, once.
pub(crate) fn start(config_path: &Path) -> Reloader {
    let (tx, rx) = mpsc::channel(8);
    spawn_sighup(tx.clone());

    let caveats = caveats(config_path);
    for caveat in &caveats {
        warn!("{caveat}");
    }
    let how = Follow {
        recheck: RECHECK,
        events: true,
        // A file mounted on its own gets no event through its directory (the
        // directory in the container is not the one the file lives in), but a
        // write in place still reaches a watch on the file itself. Nowhere
        // else is the file watched: its inode is the one thing that changes.
        file_itself: caveats
            .iter()
            .any(|caveat| matches!(caveat, Caveat::SingleFileMount { .. })),
    };
    // The watches are in place before the reloader takes its first look at
    // the file, so a change made in between is announced, not lost.
    let follower = Follower::new(config_path, tx, how);
    let reloader = Reloader::new(config_path, rx);
    tokio::spawn(follower.run());
    reloader
}

#[cfg(unix)]
fn spawn_sighup(tx: mpsc::Sender<()>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(hangup) => hangup,
            Err(err) => {
                warn!("cannot listen for SIGHUP: {err}");
                return;
            }
        };
        loop {
            tokio::select! {
                () = tx.closed() => break,
                received = hangup.recv() => {
                    if received.is_none() || tx.send(()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_sighup(_tx: mpsc::Sender<()>) {}

/// How a [`Follower`] follows the file.
#[derive(Clone, Copy)]
struct Follow {
    /// The period of the unconditional re-read.
    recheck: Duration,
    /// Whether file notifications are used at all.
    events: bool,
    /// Whether the file itself is watched besides its directory.
    file_itself: bool,
}

/// One thing handed to the notification backend: a directory (or the file),
/// with the identity it had when it was looked at. A directory that was
/// replaced at the same path is a different target, so its watch is renewed
/// instead of being left on an inode nobody will touch again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    path: PathBuf,
    identity: Option<(u64, u64)>,
}

impl Target {
    /// The target `path` leads to now; `None` when it does not exist.
    fn at(path: &Path) -> Option<Self> {
        let path = std::fs::canonicalize(path).ok()?;
        let identity = identity(&path);
        Some(Self { path, identity })
    }
}

/// Device and inode: what tells a replaced directory from the one before.
#[cfg(unix)]
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Where to watch and which names matter there, for the config path as the
/// filesystem looks right now.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    targets: Vec<Target>,
    names: HashSet<OsString>,
}

/// Walk from the config path along its symbolic links. Every step gives a
/// directory to watch and a name to listen for in it: the config's own name
/// in its own directory, then, for a link, the name it points to in the
/// directory that holds that, and for a directory that is itself a link (the
/// `..data` of a `ConfigMap`) its name in its parent. Replacing any of those
/// names changes what the config path leads to.
fn plan(config_path: &Path, file_itself: bool) -> Plan {
    let start = std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let mut plan = Plan::default();
    let mut directories: Vec<PathBuf> = Vec::new();
    let mut pending = vec![start.clone()];
    let mut steps = 0;
    while let Some(path) = pending.pop() {
        steps += 1;
        if steps > MAX_LINKS {
            break;
        }
        // No name: the root, or a path that ends in `..`.
        let (Some(name), Some(directory)) = (path.file_name(), path.parent()) else {
            continue;
        };
        plan.names.insert(name.to_owned());
        if let Ok(target) = std::fs::read_link(&path) {
            // `join` keeps an absolute target as it is.
            pending.push(directory.join(target));
        }
        if std::fs::symlink_metadata(directory).is_ok_and(|meta| meta.file_type().is_symlink()) {
            pending.push(directory.to_path_buf());
        }
        directories.push(directory.to_path_buf());
    }
    if file_itself {
        directories.push(start);
    }
    for directory in directories {
        // A directory that does not exist (yet) is found by a later plan.
        if let Some(target) = Target::at(&directory)
            && !plan.targets.contains(&target)
        {
            plan.targets.push(target);
        }
    }
    plan
}

/// What the notification callback needs to tell an event that concerns the
/// config from the rest of what happens in the watched directories.
#[derive(Debug, Default)]
struct Interest {
    names: HashSet<OsString>,
    watched: HashSet<PathBuf>,
}

/// Whether a notification is a reason to re-read the config.
///
/// An event counts when it names one of the files of the link chain, whatever
/// happened to it (created, written, renamed to or from, removed): the next
/// read goes through the path again and finds whatever is there now. An event
/// about any other file of the directory does not - with the database next to
/// the config, every WAL write would otherwise wake the reloader. A watched
/// directory that was itself removed or renamed counts. Reads (access events)
/// never do, or our own read would feed the watcher for ever. A backend error
/// and a "rescan" (the event queue overflowed, events were dropped) count:
/// whatever was lost, reading once more recovers it.
fn concerns(event: &notify::Result<notify::Event>, interest: &Interest) -> bool {
    let Ok(event) = event else { return true };
    if event.need_rescan() {
        return true;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let moved_or_gone = matches!(
        event.kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    event.paths.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| interest.names.contains(name))
            || (moved_or_gone && interest.watched.contains(path))
    })
}

/// The task behind the notifications and the periodic re-read: it owns the
/// watcher, keeps its watches on what the config path leads to, and asks the
/// [`Reloader`] for a read.
struct Follower {
    config_path: PathBuf,
    how: Follow,
    out: mpsc::Sender<()>,
    watcher: Option<RecommendedWatcher>,
    /// Events that concern the config, from the backend's thread.
    events: mpsc::Receiver<()>,
    /// Kept so that `events` stays open when there is no watcher.
    _events_tx: mpsc::Sender<()>,
    interest: Arc<Mutex<Interest>>,
    watched: Vec<Target>,
    /// Whether a watch that could not be placed was already reported.
    complained: bool,
}

impl Follower {
    /// Create the watcher and place the first watches, so that they are in
    /// force when this returns.
    fn new(config_path: &Path, out: mpsc::Sender<()>, how: Follow) -> Self {
        Self::observed(config_path, out, how, |event, concerned| {
            if concerned {
                debug!("config file notification: {event:?}");
            }
        })
    }

    /// [`new`](Self::new), with `observe` called on the backend's thread for
    /// every notification and what was made of it.
    fn observed(
        config_path: &Path,
        out: mpsc::Sender<()>,
        how: Follow,
        mut observe: impl FnMut(&notify::Result<notify::Event>, bool) + Send + 'static,
    ) -> Self {
        let (events_tx, events) = mpsc::channel(16);
        let interest = Arc::new(Mutex::new(Interest::default()));
        let watcher = if how.events {
            let seen = Arc::clone(&interest);
            let tx = events_tx.clone();
            let mut error_logged = false;
            let created =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if let Err(err) = &event
                        && !error_logged
                    {
                        error_logged = true;
                        warn!("config file notifications reported an error ({err}); re-reading");
                    }
                    let concerned =
                        concerns(&event, &seen.lock().unwrap_or_else(PoisonError::into_inner));
                    observe(&event, concerned);
                    if concerned {
                        // A full queue already holds a request: nothing is lost.
                        let _ = tx.try_send(());
                    }
                });
            match created {
                Ok(watcher) => Some(watcher),
                Err(err) => {
                    warn!(
                        "config file notifications are unavailable ({err}): a change is applied \
                         at the next re-read (every {} s), or at once on SIGHUP",
                        how.recheck.as_secs()
                    );
                    None
                }
            }
        } else {
            None
        };
        let mut follower = Self {
            config_path: config_path.to_path_buf(),
            how,
            out,
            watcher,
            events,
            _events_tx: events_tx,
            interest,
            watched: Vec::new(),
            complained: false,
        };
        follower.rewatch(plan(config_path, how.file_itself));
        follower
    }

    /// Until the reloader is gone: on an event that concerns the config and
    /// on every tick, put the watches where the path leads now, then ask for
    /// a read. In that order, so that whatever changes after the read is
    /// announced by a watch that is already on the new directory.
    async fn run(mut self) {
        let mut recheck =
            tokio::time::interval_at(Instant::now() + self.how.recheck, self.how.recheck);
        recheck.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = self.out.closed() => break,
                _ = self.events.recv() => {}
                _ = recheck.tick() => {}
            }
            if self.watcher.is_some() {
                let (path, file_itself) = (self.config_path.clone(), self.how.file_itself);
                // A few `stat` calls, off the runtime's threads like the read.
                if let Ok(plan) =
                    tokio::task::spawn_blocking(move || plan(&path, file_itself)).await
                {
                    self.rewatch(plan);
                }
            }
            // A full queue already holds a request.
            let _ = self.out.try_send(());
        }
    }

    /// Move the watches to `plan`: drop those on what is no longer part of the
    /// chain (or was replaced - a removed directory took its watch with it),
    /// add the missing ones, and tell the callback which names matter now.
    /// Nothing is touched when the plan is the one in force, which is every
    /// time for a config that is a plain file in a directory.
    fn rewatch(&mut self, plan: Plan) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        {
            let mut interest = self.interest.lock().unwrap_or_else(PoisonError::into_inner);
            interest.watched = plan
                .targets
                .iter()
                .map(|target| target.path.clone())
                .collect();
            interest.names = plan.names;
        }
        for old in &self.watched {
            if !plan.targets.contains(old) {
                // Fails when the kernel dropped the watch with the directory.
                let _ = watcher.unwatch(&old.path);
            }
        }
        let mut placed = Vec::with_capacity(plan.targets.len());
        let mut failed = false;
        for target in plan.targets {
            if self.watched.contains(&target) {
                placed.push(target);
                continue;
            }
            match watcher.watch(&target.path, RecursiveMode::NonRecursive) {
                Ok(()) => placed.push(target),
                Err(err) => {
                    failed = true;
                    if !self.complained {
                        warn!(
                            "cannot watch {} ({err}): a change to the config is applied at the \
                             next re-read (every {} s), or at once on SIGHUP",
                            target.path.display(),
                            self.how.recheck.as_secs()
                        );
                    }
                }
            }
        }
        // Tried again at the next event or tick; reported again only after it
        // worked in between.
        self.complained = failed;
        self.watched = placed;
    }
}

#[cfg(test)]
mod tests;
