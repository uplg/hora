-- Maintenance windows planned at runtime: by the operator (`POST
-- /api/maintenance`, `hora maintenance plan`) or announced by a peer on its
-- heartbeat. While one is active it behaves like a `[[maintenance]]` window
-- of the config: checks keep being recorded, alerts are muted, the page says
-- maintenance. A row is never edited, only deleted (a cancellation, a peer
-- withdrawing its announcement, the retention sweep a year after its end).
--
-- AUTOINCREMENT: the id is what a node announces to its peers, so an id is
-- never given to a second window, even after the first was cancelled.
CREATE TABLE IF NOT EXISTS maintenance_windows (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    starts_at  INTEGER NOT NULL,  -- unix epoch seconds (UTC)
    ends_at    INTEGER NOT NULL,  -- unix epoch seconds (UTC), after starts_at
    reason     TEXT    NOT NULL,
    -- What it covers: '*' (everything), 'ids:a,b' (monitor ids or watched
    -- peers' listen ids), 'host:<name>' (the monitors labelled with that
    -- host), or 'peer' (nothing but the watch of the peer that announced it).
    selector   TEXT    NOT NULL,
    origin     TEXT    NOT NULL,  -- 'api', or 'peer:<peer id>'
    remote_id  INTEGER,           -- the announcing peer's own id for it
    created_at INTEGER NOT NULL
);

-- One window per (who, what, when): planning the same one again (a host
-- script that runs every few minutes) finds it instead of stacking another.
CREATE UNIQUE INDEX IF NOT EXISTS idx_maintenance_windows_identity
    ON maintenance_windows (origin, selector, starts_at, ends_at);

CREATE INDEX IF NOT EXISTS idx_maintenance_windows_ends ON maintenance_windows (ends_at);
