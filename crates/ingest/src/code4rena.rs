//! Code4rena ingester — pulls public contest reports from GitHub.
//!
//! Code4rena ships every contest's findings as a separate
//! `code-423n4/<contest-id>-findings` repository on GitHub. The
//! findings appear in two shapes that this ingester handles:
//!
//!   - **Per-finding markdown** under `data/<auditor-handle>-<severity>.md`,
//!     `data/<contest>-H-<n>.md`, etc.
//!   - **Consolidated `report.md`** at the repo root, with each
//!     finding sectioned under `## [H-NN] ...` / `## [M-NN] ...` /
//!     `## [Q-NN] ...` headers.
//!
//! Both shapes flow into the same [`IngestRecord`] via
//! [`Code4renaFindingRow::into_ingest_record`].
//!
//! Source acquisition uses [`basilisk_git::RepoCache`] so re-ingests
//! reuse already-cached clones. Operators set the contest list
//! either via [`Code4renaIngester::with_contests`] (explicit list)
//! or by relying on the bundled default list (curated, slow-moving
//! — periodic refresh via the operator's manual list update).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use basilisk_core::GitRef;
use basilisk_embeddings::{EmbeddingInput, EmbeddingProvider};
use basilisk_git::{CloneStrategy, FetchOptions, RepoCache};
use basilisk_vector::{schema, VectorStore};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::IngestError,
    ingester::{IngestOptions, IngestProgress, IngestRecord, IngestReport, Ingester},
    normalize::chunk_record,
    state::{IngestState, SourceState},
};

/// One Code4rena finding, normalised across the per-finding-file
/// and consolidated-report shapes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Code4renaFindingRow {
    /// Stable id derived from `(contest, severity, number)` or a
    /// content-hash when the structured id isn't recoverable.
    pub id: String,
    pub contest: String,
    pub title: String,
    pub body: String,
    pub severity: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub auditor: Option<String>,
    #[serde(default)]
    pub finding_url: Option<String>,
}

impl Code4renaFindingRow {
    /// Convert into the source-neutral [`IngestRecord`] shape.
    #[must_use]
    pub fn into_ingest_record(self) -> IngestRecord {
        let mut tags: Vec<String> = vec![format!("severity:{}", self.severity.to_lowercase())];
        if let Some(c) = &self.category {
            tags.push(format!("category:{}", c.to_lowercase()));
        }
        if let Some(a) = &self.auditor {
            tags.push(format!("auditor:{a}"));
        }
        tags.push(format!("contest:{}", self.contest));

        // Pull GitHub blob URLs out of the body before move so the
        // record carries `(file, line)` refs back to the original
        // codebase. The codebase repo is the same contest slug
        // without `-findings`; lazy fetchers use that mapping.
        let code_refs = crate::code_refs::extract_code_refs(&self.body);

        let mut extra = serde_json::Map::new();
        extra.insert("contest".into(), self.contest.into());
        extra.insert("severity".into(), self.severity.into());
        if let Some(c) = self.category {
            extra.insert("category".into(), c.into());
        }
        if let Some(a) = self.auditor {
            extra.insert("auditor".into(), a.into());
        }
        if let Some(u) = self.finding_url {
            extra.insert("finding_url".into(), u.into());
        }
        if !code_refs.is_empty() {
            extra.insert(
                "code_refs".into(),
                serde_json::to_value(&code_refs).unwrap_or(serde_json::Value::Null),
            );
        }

        IngestRecord {
            source_id: self.id,
            source: "code4rena".into(),
            kind: "finding".into(),
            title: self.title,
            body: self.body,
            tags,
            extra: serde_json::Value::Object(extra),
        }
    }
}

