//! Project-wide Solidity import graph.
//!
//! Ties [`crate::sources::enumerate_sources`], [`crate::imports::parse_imports`],
//! and [`crate::resolver::ImportResolver`] together. Walks every
//! enumerated source file, parses its imports, resolves them, and
//! recursively follows resolved targets so dependency files that live
//! outside the source directories (typically under `lib/` or
//! `node_modules/`) get included in the graph as well.
//!
//! The result is a typed, queryable structure: a per-file edge list
//! (resolved + unresolved), forward / reverse traversal helpers, and
//! aggregate stats suitable for the audit report.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::ProjectConfig,
    error::ProjectError,
    imports::{parse_imports_in_file, ImportStatement},
    resolver::{ImportResolver, ResolutionAttempt, ResolutionVia, ResolvedImport},
    sources::{SourceEnumeration, SourceKind},
};

/// Resolved + unresolved imports for one source file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileImports {
    pub resolved: Vec<ResolvedEdge>,
    pub unresolved: Vec<UnresolvedEdge>,
}

impl FileImports {
    pub fn is_empty(&self) -> bool {
        self.resolved.is_empty() && self.unresolved.is_empty()
    }
    pub fn total(&self) -> usize {
        self.resolved.len() + self.unresolved.len()
    }
}

/// A successfully resolved import edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedEdge {
    pub statement: ImportStatement,
    pub target: PathBuf,
    pub via: ResolutionVia,
}

/// An import we couldn't resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedEdge {
    pub statement: ImportStatement,
    pub attempts: Vec<ResolutionAttempt>,
}

/// Aggregate counts for an [`ImportGraph`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportGraphStats {
    /// Every distinct file in the graph (enumerated + reached via imports).
    pub total_files: usize,
    /// Files that came from the enumeration (Source / Test / Script).
    pub source_files: usize,
    /// Files reached only via an import edge (typically `lib/` deps).
    pub external_files: usize,
    pub total_imports: usize,
    pub resolved_imports: usize,
    pub unresolved_imports: usize,
    /// Number of files with at least one unresolved import.
    pub files_with_unresolved: usize,
}

/// The full import graph for one project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportGraph {
    files: BTreeMap<PathBuf, FileImports>,
    /// Kind tag for files that were in the enumeration; absent for
    /// files reached only via an import edge.
    file_kinds: BTreeMap<PathBuf, SourceKind>,
}

impl ImportGraph {
    pub fn nodes(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.keys()
    }

    /// Per-file edges, if `path` is in the graph.
    pub fn imports_of(&self, path: &Path) -> Option<&FileImports> {
        self.files.get(path)
    }

    /// What kind of file this is — `Some` for enumerated files,
    /// `None` for files we only reached via an import edge.
    pub fn kind_of(&self, path: &Path) -> Option<SourceKind> {
        self.file_kinds.get(path).copied()
    }

    /// Every file transitively reachable from `file` via resolved imports.
    /// Does not include `file` itself.
    pub fn transitive_imports(&self, file: &Path) -> BTreeSet<PathBuf> {
        let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
        let mut queue: VecDeque<PathBuf> = VecDeque::new();
        queue.push_back(file.to_path_buf());
        while let Some(current) = queue.pop_front() {
            if let Some(imports) = self.files.get(&current) {
                for edge in &imports.resolved {
                    if visited.insert(edge.target.clone()) {
                        queue.push_back(edge.target.clone());
                    }
                }
            }
        }
        visited
    }

