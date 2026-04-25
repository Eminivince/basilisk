//! Mutation API on [`Scratchpad`]. The persistence layer stays
//! in `store.rs`; this module is pure in-memory state.

use crate::{
    error::ScratchpadError,
    types::{
        now_ms, Item, ItemId, ItemRevision, ItemStatus, ItemUpdate, Scratchpad, Section,
        SectionKey, SectionKind, ITEM_HISTORY_CAP,
    },
    validate_custom_name,
};

impl Scratchpad {
    /// Replace the prose content of a prose section. Errors when
    /// the section is absent or an items section.
    ///
    /// # Errors
    ///
    /// - [`ScratchpadError::MissingSection`] if the key isn't in
    ///   this scratchpad.
    /// - [`ScratchpadError::WrongSectionKind`] if the target is an
    ///   items section.
    pub fn set_prose(
        &mut self,
        key: &SectionKey,
        markdown: impl Into<String>,
    ) -> Result<(), ScratchpadError> {
        let now = now_ms();
        let section = self.section_mut(key)?;
        match section {
            Section::Prose(p) => {
                p.markdown = markdown.into();
                p.last_updated_ms = now;
            }
            Section::Items(_) => {
                return Err(ScratchpadError::WrongSectionKind {
                    section: key.wire_name(),
                    expected: "prose",
                    actual: "items",
                });
            }
        }
        self.updated_at_ms = now;
        Ok(())
    }

    /// Append a new item to an items section. Returns its stable
    /// [`ItemId`]. Errors on wrong-kind / missing-section.
    ///
    /// # Errors
    ///
    /// Same cases as [`Self::set_prose`], but raised in the
    /// items-section variant.
    pub fn append_item(
        &mut self,
        key: &SectionKey,
        content: impl Into<String>,
        tags: Vec<String>,
    ) -> Result<ItemId, ScratchpadError> {
        let now = now_ms();
        let id = ItemId(self.next_item_id);
        self.next_item_id = self.next_item_id.saturating_add(1);

        let section = self.section_mut(key)?;
        let items = match section {
            Section::Items(i) => i,
            Section::Prose(_) => {
                return Err(ScratchpadError::WrongSectionKind {
                    section: key.wire_name(),
                    expected: "items",
                    actual: "prose",
                });
            }
        };
        items.items.push(Item {
            id,
            content: content.into(),
            status: ItemStatus::Open,
            tags,
            created_at_ms: now,
            updated_at_ms: now,
            history: Vec::new(),
        });
        items.last_updated_ms = now;
        self.updated_at_ms = now;
        Ok(id)
    }

    /// Mutate an item in place. Any combination of content / status
    /// / tags may be updated; each call snapshots the prior state
    /// into the item's capped [`Item::history`].
    ///
    /// # Errors
    ///
    /// - [`ScratchpadError::MissingSection`] / [`ScratchpadError::WrongSectionKind`]
    /// - [`ScratchpadError::ItemNotFound`] when the id doesn't match
    ///   an item in the target section.
    pub fn update_item(
        &mut self,
        key: &SectionKey,
        item_id: ItemId,
        update: ItemUpdate,
    ) -> Result<(), ScratchpadError> {
        let now = now_ms();
        let section_name = key.wire_name();
        let section = self.section_mut(key)?;
        let items = match section {
            Section::Items(i) => i,
            Section::Prose(_) => {
                return Err(ScratchpadError::WrongSectionKind {
                    section: section_name,
                    expected: "items",
                    actual: "prose",
                });
            }
        };
        let item = items
            .items
            .iter_mut()
            .find(|it| it.id == item_id)
            .ok_or_else(|| ScratchpadError::ItemNotFound {
                section: section_name,
                item_id: item_id.0,
            })?;

        // Snapshot the pre-update state into history before applying.
        item.history.push(ItemRevision {
            at_ms: item.updated_at_ms,
            content: item.content.clone(),
            status: item.status.clone(),
        });
        if item.history.len() > ITEM_HISTORY_CAP {
            // Drop the oldest — VecDeque would be marginally cleaner
            // but the cap is tiny (5), so Vec::remove(0) is fine.
            let excess = item.history.len() - ITEM_HISTORY_CAP;
            item.history.drain(0..excess);
        }

        if let Some(c) = update.content {
            item.content = c;
        }
        if let Some(s) = update.status {
            item.status = s;
        }
        if let Some(t) = update.tags {
            item.tags = t;
        }
        item.updated_at_ms = now;
        items.last_updated_ms = now;
        self.updated_at_ms = now;
        Ok(())
    }

