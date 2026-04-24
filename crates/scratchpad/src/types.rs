//! Scratchpad data model.
//!
//! A scratchpad is the agent's working document during a session:
//! where it writes its evolving understanding, hypotheses, confirmed
//! findings, open questions, limitations, and suspicions. The model
//! is structured but not rigid — named sections with items that
//! carry stable ids, statuses, and revision history.
//!
//! All timestamps are stored as milliseconds since the Unix epoch
//! to match the existing session-store wire format — no `chrono`
//! dependency.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::ScratchpadError;

/// Stable id of an item within a scratchpad. Monotonically
/// increasing per scratchpad; never reused within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub u64);

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Canonical section identity. Declared order below is also the
/// render order — `BTreeMap<SectionKey, ..>` iterates in variant-
/// discriminant order, with `Custom` variants sorting last by their
/// string content.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKey {
    /// The agent's evolving prose model of the system under audit.
    SystemUnderstanding,
    /// Active theories the agent is considering.
    Hypotheses,
    /// Hypotheses that have been confirmed by evidence.
    ConfirmedFindings,
    /// Hypotheses discarded with reasoning.
    DismissedHypotheses,
    /// Things the agent doesn't yet know.
    OpenQuestions,
    /// Threads the agent is actively pursuing.
    Investigations,
    /// Gaps in tooling, data, or reasoning the agent has hit.
    LimitationsNoticed,
    /// Red flags the agent can't yet prove — first-class for
    /// Set 9's vulnerability reasoning.
    SuspicionsNotYetConfirmed,
    /// Agent-authored engagement-specific sections.
    #[serde(untagged)]
    Custom(String),
}

impl SectionKey {
    /// The eight required sections present in every scratchpad.
    #[must_use]
    pub fn required() -> [Self; 8] {
        [
            Self::SystemUnderstanding,
            Self::Hypotheses,
            Self::ConfirmedFindings,
            Self::DismissedHypotheses,
            Self::OpenQuestions,
            Self::Investigations,
            Self::LimitationsNoticed,
            Self::SuspicionsNotYetConfirmed,
        ]
    }

    /// True for the eight built-in sections. Used to reject
    /// `remove` operations that would drop a required section.
    #[must_use]
    pub fn is_required(&self) -> bool {
        !matches!(self, Self::Custom(_))
    }

    /// Wire name used in tool inputs / CLI flags. Matches the
    /// `serde(rename_all = "snake_case")` variant; custom sections
    /// round-trip as their raw string.
    #[must_use]
    pub fn wire_name(&self) -> String {
        match self {
            Self::SystemUnderstanding => "system_understanding".into(),
            Self::Hypotheses => "hypotheses".into(),
            Self::ConfirmedFindings => "confirmed_findings".into(),
            Self::DismissedHypotheses => "dismissed_hypotheses".into(),
            Self::OpenQuestions => "open_questions".into(),
            Self::Investigations => "investigations".into(),
            Self::LimitationsNoticed => "limitations_noticed".into(),
            Self::SuspicionsNotYetConfirmed => "suspicions_not_yet_confirmed".into(),
            Self::Custom(name) => name.clone(),
        }
    }

    /// Parse from the wire name. Returns `None` for names that
    /// fail custom-section validation (see
    /// [`validate_custom_name`](crate::validate_custom_name)).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "system_understanding" => Some(Self::SystemUnderstanding),
            "hypotheses" => Some(Self::Hypotheses),
            "confirmed_findings" => Some(Self::ConfirmedFindings),
            "dismissed_hypotheses" => Some(Self::DismissedHypotheses),
            "open_questions" => Some(Self::OpenQuestions),
            "investigations" => Some(Self::Investigations),
            "limitations_noticed" => Some(Self::LimitationsNoticed),
            "suspicions_not_yet_confirmed" => Some(Self::SuspicionsNotYetConfirmed),
            other if crate::validate_custom_name(other).is_ok() => Some(Self::Custom(other.into())),
            _ => None,
        }
    }

    /// The kind of section this built-in key expects. Callers use
    /// this when rebuilding empty sections during initialization.
    #[must_use]
    pub fn default_kind(&self) -> SectionKind {
        match self {
            Self::SystemUnderstanding => SectionKind::Prose,
            _ => SectionKind::Items,
        }
    }
}

/// Discriminant for section storage shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    Prose,
    Items,
}

/// A section is either a prose block or a list of structured items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Section {
    Prose(ProseSection),
    Items(ItemsSection),
}

