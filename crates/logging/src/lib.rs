//! Centralized `tracing` setup.
//!
//! The CLI calls [`init`] exactly once at startup. Format (JSON vs. pretty)
//! is selected by the caller; when `None`, we default to pretty on a TTY and
//! JSON otherwise. `RUST_LOG` always wins over `fallback_level`.

use std::io::{self, IsTerminal};

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

impl LogFormat {
    /// Resolve a format: explicit choice wins, otherwise pretty on TTY / JSON off-TTY.
    pub fn resolve(explicit: Option<LogFormat>) -> LogFormat {
        explicit.unwrap_or_else(|| {
            if io::stdout().is_terminal() {
                LogFormat::Pretty
            } else {
                LogFormat::Json
            }
        })
    }
}

/// Initialize the global `tracing` subscriber.
///
/// `fallback_level` is used as the filter directive when `RUST_LOG` is unset
/// or invalid (for example, `"info"`).
///
/// # Errors
/// Returns a string error if a subscriber is already installed.
pub fn init(format: Option<LogFormat>, fallback_level: &str) -> Result<(), String> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback_level));

    let registry = tracing_subscriber::registry().with(env_filter);

    let result = match LogFormat::resolve(format) {
        LogFormat::Pretty => registry.with(fmt::layer().pretty()).try_init(),
        LogFormat::Json => registry
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false),
            )
            .try_init(),
    };

    result.map_err(|e| e.to_string())
}
