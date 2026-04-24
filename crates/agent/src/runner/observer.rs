//! Per-turn progress hooks for [`AgentRunner`] (`CP6b`).
//!
//! The runner is long-running: one "turn" (assistant round-trip + tool
//! dispatch) can take seconds to tens of seconds against a real LLM.
//! Without surface-level progress, the CLI appears frozen. This module
//! defines a thin observer trait that the runner fires as a session
//! progresses; implementors render those events — as terminal prints
//! (CLI), UI updates (future TUI), test assertions, or nothing at all.
//!
//! Design rules:
//!
//!  - **Defaults are no-ops.** A stub impl lets callers override just
//!    the hooks they care about.
//!  - **`&self`, never `&mut`.** Observers that need mutable state use
//!    interior mutability. The runner's loop is already complex; we
//!    won't sprinkle `&mut observer` through it.
//!  - **Hot-path cheap.** `on_text_delta` runs once per streamed token
//!    fragment. Don't allocate unnecessarily inside it.
//!  - **Synchronous.** No async hooks — we're not about to pause the
//!    loop for an observer's I/O.

use crate::runner::types::{AgentOutcome, AgentStats};
use crate::tool::SessionId;

/// Progress hooks fired by [`AgentRunner`] during one run.
///
/// Every method has a default no-op implementation, so the minimal
/// override is an empty impl block:
///
/// ```ignore
/// struct MyObs;
/// impl AgentObserver for MyObs {
///     fn on_text_delta(&self, _turn: u32, text: &str) {
///         print!("{text}");
///     }
/// }
/// ```
///
/// The `turn` argument is a 1-based counter matching
/// [`AgentStats::turns`] after the current turn completes (i.e. if this
/// is turn 3's second delta, `turn == 3`).
pub trait AgentObserver: Send + Sync {
    /// Called once per session, just after the session row is created.
    fn on_session_start(&self, _session_id: &SessionId) {}

    /// Called at the top of each turn, before the LLM stream starts.
    fn on_turn_start(&self, _turn: u32) {}

    /// Called for each streamed text fragment from the assistant.
    /// `text` is the newly-arrived delta, not the cumulative buffer.
    fn on_text_delta(&self, _turn: u32, _text: &str) {}

    /// Called when a new `tool_use` block begins streaming. Fires at
    /// most once per tool call — arguments are not yet available.
    fn on_tool_use_start(&self, _turn: u32, _name: &str, _tool_use_id: &str) {}

    /// Called once the tool has been dispatched and its result
    /// recorded. `ok == false` for both retryable and non-retryable
    /// tool errors.
    fn on_tool_result(&self, _turn: u32, _name: &str, _ok: bool, _duration_ms: u64) {}

    /// Called at the end of each turn. `stats` is cumulative.
    fn on_turn_end(&self, _turn: u32, _stats: &AgentStats) {}

    /// Called exactly once, right before `run()` returns. Not fired on
    /// persistence errors that bubble up as [`AgentError`], since
    /// those abort before we have a well-formed outcome.
    fn on_session_complete(&self, _outcome: &AgentOutcome) {}
}

/// Observer that does nothing. Used when no observer is supplied.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopObserver;

impl AgentObserver for NoopObserver {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Default)]
    struct CountingObserver {
        text_deltas: AtomicU32,
        tool_starts: AtomicU32,
        tool_results: AtomicU32,
        turns: AtomicU32,
    }

    impl AgentObserver for CountingObserver {
        fn on_text_delta(&self, _: u32, _: &str) {
            self.text_deltas.fetch_add(1, Ordering::Relaxed);
        }
        fn on_tool_use_start(&self, _: u32, _: &str, _: &str) {
            self.tool_starts.fetch_add(1, Ordering::Relaxed);
        }
        fn on_tool_result(&self, _: u32, _: &str, _: bool, _: u64) {
            self.tool_results.fetch_add(1, Ordering::Relaxed);
        }
        fn on_turn_end(&self, _: u32, _: &AgentStats) {
            self.turns.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn noop_observer_is_zero_sized() {
        assert_eq!(std::mem::size_of::<NoopObserver>(), 0);
    }

    #[test]
    fn default_impls_are_truly_no_ops() {
        let obs = NoopObserver;
        // Should compile and do nothing — no assertions, just proving
        // the trait can be used without overriding any method.
        obs.on_session_start(&SessionId::new("x"));
        obs.on_turn_start(1);
        obs.on_text_delta(1, "hello");
        obs.on_tool_use_start(1, "t", "id");
        obs.on_tool_result(1, "t", true, 0);
        obs.on_turn_end(1, &AgentStats::default());
    }

    #[test]
    fn counting_observer_aggregates_via_interior_mutability() {
        let obs = CountingObserver::default();
        obs.on_text_delta(1, "a");
        obs.on_text_delta(1, "b");
        obs.on_tool_use_start(1, "read_file", "id-1");
        obs.on_tool_result(1, "read_file", true, 5);
        obs.on_turn_end(1, &AgentStats::default());
        assert_eq!(obs.text_deltas.load(Ordering::Relaxed), 2);
        assert_eq!(obs.tool_starts.load(Ordering::Relaxed), 1);
        assert_eq!(obs.tool_results.load(Ordering::Relaxed), 1);
        assert_eq!(obs.turns.load(Ordering::Relaxed), 1);
    }
}
