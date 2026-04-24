//! Four agent tools that reach into the knowledge base.
//!
//!  - [`SearchKnowledgeBase`] — natural-language search across
//!    every collection (or a filtered subset).
//!  - [`SearchSimilarCode`] — given a Solidity snippet, find
//!    similar past findings.
//!  - [`SearchProtocolDocs`] — per-engagement documentation,
//!    filtered by the context's `engagement_id`.
//!  - [`RecordFinding`] — write an agent-produced finding into
//!    `user_findings`. `record_correction` deliberately stays CLI-
//!    only: the agent proposes, humans judge.
//!
//! All four require `ctx.knowledge.is_some()`. When absent (recon
//! sessions, tests that don't wire a KB), they return a
//! non-retryable [`ToolResult::Err`] so the agent self-corrects
//! instead of crashing.

use async_trait::async_trait;
use basilisk_knowledge::{
    FindingLocation, FindingRecord, KnowledgeBase, SearchFilters,
};
use serde::Deserialize;

use crate::tool::{Tool, ToolContext, ToolResult};

const SCHEMA_QUERY_FIELD: &str =
    "Short natural-language description of the pattern or concern.";

fn kb_or_err(ctx: &ToolContext) -> Result<&KnowledgeBase, ToolResult> {
    ctx.knowledge.as_deref().ok_or_else(|| {
        ToolResult::err(
            "knowledge base is not configured for this session — the agent \
             was spawned without --with-knowledge or equivalent",
            false,
        )
    })
}

// --- search_knowledge_base -------------------------------------------

pub struct SearchKnowledgeBase;

impl SearchKnowledgeBase {
    pub const NAME: &'static str = "search_knowledge_base";
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    collections: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_include_corrections")]
    include_corrections: bool,
}

fn default_limit() -> usize {
    10
}

fn default_include_corrections() -> bool {
    true
}

#[async_trait]
impl Tool for SearchKnowledgeBase {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn description(&self) -> &'static str {
        "Search the knowledge base — past audit findings, vulnerability \
         advisories, post-mortems. Use this when investigating a pattern \
         and you want to know if it's been seen before. Query is a \
         natural-language description of the pattern or concern. Filters \
         narrow by source (solodit / swc / openzeppelin / ...), severity \
         (critical/high/medium/low/info), category (reentrancy / oracle / \
         access_control / ...), tags, and collections. Returns the most \
         similar past records with source attribution."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query":    { "type": "string", "description": SCHEMA_QUERY_FIELD },
                "limit":    { "type": "integer", "minimum": 1, "maximum": 50,
                              "description": "Max results. Default 10." },
                "collections": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Scope to specific collections (e.g. ['public_findings','advisories']). Empty = all."
                },
                "severity": { "type": "string",
                              "description": "critical | high | medium | low | info" },
                "category": { "type": "string",
                              "description": "reentrancy | oracle | access_control | ..." },
                "source":   { "type": "string", "description": "solodit | swc | openzeppelin | ..." },
                "kind":     { "type": "string", "description": "finding | advisory | post_mortem | doc" },
                "tags":     { "type": "array", "items": { "type": "string" } },
                "include_corrections": { "type": "boolean",
                                         "description": "Default true. Set false to skip correction rows." }
            }
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        let args: SearchArgs = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        if args.query.trim().is_empty() {
            return ToolResult::err("query must not be empty", false);
        }
        let kb = match kb_or_err(ctx) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let filters = SearchFilters {
            collections: args.collections,
            source: args.source,
            kind: args.kind,
            engagement_id: None,
            severity: args.severity,
            category: args.category,
            tags: args.tags,
            include_corrections: args.include_corrections,
        };
        match kb.search(&args.query, filters, args.limit).await {
            Ok(hits) => ToolResult::ok(serde_json::to_value(&hits).unwrap_or_default()),
            Err(e) => ToolResult::err(e.to_string(), false),
        }
    }
}

// --- search_similar_code ---------------------------------------------

pub struct SearchSimilarCode;

impl SearchSimilarCode {
    pub const NAME: &'static str = "search_similar_code";
}

#[derive(Debug, Deserialize)]
struct SimilarCodeArgs {
    code_snippet: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    category: Option<String>,
}

#[async_trait]
impl Tool for SearchSimilarCode {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn description(&self) -> &'static str {
        "Given a specific Solidity snippet, find similar code in past \
         findings. Use this when you're staring at a suspicious-looking \
         function and want to know if code like this has produced findings \
         before. The match is semantic, not syntactic — paraphrased code \
         still matches."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["code_snippet"],
            "properties": {
                "code_snippet": { "type": "string",
                                  "description": "The Solidity snippet to match against." },
                "limit":        { "type": "integer", "minimum": 1, "maximum": 50 },
                "severity":     { "type": "string" },
                "category":     { "type": "string" }
            }
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        let args: SimilarCodeArgs = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        if args.code_snippet.trim().is_empty() {
            return ToolResult::err("code_snippet must not be empty", false);
        }
        let kb = match kb_or_err(ctx) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let filters = SearchFilters {
            collections: Vec::new(),
            severity: args.severity,
            category: args.category,
            include_corrections: true,
            ..Default::default()
        };
        match kb
            .search_similar_code(&args.code_snippet, filters, args.limit)
            .await
        {
            Ok(hits) => ToolResult::ok(serde_json::to_value(&hits).unwrap_or_default()),
            Err(e) => ToolResult::err(e.to_string(), false),
        }
    }
}

