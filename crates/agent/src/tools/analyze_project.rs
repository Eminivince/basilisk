//! `analyze_project` — full source-side project pipeline.

use std::path::Path;

use async_trait::async_trait;
use basilisk_project::resolve_project;
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct AnalyzeProject;

#[derive(Deserialize)]
struct Input {
    root_path: String,
}

#[async_trait]
impl Tool for AnalyzeProject {
    fn name(&self) -> &'static str {
        "analyze_project"
    }

    fn description(&self) -> &'static str {
        "Analyze a local Solidity project directory. Runs the full pipeline: classify framework \
         (Foundry / Hardhat / Truffle / mixed / unknown), parse config files, enumerate `.sol` \
         files with role tags (source / test / script), parse imports, and resolve every import \
         through remappings + lib search paths. Returns the full `ResolvedProject` including the \
         import graph and any unresolved imports with attempted paths. Call after \
         `fetch_github_repo` for GitHub targets, or directly on a local path."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["root_path"],
            "properties": {
                "root_path": {
                    "type": "string",
                    "description": "Absolute path to the project root (contains the config file). For a GitHub target, this is the `working_tree` returned by `fetch_github_repo`."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };

        let path = Path::new(&input.root_path).to_path_buf();
        // Run the (sync, FS-heavy) pipeline on a blocking thread.
        let handle = tokio::task::spawn_blocking(move || resolve_project(&path));
        match handle.await {
            Ok(Ok(project)) => ToolResult::ok(project),
            Ok(Err(e)) => {
                let retryable = matches!(e, basilisk_project::ProjectError::Io { .. });
                ToolResult::err(format!("analyze_project failed: {e}"), retryable)
            }
            Err(e) => ToolResult::err(format!("blocking task panicked: {e}"), true),
        }
    }
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    async fn run(input: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        AnalyzeProject.execute(input, &ctx).await
    }

    #[tokio::test]
    async fn analyses_minimal_foundry_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nsolc = \"0.8.20\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/A.sol"), "").unwrap();

        let res = run(serde_json::json!({ "root_path": tmp.path() })).await;
        match res {
            ToolResult::Ok(v) => {
                assert!(v.get("config").is_some());
                assert!(v.get("enumeration").is_some());
                assert!(v.get("graph").is_some());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_root_is_non_retryable() {
        let res = run(serde_json::json!({ "root_path": "/does/not/exist" })).await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_foundry_toml_is_non_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foundry.toml"), "this is = not valid\n").unwrap();
        let res = run(serde_json::json!({ "root_path": tmp.path() })).await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }
}
