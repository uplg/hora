-- The latest upstream release of the project a monitor watches
-- (`release = { github = "owner/repo", ... }`). checked_at gates the polling
-- (GitHub's anonymous API allows 60 requests an hour), surviving restarts;
-- notified is the release an alert was last sent for, so a release alerts once.
CREATE TABLE IF NOT EXISTS release_watch (
    monitor_id TEXT    PRIMARY KEY,
    project    TEXT    NOT NULL,
    latest     TEXT    NOT NULL,  -- the release's tag
    url        TEXT    NOT NULL,  -- the release's page
    checked_at INTEGER NOT NULL,  -- unix epoch seconds (UTC)
    notified   TEXT
);
