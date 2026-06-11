-- Ad-hoc public announcements ("fiber incident, ETA 6pm"), pinned as a
-- status-page banner next to the config-declared [[incidents]]. Created via
-- `hora announce` or POST /api/announce; `until` auto-expires the banner so
-- the classic "incident over, banner still up three days later" cannot
-- happen by default. Expired rows are swept by the pruner.
CREATE TABLE IF NOT EXISTS announcements (
    id         INTEGER PRIMARY KEY,
    title      TEXT    NOT NULL,
    body       TEXT    NOT NULL DEFAULT '',
    severity   TEXT    NOT NULL DEFAULT 'info',  -- info | warning | critical | resolved
    until      INTEGER,                          -- unix epoch seconds (UTC), NULL = until cleared
    created_at INTEGER NOT NULL
);
