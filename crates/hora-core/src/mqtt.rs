//! MQTT monitors (`kind = "mqtt"`): one shared mqtt-glide connection per
//! `[[brokers]]` entry, subscribed to exactly the topics its monitors name,
//! each topic caching only its latest payload (bounded by the broker's
//! packet limit). A monitor's check reads that cache: no probe goes out.

use std::collections::{HashMap, HashSet};
use std::num::{NonZeroU16, NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, Weak};
use std::time::Duration;

use mqtt_glide::{ClientBuilder, LifecycleHook, MqttClient, QoS, SessionPhase, StaticCredentials};
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{Config, MonitorKind, MqttSpec, Secret};
use crate::probe::{FailureKind, Outcome};

/// A `[[brokers]]` entry: the connection MQTT monitors name with `broker`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Broker {
    /// What monitors call it (`broker = "..."`), unique.
    pub name: String,
    /// `mqtt://host:port` or `mqtts://host:port`; credentials go in the
    /// fields below, never in the URL.
    pub url: Secret,
    #[serde(default)]
    pub username: Option<Secret>,
    #[serde(default)]
    pub password: Option<Secret>,
    /// Largest packet accepted (KiB, 1 to 1024, default 64): also the
    /// largest payload a topic cache holds.
    #[serde(default = "default_packet_kb")]
    pub max_packet_kb: u32,
    /// MQTT keep-alive (5 to 300 s, default 15).
    #[serde(default = "default_keep_alive")]
    pub keep_alive_secs: u64,
}
fn default_packet_kb() -> u32 {
    64
}
fn default_keep_alive() -> u64 {
    15
}

/// The endpoint a broker connects to with its user: two brokers sharing it
/// would only open a second connection for the same subscriptions.
pub(crate) type BrokerIdentity = (String, String, u16, Option<String>);

/// One broker's own settings: URL shape, credentials and bounds. Returns its
/// endpoint identity for the duplicate check.
pub(crate) fn check_broker(broker: &Broker) -> anyhow::Result<BrokerIdentity> {
    anyhow::ensure!(!broker.name.is_empty(), "brokers: a broker needs a name");
    let name = &broker.name;
    let url = reqwest::Url::parse(broker.url.as_ref())
        .map_err(|_| anyhow::anyhow!("broker {name}: invalid url"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "mqtt" | "mqtts")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/"),
        "broker {name}: url must be mqtt(s)://host:port (credentials go in username/password)"
    );
    anyhow::ensure!(
        ClientBuilder::new(broker.url.as_ref()).is_ok(),
        "broker {name}: invalid url"
    );
    anyhow::ensure!(
        broker.password.is_none() || broker.username.is_some(),
        "broker {name}: password requires username"
    );
    anyhow::ensure!(
        (1..=1024).contains(&broker.max_packet_kb),
        "broker {name}: max_packet_kb must be between 1 and 1024"
    );
    anyhow::ensure!(
        (5..=300).contains(&broker.keep_alive_secs),
        "broker {name}: keep_alive_secs must be between 5 and 300"
    );
    Ok((
        url.scheme().to_owned(),
        url.host_str().unwrap_or_default().to_owned(),
        url.port()
            .unwrap_or(if url.scheme() == "mqtts" { 8883 } else { 1883 }),
        broker
            .username
            .as_ref()
            .map(|user| user.as_ref().to_owned()),
    ))
}

