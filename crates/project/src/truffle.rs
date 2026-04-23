//! `truffle-config.js` parser.
//!
//! Same approach as the Hardhat parser: strip JS comments, run a few
//! regexes over the source. Truffle's config keys are `snake_case`, which
//! distinguishes them from Hardhat's `camelCase` paths and lets us reuse
//! a very similar extraction strategy with different key names.

use std::{
    fs,
    path::{Path, PathBuf},
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{error::ProjectError, js_text, layout::ProjectLayout};

/// Heuristically-extracted Truffle config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruffleConfig {
    pub path: PathBuf,
    /// `contracts_directory` — source tree.
    pub contracts_directory: Option<PathBuf>,
    /// `contracts_build_directory` — artifact output dir.
    pub contracts_build_directory: Option<PathBuf>,
    /// `test_directory`.
    pub test_directory: Option<PathBuf>,
    /// `migrations_directory`.
    pub migrations_directory: Option<PathBuf>,
    /// `compilers.solc.version` — single value in the standard shape.
    pub solc_version: Option<String>,
}

impl TruffleConfig {
    pub fn contracts_or_default(&self) -> PathBuf {
        self.contracts_directory
            .clone()
            .unwrap_or_else(|| PathBuf::from("./contracts"))
    }
    pub fn tests_or_default(&self) -> PathBuf {
        self.test_directory
            .clone()
            .unwrap_or_else(|| PathBuf::from("./test"))
    }
    pub fn migrations_or_default(&self) -> PathBuf {
        self.migrations_directory
            .clone()
            .unwrap_or_else(|| PathBuf::from("./migrations"))
    }
}

/// Load and heuristically parse the Truffle config from `layout`.
/// Returns `Ok(None)` if `layout` doesn't have a truffle-config.js.
pub fn parse_truffle_config(layout: &ProjectLayout) -> Result<Option<TruffleConfig>, ProjectError> {
    let Some(path) = layout.truffle_config() else {
        return Ok(None);
    };
    let source = fs::read_to_string(path).map_err(|e| ProjectError::io(path, e))?;
    Ok(Some(parse_truffle_source(path, &source)))
}

/// Text-only variant used by tests and for in-memory configs.
pub fn parse_truffle_source(path: &Path, source: &str) -> TruffleConfig {
    let cleaned = js_text::strip_comments(source);
    TruffleConfig {
        path: path.to_path_buf(),
        contracts_directory: extract_path_key(&cleaned, "contracts_directory"),
        contracts_build_directory: extract_path_key(&cleaned, "contracts_build_directory"),
        test_directory: extract_path_key(&cleaned, "test_directory"),
        migrations_directory: extract_path_key(&cleaned, "migrations_directory"),
        solc_version: extract_solc_version(&cleaned),
    }
}

fn extract_path_key(src: &str, key: &str) -> Option<PathBuf> {
    let pattern = format!(r#"['"]?{key}['"]?\s*:\s*['"]([^'"\n]+)['"]"#);
    let re = Regex::new(&pattern).ok()?;
    re.captures(src)
        .and_then(|c| c.get(1))
        .map(|m| PathBuf::from(m.as_str()))
}

/// Extract the single `compilers.solc.version` value most Truffle
/// configs declare. We don't try to enumerate multiple compiler versions
/// because Truffle only supports one at a time.
fn extract_solc_version(src: &str) -> Option<String> {
    let re = Regex::new(r#"['"]?version['"]?\s*:\s*['"]([0-9^~<>=\. ]+)['"]"#).ok()?;
    re.captures(src)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> TruffleConfig {
        parse_truffle_source(Path::new("/tmp/truffle-config.js"), s)
    }

    #[test]
    fn typical_truffle_config_extracts_dirs_and_version() {
        let src = r#"
module.exports = {
    contracts_directory: "./contracts",
    contracts_build_directory: "./build/contracts",
    test_directory: "./test",
    migrations_directory: "./migrations",
    compilers: {
        solc: {
            version: "0.8.19",
            settings: { optimizer: { enabled: true, runs: 200 } },
        },
    },
};
"#;
        let cfg = parse(src);
        assert_eq!(cfg.contracts_directory, Some(PathBuf::from("./contracts")),);
        assert_eq!(
            cfg.contracts_build_directory,
            Some(PathBuf::from("./build/contracts")),
        );
        assert_eq!(cfg.test_directory, Some(PathBuf::from("./test")));
        assert_eq!(
            cfg.migrations_directory,
            Some(PathBuf::from("./migrations"))
        );
        assert_eq!(cfg.solc_version.as_deref(), Some("0.8.19"));
    }

    #[test]
    fn missing_dirs_fall_back_to_truffle_defaults() {
        let src = r"module.exports = {};";
        let cfg = parse(src);
        assert!(cfg.contracts_directory.is_none());
        assert_eq!(cfg.contracts_or_default(), PathBuf::from("./contracts"));
        assert_eq!(cfg.tests_or_default(), PathBuf::from("./test"));
        assert_eq!(cfg.migrations_or_default(), PathBuf::from("./migrations"));
    }

    #[test]
    fn commented_keys_are_ignored() {
        let src = r#"
module.exports = {
    // contracts_directory: "./wrong",
    contracts_directory: "./right",
    /* compilers: { solc: { version: "0.5.0" } } */
    compilers: { solc: { version: "0.8.17" } },
};
"#;
        let cfg = parse(src);
        assert_eq!(cfg.contracts_directory, Some(PathBuf::from("./right")));
        assert_eq!(cfg.solc_version.as_deref(), Some("0.8.17"));
    }

    #[test]
    fn caret_version_is_preserved() {
        let src = r#"
module.exports = {
    compilers: { solc: { version: "^0.8.0" } },
};
"#;
        let cfg = parse(src);
        assert_eq!(cfg.solc_version.as_deref(), Some("^0.8.0"));
    }

    #[test]
    fn single_quoted_values_supported() {
        let src = "module.exports = {\n  contracts_directory: './contracts',\n};\n";
        let cfg = parse(src);
        assert_eq!(cfg.contracts_directory, Some(PathBuf::from("./contracts")));
    }

    #[test]
    fn no_match_yields_all_none() {
        let cfg = parse("// just comments\nvar x = 1;\n");
        assert!(cfg.contracts_directory.is_none());
        assert!(cfg.solc_version.is_none());
    }
}
