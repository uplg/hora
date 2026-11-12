//! The status summary, kept fresh in the background and served from memory.
//!
//! Building the summary reads the database (milliseconds to seconds,
//! depending on its size); serving it must not. A refresher task rebuilds
//! the operator's view continuously and swaps it in whole; a request only
//! ever loads the latest snapshot (stale-while-revalidate) - the very first
//! one after start waits for the first build, nothing waits after that. Every
//! audience's view, and every rendered body (page, text, JSON, metrics), is
//! derived from that one build in memory, at most once per snapshot.
//!
//! Each build starts at most every [`MIN_REFRESH`], and never sooner than its
//! own duration after the previous one ended: on a database slow enough that
//! a build takes longer than that, the refresher idles at least half the
//! time instead of keeping a connection and a core busy forever. A config
//! reload, or an operator write that must show at once (an announcement, an
//! event marker), starts a build immediately.
//!
//! That pace is for someone watching. A build costs a query per monitor, so
//! at 500 monitors rebuilding every 5 s is about a hundred queries a second,
//! day and night, for a page nobody may open for hours. When no request read
//! the snapshot since the previous build, the next one waits
//! [`IDLE_REFRESH`] instead. The request that ends such a stretch is not
//! handed the old page in silence: if the snapshot is older than
//! [`STALE_AFTER`] it wakes the refresher and waits for the rebuild, up to
//! [`FRESH_WAIT`].
//!
//! What a reader is served, at worst: a snapshot [`STALE_AFTER`] (10 s) old,
//! where it used to be [`MIN_REFRESH`] (5 s) - both plus the time of a build.
//! Only when a build takes longer than [`FRESH_WAIT`] does the first reader
//! after an idle stretch get the old snapshot, then up to [`IDLE_REFRESH`]
//! (or one build, if that is longer) plus [`FRESH_WAIT`] old, once; the
//! build it woke serves the readers after it.
//!
//! A reader is whatever loads the snapshot: the status pages, `/api/summary`,
//! `/metrics` and a peer's `/api/peer/monitors` poll, all alike - a Prometheus
//! scraping every 15 s keeps what it reads under 10 s old, at about two
//! builds per scrape, and that is the service it asked for. `/healthz` and
//! the badges read the database themselves and never touch the snapshot: a
//! container health check or a README badge does not keep the refresher busy.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use axum::body::Bytes;
use hora_core::config::Config;
use hora_core::db::Store;
use hora_core::mesh::vantage::VantageMap;
use hora_core::notifications::{self, Notifiers};
use tokio::sync::{Notify, watch};

use crate::AppState;
use crate::layout::Spliced;
use crate::summary::{BuildState, Built, Summary, build_summary, derive};
use crate::visibility::{Audience, Visibility};

/// The shortest pause between two builds' starts.
pub(crate) const MIN_REFRESH: Duration = Duration::from_secs(5);
/// The pause before the next build when nobody read the last one.
pub(crate) const IDLE_REFRESH: Duration = Duration::from_secs(30);
/// The age past which a reader finding the refresher idle asks for a rebuild
/// rather than take the snapshot as it is.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(10);
/// How long that reader waits for the rebuild before settling for the old
/// snapshot: a request must not hang on a slow database.
pub(crate) const FRESH_WAIT: Duration = Duration::from_secs(2);

/// The refresher's timings (see the module docs); a value so the tests can
/// run the loop in milliseconds.
#[derive(Debug, Clone, Copy)]
struct Cadence {
    active: Duration,
    idle: Duration,
    stale_after: Duration,
    fresh_wait: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            active: MIN_REFRESH,
            idle: IDLE_REFRESH,
            stale_after: STALE_AFTER,
            fresh_wait: FRESH_WAIT,
        }
    }
}

