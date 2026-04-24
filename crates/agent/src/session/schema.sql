-- Basilisk agent session persistence.
--
-- Every agent run writes one row to `sessions`, one row per turn to
-- `turns`, and one row per tool call to `tool_calls`. The three tables
-- are updated inside one transaction per turn so a crash leaves the
-- database consistent — lingering `status = 'running'` sessions are
-- marked `'interrupted'` on the next startup.
--
-- Timestamps are stored as integer milliseconds since the Unix epoch.
-- Booleans are stored as INTEGER 0/1 — SQLite's native representation.

-- `SessionStore::apply_schema` sets PRAGMA user_version to
-- [`store::SCHEMA_VERSION`] *after* this file runs; don't set it here.

CREATE TABLE IF NOT EXISTS sessions (
    id                    TEXT    PRIMARY KEY,
    created_at_ms         INTEGER NOT NULL,
    updated_at_ms         INTEGER NOT NULL,
    target                TEXT    NOT NULL,
    model                 TEXT    NOT NULL,
    system_prompt_hash    TEXT    NOT NULL,
    status                TEXT    NOT NULL,
    stop_reason           TEXT,
    final_report_markdown TEXT,
    final_confidence      TEXT,
    -- Optional human-reviewer notes from `finalize_report`; lives here,
    -- separate from the session-level `note` column which carries the
    -- user's `--session-note` flag.
    final_report_notes    TEXT,
    note                  TEXT,
    stats_json            TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_created_at_idx
    ON sessions (created_at_ms DESC);

CREATE INDEX IF NOT EXISTS sessions_status_idx
    ON sessions (status);

CREATE TABLE IF NOT EXISTS turns (
    session_id    TEXT    NOT NULL,
    turn_index    INTEGER NOT NULL,
    role          TEXT    NOT NULL,
    content_json  TEXT    NOT NULL,
    tokens_in     INTEGER,
    tokens_out    INTEGER,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (session_id, turn_index),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tool_calls (
    session_id   TEXT    NOT NULL,
    turn_index   INTEGER NOT NULL,
    call_index   INTEGER NOT NULL,
    tool_use_id  TEXT    NOT NULL,
    tool_name    TEXT    NOT NULL,
    input_json   TEXT    NOT NULL,
    output_json  TEXT,
    is_error     INTEGER NOT NULL DEFAULT 0,
    duration_ms  INTEGER NOT NULL,
    PRIMARY KEY (session_id, turn_index, call_index),
    FOREIGN KEY (session_id, turn_index)
        REFERENCES turns(session_id, turn_index) ON DELETE CASCADE
);
