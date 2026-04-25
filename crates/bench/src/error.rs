use thiserror::Error;

#[derive(Debug, Error)]
pub enum BenchError {
    #[error("unknown benchmark target: {0}")]
    UnknownTarget(String),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}