impl Cadence {
    /// The pause after a build that took `took`. Watched: `active` between
    /// two starts, and never less than the build itself. Unwatched: `idle`.
    fn pause(&self, took: Duration, watched: bool) -> Duration {
        let active = self.active.saturating_sub(took).max(took);
        if watched {
            active
        } else {
            active.max(self.idle)
        }
    }
}
/// How long an operator write waits for the build that shows it. The write
/// is stored before the wait, which is only a courtesy to the next page
/// view: kept under the request deadline (`routes::REQUEST_TIMEOUT`), so a
/// slow build answers the write late rather than with an error. (It was a
/// minute, six times what `hora top` waits for an answer.)
pub(crate) const WRITE_VISIBLE_TIMEOUT: Duration = Duration::from_secs(20);

/// A rendered body: which view, in which format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Body {
    /// The status page (whole, or one group's) as HTML, with its per-request
    /// parts left as holes (see `crate::layout::Spliced`). The flag is a
    /// `?token=` that opened nothing (the private door says so): whether one
    /// came, never which.
    Page(Audience, Option<String>, bool),
    /// The same, as aligned plain text.
    Text(Audience, Option<String>),
    /// `/api/summary`.
    Json(Audience),
    /// `/metrics`.
    Metrics(Audience),
}

/// One build and everything derived from it.
pub(crate) struct Snapshot {
    built: Built,
    /// When the build was published, for a reader to judge its age.
    built_at: Instant,
    /// Derived views and rendered bodies, for the config they were derived
    /// against (a reload invalidates them before the next build lands).
    derived: Mutex<Derived>,
    /// Held by whoever is rendering a body nobody has rendered yet: the
    /// requests that arrive meanwhile wait for it instead of each rendering
    /// their own. A new snapshot lands every few seconds with every body to
    /// render again, and a hundred requests in flight rendered a hundred
    /// copies of `/api/summary` at once - a megabyte each on a large fleet,
    /// enough to get a small container killed.
    rendering: Mutex<()>,
}

#[derive(Default)]
struct Derived {
    config: Option<Arc<Config>>,
    summaries: HashMap<Audience, Arc<Summary>>,
    bodies: HashMap<Body, Bytes>,
    pages: HashMap<Body, Arc<Spliced>>,
}

impl Derived {
    /// The memo, emptied first if it was derived against another config.
    fn for_config(&mut self, config: &Arc<Config>) -> &mut Self {
        if !self
            .config
            .as_ref()
            .is_some_and(|derived| Arc::ptr_eq(derived, config))
        {
            *self = Self {
                config: Some(Arc::clone(config)),
                ..Self::default()
            };
        }
        self
    }
}

impl Snapshot {
    fn new(built: Built) -> Self {
        Self {
            built,
            built_at: Instant::now(),
            derived: Mutex::new(Derived::default()),
            rendering: Mutex::new(()),
        }
    }

    fn derived(&self) -> std::sync::MutexGuard<'_, Derived> {
        self.derived.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn rendering(&self) -> std::sync::MutexGuard<'_, ()> {
        self.rendering
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The summary as `audience` sees it under `config`.
    pub(crate) fn summary(&self, config: &Arc<Config>, audience: &Audience) -> Arc<Summary> {
        if *audience == Audience::Operator {
            return Arc::clone(&self.built.summary);
        }
        if let Some(summary) = self.derived().for_config(config).summaries.get(audience) {
            return Arc::clone(summary);
        }
        // Derived outside the lock: a few hundred `Arc` clones, but no reason
        // to serialize concurrent first requests on it.
        let visibility = Visibility::new(config, audience);
        let summary = Arc::new(derive(&self.built, config, &visibility));
        self.derived()
            .for_config(config)
            .summaries
            .insert(audience.clone(), Arc::clone(&summary));
        summary
    }

