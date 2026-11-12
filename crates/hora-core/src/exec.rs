//! Exec probes: run an external check following the monitoring-plugins
//! convention (exit 0 = up, 1 = degraded/WARNING, anything else = down),
//! which opens the whole Nagios/Icinga plugin ecosystem to Hora - RAID,
//! disks, exotic certificates, SNMP, or a five-line script watching another
//! container through a (rootless) Docker socket.
//!
//! The security model is the `HORA_EXEC_DIR` environment variable, by design
//! *not* a config key: the hot-reloadable config alone must never be able to
//! run code. With the variable set, `command[0]` is resolved strictly inside
//! that directory (canonicalized, so a symlink pointing outside is refused),
//! no shell is ever involved (`command` is a raw argv), and the child gets a
//! scrubbed environment - the daemon's own env carries notification tokens
//! that no plugin has any business reading.

use crate::status::CheckStatus;
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt as _;

use crate::config::{ExecSpec, Monitor};
use crate::probe::{FailureKind, Outcome};

/// Cap on the output kept from a plugin (the first line becomes the
/// message). The pipe keeps being drained beyond it, so a chatty but healthy
/// plugin never blocks on a full pipe and times out.
const MAX_OUTPUT_BYTES: usize = 8 * 1024;

/// Cap on the message stored from the plugin's first line.
const MAX_MESSAGE_CHARS: usize = 300;

/// How long the output is still read once the plugin has exited. A
/// descendant it left running (daemonized, or backgrounded with `&`) may hold
/// the pipes open forever; the plugin's verdict is its exit code, so the probe
/// stops waiting for the pipes to close shortly after it - and kills the
/// process group still holding them.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);

/// Run one exec probe. Every failure mode - missing or non-executable file,
/// an escape attempt, a timeout, a signal - is a down with a clear reason;
/// the probe itself can never break the scheduler loop.
///
/// `exec_dir` is the canonical `HORA_EXEC_DIR` (resolved once at config load).
pub(crate) async fn run(exec_dir: &Path, monitor: &Monitor, spec: &ExecSpec) -> Outcome {
    let name = &spec.program;
    let program = match resolve(exec_dir, name).await {
        Ok(program) => program,
        Err(reason) => return Outcome::down(FailureKind::Plugin, reason),
    };

    let start = Instant::now();
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&spec.args)
        .current_dir(exec_dir)
        // A scrubbed environment: the daemon's env carries channel tokens
        // (`${VAR}` interpolation); a plugin gets the bare POSIX minimum.
        .env_clear()
        .envs(
            std::env::vars()
                .filter(|(key, _)| matches!(key.as_str(), "PATH" | "HOME" | "LANG" | "TZ")),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // If this future is dropped (monitor removed mid-probe), the child
        // dies with it instead of leaking.
        .kill_on_drop(true);
    // Its own process group, so a timeout kills everything the plugin
    // started - a `sh` wrapper's hung `curl` - not only the direct child.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Outcome::down(FailureKind::Plugin, format!("could not run {name}: {err}"));
        }
    };
    // Declared after `child`, so dropped first: the group is killed while
    // its leader is still unreaped and the id cannot have been reused.
    let mut group = ProcessGroup(child.id());

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let (waited, latency) = {
        // Read both streams concurrently with the wait: a plugin filling a
        // pipe we never drained would deadlock against its own exit.
        let readers = async {
            tokio::join!(read_capped(stdout, &mut out), read_capped(stderr, &mut err));
        };
        tokio::pin!(readers);
        let mut drained = false;
        let waited = tokio::time::timeout(monitor.timeout(), async {
            loop {
                tokio::select! {
                    status = child.wait() => break status,
                    () = &mut readers, if !drained => drained = true,
                }
            }
        })
        .await;
        let latency = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
        if matches!(waited, Ok(Ok(_))) {
            // Pipes still open after the grace: a descendant the plugin left
            // behind holds them (`sleep 30 &`), and would outlive every probe
            // - one more per interval. Kill the group. Its id is still the
            // group's: a pid stays reserved while it names a process group
            // with members, and one of them holds our pipes. A descendant
            // that closed them (a proper daemon) is left alone.
            if !drained && tokio::time::timeout(OUTPUT_GRACE, readers).await.is_err() {
                group.kill();
            }
            group.disarm();
        }
        (waited, latency)
    };

    match waited {
        Ok(Ok(status)) => {
            let message = first_line(&out).or_else(|| first_line(&err));
            outcome_for(status.code(), message, latency)
        }
        Ok(Err(error)) => Outcome::down(FailureKind::Plugin, format!("exec wait failed: {error}")),
        Err(_elapsed) => {
            // SIGKILL, not a polite signal: a stuck plugin already had the
            // monitor's whole timeout to finish.
            group.kill();
            let _ = child.kill().await;
            Outcome::down(
                FailureKind::Plugin,
                format!(
                    "plugin timed out after {}",
                    crate::fmt::elapsed(monitor.timeout().as_secs())
                ),
            )
        }
    }
}

