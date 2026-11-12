use std::sync::atomic::{AtomicUsize, Ordering};

use notify::event::{AccessKind, CreateKind, DataChange, Flag, RemoveKind, RenameMode};

use super::*;

/// How long a test waits for something the filesystem must announce.
/// Generous: `FSEvents` on macOS delays and batches, and a loaded CI machine
/// is slow. A passing test never waits this long.
const PATIENCE: Duration = Duration::from_secs(30);

/// Notifications only: the periodic re-read is far away, so a test that
/// passes was told by the filesystem.
const EVENTS_ONLY: Follow = Follow {
    recheck: Duration::from_secs(3600),
    events: true,
    file_itself: false,
};

/// A directory of its own under the system's temporary one, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hora-config-watch-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What `start` does, without SIGHUP and with a short quiet period.
fn follow(path: &Path, how: Follow) -> Reloader {
    let (tx, rx) = mpsc::channel(8);
    let follower = Follower::new(path, tx, how);
    let reloader = quick(Reloader::new(path, rx));
    tokio::spawn(follower.run());
    reloader
}

fn quick(mut reloader: Reloader) -> Reloader {
    reloader.quiet = Duration::from_millis(50);
    reloader
}

/// The next text the reloader hands out.
async fn change(reloader: &mut Reloader) -> String {
    tokio::time::timeout(PATIENCE, reloader.next_change())
        .await
        .expect("the change was noticed")
        .expect("the request channel is open")
}

/// Replace `path` the way Ansible's `template` module (and most editors) do:
/// a temporary file in the same directory, renamed over the old one.
fn replace_by_rename(path: &Path, content: &str) {
    let tmp = path.with_file_name(".ansible_tmp_config");
    std::fs::write(&tmp, content).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// The owner's deployment: every deploy gives the name a new inode. The
/// second and third replacements are the point - a watch left on the first
/// inode would have heard the first and nothing since.
#[tokio::test]
async fn a_file_replaced_by_rename_is_reloaded_every_time() {
    let dir = Scratch::new("rename");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let mut reloader = follow(&path, EVENTS_ONLY);

    for content in ["two", "three", "four"] {
        replace_by_rename(&path, content);
        assert_eq!(change(&mut reloader).await, content);
    }
    // And the file that came by rename is followed when written in place.
    std::fs::write(&path, "five").unwrap();
    assert_eq!(change(&mut reloader).await, "five");
}

/// Truncate-then-write, which is what `fs::write` (and `>` in a shell) does.
#[tokio::test]
async fn a_file_rewritten_in_place_is_reloaded() {
    let dir = Scratch::new("in-place");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let mut reloader = follow(&path, EVENTS_ONLY);

    std::fs::write(&path, "two").unwrap();
    assert_eq!(change(&mut reloader).await, "two");
    std::fs::write(&path, "three").unwrap();
    assert_eq!(change(&mut reloader).await, "three");
}

/// Removed, then created again: the name is followed, not the file.
#[tokio::test]
async fn a_file_removed_and_created_again_is_reloaded() {
    let dir = Scratch::new("recreate");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let mut reloader = follow(&path, EVENTS_ONLY);

    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "two").unwrap();
    assert_eq!(change(&mut reloader).await, "two");
}

