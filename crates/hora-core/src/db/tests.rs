use super::aggregates::iso_day;
use super::certs::cert_not_after;
use super::checks::{insert_heartbeat_miss_at, insert_push_at};
use super::retention::{
    CHECKS, ORPHAN_GRACE_SECS, PRUNE_SLICE_SECS, delete_oldest_slice, delete_orphans,
    prune_announcements, prune_events, prune_incidents, prune_silences,
};
use super::*;
use crate::SECONDS_PER_DAY;
use crate::config::Config;
use crate::probe::FailureKind;
use crate::status::CheckStatus;

#[tokio::test]
async fn a_release_alerts_once_and_a_new_project_starts_afresh() {
    let store = Store::in_memory().await;
    assert_eq!(release_watch(&store, "chat").await.unwrap(), None);

    upsert_release_watch(&store, "chat", "a/b", "v1.9.2", "https://x/v1.9.2", 100)
        .await
        .unwrap();
    mark_release_notified(&store, "chat", "v1.9.2")
        .await
        .unwrap();

    // The same project looked up again: the alert already sent is remembered.
    upsert_release_watch(&store, "chat", "a/b", "v1.9.2", "https://x/v1.9.2", 200)
        .await
        .unwrap();
    let stored = release_watch(&store, "chat")
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(stored.checked_at, 200);
    assert_eq!(stored.notified.as_deref(), Some("v1.9.2"));

    // Another project's tags mean nothing to this one.
    upsert_release_watch(&store, "chat", "c/d", "v1.9.2", "https://y/v1.9.2", 300)
        .await
        .unwrap();
    let stored = release_watch(&store, "chat")
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(stored.project, "c/d");
    assert_eq!(stored.notified, None);
}

async fn insert(store: &Store, id: &str, time: i64, status: i64, latency: Option<i64>) {
    sqlx::query(
        "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
             VALUES (?, ?, ?, ?, NULL, NULL)",
    )
    .bind(time)
    .bind(id)
    .bind(status)
    .bind(latency)
    .execute(store.sqlx())
    .await
    .expect("insert check");
}

#[tokio::test]
async fn the_failure_kind_is_stored_with_its_detail() {
    let store = Store::in_memory().await;
    let outcome =
        crate::probe::Outcome::down(FailureKind::Content, "keyword missing: x".to_owned());
    insert_check(&store, "m", &outcome).await.unwrap();
    insert_push_at(&store, "m", CheckStatus::Down, None, Some("disk full"), 1)
        .await
        .unwrap();
    // A row from before the column (NULL), and one from a newer version.
    sqlx::query("INSERT INTO checks (time, monitor_id, status, error) VALUES (2, 'm', 0, 'x')")
        .execute(store.sqlx())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO checks (time, monitor_id, status, error, reason) \
         VALUES (3, 'm', 0, 'x', 'solar_flare')",
    )
    .execute(store.sqlx())
    .await
    .unwrap();

    let rows = recent_checks(&store, "m", 10).await.unwrap();
    let kinds: Vec<_> = rows.iter().map(|row| row.reason).collect();
    assert_eq!(
        kinds,
        [
            Some(FailureKind::Content),
            Some(FailureKind::Other),
            None,
            Some(FailureKind::Pushed)
        ]
    );

    let id = insert_incident_start(
        &store,
        "m",
        Some("HTTP 500: trace"),
        Some(FailureKind::Http),
        None,
        &[],
        None,
        None,
    )
    .await
    .unwrap();
    let incident = incident_by_id(&store, id).await.unwrap().unwrap();
    assert_eq!(incident.reason, Some(FailureKind::Http));
}

#[tokio::test]
async fn availability_latest_and_series() {
    let store = Store::in_memory().await;
    insert(&store, "m", 100, 1, Some(10)).await;
    insert(&store, "m", 200, 0, None).await;
    insert(&store, "m", 300, 1, Some(20)).await;

    assert_eq!(availability(&store, "m", 0).await.unwrap(), (2, 3));

    let recent = recent_checks(&store, "m", 2).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(
        (recent[0].time, recent[0].status, recent[0].latency_ms),
        (300, CheckStatus::Up, Some(20))
    );
    assert_eq!(
        recent.iter().map(|c| c.status).collect::<Vec<_>>(),
        vec![CheckStatus::Up, CheckStatus::Down]
    );

    let series = latency_series(&store, "m", 0, 1).await.unwrap();
    assert_eq!(series.len(), 2);
    assert_eq!(series[0].latency_ms, 10);
}

