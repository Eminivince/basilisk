//! `forge test` runner — the substrate for `PoC` synthesis.
//!
//! Given a [`ForgeProject`] (already scaffolded with `foundry.toml`,
//! `src/`, `test/`, optional `remappings.txt`), invoke `forge test --json`
//! against it and parse the output into a [`ForgeTestResult`]. Three
//! shapes of failure are distinguished:
//!
//!   1. **Toolchain missing** — `forge` not on `$PATH`. Returns
//!      [`ExecError::ToolchainMissing`].
//!   2. **Setup failed** — forge couldn't reach the test phase
//!      (compile error, missing remapping, RPC unreachable). Returns
//!      `Ok(ForgeTestResult { setup_failed: Some(_), .. })` so the
//!      caller can show the diagnostic; this is *not* an error in the
//!      Rust-level sense.
//!   3. **Tests ran, some failed** — populates `failed` with reasons.
//!
//! Used by `AnvilFork::run_foundry_test` (CP9.2) and directly by the
//! `build_and_run_foundry_test` tool (CP9.8) when no live fork is
//! needed.

use std::{fmt::Write as _, path::Path, process::Stdio, time::Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tracing::debug;

use crate::{
    error::ExecError,
    types::{ForgeProject, ForgeTestResult, TestCase, TestStatus},
};

/// Look up `forge` on `$PATH`. Cached behaviour mirrors `anvil_binary`.
pub fn forge_binary() -> Result<std::path::PathBuf, ExecError> {
    which::which("forge").map_err(|_| ExecError::ToolchainMissing {
        binary: "forge",
        install_hint: "install Foundry: curl -L https://foundry.paradigm.xyz | bash && foundryup",
    })
}

/// Run `forge test --json` against `project.root`, optionally
/// substituting `fork_url_override` for the project's configured
/// `fork_url` — the anvil-backed [`Fork`](crate::Fork) implementations
/// pass their own `rpc_url()` here so forge sees the fork's
/// impersonations / storage overrides.
pub async fn run_forge_test(
    project: &ForgeProject,
    fork_url_override: Option<&str>,
) -> Result<ForgeTestResult, ExecError> {
    let bin = forge_binary()?;
    let url = fork_url_override.unwrap_or(&project.fork_url);
    let started = Instant::now();
    let mut cmd = Command::new(&bin);
    cmd.current_dir(&project.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("test")
        .arg("--json")
        .arg("--fork-url")
        .arg(url);
    if project.fork_block != 0 {
        cmd.arg("--fork-block-number").arg(project.fork_block.to_string());
    }
    if let Some(filter) = &project.match_test {
        cmd.arg("--match-test").arg(filter);
    }
    debug!(
        root = %project.root.display(),
        url,
        block = project.fork_block,
        "running forge test"
    );
    let output = cmd.output().await.map_err(|source| ExecError::SpawnFailed {
        binary: "forge",
        source,
    })?;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        // Non-zero exit *might* mean tests failed (with JSON on stdout)
        // or setup failed (no JSON, diagnostics on stderr). Try the
        // JSON parse first; fall back to setup_failed.
        if let Ok(parsed) = parse_forge_json(&stdout) {
            return Ok(ForgeTestResult {
                stdout,
                stderr,
                duration_ms,
                ..parsed
            });
        }
        return Ok(ForgeTestResult {
            passed: vec![],
            failed: vec![],
            setup_failed: Some(short_setup_diag(&stderr, &stdout)),
            stdout,
            stderr,
            duration_ms,
        });
    }
    let parsed = parse_forge_json(&stdout).map_err(|e| {
        ExecError::Parse(format!("forge test JSON parse failed: {e}"))
    })?;
    Ok(ForgeTestResult {
        stdout,
        stderr,
        duration_ms,
        ..parsed
    })
}

/// `forge test --json` shape:
/// ```json
/// { "<path>:<Contract>": { "test_results": { "<testFoo()>": { "status": "Success", ... } } } }
/// ```
/// We don't model every field — just enough to surface pass/fail with
/// reasons and gas. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct ForgeContractResults {
    #[serde(default)]
    test_results: std::collections::BTreeMap<String, ForgeTest>,
}

