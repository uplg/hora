//! Feeds the built-in configuration monitor ([`CONFIG_MONITOR_ID`]): at every
//! change of the configuration in force, and every few hours so its heartbeat
//! never lapses, it records the state of the load as a push - down while
//! entries were left out or the file cannot be loaded, up otherwise.
//! Alerting, recovery and the status page then treat it like any
//! other push monitor.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::config::{CONFIG_MONITOR_ID, Config};
use crate::db::{self, Store};
use crate::status::CheckStatus;

/// Re-record an unchanged state this often (the monitor's deadline is far longer).
const REFRESH: Duration = Duration::from_hours(6);
/// The stored message is bounded (its count says how many issues there
/// are); the log has every issue in full.
const MAX_MESSAGE_CHARS: usize = 1000;

/// Feed the configuration monitor until `shutdown`.
#[must_use]
pub fn spawn(
    store: Store,
    mut config: watch::Receiver<Arc<Config>>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let snapshot = config.borrow_and_update().clone();
            let (status, message) = state(&snapshot);
            if let Err(err) =
                db::insert_push(&store, CONFIG_MONITOR_ID, status, None, Some(&message)).await
            {
                warn!("cannot record the configuration monitor's state: {err:#}");
            }
            tokio::select! {
                changed = config.changed() => if changed.is_err() { break },
                () = tokio::time::sleep(REFRESH) => {}
                _ = shutdown.changed() => break,
            }
        }
    })
}

/// The status and message the configuration monitor shows for `config`.
#[must_use]
pub fn state(config: &Config) -> (CheckStatus, String) {
    if config.issues.is_empty() {
        return (
            CheckStatus::Up,
            "configuration loaded without issues".to_owned(),
        );
    }
    let list = config
        .issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    let message = format!("{} configuration issue(s): {list}", config.issues.len());
    (
        CheckStatus::Down,
        crate::bounded(&message, MAX_MESSAGE_CHARS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_turn_the_config_monitor_down_with_every_issue_named() {
        let clean = crate::config::parse_lenient("[page]\n[server]\n").unwrap();
        assert_eq!(state(&clean).0, CheckStatus::Up);
        // One broken monitor and one broken channel: the rest loads, the
        // configuration monitor names both.
        let faulty = crate::config::parse_lenient(
            "[page]\n[server]\n[[monitors]]\nid = \"bad\"\nname = \"B\"\n\
             target = \"https://e.com\"\ninterval_secs = 0\n\
             [[monitors]]\nid = \"good\"\nname = \"G\"\ntarget = \"https://e.com\"\n\
             interval_secs = 60\n\
             [[channels]]\nname = \"ok\"\ntype = \"webhook\"\nurl = \"https://h.example/x\"\n\
             [[channels]]\nname = \"broken\"\ntype = \"webhook\"\n",
        )
        .unwrap();
        assert!(faulty.find_monitor("good").is_some());
        assert_eq!(faulty.channels.len(), 1);
        let (status, message) = state(&faulty);
        assert_eq!(status, CheckStatus::Down);
        assert_eq!(
            message,
            "2 configuration issue(s): channel broken: missing field `url`; not loaded; \
             monitor bad: interval_secs must be > 0; not loaded"
        );
    }

    /// Fed into the store, the state is what the push monitor reads back.
    #[tokio::test]
    async fn the_state_is_recorded_as_a_push() {
        let store = Store::in_memory().await;
        let config = crate::config::parse_lenient(
            "[page]\n[server]\n[[monitors]]\nid = \"bad\"\nname = \"B\"\n\
             target = \"https://e.com\"\ninterval_secs = 0\n",
        )
        .unwrap();
        let (tx, rx) = watch::channel(Arc::new(config));
        let (stop, stopped) = watch::channel(false);
        let task = spawn(store.clone(), rx, stopped);
        let beat = loop {
            if let Some(beat) = db::last_heartbeat(&store, CONFIG_MONITOR_ID).await.unwrap() {
                break beat;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(beat.status, CheckStatus::Down);
        assert!(beat.error.unwrap().contains("monitor bad"));
        stop.send(true).unwrap();
        task.await.unwrap();
        drop(tx);
    }
}