/// The default layout has the database next to the config. Its writes are
/// seen by the backend (the directory is what is watched) and must be judged
/// none of the reloader's business: no request, so no read of the config.
#[tokio::test]
async fn a_sibling_files_writes_do_not_trigger_a_read() {
    let dir = Scratch::new("sibling");
    let path = dir.path("config.toml");
    let sibling = dir.path("hora.db-wal");
    std::fs::write(&path, "one").unwrap();

    let (tx, rx) = mpsc::channel(8);
    let (seen_tx, mut seen) = mpsc::unbounded_channel();
    let follower = Follower::observed(&path, tx, EVENTS_ONLY, move |event, concerned| {
        if let Ok(event) = event {
            let _ = seen_tx.send((event.paths.clone(), concerned));
        }
    });
    let mut reloader = quick(Reloader::new(&path, rx));
    tokio::spawn(follower.run());

    for round in 0..20 {
        std::fs::write(&sibling, format!("frame {round}")).unwrap();
    }

    // Notifications come in order, so once the sibling's is here everything
    // before it has been judged. macOS may still deliver the creation of
    // config.toml above, from before the watch: that one does concern us.
    let mut concerned_before = 0;
    loop {
        let (paths, concerned) = tokio::time::timeout(PATIENCE, seen.recv())
            .await
            .expect("the backend reported the sibling's write")
            .expect("the watcher is alive");
        if paths
            .iter()
            .any(|path| path.file_name() == sibling.file_name())
        {
            assert!(
                !concerned,
                "a sibling's write asked for a reload: {paths:?}"
            );
            break;
        }
        concerned_before += usize::from(concerned);
    }

    // Long enough for a request, had there been one, to become a read.
    let waited = tokio::time::timeout(Duration::from_millis(400), reloader.next_change()).await;
    assert!(waited.is_err(), "nothing changed in the config");
    assert!(
        reloader.reads <= concerned_before,
        "{} read(s) of the config for {concerned_before} event(s) about it",
        reloader.reads
    );
    if cfg!(target_os = "linux") {
        // inotify reports nothing from before the watch: strictly no read.
        assert_eq!(reloader.reads, 0);
    }
}

/// A Kubernetes `ConfigMap` volume: `config.toml` is a link to
/// `..data/config.toml`, `..data` a link to a timestamped directory, and an
/// update creates a new directory and renames a new `..data` over the old
/// one. The file's own directory entry never changes.
#[cfg(unix)]
#[tokio::test]
async fn a_configmap_style_symlink_swap_is_reloaded() {
    use std::os::unix::fs::symlink;

    let dir = Scratch::new("configmap");
    let publish = |version: &str, content: &str| {
        let data = dir.path(&format!("..{version}"));
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("config.toml"), content).unwrap();
        let tmp = dir.path("..data_tmp");
        symlink(format!("..{version}"), &tmp).unwrap();
        std::fs::rename(&tmp, dir.path("..data")).unwrap();
    };
    publish("v1", "one");
    let path = dir.path("config.toml");
    symlink("..data/config.toml", &path).unwrap();
    let mut reloader = follow(&path, EVENTS_ONLY);

    publish("v2", "two");
    assert_eq!(change(&mut reloader).await, "two");
    // The kubelet removes the old directory, and with it any watch on it.
    std::fs::remove_dir_all(dir.path("..v1")).unwrap();

    publish("v3", "three");
    assert_eq!(change(&mut reloader).await, "three");
    std::fs::remove_dir_all(dir.path("..v2")).unwrap();

    // The watches moved with the links: a write in the directory now in use
    // is heard, which only a watch on that directory can do.
    std::fs::write(dir.path("..v3/config.toml"), "four").unwrap();
    assert_eq!(change(&mut reloader).await, "four");
}

/// No notifications at all - a network filesystem, a FUSE mount that sends
/// none: the periodic re-read alone finds the change.
#[tokio::test]
async fn the_periodic_reread_finds_a_change_nothing_announced() {
    let dir = Scratch::new("periodic");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, rx) = mpsc::channel(8);
    let follower = Follower::new(
        &path,
        tx,
        Follow {
            // Longer than the quiet period, as in production: ticks closer
            // together than that would be one burst that never ends.
            recheck: Duration::from_millis(150),
            events: false,
            file_itself: false,
        },
    );
    assert!(follower.watcher.is_none() && follower.watched.is_empty());
    let mut reloader = quick(Reloader::new(&path, rx));
    tokio::spawn(follower.run());

    replace_by_rename(&path, "two");
    assert_eq!(change(&mut reloader).await, "two");
    std::fs::write(&path, "three").unwrap();
    assert_eq!(change(&mut reloader).await, "three");
}

/// The follower stops, and its watcher with it, when the supervisor drops
/// the reloader.
#[tokio::test]
async fn dropping_the_reloader_stops_the_follower() {
    let dir = Scratch::new("stop");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(Follower::new(&path, tx, EVENTS_ONLY).run());
    drop(rx);
    tokio::time::timeout(PATIENCE, task)
        .await
        .expect("the follower ended")
        .unwrap();
}

