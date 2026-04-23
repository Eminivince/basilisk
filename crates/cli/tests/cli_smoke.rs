//! End-to-end smoke test for the `audit` binary.

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

fn audit() -> Command {
    Command::cargo_bin("audit").expect("`audit` binary should be built")
}

/// Point XDG + HOME at a scratch dir so cache writes can't leak into
/// `$HOME/.cache/basilisk` on a developer machine running the test suite.
fn audit_isolated(scratch: &TempDir) -> Command {
    let mut cmd = audit();
    cmd.env("XDG_CACHE_HOME", scratch.path())
        .env("HOME", scratch.path());
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
fn recon_help_lists_no_cache_and_timeout() {
    audit()
        .args(["recon", "--help"])
        .assert()
        .success()
        .stdout(contains("--no-cache"))
        .stdout(contains("--timeout"));
}

#[test]
fn recon_github_url_json_contains_github_and_owner() {
    audit()
        .args([
            "recon",
            "https://github.com/foundry-rs/foundry",
            "--output",
            "json",
        ])
        .assert()
        .success()
        .stdout(contains("Github"))
        .stdout(contains("foundry-rs"));
}

#[test]
fn recon_nonsense_json_contains_unknown() {
    audit()
        .args(["recon", "hello world", "--output", "json"])
        .assert()
        .success()
        .stdout(contains("Unknown"));
}

#[test]
fn recon_address_without_rpc_fails_gracefully() {
    // No Alchemy key, no user RPC_URL set, and the public mainnet endpoint
    // isn't reachable from the test harness — so either the resolve call
    // errors out (exit non-zero) or times out. Either is fine; what we
    // assert here is that the CLI didn't panic and the error surface is
    // contextual.
    let scratch = TempDir::new().unwrap();
    let out = audit_isolated(&scratch)
        .env_remove("ALCHEMY_API_KEY")
        .args([
            "recon",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "--chain",
            "ethereum-sepolia", // no public fallback, no key → NoProviderConfigured
            "--timeout",
            "2",
        ])
        .assert();
    let out = out.get_output();
    // Expect failure (no provider configured) with a contextual error message.
    assert!(!out.status.success(), "expected non-zero exit");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.to_lowercase().contains("rpc") || combined.to_lowercase().contains("provider"),
        "expected contextual RPC/provider error; got: {combined}",
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