#[derive(Debug, Deserialize)]
struct ForgeTest {
    /// `"Success"` | `"Failure"` | `"Skipped"` (forge variants vary by
    /// version — we accept any string).
    status: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    decoded_logs: Vec<String>,
    /// Gas used. Forge nests this in different shapes across
    /// versions: `{"Standard": <u64>}`, `{"Fuzz": ...}`, etc. Pull
    /// it via dynamic lookup.
    #[serde(default)]
    kind: Value,
}

fn parse_forge_json(stdout: &str) -> Result<ForgeTestResult, serde_json::Error> {
    // forge prints exactly one JSON object on stdout in --json mode,
    // sometimes preceded by warnings / progress lines from solc /
    // foundryup. Find the first `{` and parse from there.
    let start = stdout.find('{').unwrap_or(0);
    let json = &stdout[start..];
    let map: std::collections::BTreeMap<String, ForgeContractResults> =
        serde_json::from_str(json)?;
    let mut passed = Vec::new();
    let mut failed = Vec::new();
    for (contract_path, results) in map {
        for (test_name, t) in results.test_results {
            let qualified = format!("{contract_path}::{test_name}");
            let gas_used = t
                .kind
                .get("Standard")
                .and_then(Value::as_u64)
                .or_else(|| t.kind.as_u64());
            let trace = if t.decoded_logs.is_empty() {
                None
            } else {
                Some(t.decoded_logs.join("\n"))
            };
            match t.status.as_str() {
                "Success" => passed.push(TestCase {
                    name: qualified,
                    status: TestStatus::Passed,
                    gas_used,
                    trace,
                }),
                "Failure" | "Failed" => failed.push(TestCase {
                    name: qualified,
                    status: TestStatus::Failed {
                        reason: t.reason.unwrap_or_else(|| "(no reason given)".into()),
                    },
                    gas_used,
                    trace,
                }),
                _ => {
                    // Skipped / unknown statuses go to passed-as-skipped
                    // bucket so the caller can still see them.
                    passed.push(TestCase {
                        name: qualified,
                        status: TestStatus::Skipped,
                        gas_used: None,
                        trace,
                    });
                }
            }
        }
    }
    Ok(ForgeTestResult {
        passed,
        failed,
        setup_failed: None,
        stdout: String::new(),
        stderr: String::new(),
        duration_ms: 0,
    })
}

/// Shorten forge's diagnostic output to the actionable bit. forge
/// dumps full source spans with carets; for a tool result we want
/// the first error line plus a few surrounding lines.
fn short_setup_diag(stderr: &str, stdout: &str) -> String {
    let combined = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let lines: Vec<&str> = combined.lines().take(40).collect();
    if lines.is_empty() {
        "forge exited non-zero with no diagnostic output".into()
    } else {
        lines.join("\n")
    }
}

