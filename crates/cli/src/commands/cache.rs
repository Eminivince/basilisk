//! `audit cache` — inspect and manage Basilisk's on-disk cache.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use basilisk_cache::{default_base_dir, Cache, NamespaceStats};
use basilisk_git::{default_cache_root as default_repo_cache_root, RepoCache};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommand,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Remove cache entries. Clears a single namespace if `--namespace` is
    /// set, otherwise every known namespace.
    Clear {
        #[arg(long)]
        namespace: Option<String>,
    },
    /// Print one cache entry as raw JSON.
    Show {
        /// Cache namespace (e.g. `bytecode`, `verified_source`).
        namespace: String,
        /// Lookup key (e.g. `ethereum:0x...`).
        key: String,
    },
    /// List every namespace with entry count and total bytes.
    Stats,
    /// Inspect and manage the persistent repo clone cache.
    Repos(RepoCacheArgs),
}

#[derive(Debug, Args)]
pub struct RepoCacheArgs {
    /// Override the cache root (default: `$HOME/.basilisk/repos/`).
    #[arg(long, global = true)]
    pub cache_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: RepoCacheCommand,
}

#[derive(Debug, Subcommand)]
pub enum RepoCacheCommand {
    /// Summary: clone count, total bytes, oldest/newest clone time.
    Stats,
    /// Every cached (owner, repo, sha) entry with metadata.
    List,
    /// Remove cached clones. Scopes to a single owner or repo with
    /// `--owner`/`--repo`; otherwise wipes the whole repo cache.
    Clear {
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
}

pub async fn run(args: &CacheArgs) -> Result<()> {
    match &args.command {
        CacheCommand::Clear { namespace } => clear(namespace.as_deref()).await,
        CacheCommand::Show { namespace, key } => show(namespace, key).await,
        CacheCommand::Stats => stats().await,
        CacheCommand::Repos(args) => run_repos(args),
    }
}

async fn clear(namespace: Option<&str>) -> Result<()> {
    let base = default_base_dir().context("resolving cache base dir")?;
    let namespaces = match namespace {
        Some(n) => vec![n.to_string()],
        None => list_namespaces(&base)?,
    };
    for ns in namespaces {
        match Cache::open_at(&base, &ns) {
            Ok(c) => {
                c.clear()
                    .await
                    .with_context(|| format!("clearing namespace {ns:?}"))?;
                println!("cleared: {ns}");
            }
            Err(e) => {
                println!("skipped {ns}: {e}");
            }
        }
    }
    Ok(())
}

async fn show(namespace: &str, key: &str) -> Result<()> {
    let cache =
        Cache::open(namespace).with_context(|| format!("opening namespace {namespace:?}"))?;
    match cache.get::<serde_json::Value>(key).await {
        Ok(Some(hit)) => {
            println!("{}", serde_json::to_string_pretty(&hit.value)?);
            Ok(())
        }
        Ok(None) => {
            println!("no entry (or expired) for {namespace}:{key}");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

async fn stats() -> Result<()> {
    let base = default_base_dir().context("resolving cache base dir")?;
    let namespaces = list_namespaces(&base)?;
    if namespaces.is_empty() {
        println!("cache is empty: {}", base.display());
        return Ok(());
    }
    let mut rows: Vec<NamespaceStats> = Vec::with_capacity(namespaces.len());
    for ns in &namespaces {
        if let Ok(c) = Cache::open_at(&base, ns) {
            match c.stats().await {
                Ok(s) => rows.push(s),
                Err(e) => eprintln!("failed to stat {ns}: {e}"),
            }
        }
    }
    println!("cache dir: {}", base.display());
    println!("{:<24} {:>10} {:>14}", "namespace", "entries", "bytes");
    println!("{:-<24} {:->10} {:->14}", "", "", "");
    for row in rows {
        println!(
            "{:<24} {:>10} {:>14}",
            row.namespace, row.entries, row.bytes
        );
    }
    Ok(())
}

fn list_namespaces(base: &std::path::Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !base.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(base).with_context(|| format!("reading {}", base.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

// --- `audit cache repos` ------------------------------------------------------

fn open_repo_cache(dir: Option<&Path>) -> Result<RepoCache> {
    let root = match dir {
        Some(p) => p.to_path_buf(),
        None => default_repo_cache_root().context("resolving repo cache root")?,
    };
    RepoCache::open_at(root.clone())
        .with_context(|| format!("opening repo cache at {}", root.display()))
}

fn run_repos(args: &RepoCacheArgs) -> Result<()> {
    let cache = open_repo_cache(args.cache_dir.as_deref())?;
    match &args.command {
        RepoCacheCommand::Stats => repos_stats(&cache),
        RepoCacheCommand::List => repos_list(&cache),
        RepoCacheCommand::Clear { owner, repo } => {
            repos_clear(&cache, owner.as_deref(), repo.as_deref())
        }
    }
}

fn repos_stats(cache: &RepoCache) -> Result<()> {
    let stats = cache.stats().context("reading repo cache stats")?;
    println!("repo cache: {}", cache.root().display());
    if stats.repos_count == 0 {
        println!("(empty)");
        return Ok(());
    }
    println!("repos: {}", stats.repos_count);
    println!("total: {}", format_bytes(stats.total_bytes));
    if let Some(t) = stats.oldest_clone {
        println!("oldest: {}", format_relative(t));
    }
    if let Some(t) = stats.newest_clone {
        println!("newest: {}", format_relative(t));
    }
    Ok(())
}

fn repos_list(cache: &RepoCache) -> Result<()> {
    let entries = cache.list().context("listing repo cache")?;
    if entries.is_empty() {
        println!("repo cache is empty: {}", cache.root().display());
        return Ok(());
    }
    println!(
        "{:<40} {:<10} {:<8} cloned",
        "owner/repo", "sha", "depth"
    );
    println!("{:-<40} {:-<10} {:-<8} {:-<16}", "", "", "", "");
    for (owner, repo, sha, meta) in entries {
        let label = format!("{owner}/{repo}");
        let short = &sha[..sha.len().min(8)];
        let depth = match meta.clone_depth {
            basilisk_git::CloneDepth::Shallow => "shallow",
            basilisk_git::CloneDepth::Full => "full",
        };
        println!(
            "{:<40} {:<10} {:<8} {}",
            label,
            short,
            depth,
            format_relative(meta.cloned_at),
        );
    }
    Ok(())
}

fn repos_clear(cache: &RepoCache, owner: Option<&str>, repo: Option<&str>) -> Result<()> {
    let scope = match (owner, repo) {
        (Some(o), Some(r)) => format!("{o}/{r}"),
        (Some(o), None) => format!("{o}/*"),
        _ => "all".to_string(),
    };
    let freed = if owner.is_some() || repo.is_some() {
        cache
            .clear_scoped(owner, repo)
            .with_context(|| format!("clearing scope {scope}"))?
    } else {
        cache.clear().context("clearing repo cache")?
    };
    println!("cleared {scope}: {} freed", format_bytes(freed));
    Ok(())
}

// --- formatters ---------------------------------------------------------------

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        #[allow(clippy::cast_precision_loss)] // display-only
        {
            format!("{:.1} KB", n as f64 / KB as f64)
        }
    } else if n < GB {
        #[allow(clippy::cast_precision_loss)]
        {
            format!("{:.1} MB", n as f64 / MB as f64)
        }
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            format!("{:.1} GB", n as f64 / GB as f64)
        }
    }
}

fn format_relative(t: SystemTime) -> String {
    match SystemTime::now().duration_since(t) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86_400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86_400)
            }
        }
        Err(_) => "in the future".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn format_bytes_scales_across_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn format_relative_handles_future_timestamps() {
        let far_future = SystemTime::now() + Duration::from_secs(10_000);
        assert_eq!(format_relative(far_future), "in the future");
    }

    #[test]
    fn format_relative_returns_days_for_old_timestamps() {
        let old = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let out = format_relative(old);
        assert!(out.ends_with("d ago"), "got {out:?}");
    }
}
