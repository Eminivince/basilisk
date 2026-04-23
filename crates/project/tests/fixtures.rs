//! Fixture-based integration tests for the full source-side pipeline.
//!
//! Each scenario lives under `tests/fixtures/<name>/` as a real (tiny)
//! on-disk project. We run `resolve_project` against it and assert on
//! the resulting `ResolvedProject` — classification, configs, source
//! counts, import resolution outcomes. Covering the shapes we expect
//! to see in the wild: single-flavour projects (Foundry / Hardhat /
//! Truffle), mixed-config roots, bare `.sol`-only dirs, empty dirs,
//! projects with unresolvable imports, and monorepo subpackages.

use std::path::{Path, PathBuf};

use basilisk_core::ProjectKind;
use basilisk_project::{resolve_project, ResolvedProject, SourceKind};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn resolve(name: &str) -> ResolvedProject {
    resolve_project(&fixture(name))
        .unwrap_or_else(|e| panic!("fixture {name:?} failed to resolve: {e}"))
}

#[test]
fn foundry_minimal_has_full_graph_with_remappings() {
    let rp = resolve("foundry-minimal");
    assert_eq!(rp.config.layout.kind, ProjectKind::Foundry);
    assert!(rp.config.foundry.is_some());
    assert_eq!(rp.enumeration.sources().count(), 2);
    assert_eq!(rp.enumeration.tests().count(), 1);

    let stats = rp.stats();
    // 3 files enumerated + 2 lib deps reached (Ownable, forge-std Test).
    assert_eq!(stats.source_files, 3);
    assert_eq!(stats.external_files, 2, "stats: {stats:?}");
    assert_eq!(stats.unresolved_imports, 0);
    assert!(rp.config.solc_versions().contains(&"0.8.20".to_string()));
    // `remappings.txt` merged in alongside the inline `@oz/` mapping.
    assert!(rp.config.remappings().iter().any(|r| r.prefix == "@oz/"));
    assert!(rp
        .config
        .remappings()
        .iter()
        .any(|r| r.prefix == "forge-std/"));
}

#[test]
fn hardhat_minimal_resolves_oz_via_node_modules() {
    let rp = resolve("hardhat-minimal");
    assert_eq!(rp.config.layout.kind, ProjectKind::Hardhat);
    assert!(rp.config.hardhat.is_some());
    assert_eq!(rp.enumeration.sources().count(), 1);

    let stats = rp.stats();
    // Token.sol + Ownable.sol reached via node_modules.
    assert_eq!(stats.source_files, 1);
    assert_eq!(stats.external_files, 1);
    assert_eq!(stats.unresolved_imports, 0);
    assert!(rp.config.solc_versions().contains(&"0.8.24".to_string()));
}

#[test]
fn truffle_minimal_has_migrations_script_but_no_imports() {
    let rp = resolve("truffle-minimal");
    assert_eq!(rp.config.layout.kind, ProjectKind::Truffle);
    assert!(rp.config.truffle.is_some());
    assert_eq!(rp.enumeration.sources().count(), 1);
    // migrations/1_initial.js is JS, not .sol — not enumerated.
    assert_eq!(rp.enumeration.scripts().count(), 0);
    assert_eq!(rp.stats().total_imports, 0);
}

#[test]
fn mixed_project_populates_both_configs() {
    let rp = resolve("mixed");
    match &rp.config.layout.kind {
        ProjectKind::Mixed(kinds) => {
            assert!(kinds.contains(&ProjectKind::Foundry));
            assert!(kinds.contains(&ProjectKind::Hardhat));
        }
        other => panic!("expected Mixed, got {other:?}"),
    }
    assert!(rp.config.foundry.is_some());
    assert!(rp.config.hardhat.is_some());
    // Both src/ and contracts/ are enumerated.
    assert_eq!(rp.enumeration.sources().count(), 2);
}

