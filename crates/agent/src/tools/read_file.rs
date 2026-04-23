//! `read_file` — bounded file read with line-range support.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

const MAX_BYTES: usize = 100 * 1024;
const MAX_LINES: usize = 2_000;

pub struct ReadFile;

#[derive(Deserialize)]
struct Input {
    path: String,
    #[serde(default)]
    line_range: Option<[u64; 2]>,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from disk. Returns the content, the total line count, and a \
         truncated flag. Bounded at 100 KB and 2000 lines per call; for larger files, supply a \
         `line_range` to read a specific slice and call again with different ranges if needed. \
         Use this after `analyze_project` has mapped the file layout."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the file."
                },
                "line_range": {
                    "type": "array",
                    "items": { "type": "integer", "minimum": 1 },
                    "minItems": 2,
                    "maxItems": 2,
                    "description": "Optional inclusive 1-based [start, end] line range."
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
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::err(format!("file not found: {}", input.path), false);
            }
            Err(e) => return ToolResult::err(format!("reading {}: {e}", input.path), true),
        };

        let Ok(content) = String::from_utf8(bytes) else {
            return ToolResult::err(format!("file is not valid UTF-8: {}", input.path), false);
        };

        let total_lines = content.lines().count();
        let (slice, truncated, start_line, end_line) = match input.line_range {
            Some([start, end]) if start <= end => {
                let start = usize::try_from(start).unwrap_or(usize::MAX);
                let end = usize::try_from(end).unwrap_or(usize::MAX);
                let sliced: String = content
                    .lines()
                    .enumerate()
                    .filter(|(i, _)| *i + 1 >= start && *i < end)
                    .map(|(_, l)| l)
                    .take(MAX_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                let line_count = sliced.lines().count();
                let trunc = (end - start + 1) > MAX_LINES || sliced.len() > MAX_BYTES;
                (
                    bound(sliced),
                    trunc,
                    start,
                    start + line_count.saturating_sub(1),
                )
            }
            Some(_) => return ToolResult::err("line_range start must be <= end", false),
            None => {
                let bounded = bound_full(&content);
                let truncated = bounded.1;
                (bounded.0, truncated, 1, total_lines.min(MAX_LINES))
            }
        };

        ToolResult::ok(serde_json::json!({
            "content": slice,
            "total_lines": total_lines,
            "byte_count": content.len(),
            "start_line": start_line,
            "end_line": end_line,
            "truncated": truncated,
        }))
    }
}

fn bound(content: String) -> String {
    if content.len() <= MAX_BYTES {
        return content;
    }
    let cutoff = content
        .char_indices()
        .take_while(|(i, _)| *i < MAX_BYTES)
        .last()
        .map_or(MAX_BYTES, |(i, c)| i + c.len_utf8());
    content[..cutoff].to_string()
}

/// Return at most `MAX_LINES` lines and at most `MAX_BYTES` bytes.
fn bound_full(content: &str) -> (String, bool) {
    let total_lines = content.lines().count();
    if total_lines <= MAX_LINES && content.len() <= MAX_BYTES {
        return (content.to_string(), false);
    }
    let taken: Vec<&str> = content.lines().take(MAX_LINES).collect();
    let joined = taken.join("\n");
    let bounded = bound(joined);
    (bounded, true)
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    async fn run(input: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        ReadFile.execute(input, &ctx).await
    }

    #[tokio::test]
    async fn reads_small_file_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();
        let res = run(serde_json::json!({ "path": path })).await;
        let v = match res {
            ToolResult::Ok(v) => v,
            other => panic!("got {other:?}"),
        };
        assert_eq!(v["total_lines"], 3);
        assert_eq!(v["truncated"], false);
        assert!(v["content"].as_str().unwrap().contains("line2"));
    }

    #[tokio::test]
    async fn reports_not_found_as_non_retryable() {
        let res = run(serde_json::json!({ "path": "/does/not/exist.txt" })).await;
        match res {
            ToolResult::Err { message, retryable } => {
                assert!(message.contains("file not found"));
                assert!(!retryable);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_non_utf8_as_non_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bin");
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        let res = run(serde_json::json!({ "path": path })).await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn line_range_limits_output() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("many.txt");
        let mut body = String::new();
        for i in 1..=100 {
            use std::fmt::Write;
            let _ = writeln!(body, "line{i}");
        }
        std::fs::write(&path, body).unwrap();
        let res = run(serde_json::json!({
            "path": path,
            "line_range": [10, 12],
        }))
        .await;
        let v = match res {
            ToolResult::Ok(v) => v,
            other => panic!("got {other:?}"),
        };
        let c = v["content"].as_str().unwrap();
        assert!(c.contains("line10"));
        assert!(c.contains("line12"));
        assert!(!c.contains("line13"));
    }

    #[tokio::test]
    async fn invalid_line_range_is_non_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.txt");
        std::fs::write(&path, "x").unwrap();
        let res = run(serde_json::json!({
            "path": path,
            "line_range": [10, 5],
        }))
        .await;
        match res {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn truncates_files_over_line_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("huge.txt");
        let body: String = (0..MAX_LINES + 50).map(|_| "x\n").collect();
        std::fs::write(&path, body).unwrap();
        let res = run(serde_json::json!({ "path": path })).await;
        match res {
            ToolResult::Ok(v) => assert_eq!(v["truncated"], true),
            other => panic!("got {other:?}"),
        }
    }
}
