//! End-to-end tests for `audit session scratchpad` (CP8.7).
//!
//! Seed a scratchpad via the library, then exercise the CLI paths
//! against that DB. All tests isolate HOME / XDG so the real
//! `~/.basilisk/sessions.db` is never touched.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use assert_cmd::Command;
use basilisk_agent::SessionStore;
use basilisk_scratchpad::{SectionKey, ScratchpadStore};
use predicates::str::contains;
use tempfile::TempDir;

fn audit_with_home(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("audit").expect("audit binary built");
    cmd.env("HOME", home).env("XDG_CACHE_HOME", home);
    cmd
}

/// Seed a session + scratchpad in the given DB, populate a few items.
fn seed(db: &Path) -> String {
    // SessionStore::apply_schema creates the scratchpad tables too
    // (as part of the v3 migration), so opening either first works.
    let sessions = Arc::new(SessionStore::open(db).unwrap());
    let id = sessions
        .create_session("eth/0xdead", "mock", "deadbeef".repeat(8), None)
        .unwrap();
    let id_str = id.as_str().to_string();

    let pads = ScratchpadStore::open(db).unwrap();
    pads.seed_session_for_tests(&id_str).unwrap();
    let mut sp = pads.create(&id_str).unwrap();
    sp.set_prose(
        &SectionKey::SystemUnderstanding,
        "This is a proxy with a logic contract at 0xbeef.",
    )
    .unwrap();
    sp.append_item(
        &SectionKey::Hypotheses,
        "reentrancy via erc777 callback",
        vec!["draft".into()],
    )
    .unwrap();
    sp.append_item(&SectionKey::OpenQuestions, "what oracles are in use?", vec![])
        .unwrap();
    pads.save(&sp).unwrap();
    id_str
}

#[test]
fn show_renders_all_seeded_sections() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);
    audit_with_home(tmp.path())
        .args(["session", "scratchpad", "show", &id, "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("System understanding"))
        .stdout(contains("proxy with a logic contract"))
        .stdout(contains("Hypotheses"))
        .stdout(contains("reentrancy via erc777 callback"))
        .stdout(contains("Open questions"))
        .stdout(contains("Limitations noticed"))
        .stdout(contains("Suspicions (not yet confirmed)"));
}

#[test]
fn show_section_scopes_to_that_section_only() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);
    let out = audit_with_home(tmp.path())
        .args([
            "session",
            "scratchpad",
            "show",
            &id,
            "--section",
            "hypotheses",
            "--db",
        ])
        .arg(&db)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    assert!(stdout.contains("Hypotheses"));
    assert!(
        !stdout.contains("System understanding"),
        "section filter should exclude others; got:\n{stdout}",
    );
}

#[test]
fn show_compact_flag_matches_system_prompt_shape() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);
    audit_with_home(tmp.path())
        .args(["session", "scratchpad", "show", &id, "--compact", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("# Scratchpad — session"));
}

#[test]
fn summary_prints_counts_per_section() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);
    audit_with_home(tmp.path())
        .args(["session", "scratchpad", "summary", &id, "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("total items:"))
        .stdout(contains("hypotheses"))
        .stdout(contains("open_questions"));
}

#[test]
fn export_markdown_writes_file() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);
    let out = tmp.path().join("scratchpad.md");
    audit_with_home(tmp.path())
        .args([
            "session",
            "scratchpad",
            "export",
            &id,
            "--output",
        ])
        .arg(&out)
        .args(["--format", "markdown", "--db"])
        .arg(&db)
        .assert()
        .success();
    let body = fs::read_to_string(&out).unwrap();
    assert!(body.contains("System understanding"));
    assert!(body.contains("proxy with a logic"));
}

#[test]
fn export_json_roundtrips_to_original_shape() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);
    let out = tmp.path().join("scratchpad.json");
    audit_with_home(tmp.path())
        .args([
            "session",
            "scratchpad",
            "export",
            &id,
            "--format",
            "json",
            "--output",
        ])
        .arg(&out)
        .args(["--db"])
        .arg(&db)
        .assert()
        .success();
    let body = fs::read_to_string(&out).unwrap();
    let parsed: basilisk_scratchpad::Scratchpad = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed.session_id, id);
    assert_eq!(parsed.item_count(), 2);
}

#[test]
fn delete_with_yes_removes_scratchpad() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);
    audit_with_home(tmp.path())
        .args(["session", "scratchpad", "delete", &id, "--yes", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("deleted scratchpad"));
    audit_with_home(tmp.path())
        .args(["session", "scratchpad", "show", &id, "--db"])
        .arg(&db)
        .assert()
        .failure()
        .stderr(contains("no scratchpad for session"));
}

#[test]
fn show_unknown_session_fails_cleanly() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    // Open so the schema is created, but don't seed.
    SessionStore::open(&db).unwrap();
    audit_with_home(tmp.path())
        .args([
            "session",
            "scratchpad",
            "show",
            "not-a-real-id",
            "--db",
        ])
        .arg(&db)
        .assert()
        .failure()
        .stderr(contains("no scratchpad for session"));
}

#[test]
fn show_at_revision_returns_historical_state() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    let id = seed(&db);

    // `seed` calls create() (revision 1, empty) then save() after
    // populating (revision 2, with content). Point --at-revision
    // at the content-bearing one.
    let pads = ScratchpadStore::open(&db).unwrap();
    let revs = pads.list_revisions(&id).unwrap();
    assert!(revs.len() >= 2, "expected ≥2 revisions, got {}", revs.len());
    let latest = revs[0].0;

    audit_with_home(tmp.path())
        .args([
            "session",
            "scratchpad",
            "show",
            &id,
            "--at-revision",
        ])
        .arg(latest.to_string())
        .args(["--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(contains("proxy with a logic contract"));
}
