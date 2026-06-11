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

use std::path::Path;
use std::time::Instant;

use tokio::io::AsyncReadExt as _;

use crate::config::Monitor;
use crate::probe::Outcome;

/// Cap on the output kept from a plugin (the first line becomes the
/// message). The pipe keeps being drained beyond it, so a chatty but healthy
/// plugin never blocks on a full pipe and times out.
const MAX_OUTPUT_BYTES: u64 = 8 * 1024;

/// Cap on the message stored from the plugin's first line.
const MAX_MESSAGE_CHARS: usize = 300;

/// Run one exec probe. Every failure mode - missing or non-executable file,
/// an escape attempt, a timeout, a signal - is a down with a clear reason;
/// the probe itself can never break the scheduler loop.
pub(crate) async fn run(exec_dir: &Path, monitor: &Monitor) -> Outcome {
    let Some(name) = monitor.command.first() else {
        // Config validation rejects this; defensive only.
        return Outcome::down("exec monitor has no command".to_owned());
    };
    let program = match resolve(exec_dir, name) {
        Ok(program) => program,
        Err(reason) => return Outcome::down(reason),
    };

    let start = Instant::now();
    let mut child = match tokio::process::Command::new(&program)
        .args(&monitor.command[1..])
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
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return Outcome::down(format!("could not run {name}: {err}")),
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let wait = async {
        // Read both streams concurrently with the wait: a plugin filling a
        // pipe we never drained would deadlock against its own exit.
        let (status, out, err) =
            tokio::join!(child.wait(), read_capped(stdout), read_capped(stderr));
        (status, out, err)
    };

    match tokio::time::timeout(monitor.timeout(), wait).await {
        Ok((Ok(status), out, err)) => {
            let latency = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
            let message = first_line(&out).or_else(|| first_line(&err));
            outcome_for(status.code(), message, latency)
        }
        Ok((Err(error), _, _)) => Outcome::down(format!("exec wait failed: {error}")),
        Err(_elapsed) => {
            // SIGKILL, not a polite signal: a stuck plugin already had the
            // monitor's whole timeout to finish.
            let _ = child.kill().await;
            Outcome::down(format!(
                "plugin timed out after {}s",
                monitor.timeout().as_secs()
            ))
        }
    }
}

/// Resolve `name` strictly inside `exec_dir`: the joined path is
/// canonicalized and must still live under the (canonicalized) directory, so
/// neither `../` (already rejected at config load) nor a symlink planted in
/// the directory can escape it.
fn resolve(exec_dir: &Path, name: &str) -> Result<std::path::PathBuf, String> {
    let dir = exec_dir
        .canonicalize()
        .map_err(|err| format!("HORA_EXEC_DIR unusable: {err}"))?;
    let program = dir
        .join(name)
        .canonicalize()
        .map_err(|err| format!("plugin {name} not found: {err}"))?;
    if !program.starts_with(&dir) {
        return Err(format!("plugin {name} escapes HORA_EXEC_DIR, refusing"));
    }
    Ok(program)
}

/// Read a child stream keeping at most [`MAX_OUTPUT_BYTES`], then drain the
/// rest to the void so the child never blocks on a full pipe.
async fn read_capped(stream: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
    let Some(stream) = stream else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    let mut capped = stream.take(MAX_OUTPUT_BYTES);
    let _ = capped.read_to_end(&mut kept).await;
    let _ = tokio::io::copy(&mut capped.into_inner(), &mut tokio::io::sink()).await;
    kept
}

/// The plugin's message: its first output line, with the `|perfdata` tail
/// stripped (the monitoring-plugins convention), bounded.
fn first_line(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let line = text.lines().next()?.trim();
    let line = line.split('|').next().unwrap_or(line).trim();
    (!line.is_empty()).then(|| line.chars().take(MAX_MESSAGE_CHARS).collect())
}

