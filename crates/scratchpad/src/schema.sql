-- basilisk-scratchpad — schema for working-memory persistence.
--
-- Lives alongside `sessions`/`turns`/`tool_calls` in
-- `~/.basilisk/sessions.db`. The agent's `SessionStore::apply_schema`
-- calls `basilisk_scratchpad::apply_schema` as part of its own
-- migration path so both schemas are created together when the DB
-- file is first opened.
--
-- Timestamps use the same INTEGER-milliseconds-since-epoch format as
-- the session tables. Every mutation rewrites `scratchpads.sections_json`
-- and appends one `scratchpad_revisions` row capped at 100 most recent
-- rows per session.

CREATE TABLE IF NOT EXISTS scratchpads (
    session_id           TEXT    PRIMARY KEY,
    schema_version       INTEGER NOT NULL,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL,
    sections_json        TEXT    NOT NULL,
    next_item_id         INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS scratchpads_updated_at_idx
    ON scratchpads (updated_at_ms DESC);

CREATE TABLE IF NOT EXISTS scratchpad_revisions (
    session_id           TEXT    NOT NULL,
    revision_index       INTEGER NOT NULL,
    at_ms                INTEGER NOT NULL,
    sections_json        TEXT    NOT NULL,
    PRIMARY KEY (session_id, revision_index),
    FOREIGN KEY (session_id) REFERENCES scratchpads(session_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS scratchpad_revisions_at_idx
    ON scratchpad_revisions (session_id, at_ms DESC);