/// Bundled list of every public Code4rena contest archive at
/// time of writing — the canonical `gh repo list code-423n4` output
/// filtered to repos ending in `-findings`, with that suffix
/// stripped (the ingester re-appends it). Operators override via
/// [`Code4renaIngester::with_contests`].
///
/// The list is large but cheap: each contest is a fast shallow
/// clone, and the ingester skips contests already seen via the
/// incremental cursor. Re-generate by running
/// `gh repo list code-423n4 --limit 1000 --json name --jq
/// '.[].name | select(endswith("-findings"))' | sed 's/-findings$//' | sort -u`.
pub const DEFAULT_CONTESTS: &[&str] = &[
    "2021-02-slingshot",
    "2021-03-elasticdao",
    "2021-04-basedloans",
    "2021-04-maple",
    "2021-04-marginswap",
    "2021-04-meebits",
    "2021-04-vader",
    "2021-05-88mph",
    "2021-05-fairside",
    "2021-05-nftx",
    "2021-05-visorfinance",
    "2021-05-yield",
    "2021-06-gro",
    "2021-06-pooltogether",
    "2021-06-realitycards",
    "2021-06-tracer",
    "2021-07-connext",
    "2021-07-pooltogether",
    "2021-07-sherlock",
    "2021-07-spartan",
    "2021-07-wildcredit",
    "2021-08-floatcapital",
    "2021-08-gravitybridge",
    "2021-08-notional",
    "2021-08-realitycards",
    "2021-08-yield",
    "2021-09-bvecvx",
    "2021-09-defiprotocol",
    "2021-09-sushimiso",
    "2021-09-sushitrident",
    "2021-09-sushitrident-2",
    "2021-09-swivel",
    "2021-09-wildcredit",
    "2021-09-yaxis",
    "2021-10-ambire",
    "2021-10-badgerdao",
    "2021-10-covalent",
    "2021-10-defiprotocol",
    "2021-10-mochi",
    "2021-10-pooltogether",
    "2021-10-slingshot",
    "2021-10-tally",
    "2021-10-tempus",
    "2021-10-tracer",
    "2021-10-union",
    "2021-11-badgerzaps",
    "2021-11-bootfinance",
    "2021-11-fairside",
    "2021-11-fei",
    "2021-11-malt",
    "2021-11-nested",
    "2021-11-overlay",
    "2021-11-streaming",
    "2021-11-unlock",
    "2021-11-vader",
    "2021-11-yaxis",
    "2021-12-amun",
    "2021-12-defiprotocol",
    "2021-12-maple",
    "2021-12-mellow",
    "2021-12-nftx",
    "2021-12-perennial",
    "2021-12-pooltogether",
    "2021-12-sublime",
    "2021-12-vader",
    "2021-12-yetifinance",
    "2022-01-behodler",
    "2022-01-dev-test-repo",
    "2022-01-elasticswap",
    "2022-01-insure",
    "2022-01-livepeer",
    "2022-01-notional",
    "2022-01-openleverage",
    "2022-01-sandclock",
    "2022-01-sherlock",
    "2022-01-timeswap",
    "2022-01-trader-joe",
    "2022-01-xdefi",
    "2022-01-yield",
    "2022-02-aave-lens",
    "2022-02-anchor",
    "2022-02-badger-citadel",
    "2022-02-concur",
    "2022-02-foundation",
    "2022-02-hubble",
    "2022-02-jpyc",
    "2022-02-nested",
    "2022-02-pooltogether",
    "2022-02-redacted-cartel",
    "2022-02-skale",
    "2022-02-tribe-turbo",
    "2022-03-biconomy",
    "2022-03-joyn",
    "2022-03-lifinance",
    "2022-03-maple",
    "2022-03-paladin",
    "2022-03-prepo",
    "2022-03-rolla",
    "2022-03-sublime",
    "2022-03-timeswap",
    "2022-03-volt",
    "2022-04-abranft",
    "2022-04-axelar",
    "2022-04-backd",
    "2022-04-backed",
    "2022-04-badger-citadel",
    "2022-04-dualityfocus",
    "2022-04-jpegd",
    "2022-04-mimo",
    "2022-04-phuture",
    "2022-04-pooltogether",
    "2022-04-xtribe",
    "2022-05-alchemix",
    "2022-05-aura",
    "2022-05-backd",
    "2022-05-bunker",
    "2022-05-cally",
    "2022-05-cudos",
    "2022-05-factorydao",
    "2022-05-opensea-seaport",
    "2022-05-rubicon",
    "2022-05-runes",
    "2022-05-sturdy",
    "2022-05-velodrome",
    "2022-05-vetoken",
    "2022-06-badger",
    "2022-06-canto",
    "2022-06-canto-v2",
    "2022-06-connext",
    "2022-06-illuminate",
    "2022-06-infinity",
    "2022-06-nested",
    "2022-06-nibbl",
    "2022-06-notional-coop",
    "2022-06-putty",
    "2022-06-yieldy",
    "2022-07-axelar",
    "2022-07-canto",
    "2022-07-ens",
    "2022-07-fractional",
    "2022-07-golom",
    "2022-07-juicebox",
    "2022-07-swivel",
    "2022-07-yield",
    "2022-08-fiatdao",
    "2022-08-foundation",
    "2022-08-frax",
    "2022-08-mimo",
    "2022-08-nounsdao",
    "2022-08-olympus",
    "2022-08-prepo",
    "2022-08-rigor",
    "2022-09-artgobblers",
    "2022-09-canto",
    "2022-09-frax",
    "2022-09-nouns-builder",
    "2022-09-party",
    "2022-09-quickswap",
    "2022-09-tribe",
    "2022-09-vtvl",
    "2022-09-y2k-finance",
    "2022-10-blur",
    "2022-10-holograph",
    "2022-10-inverse",
    "2022-10-juicebox",
    "2022-10-paladin",
    "2022-10-thegraph",
    "2022-10-traderjoe",
    "2022-10-zksync",
    "2022-11-canto",
    "2022-11-debtdao",
    "2022-11-ens",
    "2022-11-foundation",
    "2022-11-looksrare",
    "2022-11-non-fungible",
    "2022-11-paraspace",
    "2022-11-redactedcartel",
    "2022-11-size",
    "2022-11-stakehouse",
    "2022-12-Stealth-Project",
    "2022-12-backed",
    "2022-12-caviar",
    "2022-12-ens-mitigation",
    "2022-12-escher",
    "2022-12-forgeries",
    "2022-12-forgotten-runiverse",
    "2022-12-gogopool",
    "2022-12-pooltogether",
    "2022-12-prepo",
    "2022-12-tessera",
    "2022-12-tigris",
    "2023-01-astaria",
    "2023-01-biconomy",
    "2023-01-canto-identity",
    "2023-01-drips",
    "2023-01-numoen",
    "2023-01-ondo",
    "2023-01-opensea",
    "2023-01-popcorn",
    "2023-01-rabbithole",
    "2023-01-reserve",
    "2023-01-tessera-mitigation",
    "2023-01-timeswap",
    "2023-02-ethos",
    "2023-02-gogopool-mitigation-contest",
    "2023-02-kuma",
    "2023-02-malt",
    "2023-02-reserve-mitigation-contest",
    "2023-03-aragon",
    "2023-03-asymmetry",
    "2023-03-canto-identity",
    "2023-03-kuma-mitigation-contest",
    "2023-03-mute",
    "2023-03-neotokyo",
    "2023-03-polynomial",
    "2023-03-wenwin",
    "2023-03-zksync",
    "2023-04-caviar",
    "2023-04-eigenlayer",
    "2023-04-ens",
    "2023-04-frankencoin",
    "2023-04-party",
    "2023-04-rubicon",
    "2023-05-ajna",
    "2023-05-ambire",
    "2023-05-asymmetry-mitigation",
    "2023-05-base",
    "2023-05-caviar-mitigation-contest",
    "2023-05-juicebox",
    "2023-05-maia",
    "2023-05-particle",
    "2023-05-party",
    "2023-05-venus",
    "2023-05-xeth",
    "2023-06-ambire-mitigation",
    "2023-06-angle",
    "2023-06-canto",
    "2023-06-llama",
    "2023-06-lukso",
    "2023-06-lybra",
    "2023-06-reserve",
    "2023-06-stader",
    "2023-06-xeth-mitigation",
    "2023-07-amphora",
    "2023-07-angle-mitigation",
    "2023-07-arcade",
    "2023-07-axelar",
    "2023-07-basin",
    "2023-07-lens",
    "2023-07-moonwell",
    "2023-07-nounsdao",
    "2023-07-pooltogether",
    "2023-07-reserve",
    "2023-07-tapioca",
    "2023-08-arbitrum",
    "2023-08-dopex",
    "2023-08-goodentry",
    "2023-08-livepeer",
    "2023-08-pooltogether",
    "2023-08-pooltogether-mitigation",
    "2023-08-reserve-mitigation",
    "2023-08-shell",
    "2023-08-verwa",
    "2023-09-asymmetry",
    "2023-09-centrifuge",
    "2023-09-delegate",
    "2023-09-goodentry-mitigation",
    "2023-09-maia",
    "2023-09-ondo",
    "2023-09-reserve-mitigation",
    "2023-09-venus",
    "2023-10-asymmetry-mitigation",
    "2023-10-badger",
    "2023-10-brahma",
    "2023-10-canto",
    "2023-10-ens",
    "2023-10-ethena",
    "2023-10-nextgen",
    "2023-10-opendollar",
    "2023-10-party",
    "2023-10-wildcat",
    "2023-10-zksync",
    "2023-11-betafinance",
    "2023-11-canto",
    "2023-11-kelp",
    "2023-11-panoptic",
    "2023-11-shellprotocol",
    "2023-11-zetachain",
    "2023-12-autonolas",
    "2023-12-ethereumcreditguild",
    "2023-12-initcapital",
    "2023-12-particle",
    "2023-12-revolutionprotocol",
    "2024-01-canto",
    "2024-01-curves",
    "2024-01-decent",
    "2024-01-init-capital-invitational",
    "2024-01-opus",
    "2024-01-renft",
    "2024-01-salty",
    "2024-02-ai-arena",
    "2024-02-althea-liquid-infrastructure",
    "2024-02-hydradx",
    "2024-02-renft-mitigation",
    "2024-02-spectra",
    "2024-02-tapioca",
    "2024-02-thruster",
    "2024-02-uniswap-foundation",
    "2024-02-wise-lending",
    "2024-03-abracadabra-money",
    "2024-03-acala",
    "2024-03-coinbase",
    "2024-03-dittoeth",
    "2024-03-gitcoin",
    "2024-03-gitcoin-mitigation",
    "2024-03-neobase",
    "2024-03-ondo-finance",
    "2024-03-phala-network",
    "2024-03-pooltogether",
    "2024-03-revert-lend",
    "2024-03-saltyio-mitigation",
    "2024-03-taiko",
    "2024-03-zksync",
    "2024-04-ai-arena-mitigation",
    "2024-04-coinbase-mitigation",
    "2024-04-dyad",
    "2024-04-gondi",
    "2024-04-lavarage",
    "2024-04-noya",
    "2024-04-panoptic",
    "2024-04-renzo",
    "2024-04-revert-mitigation",
    "2024-05-arbitrum-foundation",
    "2024-05-bakerfi",
    "2024-05-canto",
    "2024-05-gondi-mitigation",
    "2024-05-loop",
    "2024-05-munchables",
    "2024-05-olas",
    "2024-05-predy",
    "2024-06-badger",
    "2024-06-krystal-defi",
    "2024-06-panoptic",
    "2024-06-renzo-mitigation",
    "2024-06-size",
    "2024-06-thorchain",
    "2024-06-vultisig",
    "2024-07-basin",
    "2024-07-benddao",
    "2024-07-dittoeth",
    "2024-07-karak",
    "2024-07-loopfi",
    "2024-07-munchables",
    "2024-07-optimism",
    "2024-07-reserve",
    "2024-07-traitforge",
    "2024-08-axelar-network",
    "2024-08-basin",
    "2024-08-chakra",
    "2024-08-phi",
    "2024-08-superposition",
    "2024-08-wildcat",
    "2024-09-fenix-finance",
    "2024-09-kakarot",
    "2024-09-karak-mitigation",
    "2024-09-panoptic",
    "2024-09-reserve-mitigation",
    "2024-10-coded-estate",
    "2024-10-kleidi",
    "2024-10-loopfi",
    "2024-10-ramses-exchange",
    "2024-10-ronin",
    "2024-10-superposition",
    "2024-11-ethena-labs",
    "2024-11-nibiru",
    "2024-12-unruggable",
];



