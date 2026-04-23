//! Project layout detection.
//!
//! Pure filesystem: classify a directory as Foundry / Hardhat / Truffle
//! / Mixed / Unknown / `NoSolidity`, and record the concrete paths that
//! made us decide that. Later checkpoints (`CP5`, `CP6`) parse the
//! configs and enumerate sources; this layer is the cheap "what kind of
//! project is this?" pass that every later stage builds on.

use std::{
    fs,
    path::{Path, PathBuf},
};

use basilisk_core::ProjectKind;
use serde::{Deserialize, Serialize};

use crate::error::ProjectError;

/// A config file discovered at the project root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigFile {
    /// `foundry.toml`.
    Foundry(PathBuf),
    /// `hardhat.config.{js,ts,cjs,mjs}`.
    Hardhat(PathBuf),
    /// `truffle-config.js`.
    Truffle(PathBuf),
    /// `package.json` — noted for later tooling detection, not parsed here.
    PackageJson(PathBuf),
}

impl ConfigFile {
    /// Absolute path to this config file.
    pub fn path(&self) -> &Path {
        match self {
            Self::Foundry(p) | Self::Hardhat(p) | Self::Truffle(p) | Self::PackageJson(p) => p,
        }
    }
}

/// Conventional source layout discovered inside a project root.
///
/// All paths are absolute when present. A directory is only listed if it
/// actually exists on disk — we don't fabricate conventional defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLayout {
    /// Canonicalized absolute path to the project root.
    pub root: PathBuf,
    /// Coarse classification. Always set; may be `NoSolidity`.
    pub kind: ProjectKind,
    /// Every config file we recognized at the root.
    pub config_files: Vec<ConfigFile>,
    /// Discovered Solidity source directories (`src/`, `contracts/`).
    pub source_dirs: Vec<PathBuf>,
    /// Discovered test directories (`test/`, `tests/`).
    pub test_dirs: Vec<PathBuf>,
    /// Discovered script / migration directories.
    pub script_dirs: Vec<PathBuf>,
    /// Dependency directories (`lib/`, `node_modules/`).
    pub lib_dirs: Vec<PathBuf>,
    /// `remappings.txt` at the root, if present.
    pub remappings_file: Option<PathBuf>,
    /// Package-manager lock files found at the root.
    pub lock_files: Vec<PathBuf>,
}

impl ProjectLayout {
    /// `true` iff the project has no Solidity sources we can see.
    pub fn is_empty(&self) -> bool {
        matches!(self.kind, ProjectKind::NoSolidity) && self.source_dirs.is_empty()
    }

    /// Path to the Foundry config, if this layout has one.
    pub fn foundry_toml(&self) -> Option<&Path> {
        self.config_files.iter().find_map(|c| match c {
            ConfigFile::Foundry(p) => Some(p.as_path()),
            _ => None,
        })
    }

    /// Path to the Hardhat config, if this layout has one.
    pub fn hardhat_config(&self) -> Option<&Path> {
        self.config_files.iter().find_map(|c| match c {
            ConfigFile::Hardhat(p) => Some(p.as_path()),
            _ => None,
        })
    }

    /// Path to the Truffle config, if this layout has one.
    pub fn truffle_config(&self) -> Option<&Path> {
        self.config_files.iter().find_map(|c| match c {
            ConfigFile::Truffle(p) => Some(p.as_path()),
            _ => None,
        })
    }
}