#[tokio::test]
async fn last_heartbeat_ignores_recorded_misses() {
    let store = Store::in_memory().await;
    insert(&store, "m", 100, 1, Some(5)).await; // positive heartbeat
    insert(&store, "m", 200, 0, None).await; // recorded miss (fresher)

    // last_check_time sees the miss; last_heartbeat_time skips it, so staleness
    // is measured from the real heartbeat and an outage can't reset the clock.
    assert_eq!(last_check_time(&store, "m").await.unwrap(), Some(200));
    assert_eq!(last_heartbeat_time(&store, "m").await.unwrap(), Some(100));

    // A monitor with only misses has no positive heartbeat at all.
    insert(&store, "only-miss", 50, 0, None).await;
    assert_eq!(
        last_heartbeat_time(&store, "only-miss").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn explicit_down_push_is_a_heartbeat_but_a_miss_is_not() {
    let store = Store::in_memory().await;
    insert_push_at(&store, "job", CheckStatus::Up, None, None, 100)
        .await
        .unwrap();
    insert_push_at(
        &store,
        "job",
        CheckStatus::Down,
        None,
        Some("backup failed"),
        200,
    )
    .await
    .unwrap();
    insert_heartbeat_miss_at(
        &store,
        "job",
        FailureKind::MissingHeartbeat,
        "missing heartbeat",
        300,
    )
    .await
    .unwrap();

    // The newest heartbeat is the explicit down push, message included;
    // the fresher recorded miss is skipped.
    let beat = last_heartbeat(&store, "job").await.unwrap().unwrap();
    assert_eq!(beat.time, 200);
    assert_eq!(beat.status, CheckStatus::Down);
    assert_eq!(beat.error.as_deref(), Some("backup failed"));
}

#[tokio::test]
async fn same_second_pushes_keep_the_last_write() {
    let store = Store::in_memory().await;
    // `status=up` at the start of a job, `status=down` when it fails, both
    // within the same second: the failure must survive.
    insert_push_at(&store, "job", CheckStatus::Up, Some(5), None, 100)
        .await
        .unwrap();
    insert_push_at(
        &store,
        "job",
        CheckStatus::Down,
        None,
        Some("disk full"),
        100,
    )
    .await
    .unwrap();
    let beat = last_heartbeat(&store, "job").await.unwrap().unwrap();
    assert_eq!((beat.time, beat.status), (100, CheckStatus::Down));
    assert_eq!(beat.error.as_deref(), Some("disk full"));
    assert_eq!(beat.latency_ms, None);
    let rows = recent_checks(&store, "job", 10).await.unwrap();
    assert_eq!(rows.len(), 1, "one row per second");
}

#[tokio::test]
async fn push_replaces_a_same_second_miss_and_never_the_reverse() {
    let store = Store::in_memory().await;
    // The scheduler records a miss, then the late heartbeat lands in the
    // same second: the heartbeat wins, ending the miss streak.
    insert_heartbeat_miss_at(
        &store,
        "job",
        FailureKind::MissingHeartbeat,
        "missing heartbeat",
        100,
    )
    .await
    .unwrap();
    insert_push_at(&store, "job", CheckStatus::Up, None, None, 100)
        .await
        .unwrap();
    let beat = last_heartbeat(&store, "job").await.unwrap().unwrap();
    assert_eq!((beat.time, beat.status), (100, CheckStatus::Up));

    // The other order: a miss racing a push it did not see must not erase it.
    insert_push_at(&store, "job", CheckStatus::Up, None, None, 200)
        .await
        .unwrap();
    insert_heartbeat_miss_at(
        &store,
        "job",
        FailureKind::MissingHeartbeat,
        "missing heartbeat",
        200,
    )
    .await
    .unwrap();
    let beat = last_heartbeat(&store, "job").await.unwrap().unwrap();
    assert_eq!((beat.time, beat.status), (200, CheckStatus::Up));
    assert_eq!(
        recent_checks(&store, "job", 10).await.unwrap()[0].status,
        CheckStatus::Up
    );
}

#[tokio::test]
async fn latency_percentiles_match_nearest_rank() {
    let store = Store::in_memory().await;
    // Latencies 10,20,..,100 (n=10) for "x"; a down check (no latency) is ignored.
    let latencies = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
    for (i, &latency) in latencies.iter().enumerate() {
        insert(
            &store,
            "x",
            1000 + i64::try_from(i).unwrap(),
            1,
            Some(latency),
        )
        .await;
    }
    insert(&store, "x", 2000, 0, None).await;
    insert(&store, "y", 1000, 1, Some(42)).await; // single sample

    let pcts = latency_percentiles_all(&store, 0).await.unwrap();
    // rank = ceil(p*n/100): p50 -> 5th = 50, p95 & p99 -> 10th = 100.
    assert_eq!(pcts["x"], (50, 100, 100));
    // n = 1: every percentile is the only value.
    assert_eq!(pcts["y"], (42, 42, 42));
}

#[tokio::test]
async fn latency_sparkline_buckets_and_averages() {
    let store = Store::in_memory().await;
    insert(&store, "m", 0, 1, Some(10)).await;
    insert(&store, "m", 50, 1, Some(30)).await; // same 100s bucket as t=0 -> avg 20
    insert(&store, "m", 150, 1, Some(80)).await; // next bucket
    insert(&store, "m", 120, 0, None).await; // no latency -> ignored

    let spark = latency_sparkline_all(&store, 0, 100).await.unwrap();
    let points = &spark["m"];
    assert_eq!(points.len(), 2);
    assert_eq!(points[0].latency_ms, 20); // (10 + 30) / 2
    assert_eq!(points[1].latency_ms, 80);

    // Buckets are anchored to the epoch, not to `since`: moving the window
    // start must not regroup the rows (t=90 and t=110 straddle an epoch
    // boundary, but would share a bucket anchored at t=50).
    insert(&store, "n", 90, 1, Some(10)).await;
    insert(&store, "n", 110, 1, Some(30)).await;
    let shifted = latency_sparkline_all(&store, 50, 100).await.unwrap();
    let points: Vec<i64> = shifted["n"].iter().map(|p| p.latency_ms).collect();
    assert_eq!(points, vec![10, 30]);
}

#[tokio::test]
async fn iso_day_matches_sqlite_strftime() {
    let store = Store::in_memory().await;
    for secs in [0_i64, 86_399, 86_400, 951_782_400, 1_789_695_050] {
        let sqlite: String = sqlx::query_scalar("SELECT strftime('%Y-%m-%d', ?, 'unixepoch')")
            .bind(secs)
            .fetch_one(store.sqlx())
            .await
            .unwrap();
        assert_eq!(iso_day(secs / SECONDS_PER_DAY), sqlite, "{secs}");
    }
}

#[tokio::test]
async fn daily_aggregates_by_utc_day() {
    let store = Store::in_memory().await;
    let day0 = 1_609_459_200; // 2021-01-01 00:00:00 UTC
    insert(&store, "m", day0, 1, Some(5)).await;
    insert(&store, "m", day0 + 10, 0, None).await;
    insert(&store, "m", day0 + SECONDS_PER_DAY, 1, Some(7)).await;

    let rows = &daily_all(&store, 0, day0 + 2 * SECONDS_PER_DAY)
        .await
        .unwrap()["m"];
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].day.as_str(), rows[0].up, rows[0].down),
        ("2021-01-01", 1, 1)
    );
    assert_eq!((rows[1].day.as_str(), rows[1].up), ("2021-01-02", 1));
}

#[tokio::test]
async fn latency_hourly_reads_buckets_below_the_frontier_and_raw_above() {
    let store = Store::in_memory().await;
    let hour0 = 10 * 86_400;
    let hour1 = hour0 + 3600;

    // hour0 is rolled up (with a doctored average) and still has raw rows;
    // hour1 is raw only; an older hour is a bucket only.
    sqlx::query(
        "INSERT INTO checks_hourly \
                (monitor_id, hour, up_count, down_count, degraded_count, avg_latency_ms) \
             VALUES ('m', ?, 10, 0, 0, 999), ('m', ?, 5, 0, 0, 50)",
    )
    .bind(hour0)
    .bind(hour0 - 3600)
    .execute(store.sqlx())
    .await
    .unwrap();
    insert(&store, "m", hour0 + 10, 1, Some(100)).await;
    insert(&store, "m", hour0 + 20, 1, Some(200)).await;
    insert(&store, "m", hour1 + 10, 1, Some(300)).await;

    let cells = latency_hourly(&store, "m", 0).await.unwrap();
    // Oldest first; a rolled-up hour answers from its bucket (the raw rows
    // below the frontier are never scanned), the hour above it from raw.
    assert_eq!(cells, vec![(hour0 - 3600, 50), (hour0, 999), (hour1, 300)]);

    // `since` trims the window.
    let recent = latency_hourly(&store, "m", hour0).await.unwrap();
    assert_eq!(recent.len(), 2);
}

#[tokio::test]
async fn cert_upsert_roundtrip() {
    let store = Store::in_memory().await;
    assert_eq!(cert_not_after(&store, "m").await.unwrap(), None);
    upsert_cert(&store, "m", 1000, 1).await.unwrap();
    upsert_cert(&store, "m", 2000, 2).await.unwrap();
    assert_eq!(cert_not_after(&store, "m").await.unwrap(), Some(2000));
}

#[tokio::test]
async fn batch_reads_group_by_monitor() {
    let store = Store::in_memory().await;
    insert(&store, "a", 100, 1, Some(10)).await;
    insert(&store, "a", 200, 0, None).await; // down, no latency
    insert(&store, "b", 150, 1, Some(20)).await;
    upsert_cert(&store, "a", 5000, 1).await.unwrap();

    let availability = availability_all(&store, 0).await.unwrap();
    assert_eq!(availability.get("a"), Some(&(1, 2))); // 1 up of 2
    assert_eq!(availability.get("b"), Some(&(1, 1)));

    let daily = daily_all(&store, 0, 300).await.unwrap();
    assert!(daily.contains_key("a") && daily.contains_key("b"));

    let certs = cert_all(&store).await.unwrap();
    assert_eq!(certs.get("a"), Some(&5000));
    assert!(!certs.contains_key("b"));
}