/// Pulls public Code4rena contest findings into the
/// `public_findings` collection.
#[derive(Clone)]
pub struct Code4renaIngester {
    contests: Vec<String>,
    /// Optional override for the cache root (defaults to the global
    /// `RepoCache` location). Set by tests.
    cache_root: Option<PathBuf>,
}

impl Code4renaIngester {
    #[must_use]
    pub fn new() -> Self {
        Self {
            contests: DEFAULT_CONTESTS.iter().map(|s| (*s).to_string()).collect(),
            cache_root: None,
        }
    }

    /// Override the contest list. Each entry is the slug before
    /// `-findings` (e.g. `2024-04-renzo`).
    #[must_use]
    pub fn with_contests(mut self, contests: Vec<String>) -> Self {
        self.contests = contests;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_cache_root(mut self, root: PathBuf) -> Self {
        self.cache_root = Some(root);
        self
    }
}

impl Default for Code4renaIngester {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Ingester for Code4renaIngester {
    #[allow(clippy::unnecessary_literal_bound)]
    fn source_name(&self) -> &str {
        "code4rena"
    }

    fn target_collection(&self) -> &str {
        schema::PUBLIC_FINDINGS
    }

    #[allow(clippy::too_many_lines)]
    async fn ingest(
        &self,
        vector_store: Arc<dyn VectorStore>,
        embeddings: Arc<dyn EmbeddingProvider>,
        options: IngestOptions,
    ) -> Result<IngestReport, IngestError> {
        let start = Instant::now();
        let mut report = IngestReport::empty(self.source_name());

        let state_file = crate::state::default_state_path();
        let mut persistent = IngestState::load(&state_file)?;
        let prior = persistent.get(self.source_name());

        let cache = if let Some(root) = &self.cache_root {
            RepoCache::open_at(root.clone()).map_err(|e| IngestError::Other(e.to_string()))?
        } else {
            RepoCache::open().map_err(|e| IngestError::Other(e.to_string()))?
        };

        let max = options.max_records.unwrap_or(usize::MAX);
        let spec = schema::public_findings(embeddings.identifier(), embeddings.dimensions());
        vector_store.create_collection(spec).await?;

        let mut all_rows: Vec<Code4renaFindingRow> = Vec::new();
        let max_contests_to_visit = if options.incremental {
            // Incremental: visit contests not yet seen plus the
            // newest one (in case it changed). Operator-supplied
            // contest list is short enough to scan fully cheaply.
            self.contests.len()
        } else {
            self.contests.len()
        };
        let total_contests = max_contests_to_visit.min(self.contests.len());
        for (i, contest_slug) in self
            .contests
            .iter()
            .take(max_contests_to_visit)
            .enumerate()
        {
            if all_rows.len() >= max {
                break;
            }
            // Per-contest progress line so cache-hit runs (where
            // basilisk_git stays silent) still show the loop is alive.
            // Goes to the same progress callback the embedding phase
            // uses, plus a tracing line for log files.
            tracing::info!(
                contest = %contest_slug,
                index = i + 1,
                total = total_contests,
                rows_so_far = all_rows.len(),
                "scanning contest",
            );
            if let Some(cb) = &options.progress {
                cb(IngestProgress {
                    records_scanned: all_rows.len(),
                    records_upserted: 0,
                    records_skipped: 0,
                    embedding_tokens_used: 0,
                });
            }
            let repo_name = format!("{contest_slug}-findings");
            let opts = FetchOptions {
                strategy: CloneStrategy::Shallow,
                force_refresh: false,
                github: None,
            };
            // Try `main` first, fall back to `master`. Both
            // appear across the contest-archive history.
            let fetched = match cache
                .fetch(
                    "code-423n4",
                    &repo_name,
                    Some(GitRef::Branch("main".into())),
                    opts.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(basilisk_git::GitError::RefNotFound { .. }) => {
                    match cache
                        .fetch(
                            "code-423n4",
                            &repo_name,
                            Some(GitRef::Branch("master".into())),
                            opts,
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            report.errors.push((repo_name, e.to_string()));
                            continue;
                        }
                    }
                }
                Err(e) => {
                    report.errors.push((repo_name, e.to_string()));
                    continue;
                }
            };
            let rows = parse_contest_repo(&fetched.working_tree, contest_slug);
            report.records_scanned += rows.len();
            for r in rows {
                if all_rows.len() >= max {
                    break;
                }
                all_rows.push(r);
            }
        }