    /// Direct importers of `file` — files whose `resolved` edges point
    /// at it. Caller can transitive-close by repeated application.
    pub fn reverse_dependencies(&self, file: &Path) -> BTreeSet<PathBuf> {
        self.files
            .iter()
            .filter_map(|(p, imports)| {
                if imports.resolved.iter().any(|e| e.target == file) {
                    Some(p.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Aggregate counts. Computed on every call (graphs aren't huge).
    pub fn stats(&self) -> ImportGraphStats {
        let total_files = self.files.len();
        let source_files = self.file_kinds.len();
        let external_files = total_files.saturating_sub(source_files);

        let mut total_imports = 0usize;
        let mut resolved_imports = 0usize;
        let mut unresolved_imports = 0usize;
        let mut files_with_unresolved = 0usize;
        for imports in self.files.values() {
            total_imports += imports.total();
            resolved_imports += imports.resolved.len();
            unresolved_imports += imports.unresolved.len();
            if !imports.unresolved.is_empty() {
                files_with_unresolved += 1;
            }
        }

        ImportGraphStats {
            total_files,
            source_files,
            external_files,
            total_imports,
            resolved_imports,
            unresolved_imports,
            files_with_unresolved,
        }
    }
}

/// Walk every enumerated file, parse its imports, resolve each one,
/// follow resolved targets recursively, and assemble the full graph.
///
/// Files reached via import edges that aren't in the enumeration get
/// added as nodes with their own resolved/unresolved edges (this is how
/// we capture `lib/` and `node_modules/` dependency trees). They're
/// distinguished from enumerated files via [`ImportGraph::kind_of`].
pub fn build_import_graph(
    cfg: &ProjectConfig,
    enumeration: &SourceEnumeration,
) -> Result<ImportGraph, ProjectError> {
    let resolver = ImportResolver::from_project_config(cfg);
    let mut graph = ImportGraph::default();

    // Seed: every enumerated file gets a kind tag immediately, and
    // joins the to-visit queue.
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for file in &enumeration.files {
        let path = file.absolute_path.clone();
        graph.file_kinds.insert(path.clone(), file.kind);
        queue.push_back(path);
    }

    while let Some(path) = queue.pop_front() {
        if graph.files.contains_key(&path) {
            continue;
        }
        let imports = resolve_file_imports(&path, &resolver)?;
        for edge in &imports.resolved {
            // Schedule each newly-discovered target so dep trees get
            // crawled too. Already-visited paths are filtered above.
            if !graph.files.contains_key(&edge.target) && !queue.iter().any(|p| p == &edge.target) {
                queue.push_back(edge.target.clone());
            }
        }
        graph.files.insert(path, imports);
    }

    Ok(graph)
}

/// Convenience: enumerate + build in one step. Returns both so callers
/// can surface enumeration's `missing_dirs` separately.
pub fn build_import_graph_for(
    cfg: &ProjectConfig,
) -> Result<(SourceEnumeration, ImportGraph), ProjectError> {
    let enumeration = crate::sources::enumerate_sources(cfg)?;
    let graph = build_import_graph(cfg, &enumeration)?;
    Ok((enumeration, graph))
}

fn resolve_file_imports(
    path: &Path,
    resolver: &ImportResolver,
) -> Result<FileImports, ProjectError> {
    let statements = parse_imports_in_file(path)?;
    let importer_dir = path.parent().unwrap_or(Path::new("."));

    let mut imports = FileImports::default();
    for stmt in statements {
        match resolver.resolve(importer_dir, &stmt.raw_path) {
            ResolvedImport::Resolved { absolute_path, via } => {
                imports.resolved.push(ResolvedEdge {
                    statement: stmt,
                    target: absolute_path,
                    via,
                });
            }
            ResolvedImport::Unresolved { attempts, .. } => {
                imports.unresolved.push(UnresolvedEdge {
                    statement: stmt,
                    attempts,
                });
            }
        }
    }
    Ok(imports)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::{config::load_project_config, layout::detect_layout, sources::enumerate_sources};

    fn write_sol(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn build(tmp: &Path) -> (ProjectConfig, ImportGraph) {
        let layout = detect_layout(tmp).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let enumeration = enumerate_sources(&cfg).unwrap();
        let graph = build_import_graph(&cfg, &enumeration).unwrap();
        (cfg, graph)
    }

    #[test]
    fn simple_two_file_project_builds_one_edge() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(&tmp.path().join("src/A.sol"), "import \"./B.sol\";\n");
        write_sol(&tmp.path().join("src/B.sol"), "");
        let (_cfg, g) = build(tmp.path());
        let stats = g.stats();
        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.resolved_imports, 1);
        assert_eq!(stats.unresolved_imports, 0);
    }

    #[test]
    fn unresolved_import_is_recorded_with_attempts() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(
            &tmp.path().join("src/A.sol"),
            "import \"./does-not-exist.sol\";\n",
        );
        let (_cfg, g) = build(tmp.path());
        let stats = g.stats();
        assert_eq!(stats.total_files, 1);
        assert_eq!(stats.unresolved_imports, 1);
        assert_eq!(stats.files_with_unresolved, 1);

        let abs_a = fs::canonicalize(tmp.path().join("src/A.sol")).unwrap();
        let imports = g.imports_of(&abs_a).unwrap();
        assert_eq!(imports.unresolved.len(), 1);
        assert!(!imports.unresolved[0].attempts.is_empty());
    }

    #[test]
    fn graph_follows_imports_into_lib_dirs() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nremappings = [\"@oz/=lib/oz/\"]\n",
        );
        write_sol(&tmp.path().join("src/A.sol"), "import \"@oz/Token.sol\";\n");
        write_sol(
            &tmp.path().join("lib/oz/Token.sol"),
            "import \"./Internal.sol\";\n",
        );
        write_sol(&tmp.path().join("lib/oz/Internal.sol"), "");

        let (_cfg, g) = build(tmp.path());
        let stats = g.stats();
        // A + Token + Internal — all three are in the graph.
        assert_eq!(stats.total_files, 3);
        assert_eq!(stats.source_files, 1);
        assert_eq!(stats.external_files, 2);
        assert_eq!(stats.resolved_imports, 2);
    }

    #[test]
    fn transitive_imports_walk_resolved_edges() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(&tmp.path().join("src/A.sol"), "import \"./B.sol\";\n");
        write_sol(&tmp.path().join("src/B.sol"), "import \"./C.sol\";\n");
        write_sol(&tmp.path().join("src/C.sol"), "");

        let (_cfg, g) = build(tmp.path());
        let abs_a = fs::canonicalize(tmp.path().join("src/A.sol")).unwrap();
        let trans = g.transitive_imports(&abs_a);
        assert_eq!(trans.len(), 2);
        assert!(trans.iter().any(|p| p.ends_with("B.sol")));
        assert!(trans.iter().any(|p| p.ends_with("C.sol")));
    }

    #[test]
    fn cyclic_imports_terminate() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(&tmp.path().join("src/A.sol"), "import \"./B.sol\";\n");
        write_sol(&tmp.path().join("src/B.sol"), "import \"./A.sol\";\n");

        let (_cfg, g) = build(tmp.path());
        let abs_a = fs::canonicalize(tmp.path().join("src/A.sol")).unwrap();
        let trans = g.transitive_imports(&abs_a);
        // Includes B (and A again via the cycle, but A as the starting
        // point isn't included in its own transitive set).
        assert!(trans.iter().any(|p| p.ends_with("B.sol")));
    }

    #[test]
    fn reverse_dependencies_returns_direct_importers() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(&tmp.path().join("src/A.sol"), "import \"./Lib.sol\";\n");
        write_sol(&tmp.path().join("src/B.sol"), "import \"./Lib.sol\";\n");
        write_sol(&tmp.path().join("src/Lib.sol"), "");

        let (_cfg, g) = build(tmp.path());
        let abs_lib = fs::canonicalize(tmp.path().join("src/Lib.sol")).unwrap();
        let importers = g.reverse_dependencies(&abs_lib);
        assert_eq!(importers.len(), 2);
        assert!(importers.iter().any(|p| p.ends_with("A.sol")));
        assert!(importers.iter().any(|p| p.ends_with("B.sol")));
    }

