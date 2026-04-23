//! Target detector.
//!
//! Single public entry point — [`detect`] — that classifies an arbitrary
//! input string into a [`Target`]. Never fails: unrecognizable inputs return
//! [`Target::Unknown`] with a structured [`UnknownReason`].
//!
//! All logic is local: filesystem reads only, no network I/O.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use regex::Regex;
use url::Url;

use crate::{
    chain::Chain,
    target::{parse_address, GitRef, ProjectKind, Target, UnknownReason},
};

/// Classify `input` into a [`Target`].
///
/// `chain_hint` is only consulted when the input resolves to an on-chain
/// address. Defaults to [`Chain::EthereumMainnet`] when `None`.
pub fn detect(input: &str, chain_hint: Option<Chain>) -> Target {
    let trimmed = input.trim();

    if trimmed.is_empty() {
        return Target::Unknown {
            input: input.to_string(),
            reason: UnknownReason::Empty,
            suggestions: Vec::new(),
        };
    }

    if let Some(t) = try_detect_address(trimmed, chain_hint.as_ref()) {
        return t;
    }
    if let Some(t) = try_detect_github_url(trimmed) {
        return t;
    }
    if let Some(t) = try_detect_local_path(trimmed) {
        return t;
    }

    Target::Unknown {
        input: input.to_string(),
        reason: UnknownReason::Ambiguous {
            hints: vec![
                "not a 0x-prefixed hex address".to_string(),
                "not a recognized GitHub URL".to_string(),
                "no such file or directory".to_string(),
            ],
        },
        suggestions: vec![
            "If you meant a GitHub repo, try https://github.com/<owner>/<repo>".to_string(),
            "If you meant an on-chain address, prefix it with 0x".to_string(),
            "If you meant a local project, pass a path that exists".to_string(),
        ],
    }
}

/// Address detection. Returns `Some` if the input looks address-shaped
/// (whether it parses cleanly or not — errors surface as `Unknown`).
fn try_detect_address(input: &str, chain_hint: Option<&Chain>) -> Option<Target> {
    // 0x-prefixed branch: anything starting 0x/0X is treated as an address attempt.
    if let Some(body) = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
    {
        // Non-hex chars with leading 0x — still address-shaped, let parse_address error.
        if !body.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(malformed_address(
                input,
                format!("non-hex characters after 0x: {body:?}"),
            ));
        }
        if body.len() != 40 {
            return Some(malformed_address(
                input,
                format!("expected 40 hex chars after 0x, got {}", body.len()),
            ));
        }
        return Some(address_from_parse(input, chain_hint));
    }

    // Bare 40-hex-char branch: accept but warn.
    if input.len() == 40 && input.chars().all(|c| c.is_ascii_hexdigit()) {
        tracing::warn!(
            input = %input,
            "bare 40-char hex input accepted as an address; prefer a 0x prefix",
        );
        return Some(address_from_parse(input, chain_hint));
    }

    None
}

fn address_from_parse(input: &str, chain_hint: Option<&Chain>) -> Target {
    match parse_address(input) {
        Ok(address) => Target::OnChain {
            address,
            chain: chain_hint.cloned().unwrap_or_default(),
        },
        Err(e) => malformed_address(input, e.to_string()),
    }
}

fn malformed_address(input: &str, detail: String) -> Target {
    Target::Unknown {
        input: input.to_string(),
        reason: UnknownReason::MalformedAddress { detail },
        suggestions: vec!["An address is 40 hex characters prefixed with 0x".to_string()],
    }
}

/// GitHub URL detection. Recognizes the shapes listed in the spec. Returns
/// `Some` for anything that looks URL-shaped (unsupported hosts included);
/// returns `None` for inputs that clearly aren't URLs at all.
fn try_detect_github_url(input: &str) -> Option<Target> {
    // SSH form: git@github.com:owner/repo(.git)?
    if let Some(t) = try_ssh_github(input) {
        return Some(t);
    }

    // Normalize schemeless github.com/... to https://github.com/...
    let url_str: String = if input.starts_with("http://") || input.starts_with("https://") {
        input.to_string()
    } else if input.starts_with("github.com/")
        || input.starts_with("gitlab.com/")
        || input.starts_with("bitbucket.org/")
        || input.starts_with("www.github.com/")
    {
        format!("https://{input}")
    } else {
        return None;
    };

    let url = match Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => {
            return Some(Target::Unknown {
                input: input.to_string(),
                reason: UnknownReason::MalformedUrl {
                    detail: e.to_string(),
                },
                suggestions: Vec::new(),
            });
        }
    };

    let host = match url.host_str() {
        Some(h) => h.trim_start_matches("www.").to_ascii_lowercase(),
        None => {
            return Some(Target::Unknown {
                input: input.to_string(),
                reason: UnknownReason::MalformedUrl {
                    detail: "missing host".to_string(),
                },
                suggestions: Vec::new(),
            });
        }
    };

    if host != "github.com" {
        return Some(Target::Unknown {
            input: input.to_string(),
            reason: UnknownReason::UnsupportedUrlHost { host: host.clone() },
            suggestions: vec![format!(
                "Only GitHub is supported in Phase 1. File an issue if you need {host}."
            )],
        });
    }

    Some(parse_github_path(input, &url))
}

