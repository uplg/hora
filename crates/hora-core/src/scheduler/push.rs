//! When a push monitor's task looks at its heartbeats.
//!
//! Polling read the latest heartbeat at a fixed cadence (the interval,
//! capped at a minute), for every push monitor, whatever happened. Its
//! verdict only changes when a heartbeat comes or the deadline for the next
//! one passes, so a healthy monitor now sleeps until one of those: woken by
//! the store when a heartbeat for it is written, or by its deadline.
//! Anything short of healthy - down, degraded, a recovery not yet announced
//! (muted), a burn-rate alert to re-arm - keeps the polling cadence: the
//! threshold, maintenance ending, a down sent again or the peers asked again
//! all count on it.
//!
//! Never more reads than polling: a heartbeat is looked at no sooner than
//! one polling period after the previous look (a chatty job costs what it
//! did; a burst of `status=down` pushes cannot confirm a threshold faster),
//! and a missed one is noticed at its deadline instead of up to a period
//! after it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use crate::config::{Monitor, PushSpec};
use crate::db::{PushSubscription, Store};
use crate::heartbeat::{HeartbeatWatch, heartbeat_evaluation_for};
use crate::probe::Outcome;
use crate::status::CheckStatus;

pub(super) struct PushDriver {
    watch: HeartbeatWatch,
    changes: PushSubscription,
    /// The polling period: how often a monitor short of healthy is looked
    /// at, the soonest a heartbeat is looked at after the previous look, and
    /// the longest the task sleeps at a time (the liveness beacon ticks on,
    /// and a wall clock that jumped is caught up with).
    every: Duration,
    /// The next look, on the monotonic clock.
    poll_at: Option<Instant>,
    /// Healthy: the first second (UTC) the latest heartbeat is late.
    late_at: Option<i64>,
    /// That second as the last look found it, whatever the plan.
    deadline: Option<i64>,
    looked_at: Option<Instant>,
}

impl PushDriver {
    /// The driver of a push monitor's task, first look `offset` from now (the
    /// task's phase, see `stagger_offset`).
    pub(super) async fn new(
        store: &Store,
        monitor: &Monitor,
        push: &PushSpec,
        every: Duration,
        offset: Duration,
    ) -> Self {
        // Subscribed before the first read: a heartbeat stored meanwhile
        // still wakes the task.
        let changes = store.subscribe_push(&monitor.id);
        Self {
            watch: HeartbeatWatch::for_monitor(store, monitor, push).await,
            changes,
            every,
            poll_at: Some(Instant::now() + offset),
            late_at: None,
            deadline: None,
            looked_at: None,
        }
    }

    /// Until the next look is due. Sleeps at most one polling period at a
    /// time, beating `last_tick` on each wakeup without touching the store.
    pub(super) async fn wait(&mut self, last_tick: &AtomicU64) {
        loop {
            let remaining = self.remaining();
            if remaining.is_zero() {
                return;
            }
            tokio::select! {
                () = self.changes.changed() => {
                    // Looked at once the polling period since the previous
                    // look is over, at once if it is.
                    let soonest = self
                        .looked_at
                        .map_or_else(Instant::now, |at| (at + self.every).max(Instant::now()));
                    self.poll_at = Some(self.poll_at.map_or(soonest, |at| at.min(soonest)));
                }
                () = tokio::time::sleep(remaining.min(self.every)) => {
                    last_tick.fetch_max(
                        u64::try_from(jiff::Timestamp::now().as_second()).unwrap_or(0),
                        Ordering::Relaxed,
                    );
                }
            }
        }
    }

    fn remaining(&self) -> Duration {
        let poll = self
            .poll_at
            .map(|at| at.saturating_duration_since(Instant::now()));
        // The UTC deadline is read against the wall clock each time, so a
        // clock that was set forward or back is followed.
        let late = self.late_at.map(|at| {
            let ms = at
                .saturating_mul(1000)
                .saturating_sub(jiff::Timestamp::now().as_millisecond());
            Duration::from_millis(u64::try_from(ms).unwrap_or(0))
        });
        match (poll, late) {
            (Some(poll), Some(late)) => poll.min(late),
            (Some(due), None) | (None, Some(due)) => due,
            // Healthy and never late (a schedule with no next run): only a
            // heartbeat wakes it, and the beacon in the meantime.
            (None, None) => Duration::MAX,
        }
    }

    /// Read the latest heartbeat and judge it, as polling did. Until
    /// [`PushDriver::plan`] says otherwise, the next look is at the deadline
    /// (or, after a read error, one polling period on).
    pub(super) async fn evaluate(&mut self, store: &Store, monitor: &Monitor) -> Option<Outcome> {
        // Before the read, never after: a heartbeat stored while it runs
        // must still wake the next wait.
        self.changes.mark_seen();
        let now = Instant::now();
        self.looked_at = Some(now);
        let Some(evaluation) = heartbeat_evaluation_for(
            store,
            &monitor.id,
            &self.watch.cadence,
            self.watch.expected_since,
            i64::try_from(monitor.interval_secs).unwrap_or(i64::MAX),
            jiff::Timestamp::now().as_second(),
        )
        .await
        else {
            self.poll_at = Some(now + self.every);
            self.late_at = None;
            self.deadline = None;
            return None;
        };
        self.poll_at = None;
        self.late_at = evaluation.late_at;
        self.deadline = evaluation.late_at;
        evaluation.outcome
    }

    /// After the alert state has taken `outcome`: anything short of healthy
    /// (`settled` false: a recovery, a burn alert, a count still owed) is
    /// looked at again one polling period after this look; a healthy,
    /// settled monitor at its deadline or its next heartbeat.
    pub(super) fn plan(&mut self, outcome: &Outcome, settled: bool) {
        if outcome.status != CheckStatus::Up || !settled {
            self.poll_at = Some(self.looked_at.unwrap_or_else(Instant::now) + self.every);
            self.late_at = None;
        } else {
            self.poll_at = None;
            self.late_at = self.deadline;
        }
    }
}