/// The brokers on their own and against each other (unique names, one entry
/// per endpoint and user), and every MQTT monitor's `broker` against them.
pub(crate) fn validate(config: &Config) -> anyhow::Result<()> {
    let mut names = HashSet::new();
    let mut endpoints = HashSet::new();
    for broker in &config.brokers {
        let identity = check_broker(broker)?;
        anyhow::ensure!(
            names.insert(&broker.name),
            "broker {}: duplicate name",
            broker.name
        );
        anyhow::ensure!(
            endpoints.insert(identity),
            "broker {}: same endpoint and user as another broker; share one entry",
            broker.name
        );
    }
    for monitor in &config.monitors {
        if let MonitorKind::Mqtt(spec) = &monitor.spec {
            anyhow::ensure!(
                names.contains(&spec.broker),
                "monitor {}: unknown broker {:?}",
                monitor.id,
                spec.broker
            );
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Message {
    payload: Arc<[u8]>,
    received: Instant,
    generation: u64,
}

impl Message {
    fn new(payload: &[u8], received: Instant) -> Self {
        // Shared across broker replacements, so a cache in an unchanged monitor
        // cannot mistake the first message after a reload for an older one.
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        Self {
            payload: Arc::from(payload),
            received,
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
        }
    }
}

struct ContentCache {
    generation: u64,
    error: Option<String>,
}

#[derive(Default)]
struct Topic {
    message: Option<Message>,
    allowed: bool,
}

struct BrokerState {
    topics: Mutex<HashMap<String, Topic>>,
    client: OnceLock<MqttClient>,
    cap: usize,
    /// When this connection was set up: the start of the first grace.
    started: Instant,
    /// The latest (re)connect; `None` until the first one.
    connected_at: Mutex<Option<Instant>>,
}

impl BrokerState {
    fn new(topics: &[String], cap: usize) -> Self {
        Self {
            topics: Mutex::new(
                topics
                    .iter()
                    .map(|s| (s.clone(), Topic::default()))
                    .collect(),
            ),
            client: OnceLock::new(),
            cap,
            started: Instant::now(),
            connected_at: Mutex::new(None),
        }
    }

    fn connected(&self, now: Instant) {
        *self
            .connected_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(now);
    }

    fn connected_at(&self) -> Option<Instant> {
        *self
            .connected_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn receive(&self, topic: &str, payload: &[u8], now: Instant) {
        let mut topics = self.topics.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = topics.get_mut(topic) {
            // Copy exactly the payload, avoiding retaining a large codec buffer.
            entry.message = (payload.len() <= self.cap).then(|| Message::new(payload, now));
        }
    }

    fn clear(&self) {
        for entry in self
            .topics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values_mut()
        {
            entry.message = None;
        }
    }

    /// The topic's verdict, or `None` while the connection is still settling
    /// (see [`settling`]) and has nothing to judge yet.
    fn outcome(
        &self,
        spec: &MqttSpec,
        now: Instant,
        grace: Duration,
        cache: &mut Option<ContentCache>,
    ) -> Option<Outcome> {
        let Some(client) = self.client.get().filter(|c| c.is_connected()) else {
            // Never connected yet: still starting up, within the grace.
            if self.connected_at().is_none() && settling(Some(self.started), now, grace) {
                return None;
            }
            return Some(Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT broker disconnected".into(),
            ));
        };
        if client
            .refused_subscriptions()
            .borrow()
            .iter()
            .any(|s| &*s.filter == spec.topic.as_str())
        {
            return Some(Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT subscription refused".into(),
            ));
        }
        let (allowed, message) = self
            .topics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&spec.topic)
            .map_or((false, None), |t| (t.allowed, t.message.clone()));
        // Right after a (re)connect the cache is empty: a retained topic
        // arrives with the subscription, a periodic one only when next
        // published. Nothing to judge yet is no verdict, within the grace.
        if (!allowed || message.is_none()) && settling(self.connected_at(), now, grace) {
            return None;
        }
        if !allowed {
            return Some(Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT subscription not ready".into(),
            ));
        }
        Some(evaluate(spec, message.as_ref(), now, Some(cache)))
    }
}

/// Whether a connection made at `since` is still inside its grace: the
/// first messages after a (re)connect are on their way.
fn settling(since: Option<Instant>, now: Instant, grace: Duration) -> bool {
    since.is_some_and(|since| now.saturating_duration_since(since) < grace)
}

