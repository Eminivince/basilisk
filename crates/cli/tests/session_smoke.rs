//! End-to-end tests for the `audit session` subcommand tree (`CP6d`).
//!
//! Each test:
//!   1. Seeds a tempdir-backed `SQLite` DB with a canned session via
//!      [`basilisk_agent::SessionStore`] directly,
//!   2. Runs `audit session …` with `--db <tmp>` pointing at that DB,
//!   3. Asserts on the CLI's stdout.
//!
//! The `resume` subcommand is *not* exercised here — resume requires a
//! real LLM round-trip, so it's covered by the `#[ignore]`-d live tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use assert_cmd::Command;
use basilisk_agent::{SessionId, SessionStore, TurnRole};
use predicates::str::contains;
use tempfile::TempDir;

fn audit() -> Command {
    Command::cargo_bin("audit").expect("`audit` binary should be built")
}

fn audit_with_db(db: &Path) -> Command {
    let mut cmd = audit();
    // Isolate HOME + XDG so an accidental default-path resolution
    // cannot write into the developer's real `~/.basilisk`.
    cmd.env("HOME", db.parent().expect("db has a parent"))
        .env("XDG_CACHE_HOME", db.parent().expect("db has a parent"));
    cmd
}

/// Seed one completed session with one assistant turn + one tool call
/// + a final report. Returns `(db_path, session_id)`.
fn seed_one_completed(tmp: &TempDir, target: &str, markdown: &str) -> (PathBuf, SessionId) {
    let db = tmp.path().join("sessions.db");
    let store = Arc::new(SessionStore::open(&db).expect("open store"));

    let id = store
        .create_session(target, "test-model", "deadbeef".repeat(8), None)
        .expect("create session");

    let now = SystemTime::now();
    // Seed a user turn (the initial prompt) …
    let user_blocks = serde_json::json!([
        { "type": "text", "text": format!("Target: {target}") }
    ]);
    store
        .record_turn(&id, TurnRole::User, &user_blocks, None, None, now, now)
        .expect("user turn");

    // … an assistant turn with a tool_use block …
    let assistant_blocks = serde_json::json!([
        { "type": "text", "text": "calling classify_target" },
        {
            "type": "tool_use",
            "id": "tu_1",
            "name": "classify_target",
            "input": { "input": target }
        }
    ]);
    let turn_idx = store
        .record_turn(
            &id,
            TurnRole::Assistant,
            &assistant_blocks,
            Some(120),
            Some(40),
            now,
            now,
        )
        .expect("assistant turn");

    // … and a tool-call row for the classify_target dispatch.
    store
        .record_tool_call(
            &id,
            turn_idx,
            0,
            "tu_1".to_string(),
            "classify_target".to_string(),
            &serde_json::json!({ "input": target }),
            Some(&serde_json::json!({ "kind": "LocalPath" })),
            false,
            5,
        )
        .expect("tool call row");

    store
        .record_final_report(&id, markdown.to_string(), "medium", None)
        .expect("final report");
    store
        .mark_stopped(
            &id,
            "report_finalized",
            basilisk_agent::SessionStatus::Completed,
            &serde_json::json!({
                "turns": 1,
                "tool_calls": 1,
                "usage": {
                    "input_tokens": 120,
                    "output_tokens": 40,
                    "cache_read_input_tokens": null,
                    "cache_creation_input_tokens": null,
                },
                "cost_cents": 12,
                "duration_ms": 1234,
            }),
        )
        .expect("mark stopped");
    (db, id)
}

#[test]
fn session_list_prints_the_seeded_session_id_and_target() {
    let tmp = TempDir::new().unwrap();
    let (db, id) = seed_one_completed(&tmp, "eth/0xdeadbeef", "# recon\nbrief.");
    audit_with_db(&db)
        .args(["session", "list", "--db"])
        .arg(&db)
        .assert()
        .success()
        // ID is truncated to 10 chars in the listing.
        .stdout(contains(&id.as_str()[..10]))
        .stdout(contains("eth/0xdeadbeef"))
        .stdout(contains("completed"));
}

#[test]
fn session_list_empty_db_prints_no_sessions_marker() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("empty.db");
    // Opening creates the schema so the CLI can succeed against it.
    SessionStore::open(&db).expect("empty store");
    audit_with_db(&db)
        .args(["session", "list", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("(no sessions)"));
}

