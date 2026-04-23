//! Unified project-config surface.
//!
//! [`ProjectConfig`] is the single entry point consumers should build
//! against. It wraps a [`ProjectLayout`] with the optional
//! flavour-specific parses (Foundry / Hardhat / Truffle), each one
//! populated only when the layout advertised the corresponding config
//! file. Helper methods return effective paths and remappings without
//! the caller having to match on each flavour individually.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    error::ProjectError,
    foundry::{self, FoundryConfig, Remapping, DEFAULT_PROFILE},
    hardhat::{self, HardhatConfig},
    layout::ProjectLayout,
    truffle::{self, TruffleConfig},
};

/// The combined view over a project: its [`ProjectLayout`] plus whatever
/// config parses the layout supported.
///
/// At most one of `foundry`, `hardhat`, `truffle` is set for a clean
/// single-flavour project; a monorepo with multiple root configs will
/// populate more than one. `layout.kind` — a `basilisk_core::ProjectKind`
/// — is the authoritative classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub layout: ProjectLayout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foundry: Option<FoundryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardhat: Option<HardhatConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truffle: Option<TruffleConfig>,
}

impl ProjectConfig {
    /// `true` when the project has no recognized config file.
    /// [`ProjectLayout::is_empty`] remains the better "nothing to audit"
    /// signal; this one just reports whether any parser fired.
    pub fn is_empty(&self) -> bool {
        self.foundry.is_none() && self.hardhat.is_none() && self.truffle.is_none()
    }

    /// Absolute source directories declared by the parsed configs,
    /// falling back to the conventional directories discovered on disk
    /// when a config file is silent on paths.
    pub fn source_dirs(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let root = &self.layout.root;

        if let Some(cfg) = &self.foundry {
            let profile = cfg.profile(DEFAULT_PROFILE).cloned().unwrap_or_default();
            push_unique(&mut out, &mut seen, join(root, &profile.src_dir()));
        }
        if let Some(cfg) = &self.hardhat {
            push_unique(&mut out, &mut seen, join(root, &cfg.sources_or_default()));
        }
        if let Some(cfg) = &self.truffle {
            push_unique(&mut out, &mut seen, join(root, &cfg.contracts_or_default()));
        }

        // If no config fired, fall back to the layout's discovered
        // convention directories so the caller still gets something to work with.
        if out.is_empty() {
            for p in &self.layout.source_dirs {
                push_unique(&mut out, &mut seen, p.clone());
            }
        }
        out
    }

    /// Absolute test directories, same logic as [`Self::source_dirs`].
    pub fn test_dirs(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let root = &self.layout.root;

        if let Some(cfg) = &self.foundry {
            let profile = cfg.profile(DEFAULT_PROFILE).cloned().unwrap_or_default();
            push_unique(&mut out, &mut seen, join(root, &profile.test_dir()));
        }
        if let Some(cfg) = &self.hardhat {
            push_unique(&mut out, &mut seen, join(root, &cfg.tests_or_default()));
        }
        if let Some(cfg) = &self.truffle {
            push_unique(&mut out, &mut seen, join(root, &cfg.tests_or_default()));
        }
        if out.is_empty() {
            for p in &self.layout.test_dirs {
                push_unique(&mut out, &mut seen, p.clone());
            }
        }
        out
    }

    /// Effective remappings across every parsed config. For Foundry this
    /// is [`FoundryConfig::effective_remappings`] (inline + external).
    /// Hardhat and Truffle don't have first-class remappings, so they
    /// contribute nothing here — callers that want Hardhat-style paths
    /// should rely on `source_dirs()` instead.
    pub fn remappings(&self) -> Vec<Remapping> {
        let mut out: Vec<Remapping> = Vec::new();
        let mut seen: HashSet<(Option<String>, String)> = HashSet::new();
        if let Some(cfg) = &self.foundry {
            for r in cfg.effective_remappings() {
                let key = (r.context.clone(), r.prefix.clone());
                if seen.insert(key) {
                    out.push(r);
                }
            }
        }
        out
    }