#[tokio::test]
async fn prune_removes_orphans_and_expired_rows() {
    let store = Store::in_memory().await;
    let now = chrono::Utc::now().timestamp();

    insert(&store, "keep", now - 10, 1, Some(5)).await; // recent, retained
    insert(&store, "keep", now - 200 * SECONDS_PER_DAY, 1, Some(5)).await; // beyond retention
    insert(&store, "gone", now - 10, 1, Some(5)).await; // monitor removed from config
    upsert_cert(&store, "gone", now + 1000, now).await.unwrap();
    meta_set(&store, "heartbeat_expected_since:gone", "5")
        .await
        .unwrap();
    meta_set(&store, "heartbeat_expected_since:keep", "5")
        .await
        .unwrap();
    // A push monitor removed before it ever pinged: no rows, only state.
    meta_set(&store, "heartbeat_expected_since:never", "5")
        .await
        .unwrap();

    let config: Config = toml::from_str(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "keep"
            name = "Keep"
            target = "https://example.com"
            interval_secs = 60
        "#,
    )
    .unwrap();

    prune(&store, &config).await.unwrap();

    // The retained monitor keeps only its recent row.
    assert_eq!(latency_series(&store, "keep", 0, 1).await.unwrap().len(), 1);

    // The orphan is only scheduled for deletion: its data survives the
    // grace period, and the first sighting is recorded.
    assert_eq!(recent_checks(&store, "gone", 10).await.unwrap().len(), 1);
    let mark = meta_get(&store, "orphan_since:gone").await.unwrap();
    assert!(mark.is_some_and(|at| at.parse::<i64>().unwrap() >= now));
    let expected = |id: &str| {
        let store = store.clone();
        let key = format!("heartbeat_expected_since:{id}");
        async move { meta_get(&store, &key).await.unwrap() }
    };
    assert!(expected("gone").await.is_some(), "kept through the grace");
    assert!(expected("keep").await.is_some());
    assert_eq!(expected("never").await, None, "nothing to wait for");

    // A week later the sweep deletes it entirely, mark included.
    delete_orphans(
        &store,
        &config,
        now + ORPHAN_GRACE_SECS + 60,
        false,
        &super::retention::Pace::offline(),
    )
    .await
    .unwrap();
    assert!(recent_checks(&store, "gone", 10).await.unwrap().is_empty());
    assert_eq!(cert_not_after(&store, "gone").await.unwrap(), None);
    assert_eq!(meta_get(&store, "orphan_since:gone").await.unwrap(), None);
    assert_eq!(
        expected("gone").await,
        None,
        "per-id meta goes with the rows"
    );
    assert!(expected("keep").await.is_some());
    assert_eq!(recent_checks(&store, "keep", 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_spent_budget_stops_the_prune_and_says_so() {
    let store = Store::in_memory().await;
    for t in (0..3600).step_by(60) {
        insert(&store, "a", 1_000_000 + t, 1, Some(5)).await;
    }
    let ids = r#"["a"]"#;
    let cutoff = 1_000_000 + SECONDS_PER_DAY;
    let count = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM checks")
            .fetch_one(store.sqlx())
            .await
            .unwrap()
    };

    // Out of time before the first slice: nothing goes, and the pace says
    // rows are left, so the maintenance loop comes back for them soon.
    let spent = super::retention::Pace::expired();
    super::retention::delete_sliced(&store, CHECKS, Some(ids), cutoff, PRUNE_SLICE_SECS, &spent)
        .await
        .unwrap();
    assert_eq!(count().await, 60);
    assert!(!spent.finished());

    // Without a deadline the same call runs to the cutoff and finishes.
    let offline = super::retention::Pace::offline();
    super::retention::delete_sliced(
        &store,
        CHECKS,
        Some(ids),
        cutoff,
        PRUNE_SLICE_SECS,
        &offline,
    )
    .await
    .unwrap();
    assert_eq!(count().await, 0);
    assert!(offline.finished());
}

#[tokio::test]
async fn a_prune_slice_never_deletes_more_than_one_slice() {
    let store = Store::in_memory().await;
    let base = 1_000_000;
    // Two pruned monitors checked every minute for an hour, one more check
    // after a ten-day gap, one just past the cutoff, and a monitor with
    // another retention.
    for t in (0..3600).step_by(60) {
        insert(&store, "a", base + t, 1, Some(5)).await;
        insert(&store, "b", base + t, 1, Some(5)).await;
    }
    let cutoff = base + 20 * SECONDS_PER_DAY;
    insert(&store, "a", base + 10 * SECONDS_PER_DAY, 1, Some(5)).await;
    insert(&store, "a", cutoff, 1, Some(5)).await;
    insert(&store, "other", base, 1, Some(5)).await;
    let pruned = || async {
        sqlx::query_as::<_, (i64, Option<i64>)>(
            "SELECT COUNT(*), MIN(time) FROM checks WHERE monitor_id IN ('a', 'b')",
        )
        .fetch_one(store.sqlx())
        .await
        .unwrap()
    };

    let ids = r#"["a", "b"]"#;
    let mut slices = 0;
    loop {
        let (before, oldest) = pruned().await;
        let Some(deleted) =
            delete_oldest_slice(&store, CHECKS, Some(ids), cutoff, PRUNE_SLICE_SECS)
                .await
                .unwrap()
        else {
            break;
        };
        slices += 1;
        let (after, next) = pruned().await;
        let oldest = oldest.expect("a slice ran, so rows were left");
        // Exactly the rows of one slice went: five minutes of two monitors.
        assert!(deleted <= 10, "slice {slices} deleted {deleted} rows");
        assert_eq!(before - after, i64::try_from(deleted).unwrap());
        assert!(next.is_none_or(|next| next >= oldest + PRUNE_SLICE_SECS));
    }
    // Twelve slices for the hour, one for the lone check: the gap between
    // them costs no statement.
    assert_eq!(slices, 13);
    assert_eq!(
        pruned().await,
        (1, Some(cutoff)),
        "the cutoff's own row stays"
    );
    assert_eq!(recent_checks(&store, "other", 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_orphan_that_comes_back_keeps_its_history() {
    let store = Store::in_memory().await;
    let now = 1_000_000;
    insert(&store, "api", now - 10, 1, Some(5)).await;
    let config_with = |ids: &[&str]| -> Config {
        let [id] = ids else { unreachable!() };
        toml::from_str(&format!(
            "[page]\n[server]\n[[monitors]]\nid = \"{id}\"\nname = \"{id}\"\n\
                 target = \"https://example.com\"\ninterval_secs = 60\n"
        ))
        .unwrap()
    };

    // Renamed away: marked, not deleted.
    delete_orphans(
        &store,
        &config_with(&["other"]),
        now,
        false,
        &super::retention::Pace::offline(),
    )
    .await
    .unwrap();
    assert!(
        meta_get(&store, "orphan_since:api")
            .await
            .unwrap()
            .is_some()
    );

    // Back before the grace ran out: the mark is cleared, the data kept.
    delete_orphans(
        &store,
        &config_with(&["api"]),
        now + 3600,
        false,
        &super::retention::Pace::offline(),
    )
    .await
    .unwrap();
    assert_eq!(meta_get(&store, "orphan_since:api").await.unwrap(), None);

    // Removed again much later: a fresh grace period starts then, so the
    // old (cleared) sighting can never shortcut it.
    let later = now + 30 * SECONDS_PER_DAY;
    delete_orphans(
        &store,
        &config_with(&["other"]),
        later,
        false,
        &super::retention::Pace::offline(),
    )
    .await
    .unwrap();
    delete_orphans(
        &store,
        &config_with(&["other"]),
        later + 3600,
        false,
        &super::retention::Pace::offline(),
    )
    .await
    .unwrap();
    assert_eq!(recent_checks(&store, "api", 10).await.unwrap().len(), 1);
    assert_eq!(
        meta_get(&store, "orphan_since:api").await.unwrap(),
        Some(later.to_string())
    );
}

#[test]
fn upsert_sql_spells_out_every_column() {
    assert_eq!(
        upsert_sql!("certs", "monitor_id", "not_after", "checked_at"),
        "INSERT INTO certs (monitor_id, not_after, checked_at) VALUES (?, ?, ?) \
             ON CONFLICT(monitor_id) DO UPDATE SET \
             not_after = excluded.not_after, checked_at = excluded.checked_at"
    );
    assert_eq!(
        upsert_sql!("meta", "key", "value"),
        "INSERT INTO meta (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    );
}

#[tokio::test]
async fn incidents_between_selects_every_overlap() {
    let store = Store::in_memory().await;
    let span = |monitor: &str, started: i64, ended: Option<i64>| {
        sqlx::query(
            "INSERT INTO incidents (monitor_id, started_at, ended_at, created_at) \
                 VALUES (?, ?, ?, ?)",
        )
        .bind(monitor.to_owned())
        .bind(started)
        .bind(ended)
        .bind(started)
    };
    span("before", 10, Some(90))
        .execute(store.sqlx())
        .await
        .unwrap();
    span("straddles-start", 50, Some(150))
        .execute(store.sqlx())
        .await
        .unwrap();
    span("inside", 120, Some(130))
        .execute(store.sqlx())
        .await
        .unwrap();
    span("open", 140, None).execute(store.sqlx()).await.unwrap();
    span("straddles-end", 190, Some(260))
        .execute(store.sqlx())
        .await
        .unwrap();
    span("after", 200, Some(210))
        .execute(store.sqlx())
        .await
        .unwrap();

    let ids: Vec<String> = incidents_between(&store, 100, 200)
        .await
        .unwrap()
        .into_iter()
        .map(|incident| incident.monitor_id)
        .collect();
    // Newest first; ended exactly at `since` or started at `until` is out.
    assert_eq!(ids, ["straddles-end", "open", "inside", "straddles-start"]);
}

#[tokio::test]
async fn incident_marks_give_the_last_end_and_the_open_start() {
    let store = Store::in_memory().await;
    for (monitor, started, ended) in [
        ("calm", 10, Some(20)),
        ("calm", 30, Some(45)),
        ("down", 10, Some(20)),
        ("down", 50, None),
        ("first", 70, None),
    ] {
        sqlx::query(
            "INSERT INTO incidents (monitor_id, started_at, ended_at, created_at) \
                 VALUES (?, ?, ?, ?)",
        )
        .bind(monitor)
        .bind(started)
        .bind(ended)
        .bind(started)
        .execute(store.sqlx())
        .await
        .unwrap();
    }
    let marks = super::incident_marks(&store).await.unwrap();
    let mark = |last_end, open_since| super::IncidentMarks {
        last_end,
        open_since,
    };
    assert_eq!(marks["calm"], mark(Some(45), None));
    assert_eq!(marks["down"], mark(Some(20), Some(50)));
    assert_eq!(marks["first"], mark(None, Some(70)));
    assert!(!marks.contains_key("never"));

    // One monitor's own incidents, newest first, from `since`.
    let starts: Vec<i64> = super::monitor_incidents(&store, "calm", 20, 10)
        .await
        .unwrap()
        .iter()
        .map(|incident| incident.started_at)
        .collect();
    assert_eq!(starts, [30]);
    assert_eq!(
        super::monitor_incidents(&store, "down", 0, 1)
            .await
            .unwrap()[0]
            .started_at,
        50
    );
}

#[tokio::test]
async fn daily_all_ignores_data_past_until() {
    let store = Store::in_memory().await;
    let day0 = 10 * SECONDS_PER_DAY;
    insert(&store, "m", day0 + 10, 1, Some(5)).await;
    insert(&store, "m", day0 + SECONDS_PER_DAY + 10, 0, None).await;
    downsample_hourly(&store, day0 + 3 * SECONDS_PER_DAY)
        .await
        .unwrap();
    insert(&store, "m", day0 + 3 * SECONDS_PER_DAY + 10, 0, None).await;

    // Only the first day: neither the later bucket nor the raw tail.
    let bars = daily_all(&store, 0, day0 + SECONDS_PER_DAY - 1)
        .await
        .unwrap();
    assert_eq!(bars["m"].len(), 1);
    assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 0));
}

#[tokio::test]
async fn downsampling_is_write_once_and_only_buckets_complete_hours() {
    let store = Store::in_memory().await;
    let hour0 = 10 * 86400; // aligned on both an hour and a day
    let hour1 = hour0 + 3600;

    insert(&store, "m", hour0 + 10, 1, Some(100)).await;
    insert(&store, "m", hour0 + 20, 1, Some(200)).await;
    insert(&store, "m", hour0 + 30, 0, None).await;
    insert(&store, "m", hour1 + 10, 1, Some(300)).await;

    // Cutoff inside hour1: only hour0 is entirely below it, so only hour0
    // is bucketed.
    downsample_hourly(&store, hour1 + 1800).await.unwrap();
    let buckets = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT hour, up_count, down_count, avg_latency_ms FROM checks_hourly ORDER BY hour",
    )
    .fetch_all(store.sqlx())
    .await
    .unwrap();
    assert_eq!(buckets, vec![(hour0, 2, 1, 150)]);

    // Write-once: a late raw row never rewrites an existing bucket.
    insert(&store, "m", hour0 + 40, 0, None).await;
    downsample_hourly(&store, hour1 + 3600).await.unwrap();
    let buckets = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT hour, up_count, down_count, avg_latency_ms FROM checks_hourly ORDER BY hour",
    )
    .fetch_all(store.sqlx())
    .await
    .unwrap();
    assert_eq!(buckets, vec![(hour0, 2, 1, 150), (hour1, 1, 0, 300)]);

    // Daily roll-up weights the latency average by sample count:
    // (150 * 2 + 300 * 1) / 3 = 200.
    downsample_daily(&store, hour0 + 86400).await.unwrap();
    let days = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT day, up_count, down_count, avg_latency_ms FROM checks_daily",
    )
    .fetch_all(store.sqlx())
    .await
    .unwrap();
    assert_eq!(days, vec![(hour0, 3, 1, 200)]);
}

#[tokio::test]
async fn downsampling_only_scans_above_the_newest_bucket() {
    let store = Store::in_memory().await;
    let hour0 = 10 * 86400;
    let hour1 = hour0 + 3600;

    // A bucket already exists for hour1: the floor for the next run.
    insert(&store, "m", hour1 + 10, 1, Some(100)).await;
    downsample_hourly(&store, hour1 + 3600).await.unwrap();

    // Raw rows below the floor - a backdated write, which live recording
    // never produces - are outside the incremental scan and stay
    // unbucketed; a second monitor's rows above the floor still roll up
    // (the floor is global, monotonic time covers every monitor).
    insert(&store, "m", hour0 + 10, 1, Some(999)).await;
    insert(&store, "other", hour1 + 7200 + 10, 0, None).await;
    downsample_hourly(&store, hour1 + 3 * 3600).await.unwrap();

    let buckets = sqlx::query_as::<_, (String, i64)>(
        "SELECT monitor_id, hour FROM checks_hourly ORDER BY hour",
    )
    .fetch_all(store.sqlx())
    .await
    .unwrap();
    assert_eq!(
        buckets,
        vec![("m".to_owned(), hour1), ("other".to_owned(), hour1 + 7200)]
    );
}

#[tokio::test]
async fn daily_all_reads_aggregates_once_raw_is_pruned() {
    let store = Store::in_memory().await;
    let hour0 = 10 * 86400;

    insert(&store, "m", hour0 + 10, 1, Some(100)).await;
    insert(&store, "m", hour0 + 20, 0, None).await;
    downsample_hourly(&store, hour0 + 3600).await.unwrap();

    // Raw still present: counts come from it (and agree with the bucket).
    let day_key = "1970-01-11";
    let bars = daily_all(&store, 0, hour0 + 3600).await.unwrap();
    assert_eq!(bars["m"][0].day, day_key);
    assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 1));

    // Raw pruned: the hourly bucket transparently takes over.
    sqlx::query("DELETE FROM checks")
        .execute(store.sqlx())
        .await
        .unwrap();
    let bars = daily_all(&store, 0, hour0 + 3600).await.unwrap();
    assert_eq!(bars["m"][0].day, day_key);
    assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 1));
}

