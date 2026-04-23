//! Solidity import resolution.
//!
//! Given an [`crate::ImportStatement::raw_path`] from one source file
//! and the importing file's directory, figure out which file on disk it
//! actually points to. Mirrors the resolution rules `solc` and Foundry
//! use:
//!
//! 1. **Absolute paths** are taken as-is (rare in practice but valid).
//! 2. **Relative paths** (`./foo.sol`, `../foo.sol`) resolve against the
//!    importing file's parent directory.
//! 3. **Bare paths** (`@oz/Token.sol`, `forge-std/Test.sol`) try every
//!    applicable remapping in longest-prefix order, then fall back to
//!    each library search directory (`lib/`, `node_modules/`, …).
//!
//! Context-aware remappings (`{ context, prefix, target }`) only apply
//! when the importer's path — relative to the project root — starts
//! with `context`. Remappings with no context apply to every file.
//!
//! A successful resolve returns the canonicalized path plus a
//! [`ResolutionVia`] tag so the caller can show users *why* the file
//! resolved that way. A failed resolve returns every path we tried so
//! a missing-import error can point at the search list.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{config::ProjectConfig, foundry::Remapping};

/// Resolves Solidity import paths to filesystem paths.
#[derive(Debug, Clone)]
pub struct ImportResolver {
    project_root: PathBuf,
    /// Remappings sorted by descending prefix length so the longest
    /// match wins.
    remappings: Vec<Remapping>,
    lib_dirs: Vec<PathBuf>,
}

/// Outcome of [`ImportResolver::resolve`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedImport {
    /// Found a file on disk.
    Resolved {
        /// Canonical absolute path.
        absolute_path: PathBuf,
        /// Which resolution rule matched.
        via: ResolutionVia,
    },
    /// No matching file on disk. `attempts` lists every path we checked
    /// in the order we checked them — useful for "did you mean X?" hints.
    Unresolved {
        raw_path: String,
        attempts: Vec<ResolutionAttempt>,
    },
}

/// How an import was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionVia {
    /// Path was already absolute.
    Absolute,
    /// `./` or `../` resolved against the importing file's directory.
    Relative,
    /// Matched the given remapping.
    Remapping(Remapping),
    /// Found under one of the configured library search directories.
    LibDir(PathBuf),
}

/// A single path we tried to resolve to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionAttempt {
    pub via: ResolutionVia,
    pub tried_path: PathBuf,
}

impl ImportResolver {
    /// Build a resolver from an explicit set of inputs. Useful when the
    /// caller wants to override what [`Self::from_project_config`] would
    /// pick (e.g. add an extra remapping for a vendored dep).
    pub fn new(
        project_root: PathBuf,
        remappings: impl IntoIterator<Item = Remapping>,
        lib_dirs: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let mut remappings: Vec<Remapping> = remappings.into_iter().collect();
        remappings.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
        let lib_dirs: Vec<PathBuf> = lib_dirs.into_iter().collect();
        Self {
            project_root,
            remappings,
            lib_dirs,
        }
    }

    /// Build a resolver from a parsed [`ProjectConfig`]: project root,
    /// effective remappings (Foundry inline + external), and the
    /// library directories the layout discovered (`lib/`,
    /// `node_modules/`).
    pub fn from_project_config(cfg: &ProjectConfig) -> Self {
        Self::new(
            cfg.layout.root.clone(),
            cfg.remappings(),
            cfg.layout.lib_dirs.clone(),
        )
    }

    /// Project root the resolver was built against.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }
    /// Active remappings, longest-prefix-first.
    pub fn remappings(&self) -> &[Remapping] {
        &self.remappings
    }
    /// Library search directories.
    pub fn lib_dirs(&self) -> &[PathBuf] {
        &self.lib_dirs
    }

