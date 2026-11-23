//! A bounded wakeup per watched push id, shared by clones of the Store.
//! SQLite remains authoritative: notifications carry no payload or history.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::watch;

#[derive(Debug, Default)]
pub(super) struct PushUpdates(Mutex<HashMap<String, watch::Sender<()>>>);

impl PushUpdates {
    pub(super) fn subscribe(self: &Arc<Self>, id: &str) -> PushSubscription {
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let receiver = entries
            .entry(id.to_owned())
            .or_insert_with(|| watch::channel(()).0)
            .subscribe();
        PushSubscription {
            updates: Arc::clone(self),
            id: id.to_owned(),
            receiver: Some(receiver),
        }
    }

    pub(super) fn notify(&self, id: &str) {
        // Unknown/unwatched ids never allocate an entry. Holding this brief
        // lock also serializes registration and cleanup with publication.
        if let Some(sender) = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
        {
            sender.send_replace(());
        }
    }
}

pub(crate) struct PushSubscription {
    updates: Arc<PushUpdates>,
    id: String,
    receiver: Option<watch::Receiver<()>>,
}

impl PushSubscription {
    pub(crate) async fn changed(&mut self) {
        self.receiver
            .as_mut()
            .expect("active subscription")
            .changed()
            .await
            .expect("subscription keeps its sender alive");
    }

    /// Mark before reading SQLite, never afterwards: a push racing the read
    /// must remain pending for the next iteration.
    pub(crate) fn mark_seen(&mut self) {
        self.receiver
            .as_mut()
            .expect("active subscription")
            .mark_unchanged();
    }
}

impl Drop for PushSubscription {
    fn drop(&mut self) {
        let mut entries = self
            .updates
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Drop under the same lock as registration. Two concurrently exiting
        // tasks must not both see another receiver and leave an orphan entry.
        drop(self.receiver.take());
        if entries
            .get(&self.id)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            entries.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successful_commits_wake_readers_but_failed_writes_do_not() {
        let store = crate::db::Store::in_memory().await;
        let mut changes = store.subscribe_push("job");
        crate::db::insert_push(&store, "job", crate::status::CheckStatus::Up, None, None)
            .await
            .unwrap();
        changes.changed().await;
        assert!(
            crate::db::last_heartbeat(&store, "job")
                .await
                .unwrap()
                .is_some()
        );
        store.close().await;
        assert!(
            crate::db::insert_push(&store, "job", crate::status::CheckStatus::Down, None, None)
                .await
                .is_err()
        );
        assert!(!changes.receiver.as_ref().unwrap().has_changed().unwrap());
    }

    #[test]
    fn concurrent_task_exits_remove_the_subscription() {
        let updates = Arc::new(PushUpdates::default());
        for _ in 0..50 {
            let first = updates.subscribe("job");
            let second = updates.subscribe("job");
            std::thread::scope(|scope| {
                scope.spawn(move || drop(first));
                scope.spawn(move || drop(second));
            });
        }
        assert!(updates.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn targeted_coalesced_notifications_survive_races_and_cleanup() {
        let updates = Arc::new(PushUpdates::default());
        updates.notify("unwatched");
        let mut first = updates.subscribe("job");
        let mut replacement = updates.subscribe("job");
        let other = updates.subscribe("other");
        first.mark_seen();
        for _ in 0..10_000 {
            updates.notify("job");
        }
        assert!(!other.receiver.as_ref().unwrap().has_changed().unwrap());
        first.changed().await;
        assert!(!first.receiver.as_ref().unwrap().has_changed().unwrap());
        replacement.changed().await;
        drop(first);
        updates.notify("job");
        replacement.changed().await;
        drop(replacement);
        drop(other);
        assert!(updates.0.lock().unwrap().is_empty());
    }
}
