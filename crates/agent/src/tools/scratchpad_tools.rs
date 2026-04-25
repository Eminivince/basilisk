//! The three agent-facing tools for working memory:
//! `scratchpad_read`, `scratchpad_write`, `scratchpad_history`.
//!
//! All three operate on the [`Scratchpad`] handle carried in
//! [`ToolContext::scratchpad`]. When the session wasn't wired with a
//! scratchpad (legacy recon / tests) the tools return a typed,
//! non-retryable error so the agent can self-correct rather than
//! crash.

use std::sync::Arc;

use async_trait::async_trait;
use basilisk_scratchpad::{
    render_compact, render_markdown, ItemId, ItemStatus, ItemUpdate, Scratchpad, ScratchpadError,
    ScratchpadStore, SectionKey, SectionKind,
};
use serde::Deserialize;
use std::sync::Mutex;

use crate::tool::{Tool, ToolContext, ToolResult};

// --- shared helpers --------------------------------------------------

fn scratchpad(ctx: &ToolContext) -> Result<Arc<Mutex<Scratchpad>>, ToolResult> {
    ctx.scratchpad.clone().ok_or_else(|| {
        ToolResult::err(
            "scratchpad not configured for this session — ask the operator to enable working memory",
            false,
        )
    })
}

fn scratchpad_store(ctx: &ToolContext) -> Result<Arc<ScratchpadStore>, ToolResult> {
    ctx.scratchpad_store.clone().ok_or_else(|| {
        ToolResult::err(
            "scratchpad persistence not configured for this session",
            false,
        )
    })
}

fn parse_section_key(name: &str) -> Result<SectionKey, ToolResult> {
    SectionKey::parse(name).ok_or_else(|| {
        ToolResult::err(
            format!("unknown section '{name}' — names are ASCII alphanumeric+underscore"),
            false,
        )
    })
}

fn err_from_scratchpad(e: ScratchpadError) -> ToolResult {
    let retryable = matches!(
        e,
        ScratchpadError::Storage(_) | ScratchpadError::Sqlite(_),
    );
    ToolResult::err(e.to_string(), retryable)
}

// --- scratchpad_read -------------------------------------------------

pub struct ScratchpadRead;

#[derive(Deserialize, Default)]
struct ReadInput {
    /// `"all"`, a single section name, or an array of section names.
    #[serde(default)]
    section: Option<serde_json::Value>,
    /// `"compact"` (default) or `"full"`.
    #[serde(default)]
    format: Option<String>,
}

#[async_trait]
impl Tool for ScratchpadRead {
    fn name(&self) -> &'static str {
        "scratchpad_read"
    }

    fn description(&self) -> &'static str {
        "Read the current state of your scratchpad. Returns the structured working document where \
         you track understanding, hypotheses, open questions, limitations, and suspicions. Call \
         this at the start of long investigations, when you want to remind yourself what you've \
         already considered, or before producing final outputs. Section names: \
         system_understanding, hypotheses, confirmed_findings, dismissed_hypotheses, \
         open_questions, investigations, limitations_noticed, suspicions_not_yet_confirmed, \
         plus any custom sections you've created."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "section": {
                    "description": "Which section(s) to return. 'all' (default), one section name, or an array of names.",
                    "oneOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                },
                "format": {
                    "type": "string",
                    "enum": ["compact", "full"],
                    "description": "'compact' (default) truncates long content; 'full' returns the whole scratchpad."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let input: ReadInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let sp_lock = match scratchpad(ctx) {
            Ok(l) => l,
            Err(e) => return e,
        };
        let Ok(sp) = sp_lock.lock() else {
            return ToolResult::err("scratchpad lock poisoned", true);
        };

        let full = matches!(input.format.as_deref(), Some("full"));
        let rendered = match input.section {
            None | Some(serde_json::Value::Null) => render_all(&sp, full),
            Some(serde_json::Value::String(s)) if s == "all" => render_all(&sp, full),
            Some(serde_json::Value::String(s)) => {
                let key = match parse_section_key(&s) {
                    Ok(k) => k,
                    Err(e) => return e,
                };
                match render_one(&sp, &key, full) {
                    Ok(v) => v,
                    Err(e) => return e,
                }
            }
            Some(serde_json::Value::Array(arr)) => {
                let mut out = String::new();
                for item in arr {
                    let Some(name) = item.as_str() else {
                        return ToolResult::err(
                            "'section' array items must be strings",
                            false,
                        );
                    };
                    let key = match parse_section_key(name) {
                        Ok(k) => k,
                        Err(e) => return e,
                    };
                    match render_one(&sp, &key, full) {
                        Ok(v) => {
                            out.push_str(&v);
                            out.push('\n');
                        }
                        Err(e) => return e,
                    }
                }
                out
            }
            _ => return ToolResult::err("'section' must be a string or array of strings", false),
        };
        ToolResult::ok(serde_json::json!({ "markdown": rendered }))
    }
}

