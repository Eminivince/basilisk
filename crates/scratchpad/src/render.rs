//! Markdown render paths — one full, one bounded.
//!
//! `render_markdown` is the human-readable form used by `audit
//! session scratchpad show <id>`. `render_compact` is the
//! agent-context form pinned into the system prompt: always
//! bounded in size, truncates long items, summarises oversized
//! lists so the LLM never has to reason through 500-line
//! scratchpads.

use std::fmt::Write;

use crate::types::{Item, ItemsSection, ProseSection, Scratchpad, Section, SectionKey};

/// Token-budget ceiling for [`render_compact`]. Enforced as a
/// *byte* budget via the same `bytes/4` heuristic used by the
/// embeddings token estimator — conservative and dependency-free.
pub const COMPACT_TOKEN_BUDGET: usize = 4000;

const BYTES_PER_TOKEN_ESTIMATE: usize = 4;
const COMPACT_BYTE_BUDGET: usize = COMPACT_TOKEN_BUDGET * BYTES_PER_TOKEN_ESTIMATE;

/// Upper bound on chars per item in the compact render. Enough to
/// carry a one-line summary without ballooning; longer items get
/// truncated with an ellipsis.
const COMPACT_ITEM_CONTENT_CEILING: usize = 240;

/// Cap on how many items per section survive in the compact render
/// before we switch to "N items; showing M most recently updated".
const COMPACT_ITEMS_PER_SECTION_CEILING: usize = 20;

/// Full human-readable markdown. No truncation; every item is
/// rendered with its id, status, tags, and full content.
#[must_use]
pub fn render_markdown(sp: &Scratchpad) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Scratchpad — session {}\n\n_schema v{}, updated {}ms, {} items total_\n",
        sp.session_id,
        sp.schema_version,
        sp.updated_at_ms,
        sp.item_count(),
    );

    for (key, section) in &sp.sections {
        let _ = writeln!(out, "## {}\n", section_heading(key));
        match section {
            Section::Prose(p) => render_prose_full(p, &mut out),
            Section::Items(i) => render_items_full(i, &mut out),
        }
        out.push('\n');
    }
    out
}

/// Compact render pinned into the system prompt. Guaranteed to
/// stay under [`COMPACT_TOKEN_BUDGET`] tokens (via
/// [`COMPACT_BYTE_BUDGET`] bytes) regardless of scratchpad size.
/// Oversized content is truncated; oversized lists report their
/// length and show only the most recently updated items.
#[must_use]
pub fn render_compact(sp: &Scratchpad) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Scratchpad — session {} ({} items)\n",
        sp.session_id,
        sp.item_count(),
    );

    for (key, section) in &sp.sections {
        let _ = writeln!(out, "## {}", section_heading(key));
        match section {
            Section::Prose(p) => render_prose_compact(p, &mut out),
            Section::Items(i) => render_items_compact(i, &mut out),
        }
        out.push('\n');

        // Hard cap — if we're already over budget, stop adding
        // sections and emit a tail marker so the agent knows.
        if out.len() > COMPACT_BYTE_BUDGET {
            truncate_at_char_boundary(&mut out, COMPACT_BYTE_BUDGET);
            out.push_str("\n… [truncated — compact render at budget ceiling]\n");
            return out;
        }
    }
    out
}

/// Trim `s` to at most `max_bytes` without splitting a UTF-8
/// codepoint. If `max_bytes` lands inside a multi-byte scalar, we
/// step back to the nearest char boundary.
fn truncate_at_char_boundary(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

// --- section-heading mapping -----------------------------------------

fn section_heading(key: &SectionKey) -> String {
    match key {
        SectionKey::SystemUnderstanding => "System understanding".into(),
        SectionKey::Hypotheses => "Hypotheses".into(),
        SectionKey::ConfirmedFindings => "Confirmed findings".into(),
        SectionKey::DismissedHypotheses => "Dismissed hypotheses".into(),
        SectionKey::OpenQuestions => "Open questions".into(),
        SectionKey::Investigations => "Investigations".into(),
        SectionKey::LimitationsNoticed => "Limitations noticed".into(),
        SectionKey::SuspicionsNotYetConfirmed => "Suspicions (not yet confirmed)".into(),
        SectionKey::Custom(name) => format!("Custom: {name}"),
    }
}

// --- full render helpers ---------------------------------------------

fn render_prose_full(p: &ProseSection, out: &mut String) {
    if p.markdown.trim().is_empty() {
        out.push_str("_(empty)_\n");
    } else {
        out.push_str(&p.markdown);
        if !p.markdown.ends_with('\n') {
            out.push('\n');
        }
    }
}

fn render_items_full(i: &ItemsSection, out: &mut String) {
    if i.items.is_empty() {
        out.push_str("_(empty)_\n");
        return;
    }
    for item in &i.items {
        let _ = write!(
            out,
            "- **#{}** `[{}]` {}",
            item.id,
            item.status.label(),
            item.content,
        );
        if !item.tags.is_empty() {
            let _ = write!(out, "  _tags: {}_", item.tags.join(", "));
        }
        out.push('\n');
    }
}

// --- compact render helpers ------------------------------------------

fn render_prose_compact(p: &ProseSection, out: &mut String) {
    if p.markdown.trim().is_empty() {
        out.push_str("_(empty)_\n");
        return;
    }
    // Prose fits one paragraph in compact form. Truncate to ~480
    // chars (≈120 tokens) so a long prose block doesn't dominate.
    let snippet = truncate(&p.markdown, 480);
    out.push_str(&snippet);
    out.push('\n');
}

fn render_items_compact(i: &ItemsSection, out: &mut String) {
    if i.items.is_empty() {
        out.push_str("_(empty)_\n");
        return;
    }
    let total = i.items.len();
    if total <= COMPACT_ITEMS_PER_SECTION_CEILING {
        for item in &i.items {
            render_item_compact_line(item, out);
        }
        return;
    }
    // Oversize list: show the N most recently updated.
    let mut ranked: Vec<&Item> = i.items.iter().collect();
    ranked.sort_by_key(|it| std::cmp::Reverse(it.updated_at_ms));
    let show = &ranked[..COMPACT_ITEMS_PER_SECTION_CEILING];
    let _ = writeln!(
        out,
        "_{} items; showing {} most recently updated_",
        total,
        show.len(),
    );
    for item in show {
        render_item_compact_line(item, out);
    }
}

fn render_item_compact_line(item: &Item, out: &mut String) {
    let content = truncate(&item.content, COMPACT_ITEM_CONTENT_CEILING);
    let _ = writeln!(out, "- #{} [{}] {}", item.id, item.status.label(), content);
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.replace('\n', " ");
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{}…", truncated.replace('\n', " "))
}