        // Early progress tick so operators see scanning finished
        // before embedding starts.
        if let Some(cb) = &options.progress {
            cb(IngestProgress {
                records_scanned: report.records_scanned,
                records_upserted: 0,
                records_skipped: 0,
                embedding_tokens_used: 0,
            });
        }

        // Cursor: largest source_id seen. Code4rena ids are
        // structured (`<contest>:<severity>:<num>`) — lex sort is
        // stable enough for "have we seen this one."
        let mut latest_id = prior.cursor.clone();
        // Batches are packed by both count and total tokens so we
        // never blow past the provider's per-batch token cap (Voyage
        // 120k; OpenAI 300k). See `crate::batch::pack_batches`.
        let mut flat = Vec::new();

        for row in all_rows {
            let id = row.id.clone();
            if options.incremental && prior.cursor.as_deref().is_some_and(|c| id.as_str() <= c) {
                report.records_skipped += 1;
                continue;
            }
            if latest_id.as_deref().is_none_or(|c| id.as_str() > c) {
                latest_id = Some(id);
            }
            let ir = row.into_ingest_record();
            let chunks = chunk_record(&ir, embeddings.max_tokens_per_input());
            flat.extend(chunks);
        }

        for batch in crate::batch::pack_batches(&flat, &*embeddings) {
            if batch.is_empty() {
                continue;
            }
            let inputs: Vec<EmbeddingInput> = batch
                .iter()
                .map(|c| EmbeddingInput::document(&c.text))
                .collect();
            let vectors = embeddings.embed(&inputs).await?;
            report.embedding_tokens_used += vectors
                .iter()
                .map(|v| u64::from(v.input_tokens))
                .sum::<u64>();
            let records: Vec<_> = batch
                .iter()
                .cloned()
                .zip(vectors)
                .map(|(n, v)| n.into_record(v.vector))
                .collect();
            let stats = vector_store
                .upsert(self.target_collection(), records)
                .await?;
            report.records_new += stats.inserted;
            report.records_updated += stats.updated;

            if let Some(cb) = &options.progress {
                cb(IngestProgress {
                    records_scanned: report.records_scanned,
                    records_upserted: report.records_new + report.records_updated,
                    records_skipped: report.records_skipped,
                    embedding_tokens_used: report.embedding_tokens_used,
                });
            }
        }