    /// Remove an item from an items section. The section itself is
    /// never removed — built-in sections are load-bearing.
    ///
    /// # Errors
    ///
    /// Same as [`Self::update_item`].
    pub fn remove_item(
        &mut self,
        key: &SectionKey,
        item_id: ItemId,
    ) -> Result<(), ScratchpadError> {
        let now = now_ms();
        let section_name = key.wire_name();
        let section = self.section_mut(key)?;
        let items = match section {
            Section::Items(i) => i,
            Section::Prose(_) => {
                return Err(ScratchpadError::WrongSectionKind {
                    section: section_name,
                    expected: "items",
                    actual: "prose",
                });
            }
        };
        let before = items.items.len();
        items.items.retain(|it| it.id != item_id);
        if items.items.len() == before {
            return Err(ScratchpadError::ItemNotFound {
                section: section_name,
                item_id: item_id.0,
            });
        }
        items.last_updated_ms = now;
        self.updated_at_ms = now;
        Ok(())
    }

    /// Register a new `Custom` section. Idempotent — creating a
    /// section that already exists succeeds as a no-op so long as
    /// the kind matches.
    ///
    /// # Errors
    ///
    /// - [`ScratchpadError::InvalidCustomName`] when `name` fails
    ///   [`validate_custom_name`].
    /// - [`ScratchpadError::WrongSectionKind`] when `name` already
    ///   exists with a different kind.
    pub fn create_custom_section(
        &mut self,
        name: impl Into<String>,
        kind: SectionKind,
    ) -> Result<(), ScratchpadError> {
        let name = name.into();
        validate_custom_name(&name)?;
        let key = SectionKey::Custom(name.clone());
        if let Some(existing) = self.sections.get(&key) {
            let existing_kind = existing.kind();
            if existing_kind == kind {
                return Ok(());
            }
            return Err(ScratchpadError::WrongSectionKind {
                section: name,
                expected: match kind {
                    SectionKind::Prose => "prose",
                    SectionKind::Items => "items",
                },
                actual: match existing_kind {
                    SectionKind::Prose => "prose",
                    SectionKind::Items => "items",
                },
            });
        }
        self.sections.insert(key, Section::empty(kind));
        self.updated_at_ms = now_ms();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_prose_updates_system_understanding() {
        let mut sp = Scratchpad::new("s");
        sp.set_prose(&SectionKey::SystemUnderstanding, "It's a staking pool.")
            .unwrap();
        match sp.sections.get(&SectionKey::SystemUnderstanding).unwrap() {
            Section::Prose(p) => assert!(p.markdown.contains("staking")),
            Section::Items(_) => panic!("expected prose"),
        }
    }

    #[test]
    fn set_prose_on_items_section_errors() {
        let mut sp = Scratchpad::new("s");
        let err = sp.set_prose(&SectionKey::Hypotheses, "nope").unwrap_err();
        assert!(matches!(err, ScratchpadError::WrongSectionKind { .. }));
    }

    #[test]
    fn append_item_assigns_monotonic_ids() {
        let mut sp = Scratchpad::new("s");
        let a = sp
            .append_item(&SectionKey::Hypotheses, "reentrancy", vec![])
            .unwrap();
        let b = sp
            .append_item(&SectionKey::Hypotheses, "oracle skew", vec![])
            .unwrap();
        assert_eq!(a.0, 1);
        assert_eq!(b.0, 2);
        assert_eq!(sp.next_item_id, 3);
    }

    #[test]
    fn update_item_captures_prior_revision() {
        let mut sp = Scratchpad::new("s");
        let id = sp
            .append_item(&SectionKey::Hypotheses, "v1 text", vec![])
            .unwrap();
        sp.update_item(
            &SectionKey::Hypotheses,
            id,
            ItemUpdate {
                content: Some("v2 text".into()),
                status: Some(ItemStatus::InProgress),
                tags: None,
            },
        )
        .unwrap();
        let items = match sp.sections.get(&SectionKey::Hypotheses).unwrap() {
            Section::Items(i) => i,
            Section::Prose(_) => unreachable!(),
        };
        assert_eq!(items.items[0].content, "v2 text");
        assert!(matches!(items.items[0].status, ItemStatus::InProgress));
        assert_eq!(items.items[0].history.len(), 1);
        assert_eq!(items.items[0].history[0].content, "v1 text");
    }

    #[test]
    fn update_item_history_caps_at_five() {
        let mut sp = Scratchpad::new("s");
        let id = sp
            .append_item(&SectionKey::Hypotheses, "initial", vec![])
            .unwrap();
        for i in 0..10 {
            sp.update_item(
                &SectionKey::Hypotheses,
                id,
                ItemUpdate {
                    content: Some(format!("rev {i}")),
                    status: None,
                    tags: None,
                },
            )
            .unwrap();
        }
        let items = match sp.sections.get(&SectionKey::Hypotheses).unwrap() {
            Section::Items(i) => i,
            Section::Prose(_) => unreachable!(),
        };
        assert_eq!(items.items[0].history.len(), ITEM_HISTORY_CAP);
        // Oldest retained should be the one that matches our 10-i+cap=5
        // boundary: rev "initial" has been dropped; rev 5..=9 plus the
        // pre-rev9 snapshot survive.
        let last_content = &items.items[0].content;
        assert_eq!(last_content, "rev 9");
    }

    #[test]
    fn remove_item_removes_then_fails_twice() {
        let mut sp = Scratchpad::new("s");
        let id = sp
            .append_item(&SectionKey::OpenQuestions, "?", vec![])
            .unwrap();
        sp.remove_item(&SectionKey::OpenQuestions, id).unwrap();
        let err = sp.remove_item(&SectionKey::OpenQuestions, id).unwrap_err();
        assert!(matches!(err, ScratchpadError::ItemNotFound { .. }));
    }

    #[test]
    fn custom_prose_section_creates_and_writes() {
        let mut sp = Scratchpad::new("s");
        sp.create_custom_section("oracle_tree", SectionKind::Prose)
            .unwrap();
        let key = SectionKey::Custom("oracle_tree".into());
        sp.set_prose(&key, "Chainlink → UniV3 TWAP → something")
            .unwrap();
        assert!(sp.has_section(&key));
    }

    #[test]
    fn custom_section_name_validation_rejects_reserved() {
        let mut sp = Scratchpad::new("s");
        let err = sp
            .create_custom_section("hypotheses", SectionKind::Items)
            .unwrap_err();
        assert!(matches!(err, ScratchpadError::InvalidCustomName { .. }));
    }

    #[test]
    fn custom_section_kind_conflict_errors() {
        let mut sp = Scratchpad::new("s");
        sp.create_custom_section("notes", SectionKind::Prose)
            .unwrap();
        let err = sp
            .create_custom_section("notes", SectionKind::Items)
            .unwrap_err();
        assert!(matches!(err, ScratchpadError::WrongSectionKind { .. }));
    }

    #[test]
    fn custom_section_re_create_same_kind_is_idempotent() {
        let mut sp = Scratchpad::new("s");
        sp.create_custom_section("notes", SectionKind::Prose)
            .unwrap();
        sp.create_custom_section("notes", SectionKind::Prose)
            .unwrap();
    }

    #[test]
    fn update_item_missing_item_errors() {
        let mut sp = Scratchpad::new("s");
        let err = sp
            .update_item(&SectionKey::Hypotheses, ItemId(999), ItemUpdate::default())
            .unwrap_err();
        assert!(matches!(err, ScratchpadError::ItemNotFound { .. }));
    }

    // --- bounded compact render -----------------------------------

    #[test]
    fn compact_render_bytes_report() {
        let mut sp = Scratchpad::new("bench");
        for i in 0..500 {
            sp.append_item(
                &SectionKey::Hypotheses,
                format!(
                    "Hypothesis #{i}: a plausible theory across multiple contracts with \
                     some reasoning that would expand the token budget. ({i})"
                ),
                vec![format!("tag-{i}")],
            )
            .unwrap();
        }
        let out = crate::render_compact(&sp);
        // Print so `cargo test -- --nocapture` shows the measured
        // bytes / approximate-token count. The assertion below is
        // the load-bearing part — this is just visibility.
        println!(
            "compact-render-500-items: {} bytes ≈ {} tokens (budget ceiling: {})",
            out.len(),
            out.len() / 4,
            crate::COMPACT_TOKEN_BUDGET,
        );
        assert!(out.len() < 16_000);
    }

    #[test]
    fn compact_render_stays_under_budget_with_500_items() {
        let mut sp = Scratchpad::new("s");
        for i in 0..500 {
            sp.append_item(
                &SectionKey::Hypotheses,
                format!(
                    "Hypothesis #{i}: a fairly long explanation of some plausible theory \
                     involving multiple contracts and some reasoning that would expand \
                     the token budget if we didn't cap it. ({i})"
                ),
                vec![format!("tag-{i}")],
            )
            .unwrap();
        }
        let out = crate::render_compact(&sp);
        // Budget ceiling is COMPACT_TOKEN_BUDGET tokens ~= 16000 bytes.
        assert!(
            out.len() < 16_000,
            "compact render length {} exceeded 16000-byte budget",
            out.len(),
        );
        // Should surface the summary line.
        assert!(out.contains("500 items; showing"));
    }

    #[test]
    fn compact_render_with_custom_section_mixes_both() {
        let mut sp = Scratchpad::new("s");
        sp.create_custom_section("oracle_tree", SectionKind::Prose)
            .unwrap();
        sp.set_prose(
            &SectionKey::Custom("oracle_tree".into()),
            "Chainlink → UniV3 TWAP",
        )
        .unwrap();
        sp.append_item(&SectionKey::Hypotheses, "h1", vec![])
            .unwrap();
        let out = crate::render_compact(&sp);
        assert!(out.contains("Hypotheses"));
        assert!(out.contains("Custom: oracle_tree"));
        assert!(out.contains("Chainlink"));
    }
}
