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

/// Reconnaissance brief (`set-6`) — describes what a recon pass is and
/// what the finalized report must cover. See `src/prompts/recon_v1.md`
/// for the source.
pub const RECON_V1_PROMPT: &str = include_str!("prompts/recon_v1.md");

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
}