        persistent.set(
            self.source_name(),
            SourceState {
                cursor: latest_id,
                records_ingested: prior.records_ingested
                    + u64::try_from(report.records_new + report.records_updated)
                        .unwrap_or(u64::MAX),
                last_run_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .ok(),
            },
        );
        persistent.save(&state_file)?;
        report.duration = start.elapsed();
        Ok(report)
    }
}

/// Walk a cloned `code-423n4/<contest>-findings` repo and pull every
/// finding it contains, regardless of which of the two shapes is used.
fn parse_contest_repo(repo_root: &Path, contest_slug: &str) -> Vec<Code4renaFindingRow> {
    let mut out = Vec::new();
    // Shape A: per-finding markdown under `data/`.
    let data_dir = repo_root.join("data");
    if data_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&data_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                if let Ok(body) = std::fs::read_to_string(&path) {
                    if let Some(row) = parse_per_file_finding(&path, contest_slug, &body) {
                        out.push(row);
                    }
                }
            }
        }
    }
    // Shape B: consolidated `report.md` at root.
    let report = repo_root.join("report.md");
    if report.is_file() {
        if let Ok(body) = std::fs::read_to_string(&report) {
            out.extend(parse_consolidated_report(contest_slug, &body));
        }
    }
    out
}

/// Per-file shape: filename like `BBB-H-001.md` (auditor handle +
/// severity + number) or `c4-2023-04-ondo-H-01.md`. The first
/// `# ` heading inside is the title.
fn parse_per_file_finding(
    path: &Path,
    contest_slug: &str,
    body: &str,
) -> Option<Code4renaFindingRow> {
    let stem = path.file_stem()?.to_str()?;
    let (auditor, severity, num) = parse_filename(stem);
    let title = first_h1(body).unwrap_or_else(|| stem.to_string());
    let id = if let Some(num) = num {
        format!("{contest_slug}:{severity}:{num}")
    } else {
        content_hash_id(contest_slug, body)
    };
    Some(Code4renaFindingRow {
        id,
        contest: contest_slug.into(),
        title,
        body: body.to_string(),
        severity: severity.to_string(),
        category: None,
        auditor,
        finding_url: Some(format!(
            "https://github.com/code-423n4/{contest_slug}-findings/blob/main/data/{stem}.md"
        )),
    })
}