#[tokio::test]
async fn daily_all_adds_the_raw_tail_to_the_rolled_up_hours() {
    let store = Store::in_memory().await;
    let day0 = 10 * 86400;

    // 01:00 is rolled up, 05:00 is still raw: the same day is split across
    // both sources, and the bar must count both halves.
    insert(&store, "m", day0 + 3600 + 10, 1, Some(100)).await;
    downsample_hourly(&store, day0 + 2 * 3600).await.unwrap();
    insert(&store, "m", day0 + 5 * 3600 + 10, 0, None).await;
    let now = day0 + 6 * 3600;
    let bars = daily_all(&store, 0, now).await.unwrap();
    assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 1));

    // Rolling the tail up moves it to the buckets without double counting.
    downsample_hourly(&store, now).await.unwrap();
    let bars = daily_all(&store, 0, now).await.unwrap();
    assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 1));
}

#[tokio::test]
async fn daily_all_reads_old_days_from_buckets_not_raw() {
    let store = Store::in_memory().await;
    let now = 100 * 86400;
    let old = now - 30 * SECONDS_PER_DAY; // well past the raw window
    let recent = now - 3600; // inside the raw window

    // The old day exists as raw rows *and* a (complete) hourly bucket; the
    // bucket must be the source consulted - raw that old is never scanned.
    // The doctored bucket disagrees with raw at the *same* sample count, so
    // if raw were still read it would win the tie (first writer) and the
    // day would come out down instead.
    insert(&store, "m", old, 0, None).await;
    downsample_hourly(&store, old + 3600).await.unwrap();
    sqlx::query("UPDATE checks_hourly SET up_count = 1, down_count = 0")
        .execute(store.sqlx())
        .await
        .unwrap();
    insert(&store, "m", recent, 1, Some(10)).await;

    let bars = daily_all(&store, 0, now).await.unwrap();
    assert_eq!(bars["m"].len(), 2);
    // Old day: the doctored bucket answers, proving raw was not read.
    assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 0));
    // Recent day: raw is still authoritative inside the window.
    assert_eq!((bars["m"][1].up, bars["m"][1].down), (1, 0));
}