    #[test]
    fn duplicate_imports_are_resolved_independently() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(
            &tmp.path().join("src/A.sol"),
            "import \"./B.sol\";\nimport {X} from \"./B.sol\";\n",
        );
        write_sol(&tmp.path().join("src/B.sol"), "");

        let (_cfg, g) = build(tmp.path());
        let abs_a = fs::canonicalize(tmp.path().join("src/A.sol")).unwrap();
        let imports = g.imports_of(&abs_a).unwrap();
        assert_eq!(imports.resolved.len(), 2, "{imports:?}");
    }

    #[test]
    fn external_files_have_no_kind_tag() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\nremappings = [\"@oz/=lib/oz/\"]\n",
        );
        write_sol(&tmp.path().join("src/A.sol"), "import \"@oz/Token.sol\";\n");
        write_sol(&tmp.path().join("lib/oz/Token.sol"), "");

        let (_cfg, g) = build(tmp.path());
        let abs_token = fs::canonicalize(tmp.path().join("lib/oz/Token.sol")).unwrap();
        let abs_a = fs::canonicalize(tmp.path().join("src/A.sol")).unwrap();
        assert_eq!(g.kind_of(&abs_a), Some(SourceKind::Source));
        assert_eq!(g.kind_of(&abs_token), None);
    }

    #[test]
    fn empty_project_yields_empty_graph() {
        let tmp = TempDir::new().unwrap();
        let (_cfg, g) = build(tmp.path());
        let stats = g.stats();
        assert_eq!(stats.total_files, 0);
        assert_eq!(stats.total_imports, 0);
    }

    #[test]
    fn build_import_graph_for_combines_enumeration_and_build() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(&tmp.path().join("src/A.sol"), "");
        let layout = detect_layout(tmp.path()).unwrap();
        let cfg = load_project_config(layout).unwrap();
        let (enumeration, graph) = build_import_graph_for(&cfg).unwrap();
        assert_eq!(enumeration.len(), 1);
        assert_eq!(graph.stats().source_files, 1);
    }

    #[test]
    fn stats_distinguishes_resolved_and_unresolved() {
        let tmp = TempDir::new().unwrap();
        write_sol(
            &tmp.path().join("foundry.toml"),
            "[profile.default]\nsrc = \"src\"\n",
        );
        write_sol(
            &tmp.path().join("src/A.sol"),
            "import \"./B.sol\";\nimport \"./missing.sol\";\n",
        );
        write_sol(&tmp.path().join("src/B.sol"), "");

        let (_cfg, g) = build(tmp.path());
        let stats = g.stats();
        assert_eq!(stats.resolved_imports, 1);
        assert_eq!(stats.unresolved_imports, 1);
        assert_eq!(stats.files_with_unresolved, 1);
        assert_eq!(stats.total_imports, 2);
    }
}