/// A reloader fed by hand: the requests are the test's.
fn by_hand(path: &Path) -> (mpsc::Sender<()>, Reloader) {
    let (tx, rx) = mpsc::channel(8);
    (tx, quick(Reloader::new(path, rx)))
}

#[tokio::test]
async fn a_burst_of_requests_is_one_read() {
    let dir = Scratch::new("burst");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, mut reloader) = by_hand(&path);

    std::fs::write(&path, "two").unwrap();
    for _ in 0..5 {
        tx.send(()).await.unwrap();
    }
    assert_eq!(change(&mut reloader).await, "two");
    assert_eq!(reloader.reads, 1);
}

/// A burst that never goes quiet does not postpone the read for ever.
#[tokio::test]
async fn requests_that_never_stop_are_read_at_the_limit() {
    let dir = Scratch::new("limit");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, mut reloader) = by_hand(&path);
    reloader.quiet = PATIENCE;
    reloader.settle_limit = Duration::from_millis(100);

    std::fs::write(&path, "two").unwrap();
    let noise = tokio::spawn(async move {
        while tx.send(()).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert_eq!(change(&mut reloader).await, "two");
    drop(reloader);
    noise.await.unwrap();
}

#[tokio::test]
async fn unchanged_content_is_nothing_to_do() {
    let dir = Scratch::new("unchanged");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, mut reloader) = by_hand(&path);

    // Touched, rewritten with the same text: requests, but no change.
    std::fs::write(&path, "one").unwrap();
    tx.send(()).await.unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(300), reloader.next_change()).await;
    assert!(waited.is_err());

    std::fs::write(&path, "two").unwrap();
    tx.send(()).await.unwrap();
    assert_eq!(change(&mut reloader).await, "two");

    // A text handed out once (applied, or refused by validation) is not
    // handed out again by the next periodic re-read.
    tx.send(()).await.unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(300), reloader.next_change()).await;
    assert!(waited.is_err());
}

/// The supervisor waits in a `select!`: the liveness tick can drop the
/// future in the middle of the quiet period. The request must survive that.
#[tokio::test]
async fn a_dropped_wait_does_not_lose_the_request() {
    let dir = Scratch::new("dropped");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, mut reloader) = by_hand(&path);
    reloader.quiet = Duration::from_millis(200);

    std::fs::write(&path, "two").unwrap();
    tx.send(()).await.unwrap();
    let first = tokio::time::timeout(Duration::from_millis(1), reloader.next_change()).await;
    let raw = match first {
        // Dropped during the quiet period, as intended.
        Err(_elapsed) => change(&mut reloader).await,
        // A machine so slow that 200 ms passed in 1 ms: still the change.
        Ok(raw) => raw.unwrap(),
    };
    assert_eq!(raw, "two");
}

/// A file that cannot be read is no change, and the reloader keeps going.
#[tokio::test]
async fn an_unreadable_file_is_skipped_until_it_is_back() {
    let dir = Scratch::new("unreadable");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, mut reloader) = by_hand(&path);

    std::fs::remove_file(&path).unwrap();
    tx.send(()).await.unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(300), reloader.next_change()).await;
    assert!(waited.is_err());

    std::fs::write(&path, "two").unwrap();
    tx.send(()).await.unwrap();
    assert_eq!(change(&mut reloader).await, "two");
}

#[tokio::test]
async fn the_reloader_ends_when_nothing_can_ask_any_more() {
    let dir = Scratch::new("closed");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let (tx, mut reloader) = by_hand(&path);
    drop(tx);
    assert_eq!(reloader.next_change().await, None);
}

fn names(plan: &Plan) -> Vec<&str> {
    let mut names: Vec<&str> = plan.names.iter().filter_map(|name| name.to_str()).collect();
    names.sort_unstable();
    names
}

fn directories(plan: &Plan) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = plan.targets.iter().map(|t| t.path.clone()).collect();
    paths.sort();
    paths
}

