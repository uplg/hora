//! Short-lived memoisation of the heatmap and report bodies (the status
//! summary has its own background refresher, see `snapshot`). Every entry is
//! tied to the config snapshot it was built from, so a reload busts it at
//! once.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use hora_core::config::Config;

/// A value built from one config snapshot at one instant.
pub(crate) struct Cached<V> {
    at: Instant,
    config: Arc<Config>,
    value: Arc<V>,
}

impl<V> Cached<V> {
    pub(crate) fn new(config: &Arc<Config>, value: Arc<V>) -> Self {
        Self {
            at: Instant::now(),
            config: Arc::clone(config),
            value,
        }
    }

    fn fresh(&self, config: &Arc<Config>, ttl: Duration) -> bool {
        Arc::ptr_eq(&self.config, config) && self.at.elapsed() < ttl
    }
}

/// A keyed memo with lock-free reads: the map is swapped whole on insert
/// (copy-on-write, dropping stale entries on the way), so it only ever holds
/// live keys - bounded by what callers validate as a key (configured monitor
/// ids, retained months, configured groups).
pub(crate) struct Memo<K, V> {
    ttl: Duration,
    entries: ArcSwap<HashMap<K, Arc<Cached<V>>>>,
    /// Held while a missing entry is built, so a burst of requests hitting
    /// an expired entry builds it once (the rest wait and reuse it) instead
    /// of each running the same multi-week scan.
    build: tokio::sync::Mutex<()>,
}

impl<K: Eq + Hash + Clone, V> Memo<K, V> {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: ArcSwap::from_pointee(HashMap::new()),
            build: tokio::sync::Mutex::new(()),
        }
    }

    /// The fresh entry for `key`, or the one `build` makes - single-flight:
    /// concurrent misses wait for one build. A failed build caches nothing.
    pub(crate) async fn get_or_build<E>(
        &self,
        key: &K,
        config: &Arc<Config>,
        build: impl Future<Output = Result<V, E>>,
    ) -> Result<Arc<V>, E> {
        if let Some(value) = self.get(key, config) {
            return Ok(value);
        }
        let _building = self.build.lock().await;
        if let Some(value) = self.get(key, config) {
            return Ok(value);
        }
        let value = Arc::new(build.await?);
        self.insert(key, config, Arc::clone(&value));
        Ok(value)
    }

    pub(crate) fn get(&self, key: &K, config: &Arc<Config>) -> Option<Arc<V>> {
        let entries = self.entries.load();
        let cached = entries.get(key)?;
        cached
            .fresh(config, self.ttl)
            .then(|| Arc::clone(&cached.value))
    }

    pub(crate) fn insert(&self, key: &K, config: &Arc<Config>, value: Arc<V>) {
        let entry = Arc::new(Cached::new(config, value));
        self.entries.rcu(|entries| {
            let mut next: HashMap<K, Arc<Cached<V>>> = entries
                .iter()
                .filter(|(_, cached)| cached.fresh(config, self.ttl))
                .map(|(key, cached)| (key.clone(), Arc::clone(cached)))
                .collect();
            next.insert(key.clone(), Arc::clone(&entry));
            next
        });
    }
}

/// Probe clients for `/api/peer/probe`, one per proxy setting and
/// `ignore_tls`, so a mesh
/// confirming flaps reuses connection pools and TLS setup instead of building
/// a client per call. Rebuilt (emptied) when the config changes, since a
/// monitor's proxy may have. Generic over the client only so the tests can
/// count builds without a network stack.
pub(crate) struct ProbeClients<C = reqwest::Client> {
    inner: Mutex<Option<ProbeClientSet<C>>>,
}

impl<C> Default for ProbeClients<C> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}

struct ProbeClientSet<C> {
    config: Arc<Config>,
    clients: HashMap<(Option<String>, bool), C>,
}

