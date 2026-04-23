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