/// How long after a (re)connect a monitor with no message yet is not judged:
/// one check interval, or `max_age_secs` when that is shorter - a periodic
/// topic is due within it anyway.
#[must_use]
pub fn grace(interval_secs: u64, spec: &MqttSpec) -> Duration {
    Duration::from_secs(
        spec.max_age_secs
            .map_or(interval_secs, |age| age.min(interval_secs)),
    )
}

struct Lifecycle(Weak<BrokerState>);
impl LifecycleHook for Lifecycle {
    fn on_lifecycle(&self, phase: &SessionPhase) {
        let Some(state) = self.0.upgrade() else {
            return;
        };
        // A new session starts empty: nothing cached survives a disconnect.
        if matches!(phase, SessionPhase::Connected { .. }) {
            state.connected(Instant::now());
        } else {
            state.clear();
        }
    }
}

fn evaluate(
    spec: &MqttSpec,
    message: Option<&Message>,
    now: Instant,
    cache: Option<&mut Option<ContentCache>>,
) -> Outcome {
    let Some(message) = message else {
        return Outcome::down(FailureKind::NoMessage, "MQTT message not received".into());
    };
    if spec.max_age_secs.is_some_and(|age| {
        now.saturating_duration_since(message.received) > Duration::from_secs(age)
    }) {
        return Outcome::down(FailureKind::NoMessage, "MQTT message is stale".into());
    }
    // Keyword-only checks are cheaper than memoization for tiny state strings.
    // Keep just the last JSON assertion result, never a payload or parsed tree.
    if let Some(cache) = cache.filter(|_| spec.json.is_some()) {
        if let Some(cached) = cache
            .as_ref()
            .filter(|c| c.generation == message.generation)
        {
            return cached.error.as_ref().map_or_else(
                || Outcome::up(false, None, None),
                |error| Outcome::down(FailureKind::Content, error.clone()),
            );
        }
        let outcome = evaluate_content(spec, message);
        *cache = Some(ContentCache {
            generation: message.generation,
            error: outcome.error.clone(),
        });
        return outcome;
    }
    evaluate_content(spec, message)
}

#[cfg(test)]
thread_local! {
    /// How often this thread judged a payload's content.
    static CONTENT_EVALUATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn evaluate_content(spec: &MqttSpec, message: &Message) -> Outcome {
    #[cfg(test)]
    CONTENT_EVALUATIONS.with(|count| count.set(count.get() + 1));
    if spec.max_body_kb.is_some_and(|kb| {
        message.payload.len()
            > usize::try_from(kb)
                .unwrap_or(usize::MAX)
                .saturating_mul(1024)
    }) {
        return Outcome::down(
            FailureKind::Content,
            "MQTT payload exceeds assertion limit".into(),
        );
    }
    let text = String::from_utf8_lossy(&message.payload);
    if let Some(keyword) = &spec.keyword
        && let Some(failure) = crate::probe::check_keyword(keyword, &text)
    {
        return Outcome::down(FailureKind::Content, failure);
    }
    if let Some(json) = &spec.json
        && let Some(failure) = crate::probe::check_json(json, &text)
    {
        return Outcome::down(FailureKind::Content, failure);
    }
    Outcome::up(false, None, None)
}

#[derive(Default)]
pub struct MqttHub {
    brokers: RwLock<HashMap<String, Arc<BrokerState>>>,
}

/// One monitor's evaluator. Its borrowed specification cannot change under a
/// cached result; a changed monitor gets a new evaluator on supervisor restart.
/// Broker health and freshness are checked on every call, including cache hits.
pub struct Check<'a> {
    hub: &'a MqttHub,
    spec: &'a MqttSpec,
    grace: Duration,
    cache: Option<ContentCache>,
}

impl MqttHub {
    /// An evaluator kept for the lifetime of one monitor task.
    #[must_use]
    pub fn check<'a>(&'a self, spec: &'a MqttSpec) -> Check<'a> {
        Check {
            hub: self,
            spec,
            grace: Duration::ZERO,
            cache: None,
        }
    }