#[tokio::test]
async fn recent_checks_carry_the_failure_reason() {
    let store = Store::in_memory().await;
    insert(&store, "m", 100, 1, Some(10)).await;
    sqlx::query(
        "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
             VALUES (200, 'm', 0, NULL, 522, 'HTTP 522: origin timeout')",
    )
    .execute(store.sqlx())
    .await
    .unwrap();

    let recent = recent_checks(&store, "m", 2).await.unwrap();
    assert_eq!(recent[0].error.as_deref(), Some("HTTP 522: origin timeout"));
    assert_eq!(recent[1].error, None);
}

#[tokio::test]
async fn incidents_open_close_and_prune() {
    let store = Store::in_memory().await;

    let id = insert_incident_start(
        &store,
        "m",
        Some("boom"),
        None,
        None,
        &["a".to_owned()],
        Some("HTTP/2 503\n\n<html>maintenance</html>"),
        Some("deploy api v2.3, 3 min before"),
    )
    .await
    .unwrap();
    assert_eq!(find_open_incident(&store, "m").await.unwrap(), Some(id));

    update_incident_end(&store, id).await.unwrap();
    assert_eq!(find_open_incident(&store, "m").await.unwrap(), None);

    let incidents = recent_incidents(&store, 10).await.unwrap();
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].error.as_deref(), Some("boom"));
    assert_eq!(
        incidents[0].snapshot.as_deref(),
        Some("HTTP/2 503\n\n<html>maintenance</html>")
    );
    assert_eq!(
        incidents[0].event.as_deref(),
        Some("deploy api v2.3, 3 min before")
    );
    assert!(incidents[0].duration_s.is_some());

    // The vantage verdict lands after the peers answered; readable back.
    update_incident_vantage(&store, id, "confirmed down from 2/2 vantage points")
        .await
        .unwrap();
    assert_eq!(
        incident_by_id(&store, id)
            .await
            .unwrap()
            .expect("incident exists")
            .vantage
            .as_deref(),
        Some("confirmed down from 2/2 vantage points")
    );
    assert!(incident_by_id(&store, 999).await.unwrap().is_none());

    // Closed incidents prune by age; open ones never do.
    let open = insert_incident_start(&store, "m", None, None, None, &[], None, None)
        .await
        .unwrap();
    prune_incidents(&store, chrono::Utc::now().timestamp() + 1000)
        .await
        .unwrap();
    let incidents = recent_incidents(&store, 10).await.unwrap();
    assert_eq!(incidents.len(), 1);
    assert_eq!(incidents[0].id, open);
}

#[tokio::test]
async fn incident_notes_set_clear_and_resolve_last() {
    let store = Store::in_memory().await;
    let first = insert_incident_start(&store, "m", None, None, None, &[], None, None)
        .await
        .unwrap();
    let second = insert_incident_start(&store, "m", None, None, None, &[], None, None)
        .await
        .unwrap();

    // `last` resolves to the most recently started incident.
    assert_eq!(latest_incident_id(&store).await.unwrap(), Some(second));

    assert!(set_incident_note(&store, first, "fiber cut").await.unwrap());
    let incidents = recent_incidents(&store, 10).await.unwrap();
    let annotated = incidents.iter().find(|i| i.id == first).unwrap();
    assert_eq!(annotated.note.as_deref(), Some("fiber cut"));

    // An empty note clears; an unknown id reports "not found".
    assert!(set_incident_note(&store, first, "").await.unwrap());
    let incidents = recent_incidents(&store, 10).await.unwrap();
    assert_eq!(incidents.iter().find(|i| i.id == first).unwrap().note, None);
    assert!(!set_incident_note(&store, 999, "nope").await.unwrap());
}

#[tokio::test]
async fn events_record_list_correlate_and_prune() {
    let store = Store::in_memory().await;
    let now = chrono::Utc::now().timestamp();

    let first = insert_event(&store, "deploy api v2.3").await.unwrap();
    let second = insert_event(&store, "config rollout").await.unwrap();

    // Newest first for the list; oldest first for the chart overlay.
    let recent = recent_events(&store, 10).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].id, second);
    assert_eq!(recent[0].title, "config rollout");
    let overlay = events_since(&store, now - 60).await.unwrap();
    assert_eq!(overlay[0].id, first);

    // Correlation picks the most recent event inside the window only.
    let correlated = latest_event_before(&store, now, 3600).await.unwrap();
    assert_eq!(correlated.expect("in window").id, second);
    let outside = latest_event_before(&store, now + 7200, 3600).await.unwrap();
    assert!(
        outside.is_none(),
        "events older than the window never match"
    );

    // Old markers age out with the pruner.
    prune_events(&store, now + 1000).await.unwrap();
    assert!(recent_events(&store, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_announcement_with_a_bad_severity_reads_as_info() {
    let store = Store::in_memory().await;
    // Only a hand edit can store this; it must not break the page listing it.
    sqlx::query(
        "INSERT INTO announcements (title, severity, created_at) VALUES ('Odd', 'panic', 1)",
    )
    .execute(store.sqlx())
    .await
    .unwrap();
    let pinned = active_announcements(&store, 10).await.unwrap();
    assert_eq!(pinned[0].severity, crate::config::Severity::Info);
}

#[tokio::test]
async fn announcements_pin_expire_and_clear() {
    let store = Store::in_memory().await;
    let now = 1000;

    let id = insert_announcement(
        &store,
        "Fiber cut",
        "ETA 6pm",
        crate::config::Severity::Warning,
        Some(now + 600),
    )
    .await
    .unwrap();
    insert_announcement(
        &store,
        "Maintenance done",
        "",
        crate::config::Severity::Resolved,
        None,
    )
    .await
    .unwrap();

    // Newest first; both active.
    let pinned = active_announcements(&store, now).await.unwrap();
    assert_eq!(pinned.len(), 2);
    assert_eq!(pinned[0].title, "Maintenance done");
    assert_eq!(pinned[1].id, id);
    assert_eq!(pinned[1].severity, crate::config::Severity::Warning);

    // The bounded one expires on its own; the unbounded one stays.
    let later = active_announcements(&store, now + 601).await.unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].title, "Maintenance done");

    // The pruner drops only the expired row.
    prune_announcements(&store, now + 700).await.unwrap();
    assert_eq!(active_announcements(&store, 0).await.unwrap().len(), 1);

    assert_eq!(clear_announcements(&store, now).await.unwrap(), 1);
    assert!(active_announcements(&store, now).await.unwrap().is_empty());
}