#[test]
fn a_plain_file_is_watched_through_its_directory_only() {
    let dir = Scratch::new("plan-plain");
    let path = dir.path("config.toml");
    std::fs::write(&path, "one").unwrap();
    let real = std::fs::canonicalize(&dir.0).unwrap();

    let plan = plan(&path, false);
    assert_eq!(names(&plan), ["config.toml"]);
    assert_eq!(directories(&plan), std::slice::from_ref(&real));

    // The directory is the same target after the file was replaced: nothing
    // to move, the watch never was on the file.
    let before = plan.targets;
    replace_by_rename(&path, "two");
    assert_eq!(super::plan(&path, false).targets, before);

    // Only a file mounted on its own is watched itself.
    let with_file = super::plan(&path, true);
    assert_eq!(
        directories(&with_file),
        [real.clone(), real.join("config.toml")]
    );
}

/// A directory replaced at the same path is another target (its inode says
/// so), so its watch is renewed.
#[cfg(unix)]
#[test]
fn a_replaced_directory_is_a_new_target() {
    let dir = Scratch::new("plan-replaced");
    let inner = dir.path("conf");
    std::fs::create_dir(&inner).unwrap();
    std::fs::write(inner.join("config.toml"), "one").unwrap();
    let before = plan(&inner.join("config.toml"), false);

    let other = dir.path("conf.new");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("config.toml"), "two").unwrap();
    std::fs::rename(&inner, dir.path("conf.old")).unwrap();
    std::fs::rename(&other, &inner).unwrap();

    let after = plan(&inner.join("config.toml"), false);
    assert_eq!(directories(&before), directories(&after));
    assert_ne!(before.targets, after.targets);
}

#[cfg(unix)]
#[test]
fn a_symlinked_config_is_watched_along_its_chain() {
    use std::os::unix::fs::symlink;

    let dir = Scratch::new("plan-chain");
    std::fs::create_dir(dir.path("..v1")).unwrap();
    std::fs::write(dir.path("..v1/config.toml"), "one").unwrap();
    symlink("..v1", dir.path("..data")).unwrap();
    symlink("..data/config.toml", dir.path("config.toml")).unwrap();
    let real = std::fs::canonicalize(&dir.0).unwrap();

    let plan = plan(&dir.path("config.toml"), false);
    assert_eq!(names(&plan), ["..data", "..v1", "config.toml"]);
    assert_eq!(directories(&plan), [real.clone(), real.join("..v1")]);
}

#[cfg(unix)]
#[test]
fn a_loop_of_links_ends() {
    use std::os::unix::fs::symlink;

    let dir = Scratch::new("plan-loop");
    symlink("b.toml", dir.path("a.toml")).unwrap();
    symlink("a.toml", dir.path("b.toml")).unwrap();
    let plan = plan(&dir.path("a.toml"), false);
    assert_eq!(names(&plan), ["a.toml", "b.toml"]);
    assert_eq!(plan.targets.len(), 1);
}

fn interest() -> Interest {
    Interest {
        names: HashSet::from([OsString::from("config.toml"), OsString::from("..data")]),
        watched: HashSet::from([PathBuf::from("/etc/hora")]),
    }
}

/// Whether an event of `kind` about `path` asks for a reload.
fn about(kind: EventKind, path: &str, interest: &Interest) -> bool {
    let event = notify::Event::new(kind).add_path(PathBuf::from(path));
    concerns(&Ok(event), interest)
}

#[test]
fn only_events_about_the_configs_names_concern_it() {
    let interest = interest();
    let written = EventKind::Modify(ModifyKind::Data(DataChange::Any));
    let renamed_to = EventKind::Modify(ModifyKind::Name(RenameMode::To));
    let created = EventKind::Create(CreateKind::File);
    let removed = EventKind::Remove(RemoveKind::File);

    for kind in [written, renamed_to, created, removed] {
        assert!(about(kind, "/etc/hora/config.toml", &interest), "{kind:?}");
        // The database and its journal, the deploy tool's temporary file.
        for other in ["hora.db", "hora.db-wal", ".ansible_tmp_config"] {
            let path = format!("/etc/hora/{other}");
            assert!(!about(kind, &path, &interest), "{kind:?} {other}");
        }
    }
    // The link a ConfigMap update swaps.
    assert!(about(renamed_to, "/etc/hora/..data", &interest));
    // A rename is reported with both paths: the temporary name and ours.
    let both = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
        .add_path(PathBuf::from("/etc/hora/.ansible_tmp_config"))
        .add_path(PathBuf::from("/etc/hora/config.toml"));
    assert!(concerns(&Ok(both), &interest));
}