    /// A rendered body, rendered once per snapshot. `render` returning `None`
    /// (a group nobody here may see) is not cached.
    pub(crate) fn body<E>(
        &self,
        config: &Arc<Config>,
        key: Body,
        render: impl FnOnce() -> Result<Option<Bytes>, E>,
    ) -> Result<Option<Bytes>, E> {
        let cached = || self.derived().for_config(config).bodies.get(&key).cloned();
        if let Some(body) = cached() {
            return Ok(Some(body));
        }
        // One render at a time; whoever waited finds it done.
        let _rendering = self.rendering();
        if let Some(body) = cached() {
            return Ok(Some(body));
        }
        let Some(body) = render()? else {
            return Ok(None);
        };
        self.derived()
            .for_config(config)
            .bodies
            .insert(key, body.clone());
        Ok(Some(body))
    }
}

impl Snapshot {
    /// A page with holes, rendered and cut once per snapshot (see
    /// [`Snapshot::body`]); each request splices its own parts in.
    pub(crate) fn page<E>(
        &self,
        config: &Arc<Config>,
        key: Body,
        render: impl FnOnce() -> Result<Option<String>, E>,
    ) -> Result<Option<Arc<Spliced>>, E> {
        let cached = || self.derived().for_config(config).pages.get(&key).cloned();
        if let Some(page) = cached() {
            return Ok(Some(page));
        }
        let _rendering = self.rendering();
        if let Some(page) = cached() {
            return Ok(Some(page));
        }
        let Some(rendered) = render()? else {
            return Ok(None);
        };
        let page = Arc::new(Spliced::new(rendered));
        self.derived()
            .for_config(config)
            .pages
            .insert(key, Arc::clone(&page));
        Ok(Some(page))
    }
}

/// The latest snapshot and the background task keeping it fresh.
pub(crate) struct Refresher {
    current: ArcSwapOption<Snapshot>,
    /// The generation of the latest published snapshot.
    published: watch::Sender<u64>,
    /// The generation of the latest build started.
    started: AtomicU64,
    wake: Arc<Notify>,
    spawned: AtomicBool,
    /// A request loaded the snapshot since the loop last chose its pause.
    read: AtomicBool,
    /// The loop is in a stretched pause: nobody was reading.
    idle: AtomicBool,
    cadence: Cadence,
}

impl Default for Refresher {
    fn default() -> Self {
        Self {
            current: ArcSwapOption::empty(),
            published: watch::Sender::new(0),
            started: AtomicU64::new(0),
            wake: Arc::new(Notify::new()),
            spawned: AtomicBool::new(false),
            read: AtomicBool::new(false),
            idle: AtomicBool::new(false),
            cadence: Cadence::default(),
        }
    }
}

impl Refresher {
    /// Start the background task (once; later calls are no-ops). The daemon
    /// calls it at startup so the first build overlaps the boot; a request
    /// arriving first starts it too.
    pub(crate) fn start(self: &Arc<Self>, state: &AppState) {
        if self.spawned.swap(true, Ordering::AcqRel) {
            return;
        }
        let refresher = Arc::downgrade(self);
        let wake = Arc::clone(&self.wake);
        let store = state.store.clone();
        let config = state.config.clone();
        let notifier = state.notifier.clone();
        let vantage = Arc::clone(&state.vantage);
        tokio::spawn(run(refresher, wake, store, config, notifier, vantage));
    }

    /// The latest snapshot. Waits on a build only for the very first one
    /// after start, and - bounded by [`FRESH_WAIT`] - when it is the read
    /// that ends an idle stretch and what is there has aged past
    /// [`STALE_AFTER`]. Either way the read is noted, which keeps the
    /// refresher at its watched pace.
    pub(crate) async fn snapshot(self: &Arc<Self>, state: &AppState) -> Arc<Snapshot> {
        if let Some(snapshot) = self.current.load_full() {
            self.read.store(true, Ordering::Release);
            if !self.idle.load(Ordering::Acquire)
                || snapshot.built_at.elapsed() <= self.cadence.stale_after
            {
                return snapshot;
            }
            let mut published = self.published.subscribe();
            let newer = *published.borrow_and_update() + 1;
            // A build already under way is the rebuild: waking the loop on
            // top of it would only queue a second one behind.
            if self.started.load(Ordering::Acquire) < newer {
                self.wake.notify_one();
            }
            let _ = tokio::time::timeout(
                self.cadence.fresh_wait,
                published.wait_for(|&generation| generation >= newer),
            )
            .await;
            return self.current.load_full().unwrap_or(snapshot);
        }
        self.start(state);
        let mut published = self.published.subscribe();
        // The sender lives in `self`, so the wait cannot fail.
        let _ = published.wait_for(|&generation| generation >= 1).await;
        self.current
            .load_full()
            .expect("a published generation has a snapshot")
    }