#[tokio::test]
async fn silences_cover_wildcard_expire_and_clear() {
    let store = Store::in_memory().await;
    let now = 1000;

    insert_silence(&store, "api", now + 600, Some("deploying"))
        .await
        .unwrap();
    assert!(is_silenced(&store, "api", now).await.unwrap());
    assert!(!is_silenced(&store, "web", now).await.unwrap());
    // Expiry is a hard edge: at `until` the silence is over.
    assert!(!is_silenced(&store, "api", now + 600).await.unwrap());

    // The wildcard covers every monitor.
    insert_silence(&store, "*", now + 300, None).await.unwrap();
    assert!(is_silenced(&store, "web", now).await.unwrap());

    let active = active_silences(&store, now).await.unwrap();
    assert_eq!(active.len(), 2);
    // Soonest to expire first.
    assert_eq!(active[0].monitor_id, "*");
    assert_eq!(active[1].reason.as_deref(), Some("deploying"));

    // The expired row is swept by the pruner; the active ones survive.
    prune_silences(&store, now + 450).await.unwrap();
    assert_eq!(active_silences(&store, 0).await.unwrap().len(), 1);

    assert_eq!(clear_silences(&store, now).await.unwrap(), 1);
    assert!(!is_silenced(&store, "api", now).await.unwrap());
}

#[tokio::test]
async fn backup_into_snapshots_and_refuses_overwrite() {
    let dir = std::env::temp_dir();
    let src = dir.join(format!("hora-test-backup-src-{}.db", std::process::id()));
    let dest = dir.join(format!("hora-test-backup-dest-{}.db", std::process::id()));
    for path in [&src, &dest] {
        let _ = std::fs::remove_file(path);
    }
    let src_s = src.to_str().unwrap();
    let dest_s = dest.to_str().unwrap();

    let store = connect(src_s).await.unwrap();
    insert(&store, "m", 100, 1, Some(10)).await;
    backup_into(src_s, dest_s).await.unwrap();

    // Owner-only, like the live database (created so, never chmod-ed).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    // A failed copy leaves no empty placeholder behind.
    let missing = dir.join(format!("hora-test-backup-none-{}.db", std::process::id()));
    let failed = dir.join(format!("hora-test-backup-fail-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&failed);
    assert!(
        backup_into(missing.to_str().unwrap(), failed.to_str().unwrap())
            .await
            .is_err()
    );
    assert!(!failed.exists());

    // The snapshot is a self-sufficient database with the data.
    let copy = connect(dest_s).await.unwrap();
    assert_eq!(recent_checks(&copy, "m", 10).await.unwrap().len(), 1);
    copy.close().await;

    // An existing destination is never overwritten.
    let err = backup_into(src_s, dest_s).await.unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    store.close().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{src_s}{suffix}"));
        let _ = std::fs::remove_file(format!("{dest_s}{suffix}"));
    }
}

// --- Rolling windows: roll-ups + raw edges must equal the raw-only reads ---

/// A deterministic spread of checks for `monitors` over `[start, end)`: a
/// sample every `step` seconds, mostly up with varied latency, some
/// degraded and down (down checks without latency), so every aggregate has
/// something to count.
async fn seed_checks(store: &Store, monitors: &[&str], start: i64, end: i64, step: i64) {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut tx = store.sqlx().begin().await.unwrap();
    let mut time = start;
    while time < end {
        for id in monitors {
            let roll = next() % 100;
            let (status, latency) = match roll {
                0..=2 => (0, None),
                3..=7 => (2, Some(1500 + i64::try_from(next() % 900).unwrap())),
                _ => (1, Some(20 + i64::try_from(next() % 400).unwrap())),
            };
            sqlx::query(
                "INSERT INTO checks (time, monitor_id, status, latency_ms) VALUES (?, ?, ?, ?)",
            )
            .bind(time + i64::try_from(next() % 5).unwrap())
            .bind(id)
            .bind(status)
            .bind(latency)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        time += step;
    }
    tx.commit().await.unwrap();
}

/// The window path must agree with the raw-only reads: availability exactly,
/// percentiles within the histogram's 1/64 bound.
async fn assert_window_matches_raw(store: &Store, since: i64) {
    let stats = window_stats_all(store, since).await.unwrap();
    let availability = availability_all(store, since).await.unwrap();
    let percentiles = latency_percentiles_all(store, since).await.unwrap();
    assert_eq!(stats.len(), availability.len());
    for (id, &(available, total)) in &availability {
        let window = &stats[id];
        assert_eq!((window.available, window.total), (available, total), "{id}");
        let (p50, p95, p99) = window.latency.p50_p95_p99().expect("samples");
        let (e50, e95, e99) = percentiles[id];
        for (got, want) in [(p50, e50), (p95, e95), (p99, e99)] {
            assert!((got - want).abs() * 64 <= want, "{id}: {got} vs {want}");
        }
    }
}

#[tokio::test]
async fn window_stats_equal_the_raw_reads_across_roll_ups() {
    let store = Store::in_memory().await;
    let day0 = 100 * SECONDS_PER_DAY;
    let now = day0 + 30 * 3600 + 1234;
    seed_checks(&store, &["a", "b", "c"], day0, now, 60).await;
    // A mid-hour window start, so the leading partial hour is read raw.
    let since = now - SECONDS_PER_DAY;

    // Nothing rolled up: everything raw.
    assert_window_matches_raw(&store, since).await;
    // Rolled up to two hours ago: buckets plus raw edges.
    roll_up_recent(&store, now - 7200).await.unwrap();
    assert_window_matches_raw(&store, since).await;
    // Everything that can be: only the current hour stays raw.
    roll_up_recent(&store, now).await.unwrap();
    assert_window_matches_raw(&store, since).await;
    // Every bucket in the window carries a histogram now.
    let missing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM checks_hourly WHERE hour >= ? AND latency_hist IS NULL",
    )
    .bind(since - 3600)
    .fetch_one(store.sqlx())
    .await
    .unwrap();
    assert_eq!(missing, 0);

    // Buckets rolled up before histograms existed: the window reads their
    // latency raw, and the next roll-up backfills them.
    sqlx::query("UPDATE checks_hourly SET latency_hist = NULL WHERE hour % 7200 = 0")
        .execute(store.sqlx())
        .await
        .unwrap();
    assert_window_matches_raw(&store, since).await;
    fill_latency_histograms(&store, since - 3600).await.unwrap();
    assert_window_matches_raw(&store, since).await;
}

#[tokio::test]
async fn histograms_cover_every_bucket_even_without_latency() {
    let store = Store::in_memory().await;
    let hour0 = 10 * 86_400;
    insert(&store, "fast", hour0 + 10, 1, Some(40)).await;
    insert(&store, "fast", hour0 + 20, 1, Some(40)).await;
    insert(&store, "dead", hour0 + 10, 0, None).await;
    roll_up_recent(&store, hour0 + 3600 + 300).await.unwrap();

    let rows = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT monitor_id, latency_hist FROM checks_hourly ORDER BY monitor_id",
    )
    .fetch_all(store.sqlx())
    .await
    .unwrap();
    let decoded: Vec<(String, u64)> = rows
        .into_iter()
        .map(|(id, bytes)| {
            let hist = crate::histogram::LatencyHistogram::decode(&bytes).unwrap();
            (id, hist.len())
        })
        .collect();
    assert_eq!(
        decoded,
        vec![("dead".to_owned(), 0), ("fast".to_owned(), 2)]
    );
}