/// Consolidated report.md shape: each finding is a level-2 heading
/// like `## [H-01] Reentrancy in withdraw()`. Walk headings, pair
/// each with its body up to the next heading.
fn parse_consolidated_report(contest_slug: &str, body: &str) -> Vec<Code4renaFindingRow> {
    let header_re = Regex::new(r"(?m)^##\s+\[([HMQGI])-(\d+)\]\s+(.+?)\s*$").expect("regex");
    let mut out = Vec::new();
    let mut last: Option<(String, String, String, usize)> = None; // (severity, num, title, body_start)
    for m in header_re.captures_iter(body) {
        let sev = m.get(1).map_or("", |g| g.as_str()).to_string();
        let num = m.get(2).map_or("", |g| g.as_str()).to_string();
        let title = m.get(3).map_or("", |g| g.as_str()).to_string();
        let body_start = m.get(0).map_or(0, |g| g.end());

        if let Some((prev_sev, prev_num, prev_title, prev_start)) = last.take() {
            let header_pos = m.get(0).map_or(body.len(), |g| g.start());
            let prev_body = body
                .get(prev_start..header_pos)
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(make_consolidated_row(
                contest_slug,
                &prev_sev,
                &prev_num,
                &prev_title,
                &prev_body,
            ));
        }
        last = Some((sev, num, title, body_start));
    }
    if let Some((sev, num, title, start)) = last {
        let body_text = body.get(start..).unwrap_or("").trim().to_string();
        out.push(make_consolidated_row(
            contest_slug,
            &sev,
            &num,
            &title,
            &body_text,
        ));
    }
    out
}