impl<C: Clone> ProbeClients<C> {
    /// The cached client for `proxy` and `ignore_tls` under `config`, or a
    /// fresh one from `build` (cached on success).
    pub(crate) fn get_or_build<E>(
        &self,
        config: &Arc<Config>,
        proxy: Option<&str>,
        ignore_tls: bool,
        build: impl FnOnce(Option<&str>, bool) -> Result<C, E>,
    ) -> Result<C, E> {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let set = match guard.take() {
            Some(set) if Arc::ptr_eq(&set.config, config) => set,
            _ => ProbeClientSet {
                config: Arc::clone(config),
                clients: HashMap::new(),
            },
        };
        let set = guard.insert(set);
        let key = (proxy.map(str::to_owned), ignore_tls);
        if let Some(client) = set.clients.get(&key) {
            return Ok(client.clone());
        }
        let client = build(proxy, ignore_tls)?;
        set.clients.insert(key, client.clone());
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Arc<Config> {
        Arc::new(hora_core::config::parse("[page]\n[server]\n").expect("config"))
    }

    #[test]
    fn memo_serves_fresh_entries_and_busts_on_reload() {
        let memo: Memo<&str, u32> = Memo::new(Duration::from_mins(1));
        let config = config();
        assert!(memo.get(&"a", &config).is_none());
        memo.insert(&"a", &config, Arc::new(1));
        assert_eq!(memo.get(&"a", &config).as_deref(), Some(&1));
        // Another config snapshot (a reload) never reads the old entry...
        let reloaded = self::config();
        assert!(memo.get(&"a", &reloaded).is_none());
        // ...and inserting under it drops the stale one.
        memo.insert(&"b", &reloaded, Arc::new(2));
        assert_eq!(memo.entries.load().len(), 1);
    }

    #[tokio::test]
    async fn memo_builds_a_missing_entry_once_for_concurrent_misses() {
        let memo: Memo<&str, u32> = Memo::new(Duration::from_mins(1));
        let config = config();
        let builds = std::sync::atomic::AtomicU32::new(0);
        let build = || async {
            builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok::<_, ()>(7)
        };
        let (a, b, c) = tokio::join!(
            memo.get_or_build(&"k", &config, build()),
            memo.get_or_build(&"k", &config, build()),
            memo.get_or_build(&"k", &config, build()),
        );
        assert_eq!((*a.unwrap(), *b.unwrap(), *c.unwrap()), (7, 7, 7));
        assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A failure is not cached.
        let failed = memo
            .get_or_build(&"x", &config, async { Err::<u32, _>("no") })
            .await;
        assert_eq!(failed.unwrap_err(), "no");
        assert!(memo.get(&"x", &config).is_none());
    }

    #[test]
    fn memo_entries_expire() {
        let memo: Memo<&str, u32> = Memo::new(Duration::ZERO);
        let config = config();
        memo.insert(&"a", &config, Arc::new(1));
        assert!(memo.get(&"a", &config).is_none());
    }

    #[test]
    fn probe_clients_are_reused_per_proxy_and_rebuilt_on_reload() {
        let clients = ProbeClients::<String>::default();
        let config = config();
        let mut builds = 0;
        let mut build = |proxy: Option<&str>, ignore_tls: bool| -> Result<String, ()> {
            builds += 1;
            Ok(format!("client:{proxy:?}:{ignore_tls}"))
        };
        assert_eq!(
            clients.get_or_build(&config, None, false, &mut build),
            Ok("client:None:false".to_owned())
        );
        assert_eq!(
            clients.get_or_build(&config, None, false, &mut build),
            Ok("client:None:false".to_owned())
        );
        clients
            .get_or_build(&config, Some("socks5://proxy:1080"), false, &mut build)
            .unwrap();
        // An ignore_tls monitor never gets the verifying client, nor the
        // other way round.
        assert_eq!(
            clients.get_or_build(&config, None, true, &mut build),
            Ok("client:None:true".to_owned())
        );
        let reloaded = self::config();
        clients
            .get_or_build(&reloaded, None, false, &mut build)
            .unwrap();
        assert_eq!(
            builds, 4,
            "one build per proxy and certificate policy, plus one after the reload"
        );
    }
}
