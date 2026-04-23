//! `foundry.toml` + `remappings.txt` parsing.
//!
//! Only the subset the auditor actually needs today: per-profile source
//! layout (`src`, `test`, `script`, `out`, `libs`), `solc` version,
//! `remappings`, and a handful of compile-relevant flags. Everything
//! else round-trips through `toml::Value` in [`FoundryProfile::extra`]
//! so we don't drop information the user set.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{error::ProjectError, layout::ProjectLayout};

/// Name of the default profile in `foundry.toml`.
pub const DEFAULT_PROFILE: &str = "default";

/// Parsed `foundry.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoundryConfig {
    /// Absolute path to the `foundry.toml` we parsed.
    pub path: PathBuf,
    /// Profile map, keyed by name. Always contains at least `default`
    /// (inserted empty if the file didn't declare one).
    pub profiles: BTreeMap<String, FoundryProfile>,
    /// Remappings declared at the top-level `[rpc_endpoints]` siblings
    /// — i.e. via `remappings.txt` on disk, merged in by the caller.
    /// Inline `profile.default.remappings` lives on the profile itself.
    #[serde(default)]
    pub external_remappings: Vec<Remapping>,
}

impl FoundryConfig {
    /// Return the named profile, falling back to `default` if missing.
    pub fn profile(&self, name: &str) -> Option<&FoundryProfile> {
        self.profiles
            .get(name)
            .or_else(|| self.profiles.get(DEFAULT_PROFILE))
    }

    /// Effective remappings: every inline remapping on the `default`
    /// profile, followed by the ones read from `remappings.txt`.
    /// De-duplicated by `(context, prefix)` — the first occurrence wins.
    pub fn effective_remappings(&self) -> Vec<Remapping> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let default_remaps = self
            .profiles
            .get(DEFAULT_PROFILE)
            .map_or(&[][..], |p| p.remappings.as_slice());
        for r in default_remaps.iter().chain(self.external_remappings.iter()) {
            let key = (r.context.clone(), r.prefix.clone());
            if seen.insert(key) {
                out.push(r.clone());
            }
        }
        out
    }
}

/// A single `[profile.<name>]` block.
///
/// Directories here are **relative to the project root** as written; the
/// caller joins them with `layout.root` when doing actual I/O.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FoundryProfile {
    #[serde(default)]
    pub src: Option<PathBuf>,
    #[serde(default)]
    pub test: Option<PathBuf>,
    #[serde(default)]
    pub script: Option<PathBuf>,
    #[serde(default)]
    pub out: Option<PathBuf>,
    #[serde(default)]
    pub libs: Vec<PathBuf>,
    /// `solc = "0.8.21"` or `solc_version = "..."`. Both spellings are
    /// accepted; the parser normalises into this field.
    #[serde(default)]
    pub solc: Option<String>,
    #[serde(default)]
    pub evm_version: Option<String>,
    #[serde(default)]
    pub optimizer: Option<bool>,
    #[serde(default)]
    pub optimizer_runs: Option<u64>,
    /// Inline remappings array. Parsed but not context-aware: each entry
    /// is `"prefix=target"` or `"context:prefix=target"`.
    #[serde(default)]
    pub remappings: Vec<Remapping>,
    /// Everything else from this profile, verbatim.
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

impl FoundryProfile {
    /// Effective source directory for this profile: the `src` key if set,
    /// otherwise Foundry's built-in default (`src`).
    pub fn src_dir(&self) -> PathBuf {
        self.src.clone().unwrap_or_else(|| PathBuf::from("src"))
    }
    pub fn test_dir(&self) -> PathBuf {
        self.test.clone().unwrap_or_else(|| PathBuf::from("test"))
    }
    pub fn script_dir(&self) -> PathBuf {
        self.script
            .clone()
            .unwrap_or_else(|| PathBuf::from("script"))
    }
    pub fn out_dir(&self) -> PathBuf {
        self.out.clone().unwrap_or_else(|| PathBuf::from("out"))
    }
    pub fn libs_dirs(&self) -> Vec<PathBuf> {
        if self.libs.is_empty() {
            vec![PathBuf::from("lib")]
        } else {
            self.libs.clone()
        }
    }
}

/// A single remapping. Matches the Foundry / Solc spec:
/// `[context:]prefix=target` (context is optional).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Remapping {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub prefix: String,
    pub target: String,
}

