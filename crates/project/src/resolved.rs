//! `ResolvedProject` — the source-side counterpart of
//! `basilisk_onchain::ResolvedSystem`.
//!
//! Aggregates every source-side artefact the auditor has computed for
//! one project: the parsed [`ProjectConfig`] (layout + per-flavour
//! configs), the [`SourceEnumeration`] (`.sol` files tagged by role),
//! and the [`ImportGraph`] (resolved + unresolved import edges with
//! traversal helpers).
//!
//! The single-shot entry point [`resolve_project`] runs the whole
//! pipeline. It's the function the CLI's `recon` path calls for a
//! `Target::LocalPath` and (once `CP7d` wires it up) a `Target::Github`
//! after its working tree has been cloned.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::{
    config::{load_project_config, ProjectConfig},
    error::ProjectError,
    graph::{build_import_graph, ImportGraph, ImportGraphStats},
    layout::{detect_layout, ProjectLayout},
    sources::{enumerate_sources, SourceEnumeration},
};

/// Everything we know about one project after the source-side pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedProject {
    /// Canonical absolute path to the project root (same as
    /// `config.layout.root`, repeated here for convenience).
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub enumeration: SourceEnumeration,
    pub graph: ImportGraph,
    #[serde(with = "crate::time_serde")]
    pub resolved_at: SystemTime,
}

impl ResolvedProject {
    /// Shortcut: the canonical classification of this project.
    pub fn kind(&self) -> basilisk_core::ProjectKind {
        self.config.layout.kind.clone()
    }

    /// Shortcut: the graph's stats.
    pub fn stats(&self) -> ImportGraphStats {
        self.graph.stats()
    }

    /// Every unresolved-import edge across every file, in a flat list.
    /// Ordered by `(importer, line)` for stable display.
    pub fn unresolved_imports(&self) -> Vec<(&Path, usize, &str)> {
        let mut out: Vec<(&Path, usize, &str)> = Vec::new();
        for path in self.graph.nodes() {
            if let Some(imports) = self.graph.imports_of(path) {
                for edge in &imports.unresolved {
                    out.push((
                        path.as_path(),
                        edge.statement.line,
                        &edge.statement.raw_path,
                    ));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
        out
    }
}

/// Run the full source-side pipeline for the project rooted at `root`.
pub fn resolve_project(root: &Path) -> Result<ResolvedProject, ProjectError> {
    let layout: ProjectLayout = detect_layout(root)?;
    let canonical_root = layout.root.clone();
    let config = load_project_config(layout)?;
    let enumeration = enumerate_sources(&config)?;
    let graph = build_import_graph(&config, &enumeration)?;
    Ok(ResolvedProject {
        root: canonical_root,
        config,
        enumeration,
        graph,
        resolved_at: SystemTime::now(),
    })
}

impl fmt::Display for ResolvedProject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Project at {}", self.root.display())?;
        writeln!(f, "  kind: {}", self.config.layout.kind)?;

        let configs: Vec<&'static str> = [
            self.config.foundry.as_ref().map(|_| "foundry.toml"),
            self.config.hardhat.as_ref().map(|_| "hardhat.config"),
            self.config.truffle.as_ref().map(|_| "truffle-config.js"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !configs.is_empty() {
            writeln!(f, "  configs: {}", configs.join(", "))?;
        }

        let solcs = self.config.solc_versions();
        if !solcs.is_empty() {
            writeln!(f, "  solc: {}", solcs.join(", "))?;
        }

        let remaps = self.config.remappings();
        if !remaps.is_empty() {
            writeln!(f, "  remappings: {}", remaps.len())?;
        }

        writeln!(
            f,
            "  sources: {} file(s)",
            self.enumeration.sources().count()
        )?;
        let test_count = self.enumeration.tests().count();
        if test_count > 0 {
            writeln!(f, "  tests: {test_count} file(s)")?;
        }
        let script_count = self.enumeration.scripts().count();
        if script_count > 0 {
            writeln!(f, "  scripts: {script_count} file(s)")?;
        }
        if !self.enumeration.missing_dirs.is_empty() {
            writeln!(f, "  missing dirs: {}", self.enumeration.missing_dirs.len())?;
            for dir in &self.enumeration.missing_dirs {
                writeln!(f, "    - {}", dir.display())?;
            }
        }

        let stats = self.graph.stats();
        writeln!(
            f,
            "  imports: {} resolved, {} unresolved ({} file(s) with unresolved)",
            stats.resolved_imports, stats.unresolved_imports, stats.files_with_unresolved,
        )?;
        if stats.external_files > 0 {
            writeln!(
                f,
                "  externals: {} file(s) reached via imports (deps)",
                stats.external_files
            )?;
        }

        let unresolved = self.unresolved_imports();
        if !unresolved.is_empty() {
            writeln!(f)?;
            writeln!(f, "Unresolved imports ({}):", unresolved.len())?;
            for (importer, line, raw) in unresolved {
                let rel = importer.strip_prefix(&self.root).unwrap_or(importer);
                writeln!(f, "  {}:{line}  {raw:?}", rel.display())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn resolve_foundry_project_populates_every_field() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nsolc = \"0.8.20\"\n",
        );
        write(&tmp.path().join("src/A.sol"), "import \"./B.sol\";\n");
        write(&tmp.path().join("src/B.sol"), "");

        let rp = resolve_project(tmp.path()).unwrap();
        assert!(rp.config.foundry.is_some());
        assert_eq!(rp.enumeration.sources().count(), 2);
        let stats = rp.stats();
        assert_eq!(stats.resolved_imports, 1);
        assert_eq!(stats.unresolved_imports, 0);
    }

    #[test]
    fn resolve_propagates_malformed_foundry_toml() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("foundry.toml"), "this is = not valid\n");
        let err = resolve_project(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ProjectError::ParseFailed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn resolve_empty_project_succeeds() {
        let tmp = TempDir::new().unwrap();
        let rp = resolve_project(tmp.path()).unwrap();
        assert_eq!(rp.enumeration.sources().count(), 0);
        assert_eq!(rp.stats().total_files, 0);
    }

    #[test]
    fn display_renders_core_fields() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nsolc = \"0.8.20\"\n",
        );
        write(&tmp.path().join("src/A.sol"), "");
        let rp = resolve_project(tmp.path()).unwrap();
        let out = format!("{rp}");
        assert!(out.contains("Project at "));
        assert!(out.contains("kind: foundry"));
        assert!(out.contains("0.8.20"));
        assert!(out.contains("sources: 1 file(s)"));
        // No unresolved → no warning section rendered.
        assert!(!out.contains("Unresolved imports"));
    }

    #[test]
    fn display_includes_unresolved_section_when_present() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write(&tmp.path().join("src/A.sol"), "import \"./missing.sol\";\n");
        let rp = resolve_project(tmp.path()).unwrap();
        let out = format!("{rp}");
        assert!(out.contains("Unresolved imports"));
        assert!(out.contains("src/A.sol:1"));
        assert!(out.contains("./missing.sol"));
    }