#[tokio::test]
async fn sparkline_cache_equals_the_one_shot_read_as_time_moves() {
    let store = Store::in_memory().await;
    let day0 = 100 * SECONDS_PER_DAY;
    let bucket = 720;
    let mut cache = SparklineCache::default();
    let mut now = day0 + 26 * 3600;
    seed_checks(&store, &["a", "b"], day0, now, 45).await;
    for step in 0..12 {
        let since = now - SECONDS_PER_DAY;
        let cached = cache.refresh(&store, since, bucket, now).await.unwrap();
        let direct = latency_sparkline_all(&store, since, bucket).await.unwrap();
        assert_eq!(cached.len(), direct.len(), "step {step}");
        for (id, points) in &direct {
            let got: Vec<(i64, i64)> = cached[id].iter().map(|p| (p.t, p.latency_ms)).collect();
            let want: Vec<(i64, i64)> = points.iter().map(|p| (p.t, p.latency_ms)).collect();
            assert_eq!(got, want, "step {step}, {id}");
        }
        // Time moves by uneven strides (inside a bucket, across several),
        // and new checks keep arriving.
        let stride = [5, 400, 30, 2000, 719, 721][step % 6];
        seed_checks(&store, &["a", "b"], now, now + stride, 45).await;
        now += stride;
    }
    // Another bucket width starts over.
    let since = now - SECONDS_PER_DAY;
    let cached = cache.refresh(&store, since, 600, now).await.unwrap();
    let direct = latency_sparkline_all(&store, since, 600).await.unwrap();
    assert_eq!(cached["a"].len(), direct["a"].len());
}

#[tokio::test]
async fn daily_cache_equals_daily_all_as_roll_ups_and_days_advance() {
    let store = Store::in_memory().await;
    let day0 = 100 * SECONDS_PER_DAY;
    let mut now = day0 + 5 * SECONDS_PER_DAY + 7 * 3600;
    seed_checks(&store, &["a", "b"], day0, now, 300).await;
    roll_up_recent(&store, now).await.unwrap();
    // Older days live in daily buckets only, as after the 90-day roll-up.
    downsample_daily(&store, day0 + 2 * SECONDS_PER_DAY)
        .await
        .unwrap();
    super::retention::prune_hourly(
        &store,
        day0 + SECONDS_PER_DAY,
        &super::retention::Pace::offline(),
    )
    .await
    .unwrap();

    let mut cache = DailyCache::default();
    for step in 0..10 {
        let since = now - 4 * SECONDS_PER_DAY - 1800;
        let cached = cache.refresh(&store, since, now).await.unwrap();
        let direct = daily_all(&store, since, now).await.unwrap();
        assert_eq!(cached.len(), direct.len(), "step {step}");
        for (id, rows) in &direct {
            let got: Vec<_> = cached[id]
                .iter()
                .map(|r| (r.day.clone(), r.up, r.down, r.degraded))
                .collect();
            let want: Vec<_> = rows
                .iter()
                .map(|r| (r.day.clone(), r.up, r.down, r.degraded))
                .collect();
            assert_eq!(got, want, "step {step}, {id}");
        }
        // Hours of new checks, rolled up on some steps only, a step inside
        // the settled tail's lag, and a step that crosses midnight.
        let stride = [1800, 3 * 3600, 90, 600, 20 * 3600][step % 5];
        seed_checks(&store, &["a", "b"], now, now + stride, 300).await;
        now += stride;
        if step % 2 == 0 {
            roll_up_recent(&store, now).await.unwrap();
        }
    }
}

#[tokio::test]
async fn window_cache_equals_the_one_shot_read_as_time_moves() {
    let store = Store::in_memory().await;
    let day0 = 100 * SECONDS_PER_DAY;
    let mut now = day0 + 26 * 3600 + 1234;
    seed_checks(&store, &["a", "b"], day0, now, 30).await;
    roll_up_recent(&store, now - 3 * 3600).await.unwrap();
    // Some hours predate histograms.
    sqlx::query("UPDATE checks_hourly SET latency_hist = NULL WHERE hour % 10800 = 0")
        .execute(store.sqlx())
        .await
        .unwrap();

    let mut cache = WindowCache::default();
    for step in 0..16 {
        let since = now - SECONDS_PER_DAY;
        let cached = cache.refresh(&store, since, now).await.unwrap();
        let direct = window_stats_all(&store, since).await.unwrap();
        assert_eq!(cached, direct, "step {step}");
        // Strides inside the hour, across it, and past the tail's lag; new
        // checks keep arriving and roll-ups land on some steps.
        let stride = [5, 61, 900, 3600, 2, 4000][step % 6];
        seed_checks(&store, &["a", "b"], now, now + stride, 30).await;
        now += stride;
        if step % 3 == 1 {
            roll_up_recent(&store, now).await.unwrap();
        }
    }
}

#[tokio::test]
async fn purge_removed_deletes_a_removed_monitor_at_once() {
    let store = Store::in_memory().await;
    // Recent rows: the retention window must not be what deletes them.
    let now = chrono::Utc::now().timestamp();
    insert(&store, "gone", now - 60, 1, Some(10)).await;
    insert(&store, "kept", now - 60, 1, Some(10)).await;
    let config = crate::config::parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "kept"
            name = "Kept"
            target = "https://example.com"
            interval_secs = 60
        "#,
    )
    .unwrap();
    // The regular pass only marks it, for the grace period...
    apply_retention(&store, &config, false).await.unwrap();
    assert_eq!(recent_checks(&store, "gone", 5).await.unwrap().len(), 1);
    // ...the explicit purge deletes it now, and leaves the others alone.
    apply_retention(&store, &config, true).await.unwrap();
    assert!(recent_checks(&store, "gone", 5).await.unwrap().is_empty());
    assert_eq!(recent_checks(&store, "kept", 5).await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_incident_marks_cache_follows_the_log_like_a_full_read() {
    type Marks = std::collections::HashMap<String, IncidentMarks>;
    let store = Store::in_memory().await;
    let mut cache = IncidentMarksCache::default();
    let check = |cache_marks: Marks, full: Marks| assert_eq!(cache_marks, full);
    check(
        cache.refresh(&store).await.unwrap(),
        incident_marks(&store).await.unwrap(),
    );

    // Opened, then closed between two refreshes; another opened meanwhile.
    let first = insert_incident_start(&store, "a", None, None, None, &[], None, None)
        .await
        .unwrap();
    check(
        cache.refresh(&store).await.unwrap(),
        incident_marks(&store).await.unwrap(),
    );
    assert!(
        cache.refresh(&store).await.unwrap()["a"]
            .open_since
            .is_some()
    );
    update_incident_end(&store, first).await.unwrap();
    insert_incident_start(&store, "b", None, None, None, &[], None, None)
        .await
        .unwrap();
    let marks = cache.refresh(&store).await.unwrap();
    check(marks.clone(), incident_marks(&store).await.unwrap());
    assert!(marks["a"].last_end.is_some() && marks["a"].open_since.is_none());
    assert!(marks["b"].open_since.is_some());

    // An open incident deleted (its monitor purged) leaves no open mark.
    sqlx::query("DELETE FROM incidents WHERE monitor_id = 'b'")
        .execute(store.sqlx())
        .await
        .unwrap();
    check(
        cache.refresh(&store).await.unwrap(),
        incident_marks(&store).await.unwrap(),
    );
}

/// The hourly buckets are the daily roll-up's only source: a pass whose daily
/// roll-up fails (here the table is gone; in the field, a passing "database is
/// locked") must leave them for the next one instead of pruning the day away.
#[tokio::test]
async fn a_failed_daily_roll_up_keeps_its_hourly_source() {
    let store = Store::in_memory().await;
    let now = 400 * 86_400;
    let old_hour = now - 120 * 86_400;
    sqlx::query(
        "INSERT INTO checks_hourly \
            (monitor_id, hour, up_count, down_count, degraded_count, avg_latency_ms) \
         VALUES ('m', ?, 60, 0, 0, 20)",
    )
    .bind(old_hour)
    .execute(store.sqlx())
    .await
    .unwrap();
    let hourly = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM checks_hourly")
            .fetch_one(store.sqlx())
            .await
            .unwrap()
    };

    sqlx::query("ALTER TABLE checks_daily RENAME TO checks_daily_away")
        .execute(store.sqlx())
        .await
        .unwrap();
    retention::roll_up_history(&store, now, &retention::Pace::offline()).await;
    assert_eq!(hourly().await, 1, "kept: its day was never rolled up");

    // With the roll-up working again the day is written, then pruned.
    sqlx::query("ALTER TABLE checks_daily_away RENAME TO checks_daily")
        .execute(store.sqlx())
        .await
        .unwrap();
    retention::roll_up_history(&store, now, &retention::Pace::offline()).await;
    assert_eq!(hourly().await, 0);
    let days: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checks_daily")
        .fetch_one(store.sqlx())
        .await
        .unwrap();
    assert_eq!(days, 1);
}