/// Inspect `root` and produce a [`ProjectLayout`].
///
/// `root` is canonicalized on entry so downstream code can rely on
/// absolute paths. The function never walks deep into the tree — it only
/// looks at direct children of `root`, plus the well-known convention
/// directories (one level deep). That keeps CP4 cheap; CP5 onwards can
/// walk source subtrees with explicit ignore rules.
pub fn detect_layout(root: &Path) -> Result<ProjectLayout, ProjectError> {
    if !root.exists() {
        return Err(ProjectError::DoesNotExist(root.to_path_buf()));
    }
    if !root.is_dir() {
        return Err(ProjectError::NotADirectory(root.to_path_buf()));
    }
    let root = fs::canonicalize(root).map_err(|e| ProjectError::io(root, e))?;

    let mut layout = ProjectLayout {
        root: root.clone(),
        kind: ProjectKind::NoSolidity,
        config_files: Vec::new(),
        source_dirs: Vec::new(),
        test_dirs: Vec::new(),
        script_dirs: Vec::new(),
        lib_dirs: Vec::new(),
        remappings_file: None,
        lock_files: Vec::new(),
    };

    // Pass 1: scan the root directory for config / lock / remappings files,
    // and record which convention directories exist.
    let entries = fs::read_dir(&root).map_err(|e| ProjectError::io(&root, e))?;
    let mut has_foundry = false;
    let mut has_hardhat = false;
    let mut has_truffle = false;

    for entry in entries {
        let entry = entry.map_err(|e| ProjectError::io(&root, e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| ProjectError::io(entry.path(), e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();

        if file_type.is_file() {
            match name {
                "foundry.toml" => {
                    has_foundry = true;
                    layout.config_files.push(ConfigFile::Foundry(path));
                }
                "hardhat.config.js" | "hardhat.config.ts" | "hardhat.config.cjs"
                | "hardhat.config.mjs" => {
                    has_hardhat = true;
                    layout.config_files.push(ConfigFile::Hardhat(path));
                }
                "truffle-config.js" => {
                    has_truffle = true;
                    layout.config_files.push(ConfigFile::Truffle(path));
                }
                "package.json" => {
                    layout.config_files.push(ConfigFile::PackageJson(path));
                }
                "remappings.txt" => {
                    layout.remappings_file = Some(path);
                }
                "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "bun.lockb" => {
                    layout.lock_files.push(path);
                }
                _ => {}
            }
        } else if file_type.is_dir() {
            match name {
                "src" | "contracts" => layout.source_dirs.push(path),
                "test" | "tests" => layout.test_dirs.push(path),
                "script" | "scripts" | "migrations" => layout.script_dirs.push(path),
                "lib" | "node_modules" => layout.lib_dirs.push(path),
                _ => {}
            }
        }
    }

    layout.kind = classify_kind(&root, has_foundry, has_hardhat, has_truffle);

    // Stable ordering — makes tests deterministic and Display output
    // predictable across filesystems.
    sort_layout_paths(&mut layout);

    Ok(layout)
}

fn classify_kind(root: &Path, foundry: bool, hardhat: bool, truffle: bool) -> ProjectKind {
    let mut found = Vec::new();
    if foundry {
        found.push(ProjectKind::Foundry);
    }
    if hardhat {
        found.push(ProjectKind::Hardhat);
    }
    if truffle {
        found.push(ProjectKind::Truffle);
    }

    match found.len() {
        0 => {
            if has_any_sol_file(root) {
                ProjectKind::Unknown
            } else {
                ProjectKind::NoSolidity
            }
        }
        1 => found.into_iter().next().expect("len == 1"),
        _ => ProjectKind::Mixed(found),
    }
}

/// Mirror of the `basilisk_core::detect::contains_sol_file` heuristic.
/// We duplicate it here to keep the crate-level concern self-contained
/// and because CP5 will extend this with richer rules.
fn has_any_sol_file(root: &Path) -> bool {
    const SKIP: &[&str] = &[
        "node_modules",
        "lib",
        "out",
        "artifacts",
        "cache",
        ".git",
        "target",
        "dist",
        "build",
        "coverage",
    ];
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                let skip = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| SKIP.contains(&n));
                if !skip {
                    stack.push(path);
                }
            } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("sol") {
                return true;
            }
        }
    }
    false
}

