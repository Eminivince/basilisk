//! ETL pipelines for Basilisk's knowledge base.
//!
//! One trait ([`Ingester`]) per external corpus. Each ingester is
//! responsible for: reading from its source, normalising into the
//! common [`IngestRecord`] intermediate shape, chunking oversize
//! content, embedding via the configured provider, and upserting
//! into the correct [`basilisk_vector::VectorStore`] collection.
//!
//! The foundation (CP7.4) ships:
//!  - [`ingester::Ingester`] trait + option/report/progress types
//!  - [`normalize`] — semantic chunker for oversize records with
//!    `parent_id`/`chunk_index`/`total_chunks` linkage
//!  - [`state`] — incremental-ingest state tracked at
//!    `~/.basilisk/knowledge/ingest_state.json` so a resumed run
//!    skips records already persisted
//!
//! Concrete ingesters (Solodit, SWC, OZ advisories, protocol
//! context) arrive in CP7.5–CP7.7.

pub mod error;
pub mod ingester;
pub mod normalize;
pub mod openzeppelin;
pub mod solodit;
pub mod state;
pub mod swc;

pub use error::IngestError;
pub use ingester::{
    IngestOptions, IngestProgress, IngestRecord, IngestReport, Ingester, IngesterKind,
};
pub use normalize::{chunk_record, NormalizedRecord};
pub use openzeppelin::{OzAdvisoriesIngester, OzAdvisoryRow};
pub use solodit::{SoloditFindingRow, SoloditIngester};
pub use state::{default_state_path, IngestState, SourceState};
pub use swc::{SwcEntry, SwcIngester};