/// Map an exit code to an outcome, monitoring-plugins style. `None` (killed
/// by a signal) is down: a crashed check vouches for nothing.
fn outcome_for(code: Option<i32>, message: Option<String>, latency_ms: i64) -> Outcome {
    match code {
        Some(0) => Outcome {
            up: true,
            degraded: false,
            latency_ms: Some(latency_ms),
            status_code: None,
            error: None,
            snapshot: None,
        },
        Some(1) => Outcome {
            up: true,
            degraded: true,
            latency_ms: Some(latency_ms),
            status_code: None,
            error: Some(message.unwrap_or_else(|| "plugin warning (exit 1)".to_owned())),
            snapshot: None,
        },
        Some(code) => Outcome {
            up: false,
            degraded: false,
            latency_ms: Some(latency_ms),
            status_code: None,
            error: Some(
                message.unwrap_or_else(|| format!("plugin reported critical (exit {code})")),
            ),
            snapshot: None,
        },
        None => Outcome::down("plugin killed by a signal".to_owned()),
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

    #[tokio::test]
    async fn exit_codes_follow_the_monitoring_plugins_convention() {
        let fixture = Fixture::new("codes");
        fixture.script("ok", r#"echo "RAID OK | time=3ms"; exit 0"#);
        fixture.script("warn", r#"echo "DISK WARNING - 85% used"; exit 1"#);
        fixture.script("crit", r#"echo "DISK CRITICAL - 99% used"; exit 2"#);
        fixture.script("silent-crit", "exit 3");

        let up = run(&fixture.dir, &exec_monitor(&["ok"], 5)).await;
        assert!(up.up && !up.degraded);
        assert_eq!(up.error, None);
        assert!(up.latency_ms.is_some());

        // Exit 1: degraded, message kept, perfdata-free.
        let warn = run(&fixture.dir, &exec_monitor(&["warn"], 5)).await;
        assert!(warn.up && warn.degraded);
        assert_eq!(warn.error.as_deref(), Some("DISK WARNING - 85% used"));

        let crit = run(&fixture.dir, &exec_monitor(&["crit"], 5)).await;
        assert!(!crit.up);
        assert_eq!(crit.error.as_deref(), Some("DISK CRITICAL - 99% used"));

        // No output: a synthesized reason carries the exit code.
        let silent = run(&fixture.dir, &exec_monitor(&["silent-crit"], 5)).await;
        assert!(!silent.up);
        assert!(silent.error.as_deref().unwrap().contains("exit 3"));
    }

    #[tokio::test]
    async fn arguments_reach_the_plugin_and_perfdata_is_stripped() {
        let fixture = Fixture::new("args");
        fixture.script("echoer", r#"echo "got $1 $2 | perf=1"; exit 2"#);
        let outcome = run(&fixture.dir, &exec_monitor(&["echoer", "-H", "x.org"], 5)).await;
        assert_eq!(outcome.error.as_deref(), Some("got -H x.org"));
    }

    #[tokio::test]
    async fn a_stuck_plugin_is_killed_at_the_timeout() {
        let fixture = Fixture::new("stuck");
        fixture.script("hang", "sleep 60");
        let started = std::time::Instant::now();
        let outcome = run(&fixture.dir, &exec_monitor(&["hang"], 1)).await;
        assert!(!outcome.up);
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
        let outcome = run(&fixture.dir, &exec_monitor(&["flood"], 10)).await;
        assert!(outcome.up, "{:?}", outcome.error);
    }

    #[tokio::test]
    async fn symlinks_cannot_escape_the_exec_dir() {
        let fixture = Fixture::new("escape");
        // A symlink inside the dir pointing outside it: refused even though
        // the *name* looks legitimate.
        std::os::unix::fs::symlink("/bin/sh", fixture.dir.join("sneaky")).expect("symlink");
        let outcome = run(&fixture.dir, &exec_monitor(&["sneaky"], 5)).await;
        assert!(!outcome.up);
        assert!(
            outcome.error.as_deref().unwrap().contains("escapes"),
            "{:?}",
            outcome.error
        );
    }

    #[tokio::test]
    async fn missing_plugins_and_missing_dirs_are_clean_downs() {
        let fixture = Fixture::new("missing");
        let outcome = run(&fixture.dir, &exec_monitor(&["nope"], 5)).await;
        assert!(!outcome.up);
        assert!(outcome.error.as_deref().unwrap().contains("not found"));

        let gone = std::path::Path::new("/nonexistent-hora-exec-dir");
        let outcome = run(gone, &exec_monitor(&["nope"], 5)).await;
        assert!(!outcome.up);
        assert!(outcome.error.as_deref().unwrap().contains("unusable"));
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
        let outcome = run(&fixture.dir, &exec_monitor(&["leak"], 5)).await;
        assert!(outcome.up, "{:?}", outcome.error);
    }
}
