//! The set of tools the agent can call.
//!
//! Each tool is a small struct implementing [`crate::Tool`]. They wrap
//! the ingestion + analysis APIs from `basilisk-onchain`, `basilisk-git`,
//! `basilisk-project`, and `basilisk-rpc`. The agent sees their names,
//! descriptions, and input schemas; it decides when to call each one.
//!
//! [`standard_registry`] returns a `ToolRegistry` populated with every
//! tool — the standard set for recon (set-6 `CP3`).

use std::sync::Arc;

pub mod analyze_project;
pub mod classify_target;
pub mod fetch_github_repo;
pub mod finalize_report;
pub mod get_storage_slot;
pub mod grep_project;
pub mod knowledge_tools;
pub mod list_directory;
pub mod read_file;
pub mod resolve_onchain_contract;
pub mod resolve_onchain_system;
pub mod scratchpad_tools;
pub mod static_call;

pub use analyze_project::AnalyzeProject;
pub use classify_target::ClassifyTarget;
pub use fetch_github_repo::FetchGithubRepo;
pub use finalize_report::{Confidence, FinalReport, FinalizeReport, NAME as FINALIZE_REPORT_NAME};
pub use get_storage_slot::GetStorageSlot;
pub use grep_project::GrepProject;
pub use knowledge_tools::{
    RecordFinding, SearchKnowledgeBase, SearchProtocolDocs, SearchSimilarCode,
};
pub use list_directory::ListDirectory;
pub use read_file::ReadFile;
pub use resolve_onchain_contract::ResolveOnchainContract;
pub use resolve_onchain_system::ResolveOnchainSystem;
pub use scratchpad_tools::{ScratchpadHistory, ScratchpadRead, ScratchpadWrite};
pub use static_call::StaticCall;

use crate::tool::ToolRegistry;

/// Build the standard recon tool set.
///
/// Tools included (14):
///  - `classify_target`
///  - `resolve_onchain_contract`
///  - `resolve_onchain_system`
///  - `fetch_github_repo`
///  - `analyze_project`
///  - `read_file`
///  - `grep_project`
///  - `list_directory`
///  - `get_storage_slot`
///  - `static_call`
///  - `finalize_report`
///  - `scratchpad_read`
///  - `scratchpad_write`
///  - `scratchpad_history`
///
/// The three scratchpad tools only function when the session was
/// wired with working memory — see [`crate::AgentRunner::with_scratchpad`]
/// (landing in Set 8 CP8.4). They degrade to a typed error on
/// sessions without scratchpad, matching the knowledge-tools shape.
#[must_use]
pub fn standard_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ClassifyTarget));
    reg.register(Arc::new(ResolveOnchainContract));
    reg.register(Arc::new(ResolveOnchainSystem));
    reg.register(Arc::new(FetchGithubRepo));
    reg.register(Arc::new(AnalyzeProject));
    reg.register(Arc::new(ReadFile));
    reg.register(Arc::new(GrepProject));
    reg.register(Arc::new(ListDirectory));
    reg.register(Arc::new(GetStorageSlot));
    reg.register(Arc::new(StaticCall));
    reg.register(Arc::new(FinalizeReport));
    reg.register(Arc::new(ScratchpadRead));
    reg.register(Arc::new(ScratchpadWrite));
    reg.register(Arc::new(ScratchpadHistory));
    reg
}

/// Standard recon registry plus the four knowledge tools.
///
/// Tools added beyond [`standard_registry`] (4):
///  - `search_knowledge_base`
///  - `search_similar_code`
///  - `search_protocol_docs`
///  - `record_finding`
///
/// The `ToolContext` this registry dispatches against must have
/// `ctx.knowledge.is_some()` — otherwise the knowledge tools
/// return a structured error on every invocation. The runner
/// populates it via [`AgentRunner::with_knowledge`].
///
/// [`AgentRunner::with_knowledge`]: crate::AgentRunner::with_knowledge
#[must_use]
pub fn knowledge_enhanced_registry() -> ToolRegistry {
    let mut reg = standard_registry();
    reg.register(Arc::new(SearchKnowledgeBase));
    reg.register(Arc::new(SearchSimilarCode));
    reg.register(Arc::new(SearchProtocolDocs));
    reg.register(Arc::new(RecordFinding));
    reg
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)]
mod tests {
    use super::*;

    #[test]
    fn standard_registry_has_fourteen_tools() {
        let reg = standard_registry();
        assert_eq!(reg.len(), 14);
    }

    #[test]
    fn standard_registry_includes_finalize_report_by_canonical_name() {
        let reg = standard_registry();
        assert!(reg.get(FINALIZE_REPORT_NAME).is_some());
    }

    #[test]
    fn standard_registry_names_are_stable_and_sorted() {
        let reg = standard_registry();
        let expected = vec![
            "analyze_project",
            "classify_target",
            "fetch_github_repo",
            "finalize_report",
            "get_storage_slot",
            "grep_project",
            "list_directory",
            "read_file",
            "resolve_onchain_contract",
            "resolve_onchain_system",
            "scratchpad_history",
            "scratchpad_read",
            "scratchpad_write",
            "static_call",
        ];
        let actual: Vec<String> = reg.names();
        let expected: Vec<String> = expected.into_iter().map(String::from).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn every_tool_declares_non_empty_description_and_object_schema() {
        let reg = standard_registry();
        for def in reg.definitions() {
            assert!(
                def.description.len() > 20,
                "tool {:?} has short description: {:?}",
                def.name,
                def.description,
            );
            assert_eq!(def.input_schema["type"], "object", "tool {:?}", def.name);
            assert!(
                def.input_schema.get("properties").is_some(),
                "tool {:?} missing properties",
                def.name,
            );
        }
    }

    #[test]
    fn knowledge_enhanced_registry_has_eighteen_tools() {
        let reg = knowledge_enhanced_registry();
        assert_eq!(reg.len(), 18);
    }

    #[test]
    fn knowledge_enhanced_registry_includes_all_four_new_tools() {
        let reg = knowledge_enhanced_registry();
        for name in [
            "search_knowledge_base",
            "search_similar_code",
            "search_protocol_docs",
            "record_finding",
        ] {
            assert!(reg.get(name).is_some(), "missing {name}");
        }
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_is_non_retryable_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path().to_path_buf());
        let reg = standard_registry();
        match reg
            .dispatch("made_up_tool", serde_json::json!({}), &ctx)
            .await
        {
            crate::ToolResult::Err { message, retryable } => {
                assert!(message.contains("unknown tool"));
                assert!(!retryable);
            }
            other => panic!("got {other:?}"),
        }
    }
}