// --- search_protocol_docs --------------------------------------------

pub struct SearchProtocolDocs;

impl SearchProtocolDocs {
    pub const NAME: &'static str = "search_protocol_docs";
}

#[derive(Debug, Deserialize)]
struct ProtocolDocsArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[async_trait]
impl Tool for SearchProtocolDocs {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn description(&self) -> &'static str {
        "Search protocol-specific documentation ingested for this \
         engagement. Use this to check what the protocol INTENDS to do, \
         vs what the code does. If documentation wasn't ingested for this \
         engagement, returns an empty array — that's not an error."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string", "description": SCHEMA_QUERY_FIELD },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
            }
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        let args: ProtocolDocsArgs = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        if args.query.trim().is_empty() {
            return ToolResult::err("query must not be empty", false);
        }
        let kb = match kb_or_err(ctx) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let filters = SearchFilters {
            collections: vec![basilisk_vector::schema::PROTOCOLS.into()],
            engagement_id: ctx.engagement_id.clone(),
            include_corrections: true,
            ..Default::default()
        };
        match kb.search(&args.query, filters, args.limit).await {
            Ok(hits) => ToolResult::ok(serde_json::to_value(&hits).unwrap_or_default()),
            Err(e) => ToolResult::err(e.to_string(), false),
        }
    }
}

// --- record_finding --------------------------------------------------

pub struct RecordFinding;

impl RecordFinding {
    pub const NAME: &'static str = "record_finding";
}

#[derive(Debug, Deserialize)]
struct RecordFindingArgs {
    title: String,
    severity: String,
    category: String,
    summary: String,
    #[serde(default)]
    vulnerable_code: Option<String>,
    #[serde(default)]
    location: Option<FindingLocation>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    related_findings: Vec<String>,
    #[serde(default)]
    poc_sketch: Option<String>,
    /// The audit target (address / repo URL / path). Required so
    /// future retrieval can filter by target.
    target: String,
}

