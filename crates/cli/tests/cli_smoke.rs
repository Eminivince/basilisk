//! End-to-end smoke test for the `audit` binary.
//!
//! Set 9.5 / CP9.5.3 deleted the deterministic recon dispatch. The
//! `recon` subcommand always runs the agent now, which means
//! everything past argument parsing requires an LLM API key. Tests
//! that previously exercised the deterministic path have been
//! removed; the cache + bench surfaces (and `--help`) remain testable
//! without keys.

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

fn audit() -> Command {
    Command::cargo_bin("audit").expect("`audit` binary should be built")
}

/// Point XDG + HOME at a scratch dir so cache writes can't leak into
/// `$HOME/.cache/basilisk` on a developer machine running the test
/// suite.
///
/// Also runs the binary with `cwd = scratch`, so `Config::load()`'s
/// dotenv-walk doesn't find the repo's top-level `.env` and silently
/// repopulate the API keys this test tries to unset.
fn audit_isolated(scratch: &TempDir) -> Command {
    let mut cmd = audit();
    cmd.env("XDG_CACHE_HOME", scratch.path())
        .env("HOME", scratch.path())
        .current_dir(scratch.path());
    cmd
}

#[test]
fn help_lists_recon_subcommand() {
    audit()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("recon"));
}

#[test]
fn help_lists_cache_subcommand() {
    audit()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("cache"));
}

#[test]
fn help_lists_bench_subcommand() {
    audit()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("bench"));
}

#[test]
fn recon_help_lists_vuln_and_chain_flags() {
    // After CP9.5.3, recon has just `--chain` plus the AgentFlags it
    // flattens in. The deterministic-path flags are gone.
    audit()
        .args(["recon", "--help"])
        .assert()
        .success()
        .stdout(contains("--chain"))
        .stdout(contains("--vuln"))
        .stdout(contains("--model"));
}

#[test]
fn recon_help_does_not_advertise_deleted_deterministic_flags() {
    // Regression guard. These flags belonged to the deterministic
    // dispatch and were removed in CP9.5.3.
    let out = audit()
        .args(["recon", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    for forbidden in ["--no-cache", "--no-fetch", "--max-depth", "--max-contracts"] {
        assert!(
            !text.contains(forbidden),
            "deterministic-path flag {forbidden} still in --help",
        );
    }
}

#[test]
fn recon_without_api_key_fails_with_contextual_error() {
    // No deterministic fallback exists anymore — every recon goes
    // through the agent, which needs an Anthropic / OpenRouter /
    // OpenAI key. Assert the error surface is contextual rather than
    // a panic or generic clap error.
    let scratch = TempDir::new().unwrap();
    let out = audit_isolated(&scratch)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .args([
            "recon",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "--chain",
            "ethereum",
        ])
        .assert();
    let out = out.get_output();
    assert!(!out.status.success(), "expected non-zero exit");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.to_lowercase().contains("api key") || combined.to_lowercase().contains("not set"),
        "expected contextual API-key error; got: {combined}",
    );
}

#[test]
fn cache_stats_empty_dir_says_so() {
    let scratch = TempDir::new().unwrap();
    audit_isolated(&scratch)
        .args(["cache", "stats"])
        .assert()
        .success()
        .stdout(contains("cache"));
}

#[test]
fn cache_clear_empty_dir_is_ok() {
    let scratch = TempDir::new().unwrap();
    audit_isolated(&scratch)
        .args(["cache", "clear"])
        .assert()
        .success();
}

#[test]
fn cache_show_missing_entry_prints_no_entry() {
    let scratch = TempDir::new().unwrap();
    audit_isolated(&scratch)
        .args(["cache", "show", "bytecode", "ethereum:0xdead"])
        .assert()
        .success()
        .stdout(contains("no entry"));
}

// --- `audit cache repos` -----------------------------------------------------

#[test]
fn cache_repos_help_lists_stats_list_clear() {
    audit()
        .args(["cache", "repos", "--help"])
        .assert()
        .success()
        .stdout(contains("stats"))
        .stdout(contains("list"))
        .stdout(contains("clear"));
}

#[test]
fn cache_repos_stats_empty_cache_says_so() {
    let scratch = TempDir::new().unwrap();
    audit_isolated(&scratch)
        .args([
            "cache",
            "repos",
            "--cache-dir",
            scratch.path().to_str().unwrap(),
            "stats",
        ])
        .assert()
        .success()
        .stdout(contains("empty"));
}

#[test]
fn cache_repos_list_empty_cache_says_so() {
    let scratch = TempDir::new().unwrap();
    audit_isolated(&scratch)
        .args([
            "cache",
            "repos",
            "--cache-dir",
            scratch.path().to_str().unwrap(),
            "list",
        ])
        .assert()
        .success()
        .stdout(contains("empty"));
}

#[test]
fn cache_repos_clear_empty_cache_reports_zero_bytes_freed() {
    let scratch = TempDir::new().unwrap();
    audit_isolated(&scratch)
        .args([
            "cache",
            "repos",
            "--cache-dir",
            scratch.path().to_str().unwrap(),
            "clear",
        ])
        .assert()
        .success()
        .stdout(contains("0 B"));
}

// --- `audit bench` -----------------------------------------------------------

#[test]
fn bench_list_renders_targets() {
    audit()
        .args(["bench", "list"])
        .assert()
        .success()
        .stdout(contains("euler-2023"))
        .stdout(contains("visor-2021"));
}

#[test]
fn bench_show_known_target_renders_dossier() {
    audit()
        .args(["bench", "show", "visor-2021"])
        .assert()
        .success()
        .stdout(contains("Visor"))
        .stdout(contains("Expected findings"));
}

#[test]
fn bench_show_unknown_target_errors_clearly() {
    audit()
        .args(["bench", "show", "not-a-real-target"])
        .assert()
        .failure()
        .stderr(contains("unknown target"));
}