/// Scaffold a minimal Foundry project at `root` with `foundry.toml`,
/// `src/.gitkeep`, and `test/<file_name>` containing `test_source`.
/// Caller is responsible for making `root` exist and clean.
///
/// Used by the `build_and_run_foundry_test` tool (CP9.8); kept here
/// because the runner needs to know the scaffold's invariants.
pub async fn scaffold_minimal_project(
    root: &Path,
    test_source: &str,
    file_name: &str,
    solc_version: Option<&str>,
    remappings: &[String],
) -> Result<(), ExecError> {
    use tokio::fs;
    fs::create_dir_all(root.join("src"))
        .await
        .map_err(|e| ExecError::Other(format!("create src/: {e}")))?;
    fs::create_dir_all(root.join("test"))
        .await
        .map_err(|e| ExecError::Other(format!("create test/: {e}")))?;
    fs::write(root.join("src").join(".gitkeep"), b"")
        .await
        .map_err(|e| ExecError::Other(format!("write src/.gitkeep: {e}")))?;
    fs::write(root.join("test").join(file_name), test_source.as_bytes())
        .await
        .map_err(|e| ExecError::Other(format!("write test source: {e}")))?;

    let mut toml = String::new();
    toml.push_str("[profile.default]\n");
    toml.push_str("src = \"src\"\n");
    toml.push_str("test = \"test\"\n");
    toml.push_str("out = \"out\"\n");
    toml.push_str("libs = [\"lib\"]\n");
    toml.push_str("optimizer = true\n");
    toml.push_str("optimizer_runs = 200\n");
    if let Some(v) = solc_version {
        let _ = writeln!(toml, "solc = \"{v}\"");
    }
    fs::write(root.join("foundry.toml"), toml.as_bytes())
        .await
        .map_err(|e| ExecError::Other(format!("write foundry.toml: {e}")))?;

    if !remappings.is_empty() {
        let body = remappings.join("\n");
        fs::write(root.join("remappings.txt"), body.as_bytes())
            .await
            .map_err(|e| ExecError::Other(format!("write remappings: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn forge_binary_either_resolves_or_returns_typed_missing() {
        match forge_binary() {
            Ok(p) => assert!(p.exists()),
            Err(ExecError::ToolchainMissing { binary, .. }) => assert_eq!(binary, "forge"),
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_forge_json_picks_pass_and_fail() {
        let json = r#"{
            "test/Foo.t.sol:FooTest": {
                "test_results": {
                    "testOk()": {
                        "status": "Success",
                        "reason": null,
                        "decoded_logs": [],
                        "kind": {"Standard": 12345}
                    },
                    "testNope()": {
                        "status": "Failure",
                        "reason": "expected 1, got 2",
                        "decoded_logs": ["assert failed"],
                        "kind": {"Standard": 9999}
                    }
                }
            }
        }"#;
        let r = parse_forge_json(json).unwrap();
        assert_eq!(r.passed.len(), 1);
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.passed[0].gas_used, Some(12345));
        match &r.failed[0].status {
            TestStatus::Failed { reason } => assert!(reason.contains("expected 1")),
            _ => panic!("wrong status"),
        }
    }

    #[test]
    fn parse_forge_json_handles_leading_garbage() {
        // forge sometimes prints solc warnings before the JSON.
        let json = "warning: foo\nwarning: bar\n{\"test/A.t.sol:T\":{\"test_results\":{}}}";
        let r = parse_forge_json(json).unwrap();
        assert!(r.passed.is_empty());
        assert!(r.failed.is_empty());
    }

    #[test]
    fn parse_forge_json_skipped_status_lands_in_passed_as_skipped() {
        let json = r#"{
            "test/A.t.sol:T": {
                "test_results": {
                    "testSkip()": {
                        "status": "Skipped",
                        "reason": null,
                        "decoded_logs": [],
                        "kind": {}
                    }
                }
            }
        }"#;
        let r = parse_forge_json(json).unwrap();
        assert_eq!(r.passed.len(), 1);
        assert!(matches!(r.passed[0].status, TestStatus::Skipped));
    }

    #[test]
    fn parse_forge_json_missing_top_level_is_error() {
        let bad = "not even json";
        assert!(parse_forge_json(bad).is_err());
    }

    #[test]
    fn short_setup_diag_caps_at_forty_lines() {
        let big = (0..200).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let s = short_setup_diag(&big, "");
        assert_eq!(s.lines().count(), 40);
    }

    #[test]
    fn short_setup_diag_falls_back_to_stdout_when_stderr_empty() {
        let s = short_setup_diag("", "stdout-line");
        assert!(s.contains("stdout-line"));
    }

    #[test]
    fn short_setup_diag_returns_default_when_both_empty() {
        let s = short_setup_diag("", "");
        assert!(s.contains("no diagnostic output"));
    }

    #[tokio::test]
    async fn scaffold_minimal_project_writes_expected_files() {
        let dir = tempdir().unwrap();
        scaffold_minimal_project(
            dir.path(),
            "// hello\ncontract Test {}",
            "Foo.t.sol",
            Some("0.8.20"),
            &["forge-std/=lib/forge-std/src/".into()],
        )
        .await
        .unwrap();

        assert!(dir.path().join("foundry.toml").exists());
        assert!(dir.path().join("src/.gitkeep").exists());
        assert!(dir.path().join("test/Foo.t.sol").exists());
        assert!(dir.path().join("remappings.txt").exists());
        let toml = std::fs::read_to_string(dir.path().join("foundry.toml")).unwrap();
        assert!(toml.contains("solc = \"0.8.20\""));
    }

    #[tokio::test]
    async fn scaffold_skips_remappings_when_empty() {
        let dir = tempdir().unwrap();
        scaffold_minimal_project(dir.path(), "", "Bar.t.sol", None, &[])
            .await
            .unwrap();
        assert!(!dir.path().join("remappings.txt").exists());
    }
}