    /// Rebuild now and wait (bounded) until a snapshot that started after
    /// this call is published - for an operator write that should be visible
    /// on the very next page view.
    pub(crate) async fn refresh_now(self: &Arc<Self>, state: &AppState) {
        self.start(state);
        let target = self.started.load(Ordering::Acquire) + 1;
        let mut published = self.published.subscribe();
        self.wake.notify_one();
        let _ = tokio::time::timeout(
            WRITE_VISIBLE_TIMEOUT,
            published.wait_for(|&generation| generation >= target),
        )
        .await;
    }
}

/// The refresher loop. Holds the refresher weakly, so it ends with the app
/// state. A closed config channel only means no more reloads.
async fn run(
    refresher: Weak<Refresher>,
    wake: Arc<Notify>,
    store: Store,
    mut config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    vantage: VantageMap,
) {
    let mut state = BuildState::default();
    let mut built_for: Option<Arc<Config>> = None;
    let mut reloads = true;
    loop {
        let Some(this) = refresher.upgrade() else {
            return;
        };
        let current = config.borrow_and_update().clone();
        // The caches are keyed by monitor id and follow the windows on their
        // own; only an id coming back (its history may have been swept while
        // it was gone) must not find what they kept of it.
        if built_for
            .as_ref()
            .is_some_and(|previous| adds_monitors(previous, &current))
        {
            state = BuildState::default();
        }
        built_for = Some(Arc::clone(&current));
        this.idle.store(false, Ordering::Release);
        let generation = this.started.fetch_add(1, Ordering::AcqRel) + 1;
        let began = Instant::now();
        // Channel health is read live from the dispatcher; the vantage map is
        // the poller's last snapshot - a lock-free read, never the network.
        let health = notifications::health_snapshot(&notifier);
        let peers = vantage.load_full();
        // Built in a task of its own so the build's future is `'static` and
        // its caches move in and back out. It is no panic barrier in a
        // release build: the profile is `panic = "abort"`, a panic in the
        // build ends the process and the service manager restarts it. Only a
        // build that unwinds (debug, tests) takes the `Err` arm below, losing
        // this build's caches and keeping the refresher.
        let task = tokio::spawn({
            let store = store.clone();
            let current = Arc::clone(&current);
            let mut state = std::mem::take(&mut state);
            async move {
                let built = build_summary(&store, &current, &mut state, &health, &peers).await;
                (state, built)
            }
        });
        let took = match task.await {
            Ok((kept, built)) => {
                state = kept;
                this.current.store(Some(Arc::new(Snapshot::new(built))));
                this.published.send_replace(generation);
                let took = began.elapsed();
                if generation == 1 {
                    tracing::info!("status page ready (first build {took:.1?})");
                } else {
                    tracing::debug!("status summary rebuilt in {took:.1?}");
                }
                took
            }
            Err(err) => {
                tracing::error!("status summary build failed: {err}");
                began.elapsed()
            }
        };
        // Nobody read the snapshot since the last pause was chosen: stretch
        // this one. A reader arriving meanwhile wakes the loop itself.
        let watched = this.read.swap(false, Ordering::AcqRel);
        let pause = this.cadence.pause(took, watched);
        this.idle.store(!watched, Ordering::Release);
        drop(this);

        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            () = wake.notified() => {}
            changed = config.changed(), if reloads => {
                reloads = changed.is_ok();
            }
        }
    }
}