#[test]
fn a_read_of_the_config_never_concerns_it() {
    let opened = EventKind::Access(AccessKind::Any);
    assert!(!about(opened, "/etc/hora/config.toml", &interest()));
}

#[test]
fn a_watched_directory_that_goes_away_concerns_it() {
    let interest = interest();
    let gone = EventKind::Remove(RemoveKind::Folder);
    assert!(about(gone, "/etc/hora", &interest));
    // Its attributes changing (a child was created) does not.
    let touched = EventKind::Modify(ModifyKind::Any);
    assert!(!about(touched, "/etc/hora", &interest));
}

/// Dropped events and backend errors name no file: read once more.
#[test]
fn lost_events_and_errors_ask_for_a_read() {
    let interest = interest();
    let overflow = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
    assert!(concerns(&Ok(overflow), &interest));
    assert!(concerns(
        &Err(notify::Error::generic("backend failed")),
        &interest
    ));
    assert!(!concerns(
        &Ok(notify::Event::new(EventKind::Other)),
        &interest
    ));
}

/// The owner's stack: rootless Docker, read-only root, the config directory
/// and the database volume bind-mounted.
const COMPOSE: &str = "\
1502 1480 0:52 / / ro,relatime - overlay overlay rw,lowerdir=/l/a:/l/b,upperdir=/u,workdir=/w
1503 1502 0:55 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
1510 1502 8:2 /home/ops/hora/config /etc/hora rw,relatime master:1 - ext4 /dev/sda2 rw
1511 1502 8:2 /home/ops/.local/share/docker/volumes/hora-data/_data /data rw,relatime - ext4 /dev/sda2 rw
";

fn found(mountinfo: &str, file: &str) -> Vec<Caveat> {
    mount_caveats(mountinfo, Path::new(file), Path::new(file))
}

#[test]
fn a_mounted_directory_has_no_caveat() {
    assert_eq!(found(COMPOSE, "/etc/hora/config.toml"), []);
    // Nor has a file on the container's own filesystem.
    assert_eq!(found(COMPOSE, "/config.toml"), []);
}

#[test]
fn a_file_mounted_on_its_own_is_found() {
    let single = COMPOSE.replace(
        "/home/ops/hora/config /etc/hora ",
        "/home/ops/hora/config.toml /etc/hora/config.toml ",
    );
    let caveats = found(&single, "/etc/hora/config.toml");
    assert_eq!(
        caveats,
        [Caveat::SingleFileMount {
            file: PathBuf::from("/etc/hora/config.toml")
        }]
    );
    assert_eq!(
        caveats[0].to_string(),
        "live reload is limited: /etc/hora/config.toml is bind-mounted as a single file. An \
         edit made in place is still applied, but once the file is replaced on the host (an \
         editor or a deploy tool that saves by renaming, such as Ansible's template module) \
         this container keeps the old file until it restarts, and SIGHUP re-reads the old one \
         too. Mount the directory instead of the file (./config:/etc/hora rather than \
         ./config/config.toml:/etc/hora/config.toml)"
    );

    // A config that is a link to the mounted file is the same case.
    let through_a_link = mount_caveats(
        &single,
        Path::new("/app/config.toml"),
        Path::new("/etc/hora/config.toml"),
    );
    assert!(matches!(
        through_a_link[..],
        [Caveat::SingleFileMount { .. }]
    ));
}