impl Remapping {
    /// Parse a single `[context:]prefix=target` line.
    ///
    /// Whitespace on either side is trimmed. Returns `None` for blanks
    /// and comment lines (`#`/`//`) so callers can run
    /// [`Remapping::parse`] over every line of a `remappings.txt`.
    pub fn parse(line: &str) -> Result<Option<Self>, String> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            return Ok(None);
        }
        let Some(eq) = line.find('=') else {
            return Err(format!("missing '=' in remapping: {line:?}"));
        };
        let (lhs, rhs) = line.split_at(eq);
        let target = rhs[1..].trim().to_string();
        if target.is_empty() {
            return Err(format!("empty remapping target: {line:?}"));
        }
        let lhs = lhs.trim();
        let (context, prefix) = match lhs.find(':') {
            Some(colon) => {
                let ctx = lhs[..colon].trim();
                let prefix = lhs[colon + 1..].trim();
                (
                    if ctx.is_empty() {
                        None
                    } else {
                        Some(ctx.to_string())
                    },
                    prefix.to_string(),
                )
            }
            None => (None, lhs.to_string()),
        };
        if prefix.is_empty() {
            return Err(format!("empty remapping prefix: {line:?}"));
        }
        Ok(Some(Self {
            context,
            prefix,
            target,
        }))
    }
}

/// Parse a `foundry.toml` file from disk.
///
/// Returns `Ok(None)` if the layout doesn't have a `foundry.toml`.
pub fn parse_foundry_config(layout: &ProjectLayout) -> Result<Option<FoundryConfig>, ProjectError> {
    let Some(path) = layout.foundry_toml() else {
        return Ok(None);
    };
    let bytes = fs::read_to_string(path).map_err(|e| ProjectError::io(path, e))?;
    let config = parse_foundry_toml(path, &bytes)?;
    Ok(Some(config))
}

