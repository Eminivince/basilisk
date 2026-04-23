//! End-to-end smoke test for the `audit` binary.

use assert_cmd::Command;
use predicates::str::contains;

fn audit() -> Command {
    Command::cargo_bin("audit").expect("`audit` binary should be built")
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
fn recon_stub_exits_zero() {
    audit().args(["recon", "some-target"]).assert().success();
}

#[test]
fn recon_address_json_contains_onchain() {
    audit()
        .args([
            "recon",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "--output",
            "json",
        ])
        .assert()
        .success()
        .stdout(contains("OnChain"));
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