    /// A one-off check. Repeated callers should keep a [`Check`] instead.
    #[must_use]
    pub fn outcome(&self, spec: &MqttSpec) -> Outcome {
        self.check(spec).outcome()
    }
}

impl Check<'_> {
    /// Leave a monitor unjudged for this long after a (re)connect while its
    /// topic has no message yet (see [`grace`]).
    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Evaluate the latest message; only unchanged JSON content is memoized.
    /// Ignores the grace: a topic with nothing cached is down.
    #[must_use]
    pub fn outcome(&mut self) -> Outcome {
        let grace = std::mem::replace(&mut self.grace, Duration::ZERO);
        let outcome = self.verdict();
        self.grace = grace;
        outcome.unwrap_or_else(|| {
            Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT broker not ready".into(),
            )
        })
    }

    /// The scheduler's view: like [`Check::outcome`], but `None` (no verdict,
    /// nothing recorded) while the connection is inside its grace and the
    /// topic has nothing cached yet - so a restart or a reconnect does not
    /// read as an outage of every non-retained topic.
    #[must_use]
    pub fn verdict(&mut self) -> Option<Outcome> {
        let state = self
            .hub
            .brokers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&self.spec.broker)
            .cloned();
        match state {
            Some(state) => state.outcome(self.spec, Instant::now(), self.grace, &mut self.cache),
            None => Some(Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT broker not ready".into(),
            )),
        }
    }
}

