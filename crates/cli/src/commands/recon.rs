//! `audit recon <target>` — Phase 1 stub.
//!
//! Accepts any string and logs a structured event. Real classification —
//! GitHub URL vs. chain address vs. local path — arrives in the next
//! instruction set, at which point this command will populate a
//! `basilisk_core::Target` instead of emitting `detected_type = "unknown"`.

use anyhow::Result;
use basilisk_core::Config;
use clap::Args;

#[derive(Debug, Args)]
pub struct ReconArgs {
    /// Target to reconnaissance: a URL, address, path, or free-form string.
    pub target: String,
}

#[allow(clippy::unnecessary_wraps)] // Later phases will introduce fallible work here.
pub fn run(args: &ReconArgs, _config: &Config) -> Result<()> {
    tracing::info!(
        target = %args.target,
        detected_type = "unknown",
        "recon: target classification not yet implemented",
    );
    Ok(())
}