    /// Every solc version string we were able to extract, across every
    /// flavour. Preserves insertion order and de-duplicates.
    pub fn solc_versions(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(cfg) = &self.foundry {
            if let Some(v) = cfg.profile(DEFAULT_PROFILE).and_then(|p| p.solc.clone()) {
                if seen.insert(v.clone()) {
                    out.push(v);
                }
            }
        }
        if let Some(cfg) = &self.hardhat {
            for v in &cfg.solc_versions {
                if seen.insert(v.clone()) {
                    out.push(v.clone());
                }
            }
        }
        if let Some(cfg) = &self.truffle {
            if let Some(v) = &cfg.solc_version {
                if seen.insert(v.clone()) {
                    out.push(v.clone());
                }
            }
        }
        out
    }
}

/// Load every applicable config for `layout` and return a unified
/// [`ProjectConfig`]. Failures from individual parsers propagate; a
/// missing config file for a given flavour simply leaves that field as
/// `None`. `remappings.txt` at the layout root, if present, is read and
/// merged into `FoundryConfig::external_remappings`.
pub fn load_project_config(layout: ProjectLayout) -> Result<ProjectConfig, ProjectError> {
    let mut foundry = foundry::parse_foundry_config(&layout)?;
    if let (Some(cfg), Some(path)) = (foundry.as_mut(), layout.remappings_file.as_deref()) {
        cfg.external_remappings = foundry::parse_remappings_txt(path)?;
    }
    let hardhat = hardhat::parse_hardhat_config(&layout)?;
    let truffle = truffle::parse_truffle_config(&layout)?;

    Ok(ProjectConfig {
        layout,
        foundry,
        hardhat,
        truffle,
    })
}

fn join(root: &Path, rel: &Path) -> PathBuf {
    if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        // `./contracts` → `contracts` to avoid an ugly `/.` in the path.
        let normalized = rel
            .strip_prefix(".")
            .map_or_else(|_| rel.to_path_buf(), Path::to_path_buf);
        root.join(normalized)
    }
}

