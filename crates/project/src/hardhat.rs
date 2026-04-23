//! `hardhat.config.{js,ts,cjs,mjs}` parser.
//!
//! Hardhat configs are JavaScript / TypeScript modules, which means a
//! _correct_ parse would require running a JS VM. We don't need that —
//! the auditor only needs `paths.sources`, `paths.tests`, artifacts/
//! cache dirs, and the `solidity` version. A handful of regexes over
//! comment-stripped source gets us there for the ~99% of configs that
//! follow the standard shape.
//!
//! Anything we can't confidently extract is left as `None` / empty, and
//! callers can fall back to Hardhat's built-in defaults via the helper
//! methods on [`HardhatConfig`].

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{error::ProjectError, js_text, layout::ProjectLayout};

/// Source language / module flavour of a Hardhat config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HardhatStyle {
    Js,
    Ts,
    Cjs,
    Mjs,
}

impl HardhatStyle {
    /// Infer the style from the config file path extension.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("ts") => Self::Ts,
            Some("cjs") => Self::Cjs,
            Some("mjs") => Self::Mjs,
            _ => Self::Js,
        }
    }
}

/// Heuristically-extracted Hardhat config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardhatConfig {
    pub path: PathBuf,
    pub style: HardhatStyle,
    /// `paths.sources` — contracts directory. `None` if unset.
    pub sources: Option<PathBuf>,
    /// `paths.tests` — test directory.
    pub tests: Option<PathBuf>,
    /// `paths.artifacts` — build artifact directory.
    pub artifacts: Option<PathBuf>,
    /// `paths.cache` — compilation cache directory.
    pub cache: Option<PathBuf>,
    /// Every `version: "<semver>"` occurrence we saw. One entry for the
    /// common `solidity: "0.8.x"` / `solidity: { version: "0.8.x" }`
    /// shapes, multiple for `compilers: [...]` arrays.
    pub solc_versions: Vec<String>,
}

impl HardhatConfig {
    /// Effective sources dir (config value or Hardhat default `contracts`).
    pub fn sources_or_default(&self) -> PathBuf {
        self.sources
            .clone()
            .unwrap_or_else(|| PathBuf::from("contracts"))
    }
    /// Effective tests dir (config value or default `test`).
    pub fn tests_or_default(&self) -> PathBuf {
        self.tests.clone().unwrap_or_else(|| PathBuf::from("test"))
    }
}

/// Load and heuristically parse the Hardhat config from `layout`.
/// Returns `Ok(None)` if `layout` doesn't have a hardhat config.
pub fn parse_hardhat_config(layout: &ProjectLayout) -> Result<Option<HardhatConfig>, ProjectError> {
    let Some(path) = layout.hardhat_config() else {
        return Ok(None);
    };
    let source = fs::read_to_string(path).map_err(|e| ProjectError::io(path, e))?;
    Ok(Some(parse_hardhat_source(path, &source)))
}

/// Text-only variant used by tests and for in-memory configs.
pub fn parse_hardhat_source(path: &Path, source: &str) -> HardhatConfig {
    let cleaned = js_text::strip_comments(source);
    HardhatConfig {
        path: path.to_path_buf(),
        style: HardhatStyle::from_path(path),
        sources: extract_path_key(&cleaned, "sources"),
        tests: extract_path_key(&cleaned, "tests"),
        artifacts: extract_path_key(&cleaned, "artifacts"),
        cache: extract_path_key(&cleaned, "cache"),
        solc_versions: extract_solc_versions(&cleaned),
    }
}

