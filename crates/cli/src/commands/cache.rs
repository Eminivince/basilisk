//! `audit cache` — inspect and manage Basilisk's on-disk cache.

use anyhow::{Context, Result};
use basilisk_cache::{default_base_dir, Cache, NamespaceStats};
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
}

pub async fn run(args: &CacheArgs) -> Result<()> {
    match &args.command {
        CacheCommand::Clear { namespace } => clear(namespace.as_deref()).await,
        CacheCommand::Show { namespace, key } => show(namespace, key).await,
        CacheCommand::Stats => stats().await,
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
