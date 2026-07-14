-- Operator-recorded event markers ("deploy api v2.3"), created via
-- `hora event` or POST /api/event - the answer to the first diagnostic
-- question, "what changed?". Drawn as vertical markers on the latency
-- sparklines, listed on /history, and correlated into incidents: a monitor
-- confirming down shortly after an event carries "recent change: ..." in its
-- alert and its incident record. Swept by the pruner with the aggregates.
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY,
    title      TEXT    NOT NULL,
    created_at INTEGER NOT NULL              -- unix epoch seconds (UTC)
);

CREATE INDEX IF NOT EXISTS idx_events_time ON events (created_at DESC);

-- The correlated event phrase ("deploy api v2.3, 3m before"), resolved when
-- the down was confirmed. Operator detail, sanitized for anonymous viewers
-- like the failure reason.
ALTER TABLE incidents ADD COLUMN event TEXT;
