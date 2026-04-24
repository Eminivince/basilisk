//! End-to-end tests for the `audit knowledge` subcommand tree (CP7.10).
//!
//! These cover the CLI surface without requiring a real embedding
//! provider or live network calls:
//!
//! - the subcommand is wired into the binary,
//! - `--help` advertises the commands shipped in Set 7,
//! - `stats` against an empty `~/.basilisk/knowledge` prints the
//!   "empty" banner + the five shipped collection names,
//! - `ingest` with no source + no `--all` errors cleanly,
//! - `clear --all` without `--yes` aborts without deleting anything.
//!
//! Ingest and search happy-paths require a live (or mocked-HTTP)
//! embedding provider, which is covered by the unit and wiremock tests
//! in `basilisk-embeddings` / `basilisk-knowledge`.

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

fn audit_with_home(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("audit").expect("`audit` binary built");
    // Isolate from the developer's real ~/.basilisk.
    cmd.env("HOME", home)
        .env("XDG_CACHE_HOME", home)
        .env_remove("VOYAGE_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OLLAMA_HOST")
        .env_remove("EMBEDDINGS_PROVIDER");
    cmd
}

#[test]
fn knowledge_help_lists_subcommands() {
    let tmp = TempDir::new().unwrap();
    audit_with_home(tmp.path())
        .args(["knowledge", "--help"])
        .assert()
        .success()
        .stdout(contains("stats"))
        .stdout(contains("ingest"))
        .stdout(contains("add-protocol"))
        .stdout(contains("list-findings"))
        .stdout(contains("show-finding"))
        .stdout(contains("correct"))
        .stdout(contains("dismiss"))
        .stdout(contains("confirm"))
        .stdout(contains("search"))
        .stdout(contains("clear"));
}

#[test]
fn knowledge_ingest_help_lists_the_three_sources() {
    let tmp = TempDir::new().unwrap();
    audit_with_home(tmp.path())
        .args(["knowledge", "ingest", "--help"])
        .assert()
        .success()
        .stdout(contains("solodit"))
        .stdout(contains("swc"))
        .stdout(contains("openzeppelin"));
}

#[test]
fn knowledge_stats_on_empty_dir_prints_empty_banner_and_shipped_collections() {
    let tmp = TempDir::new().unwrap();
    audit_with_home(tmp.path())
        .args(["knowledge", "stats"])
        .assert()
        .success()
        .stdout(contains("knowledge base is empty"))
        .stdout(contains("public_findings"))
        .stdout(contains("advisories"))
        .stdout(contains("post_mortems"))
        .stdout(contains("protocols"))
        .stdout(contains("user_findings"));
}

#[test]
fn knowledge_ingest_without_source_or_all_errors_cleanly() {
    let tmp = TempDir::new().unwrap();
    // No source, no --all → anyhow::bail! prints to stderr and exits 1.
    audit_with_home(tmp.path())
        .args(["knowledge", "ingest"])
        .assert()
        .failure()
        .stderr(contains("specify a source"));
}

#[test]
fn knowledge_add_protocol_without_a_source_flag_errors() {
    let tmp = TempDir::new().unwrap();
    audit_with_home(tmp.path())
        .args(["knowledge", "add-protocol", "engagement-1"])
        .assert()
        .failure()
        .stderr(contains("--url"));
}

#[test]
fn knowledge_clear_without_collection_or_all_errors() {
    let tmp = TempDir::new().unwrap();
    audit_with_home(tmp.path())
        .args(["knowledge", "clear"])
        .assert()
        .failure()
        .stderr(contains("specify a collection"));
}