#[test]
fn no_config_project_falls_back_to_bare_discovery() {
    let rp = resolve("no-config");
    assert_eq!(rp.config.layout.kind, ProjectKind::Unknown);
    assert!(rp.config.foundry.is_none());
    assert!(rp.config.hardhat.is_none());
    // Source dir still discovered via the layout fallback.
    assert_eq!(rp.enumeration.sources().count(), 1);
}

#[test]
fn empty_project_yields_no_solidity_and_empty_graph() {
    let rp = resolve("empty");
    assert_eq!(rp.config.layout.kind, ProjectKind::NoSolidity);
    assert!(rp.config.is_empty());
    assert_eq!(rp.stats().total_files, 0);
}

#[test]
fn broken_imports_reports_unresolved_but_keeps_good_edges() {
    let rp = resolve("broken-imports");
    assert_eq!(rp.config.layout.kind, ProjectKind::Foundry);

    let stats = rp.stats();
    assert_eq!(stats.resolved_imports, 1, "Good → Helper should resolve");
    assert_eq!(
        stats.unresolved_imports, 2,
        "missing.sol + @missing/Something.sol should fail to resolve",
    );
    assert_eq!(stats.files_with_unresolved, 1);

    // Display output surfaces the unresolved section with raw paths.
    let rendered = format!("{rp}");
    assert!(rendered.contains("Unresolved imports"));
    assert!(rendered.contains("./does-not-exist.sol"));
    assert!(rendered.contains("@missing/Something.sol"));
}

#[test]
fn monorepo_root_has_no_config_but_sees_solidity_somewhere() {
    // CP4's detector only looks at direct children for config files, so
    // the monorepo root has no config parses. It does, however, descend
    // to find `.sol` files in `packages/*/src/` — so the classification
    // is `Unknown` (sol present, no root config), not `NoSolidity`.
    let root = resolve("monorepo");
    assert_eq!(root.config.layout.kind, ProjectKind::Unknown);
    assert!(
        root.config.is_empty(),
        "no config parses at the root itself"
    );
}

#[test]
fn monorepo_foundry_subpackage_resolves_like_a_standalone_project() {
    let rp = resolve_project(&fixture("monorepo").join("packages/foundry-sub")).unwrap();
    assert_eq!(rp.config.layout.kind, ProjectKind::Foundry);
    assert_eq!(rp.enumeration.sources().count(), 1);
    assert!(rp.config.solc_versions().contains(&"0.8.20".to_string()));
}

#[test]
fn monorepo_hardhat_subpackage_resolves_like_a_standalone_project() {
    let rp = resolve_project(&fixture("monorepo").join("packages/hardhat-sub")).unwrap();
    assert_eq!(rp.config.layout.kind, ProjectKind::Hardhat);
    assert_eq!(rp.enumeration.sources().count(), 1);
    assert!(rp.config.solc_versions().contains(&"0.8.24".to_string()));
}

#[test]
fn every_fixture_resolves_without_panicking() {
    // Guard rail: every fixture in the directory must at least return
    // an `Ok(ResolvedProject)` — regression check for future fixture
    // additions.
    let names = [
        "foundry-minimal",
        "hardhat-minimal",
        "truffle-minimal",
        "mixed",
        "no-config",
        "empty",
        "broken-imports",
        "monorepo",
    ];
    for name in names {
        let _rp = resolve(name);
    }
}

#[test]
fn source_kind_tags_match_directory_roles() {
    // Foundry fixture has `src/` (Source) + `test/` (Test). Verify tags.
    let rp = resolve("foundry-minimal");
    let source_files: Vec<_> = rp.enumeration.sources().collect();
    let test_files: Vec<_> = rp.enumeration.tests().collect();
    assert!(source_files.iter().all(|f| f.kind == SourceKind::Source));
    assert!(test_files.iter().all(|f| f.kind == SourceKind::Test));
    assert!(source_files
        .iter()
        .any(|f| f.relative_path.ends_with("src/Token.sol")));
    assert!(source_files
        .iter()
        .any(|f| f.relative_path.ends_with("src/Wallet.sol")));
    assert!(test_files
        .iter()
        .any(|f| f.relative_path.ends_with("test/TokenTest.sol")));
}
