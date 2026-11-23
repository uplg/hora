//! MQTT monitors against a real broker. Run with .github/scripts/mqtt-integration.sh.
use std::fmt::Write as _;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use hora_core::config::{Config, MonitorKind, MqttSpec};
use hora_core::mqtt::MqttHub;
use mqtt_glide::{ClientBuilder, QoS};
use tokio::sync::watch;

fn spec(config: &Config, id: &str) -> MqttSpec {
    let MonitorKind::Mqtt(spec) = &config.find_monitor(id).unwrap().spec else {
        panic!()
    };
    spec.clone()
}

async fn wait_status(hub: &MqttHub, spec: &MqttSpec, up: bool) {
    wait_cached_status(&mut hub.check(spec), up).await;
}

async fn wait_cached_status(check: &mut hora_core::mqtt::Check<'_>, up: bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while check.outcome().is_up() != up {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("MQTT status transition before deadline");
}

async fn docker(action: &str, container: &str) {
    let result = tokio::process::Command::new("docker")
        .args([action, container])
        .output()
        .await
        .unwrap();
    assert!(result.status.success(), "docker {action} failed");
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One broker lifecycle scenario with shared test resources.
#[ignore = "requires the isolated Mosquitto container created by .github/scripts/mqtt-integration.sh"]
async fn mqtt_mosquitto_retained_periodic_restart_reload_and_one_connection() {
    let url = std::env::var("HORA_TEST_MQTT_URL").expect("test broker URL");
    let container = std::env::var("HORA_TEST_MQTT_CONTAINER").expect("test broker container");
    let connected_count = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&connected_count);
    let publisher = ClientBuilder::new(&url)
        .unwrap()
        .on_inbound(move |message| {
            if message.topic == "$SYS/broker/clients/connected"
                && let Ok(value) = std::str::from_utf8(&message.payload).unwrap_or("").parse()
            {
                count.store(value, Ordering::Relaxed);
            }
        })
        .options(|o| o.keep_alive(Duration::from_secs(5)))
        .connect()
        .await
        .unwrap();
    publisher
        .subscribe([("$SYS/broker/clients/connected", QoS::AtMostOnce)])
        .await
        .unwrap();
    publisher
        .publish("test/state", "online")
        .retain(true)
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    let mut raw =
        format!("[page]\n[server]\n[[brokers]]\nname='test'\nurl='{url}'\nkeep_alive_secs=5\n");
    for i in 0..16 {
        write!(raw, "[[monitors]]\nid='state-{i}'\nname='State {i}'\nkind='mqtt'\nbroker='test'\ntopic='test/state'\nkeyword='online'\ninterval_secs=1\n").unwrap();
    }
    raw.push_str("[[monitors]]\nid='periodic'\nname='Periodic'\nkind='mqtt'\nbroker='test'\ntopic='test/periodic'\nkeyword='sensorData'\nmax_age_secs=1\ninterval_secs=1\n");
    raw.push_str("[[monitors]]\nid='metric'\nname='Metric'\nkind='mqtt'\nbroker='test'\ntopic='test/metric'\njson_query='$[?@ > 10]'\ninterval_secs=1\n");
    let config = Arc::new(hora_core::config::parse(&raw).unwrap());
    let retained = spec(&config, "state-0");
    let periodic = spec(&config, "periodic");
    let metric = spec(&config, "metric");
    let (config_tx, config_rx) = watch::channel(Arc::clone(&config));
    let (shutdown, stopped) = watch::channel(false);
    let (hub, task) = hora_core::mqtt::spawn(config_rx, stopped);
    let mut metric_check = hub.check(&metric);
    wait_status(&hub, &retained, true).await;
    assert!(!hub.outcome(&periodic).is_up());
    publisher
        .publish("test/periodic", "sensorData")
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_status(&hub, &periodic, true).await;
    publisher
        .publish("test/metric", r#"{"value":11}"#)
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_cached_status(&mut metric_check, true).await;
    publisher
        .publish("test/metric", r#"{"value":9}"#)
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_cached_status(&mut metric_check, false).await;
    wait_status(&hub, &periodic, false).await;
    // Broker counts both this publisher and Hora: 18 monitors add ONE client.
    tokio::time::timeout(Duration::from_secs(10), async {
        while connected_count.load(Ordering::Relaxed) != 2 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("one shared Hora connection plus publisher");
    config_tx.send(Arc::clone(&config)).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        hub.outcome(&retained).is_up(),
        "unrelated reload preserves cache"
    );
    docker("stop", &container).await;
    wait_status(&hub, &retained, false).await;
    assert_eq!(
        metric_check.outcome().reason,
        Some(hora_core::probe::FailureKind::ConnectionFailed)
    );
    docker("start", &container).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !publisher.is_connected() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|err| panic!("{err}: {:?}", publisher.session_phase().borrow()));
    // Persistence is off: a pre-disconnect payload must never revive the monitor.
    assert!(!hub.outcome(&periodic).is_up());
    assert!(!metric_check.outcome().is_up());
    // Reconnected with nothing cached yet: the scheduler's view gives no
    // verdict within the grace instead of a false down.
    let mut settling = hub.check(&periodic).with_grace(Duration::from_mins(1));
    tokio::time::timeout(Duration::from_secs(20), async {
        while settling.verdict().is_some() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("no verdict right after the reconnect");
    publisher
        .publish("test/metric", r#"{"value":11}"#)
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_cached_status(&mut metric_check, true).await;
    // The same cached message must be re-evaluated under changed assertions.
    let changed_raw = raw.replace("json_query='$[?@ > 10]'", "json_query='$[?@ > 20]'");
    let changed = Arc::new(hora_core::config::parse(&changed_raw).unwrap());
    let changed_metric = spec(&changed, "metric");
    config_tx.send(changed).unwrap();
    wait_cached_status(&mut hub.check(&changed_metric), false).await;
    publisher
        .publish("test/state", "online")
        .retain(true)
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_status(&hub, &retained, true).await;
    publisher
        .publish("test/periodic", "sensorData")
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_status(&hub, &periodic, true).await;
    let mut removed = (*config).clone();
    removed.monitors.clear();
    config_tx.send(Arc::new(removed)).unwrap();
    wait_status(&hub, &retained, false).await;
    // Recreating the broker hub must not reuse a previous message generation.
    config_tx.send(Arc::clone(&config)).unwrap();
    wait_status(&hub, &retained, true).await;
    assert!(!metric_check.outcome().is_up());
    publisher
        .publish("test/metric", r#"{"value":9}"#)
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_cached_status(&mut metric_check, false).await;
    publisher
        .publish("test/metric", r#"{"value":11}"#)
        .qos(QoS::AtLeastOnce)
        .send()
        .await
        .unwrap();
    wait_cached_status(&mut metric_check, true).await;
    shutdown.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    publisher.shutdown().await.unwrap();
}