    #[test]
    fn display_shows_externals_for_dep_reached_files() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nremappings = [\"@oz/=lib/oz/\"]\n",
        );
        write(&tmp.path().join("src/A.sol"), "import \"@oz/Token.sol\";\n");
        write(&tmp.path().join("lib/oz/Token.sol"), "");
        let rp = resolve_project(tmp.path()).unwrap();
        let out = format!("{rp}");
        assert!(out.contains("externals:"));
    }

    #[test]
    fn unresolved_imports_list_is_sorted_by_importer_then_line() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write(
            &tmp.path().join("src/B.sol"),
            "import \"./a.sol\";\nimport \"./b.sol\";\n",
        );
        write(&tmp.path().join("src/A.sol"), "import \"./x.sol\";\n");
        let rp = resolve_project(tmp.path()).unwrap();
        let unresolved = rp.unresolved_imports();
        assert_eq!(unresolved.len(), 3);
        // A.sol (alphabetically first) before B.sol.
        assert!(unresolved[0].0.ends_with("A.sol"));
        assert!(unresolved[1].0.ends_with("B.sol"));
        assert_eq!(unresolved[1].1, 1); // B.sol line 1 before line 2.
        assert_eq!(unresolved[2].1, 2);
    }

    #[test]
    fn resolve_hardhat_project_populates_config_and_sources() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("hardhat.config.ts"),
            "module.exports = { solidity: \"0.8.24\", paths: { sources: \"./contracts\" } };",
        );
        write(&tmp.path().join("contracts/A.sol"), "");
        let rp = resolve_project(tmp.path()).unwrap();
        assert!(rp.config.hardhat.is_some());
        assert_eq!(rp.enumeration.sources().count(), 1);
    }
}