#[test]
fn a_network_filesystem_is_found() {
    let nfs = format!(
        "{COMPOSE}1520 1502 0:61 / /mnt/shared rw,relatime shared:9 master:3 - nfs4 \
         nas.example:/export/hora rw,vers=4.2\n"
    );
    let caveats = found(&nfs, "/mnt/shared/hora/config.toml");
    assert_eq!(
        caveats,
        [Caveat::NetworkFilesystem {
            file: PathBuf::from("/mnt/shared/hora/config.toml"),
            fstype: "nfs4".to_owned()
        }]
    );
    assert_eq!(
        caveats[0].to_string(),
        "live reload is delayed: /mnt/shared/hora/config.toml is on a network filesystem \
         (nfs4), where a change made on another machine may send no notification. Hora \
         re-reads the file every 60 s, so an edit is applied within about a minute; send \
         SIGHUP to apply it at once"
    );
    // A sibling whose name merely starts the same way is on another mount.
    assert_eq!(found(&nfs, "/mnt/shared-local/config.toml"), []);
    assert_eq!(found(&nfs, "/etc/hora/config.toml"), []);

    // A local filesystem mounted over the network one wins, being on top.
    let covered = format!("{nfs}1521 1520 8:3 / /mnt/shared rw - ext4 /dev/sdb1 rw\n");
    assert_eq!(found(&covered, "/mnt/shared/hora/config.toml"), []);

    // FUSE in general passes local writes through (Unraid's shfs).
    let unraid = "30 1 0:40 / /mnt/user rw - fuse.shfs shfs rw\n";
    assert_eq!(found(unraid, "/mnt/user/appdata/hora/config.toml"), []);
}

#[test]
fn a_single_file_on_a_network_filesystem_is_both() {
    let both = "\
1502 1480 0:52 / / ro - overlay overlay rw
1510 1502 0:61 /hora/config.toml /etc/hora/config.toml rw - cifs //nas/share rw
";
    let caveats = found(both, "/etc/hora/config.toml");
    assert!(
        matches!(
            caveats[..],
            [
                Caveat::SingleFileMount { .. },
                Caveat::NetworkFilesystem { .. }
            ]
        ),
        "{caveats:?}"
    );
}

#[test]
fn an_escaped_space_in_a_mount_point_is_decoded() {
    let line =
        "41 22 0:45 /my\\040conf /srv/my\\040conf/config.toml rw,relatime - ext4 /dev/sdb1 rw";
    assert_eq!(
        parse_mount(line),
        Some(Mount {
            point: PathBuf::from("/srv/my conf/config.toml"),
            fstype: "ext4".to_owned()
        })
    );
    assert!(matches!(
        found(line, "/srv/my conf/config.toml")[..],
        [Caveat::SingleFileMount { .. }]
    ));

    assert_eq!(unescape("a\\011b\\012c\\134d"), "a\tb\nc\\d");
    // Not an escape: left as written, and never read past the end.
    assert_eq!(unescape("a\\b"), "a\\b");
    assert_eq!(unescape("a\\04"), "a\\04");
    assert_eq!(unescape("a\\9z9"), "a\\9z9");
    assert_eq!(unescape("\\"), "\\");
    assert_eq!(unescape(""), "");
}

#[test]
fn lines_that_are_not_mounts_are_skipped() {
    for line in [
        "",
        "garbage",
        "41 22 0:45 / /srv rw",
        "41 22 0:45 / /srv rw shared:1",
        "41 22 0:45 / /srv rw shared:1 -",
        "41 22 0:45 / /srv rw - ",
        "- - - -",
    ] {
        assert_eq!(parse_mount(line), None, "{line:?}");
    }
    // Any number of optional fields before the separator.
    for optional in ["", "shared:1 ", "shared:1 master:2 propagate_from:3 "] {
        let line = format!("41 22 0:45 / /srv rw {optional}- xfs /dev/sdb1 rw");
        assert_eq!(
            parse_mount(&line).map(|mount| mount.fstype),
            Some("xfs".to_owned()),
            "{line}"
        );
    }
    // A table with junk in it still yields its good lines.
    let mixed = format!("garbage\n\n{COMPOSE}");
    assert_eq!(found(&mixed, "/etc/hora/config.toml"), []);
}
