-- Widen the checks index to cover every column the status-page queries read
-- (status, latency_ms) in addition to the filter/order columns (monitor_id,
-- time). Aggregate scans over the history window (availability, daily) and the
-- latency series then run index-only -- no per-row table lookup -- while writes
-- still maintain a single index.
DROP INDEX IF EXISTS idx_checks_monitor_time;
CREATE INDEX IF NOT EXISTS idx_checks_monitor_time
    ON checks (monitor_id, time DESC, status, latency_ms);
