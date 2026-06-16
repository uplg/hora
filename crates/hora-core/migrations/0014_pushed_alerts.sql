-- Alerts pushed by external producers (POST /api/monitors/{id}/alert): a
-- producer's own failure ("patch materialization failed") routed through a
-- monitor's notification channels. Kept apart from `checks` and `incidents`
-- on purpose: a pushed alert is a timeline annotation, never a probe result,
-- so it must not move the monitor's up/down status or skew its uptime/SLA
-- maths. Surfaced in its own section on /history; swept by the pruner with
-- the closed incidents.
CREATE TABLE IF NOT EXISTS pushed_alerts (
    id         INTEGER PRIMARY KEY,
    monitor_id TEXT    NOT NULL,
    severity   TEXT    NOT NULL DEFAULT 'info',  -- info | warning | error | critical
    title      TEXT    NOT NULL,
    message    TEXT    NOT NULL DEFAULT '',      -- detail, with any tags folded in
    dedup_key  TEXT,                             -- coalescing key, NULL when none was sent
    created_at INTEGER NOT NULL                  -- unix epoch seconds (UTC)
);

CREATE INDEX IF NOT EXISTS idx_pushed_alerts_monitor_time
    ON pushed_alerts (monitor_id, created_at DESC);