/// The plugin's process group (its id is the plugin's pid), sent `SIGKILL`
/// as a whole on timeout - and on drop, should the probe be dropped mid-run -
/// so no descendant outlives the plugin. Disarmed once the plugin is reaped.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn kill(&mut self) {
        let Some(id) = self.0.take() else {
            return;
        };
        #[cfg(unix)]
        if let Ok(id) = i32::try_from(id) {
            // ESRCH (the group already gone) is the outcome we want anyway.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(id),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        #[cfg(not(unix))]
        let _ = id;
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Resolve `name` strictly inside the canonical `exec_dir`: the joined path
/// is canonicalized and must still live under the directory, so neither
/// `../` (already rejected at config load) nor a symlink planted in the
/// directory can escape it. Off the runtime threads: it touches the disk.
async fn resolve(exec_dir: &Path, name: &str) -> Result<std::path::PathBuf, String> {
    let program = tokio::fs::canonicalize(exec_dir.join(name))
        .await
        .map_err(|err| format!("plugin {name} not found: {err}"))?;
    if !program.starts_with(exec_dir) {
        return Err(format!("plugin {name} escapes HORA_EXEC_DIR, refusing"));
    }
    Ok(program)
}

/// Read a child stream into `kept`, keeping at most [`MAX_OUTPUT_BYTES`] and
/// draining the rest to the void so the child never blocks on a full pipe.
/// What was read stays in `kept` even if this future is dropped mid-way.
async fn read_capped(stream: Option<impl tokio::io::AsyncRead + Unpin>, kept: &mut Vec<u8>) {
    let Some(mut stream) = stream else {
        return;
    };
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..read.min(room)]);
            }
        }
    }
}

/// The plugin's message: its first output line, with the `|perfdata` tail
/// stripped (the monitoring-plugins convention), bounded.
fn first_line(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let line = text.lines().next()?.trim();
    let line = line.split('|').next().unwrap_or(line).trim();
    (!line.is_empty()).then(|| {
        crate::fmt::printable(line)
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .collect()
    })
}

/// Map an exit code to an outcome, monitoring-plugins style. `None` (killed
/// by a signal) is down: a crashed check vouches for nothing.
fn outcome_for(code: Option<i32>, message: Option<String>, latency_ms: i64) -> Outcome {
    match code {
        Some(0) => Outcome::up(false, Some(latency_ms), None),
        Some(1) => Outcome {
            status: CheckStatus::Degraded,
            latency_ms: Some(latency_ms),
            status_code: None,
            error: Some(message.unwrap_or_else(|| "plugin warning (exit 1)".to_owned())),
            reason: Some(FailureKind::Plugin),
            snapshot: None,
        },
        Some(code) => Outcome {
            status: CheckStatus::Down,
            latency_ms: Some(latency_ms),
            status_code: None,
            error: Some(
                message.unwrap_or_else(|| format!("plugin reported critical (exit {code})")),
            ),
            reason: Some(FailureKind::Plugin),
            snapshot: None,
        },
        None => Outcome::down(FailureKind::Plugin, "plugin killed by a signal".to_owned()),
    }
}