fn make_consolidated_row(
    contest_slug: &str,
    severity_letter: &str,
    num: &str,
    title: &str,
    body: &str,
) -> Code4renaFindingRow {
    let severity = match severity_letter {
        "H" => "high",
        "M" => "medium",
        "Q" | "G" => "low",
        _ => "info",
    }
    .to_string();
    Code4renaFindingRow {
        id: format!("{contest_slug}:{severity_letter}:{num}"),
        contest: contest_slug.into(),
        title: title.into(),
        body: body.into(),
        severity,
        category: None,
        auditor: None,
        finding_url: Some(format!(
            "https://github.com/code-423n4/{contest_slug}-findings/blob/main/report.md"
        )),
    }
}

/// Extract `(auditor_handle, severity_word, number_string)` from a
/// per-finding filename stem like `BBB-H-001` or `0xfoo-M-12`.
/// Severity letter mapping: H→high, M→medium, L→low, Q/G→low,
/// I→info. When the filename doesn't match, severity defaults to
/// "info" and auditor/number become None.
fn parse_filename(stem: &str) -> (Option<String>, &'static str, Option<String>) {
    let re = Regex::new(r"^(.*?)-([HMLQGI])-(\d+)$").expect("regex");
    if let Some(caps) = re.captures(stem) {
        let auditor = caps
            .get(1)
            .map(|g| g.as_str().to_string())
            .filter(|s| !s.is_empty());
        let sev = match caps.get(2).map_or("", |g| g.as_str()) {
            "H" => "high",
            "M" => "medium",
            "L" | "Q" | "G" => "low",
            _ => "info",
        };
        let num = caps.get(3).map(|g| g.as_str().to_string());
        (auditor, sev, num)
    } else {
        (None, "info", None)
    }
}

fn first_h1(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return Some(rest.to_string());
        }
    }
    None
}

