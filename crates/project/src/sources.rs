//! Solidity source-file enumeration.
//!
//! Given a [`ProjectConfig`], walk the effective source / test / script
//! directories and collect every `.sol` file under them. The walker
//! skips well-known build / dependency directories (`node_modules`,
//! `out`, `artifacts`, …) so a Hardhat project with `.sol` files buried
//! under `node_modules/` doesn't drown the real sources. Dependency
//! files still get visited later by the import-resolution pass when a
//! top-level file pulls them in — this pass just scopes the initial
//! working set.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{config::ProjectConfig, error::ProjectError};

/// Directories we never descend into while enumerating sources.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "lib",
    "out",
    "artifacts",
    "cache",
    "target",
    "dist",
    "build",
    "coverage",
    "forge-cache",
    "broadcast",
];

/// What role a source file plays in the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    /// File lives under `source_dirs()` — the audit targets.
    Source,
    /// File lives under `test_dirs()`.
    Test,
    /// File lives under `script_dirs()`.
    Script,
}

/// One `.sol` file discovered by the enumerator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceFile {
    /// Canonical absolute path.
    pub absolute_path: PathBuf,
    /// Path relative to the project root — stable across machines.
    pub relative_path: PathBuf,
    pub kind: SourceKind,
}

/// Collected set of source files plus bookkeeping for the caller.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceEnumeration {
    pub files: Vec<SourceFile>,
    /// Directories we intended to walk but couldn't (missing on disk).
    /// The enumerator treats these as non-fatal — CP5 helpers synthesise
    /// "effective" defaults that may not exist in every repo.
    pub missing_dirs: Vec<PathBuf>,
}

impl SourceEnumeration {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
    /// `.sol` files tagged as `Source` (audit targets).
    pub fn sources(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter().filter(|f| f.kind == SourceKind::Source)
    }
    pub fn tests(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter().filter(|f| f.kind == SourceKind::Test)
    }
    pub fn scripts(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter().filter(|f| f.kind == SourceKind::Script)
    }
}

/// Walk the effective source / test / script directories of `cfg`, returning
/// every `.sol` file under them.
///
/// Rules:
/// - A file visited via multiple kind buckets (e.g. Foundry puts `src/`
///   into both source and script for some profiles) is reported once,
///   keyed by absolute path. Source wins over Test wins over Script.
/// - Symlinks are resolved once at the directory entry, but we don't
///   follow them recursively — that avoids the classic project-layout
///   infinite loop (`node_modules/foo -> ../foo`).
pub fn enumerate_sources(cfg: &ProjectConfig) -> Result<SourceEnumeration, ProjectError> {
    let root = cfg.layout.root.clone();
    let mut files: std::collections::BTreeMap<PathBuf, SourceFile> =
        std::collections::BTreeMap::new();
    let mut missing: Vec<PathBuf> = Vec::new();

    // Order matters: Source overwrites Test overwrites Script in the
    // dedupe step below. Iterate in reverse precedence so the final
    // insert wins.
    let buckets: [(SourceKind, Vec<PathBuf>); 3] = [
        (SourceKind::Script, cfg.script_dirs_for_enum()),
        (SourceKind::Test, cfg.test_dirs()),
        (SourceKind::Source, cfg.source_dirs()),
    ];

    for (kind, dirs) in buckets {
        for dir in dirs {
            if !dir.exists() {
                missing.push(dir);
                continue;
            }
            walk_dir(&root, &dir, kind, &mut files)?;
        }
    }

    let mut out: Vec<SourceFile> = files.into_values().collect();
    out.sort();

    let mut unique_missing: Vec<PathBuf> = missing
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    unique_missing.sort();

    Ok(SourceEnumeration {
        files: out,
        missing_dirs: unique_missing,
    })
}

/// Helper that mirrors [`ProjectConfig::source_dirs`] / `test_dirs` for
/// the script bucket. Kept local to this module so the `CP5c` surface
/// stays minimal — callers that want scripts use [`enumerate_sources`].
impl ProjectConfig {
    fn script_dirs_for_enum(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let root = &self.layout.root;

        if let Some(cfg) = &self.foundry {
            let profile = cfg
                .profile(crate::foundry::DEFAULT_PROFILE)
                .cloned()
                .unwrap_or_default();
            push_unique(&mut out, &mut seen, join_rel(root, &profile.script_dir()));
        }
        if let Some(cfg) = &self.truffle {
            push_unique(
                &mut out,
                &mut seen,
                join_rel(root, &cfg.migrations_or_default()),
            );
        }
        if out.is_empty() {
            for p in &self.layout.script_dirs {
                push_unique(&mut out, &mut seen, p.clone());
            }
        }
        out
    }
}

fn push_unique(out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if seen.insert(path.clone()) {
        out.push(path);
    }
}

