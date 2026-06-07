-- Latest known TLS certificate expiry per monitor (one row per monitor).
CREATE TABLE IF NOT EXISTS certs (
    monitor_id TEXT    PRIMARY KEY,
    not_after  INTEGER NOT NULL, -- unix epoch seconds (UTC) of certificate expiry
    checked_at INTEGER NOT NULL  -- when this was last refreshed
);