fn content_hash_id(contest_slug: &str, body: &str) -> String {
    let mut h = Sha256::new();
    h.update(contest_slug.as_bytes());
    h.update(b"\0");
    h.update(body.as_bytes());
    let digest = h.finalize();
    format!("{contest_slug}:hash:{}", hex::encode(&digest[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_filename_handles_per_finding_shape() {
        let (a, s, n) = parse_filename("BBB-H-001");
        assert_eq!(a.as_deref(), Some("BBB"));
        assert_eq!(s, "high");
        assert_eq!(n.as_deref(), Some("001"));
    }

    #[test]
    fn parse_filename_handles_dashed_handle() {
        let (a, s, n) = parse_filename("0xfoo-bar-M-12");
        assert_eq!(a.as_deref(), Some("0xfoo-bar"));
        assert_eq!(s, "medium");
        assert_eq!(n.as_deref(), Some("12"));
    }

    #[test]
    fn parse_filename_falls_back_when_no_match() {
        let (a, s, n) = parse_filename("README");
        assert!(a.is_none());
        assert_eq!(s, "info");
        assert!(n.is_none());
    }

    #[test]
    fn first_h1_extracts_title() {
        let body = "# My finding title\n\nSome body.";
        assert_eq!(first_h1(body).as_deref(), Some("My finding title"));
    }

    #[test]
    fn first_h1_skips_h2() {
        let body = "## not me\n# title\n";
        assert_eq!(first_h1(body).as_deref(), Some("title"));
    }

    #[test]
    fn parse_consolidated_report_pulls_findings() {
        let body = "# Contest report

## [H-01] First high finding

This is the first finding.

It has multiple paragraphs.

## [M-02] Second medium finding

Medium severity body.

## [Q-03] Quality issue

Low quality issue.
";
        let rows = parse_consolidated_report("2024-04-test", body);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].severity, "high");
        assert_eq!(rows[0].title, "First high finding");
        assert!(rows[0].body.contains("multiple paragraphs"));
        assert_eq!(rows[1].severity, "medium");
        assert_eq!(rows[2].severity, "low");
        assert_eq!(rows[0].id, "2024-04-test:H:01");
    }

    #[test]
    fn parse_per_file_finding_extracts_metadata() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("BBB-H-007.md");
        fs::write(
            &path,
            "# Reentrancy in withdraw\n\nThe withdraw function calls...",
        )
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        let row = parse_per_file_finding(&path, "2024-04-test", &body).unwrap();
        assert_eq!(row.title, "Reentrancy in withdraw");
        assert_eq!(row.severity, "high");
        assert_eq!(row.auditor.as_deref(), Some("BBB"));
        assert_eq!(row.id, "2024-04-test:high:007");
    }

    #[test]
    fn parse_contest_repo_handles_both_shapes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // Shape A: per-finding files in data/
        let data = root.join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("alice-H-001.md"), "# A bug\n\nbody one").unwrap();
        fs::write(data.join("bob-M-002.md"), "# Another bug\n\nbody two").unwrap();
        // Shape B: report.md
        fs::write(
            root.join("report.md"),
            "# Report\n\n## [H-99] Consolidated\n\nbody three\n",
        )
        .unwrap();

        let rows = parse_contest_repo(root, "2024-test");
        assert_eq!(rows.len(), 3);
        let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
        assert!(titles.contains(&"A bug"));
        assert!(titles.contains(&"Another bug"));
        assert!(titles.contains(&"Consolidated"));
    }

    #[test]
    fn into_ingest_record_carries_metadata() {
        let row = Code4renaFindingRow {
            id: "2024-04-test:high:001".into(),
            contest: "2024-04-test".into(),
            title: "Reentrancy".into(),
            body: "body".into(),
            severity: "high".into(),
            category: Some("reentrancy".into()),
            auditor: Some("alice".into()),
            finding_url: Some("https://example".into()),
        };
        let ir = row.into_ingest_record();
        assert_eq!(ir.source, "code4rena");
        assert_eq!(ir.kind, "finding");
        assert!(ir.tags.contains(&"severity:high".to_string()));
        assert!(ir.tags.contains(&"category:reentrancy".to_string()));
        assert!(ir.tags.contains(&"auditor:alice".to_string()));
        assert!(ir.tags.contains(&"contest:2024-04-test".to_string()));
        assert_eq!(ir.extra["contest"], "2024-04-test");
    }

    #[test]
    fn into_ingest_record_extracts_code_refs_from_body() {
        let body = "
            Bug at [PositionManager.sol#L262-L323](https://github.com/code-423n4/2023-05-ajna/blob/276942bc/ajna-core/src/PositionManager.sol#L262-L323).
        ";
        let row = Code4renaFindingRow {
            id: "2023-05-ajna:high:001".into(),
            contest: "2023-05-ajna".into(),
            title: "T".into(),
            body: body.into(),
            severity: "high".into(),
            category: None,
            auditor: None,
            finding_url: None,
        };
        let ir = row.into_ingest_record();
        let refs = ir
            .extra
            .get("code_refs")
            .expect("code_refs missing")
            .as_array()
            .expect("code_refs not an array");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["repo"], "2023-05-ajna");
        assert_eq!(refs[0]["path"], "ajna-core/src/PositionManager.sol");
        assert_eq!(refs[0]["line_start"], 262);
        assert_eq!(refs[0]["line_end"], 323);
    }

    #[test]
    fn into_ingest_record_omits_code_refs_when_body_has_none() {
        let row = Code4renaFindingRow {
            id: "x:y:z".into(),
            contest: "x".into(),
            title: "T".into(),
            body: "no urls here".into(),
            severity: "low".into(),
            category: None,
            auditor: None,
            finding_url: None,
        };
        let ir = row.into_ingest_record();
        assert!(ir.extra.get("code_refs").is_none());
    }

    #[test]
    fn empty_repo_yields_zero_rows() {
        let dir = TempDir::new().unwrap();
        let rows = parse_contest_repo(dir.path(), "test");
        assert!(rows.is_empty());
    }

    #[test]
    fn ingester_source_name_and_target_collection_are_stable() {
        let i = Code4renaIngester::new();
        assert_eq!(i.source_name(), "code4rena");
        assert_eq!(i.target_collection(), schema::PUBLIC_FINDINGS);
    }

    #[test]
    fn default_contests_list_is_non_empty() {
        assert!(!DEFAULT_CONTESTS.is_empty());
        for slug in DEFAULT_CONTESTS {
            // sanity: each slug looks like YYYY-MM-name
            assert!(
                slug.starts_with("20"),
                "contest slug {slug} doesn't start with year"
            );
        }
    }

    #[test]
    fn content_hash_id_is_stable_for_same_input() {
        let a = content_hash_id("contest", "body");
        let b = content_hash_id("contest", "body");
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_id_differs_with_body() {
        let a = content_hash_id("contest", "body1");
        let b = content_hash_id("contest", "body2");
        assert_ne!(a, b);
    }
}
