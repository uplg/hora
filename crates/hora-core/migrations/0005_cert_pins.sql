-- TLS certificate pinning: store the SHA-256 fingerprint of the leaf public key
-- to detect unexpected changes (MITM, unexpected renewal).
CREATE TABLE IF NOT EXISTS cert_pins (
    monitor_id TEXT    PRIMARY KEY,
    fingerprint TEXT   NOT NULL,  -- hex-encoded SHA-256 of the leaf public key
    checked_at INTEGER NOT NULL   -- when this was last refreshed
);