fn try_ssh_github(input: &str) -> Option<Target> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // git@github.com:<owner>/<repo>[.git]
        Regex::new(r"^git@github\.com:([^/\s]+)/([^/\s]+?)(?:\.git)?$").unwrap()
    });
    let caps = re.captures(input)?;
    Some(Target::Github {
        owner: caps[1].to_string(),
        repo: caps[2].to_string(),
        reference: None,
        subpath: None,
    })
}

fn parse_github_path(input: &str, url: &Url) -> Target {
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();

    if segments.len() < 2 {
        return Target::Unknown {
            input: input.to_string(),
            reason: UnknownReason::MalformedUrl {
                detail: "GitHub URL must include /<owner>/<repo>".to_string(),
            },
            suggestions: Vec::new(),
        };
    }

    let owner = segments[0].to_string();
    let repo = segments[1].trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        return Target::Unknown {
            input: input.to_string(),
            reason: UnknownReason::MalformedUrl {
                detail: "empty owner or repo".to_string(),
            },
            suggestions: Vec::new(),
        };
    }

    let (reference, subpath) = match segments.get(2).copied() {
        None => (None, None),
        Some("commit") => {
            let sha = segments.get(3).copied().unwrap_or_default();
            (Some(classify_ref(sha, RefKind::Commit)), None)
        }
        Some("tree" | "blob") => {
            // /tree/refs/heads/<branch>, /tree/refs/tags/<tag>, or /tree/<ref>[/<subpath>...]
            if segments.get(3).copied() == Some("refs") && segments.len() >= 6 {
                let kind = segments[4];
                let name = segments[5].to_string();
                let subpath = build_subpath(&segments[6..]);
                let reference = match kind {
                    "heads" => Some(GitRef::Branch(name)),
                    "tags" => Some(GitRef::Tag(name)),
                    _ => Some(classify_ref(&name, RefKind::Ambiguous)),
                };
                (reference, subpath)
            } else {
                let ref_name = segments.get(3).copied().unwrap_or_default();
                let subpath = build_subpath(&segments[4.min(segments.len())..]);
                (Some(classify_ref(ref_name, RefKind::Ambiguous)), subpath)
            }
        }
        Some(_) => {
            // Unrecognized third segment (issues, pulls, actions, etc.) — degrade
            // to a bare repo reference. The user pointed at the project; we don't
            // need to understand the sub-URL to know which repo they meant.
            (None, None)
        }
    };

    Target::Github {
        owner,
        repo,
        reference,
        subpath,
    }
}

#[derive(Copy, Clone)]
enum RefKind {
    Commit,
    Ambiguous,
}

fn classify_ref(name: &str, default: RefKind) -> GitRef {
    if is_commit_sha(name) {
        return GitRef::Commit(name.to_string());
    }
    match default {
        RefKind::Commit => GitRef::Commit(name.to_string()),
        RefKind::Ambiguous => GitRef::Ambiguous(name.to_string()),
    }
}