fn render_all(sp: &Scratchpad, full: bool) -> String {
    if full {
        render_markdown(sp)
    } else {
        render_compact(sp)
    }
}

fn render_one(sp: &Scratchpad, key: &SectionKey, full: bool) -> Result<String, ToolResult> {
    let section = sp.sections.get(key).ok_or_else(|| {
        ToolResult::err(format!("section '{}' doesn't exist", key.wire_name()), false)
    })?;
    // Build a mini-scratchpad containing only this section so the
    // existing renderers apply uniformly.
    let mut mini = Scratchpad::new(sp.session_id.clone());
    // Wipe the mini's defaults; we want only the target section.
    mini.sections.clear();
    mini.sections.insert(key.clone(), section.clone());
    Ok(if full {
        render_markdown(&mini)
    } else {
        render_compact(&mini)
    })
}

// --- scratchpad_write ------------------------------------------------

pub struct ScratchpadWrite;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WriteOp {
    SetProse {
        section: String,
        markdown: String,
    },
    AppendItem {
        section: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    UpdateItem {
        section: String,
        item_id: u64,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        status: Option<WriteStatus>,
        #[serde(default)]
        tags: Option<Vec<String>>,
    },
    RemoveItem {
        section: String,
        item_id: u64,
    },
    CreateCustomSection {
        name: String,
        kind: WriteKind,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WriteStatus {
    Open,
    InProgress,
    Confirmed,
    Dismissed,
    Blocked { reason: String },
}

impl From<WriteStatus> for ItemStatus {
    fn from(s: WriteStatus) -> Self {
        match s {
            WriteStatus::Open => Self::Open,
            WriteStatus::InProgress => Self::InProgress,
            WriteStatus::Confirmed => Self::Confirmed,
            WriteStatus::Dismissed => Self::Dismissed,
            WriteStatus::Blocked { reason } => Self::Blocked { reason },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WriteKind {
    Prose,
    Items,
}

impl From<WriteKind> for SectionKind {
    fn from(k: WriteKind) -> Self {
        match k {
            WriteKind::Prose => Self::Prose,
            WriteKind::Items => Self::Items,
        }
    }
}

#[async_trait]
impl Tool for ScratchpadWrite {
    fn name(&self) -> &'static str {
        "scratchpad_write"
    }

    fn description(&self) -> &'static str {
        "Update your scratchpad. Use this to capture understanding as you learn it, hypotheses as \
         you form them, status changes as evidence accumulates, limitations when you hit a wall, \
         suspicions when something looks off. The scratchpad is your working memory — keep it \
         current. One operation per call: set_prose, append_item, update_item, remove_item, or \
         create_custom_section."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["op"],
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["set_prose", "append_item", "update_item", "remove_item", "create_custom_section"],
                    "description": "Which mutation to perform."
                },
                "section": { "type": "string", "description": "Section name. Omit for create_custom_section." },
                "markdown": { "type": "string", "description": "set_prose only — the new prose content." },
                "content": { "type": "string", "description": "append_item / update_item — the item body." },
                "tags": {
                    "type": "array", "items": { "type": "string" },
                    "description": "append_item / update_item — freeform tags."
                },
                "item_id": { "type": "integer", "description": "update_item / remove_item — the stable id." },
                "status": {
                    "description": "update_item — new status.",
                    "oneOf": [
                        { "type": "string", "enum": ["open", "in_progress", "confirmed", "dismissed"] },
                        {
                            "type": "object",
                            "required": ["blocked"],
                            "properties": {
                                "blocked": {
                                    "type": "object",
                                    "required": ["reason"],
                                    "properties": { "reason": { "type": "string" } }
                                }
                            }
                        }
                    ]
                },
                "name": { "type": "string", "description": "create_custom_section — ASCII alnum+_ up to 64 chars." },
                "kind": { "type": "string", "enum": ["prose", "items"] }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let op: WriteOp = match serde_json::from_value(input) {
            Ok(o) => o,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let sp_lock = match scratchpad(ctx) {
            Ok(l) => l,
            Err(e) => return e,
        };
        let store = match scratchpad_store(ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };

        // Mutate under the lock, drop before persisting to keep the
        // critical section short.
        let (result, snapshot) = {
            let Ok(mut sp) = sp_lock.lock() else {
                return ToolResult::err("scratchpad lock poisoned", true);
            };
            let result = apply_op(&mut sp, op);
            let snapshot = sp.clone();
            (result, snapshot)
        };

        let result_value = match result {
            Ok(v) => v,
            Err(e) => return err_from_scratchpad(e),
        };

        // Persist best-effort. Failure is logged via the error
        // surface but doesn't undo the in-memory mutation — the
        // runner retries on next save.
        if let Err(e) = store.save(&snapshot) {
            tracing::warn!(error = %e, "scratchpad save failed; in-memory state retained");
        }

        ToolResult::ok(result_value)
    }
}

fn apply_op(sp: &mut Scratchpad, op: WriteOp) -> Result<serde_json::Value, ScratchpadError> {
    match op {
        WriteOp::SetProse { section, markdown } => {
            let key = SectionKey::parse(&section).ok_or(ScratchpadError::MissingSection(section))?;
            sp.set_prose(&key, markdown)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        WriteOp::AppendItem {
            section,
            content,
            tags,
        } => {
            let key = SectionKey::parse(&section).ok_or(ScratchpadError::MissingSection(section))?;
            let id = sp.append_item(&key, content, tags)?;
            Ok(serde_json::json!({ "ok": true, "item_id": id.0 }))
        }
        WriteOp::UpdateItem {
            section,
            item_id,
            content,
            status,
            tags,
        } => {
            let key = SectionKey::parse(&section).ok_or(ScratchpadError::MissingSection(section))?;
            sp.update_item(
                &key,
                ItemId(item_id),
                ItemUpdate {
                    content,
                    status: status.map(Into::into),
                    tags,
                },
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        WriteOp::RemoveItem { section, item_id } => {
            let key = SectionKey::parse(&section).ok_or(ScratchpadError::MissingSection(section))?;
            sp.remove_item(&key, ItemId(item_id))?;
            Ok(serde_json::json!({ "ok": true }))
        }
        WriteOp::CreateCustomSection { name, kind } => {
            sp.create_custom_section(name, kind.into())?;
            Ok(serde_json::json!({ "ok": true }))
        }
    }
}

// --- scratchpad_history ----------------------------------------------

pub struct ScratchpadHistory;

#[derive(Deserialize)]
struct HistoryInput {
    /// `"section"` or `"item"`.
    scope: String,
    /// Section name. Required for both scopes.
    section: String,
    /// Item id. Required when `scope` is `"item"`.
    #[serde(default)]
    item_id: Option<u64>,
}

#[async_trait]
impl Tool for ScratchpadHistory {
    fn name(&self) -> &'static str {
        "scratchpad_history"
    }

    fn description(&self) -> &'static str {
        "View how a specific section or item has evolved. Use this when you want to recall what \
         you previously thought, or when an item's current state seems wrong and you want to see \
         when it changed. Two scopes: 'section' returns the scratchpad's saved revisions for the \
         given section, 'item' returns that item's capped in-memory revision trail."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["scope", "section"],
            "properties": {
                "scope": { "type": "string", "enum": ["section", "item"] },
                "section": { "type": "string" },
                "item_id": { "type": "integer" }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let input: HistoryInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        let key = match parse_section_key(&input.section) {
            Ok(k) => k,
            Err(e) => return e,
        };
        let sp_lock = match scratchpad(ctx) {
            Ok(l) => l,
            Err(e) => return e,
        };

        match input.scope.as_str() {
            "item" => {
                let Some(id) = input.item_id else {
                    return ToolResult::err("'item_id' required when scope=item", false);
                };
                let Ok(sp) = sp_lock.lock() else {
                    return ToolResult::err("scratchpad lock poisoned", true);
                };
                let section = sp.sections.get(&key);
                let items = match section {
                    Some(basilisk_scratchpad::Section::Items(i)) => &i.items,
                    Some(_) => {
                        return ToolResult::err(
                            format!("section '{}' has no items", key.wire_name()),
                            false,
                        )
                    }
                    None => {
                        return ToolResult::err(
                            format!("section '{}' doesn't exist", key.wire_name()),
                            false,
                        )
                    }
                };
                let item = items
                    .iter()
                    .find(|it| it.id.0 == id)
                    .ok_or_else(|| ToolResult::err(format!("item {id} not found"), false));
                let item = match item {
                    Ok(i) => i,
                    Err(e) => return e,
                };
                ToolResult::ok(serde_json::json!({
                    "scope": "item",
                    "section": key.wire_name(),
                    "item_id": id,
                    "current": {
                        "content": item.content,
                        "status": item.status.label(),
                        "updated_at_ms": item.updated_at_ms,
                    },
                    "history": item.history.iter().map(|r| serde_json::json!({
                        "at_ms": r.at_ms,
                        "content": r.content,
                        "status": r.status.label(),
                    })).collect::<Vec<_>>(),
                }))
            }
            "section" => {
                let store = match scratchpad_store(ctx) {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let session_id = ctx.session_id.as_str();
                let revisions = match store.list_revisions(session_id) {
                    Ok(r) => r,
                    Err(e) => return err_from_scratchpad(e),
                };
                // Load the first 10 revisions' section state so the
                // agent sees recent evolution without a flood of DB
                // calls.
                let wanted = revisions.iter().take(10);
                let mut entries = Vec::new();
                for (rev_idx, at_ms) in wanted {
                    match store.load_at_revision(session_id, *rev_idx) {
                        Ok(Some(sections)) => {
                            let section_state = sections.get(&key).map_or(
                                serde_json::Value::Null,
                                |s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null),
                            );
                            entries.push(serde_json::json!({
                                "revision_index": rev_idx,
                                "at_ms": at_ms,
                                "section": section_state,
                            }));
                        }
                        Ok(None) => {}
                        Err(e) => return err_from_scratchpad(e),
                    }
                }
                ToolResult::ok(serde_json::json!({
                    "scope": "section",
                    "section": key.wire_name(),
                    "revisions": entries,
                }))
            }
            other => ToolResult::err(format!("unknown scope '{other}'"), false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn ctx_with_scratchpad() -> (
        ToolContext,
        Arc<Mutex<Scratchpad>>,
        Arc<ScratchpadStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);

        let store = Arc::new(ScratchpadStore::open_in_memory().unwrap());
        // Seed the session row so the FK is satisfied.
        {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            let _ = conn; // placeholder — open_in_memory above already seeds sessions.
        }
        let sid = ctx.session_id.as_str().to_string();
        // ScratchpadStore::open_in_memory seeds a `sessions` table stub.
        // Insert our session id so we can save.
        {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            drop(conn);
        }
        // Use the store's own conn via its seed helper:
        let pad = {
            // Need to insert into sessions first; the store exposes no
            // helper, so we use raw SQL via a new store handle backed
            // by the same in-memory DB. Easier path: store.save skips
            // the FK because open_in_memory creates a stub-less table.
            // Our open_in_memory creates `CREATE TABLE IF NOT EXISTS
            // sessions (id TEXT PRIMARY KEY)`, so we need to insert.
            let conn_cell = &*store;
            let sp = Scratchpad::new(&sid);
            // Re-acquire a raw connection via the store's method path
            // isn't public. Instead, work around by exposing through
            // the raw approach used in store::tests:
            let raw_store = store.clone();
            // Insert the session id row manually via a short helper.
            let raw = raw_store.clone();
            // Use the public API: create() needs FK. Skip FK issues by
            // calling save() which uses upsert; the FK would fire on
            // insert. ScratchpadStore::open_in_memory seeds an empty
            // sessions table so we need to populate it.
            let _ = conn_cell;
            // Direct DB access via a helper on the store:
            raw.seed_session_for_tests(&sid).unwrap();
            raw.save(&sp).unwrap();
            sp
        };
        let sp_lock = Arc::new(Mutex::new(pad));
        ctx.scratchpad = Some(sp_lock.clone());
        ctx.scratchpad_store = Some(store.clone());
        (ctx, sp_lock, store)
    }

    #[tokio::test]
    async fn read_returns_all_sections_on_default() {
        let (ctx, _lock, _store) = ctx_with_scratchpad();
        let res = ScratchpadRead
            .execute(serde_json::json!({}), &ctx)
            .await;
        match res {
            ToolResult::Ok(v) => {
                let md = v.get("markdown").and_then(|s| s.as_str()).unwrap_or("");
                assert!(md.contains("Hypotheses"));
                assert!(md.contains("Limitations noticed"));
            }
            ToolResult::Err { message, .. } => panic!("expected Ok; got: {message}"),
        }
    }

    #[tokio::test]
    async fn write_append_item_persists() {
        let (ctx, lock, store) = ctx_with_scratchpad();
        let res = ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "append_item",
                    "section": "hypotheses",
                    "content": "first hypothesis",
                    "tags": ["draft"],
                }),
                &ctx,
            )
            .await;
        match res {
            ToolResult::Ok(v) => {
                assert_eq!(v.get("ok"), Some(&serde_json::json!(true)));
                assert_eq!(v.get("item_id"), Some(&serde_json::json!(1)));
            }
            ToolResult::Err { message, .. } => panic!("expected Ok; got: {message}"),
        }
        assert_eq!(lock.lock().unwrap().item_count(), 1);
        let loaded = store.load(ctx.session_id.as_str()).unwrap().unwrap();
        assert_eq!(loaded.item_count(), 1);
    }

    #[tokio::test]
    async fn write_update_item_changes_status() {
        let (ctx, _lock, _store) = ctx_with_scratchpad();
        ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "append_item", "section": "hypotheses", "content": "hy",
                }),
                &ctx,
            )
            .await;
        let res = ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "update_item",
                    "section": "hypotheses",
                    "item_id": 1,
                    "status": "confirmed",
                }),
                &ctx,
            )
            .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn write_create_custom_section_then_read_it() {
        let (ctx, _lock, _store) = ctx_with_scratchpad();
        ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "create_custom_section", "name": "oracle_tree", "kind": "prose",
                }),
                &ctx,
            )
            .await;
        ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "set_prose", "section": "oracle_tree", "markdown": "tree…",
                }),
                &ctx,
            )
            .await;
        let res = ScratchpadRead
            .execute(
                serde_json::json!({ "section": "oracle_tree", "format": "full" }),
                &ctx,
            )
            .await;
        match res {
            ToolResult::Ok(v) => {
                let md = v.get("markdown").and_then(|s| s.as_str()).unwrap_or("");
                assert!(md.contains("tree"));
                assert!(md.contains("oracle_tree"));
            }
            ToolResult::Err { message, .. } => panic!("{message}"),
        }
    }

    #[tokio::test]
    async fn write_with_no_scratchpad_errors_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::test(dir.path().to_path_buf());
        std::mem::forget(dir);
        let res = ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "append_item", "section": "hypotheses", "content": "x",
                }),
                &ctx,
            )
            .await;
        match res {
            ToolResult::Err { message, retryable } => {
                assert!(message.contains("scratchpad"));
                assert!(!retryable);
            }
            ToolResult::Ok(_) => panic!("should not succeed without scratchpad"),
        }
    }

    #[tokio::test]
    async fn history_item_returns_revisions() {
        let (ctx, _lock, _store) = ctx_with_scratchpad();
        ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "append_item", "section": "hypotheses", "content": "v1",
                }),
                &ctx,
            )
            .await;
        ScratchpadWrite
            .execute(
                serde_json::json!({
                    "op": "update_item", "section": "hypotheses",
                    "item_id": 1, "content": "v2",
                }),
                &ctx,
            )
            .await;
        let res = ScratchpadHistory
            .execute(
                serde_json::json!({
                    "scope": "item", "section": "hypotheses", "item_id": 1,
                }),
                &ctx,
            )
            .await;
        match res {
            ToolResult::Ok(v) => {
                assert_eq!(
                    v.get("current").and_then(|c| c.get("content")),
                    Some(&serde_json::json!("v2")),
                );
                let hist = v.get("history").and_then(|h| h.as_array()).unwrap();
                assert_eq!(hist.len(), 1);
                assert_eq!(
                    hist[0].get("content"),
                    Some(&serde_json::json!("v1")),
                );
            }
            ToolResult::Err { message, .. } => panic!("{message}"),
        }
    }
}
