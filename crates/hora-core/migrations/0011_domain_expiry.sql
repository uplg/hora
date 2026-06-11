-- Registered-domain expiration as reported by the registry over RDAP, for
-- monitors that opt in with `domain_expiry = "example.com"`. checked_at gates
-- the polling to once a day (RDAP politeness), surviving restarts.
CREATE TABLE IF NOT EXISTS domain_expiry (
    monitor_id TEXT    PRIMARY KEY,
    domain     TEXT    NOT NULL,
    expires_at INTEGER NOT NULL,  -- unix epoch seconds (UTC)
    checked_at INTEGER NOT NULL
);