#[test]
fn session_list_respects_status_filter() {
    let tmp = TempDir::new().unwrap();
    let (db, _id) = seed_one_completed(&tmp, "eth/0xabc", "# brief\n");
    // Filter to running — no rows should match.
    audit_with_db(&db)
        .args(["session", "list", "--status", "running", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("(no sessions)"));
}

#[test]
fn session_show_report_only_prints_the_final_markdown() {
    let tmp = TempDir::new().unwrap();
    let (db, id) = seed_one_completed(
        &tmp,
        "eth/0xfeed",
        "# recon brief\n\nthis is the brief body.",
    );
    audit_with_db(&db)
        .args(["session", "show", id.as_str(), "--report-only", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("recon brief"))
        .stdout(contains("the brief body"));
}

#[test]
fn session_show_pretty_includes_target_status_and_turns() {
    let tmp = TempDir::new().unwrap();
    let (db, id) = seed_one_completed(&tmp, "eth/0x1111", "# brief\n");
    audit_with_db(&db)
        .args(["session", "show", id.as_str(), "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("eth/0x1111"))
        .stdout(contains("completed"))
        .stdout(contains("turn 0"))
        .stdout(contains("turn 1"))
        .stdout(contains("classify_target"));
}

#[test]
fn session_show_json_emits_valid_json_with_session_and_turns() {
    let tmp = TempDir::new().unwrap();
    let (db, id) = seed_one_completed(&tmp, "eth/0x2222", "# brief\n");
    let out = audit_with_db(&db)
        .args(["session", "show", id.as_str(), "--format", "json", "--db"])
        .arg(&db)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert!(parsed.get("session").is_some(), "got: {parsed}");
    assert!(parsed.get("turns").is_some(), "got: {parsed}");
    assert!(parsed.get("tool_calls").is_some(), "got: {parsed}");
}

#[test]
fn session_show_unknown_id_fails_gracefully() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    SessionStore::open(&db).expect("open store");
    audit_with_db(&db)
        .args(["session", "show", "not-a-real-id", "--db"])
        .arg(&db)
        .assert()
        .failure()
        .stderr(contains("loading session"));
}

#[test]
fn session_delete_with_yes_removes_the_row() {
    let tmp = TempDir::new().unwrap();
    let (db, id) = seed_one_completed(&tmp, "eth/0x3333", "# brief\n");
    audit_with_db(&db)
        .args(["session", "delete", id.as_str(), "--yes", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("deleted"));

    // Follow-up list call no longer sees it.
    audit_with_db(&db)
        .args(["session", "list", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("(no sessions)"));
}

#[test]
fn recon_help_lists_the_agent_flag() {
    audit()
        .args(["recon", "--help"])
        .assert()
        .success()
        .stdout(contains("--agent"));
}

#[test]
fn recon_help_lists_agent_budget_flags() {
    audit()
        .args(["recon", "--help"])
        .assert()
        .success()
        .stdout(contains("--max-turns"))
        .stdout(contains("--max-cost"))
        .stdout(contains("--max-tokens"));
}

#[test]
fn recon_help_advertises_env_var_support() {
    // clap renders `[env: VAR=]` next to flags that honour env vars.
    // This is how operators discover that the flag is configurable
    // from a `.env` file — we assert it's there.
    audit()
        .args(["recon", "--help"])
        .assert()
        .success()
        .stdout(contains("BASILISK_LLM_PROVIDER"))
        .stdout(contains("BASILISK_LLM_MODEL"))
        .stdout(contains("BASILISK_LLM_BASE_URL"))
        .stdout(contains("BASILISK_MAX_COST_CENTS"));
}

#[test]
fn env_var_configured_provider_shows_in_agent_starts_up_message() {
    // Setting BASILISK_LLM_PROVIDER=ollama + model should make the
    // binary pick the ollama backend. Ollama accepts no key, so the
    // run will proceed until it tries to reach the ollama server and
    // get a connection refused. We assert the startup banner shows
    // the ollama identifier — proving env vars won.
    let tmp = tempfile::TempDir::new().unwrap();
    let out = audit()
        .env("BASILISK_LLM_PROVIDER", "ollama")
        .env("BASILISK_LLM_MODEL", "llama3.1:70b")
        .env("BASILISK_LLM_BASE_URL", "http://127.0.0.1:1/v1") // unreachable
        .env("BASILISK_MAX_TURNS", "1")
        .env("HOME", tmp.path())
        .env("XDG_CACHE_HOME", tmp.path())
        .current_dir(tmp.path())
        .args(["recon", "/tmp/nowhere", "--agent"])
        .assert();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.get_output().stdout),
        String::from_utf8_lossy(&out.get_output().stderr),
    );
    assert!(
        combined.contains("ollama/llama3.1:70b"),
        "expected startup banner to show ollama/llama3.1:70b; got:\n{combined}",
    );
}
