-- Small key-value store for daemon state that must survive restarts and is
-- not a time series: the digest's last-sent timestamp, and whatever a future
-- feature needs without a table of its own.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