impl Section {
    /// A freshly-initialized empty section of the given kind.
    #[must_use]
    pub fn empty(kind: SectionKind) -> Self {
        match kind {
            SectionKind::Prose => Self::Prose(ProseSection {
                markdown: String::new(),
                last_updated_ms: now_ms(),
            }),
            SectionKind::Items => Self::Items(ItemsSection {
                items: Vec::new(),
                last_updated_ms: now_ms(),
            }),
        }
    }

    #[must_use]
    pub fn kind(&self) -> SectionKind {
        match self {
            Self::Prose(_) => SectionKind::Prose,
            Self::Items(_) => SectionKind::Items,
        }
    }

    /// True iff the section holds no content — used by the markdown
    /// renderers to surface empty required sections explicitly
    /// rather than hiding them.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Prose(p) => p.markdown.trim().is_empty(),
            Self::Items(i) => i.items.is_empty(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProseSection {
    pub markdown: String,
    pub last_updated_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemsSection {
    pub items: Vec<Item>,
    pub last_updated_ms: u64,
}

/// Lifecycle of a tracked item. The status is advisory — the agent
/// is free to move items between states as the investigation
/// evolves. `Blocked` carries a free-form reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ItemStatus {
    Open,
    InProgress,
    Confirmed,
    Dismissed,
    Blocked { reason: String },
}

impl ItemStatus {
    /// Short human label used in the compact render.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in-progress",
            Self::Confirmed => "confirmed",
            Self::Dismissed => "dismissed",
            Self::Blocked { .. } => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub content: String,
    pub status: ItemStatus,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Capped at [`ITEM_HISTORY_CAP`] entries; older revisions drop
    /// off the front when the cap is exceeded.
    #[serde(default)]
    pub history: Vec<ItemRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRevision {
    pub at_ms: u64,
    pub content: String,
    pub status: ItemStatus,
}

/// Partial update applied by [`Scratchpad::update_item`].
#[derive(Debug, Clone, Default)]
pub struct ItemUpdate {
    pub content: Option<String>,
    pub status: Option<ItemStatus>,
    pub tags: Option<Vec<String>>,
}

/// Per-item revision cap. Older entries fall off the front.
pub const ITEM_HISTORY_CAP: usize = 5;

/// The scratchpad itself — a per-session working document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scratchpad {
    /// The session this scratchpad belongs to. Stored as a bare
    /// string so the crate doesn't depend on `basilisk-agent`.
    pub session_id: String,
    pub schema_version: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Insertion order is the render order — `BTreeMap` sorts by
    /// the enum's declared variant order (with `Custom` sections
    /// last, alphabetised).
    pub sections: BTreeMap<SectionKey, Section>,
    /// Running counter for item ids; never resets within a session.
    pub next_item_id: u64,
}

impl Scratchpad {
    /// Build a fresh scratchpad with all eight required sections
    /// empty. Called once per session at creation.
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        let now = now_ms();
        let mut sections = BTreeMap::new();
        for key in SectionKey::required() {
            sections.insert(key.clone(), Section::empty(key.default_kind()));
        }
        Self {
            session_id: session_id.into(),
            schema_version: SCRATCHPAD_SCHEMA_VERSION,
            created_at_ms: now,
            updated_at_ms: now,
            sections,
            next_item_id: 1,
        }
    }

    /// Total items across every section.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.sections
            .values()
            .map(|s| match s {
                Section::Items(i) => i.items.len(),
                Section::Prose(_) => 0,
            })
            .sum()
    }

    /// Approximate byte size of the current state — the same
    /// metric exposed by [`ScratchpadSummary`](crate::ScratchpadSummary).
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        serde_json::to_vec(&self.sections)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// True iff `key` is present in this scratchpad.
    #[must_use]
    pub fn has_section(&self, key: &SectionKey) -> bool {
        self.sections.contains_key(key)
    }

    /// Guaranteed-present reference to a built-in section. Returns
    /// `MissingSection` for `Custom` keys that haven't been created.
    #[allow(dead_code)] // used once CP8.3 lands the ops API
    pub(crate) fn section(&self, key: &SectionKey) -> Result<&Section, ScratchpadError> {
        self.sections
            .get(key)
            .ok_or_else(|| ScratchpadError::MissingSection(key.wire_name()))
    }

    #[allow(dead_code)] // used once CP8.3 lands the ops API
    pub(crate) fn section_mut(&mut self, key: &SectionKey) -> Result<&mut Section, ScratchpadError> {
        self.sections
            .get_mut(key)
            .ok_or_else(|| ScratchpadError::MissingSection(key.wire_name()))
    }
}

/// Schema version embedded in every persisted scratchpad. Bump
/// when the on-disk shape changes.
pub const SCRATCHPAD_SCHEMA_VERSION: u32 = 1;

/// Current milliseconds since the Unix epoch. Saturates at `0` on
/// platforms where the system clock is before the epoch.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