/// Match `<key>: "<value>"` or `'<value>'`, optionally quoted key,
/// returning the first capture. The bespoke "first match wins" behaviour
/// lines up with how Hardhat users typically structure `paths: { ... }`.
fn extract_path_key(src: &str, key: &str) -> Option<PathBuf> {
    // `(?m)` isn't strictly necessary — we only care about the first match —
    // but keeping the regex compiled once per key via a map would complicate
    // the code without meaningfully helping performance at CP5 scale.
    let pattern = format!(r#"['"]?{key}['"]?\s*:\s*['"]([^'"\n]+)['"]"#);
    let re = Regex::new(&pattern).ok()?;
    re.captures(src)
        .and_then(|c| c.get(1))
        .map(|m| PathBuf::from(m.as_str()))
}

/// Collect every `version: "<semver>"` occurrence, plus the shorthand
/// `solidity: "<semver>"` form. Preserves source order, de-duplicates.
fn extract_solc_versions(src: &str) -> Vec<String> {
    static VERSION_RE: OnceLock<Regex> = OnceLock::new();
    static SHORT_RE: OnceLock<Regex> = OnceLock::new();

    let version_re = VERSION_RE
        .get_or_init(|| Regex::new(r#"['"]?version['"]?\s*:\s*['"]([^'"\n]+)['"]"#).unwrap());
    let short_re =
        SHORT_RE.get_or_init(|| Regex::new(r#"solidity\s*:\s*['"]([^'"\n]+)['"]"#).unwrap());

    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for caps in short_re.captures_iter(src) {
        if let Some(v) = caps.get(1) {
            let s = v.as_str().to_string();
            if seen.insert(s.clone()) {
                out.push(s);
            }
        }
    }
    for caps in version_re.captures_iter(src) {
        if let Some(v) = caps.get(1) {
            let s = v.as_str().to_string();
            if looks_like_semver(&s) && seen.insert(s.clone()) {
                out.push(s);
            }
        }
    }
    out
}

fn looks_like_semver(s: &str) -> bool {
    // "0.8.21", "0.8", "^0.8.0", "~0.8.20", ">=0.8.0 <0.9.0".
    // We only need a cheap filter: does it start with an optional `^`/`~`/`>=`/`<=` then a digit and a dot.
    let stripped = s
        .trim_start_matches(['^', '~', '=', '>', '<', ' '])
        .trim_start();
    let first = stripped.chars().next();
    let second = stripped.chars().nth(1);
    matches!(first, Some(c) if c.is_ascii_digit()) && matches!(second, Some('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> HardhatConfig {
        parse_hardhat_source(Path::new("/tmp/hardhat.config.ts"), s)
    }

    #[test]
    fn style_from_path_recognises_ts_cjs_mjs() {
        assert_eq!(
            HardhatStyle::from_path(Path::new("hardhat.config.ts")),
            HardhatStyle::Ts,
        );
        assert_eq!(
            HardhatStyle::from_path(Path::new("hardhat.config.cjs")),
            HardhatStyle::Cjs,
        );
        assert_eq!(
            HardhatStyle::from_path(Path::new("hardhat.config.mjs")),
            HardhatStyle::Mjs,
        );
        assert_eq!(
            HardhatStyle::from_path(Path::new("hardhat.config.js")),
            HardhatStyle::Js,
        );
    }

    #[test]
    fn typical_ts_config_extracts_paths_and_version() {
        let src = r#"
import { HardhatUserConfig } from "hardhat/config";

const config: HardhatUserConfig = {
    solidity: "0.8.24",
    paths: {
        sources: "./contracts",
        tests: "./test",
        artifacts: "./artifacts",
        cache: "./cache",
    },
};

export default config;
"#;
        let cfg = parse(src);
        assert_eq!(cfg.sources, Some(PathBuf::from("./contracts")));
        assert_eq!(cfg.tests, Some(PathBuf::from("./test")));
        assert_eq!(cfg.artifacts, Some(PathBuf::from("./artifacts")));
        assert_eq!(cfg.cache, Some(PathBuf::from("./cache")));
        assert_eq!(cfg.solc_versions, vec!["0.8.24".to_string()]);
    }

    #[test]
    fn solidity_object_with_version_field() {
        let src = r#"
module.exports = {
    solidity: {
        version: "0.8.20",
        settings: { optimizer: { enabled: true, runs: 200 } },
    },
};
"#;
        let cfg = parse(src);
        assert_eq!(cfg.solc_versions, vec!["0.8.20".to_string()]);
    }

    #[test]
    fn compilers_array_collects_multiple_versions() {
        let src = r#"
module.exports = {
    solidity: {
        compilers: [
            { version: "0.8.20" },
            { version: "0.7.6" },
            { version: "0.6.12" },
        ],
    },
};
"#;
        let cfg = parse(src);
        assert_eq!(
            cfg.solc_versions,
            vec![
                "0.8.20".to_string(),
                "0.7.6".to_string(),
                "0.6.12".to_string()
            ],
        );
    }

    #[test]
    fn commented_out_keys_are_ignored() {
        let src = r#"
module.exports = {
    // solidity: "0.5.0",
    solidity: "0.8.20",
    /* paths: { sources: "./wrong" } */
    paths: { sources: "./contracts" },
};
"#;
        let cfg = parse(src);
        assert_eq!(cfg.solc_versions, vec!["0.8.20".to_string()]);
        assert_eq!(cfg.sources, Some(PathBuf::from("./contracts")));
    }

    #[test]
    fn minimal_config_with_only_solidity() {
        let src = r#"module.exports = { solidity: "0.8.19" };"#;
        let cfg = parse(src);
        assert_eq!(cfg.solc_versions, vec!["0.8.19".to_string()]);
        assert!(cfg.sources.is_none());
    }

    #[test]
    fn config_without_solidity_yields_empty_versions() {
        let src = r#"module.exports = { paths: { sources: "contracts" } };"#;
        let cfg = parse(src);
        assert!(cfg.solc_versions.is_empty());
        assert_eq!(cfg.sources, Some(PathBuf::from("contracts")));
    }

    #[test]
    fn sources_or_default_falls_back_to_contracts() {
        let cfg = parse("module.exports = {};");
        assert_eq!(cfg.sources_or_default(), PathBuf::from("contracts"));
        assert_eq!(cfg.tests_or_default(), PathBuf::from("test"));
    }

    #[test]
    fn semver_filter_rejects_non_version_strings() {
        // `version: "keep this"` shouldn't pollute solc_versions even if a
        // regex accidentally captured it.
        let src = r#"
module.exports = {
    solidity: {
        version: "0.8.20",
        settings: { version: "keep this" },
    },
};
"#;
        let cfg = parse(src);
        assert_eq!(cfg.solc_versions, vec!["0.8.20".to_string()]);
    }

    #[test]
    fn caret_and_tilde_versions_are_accepted() {
        let src = r#"
module.exports = {
    solidity: {
        compilers: [{ version: "^0.8.0" }, { version: "~0.7.6" }],
    },
};
"#;
        let cfg = parse(src);
        assert!(cfg.solc_versions.contains(&"^0.8.0".to_string()));
        assert!(cfg.solc_versions.contains(&"~0.7.6".to_string()));
    }

    #[test]
    fn duplicate_versions_are_de_duplicated() {
        let src = r#"
module.exports = {
    solidity: {
        compilers: [{ version: "0.8.20" }, { version: "0.8.20" }],
    },
};
"#;
        let cfg = parse(src);
        assert_eq!(cfg.solc_versions, vec!["0.8.20".to_string()]);
    }
}