/// Parse `foundry.toml` contents. Split out from [`parse_foundry_config`]
/// so tests don't need to touch the filesystem.
pub fn parse_foundry_toml(path: &Path, source: &str) -> Result<FoundryConfig, ProjectError> {
    let root: toml::Value = toml::from_str(source).map_err(|e| ProjectError::ParseFailed {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;

    let profiles_tbl = root
        .get("profile")
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();

    let mut profiles: BTreeMap<String, FoundryProfile> = BTreeMap::new();
    for (name, body) in profiles_tbl {
        let profile = profile_from_toml(path, &body)?;
        profiles.insert(name, profile);
    }
    // Foundry treats a missing [profile.default] as an empty default.
    profiles.entry(DEFAULT_PROFILE.to_string()).or_default();

    Ok(FoundryConfig {
        path: path.to_path_buf(),
        profiles,
        external_remappings: Vec::new(),
    })
}

fn profile_from_toml(path: &Path, v: &toml::Value) -> Result<FoundryProfile, ProjectError> {
    let Some(tbl) = v.as_table() else {
        return Err(ProjectError::ParseFailed {
            path: path.to_path_buf(),
            detail: "profile entry was not a table".into(),
        });
    };
    let mut profile = FoundryProfile::default();
    let mut extra: BTreeMap<String, toml::Value> = BTreeMap::new();

    for (k, val) in tbl {
        match k.as_str() {
            "src" => profile.src = string_to_path(val),
            "test" => profile.test = string_to_path(val),
            "script" => profile.script = string_to_path(val),
            "out" => profile.out = string_to_path(val),
            "libs" => profile.libs = array_of_paths(val),
            "solc" | "solc_version" => profile.solc = val.as_str().map(str::to_string),
            "evm_version" => profile.evm_version = val.as_str().map(str::to_string),
            "optimizer" => profile.optimizer = val.as_bool(),
            "optimizer_runs" => {
                profile.optimizer_runs = val.as_integer().and_then(|n| u64::try_from(n).ok());
            }
            "remappings" => {
                let arr = val.as_array().cloned().unwrap_or_default();
                for item in arr {
                    let Some(line) = item.as_str() else {
                        return Err(ProjectError::ParseFailed {
                            path: path.to_path_buf(),
                            detail: "remapping entry was not a string".into(),
                        });
                    };
                    match Remapping::parse(line) {
                        Ok(Some(r)) => profile.remappings.push(r),
                        Ok(None) => {}
                        Err(e) => {
                            return Err(ProjectError::ParseFailed {
                                path: path.to_path_buf(),
                                detail: e,
                            });
                        }
                    }
                }
            }
            _ => {
                extra.insert(k.clone(), val.clone());
            }
        }
    }
    profile.extra = extra;
    Ok(profile)
}

fn string_to_path(v: &toml::Value) -> Option<PathBuf> {
    v.as_str().map(PathBuf::from)
}

fn array_of_paths(v: &toml::Value) -> Vec<PathBuf> {
    v.as_array()
        .map(|arr| arr.iter().filter_map(string_to_path).collect())
        .unwrap_or_default()
}

/// Parse a `remappings.txt` file. Blank lines + `#`/`//` comments are
/// ignored; malformed entries produce a [`ProjectError::ParseFailed`].
pub fn parse_remappings_txt(path: &Path) -> Result<Vec<Remapping>, ProjectError> {
    let bytes = fs::read_to_string(path).map_err(|e| ProjectError::io(path, e))?;
    parse_remappings_str(path, &bytes)
}

/// Text-only variant of [`parse_remappings_txt`] for tests and embedded use.
pub fn parse_remappings_str(path: &Path, source: &str) -> Result<Vec<Remapping>, ProjectError> {
    let mut out = Vec::new();
    for (i, line) in source.lines().enumerate() {
        match Remapping::parse(line) {
            Ok(Some(r)) => out.push(r),
            Ok(None) => {}
            Err(e) => {
                return Err(ProjectError::ParseFailed {
                    path: path.to_path_buf(),
                    detail: format!("line {}: {e}", i + 1),
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r"
[profile.default]
";

    const RICH: &str = r#"
[profile.default]
src = "contracts"
test = "tests"
out = "artifacts"
libs = ["lib", "node_modules"]
solc = "0.8.21"
evm_version = "paris"
optimizer = true
optimizer_runs = 200
remappings = [
    "@openzeppelin/=lib/openzeppelin-contracts/",
    "forge-std/=lib/forge-std/src/",
    "   # ignored comment line is not valid toml, skip this",
]

[profile.ci]
solc = "0.8.25"
verbosity = 3

[rpc_endpoints]
mainnet = "https://eth.llamarpc.com"
"#;

    fn dummy_path() -> PathBuf {
        PathBuf::from("/tmp/foundry.toml")
    }

    #[test]
    fn minimal_foundry_toml_yields_empty_default_profile() {
        let cfg = parse_foundry_toml(&dummy_path(), MINIMAL).unwrap();
        assert!(cfg.profiles.contains_key(DEFAULT_PROFILE));
        let p = cfg.profile(DEFAULT_PROFILE).unwrap();
        assert!(p.src.is_none());
        assert_eq!(p.src_dir(), PathBuf::from("src"));
        assert_eq!(p.test_dir(), PathBuf::from("test"));
        assert_eq!(p.libs_dirs(), vec![PathBuf::from("lib")]);
    }

    #[test]
    fn rich_foundry_toml_parses_paths_and_solc() {
        // Drop the bogus comment line — TOML doesn't permit comments inside arrays as strings.
        let cleaned = RICH.replace(
            r#"    "   # ignored comment line is not valid toml, skip this","#,
            "",
        );
        let cfg = parse_foundry_toml(&dummy_path(), &cleaned).unwrap();
        let p = cfg.profile(DEFAULT_PROFILE).unwrap();
        assert_eq!(p.src, Some(PathBuf::from("contracts")));
        assert_eq!(p.test, Some(PathBuf::from("tests")));
        assert_eq!(p.out, Some(PathBuf::from("artifacts")));
        assert_eq!(
            p.libs,
            vec![PathBuf::from("lib"), PathBuf::from("node_modules")]
        );
        assert_eq!(p.solc.as_deref(), Some("0.8.21"));
        assert_eq!(p.evm_version.as_deref(), Some("paris"));
        assert_eq!(p.optimizer, Some(true));
        assert_eq!(p.optimizer_runs, Some(200));
        assert_eq!(p.remappings.len(), 2);
        assert_eq!(p.remappings[0].prefix, "@openzeppelin/");
        assert_eq!(p.remappings[0].target, "lib/openzeppelin-contracts/");
    }

    #[test]
    fn solc_version_alias_is_accepted() {
        let src = r#"
[profile.default]
solc_version = "0.8.17"
"#;
        let cfg = parse_foundry_toml(&dummy_path(), src).unwrap();
        assert_eq!(
            cfg.profile(DEFAULT_PROFILE).unwrap().solc.as_deref(),
            Some("0.8.17"),
        );
    }

    #[test]
    fn unknown_profile_keys_land_in_extra() {
        let src = r#"
[profile.default]
verbosity = 3
custom_flag = "on"
"#;
        let cfg = parse_foundry_toml(&dummy_path(), src).unwrap();
        let p = cfg.profile(DEFAULT_PROFILE).unwrap();
        assert!(p.extra.contains_key("verbosity"));
        assert!(p.extra.contains_key("custom_flag"));
    }

    #[test]
    fn multiple_profiles_all_parsed() {
        let src = r#"
[profile.default]
src = "src"

[profile.ci]
solc = "0.8.25"

[profile.deploy]
optimizer_runs = 1000000
"#;
        let cfg = parse_foundry_toml(&dummy_path(), src).unwrap();
        assert_eq!(cfg.profiles.len(), 3);
        assert_eq!(cfg.profile("ci").unwrap().solc.as_deref(), Some("0.8.25"),);
        assert_eq!(
            cfg.profile("deploy").unwrap().optimizer_runs,
            Some(1_000_000),
        );
    }

    #[test]
    fn missing_profile_default_is_inserted_empty() {
        let src = r#"[rpc_endpoints]
mainnet = "https://example"
"#;
        let cfg = parse_foundry_toml(&dummy_path(), src).unwrap();
        assert!(cfg.profile(DEFAULT_PROFILE).is_some());
    }

    #[test]
    fn malformed_toml_errors_with_path() {
        let err = parse_foundry_toml(&dummy_path(), "this is = not = valid = toml\n").unwrap_err();
        match err {
            ProjectError::ParseFailed { path, .. } => assert_eq!(path, dummy_path()),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn remapping_parse_accepts_context() {
        let r = Remapping::parse("test/:@std/=lib/std-test/")
            .unwrap()
            .unwrap();
        assert_eq!(r.context.as_deref(), Some("test/"));
        assert_eq!(r.prefix, "@std/");
        assert_eq!(r.target, "lib/std-test/");
    }

    #[test]
    fn remapping_parse_without_context() {
        let r = Remapping::parse("@oz/=lib/openzeppelin/").unwrap().unwrap();
        assert!(r.context.is_none());
        assert_eq!(r.prefix, "@oz/");
    }

    #[test]
    fn remapping_parse_skips_blanks_and_comments() {
        assert!(Remapping::parse("").unwrap().is_none());
        assert!(Remapping::parse("   ").unwrap().is_none());
        assert!(Remapping::parse("# this is a comment").unwrap().is_none());
        assert!(Remapping::parse("// js-style comment").unwrap().is_none());
    }

    #[test]
    fn remapping_parse_rejects_missing_equals() {
        assert!(Remapping::parse("@oz/").is_err());
    }

    #[test]
    fn remapping_parse_rejects_empty_target() {
        assert!(Remapping::parse("@oz/=").is_err());
    }

    #[test]
    fn parse_remappings_str_handles_multi_line() {
        let src = r"
# top comment
@oz/=lib/openzeppelin-contracts/contracts/
forge-std/=lib/forge-std/src/

// another style of comment
test/:@std/=lib/std-test/
";
        let rs = parse_remappings_str(Path::new("/tmp/remappings.txt"), src).unwrap();
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[2].context.as_deref(), Some("test/"));
    }

    #[test]
    fn parse_remappings_str_reports_line_number_on_error() {
        let src = "good/=lib/good/\nbroken line\n";
        let err = parse_remappings_str(Path::new("/tmp/r.txt"), src).unwrap_err();
        match err {
            ProjectError::ParseFailed { detail, .. } => {
                assert!(detail.contains("line 2"), "detail was {detail}");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn effective_remappings_merge_inline_then_external() {
        let mut cfg = parse_foundry_toml(
            &dummy_path(),
            r#"
[profile.default]
remappings = ["@oz/=lib/oz-inline/"]
"#,
        )
        .unwrap();
        cfg.external_remappings = vec![
            Remapping::parse("@oz/=lib/oz-external/").unwrap().unwrap(),
            Remapping::parse("ds-test/=lib/ds-test/src/")
                .unwrap()
                .unwrap(),
        ];
        let merged = cfg.effective_remappings();
        // Inline @oz/ wins over external @oz/.
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].target, "lib/oz-inline/");
        assert_eq!(merged[1].prefix, "ds-test/");
    }
}