    /// Resolve `raw_path` as imported from a file in `importer_dir`.
    ///
    /// `importer_dir` should be the *parent directory* of the importing
    /// `.sol` file — the same folder the file lives in. Pass the project
    /// root if you don't have a specific importer (e.g. resolving a
    /// remapping standalone).
    pub fn resolve(&self, importer_dir: &Path, raw_path: &str) -> ResolvedImport {
        let trimmed = raw_path.trim();
        let mut attempts: Vec<ResolutionAttempt> = Vec::new();

        // 1. Absolute path.
        let candidate_path = Path::new(trimmed);
        if candidate_path.is_absolute() {
            if let Some(absolute) = canonical_if_exists(candidate_path) {
                return ResolvedImport::Resolved {
                    absolute_path: absolute,
                    via: ResolutionVia::Absolute,
                };
            }
            attempts.push(ResolutionAttempt {
                via: ResolutionVia::Absolute,
                tried_path: candidate_path.to_path_buf(),
            });
            return ResolvedImport::Unresolved {
                raw_path: raw_path.to_string(),
                attempts,
            };
        }

        // 2. Relative path.
        if trimmed.starts_with("./") || trimmed.starts_with("../") {
            let candidate = importer_dir.join(trimmed);
            if let Some(absolute) = canonical_if_exists(&candidate) {
                return ResolvedImport::Resolved {
                    absolute_path: absolute,
                    via: ResolutionVia::Relative,
                };
            }
            attempts.push(ResolutionAttempt {
                via: ResolutionVia::Relative,
                tried_path: candidate,
            });
            return ResolvedImport::Unresolved {
                raw_path: raw_path.to_string(),
                attempts,
            };
        }

        // 3. Remappings.
        let importer_rel = importer_dir
            .strip_prefix(&self.project_root)
            .unwrap_or(importer_dir);
        for remap in &self.remappings {
            if !remapping_applies(remap, importer_rel) {
                continue;
            }
            let Some(rest) = trimmed.strip_prefix(&remap.prefix) else {
                continue;
            };
            let target_root = if Path::new(&remap.target).is_absolute() {
                PathBuf::from(&remap.target)
            } else {
                self.project_root.join(&remap.target)
            };
            let candidate = target_root.join(rest);
            if let Some(absolute) = canonical_if_exists(&candidate) {
                return ResolvedImport::Resolved {
                    absolute_path: absolute,
                    via: ResolutionVia::Remapping(remap.clone()),
                };
            }
            attempts.push(ResolutionAttempt {
                via: ResolutionVia::Remapping(remap.clone()),
                tried_path: candidate,
            });
        }

        // 4. Library search directories.
        for lib in &self.lib_dirs {
            let candidate = lib.join(trimmed);
            if let Some(absolute) = canonical_if_exists(&candidate) {
                return ResolvedImport::Resolved {
                    absolute_path: absolute,
                    via: ResolutionVia::LibDir(lib.clone()),
                };
            }
            attempts.push(ResolutionAttempt {
                via: ResolutionVia::LibDir(lib.clone()),
                tried_path: candidate,
            });
        }

        ResolvedImport::Unresolved {
            raw_path: raw_path.to_string(),
            attempts,
        }
    }
}

fn canonical_if_exists(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    fs::canonicalize(path).ok()
}

fn remapping_applies(remap: &Remapping, importer_rel: &Path) -> bool {
    match &remap.context {
        None => true,
        Some(ctx) => {
            // Context is matched as a path prefix on the importer's
            // project-relative path. Solc uses string-prefix semantics;
            // we follow suit but compare components first to avoid
            // `test/` matching `tests/foo.sol`.
            let ctx_path = Path::new(ctx);
            importer_rel.starts_with(ctx_path)
                || importer_rel
                    .to_string_lossy()
                    .starts_with(&ctx.trim_end_matches('/').to_string())
        }
    }
}

