//! `list_directory` — non-recursive directory listing.

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tool::{Tool, ToolContext, ToolResult};

pub struct ListDirectory;

#[derive(Deserialize)]
struct Input {
    path: String,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    is_dir: bool,
    size_bytes: u64,
}

#[async_trait]
impl Tool for ListDirectory {
    fn name(&self) -> &'static str {
        "list_directory"
    }

    fn description(&self) -> &'static str {
        "List the direct contents of a directory (one level deep — this is NOT recursive). Use to \
         discover files and subdirectories; call again on a subdirectory to descend. Hidden \
         entries (names starting with `.`) are included. Returns each entry's name, whether it's \
         a directory, and size in bytes."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the directory."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };

        let path = Path::new(&input.path);
        let mut entries = match tokio::fs::read_dir(path).await {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::err(format!("directory not found: {}", input.path), false);
            }
            Err(e) => {
                return ToolResult::err(format!("reading {}: {e}", input.path), true);
            }
        };

        let mut out: Vec<Entry> = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let Ok(metadata) = entry.metadata().await else {
                        continue;
                    };
                    out.push(Entry {
                        name,
                        is_dir: metadata.is_dir(),
                        size_bytes: metadata.len(),
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    return ToolResult::err(format!("enumerating: {e}"), true);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));

        let file_count = out.iter().filter(|e| !e.is_dir).count();
        let directory_count = out.iter().filter(|e| e.is_dir).count();
        ToolResult::ok(serde_json::json!({
            "entries": out,
            "file_count": file_count,
            "directory_count": directory_count,
        }))
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
        ListDirectory.execute(input, &ctx).await
    }

    #[tokio::test]
    async fn lists_files_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "aaa").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "bbbb").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let res = run(serde_json::json!({ "path": tmp.path() })).await;
        match res {
            ToolResult::Ok(v) => {
                assert_eq!(v["file_count"], 2);
                assert_eq!(v["directory_count"], 1);
                let names: Vec<&str> = v["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["name"].as_str().unwrap())
                    .collect();
                assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reports_missing_dir_as_non_retryable() {
        let res = run(serde_json::json!({ "path": "/definitely/not/here" })).await;
        match res {
            ToolResult::Err { message, retryable } => {
                assert!(message.contains("directory not found"));
                assert!(!retryable);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn includes_hidden_entries() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".hidden"), "x").unwrap();
        std::fs::write(tmp.path().join("visible"), "y").unwrap();
        let res = run(serde_json::json!({ "path": tmp.path() })).await;
        match res {
            ToolResult::Ok(v) => {
                let names: Vec<&str> = v["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["name"].as_str().unwrap())
                    .collect();
                assert!(names.contains(&".hidden"));
                assert!(names.contains(&"visible"));
            }
            other => panic!("got {other:?}"),
        }
    }
}
