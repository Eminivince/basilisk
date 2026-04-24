//! System prompts shipped with the agent.
//!
//! Prompts are authored as Markdown files under `src/prompts/` and
//! embedded into the binary via [`include_str!`] at compile time. This
//! gives us three properties at once:
//!
//!  - **Build-time existence check.** If the file is missing or
//!    renamed, the build fails immediately (the spec calls this out).
//!  - **Reliable shipping.** Released binaries carry the prompt with
//!    them — operators don't need the source tree.
//!  - **Edit-and-iterate without rebuild.** The CLI's
//!    `--system-prompt <path>` flag lets operators point at a working
//!    copy of the Markdown file, so prompt iteration doesn't require
//!    `cargo build`.
//!
//! We keep older prompt versions in the repo even when they aren't the
//! default, so operators can diff the iteration history and point
//! `--system-prompt` at an earlier version to reproduce an older run.

/// Reconnaissance brief v1 — the original prompt shipped in set-6.
/// Kept for reference / A/B comparison. See `src/prompts/recon_v1.md`.
pub const RECON_V1_PROMPT: &str = include_str!("prompts/recon_v1.md");

/// Reconnaissance brief v2 (set-6.5) — tightens the report style with
/// explicit length ceilings, bullet density limits, and a no-boilerplate
/// rule. This is the current default. See `src/prompts/recon_v2.md`.
pub const RECON_V2_PROMPT: &str = include_str!("prompts/recon_v2.md");

/// The prompt the CLI loads when `--system-prompt` / `BASILISK_SYSTEM_PROMPT`
/// isn't set. Currently points at `v2`. To run against an earlier version,
/// pass `--system-prompt crates/agent/src/prompts/recon_v1.md`.
pub const RECON_DEFAULT_PROMPT: &str = RECON_V2_PROMPT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recon_v1_prompt_is_nonempty_and_mentions_finalize_report() {
        assert!(!RECON_V1_PROMPT.is_empty());
        assert!(
            RECON_V1_PROMPT.contains("finalize_report"),
            "prompt should mention the stop-signal tool",
        );
    }

    #[test]
    fn recon_v1_prompt_mentions_classify_target_as_starting_point() {
        assert!(
            RECON_V1_PROMPT.contains("classify_target"),
            "prompt should steer the agent toward the classifier first",
        );
    }

    #[test]
    fn recon_v2_prompt_is_nonempty_and_mentions_finalize_report() {
        assert!(!RECON_V2_PROMPT.is_empty());
        assert!(RECON_V2_PROMPT.contains("finalize_report"));
    }

    #[test]
    fn recon_v2_prompt_ships_tightening_guidance() {
        // The whole point of v2 is explicit length + density ceilings.
        // If any of these markers vanish we've regressed on the feature.
        assert!(
            RECON_V2_PROMPT.contains("Report style"),
            "v2 should have a 'Report style' section",
        );
        assert!(
            RECON_V2_PROMPT.contains("bullets per section"),
            "v2 should cap bullet density",
        );
        assert!(
            RECON_V2_PROMPT.contains("actionable brief"),
            "v2 should frame the report as actionable, not a checklist",
        );
    }

    #[test]
    fn default_prompt_points_at_v2() {
        assert_eq!(RECON_DEFAULT_PROMPT, RECON_V2_PROMPT);
    }
}