/// The output parsing for the fuzz targets in `fuzz/` (built with
/// `--cfg fuzzing` only).
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzz {
    /// [`super::first_line`].
    #[must_use]
    pub fn first_line(output: &[u8]) -> Option<String> {
        super::first_line(output)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt as _;

    /// A scratch exec dir with the given scripts, cleaned on drop.
    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("hora-exec-test-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create fixture dir");
            // `run` takes the canonical directory, as config load provides it
            // (on macOS the temp dir lives behind the /var -> /private/var link).
            let dir = dir.canonicalize().expect("canonical fixture dir");
            Self { dir }
        }

        fn script(&self, name: &str, body: &str) {
            let path = self.dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod script");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn exec_monitor(command: &[&str], timeout_secs: u64) -> Monitor {
        crate::config::parse_with_exec_dir(
            &format!(
                r#"
                [page]
                [server]
                [[monitors]]
                id = "m"
                name = "M"
                kind = "exec"
                command = [{}]
                interval_secs = 60
                timeout_secs = {timeout_secs}
                "#,
                command
                    .iter()
                    .map(|part| format!("{part:?}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Some(std::env::temp_dir()), // validation only needs an existing dir
        )
        .expect("config")
        .monitors
        .remove(0)
    }

    async fn run_exec(dir: &Path, monitor: &Monitor) -> Outcome {
        let crate::config::MonitorKind::Exec(spec) = &monitor.spec else {
            panic!("not an exec monitor");
        };
        run(dir, monitor, spec).await
    }

    #[tokio::test]
    async fn exit_codes_follow_the_monitoring_plugins_convention() {
        let fixture = Fixture::new("codes");
        fixture.script("ok", r#"echo "RAID OK | time=3ms"; exit 0"#);
        fixture.script("warn", r#"echo "DISK WARNING - 85% used"; exit 1"#);
        fixture.script("crit", r#"echo "DISK CRITICAL - 99% used"; exit 2"#);
        fixture.script("silent-crit", "exit 3");

        let up = run_exec(&fixture.dir, &exec_monitor(&["ok"], 5)).await;
        assert!(up.is_up() && !up.is_degraded());
        assert_eq!(up.error, None);
        assert!(up.latency_ms.is_some());

        // Exit 1: degraded, message kept, perfdata-free.
        let warn = run_exec(&fixture.dir, &exec_monitor(&["warn"], 5)).await;
        assert!(warn.is_up() && warn.is_degraded());
        assert_eq!(warn.error.as_deref(), Some("DISK WARNING - 85% used"));

        let crit = run_exec(&fixture.dir, &exec_monitor(&["crit"], 5)).await;
        assert!(!crit.is_up());
        assert_eq!(crit.error.as_deref(), Some("DISK CRITICAL - 99% used"));

        // No output: a synthesized reason carries the exit code.
        let silent = run_exec(&fixture.dir, &exec_monitor(&["silent-crit"], 5)).await;
        assert!(!silent.is_up());
        assert!(silent.error.as_deref().unwrap().contains("exit 3"));
    }

    #[tokio::test]
    async fn arguments_reach_the_plugin_and_perfdata_is_stripped() {
        let fixture = Fixture::new("args");
        fixture.script("echoer", r#"echo "got $1 $2 | perf=1"; exit 2"#);
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["echoer", "-H", "x.org"], 5)).await;
        assert_eq!(outcome.error.as_deref(), Some("got -H x.org"));
    }

    #[tokio::test]
    async fn a_stuck_plugin_is_killed_at_the_timeout() {
        let fixture = Fixture::new("stuck");
        fixture.script("hang", "sleep 60");
        let started = std::time::Instant::now();
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["hang"], 1)).await;
        assert!(!outcome.is_up());
        assert!(outcome.error.as_deref().unwrap().contains("timed out"));
        assert!(started.elapsed().as_secs() < 5, "killed promptly");
    }

    #[tokio::test]
    async fn a_chatty_plugin_is_bounded_not_deadlocked() {
        let fixture = Fixture::new("chatty");
        // ~16 MB of output: far past the cap and past any pipe buffer.
        fixture.script(
            "flood",
            r#"echo "still fine"; i=0; while [ $i -lt 4000 ]; do printf '%4096s' x; i=$((i+1)); done; exit 0"#,
        );
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["flood"], 10)).await;
        assert!(outcome.is_up(), "{:?}", outcome.error);
    }

    #[tokio::test]
    async fn symlinks_cannot_escape_the_exec_dir() {
        let fixture = Fixture::new("escape");
        // A symlink inside the dir pointing outside it: refused even though
        // the *name* looks legitimate.
        std::os::unix::fs::symlink("/bin/sh", fixture.dir.join("sneaky")).expect("symlink");
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["sneaky"], 5)).await;
        assert!(!outcome.is_up());
        assert!(
            outcome.error.as_deref().unwrap().contains("escapes"),
            "{:?}",
            outcome.error
        );
    }

    #[tokio::test]
    async fn missing_plugins_and_missing_dirs_are_clean_downs() {
        let fixture = Fixture::new("missing");
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["nope"], 5)).await;
        assert!(!outcome.is_up());
        assert!(outcome.error.as_deref().unwrap().contains("not found"));

        let gone = std::path::Path::new("/nonexistent-hora-exec-dir");
        let outcome = run_exec(gone, &exec_monitor(&["nope"], 5)).await;
        assert!(!outcome.is_up());
        assert!(outcome.error.as_deref().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group() {
        let fixture = Fixture::new("group");
        let pidfile = fixture.dir.join("grandchild.pid");
        // A wrapper whose own child hangs: killing only the wrapper would
        // leave the `sleep` orphaned, one per interval.
        fixture.script(
            "wrapper",
            &format!("sleep 60 &\necho $! > {}\nwait", pidfile.display()),
        );
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["wrapper"], 1)).await;
        assert!(outcome.error.as_deref().unwrap().contains("timed out"));

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pid written")
            .trim()
            .parse()
            .expect("numeric pid");
        let pid = nix::unistd::Pid::from_raw(pid);
        // SIGKILLed, then reaped by init once orphaned: poll for it to vanish.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while nix::sys::signal::kill(pid, None).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild survived the timeout"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_lingering_descendant_does_not_turn_a_pass_into_a_timeout() {
        let fixture = Fixture::new("linger");
        // Exits 0 at once, but a backgrounded child inherits stdout and keeps
        // the pipe open far past the timeout.
        let pidfile = fixture.dir.join("lingering.pid");
        fixture.script(
            "daemonizes",
            &format!(
                "sleep 30 &\necho $! > {}\necho \"all good\"\nexit 0",
                pidfile.display()
            ),
        );
        let started = std::time::Instant::now();
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["daemonizes"], 5)).await;
        assert!(outcome.is_up(), "{:?}", outcome.error);
        assert!(started.elapsed().as_secs() < 3, "waited for the pipe");

        // And it does not outlive the probe: one leaked `sleep` per interval
        // adds up.
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("pid written")
            .trim()
            .parse()
            .expect("numeric pid");
        let pid = nix::unistd::Pid::from_raw(pid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while nix::sys::signal::kill(pid, None).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the lingering descendant survived the probe"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn the_plugin_environment_is_scrubbed() {
        let fixture = Fixture::new("env");
        // The daemon's env carries secrets; the plugin must not see them. We
        // can't safely set env vars in a test, but we CAN assert the scrub
        // list: anything not allowlisted is absent, even ubiquitous ones.
        fixture.script(
            "leak",
            r#"if [ -n "$CARGO_PKG_NAME$CARGO_MANIFEST_DIR$HORA_LOG" ]; then echo "LEAKED"; exit 2; else echo "clean"; exit 0; fi"#,
        );
        let outcome = run_exec(&fixture.dir, &exec_monitor(&["leak"], 5)).await;
        assert!(outcome.is_up(), "{:?}", outcome.error);
    }
}