fn sort_layout_paths(layout: &mut ProjectLayout) {
    layout.config_files.sort_by(|a, b| a.path().cmp(b.path()));
    layout.source_dirs.sort();
    layout.test_dirs.sort();
    layout.script_dirs.sort();
    layout.lib_dirs.sort();
    layout.lock_files.sort();
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"").unwrap();
        p
    }

    fn mkdir(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn detect_foundry_project_collects_config_and_dirs() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "foundry.toml");
        touch(tmp.path(), "remappings.txt");
        mkdir(tmp.path(), "src");
        mkdir(tmp.path(), "test");
        mkdir(tmp.path(), "script");
        mkdir(tmp.path(), "lib");

        let layout = detect_layout(tmp.path()).unwrap();
        assert_eq!(layout.kind, ProjectKind::Foundry);
        assert!(layout.foundry_toml().is_some());
        assert!(layout.hardhat_config().is_none());
        assert_eq!(layout.source_dirs.len(), 1);
        assert_eq!(layout.test_dirs.len(), 1);
        assert_eq!(layout.script_dirs.len(), 1);
        assert_eq!(layout.lib_dirs.len(), 1);
        assert!(layout.remappings_file.is_some());
    }

    #[test]
    fn detect_hardhat_project_notes_package_json_and_node_modules() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "hardhat.config.ts");
        touch(tmp.path(), "package.json");
        touch(tmp.path(), "package-lock.json");
        mkdir(tmp.path(), "contracts");
        mkdir(tmp.path(), "test");
        mkdir(tmp.path(), "scripts");
        mkdir(tmp.path(), "node_modules");

        let layout = detect_layout(tmp.path()).unwrap();
        assert_eq!(layout.kind, ProjectKind::Hardhat);
        assert!(layout.hardhat_config().is_some());
        assert!(layout
            .config_files
            .iter()
            .any(|c| matches!(c, ConfigFile::PackageJson(_))));
        assert_eq!(layout.lock_files.len(), 1);
        assert!(layout.lib_dirs.iter().any(|p| p.ends_with("node_modules")));
    }

    #[test]
    fn detect_truffle_project() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "truffle-config.js");
        mkdir(tmp.path(), "contracts");
        mkdir(tmp.path(), "migrations");
        mkdir(tmp.path(), "test");

        let layout = detect_layout(tmp.path()).unwrap();
        assert_eq!(layout.kind, ProjectKind::Truffle);
        assert!(layout.truffle_config().is_some());
        assert!(layout.script_dirs.iter().any(|p| p.ends_with("migrations")));
    }

    #[test]
    fn detect_mixed_project_records_every_config() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "foundry.toml");
        touch(tmp.path(), "hardhat.config.js");

        let layout = detect_layout(tmp.path()).unwrap();
        match &layout.kind {
            ProjectKind::Mixed(kinds) => {
                assert!(kinds.contains(&ProjectKind::Foundry));
                assert!(kinds.contains(&ProjectKind::Hardhat));
            }
            other => panic!("expected Mixed, got {other:?}"),
        }
        assert_eq!(layout.config_files.len(), 2);
    }

    #[test]
    fn detect_empty_dir_is_no_solidity() {
        let tmp = TempDir::new().unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        assert_eq!(layout.kind, ProjectKind::NoSolidity);
        assert!(layout.config_files.is_empty());
        assert!(layout.is_empty());
    }

    #[test]
    fn detect_bare_sol_is_unknown_kind() {
        let tmp = TempDir::new().unwrap();
        mkdir(tmp.path(), "src");
        fs::write(tmp.path().join("src/A.sol"), b"// SPDX\n").unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        assert_eq!(layout.kind, ProjectKind::Unknown);
        assert_eq!(layout.source_dirs.len(), 1);
    }

    #[test]
    fn detect_skips_sol_only_inside_ignored_dirs() {
        let tmp = TempDir::new().unwrap();
        mkdir(tmp.path(), "node_modules");
        fs::write(tmp.path().join("node_modules/A.sol"), b"").unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        assert_eq!(layout.kind, ProjectKind::NoSolidity);
    }

    #[test]
    fn detect_missing_path_errors() {
        let err = detect_layout(Path::new("/definitely/not/a/basilisk/path")).unwrap_err();
        assert!(matches!(err, ProjectError::DoesNotExist(_)), "got {err:?}");
    }

    #[test]
    fn detect_file_path_is_not_a_directory() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        fs::write(&f, b"hi").unwrap();
        let err = detect_layout(&f).unwrap_err();
        assert!(matches!(err, ProjectError::NotADirectory(_)), "got {err:?}");
    }

    #[test]
    fn layout_config_file_paths_are_sorted() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "hardhat.config.ts");
        touch(tmp.path(), "foundry.toml");
        let layout = detect_layout(tmp.path()).unwrap();
        let names: Vec<_> = layout
            .config_files
            .iter()
            .map(|c| c.path().file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["foundry.toml", "hardhat.config.ts"]);
    }

    #[test]
    fn layout_lock_files_include_yarn_and_pnpm() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "hardhat.config.js");
        touch(tmp.path(), "yarn.lock");
        touch(tmp.path(), "pnpm-lock.yaml");
        let layout = detect_layout(tmp.path()).unwrap();
        assert_eq!(layout.lock_files.len(), 2);
    }
}
