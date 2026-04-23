//! `grep_project` — regex search across a project tree.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::tool::{Tool, ToolContext, ToolResult};

const DEFAULT_MAX_MATCHES: usize = 200;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024; // Skip single files over 2 MB.

/// Same skip list the project-enumeration walker uses.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "lib",
    "out",
    "artifacts",
    "cache",
    "target",
    "dist",
    "build",
    "coverage",
    "forge-cache",
    "broadcast",
];

pub struct GrepProject;

#[derive(Deserialize)]
struct Input {
    root_path: String,
    pattern: String,
    #[serde(default)]
    file_glob: Option<String>,
    #[serde(default)]
    max_matches: Option<usize>,
}

#[derive(Serialize)]
struct Match {
    file: String,
    line: usize,
    snippet: String,
}

#[async_trait]
impl Tool for GrepProject {
    fn name(&self) -> &'static str {
        "grep_project"
    }

    fn description(&self) -> &'static str {
        "Search a project directory tree for a regex pattern. Skips build/cache/dep directories \
         (`.git`, `node_modules`, `lib`, `out`, `artifacts`, `cache`, `target`, `dist`, `build`, \
         `coverage`). Use when `analyze_project` has given you a map and you want to locate \
         specific symbols — function names, access modifiers, magic constants. Returns matches \
         with file path, 1-based line number, and the matched line as a snippet. Capped at \
         200 matches by default."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["root_path", "pattern"],
            "properties": {
                "root_path": {
                    "type": "string",
                    "description": "Absolute path to the directory to search."
                },
                "pattern": {
                    "type": "string",
                    "description": "Rust-style regex (not grep-style). Use `\\b` for word boundaries."
                },
                "file_glob": {
                    "type": "string",
                    "description": "Optional file suffix filter (e.g. `.sol`, `.toml`). Matches when the file path *ends with* this string."
                },
                "max_matches": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 5000,
                    "description": "Override the default 200-match cap. Hard max 5000."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };

        let re = match Regex::new(&input.pattern) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("invalid regex: {e}"), false),
        };

        let root = Path::new(&input.root_path);
        if !root.exists() {
            return ToolResult::err(format!("root not found: {}", input.root_path), false);
        }
        if !root.is_dir() {
            return ToolResult::err(
                format!("root is not a directory: {}", input.root_path),
                false,
            );
        }

        let cap = input.max_matches.unwrap_or(DEFAULT_MAX_MATCHES).min(5000);
        let glob = input.file_glob.as_deref();

        let mut matches: Vec<Match> = Vec::new();
        let mut truncated = false;

        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        'outer: while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    let skip = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| SKIP_DIRS.contains(&n));
                    if !skip {
                        stack.push(path);
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                if let Some(g) = glob {
                    let name = path.to_string_lossy();
                    if !name.ends_with(g) {
                        continue;
                    }
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if metadata.len() > MAX_FILE_BYTES {
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (i, line) in body.lines().enumerate() {
                    if re.is_match(line) {
                        matches.push(Match {
                            file: path.display().to_string(),
                            line: i + 1,
                            snippet: truncate_line(line),
                        });
                        if matches.len() >= cap {
                            truncated = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        ToolResult::ok(serde_json::json!({
            "matches": matches,
            "match_count": matches.len(),
            "truncated": truncated,
        }))
    }
}

fn truncate_line(line: &str) -> String {
    static CAP: OnceLock<usize> = OnceLock::new();
    let cap = *CAP.get_or_init(|| 200);
    if line.len() <= cap {
        return line.to_string();
    }
    let cutoff = line
        .char_indices()
        .take_while(|(i, _)| *i < cap)
        .last()
        .map_or(cap, |(i, c)| i + c.len_utf8());
    format!("{}…", &line[..cutoff])
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    async fn run(input: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        GrepProject.execute(input, &ctx).await
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn finds_pattern_in_nested_file() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("src/A.sol"),
            "pragma solidity ^0.8.20;\ncontract Foo {}\n",
        );
        write(
            &tmp.path().join("src/sub/B.sol"),
            "contract Foo {} // another\n",
        );
        let res = run(serde_json::json!({
            "root_path": tmp.path(),
            "pattern": r"\bcontract\s+\w+",
        }))
        .await;
        match res {
            ToolResult::Ok(v) => {
                assert_eq!(v["match_count"], 2);
                assert_eq!(v["truncated"], false);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_glob_filters_by_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/A.sol"), "contract Foo {}\n");
        write(&tmp.path().join("README.md"), "contract Foo in docs\n");
        let res = run(serde_json::json!({
            "root_path": tmp.path(),
            "pattern": "Foo",
            "file_glob": ".sol",
        }))
        .await;
        match res {
            ToolResult::Ok(v) => assert_eq!(v["match_count"], 1),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_node_modules_and_lib() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("src/A.sol"), "contract Foo {}\n");
        write(&tmp.path().join("lib/B.sol"), "contract Foo {}\n");
        write(&tmp.path().join("node_modules/C.sol"), "contract Foo {}\n");
        let res = run(serde_json::json!({
            "root_path": tmp.path(),
            "pattern": "Foo",
        }))
        .await;
        match res {
            ToolResult::Ok(v) => assert_eq!(v["match_count"], 1),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_regex_is_non_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let res = run(serde_json::json!({
            "root_path": tmp.path(),
            "pattern": "[unterminated",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_root_is_non_retryable() {
        let res = run(serde_json::json!({
            "root_path": "/does/not/exist",
            "pattern": "x",
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_matches_cap_triggers_truncated_flag() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..10 {
            write(&tmp.path().join(format!("f{i}.sol")), "contract Foo {}\n");
        }
        let res = run(serde_json::json!({
            "root_path": tmp.path(),
            "pattern": "Foo",
            "max_matches": 3,
        }))
        .await;
        match res {
            ToolResult::Ok(v) => {
                assert_eq!(v["match_count"], 3);
                assert_eq!(v["truncated"], true);
            }
            other => panic!("got {other:?}"),
        }
    }
}