fn is_commit_sha(s: &str) -> bool {
    let len = s.len();
    (7..=40).contains(&len) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn build_subpath(segments: &[&str]) -> Option<PathBuf> {
    if segments.is_empty() {
        return None;
    }
    let joined: PathBuf = segments.iter().collect();
    Some(joined)
}

/// Local path detection.
///
/// Treats inputs that look path-shaped or that exist on the filesystem as
/// candidates; canonicalizes (follows symlinks) and classifies project kind.
fn try_detect_local_path(input: &str) -> Option<Target> {
    let expanded = expand_tilde(input);
    let candidate = Path::new(&expanded);

    if !(looks_path_shaped(input) || candidate.exists()) {
        return None;
    }

    let Ok(canonical) = fs::canonicalize(candidate) else {
        return Some(Target::Unknown {
            input: input.to_string(),
            reason: UnknownReason::PathDoesNotExist,
            suggestions: Vec::new(),
        });
    };

    if !canonical.is_dir() {
        return Some(Target::Unknown {
            input: input.to_string(),
            reason: UnknownReason::PathNotADirectory,
            suggestions: vec!["Pass the containing project directory".to_string()],
        });
    }

    let project_kind = classify_project(&canonical);
    Some(Target::LocalPath {
        root: canonical,
        project_kind,
    })
}

fn expand_tilde(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    input.to_string()
}

fn looks_path_shaped(input: &str) -> bool {
    input.starts_with('/')
        || input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with("~/")
        || is_windows_drive_prefix(input)
}

fn is_windows_drive_prefix(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn classify_project(root: &Path) -> ProjectKind {
    let mut found: Vec<ProjectKind> = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return ProjectKind::NoSolidity;
    };

    let mut has_foundry = false;
    let mut has_hardhat = false;
    let mut has_truffle = false;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        match n {
            "foundry.toml" => has_foundry = true,
            "hardhat.config.js" | "hardhat.config.ts" | "hardhat.config.cjs"
            | "hardhat.config.mjs" => has_hardhat = true,
            "truffle-config.js" => has_truffle = true,
            _ => {}
        }
    }

    if has_foundry {
        found.push(ProjectKind::Foundry);
    }
    if has_hardhat {
        found.push(ProjectKind::Hardhat);
    }
    if has_truffle {
        found.push(ProjectKind::Truffle);
    }

    match found.len() {
        0 => {
            if contains_sol_file(root) {
                ProjectKind::Unknown
            } else {
                ProjectKind::NoSolidity
            }
        }
        1 => found.into_iter().next().unwrap(),
        _ => ProjectKind::Mixed(found),
    }
}

