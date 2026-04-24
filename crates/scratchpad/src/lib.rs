//! Agent working memory — a structured, mutable, persistent
//! scratchpad maintained across a session.
//!
//! The scratchpad is the agent's own working document. It carries
//! the evolving system understanding, hypotheses, confirmed
//! findings, open questions, limitations, and suspicions. The
//! agent owns what it writes; we provide shape and persistence.
//!
//! # Layering
//!
//! `basilisk-scratchpad` depends only on `basilisk-core`, rusqlite,
//! serde. The agent crate registers the scratchpad tools and wires
//! the compact render into its system prompt. The CLI exposes
//! inspection commands. No circular dependency.
//!
//! # Persistence
//!
//! Lives alongside sessions in `~/.basilisk/sessions.db` via two
//! tables: `scratchpads` (current state) + `scratchpad_revisions`
//! (capped history). Schema is v3; the agent's `SessionStore`
//! calls [`apply_schema`] as part of its own migration path.
//!
//! # Surface
//!
//! - [`Scratchpad`] — the in-memory data model.
//! - [`ScratchpadStore`] — the `SQLite` persistence adapter
//!   (available once CP8.2 lands; see TODO).
//! - [`render_markdown`] / [`render_compact`] — the two render
//!   paths used by the CLI and agent system prompt, respectively.

pub mod error;
pub mod render;
pub mod types;

pub use error::ScratchpadError;
pub use render::{render_compact, render_markdown, COMPACT_TOKEN_BUDGET};
pub use types::{
    now_ms, Item, ItemId, ItemRevision, ItemStatus, ItemUpdate, ItemsSection, ProseSection,
    Scratchpad, Section, SectionKey, SectionKind, ITEM_HISTORY_CAP, SCRATCHPAD_SCHEMA_VERSION,
};

/// Validate a proposed custom-section name. Returns `Ok(())` if
/// the name is ASCII alphanumeric-plus-underscore, non-empty, ≤64
/// chars, and not a reserved built-in name.
///
/// # Errors
///
/// Returns [`ScratchpadError::InvalidCustomName`] with a
/// human-readable reason on any failure.
pub fn validate_custom_name(name: &str) -> Result<(), ScratchpadError> {
    const RESERVED: &[&str] = &[
        "system_understanding",
        "hypotheses",
        "confirmed_findings",
        "dismissed_hypotheses",
        "open_questions",
        "investigations",
        "limitations_noticed",
        "suspicions_not_yet_confirmed",
    ];
    if name.is_empty() {
        return Err(ScratchpadError::InvalidCustomName {
            name: name.into(),
            reason: "empty".into(),
        });
    }
    if name.len() > 64 {
        return Err(ScratchpadError::InvalidCustomName {
            name: name.into(),
            reason: format!("too long ({} chars; max 64)", name.len()),
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(ScratchpadError::InvalidCustomName {
            name: name.into(),
            reason: "only ASCII alphanumeric + underscore allowed".into(),
        });
    }
    if RESERVED.contains(&name) {
        return Err(ScratchpadError::InvalidCustomName {
            name: name.into(),
            reason: "collides with a built-in section".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_scratchpad_has_all_required_sections() {
        let sp = Scratchpad::new("sess-abc");
        for key in SectionKey::required() {
            assert!(sp.has_section(&key), "missing {}", key.wire_name());
        }
        assert_eq!(sp.item_count(), 0);
        assert_eq!(sp.next_item_id, 1);
    }

    #[test]
    fn section_iteration_is_in_declared_order() {
        let sp = Scratchpad::new("sess-xyz");
        let keys: Vec<_> = sp.sections.keys().map(SectionKey::wire_name).collect();
        assert_eq!(
            keys,
            vec![
                "system_understanding",
                "hypotheses",
                "confirmed_findings",
                "dismissed_hypotheses",
                "open_questions",
                "investigations",
                "limitations_noticed",
                "suspicions_not_yet_confirmed",
            ],
        );
    }

    #[test]
    fn scratchpad_roundtrips_through_json() {
        let sp = Scratchpad::new("sess-1");
        let json = serde_json::to_string(&sp).unwrap();
        let back: Scratchpad = serde_json::from_str(&json).unwrap();
        assert_eq!(sp, back);
    }

    #[test]
    fn section_key_wire_name_roundtrips() {
        for key in SectionKey::required() {
            let name = key.wire_name();
            assert_eq!(SectionKey::parse(&name), Some(key));
        }
        assert_eq!(
            SectionKey::parse("oracle_tree"),
            Some(SectionKey::Custom("oracle_tree".into())),
        );
        assert_eq!(SectionKey::parse("bad-name"), None);
    }

    #[test]
    fn render_markdown_lists_all_sections_even_when_empty() {
        let sp = Scratchpad::new("sess-empty");
        let md = render_markdown(&sp);
        assert!(md.contains("System understanding"));
        assert!(md.contains("Hypotheses"));
        assert!(md.contains("Limitations noticed"));
        assert!(md.contains("Suspicions (not yet confirmed)"));
        // Empty sections surface an "(empty)" marker.
        assert!(md.contains("_(empty)_"));
    }

    #[test]
    fn render_compact_stays_under_budget_for_fresh_scratchpad() {
        let sp = Scratchpad::new("sess-bounded");
        let out = render_compact(&sp);
        // Fresh scratchpad is tiny — well under the budget.
        assert!(out.len() < 1000, "compact len={}", out.len());
    }

    #[test]
    fn validate_custom_name_rejects_collisions_and_bad_chars() {
        assert!(validate_custom_name("").is_err());
        assert!(validate_custom_name("hypotheses").is_err());
        assert!(validate_custom_name("bad-chars").is_err());
        assert!(validate_custom_name("has spaces").is_err());
        let long: String = std::iter::repeat_n('a', 65).collect();
        assert!(validate_custom_name(&long).is_err());
        assert!(validate_custom_name("oracle_dependency_tree").is_ok());
    }

    #[test]
    fn is_required_distinguishes_builtin_from_custom() {
        for key in SectionKey::required() {
            assert!(key.is_required());
        }
        assert!(!SectionKey::Custom("x".into()).is_required());
    }
}