/// Whether `next` configures a monitor id `previous` did not.
fn adds_monitors(previous: &Config, next: &Config) -> bool {
    let known: std::collections::HashSet<&str> =
        previous.monitors.iter().map(|m| m.id.as_str()).collect();
    next.monitors
        .iter()
        .any(|monitor| !known.contains(monitor.id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unwatched_build_is_followed_by_the_idle_pause() {
        let cadence = Cadence::default();
        let quick = Duration::from_millis(200);
        assert_eq!(cadence.pause(quick, true), Duration::from_millis(4800));
        assert_eq!(cadence.pause(quick, false), IDLE_REFRESH);
        // A build slower than either pause still idles as long as it ran.
        let slow = Duration::from_secs(40);
        assert_eq!(cadence.pause(slow, true), slow);
        assert_eq!(cadence.pause(slow, false), slow);
    }

    async fn state() -> AppState {
        let config = hora_core::config::parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            "#,
        )
        .expect("config");
        let config = Arc::new(config);
        let client = hora_core::http::client(None).expect("client");
        let notifier = notifications::shared(&config, &client);
        // The sender may go: a closed channel only means no more reloads.
        let (_tx, rx) = watch::channel(config);
        AppState::new(Store::in_memory().await, rx, Arc::default(), notifier)
    }

    /// With nobody reading, the refresher stops rebuilding at its watched
    /// pace; the read that ends the stretch gets a snapshot built for it,
    /// not the one left from before.
    #[tokio::test]
    async fn the_refresher_idles_unread_and_rebuilds_for_the_next_reader() {
        let state = state().await;
        let refresher = Arc::new(Refresher {
            cadence: Cadence {
                active: Duration::from_millis(10),
                idle: Duration::from_secs(3600),
                stale_after: Duration::from_millis(50),
                fresh_wait: Duration::from_secs(10),
            },
            ..Refresher::default()
        });
        let first = refresher.snapshot(&state).await;

        // Unread, it settles: at 10 ms a build this would be dozens more.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let settled = refresher.started.load(Ordering::Acquire);
        assert!(settled <= 3, "{settled} builds with one early reader");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(refresher.started.load(Ordering::Acquire), settled);
        assert!(refresher.idle.load(Ordering::Acquire));

        let fresh = refresher.snapshot(&state).await;
        assert!(!Arc::ptr_eq(&first, &fresh));
        assert!(fresh.built_at.elapsed() < Duration::from_millis(300));
        assert_eq!(*refresher.published.borrow(), settled + 1);
    }

    /// Many requests reaching a snapshot nobody has rendered yet: one of
    /// them renders, the others take what it made.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_body_is_rendered_once_however_many_ask_at_once() {
        let state = state().await;
        let refresher = Arc::new(Refresher::default());
        let snapshot = refresher.snapshot(&state).await;
        let config = state.config.borrow().clone();
        let renders = Arc::new(AtomicU64::new(0));

        let waiting: Vec<_> = (0..32)
            .map(|_| {
                let (snapshot, config, renders) = (
                    Arc::clone(&snapshot),
                    Arc::clone(&config),
                    Arc::clone(&renders),
                );
                tokio::task::spawn_blocking(move || {
                    snapshot.body(&config, Body::Json(Audience::Public), || {
                        renders.fetch_add(1, Ordering::AcqRel);
                        // Long enough for every other caller to arrive.
                        std::thread::sleep(Duration::from_millis(100));
                        Ok::<_, std::convert::Infallible>(Some(Bytes::from_static(b"{}")))
                    })
                })
            })
            .collect();
        for body in waiting {
            let body = body.await.expect("joined").expect("infallible");
            assert_eq!(body.as_deref(), Some(&b"{}"[..]));
        }
        assert_eq!(renders.load(Ordering::Acquire), 1);
    }
}