/// Recursively search `root` for any `.sol` file, skipping build/dep dirs.
fn contains_sol_file(root: &Path) -> bool {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const VITALIK_LOWER: &str = "0xfb6916095ca1df60bb79ce92ce3ea74c37c5d359";
    const VITALIK_CHECKSUM: &str = "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359";

    fn assert_unknown_reason(t: &Target, matches: impl Fn(&UnknownReason) -> bool) {
        match t {
            Target::Unknown { reason, .. } => assert!(matches(reason), "got reason {reason:?}"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    // 1-4: empty / whitespace / lone 0 / lone 0x
    #[test]
    fn empty_input_is_unknown_empty() {
        assert_unknown_reason(&detect("", None), |r| matches!(r, UnknownReason::Empty));
    }

    #[test]
    fn whitespace_input_is_unknown_empty() {
        assert_unknown_reason(&detect("   \t\n  ", None), |r| {
            matches!(r, UnknownReason::Empty)
        });
    }

    #[test]
    fn lone_zero_falls_through_to_ambiguous() {
        assert_unknown_reason(&detect("0", None), |r| {
            matches!(r, UnknownReason::Ambiguous { .. })
        });
    }

    #[test]
    fn lone_0x_is_malformed_address() {
        assert_unknown_reason(&detect("0x", None), |r| {
            matches!(r, UnknownReason::MalformedAddress { .. })
        });
    }

    // 5-7: valid addresses
    #[test]
    fn valid_checksum_address_no_hint_defaults_mainnet() {
        match detect(VITALIK_CHECKSUM, None) {
            Target::OnChain { chain, .. } => assert_eq!(chain, Chain::EthereumMainnet),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn valid_checksum_address_with_hint() {
        match detect(VITALIK_CHECKSUM, Some(Chain::Arbitrum)) {
            Target::OnChain { chain, .. } => assert_eq!(chain, Chain::Arbitrum),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn lowercase_address_accepted_without_checksum_enforcement() {
        match detect(VITALIK_LOWER, None) {
            Target::OnChain { address, .. } => {
                assert_eq!(address.to_string(), VITALIK_CHECKSUM);
            }
            other => panic!("got {other:?}"),
        }
    }

    // 8-10: address length / hex errors
    #[test]
    fn address_39_chars_is_malformed() {
        let short = "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d35";
        assert_unknown_reason(&detect(short, None), |r| {
            matches!(r, UnknownReason::MalformedAddress { .. })
        });
    }

    #[test]
    fn address_41_chars_is_malformed() {
        let long = "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d35900";
        assert_unknown_reason(&detect(long, None), |r| {
            matches!(r, UnknownReason::MalformedAddress { .. })
        });
    }

    #[test]
    fn address_non_hex_is_malformed() {
        let bad = "0xZZ6916095ca1df60bB79Ce92cE3Ea74c37c5d359";
        assert_unknown_reason(&detect(bad, None), |r| {
            matches!(r, UnknownReason::MalformedAddress { .. })
        });
    }

    #[test]
    fn address_bad_checksum_is_malformed() {
        let broken = "0xFB6916095ca1df60bB79Ce92cE3Ea74c37c5d359";
        assert_unknown_reason(&detect(broken, None), |r| {
            matches!(r, UnknownReason::MalformedAddress { .. })
        });
    }

    #[test]
    fn bare_40_hex_accepted() {
        let bare = "fB6916095ca1df60bB79Ce92cE3Ea74c37c5d359";
        assert!(matches!(detect(bare, None), Target::OnChain { .. }));
    }

    // 12-19: GitHub URL shapes
    #[test]
    fn github_https_plain() {
        match detect("https://github.com/foundry-rs/foundry", None) {
            Target::Github {
                owner,
                repo,
                reference,
                subpath,
            } => {
                assert_eq!(owner, "foundry-rs");
                assert_eq!(repo, "foundry");
                assert!(reference.is_none() && subpath.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_https_with_dot_git() {
        match detect("https://github.com/foundry-rs/foundry.git", None) {
            Target::Github { repo, .. } => assert_eq!(repo, "foundry"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_tree_branch_ambiguous() {
        match detect("https://github.com/aave/aave-v3-core/tree/main", None) {
            Target::Github {
                reference: Some(GitRef::Ambiguous(name)),
                subpath,
                ..
            } => {
                assert_eq!(name, "main");
                assert!(subpath.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_tree_with_subpath() {
        match detect(
            "https://github.com/aave/aave-v3-core/tree/main/contracts/protocol",
            None,
        ) {
            Target::Github {
                reference: Some(GitRef::Ambiguous(r)),
                subpath: Some(sp),
                ..
            } => {
                assert_eq!(r, "main");
                assert_eq!(sp.to_str().unwrap(), "contracts/protocol");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_blob_url_with_file_subpath() {
        match detect(
            "https://github.com/foundry-rs/foundry/blob/main/README.md",
            None,
        ) {
            Target::Github {
                reference: Some(GitRef::Ambiguous(r)),
                subpath: Some(sp),
                ..
            } => {
                assert_eq!(r, "main");
                assert_eq!(sp.to_str().unwrap(), "README.md");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_commit_url_is_commit_ref() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let url = format!("https://github.com/foo/bar/commit/{sha}");
        match detect(&url, None) {
            Target::Github {
                reference: Some(GitRef::Commit(s)),
                ..
            } => assert_eq!(s, sha),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_tree_refs_heads_is_branch() {
        match detect("https://github.com/foo/bar/tree/refs/heads/develop", None) {
            Target::Github {
                reference: Some(GitRef::Branch(b)),
                ..
            } => assert_eq!(b, "develop"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_tree_refs_tags_is_tag() {
        match detect("https://github.com/foo/bar/tree/refs/tags/v1.2.3", None) {
            Target::Github {
                reference: Some(GitRef::Tag(t)),
                ..
            } => assert_eq!(t, "v1.2.3"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_tree_with_sha_classifies_as_commit() {
        let sha = "abcdef1234567";
        let url = format!("https://github.com/foo/bar/tree/{sha}");
        match detect(&url, None) {
            Target::Github {
                reference: Some(GitRef::Commit(c)),
                ..
            } => assert_eq!(c, sha),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_ssh_form() {
        match detect("git@github.com:foundry-rs/foundry.git", None) {
            Target::Github { owner, repo, .. } => {
                assert_eq!(owner, "foundry-rs");
                assert_eq!(repo, "foundry");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_issues_url_degrades_to_bare_repo() {
        match detect("https://github.com/foundry-rs/foundry/issues/1234", None) {
            Target::Github {
                owner,
                repo,
                reference,
                subpath,
            } => {
                assert_eq!(owner, "foundry-rs");
                assert_eq!(repo, "foundry");
                assert!(reference.is_none());
                assert!(subpath.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn github_schemeless() {
        match detect("github.com/foundry-rs/foundry", None) {
            Target::Github { owner, repo, .. } => {
                assert_eq!(owner, "foundry-rs");
                assert_eq!(repo, "foundry");
            }
            other => panic!("got {other:?}"),
        }
    }

    // 20-22: unsupported hosts / malformed URLs
    #[test]
    fn gitlab_url_is_unsupported_host() {
        assert_unknown_reason(
            &detect("https://gitlab.com/group/project", None),
            |r| matches!(r, UnknownReason::UnsupportedUrlHost { host } if host == "gitlab.com"),
        );
    }

    #[test]
    fn bitbucket_url_is_unsupported_host() {
        assert_unknown_reason(
            &detect("https://bitbucket.org/team/project", None),
            |r| matches!(r, UnknownReason::UnsupportedUrlHost { host } if host == "bitbucket.org"),
        );
    }

    #[test]
    fn github_url_missing_repo_is_malformed() {
        assert_unknown_reason(&detect("https://github.com/owner-only", None), |r| {
            matches!(r, UnknownReason::MalformedUrl { .. })
        });
    }

    // 23-26: local paths
    #[test]
    fn local_path_existing_empty_dir_is_no_solidity() {
        let tmp = TempDir::new().unwrap();
        match detect(tmp.path().to_str().unwrap(), None) {
            Target::LocalPath { project_kind, .. } => {
                assert_eq!(project_kind, ProjectKind::NoSolidity);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn local_path_nonexistent_is_path_does_not_exist() {
        // Use a prefixed path so we don't fall through to Ambiguous.
        assert_unknown_reason(&detect("/definitely/not/a/real/basilisk/path", None), |r| {
            matches!(r, UnknownReason::PathDoesNotExist)
        });
    }

    #[test]
    fn local_path_file_not_dir_is_path_not_a_directory() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("file.txt");
        fs::write(&f, b"hi").unwrap();
        assert_unknown_reason(&detect(f.to_str().unwrap(), None), |r| {
            matches!(r, UnknownReason::PathNotADirectory)
        });
    }

    // 27-31: project-kind classification
    #[test]
    fn local_path_foundry_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("foundry.toml"), b"[profile.default]\n").unwrap();
        match detect(tmp.path().to_str().unwrap(), None) {
            Target::LocalPath { project_kind, .. } => {
                assert_eq!(project_kind, ProjectKind::Foundry);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn local_path_hardhat_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("hardhat.config.ts"), b"export default {};").unwrap();
        match detect(tmp.path().to_str().unwrap(), None) {
            Target::LocalPath { project_kind, .. } => {
                assert_eq!(project_kind, ProjectKind::Hardhat);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn local_path_truffle_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("truffle-config.js"),
            b"module.exports = {};",
        )
        .unwrap();
        match detect(tmp.path().to_str().unwrap(), None) {
            Target::LocalPath { project_kind, .. } => {
                assert_eq!(project_kind, ProjectKind::Truffle);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn local_path_mixed_project() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("foundry.toml"), b"").unwrap();
        fs::write(tmp.path().join("hardhat.config.js"), b"").unwrap();
        match detect(tmp.path().to_str().unwrap(), None) {
            Target::LocalPath {
                project_kind: ProjectKind::Mixed(kinds),
                ..
            } => {
                assert!(kinds.contains(&ProjectKind::Foundry));
                assert!(kinds.contains(&ProjectKind::Hardhat));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn local_path_bare_sol_is_unknown_kind() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/A.sol"),
            b"// SPDX-License-Identifier: MIT\n",
        )
        .unwrap();
        match detect(tmp.path().to_str().unwrap(), None) {
            Target::LocalPath { project_kind, .. } => {
                assert_eq!(project_kind, ProjectKind::Unknown);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn local_path_skip_dirs_respected() {
        // .sol only inside node_modules should NOT count — NoSolidity.
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("node_modules")).unwrap();
        fs::write(tmp.path().join("node_modules/X.sol"), b"").unwrap();
        match detect(tmp.path().to_str().unwrap(), None) {
            Target::LocalPath { project_kind, .. } => {
                assert_eq!(project_kind, ProjectKind::NoSolidity);
            }
            other => panic!("got {other:?}"),
        }
    }

    // 32-34: ambiguous fallback
    #[test]
    fn ambiguous_plain_word() {
        assert_unknown_reason(&detect("aave", None), |r| {
            matches!(r, UnknownReason::Ambiguous { .. })
        });
    }

    #[test]
    fn ambiguous_short_hex_0x1234_is_malformed_address() {
        // "0x1234" starts with 0x — routed to the address branch, wrong length.
        assert_unknown_reason(&detect("0x1234", None), |r| {
            matches!(r, UnknownReason::MalformedAddress { .. })
        });
    }

    #[test]
    fn ambiguous_random_text() {
        assert_unknown_reason(&detect("hello world", None), |r| {
            matches!(r, UnknownReason::Ambiguous { .. })
        });
    }
}
