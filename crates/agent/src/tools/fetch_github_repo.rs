//! `fetch_github_repo` — shallow-clone into the persistent repo cache.

use async_trait::async_trait;
use basilisk_core::GitRef;
use basilisk_git::{CloneStrategy, FetchOptions};
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct FetchGithubRepo;

#[derive(Deserialize)]
struct Input {
    owner: String,
    repo: String,
    #[serde(default)]
    reference: Option<RefInput>,
    #[serde(default)]
    strategy: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RefInput {
    Branch { name: String },
    Tag { name: String },
    Commit { sha: String },
    Ambiguous { name: String },
}

impl From<RefInput> for GitRef {
    fn from(r: RefInput) -> Self {
        match r {
            RefInput::Branch { name } => Self::Branch(name),
            RefInput::Tag { name } => Self::Tag(name),
            RefInput::Commit { sha } => Self::Commit(sha),
            RefInput::Ambiguous { name } => Self::Ambiguous(name),
        }
    }
}

#[async_trait]
impl Tool for FetchGithubRepo {
    fn name(&self) -> &'static str {
        "fetch_github_repo"
    }

    fn description(&self) -> &'static str {
        "Clone a GitHub repository into the persistent cache. Shallow clone (depth 1) by default \
         — fast and cheap; use `strategy: \"full\"` only when you need history. Returns the \
         working-tree path, the resolved full 40-char commit SHA, and whether the cache was hit. \
         Call this before `analyze_project` when the target is a GitHub URL. Subsequent calls \
         against the same ref return instantly from the cache."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["owner", "repo"],
            "properties": {
                "owner": { "type": "string", "description": "GitHub owner/org, e.g. `foundry-rs`." },
                "repo": { "type": "string", "description": "Repository name, e.g. `forge-template`." },
                "reference": {
                    "type": "object",
                    "description": "Optional ref. Omit for the default branch.",
                    "required": ["kind"],
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["branch", "tag", "commit", "ambiguous"]
                        },
                        "name": { "type": "string", "description": "Branch/tag name or ambiguous ref." },
                        "sha": { "type": "string", "description": "For `kind: commit`: 7-40 hex chars." }
                    }
                },
                "strategy": {
                    "type": "string",
                    "enum": ["shallow", "full"],
                    "description": "Clone depth. Default `shallow` (depth 1)."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };

        let strategy = match input.strategy.as_deref() {
            None | Some("shallow") => CloneStrategy::Shallow,
            Some("full") => CloneStrategy::Full,
            Some(other) => {
                return ToolResult::err(format!("unknown strategy: {other}"), false);
            }
        };

        let options = FetchOptions {
            strategy,
            force_refresh: false,
            github: Some((*ctx.github).clone()),
        };
        let reference: Option<GitRef> = input.reference.map(Into::into);

        match ctx
            .repo_cache
            .fetch(&input.owner, &input.repo, reference, options)
            .await
        {
            Ok(fetched) => ToolResult::ok(serde_json::json!({
                "owner": fetched.owner,
                "repo": fetched.repo,
                "commit_sha": fetched.commit_sha,
                "working_tree": fetched.working_tree.display().to_string(),
                "cached": fetched.cached,
            })),
            Err(e) => {
                let retryable = matches!(
                    e,
                    basilisk_git::GitError::CloneFailed { .. } | basilisk_git::GitError::Io(_)
                );
                ToolResult::err(format!("fetch failed: {e}"), retryable)
            }
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
        FetchGithubRepo.execute(input, &ctx).await
    }

    #[tokio::test]
    async fn bad_strategy_is_non_retryable() {
        let res = run(serde_json::json!({
            "owner": "foundry-rs",
            "repo": "forge-template",
            "strategy": "xyz",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_input_shape_is_non_retryable() {
        let res = run(serde_json::json!({ "owner": 1 })).await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    /// Live test — only runs with `--ignored` + real network access.
    #[tokio::test]
    #[ignore = "requires network access to github.com"]
    async fn live_fetch_forge_template_returns_working_tree() {
        let res = run(serde_json::json!({
            "owner": "foundry-rs",
            "repo": "forge-template",
        }))
        .await;
        match res {
            ToolResult::Ok(v) => {
                assert_eq!(v["owner"], "foundry-rs");
                assert_eq!(v["repo"], "forge-template");
                assert!(v["commit_sha"].as_str().unwrap().len() == 40);
            }
            other => panic!("got {other:?}"),
        }
    }
}