/// An incident is unannounced from its record until a channel takes the
/// alert; the mark is what a restart reads to tell the two apart.
#[tokio::test]
async fn an_incident_is_unannounced_until_marked() {
    let store = Store::in_memory().await;
    let id = insert_incident_start(&store, "m", None, None, None, &[], None, None)
        .await
        .unwrap();
    assert!(!incident_notified(&store, id).await.unwrap());
    mark_incident_notified(&store, id).await.unwrap();
    assert!(incident_notified(&store, id).await.unwrap());
    // Nothing left to send for an incident that is gone.
    assert!(incident_notified(&store, id + 1).await.unwrap());
}

/// A missed heartbeat is down at once on the page, as it is for the alert; a
/// check that ran and failed still needs the threshold, and so does a peer.
#[test]
fn a_missed_heartbeat_is_down_without_the_threshold() {
    use crate::status::MonitorState;
    let row = |status, reason| Latest {
        time: 0,
        latency_ms: None,
        status,
        error: None,
        reason,
    };
    let miss = || row(CheckStatus::Down, Some(FailureKind::MissingHeartbeat));
    let up = || row(CheckStatus::Up, None);
    let failed = row(CheckStatus::Down, Some(FailureKind::Timeout));
    let pushed = row(CheckStatus::Down, Some(FailureKind::Pushed));

    assert_eq!(derive_status(&[miss(), up()], 3), MonitorState::Down);
    assert_eq!(derive_status(&[failed, up()], 3), MonitorState::Degraded);
    // A job that says "down" is a check that ran: the threshold applies.
    assert_eq!(derive_status(&[pushed, up()], 3), MonitorState::Degraded);
    assert_eq!(
        derive_status_by_threshold(&[miss(), up()], 3),
        MonitorState::Degraded
    );
}

/// A stored maintenance window, by selector and hours.
async fn window(store: &Store, selector: &str, starts_at: i64, ends_at: i64) -> i64 {
    insert_maintenance_window(
        store,
        NewMaintenanceWindow {
            starts_at,
            ends_at,
            reason: "kernel reboot",
            selector,
            origin: "api",
            remote_id: None,
        },
    )
    .await
    .unwrap()
    .0
    .id
}

#[tokio::test]
async fn planned_windows_are_stored_once_and_read_by_overlap() {
    let store = Store::in_memory().await;
    let id = window(&store, "host:frank", 1_000, 1_600).await;
    // The same origin, selector and hours: the same row, not a second one.
    let (again, created) = insert_maintenance_window(
        &store,
        NewMaintenanceWindow {
            starts_at: 1_000,
            ends_at: 1_600,
            reason: "another wording",
            selector: "host:frank",
            origin: "api",
            remote_id: None,
        },
    )
    .await
    .unwrap();
    assert!(!created);
    assert_eq!(again.id, id);
    assert_eq!(again.reason, "kernel reboot");
    // Another origin is another window.
    let (other, created) = insert_maintenance_window(
        &store,
        NewMaintenanceWindow {
            starts_at: 1_000,
            ends_at: 1_600,
            reason: "kernel reboot",
            selector: "host:frank",
            origin: "peer:hora-frank",
            remote_id: Some(4),
        },
    )
    .await
    .unwrap();
    assert!(created && other.id != id);

    // Both bounds count, like a config window's.
    let ids = |rows: Vec<MaintenanceWindow>| rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
    let between = |from, to| {
        let store = store.clone();
        async move { maintenance_windows_between(&store, from, to).await.unwrap() }
    };
    assert_eq!(ids(between(1_600, 1_600).await), [id, other.id]);
    assert_eq!(ids(between(0, 1_000).await), [id, other.id]);
    assert_eq!(between(0, 999).await, Vec::new());
    assert_eq!(between(1_601, 9_999).await, Vec::new());
    assert_eq!(
        pending_maintenance_windows(&store, 1_600)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        pending_maintenance_windows(&store, 1_601).await.unwrap(),
        Vec::new()
    );

    // An id is never handed out twice, even once its window is gone.
    assert!(delete_maintenance_window(&store, other.id).await.unwrap());
    assert!(!delete_maintenance_window(&store, other.id).await.unwrap());
    assert_eq!(maintenance_window(&store, other.id).await.unwrap(), None);
    let next = window(&store, "*", 5_000, 5_600).await;
    assert!(next > other.id);
}

#[tokio::test]
async fn the_retention_sweep_prunes_old_windows_and_the_ones_left_without_a_monitor() {
    let store = Store::in_memory().await;
    let now = chrono::Utc::now().timestamp();
    let day = SECONDS_PER_DAY;
    // Ended more than a year ago, last month, and still to come.
    let ancient = window(&store, "*", now - 400 * day, now - 399 * day).await;
    let recent = window(&store, "*", now - 30 * day, now - 29 * day).await;
    let planned = window(&store, "host:frank", now + 3600, now + 4200).await;
    // Naming monitors: one removed since, one of two removed, one kept.
    let orphan = window(&store, "ids:gone", now + 3600, now + 4200).await;
    let half = window(&store, "ids:gone,keep", now + 3600, now + 4200).await;
    let kept = window(&store, "ids:keep", now + 3600, now + 4200).await;
    // A watched peer's listen id is an id too.
    let peer = window(&store, "ids:peer-in", now + 3600, now + 4200).await;

    let config: Config = toml::from_str(
        r#"
            [page]
            [server]
            [health]
            id = "node"
            [[peers]]
            id = "peer"
            name = "Peer"
            listen_id = "peer-in"
            expect_every_secs = 60
            listen_token = "a-long-enough-listen-token"
            [[monitors]]
            id = "keep"
            name = "Keep"
            target = "https://example.com"
            interval_secs = 60
        "#,
    )
    .unwrap();
    prune(&store, &config).await.unwrap();

    let left: Vec<i64> = maintenance_windows_between(&store, 0, i64::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    // The month-old one stays: the reports and the day bars still show it.
    assert!(!left.contains(&ancient), "a year after its end it goes");
    assert!(!left.contains(&orphan), "it covered a removed monitor only");
    for id in [recent, planned, half, kept, peer] {
        assert!(left.contains(&id), "{id} in {left:?}");
    }
}

/// The build flag in `.cargo/config.toml` reached the bundled `SQLite`: with
/// its memory statistics on, every allocation takes one process-wide mutex
/// and concurrent readers queue behind it.
#[tokio::test]
async fn sqlite_is_built_without_memory_statistics() {
    let store = Store::in_memory().await;
    let options: Vec<(String,)> = sqlx::query_as("PRAGMA compile_options")
        .fetch_all(store.sqlx())
        .await
        .unwrap();
    assert!(
        options
            .iter()
            .any(|(option,)| option == "DEFAULT_MEMSTATUS=0"),
        "SQLite was compiled with its memory statistics on: \
         LIBSQLITE3_FLAGS from .cargo/config.toml did not reach the build"
    );
}
