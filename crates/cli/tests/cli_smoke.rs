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
fn recon_github_url_no_fetch_classifier_only() {
    // With --no-fetch, the CLI returns the Target classifier output
    // without cloning. This keeps the test hermetic (no network).
    audit()
        .args([
            "recon",
            "https://github.com/foundry-rs/foundry",
            "--no-fetch",
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

// --- `audit cache repos` ------------------------------------------------------

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

// --- `audit recon <path>` (LocalPath target) ---------------------------------

fn write_sol(path: &std::path::Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn recon_local_foundry_project_prints_project_summary() {
    let tmp = TempDir::new().unwrap();
    write_sol(
        &tmp.path().join("foundry.toml"),
        "[profile.default]\nsrc = \"src\"\nsolc = \"0.8.20\"\n",
    );
    write_sol(&tmp.path().join("src/A.sol"), "import \"./B.sol\";\n");
    write_sol(&tmp.path().join("src/B.sol"), "");

    audit_isolated(&tmp)
        .args(["recon", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Project at"))
        .stdout(contains("kind: foundry"))
        .stdout(contains("0.8.20"))
        .stdout(contains("sources: 2 file(s)"));
}

#[test]
fn recon_local_path_with_unresolved_imports_shows_warning_section() {
    let tmp = TempDir::new().unwrap();
    write_sol(
        &tmp.path().join("foundry.toml"),
        "[profile.default]\nsrc = \"src\"\n",
    );
    write_sol(&tmp.path().join("src/A.sol"), "import \"./missing.sol\";\n");

    audit_isolated(&tmp)
        .args(["recon", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(contains("Unresolved imports"))
        .stdout(contains("./missing.sol"));
}

#[test]
fn recon_local_path_json_output_is_valid_json() {
    let tmp = TempDir::new().unwrap();
    write_sol(
        &tmp.path().join("hardhat.config.ts"),
        "module.exports = {};",
    );
    write_sol(&tmp.path().join("contracts/A.sol"), "");

    let out = audit_isolated(&tmp)
        .args(["recon", tmp.path().to_str().unwrap(), "--output", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).to_string();
    let v: serde_json::Value =
        serde_json::from_str(&stdout).expect("recon --output json should emit valid JSON");
    assert!(v.get("root").is_some(), "json should have root: {stdout}");
    assert!(v.get("config").is_some(), "json should have config");
    assert!(v.get("graph").is_some(), "json should have graph");
}

// --- `audit recon <github-url>` (live network) ------------------------------

#[test]
#[ignore = "requires network access to github.com"]
fn recon_github_url_live_clone_renders_project() {
    let scratch = TempDir::new().unwrap();
    audit_isolated(&scratch)
        .args(["recon", "https://github.com/foundry-rs/forge-template"])
        .assert()
        .success()
        .stdout(contains("Project at"))
        .stdout(contains("kind: foundry"));
}

#[test]
fn recon_help_lists_no_fetch_flag() {
    audit()
        .args(["recon", "--help"])
        .assert()
        .success()
        .stdout(contains("--no-fetch"));
}
