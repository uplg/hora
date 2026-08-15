-- The batched status-page aggregates (daily bars, 24h availability, latency
-- percentiles/sparklines) filter on `time` alone; the monitor-led index cannot
-- seek for them, so each one scanned the whole table - seconds once months of
-- raw checks accumulate. A time-led covering index turns those scans into a
-- range seek over just the queried window.
CREATE INDEX IF NOT EXISTS idx_checks_time
    ON checks (time, monitor_id, status, latency_ms);