#[cfg(test)]
#[allow(clippy::match_wildcard_for_single_variants)] // Tests panic on the unexpected variant; explicit names just add noise.
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"// SPDX-License-Identifier: MIT\n").unwrap();
    }

    fn remap(prefix: &str, target: &str) -> Remapping {
        Remapping {
            context: None,
            prefix: prefix.into(),
            target: target.into(),
        }
    }

    fn ctx_remap(context: &str, prefix: &str, target: &str) -> Remapping {
        Remapping {
            context: Some(context.into()),
            prefix: prefix.into(),
            target: target.into(),
        }
    }

    #[test]
    fn resolves_dot_relative_import() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("src/A.sol"));
        touch(&tmp.path().join("src/B.sol"));
        let r = ImportResolver::new(tmp.path().to_path_buf(), [], []);
        match r.resolve(&tmp.path().join("src"), "./B.sol") {
            ResolvedImport::Resolved { via, absolute_path } => {
                assert_eq!(via, ResolutionVia::Relative);
                assert!(absolute_path.ends_with("src/B.sol"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn resolves_dot_dot_relative_import() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("src/sub/A.sol"));
        touch(&tmp.path().join("src/B.sol"));
        let r = ImportResolver::new(tmp.path().to_path_buf(), [], []);
        let res = r.resolve(&tmp.path().join("src/sub"), "../B.sol");
        match res {
            ResolvedImport::Resolved { via, absolute_path } => {
                assert_eq!(via, ResolutionVia::Relative);
                assert!(absolute_path.ends_with("src/B.sol"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn relative_import_missing_file_records_attempt() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("src/A.sol"));
        let r = ImportResolver::new(tmp.path().to_path_buf(), [], []);
        match r.resolve(&tmp.path().join("src"), "./missing.sol") {
            ResolvedImport::Unresolved { attempts, raw_path } => {
                assert_eq!(raw_path, "./missing.sol");
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].via, ResolutionVia::Relative);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn resolves_via_remapping() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("lib/openzeppelin-contracts/Token.sol"));
        let remappings = vec![remap("@oz/", "lib/openzeppelin-contracts/")];
        let r = ImportResolver::new(tmp.path().to_path_buf(), remappings, []);
        let res = r.resolve(&tmp.path().join("src"), "@oz/Token.sol");
        match res {
            ResolvedImport::Resolved { via, absolute_path } => {
                assert!(matches!(via, ResolutionVia::Remapping(_)));
                assert!(absolute_path.ends_with("lib/openzeppelin-contracts/Token.sol"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn longest_prefix_wins() {
        let tmp = TempDir::new().unwrap();
        // Both remappings would match `@oz/security/`, but the more
        // specific prefix points at a different lib.
        touch(&tmp.path().join("lib/oz-base/security/Auth.sol"));
        touch(&tmp.path().join("lib/oz-security/Auth.sol"));
        let remappings = vec![
            remap("@oz/", "lib/oz-base/"),
            remap("@oz/security/", "lib/oz-security/"),
        ];
        let r = ImportResolver::new(tmp.path().to_path_buf(), remappings, []);
        let res = r.resolve(&tmp.path().join("src"), "@oz/security/Auth.sol");
        match res {
            ResolvedImport::Resolved { absolute_path, via } => {
                assert!(matches!(via, ResolutionVia::Remapping(r) if r.prefix == "@oz/security/"));
                assert!(absolute_path.ends_with("lib/oz-security/Auth.sol"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn context_remapping_only_applies_to_matching_importer() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("test/MockToken.sol"));
        touch(&tmp.path().join("lib/real/Token.sol"));
        // In tests, @oz/Token.sol means the mock; in src, it means the real one.
        let remappings = vec![
            ctx_remap("test/", "@oz/", "test/"),
            remap("@oz/", "lib/real/"),
        ];
        let r = ImportResolver::new(tmp.path().to_path_buf(), remappings, []);

        let from_test = r.resolve(&tmp.path().join("test"), "@oz/MockToken.sol");
        match from_test {
            ResolvedImport::Resolved { absolute_path, .. } => {
                assert!(absolute_path.ends_with("test/MockToken.sol"));
            }
            other => panic!("got {other:?}"),
        }

        let from_src = r.resolve(&tmp.path().join("src"), "@oz/Token.sol");
        match from_src {
            ResolvedImport::Resolved { absolute_path, via } => {
                assert!(absolute_path.ends_with("lib/real/Token.sol"));
                assert!(matches!(via, ResolutionVia::Remapping(r) if r.context.is_none()));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_lib_dirs_when_no_remapping_matches() {
        let tmp = TempDir::new().unwrap();
        touch(&tmp.path().join("node_modules/forge-std/Test.sol"));
        let r = ImportResolver::new(
            tmp.path().to_path_buf(),
            [],
            [tmp.path().join("node_modules")],
        );
        let res = r.resolve(&tmp.path().join("contracts"), "forge-std/Test.sol");
        match res {
            ResolvedImport::Resolved { via, absolute_path } => {
                assert!(matches!(via, ResolutionVia::LibDir(_)));
                assert!(absolute_path.ends_with("node_modules/forge-std/Test.sol"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unresolved_records_every_attempt_in_order() {
        let tmp = TempDir::new().unwrap();
        // Nothing exists at any of the candidate paths.
        let remappings = vec![remap("@oz/", "lib/oz/")];
        let r = ImportResolver::new(
            tmp.path().to_path_buf(),
            remappings,
            [tmp.path().join("node_modules"), tmp.path().join("lib")],
        );
        let res = r.resolve(&tmp.path().join("src"), "@oz/Token.sol");
        match res {
            ResolvedImport::Unresolved { attempts, raw_path } => {
                assert_eq!(raw_path, "@oz/Token.sol");
                assert!(
                    attempts.len() >= 3,
                    "expected >=3 attempts, got {attempts:?}"
                );
                // Order: remapping → first lib → second lib.
                assert!(matches!(attempts[0].via, ResolutionVia::Remapping(_)));
                assert!(matches!(attempts[1].via, ResolutionVia::LibDir(_)));
                assert!(matches!(attempts[2].via, ResolutionVia::LibDir(_)));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn absolute_path_resolves_directly() {
        let tmp = TempDir::new().unwrap();
        let abs = tmp.path().join("src/A.sol");
        touch(&abs);
        let r = ImportResolver::new(tmp.path().to_path_buf(), [], []);
        match r.resolve(&tmp.path().join("anywhere"), abs.to_str().unwrap()) {
            ResolvedImport::Resolved { via, .. } => {
                assert_eq!(via, ResolutionVia::Absolute);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn from_project_config_picks_up_remappings_and_lib_dirs() {
        use crate::{config::load_project_config, layout::detect_layout};

        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("foundry.toml"),
            "[profile.default]\nremappings = [\"@oz/=lib/oz/\"]\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("lib/oz")).unwrap();
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let r = ImportResolver::from_project_config(&cfg);
        assert_eq!(r.remappings().len(), 1);
        assert_eq!(r.remappings()[0].prefix, "@oz/");
        assert!(r.lib_dirs().iter().any(|p| p.ends_with("lib")));
    }

    #[test]
    fn remapping_target_can_be_absolute() {
        let tmp = TempDir::new().unwrap();
        let lib = tmp.path().join("vendored/oz");
        touch(&lib.join("Token.sol"));
        let remappings = vec![remap("@oz/", lib.to_str().unwrap())];
        let r = ImportResolver::new(tmp.path().to_path_buf(), remappings, []);
        let res = r.resolve(&tmp.path().join("src"), "@oz/Token.sol");
        match res {
            ResolvedImport::Resolved { absolute_path, .. } => {
                assert!(absolute_path.ends_with("vendored/oz/Token.sol"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn resolver_constructor_sorts_remappings_longest_first() {
        let r = ImportResolver::new(
            PathBuf::from("/tmp"),
            [
                remap("@a/", "lib/a/"),
                remap("@a/long/prefix/", "lib/long/"),
                remap("@a/x/", "lib/x/"),
            ],
            [],
        );
        let prefixes: Vec<&str> = r.remappings().iter().map(|r| r.prefix.as_str()).collect();
        assert_eq!(prefixes, vec!["@a/long/prefix/", "@a/x/", "@a/"]);
    }
}