#[async_trait]
impl Tool for RecordFinding {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn description(&self) -> &'static str {
        "Record a finding. Call this when you've identified a \
         vulnerability, misconfiguration, or concerning pattern. The \
         finding is stored in Basilisk's memory and will surface when \
         similar code is encountered in future audits. Do not call for \
         general observations — only for things you'd flag to a human \
         auditor. Returns the finding_id the operator can use to correct \
         or confirm later."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["title", "severity", "category", "summary", "target"],
            "properties": {
                "title":    { "type": "string" },
                "severity": { "type": "string",
                              "enum": ["critical", "high", "medium", "low", "info"] },
                "category": { "type": "string",
                              "description": "reentrancy | oracle | access_control | ..." },
                "summary":  { "type": "string",
                              "description": "2–3 sentence description." },
                "target":   { "type": "string",
                              "description": "Address / repo URL / local path." },
                "vulnerable_code": { "type": "string",
                                     "description": "The specific code, as an excerpt." },
                "location": {
                    "type": "object",
                    "required": ["file"],
                    "properties": {
                        "file":       { "type": "string" },
                        "line_range": { "type": "array",
                                        "items": { "type": "integer" },
                                        "minItems": 2, "maxItems": 2 },
                        "function":   { "type": "string" },
                        "contract":   { "type": "string" }
                    }
                },
                "reasoning": { "type": "string" },
                "related_findings": { "type": "array", "items": { "type": "string" } },
                "poc_sketch": { "type": "string" }
            }
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        let args: RecordFindingArgs = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid input: {e}"), false),
        };
        if args.title.trim().is_empty() {
            return ToolResult::err("title must not be empty", false);
        }
        let kb = match kb_or_err(ctx) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let record = FindingRecord {
            title: args.title,
            severity: args.severity,
            category: args.category,
            summary: args.summary,
            vulnerable_code: args.vulnerable_code,
            location: args.location,
            reasoning: args.reasoning,
            related_findings: args.related_findings,
            poc_sketch: args.poc_sketch,
        };
        match kb
            .record_finding(ctx.session_id.as_str(), &args.target, record)
            .await
        {
            Ok(id) => ToolResult::ok(serde_json::json!({ "finding_id": id.as_str() })),
            Err(e) => ToolResult::err(e.to_string(), false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use basilisk_embeddings::{Embedding, EmbeddingError, EmbeddingInput, EmbeddingProvider};
    use basilisk_vector::{MemoryVectorStore, VectorStore};

    struct MockEmbed;

    #[async_trait]
    impl EmbeddingProvider for MockEmbed {
        #[allow(clippy::unnecessary_literal_bound)]
        fn identifier(&self) -> &str {
            "mock/test"
        }
        fn dimensions(&self) -> usize {
            8
        }
        fn max_tokens_per_input(&self) -> usize {
            1000
        }
        fn max_batch_size(&self) -> usize {
            32
        }
        async fn embed(
            &self,
            inputs: &[EmbeddingInput],
        ) -> Result<Vec<Embedding>, EmbeddingError> {
            Ok(inputs
                .iter()
                .map(|i| Embedding {
                    vector: (0..8_i16)
                        .map(|k| {
                            let len = u16::try_from(i.text.len()).unwrap_or(u16::MAX);
                            f32::from(len) + f32::from(k)
                        })
                        .collect(),
                    input_tokens: 1,
                })
                .collect())
        }
    }

    fn kb_context() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        std::mem::forget(dir); // live for the test's lifetime
        let store: Arc<dyn VectorStore> = Arc::new(MemoryVectorStore::new());
        let embed: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbed);
        let kb = Arc::new(KnowledgeBase::new(store, embed));
        let mut ctx = ToolContext::test(path);
        ctx.knowledge = Some(kb);
        ctx
    }

    fn empty_kb_context() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        ToolContext::test(path)
    }

    #[tokio::test]
    async fn search_rejects_empty_query() {
        let ctx = kb_context();
        let result = SearchKnowledgeBase
            .execute(serde_json::json!({"query": "   "}), &ctx)
            .await;
        match result {
            ToolResult::Err { retryable, .. } => assert!(!retryable),
            ToolResult::Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn search_returns_structured_error_when_no_kb_configured() {
        let ctx = empty_kb_context();
        let result = SearchKnowledgeBase
            .execute(serde_json::json!({"query": "reentrancy"}), &ctx)
            .await;
        match result {
            ToolResult::Err { message, retryable } => {
                assert!(!retryable);
                assert!(
                    message.contains("knowledge base is not configured"),
                    "got: {message}",
                );
            }
            ToolResult::Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn record_finding_returns_finding_id_on_success() {
        let ctx = kb_context();
        let result = RecordFinding
            .execute(
                serde_json::json!({
                    "title": "Reentrancy in withdraw",
                    "severity": "high",
                    "category": "reentrancy",
                    "summary": "Attacker can drain funds",
                    "target": "eth/0xdead",
                }),
                &ctx,
            )
            .await;
        match result {
            ToolResult::Ok(v) => assert!(v.get("finding_id").is_some()),
            other @ ToolResult::Err { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn record_finding_rejects_empty_title() {
        let ctx = kb_context();
        let result = RecordFinding
            .execute(
                serde_json::json!({
                    "title": "  ",
                    "severity": "low",
                    "category": "misc",
                    "summary": "s",
                    "target": "t",
                }),
                &ctx,
            )
            .await;
        assert!(matches!(result, ToolResult::Err { .. }));
    }

    #[tokio::test]
    async fn record_finding_without_kb_returns_structured_error() {
        let ctx = empty_kb_context();
        let result = RecordFinding
            .execute(
                serde_json::json!({
                    "title": "T",
                    "severity": "low",
                    "category": "c",
                    "summary": "s",
                    "target": "t",
                }),
                &ctx,
            )
            .await;
        assert!(matches!(result, ToolResult::Err { retryable: false, .. }));
    }

    #[tokio::test]
    async fn search_similar_code_rejects_empty_snippet() {
        let ctx = kb_context();
        let result = SearchSimilarCode
            .execute(serde_json::json!({"code_snippet": ""}), &ctx)
            .await;
        assert!(matches!(result, ToolResult::Err { retryable: false, .. }));
    }

    #[tokio::test]
    async fn search_protocol_docs_returns_empty_when_no_engagement_has_docs() {
        let mut ctx = kb_context();
        ctx.engagement_id = Some("eng-1".into());
        // No docs ingested → empty hits, not an error.
        let result = SearchProtocolDocs
            .execute(serde_json::json!({"query": "architecture"}), &ctx)
            .await;
        match result {
            ToolResult::Ok(v) => {
                let arr = v.as_array().expect("array");
                assert!(arr.is_empty());
            }
            other @ ToolResult::Err { .. } => panic!("expected Ok([]), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn input_schemas_are_valid_json_objects_with_required_properties() {
        for name_fn in [
            SearchKnowledgeBase.input_schema(),
            SearchSimilarCode.input_schema(),
            SearchProtocolDocs.input_schema(),
            RecordFinding.input_schema(),
        ] {
            assert_eq!(name_fn["type"], "object");
            assert!(name_fn.get("properties").is_some());
        }
    }

    #[test]
    fn tool_names_and_descriptions_nonempty() {
        for (name, desc) in [
            (SearchKnowledgeBase.name(), SearchKnowledgeBase.description()),
            (SearchSimilarCode.name(), SearchSimilarCode.description()),
            (SearchProtocolDocs.name(), SearchProtocolDocs.description()),
            (RecordFinding.name(), RecordFinding.description()),
        ] {
            assert!(!name.is_empty());
            assert!(desc.len() > 50, "description too short: {desc}");
        }
    }
}