struct Running {
    spec: Broker,
    topics: Vec<String>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// Start one client per used broker. Unchanged brokers survive config reloads.
#[must_use]
pub fn spawn(
    mut config: watch::Receiver<Arc<Config>>,
    mut shutdown: watch::Receiver<bool>,
) -> (Arc<MqttHub>, JoinHandle<()>) {
    let hub = Arc::new(MqttHub::default());
    let shared = Arc::clone(&hub);
    let task = tokio::spawn(async move {
        let mut running: HashMap<String, Running> = HashMap::new();
        loop {
            let snapshot = config.borrow_and_update().clone();
            let desired: HashMap<String, (Broker, Vec<String>)> = snapshot
                .brokers
                .iter()
                .filter_map(|broker| {
                    let mut topics: Vec<_> = snapshot
                        .monitors
                        .iter()
                        .filter_map(|m| match &m.spec {
                            MonitorKind::Mqtt(s) if s.broker == broker.name => {
                                Some(s.topic.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    topics.sort();
                    topics.dedup();
                    (!topics.is_empty()).then(|| (broker.name.clone(), (broker.clone(), topics)))
                })
                .collect();
            let obsolete: Vec<_> = running
                .iter()
                .filter(|(name, run)| {
                    desired
                        .get(*name)
                        .is_none_or(|(b, topics)| *b != run.spec || *topics != run.topics)
                        || run.task.is_finished()
                })
                .map(|(n, _)| n.clone())
                .collect();
            for name in obsolete {
                shared
                    .brokers
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&name);
                if let Some(run) = running.remove(&name) {
                    let _ = run.stop.send(true);
                    let _ = run.task.await;
                }
            }
            for (name, (spec, topics)) in desired {
                if running.contains_key(&name) {
                    continue;
                }
                let state = Arc::new(BrokerState::new(
                    &topics,
                    usize::try_from(spec.max_packet_kb).unwrap_or(1024) * 1024,
                ));
                shared
                    .brokers
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.clone(), Arc::clone(&state));
                let (stop, stopped) = watch::channel(false);
                let task = tokio::spawn(run_broker(spec.clone(), topics.clone(), state, stopped));
                running.insert(
                    name,
                    Running {
                        spec,
                        topics,
                        stop,
                        task,
                    },
                );
            }
            tokio::select! {
                changed = config.changed() => { if changed.is_err() { break; } }
                _ = shutdown.changed() => break,
                // Recover a worker that stopped unexpectedly without waking per monitor.
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
        shared
            .brokers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        for run in running.values() {
            let _ = run.stop.send(true);
        }
        for (_, run) in running {
            let _ = run.task.await;
        }
    });
    (hub, task)
}

async fn run_broker(
    spec: Broker,
    topics: Vec<String>,
    state: Arc<BrokerState>,
    mut stopped: watch::Receiver<bool>,
) {
    let Ok(builder) = ClientBuilder::new(spec.url.as_ref()) else {
        return;
    };
    let inbound = Arc::downgrade(&state);
    let mut builder = builder
        .on_inbound(move |msg| {
            if let Some(state) = inbound.upgrade() {
                state.receive(msg.topic.as_ref(), &msg.payload, Instant::now());
            }
        })
        .lifecycle(Lifecycle(Arc::downgrade(&state)))
        .wait_connected(false)
        .options(|o| {
            o.keep_alive(Duration::from_secs(spec.keep_alive_secs))
                .max_packet_size(
                    NonZeroU32::new(spec.max_packet_kb * 1024).expect("validated packet limit"),
                )
                .receive_maximum(NonZeroU16::new(4).unwrap())
                .command_channel_capacity(NonZeroUsize::new(8).unwrap())
        });
    if let Some(username) = &spec.username {
        builder = builder.credentials(StaticCredentials::new(
            username.as_ref(),
            spec.password
                .as_ref()
                .map_or("", AsRef::as_ref)
                .as_bytes()
                .to_vec(),
        ));
    }
    let Ok(client) = builder.connect().await else {
        tracing::warn!(broker = %spec.name, "MQTT client initialization failed");
        return;
    };
    let _ = state.client.set(client.clone());
    let mut phase = client.session_phase();
    let mut subscribed = false;
    loop {
        phase.borrow_and_update();
        if *stopped.borrow() {
            break;
        }
        if !subscribed && client.is_connected() {
            let result = tokio::select! {
                result = client.subscribe(topics.iter().map(|t| (t.as_str(), QoS::AtLeastOnce))) => result,
                _ = stopped.changed() => break,
            };
            if let Ok(ack) = result {
                let mut cache = state.topics.lock().unwrap_or_else(PoisonError::into_inner);
                for (topic, reason) in topics.iter().zip(&ack.reasons) {
                    if let Some(entry) = cache.get_mut(topic) {
                        entry.allowed = reason.is_granted();
                    }
                }
                subscribed =
                    ack.reasons.len() == topics.len() && ack.reasons.iter().all(|r| r.is_granted());
            }
        }
        if subscribed {
            let _ = stopped.changed().await;
            break;
        }
        tokio::select! {
            _ = stopped.changed() => break,
            changed = phase.changed() => { if changed.is_err() { break; } }
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
    state.clear();
    let _ = tokio::time::timeout(Duration::from_secs(3), client.shutdown()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> MqttSpec {
        MqttSpec {
            broker: "test".into(),
            topic: "device/state".into(),
            max_age_secs: Some(10),
            keyword: Some(crate::config::KeywordAssert {
                text: "online".into(),
                invert: false,
            }),
            json: None,
            max_body_kb: None,
        }
    }
    fn json_assert(query: &str) -> crate::config::JsonAssert {
        crate::config::JsonAssert {
            query: crate::config::Compiled::parse(query).unwrap(),
            expected: None,
        }
    }
    #[test]
    fn mqtt_missing_stale_keyword_inversion_and_bounded_cache() {
        let now = Instant::now();
        let mut spec = spec();
        assert!(!evaluate(&spec, None, now, None).is_up());
        let msg = Message::new(b"online", now);
        assert!(evaluate(&spec, Some(&msg), now, None).is_up());
        assert!(!evaluate(&spec, Some(&msg), now + Duration::from_secs(11), None).is_up());
        spec.keyword.as_mut().unwrap().invert = true;
        assert!(!evaluate(&spec, Some(&msg), now, None).is_up());
        let state = BrokerState::new(&[spec.topic.clone()], 16);
        state.receive("unexpected", b"data", now);
        state.receive(&spec.topic, b"online", now);
        assert_eq!(state.topics.lock().unwrap().len(), 1);
        state.receive(&spec.topic, &[0; 17], now);
        assert!(state.topics.lock().unwrap()[&spec.topic].message.is_none());
        state.receive(&spec.topic, b"online", now);
        state.clear();
        assert!(state.topics.lock().unwrap()[&spec.topic].message.is_none());
    }
    #[test]
    fn mqtt_reuses_json_assertions_and_fails_bad_json() {
        let now = Instant::now();
        let mut spec = spec();
        spec.keyword = None;
        spec.json = Some(json_assert("$[?@ > 100]"));
        for (payload, up) in [
            (br#"{"value":101}"#.as_slice(), true),
            (br#"{"value":99}"#.as_slice(), false),
            (b"null".as_slice(), false),
            (b"invalid".as_slice(), false),
        ] {
            let message = Message::new(payload, now);
            assert_eq!(evaluate(&spec, Some(&message), now, None).is_up(), up);
        }
    }

    #[test]
    fn memoized_content_keeps_freshness_and_new_messages_live() {
        let now = Instant::now();
        let mut spec = spec();
        spec.keyword = None;
        spec.max_age_secs = Some(900);
        spec.json = Some(json_assert("$[?@ > 10]"));
        let mut cache = None;
        for (payload, up) in [
            (br#"{"value":11}"#.as_slice(), true),
            (br#"{"value":9}"#.as_slice(), false),
            (b"invalid".as_slice(), false),
            (br#"{"value":12}"#.as_slice(), true),
        ] {
            // Identical timestamps must not make different messages share results.
            let message = Message::new(payload, now);
            for _ in 0..3 {
                let outcome = evaluate(&spec, Some(&message), now, Some(&mut cache));
                assert_eq!(outcome.is_up(), up);
                assert_eq!(
                    outcome.error,
                    evaluate(&spec, Some(&message), now, None).error
                );
            }
            assert_eq!(
                Arc::strong_count(&message.payload),
                1,
                "cache retained the payload"
            );
        }
        let message = Message::new(br#"{"value":11}"#, now);
        assert!(evaluate(&spec, Some(&message), now, Some(&mut cache)).is_up());
        assert!(
            evaluate(
                &spec,
                Some(&message),
                now + Duration::from_mins(15),
                Some(&mut cache)
            )
            .is_up()
        );
        let stale = evaluate(
            &spec,
            Some(&message),
            now + Duration::from_secs(901),
            Some(&mut cache),
        );
        assert_eq!(stale.reason, Some(FailureKind::NoMessage));
        assert!(!evaluate(&spec, None, now, Some(&mut cache)).is_up());
    }

    /// A task's evaluator judges the content once per received message,
    /// however many ticks find it unchanged in the cache; a message received
    /// again (same bytes) is judged again, once.
    #[test]
    fn an_unchanged_message_is_judged_once() {
        let mut spec = spec();
        spec.keyword = None;
        spec.json = Some(json_assert("$[?@ > 100]"));
        let evaluations = || CONTENT_EVALUATIONS.with(std::cell::Cell::get);
        let now = Instant::now();
        let mut cache = None;
        let message = Message::new(br#"{"power":230}"#, now);
        let before = evaluations();
        for tick in 0..10 {
            let at = now + Duration::from_secs(tick);
            assert!(evaluate(&spec, Some(&message), at, Some(&mut cache)).is_up());
        }
        assert_eq!(evaluations() - before, 1);
        let again = Message::new(br#"{"power":230}"#, now);
        for _ in 0..10 {
            assert!(evaluate(&spec, Some(&again), now, Some(&mut cache)).is_up());
        }
        assert_eq!(evaluations() - before, 2);
    }

    #[test]
    fn evaluators_do_not_share_assertions_and_keyword_checks_do_not_cache() {
        let now = Instant::now();
        let mut cache = None;
        assert!(
            evaluate(
                &spec(),
                Some(&Message::new(b"online", now)),
                now,
                Some(&mut cache)
            )
            .is_up()
        );
        assert!(cache.is_none());
        let json = |expected: i64| MqttSpec {
            keyword: None,
            json: Some(crate::config::JsonAssert {
                query: crate::config::Compiled::parse("$.value").unwrap(),
                expected: Some(expected.to_string()),
            }),
            ..spec()
        };
        let first = json(11);
        let second = json(12);
        let hub = MqttHub::default();
        let mut first_check = hub.check(&first);
        let mut second_check = hub.check(&second);
        let message = Message::new(br#"{"value":11}"#, now);
        assert!(
            evaluate(
                first_check.spec,
                Some(&message),
                now,
                Some(&mut first_check.cache)
            )
            .is_up()
        );
        assert!(
            !evaluate(
                second_check.spec,
                Some(&message),
                now,
                Some(&mut second_check.cache)
            )
            .is_up()
        );

        let mut limited = first.clone();
        limited.max_body_kb = Some(1);
        let mut limited_check = hub.check(&limited);
        let message = Message::new(&[b' '; 1025], now);
        for _ in 0..2 {
            let outcome = evaluate(
                limited_check.spec,
                Some(&message),
                now,
                Some(&mut limited_check.cache),
            );
            assert_eq!(
                outcome.error.as_deref(),
                Some("MQTT payload exceeds assertion limit")
            );
        }
    }

    #[test]
    fn a_settling_connection_gives_no_verdict_until_its_grace_ends() {
        let now = Instant::now();
        let grace = Duration::from_secs(30);
        assert!(!settling(None, now, grace));
        assert!(settling(Some(now), now + Duration::from_secs(29), grace));
        assert!(!settling(Some(now), now + grace, grace));
        assert!(!settling(Some(now), now, Duration::ZERO));

        // Never connected: starting up, no verdict within the grace...
        let spec = spec();
        let state = BrokerState::new(std::slice::from_ref(&spec.topic), 16);
        let mut cache = None;
        let started = state.started;
        assert!(state.outcome(&spec, started, grace, &mut cache).is_none());
        // ... down once it has passed, and at once without a grace.
        let late = state
            .outcome(&spec, started + grace, grace, &mut cache)
            .unwrap();
        assert_eq!(late.reason, Some(FailureKind::ConnectionFailed));
        assert!(
            state
                .outcome(&spec, started, Duration::ZERO, &mut cache)
                .is_some()
        );
        // A connection that was up and dropped is an outage, grace or not.
        state.connected(started);
        let dropped = state.outcome(&spec, started, grace, &mut cache).unwrap();
        assert_eq!(dropped.error.as_deref(), Some("MQTT broker disconnected"));
    }

    #[test]
    fn the_grace_is_one_interval_or_the_shorter_max_age() {
        let mut spec = spec();
        spec.max_age_secs = Some(900);
        assert_eq!(grace(300, &spec), Duration::from_secs(300));
        spec.max_age_secs = Some(120);
        assert_eq!(grace(300, &spec), Duration::from_secs(120));
        spec.max_age_secs = None;
        assert_eq!(grace(60, &spec), Duration::from_mins(1));
    }

    #[test]
    fn an_unknown_broker_is_down_and_a_missing_message_reads_as_no_message() {
        let hub = MqttHub::default();
        let spec = spec();
        let mut check = hub.check(&spec).with_grace(Duration::from_secs(60));
        let down = check.verdict().unwrap();
        assert_eq!(down.error.as_deref(), Some("MQTT broker not ready"));
        assert!(!check.outcome().is_up());
        let missing = evaluate(&spec, None, Instant::now(), None);
        assert_eq!(missing.reason, Some(FailureKind::NoMessage));
        assert_eq!(
            crate::probe::public_reason(missing.reason, missing.error.as_deref().unwrap()),
            "no recent message"
        );
    }
}