fn push_unique(out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if seen.insert(path.clone()) {
        out.push(path);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::layout::detect_layout;

    fn write(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
    }

    fn foundry_project(tmp: &Path, toml: &str) {
        write(&tmp.join("foundry.toml"), toml);
        fs::create_dir_all(tmp.join("src")).unwrap();
    }

    fn hardhat_project(tmp: &Path, src: &str) {
        write(&tmp.join("hardhat.config.ts"), src);
        fs::create_dir_all(tmp.join("contracts")).unwrap();
    }

    fn truffle_project(tmp: &Path, src: &str) {
        write(&tmp.join("truffle-config.js"), src);
        fs::create_dir_all(tmp.join("contracts")).unwrap();
    }

    #[test]
    fn load_populates_foundry_only_when_present() {
        let tmp = TempDir::new().unwrap();
        foundry_project(
            tmp.path(),
            "[profile.default]\nsrc = \"src\"\nsolc = \"0.8.20\"\n",
        );
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        assert!(cfg.foundry.is_some());
        assert!(cfg.hardhat.is_none());
        assert!(cfg.truffle.is_none());
        assert_eq!(cfg.solc_versions(), vec!["0.8.20".to_string()]);
    }

    #[test]
    fn load_merges_remappings_txt_into_foundry_external() {
        let tmp = TempDir::new().unwrap();
        foundry_project(tmp.path(), "[profile.default]\n");
        write(
            &tmp.path().join("remappings.txt"),
            "@oz/=lib/openzeppelin/\nforge-std/=lib/forge-std/src/\n",
        );
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let foundry = cfg.foundry.as_ref().unwrap();
        assert_eq!(foundry.external_remappings.len(), 2);
        let remaps = cfg.remappings();
        assert_eq!(remaps.len(), 2);
        assert_eq!(remaps[0].prefix, "@oz/");
    }

    #[test]
    fn load_populates_hardhat_only_when_present() {
        let tmp = TempDir::new().unwrap();
        hardhat_project(
            tmp.path(),
            "module.exports = { solidity: \"0.8.24\", paths: { sources: \"./contracts\" } };",
        );
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        assert!(cfg.hardhat.is_some());
        assert!(cfg.foundry.is_none());
        assert_eq!(cfg.solc_versions(), vec!["0.8.24".to_string()]);
    }

    #[test]
    fn load_populates_truffle_only_when_present() {
        let tmp = TempDir::new().unwrap();
        truffle_project(
            tmp.path(),
            "module.exports = { compilers: { solc: { version: \"0.8.19\" } } };",
        );
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        assert!(cfg.truffle.is_some());
        assert!(cfg.foundry.is_none());
        assert_eq!(cfg.solc_versions(), vec!["0.8.19".to_string()]);
    }

    #[test]
    fn load_mixed_project_populates_multiple_fields() {
        let tmp = TempDir::new().unwrap();
        foundry_project(
            tmp.path(),
            "[profile.default]\nsrc = \"src\"\nsolc = \"0.8.20\"\n",
        );
        hardhat_project(
            tmp.path(),
            "module.exports = { solidity: \"0.8.24\", paths: { sources: \"./contracts\" } };",
        );
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        assert!(cfg.foundry.is_some());
        assert!(cfg.hardhat.is_some());
        let versions = cfg.solc_versions();
        assert!(versions.contains(&"0.8.20".to_string()));
        assert!(versions.contains(&"0.8.24".to_string()));
    }

    #[test]
    fn load_empty_directory_yields_all_none() {
        let tmp = TempDir::new().unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        assert!(cfg.is_empty());
        assert!(cfg.source_dirs().is_empty());
        assert!(cfg.remappings().is_empty());
    }

    #[test]
    fn source_dirs_fall_back_to_discovered_dirs_when_no_config() {
        let tmp = TempDir::new().unwrap();
        // Bare sol project: has src/ + contracts/ but no config.
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/A.sol"), b"").unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        assert!(cfg.is_empty());
        assert_eq!(cfg.source_dirs().len(), 1);
        assert!(cfg.source_dirs()[0].ends_with("src"));
    }

    #[test]
    fn source_dirs_from_foundry_uses_configured_src() {
        let tmp = TempDir::new().unwrap();
        foundry_project(tmp.path(), "[profile.default]\nsrc = \"contracts\"\n");
        fs::create_dir_all(tmp.path().join("contracts")).unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let dirs = cfg.source_dirs();
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("contracts"));
    }

    #[test]
    fn malformed_foundry_toml_propagates_as_parse_failed() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("foundry.toml"), "this is = not = valid\n");
        let layout = detect_layout(tmp.path()).unwrap();
        let err = load_project_config(layout).unwrap_err();
        assert!(
            matches!(err, ProjectError::ParseFailed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn mixed_versions_are_de_duplicated_across_configs() {
        let tmp = TempDir::new().unwrap();
        foundry_project(
            tmp.path(),
            "[profile.default]\nsrc = \"src\"\nsolc = \"0.8.20\"\n",
        );
        hardhat_project(tmp.path(), "module.exports = { solidity: \"0.8.20\" };");
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        assert_eq!(cfg.solc_versions(), vec!["0.8.20".to_string()]);
    }

    #[test]
    fn source_and_test_dirs_are_absolute_under_root() {
        let tmp = TempDir::new().unwrap();
        foundry_project(
            tmp.path(),
            "[profile.default]\nsrc = \"contracts\"\ntest = \"tests\"\n",
        );
        fs::create_dir_all(tmp.path().join("contracts")).unwrap();
        fs::create_dir_all(tmp.path().join("tests")).unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        let root = layout.root.clone();
        let cfg = load_project_config(layout).unwrap();
        let src = &cfg.source_dirs()[0];
        let test = &cfg.test_dirs()[0];
        assert!(src.starts_with(&root), "{src:?} not under {root:?}");
        assert!(test.starts_with(&root), "{test:?} not under {root:?}");
    }
}