fn join_rel(root: &Path, rel: &Path) -> PathBuf {
    if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        let normalized = rel
            .strip_prefix(".")
            .map_or_else(|_| rel.to_path_buf(), Path::to_path_buf);
        root.join(normalized)
    }
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    kind: SourceKind,
    out: &mut std::collections::BTreeMap<PathBuf, SourceFile>,
) -> Result<(), ProjectError> {
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current).map_err(|e| ProjectError::io(&current, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| ProjectError::io(&current, e))?;
            let file_type = entry
                .file_type()
                .map_err(|e| ProjectError::io(entry.path(), e))?;
            let path = entry.path();
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };

            if file_type.is_dir() {
                if SKIP_DIRS.contains(&name_str) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some("sol")
            {
                let absolute = fs::canonicalize(&path).unwrap_or(path.clone());
                let relative = absolute
                    .strip_prefix(root)
                    .unwrap_or(&absolute)
                    .to_path_buf();
                let entry = SourceFile {
                    absolute_path: absolute.clone(),
                    relative_path: relative,
                    kind,
                };
                // Source wins over Test wins over Script via insertion order.
                out.entry(absolute)
                    .and_modify(|prev| {
                        if kind < prev.kind {
                            prev.kind = kind;
                        }
                    })
                    .or_insert(entry);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::{config::load_project_config, layout::detect_layout};

    fn mksol(dir: &Path, rel: &str) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(
            &p,
            b"// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\n",
        )
        .unwrap();
        p
    }

    fn scaffold_foundry(tmp: &Path) {
        fs::write(
            tmp.join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\ntest = \"test\"\nscript = \"script\"\n",
        )
        .unwrap();
    }

    fn scaffold_hardhat(tmp: &Path) {
        fs::write(
            tmp.join("hardhat.config.ts"),
            "module.exports = { solidity: \"0.8.20\", paths: { sources: \"./contracts\", tests: \"./test\" } };",
        )
        .unwrap();
    }

    #[test]
    fn enumerate_foundry_project_tags_source_test_and_script() {
        let tmp = TempDir::new().unwrap();
        scaffold_foundry(tmp.path());
        mksol(tmp.path(), "src/A.sol");
        mksol(tmp.path(), "src/nested/B.sol");
        mksol(tmp.path(), "test/ATest.sol");
        mksol(tmp.path(), "script/Deploy.s.sol");

        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();

        assert_eq!(enum_.sources().count(), 2, "{:?}", enum_.files);
        assert_eq!(enum_.tests().count(), 1);
        assert_eq!(enum_.scripts().count(), 1);
        assert_eq!(enum_.len(), 4);
    }

    #[test]
    fn enumerate_hardhat_project_walks_contracts_and_test() {
        let tmp = TempDir::new().unwrap();
        scaffold_hardhat(tmp.path());
        mksol(tmp.path(), "contracts/A.sol");
        mksol(tmp.path(), "contracts/B.sol");
        mksol(tmp.path(), "test/A.test.sol");

        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        assert_eq!(enum_.sources().count(), 2);
        assert_eq!(enum_.tests().count(), 1);
    }

    #[test]
    fn enumerate_skips_node_modules_and_out_and_artifacts() {
        let tmp = TempDir::new().unwrap();
        scaffold_foundry(tmp.path());
        mksol(tmp.path(), "src/A.sol");
        mksol(tmp.path(), "src/node_modules/bad.sol");
        mksol(tmp.path(), "src/out/bad.sol");
        mksol(tmp.path(), "src/artifacts/bad.sol");
        mksol(tmp.path(), "src/.git/bad.sol");

        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        assert_eq!(enum_.len(), 1, "got {:?}", enum_.files);
    }

    #[test]
    fn enumerate_deduplicates_when_dirs_overlap() {
        // A pathological (but real) Foundry config: src == test == "src".
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\ntest = \"src\"\n",
        )
        .unwrap();
        mksol(tmp.path(), "src/A.sol");

        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        assert_eq!(enum_.len(), 1);
        // Source wins over Test for overlapping dirs.
        assert_eq!(enum_.files[0].kind, SourceKind::Source);
    }

    #[test]
    fn enumerate_records_missing_dirs_non_fatally() {
        let tmp = TempDir::new().unwrap();
        scaffold_foundry(tmp.path());
        // Only `src` exists; test/ and script/ are listed by default but missing.
        mksol(tmp.path(), "src/A.sol");

        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        assert_eq!(enum_.sources().count(), 1);
        assert!(!enum_.missing_dirs.is_empty());
    }

    #[test]
    fn enumerate_result_is_deterministically_sorted() {
        let tmp = TempDir::new().unwrap();
        scaffold_foundry(tmp.path());
        mksol(tmp.path(), "src/Z.sol");
        mksol(tmp.path(), "src/A.sol");
        mksol(tmp.path(), "src/M.sol");

        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        let names: Vec<_> = enum_
            .files
            .iter()
            .map(|f| {
                f.relative_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["A.sol", "M.sol", "Z.sol"]);
    }

    #[test]
    fn relative_path_is_under_root() {
        let tmp = TempDir::new().unwrap();
        scaffold_foundry(tmp.path());
        mksol(tmp.path(), "src/A.sol");
        let layout = detect_layout(tmp.path()).unwrap();
        let root = layout.root.clone();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        let rel = &enum_.files[0].relative_path;
        assert!(!rel.is_absolute(), "relative_path was absolute: {rel:?}");
        assert!(enum_.files[0].absolute_path.starts_with(&root));
    }

    #[test]
    fn enumerate_empty_project_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        assert!(enum_.is_empty());
    }

    #[test]
    fn enumerate_only_sol_files_counted() {
        let tmp = TempDir::new().unwrap();
        scaffold_foundry(tmp.path());
        mksol(tmp.path(), "src/A.sol");
        fs::write(tmp.path().join("src/README.md"), b"").unwrap();
        fs::write(tmp.path().join("src/junk.txt"), b"").unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enum_ = enumerate_sources(&cfg).unwrap();
        assert_eq!(enum_.len(), 1);
    }
}
