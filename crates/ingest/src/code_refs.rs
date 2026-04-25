//! GitHub blob-URL extraction shared by GitHub-clone ingesters.
//!
//! Code4rena, Sherlock, and similar findings-style sources cite the
//! vulnerable code by linking to a GitHub blob URL with `#Lnnn` or
//! `#Lnnn-Lmmm` line refs:
//!
//! ```text
//! https://github.com/code-423n4/2023-05-ajna/blob/<sha>/ajna-core/src/PositionManager.sol#L262-L323
//! ```
//!
//! Pulling the snippet itself at ingest time would mean a second
//! shallow clone per contest (the codebase repo, no `-findings`
//! suffix). The list of contests is in the hundreds, so doubling the
//! clone surface is heavy. Instead, this extractor parses the URLs
//! out of the finding body at ingest time, stores them as structured
//! metadata on the `IngestRecord`, and leaves the actual snippet
//! fetch lazy: a retrieval-time consumer (CLI / agent tool) clones
//! the codebase repo on demand and reads the lines.

use serde::{Deserialize, Serialize};

/// One GitHub blob reference parsed out of a finding body.
///
/// `git_ref` is the URL path segment immediately after `/blob/`.
/// In Code4rena reports it's almost always a commit sha (the audit
/// snapshot); in newer Sherlock reports it can be a branch like
/// `main`. The lazy fetcher uses it verbatim with `RepoCache`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeRef {
    pub owner: String,
    pub repo: String,
    pub git_ref: String,
    pub path: String,
    pub line_start: u32,
    /// Inclusive end. `None` means single-line ref (`#L123`).
    pub line_end: Option<u32>,
}

impl CodeRef {
    /// `github.com/<owner>/<repo>` — useful for grouping refs by
    /// codebase repo before fetching.
    #[must_use]
    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Reconstruct the URL the ref came from. Round-trips
    /// `extract_code_refs` on the produced URL.
    #[must_use]
    pub fn url(&self) -> String {
        let lines = match self.line_end {
            Some(e) => format!("L{}-L{}", self.line_start, e),
            None => format!("L{}", self.line_start),
        };
        format!(
            "https://github.com/{}/{}/blob/{}/{}#{lines}",
            self.owner, self.repo, self.git_ref, self.path,
        )
    }
}

/// Extract every GitHub blob-URL `(file, line)` reference from a
/// chunk of markdown / plaintext. Order is preserved; duplicates
/// are de-duped (same `(owner, repo, ref, path, start, end)` tuple
/// only appears once even if cited multiple times in the body).
#[must_use]
pub fn extract_code_refs(body: &str) -> Vec<CodeRef> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // The path component is everything between `<ref>/` and
        // `#L<digits>` — non-greedy so a trailing `>)` or whitespace
        // doesn't get sucked in. URL-safe path chars only.
        regex::Regex::new(
            r"https?://github\.com/(?P<owner>[\w.\-]+)/(?P<repo>[\w.\-]+)/blob/(?P<ref>[\w.\-]+)/(?P<path>[\w.\-/]+?)#L(?P<start>\d+)(?:-L(?P<end>\d+))?",
        )
        .expect("static regex compiles")
    });

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for caps in re.captures_iter(body) {
        let owner = caps.name("owner").unwrap().as_str().to_string();
        let repo = caps.name("repo").unwrap().as_str().to_string();
        let git_ref = caps.name("ref").unwrap().as_str().to_string();
        let path = caps.name("path").unwrap().as_str().to_string();
        let line_start: u32 = match caps.name("start").unwrap().as_str().parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let line_end = caps
            .name("end")
            .and_then(|m| m.as_str().parse::<u32>().ok());
        let key = (
            owner.clone(),
            repo.clone(),
            git_ref.clone(),
            path.clone(),
            line_start,
            line_end,
        );
        if seen.insert(key) {
            out.push(CodeRef {
                owner,
                repo,
                git_ref,
                path,
                line_start,
                line_end,
            });
        }
    }
    out
}

/// Lazily fetch the source-code snippet a [`CodeRef`] points at.
///
/// Clones the codebase repo via `RepoCache` (shallow, anonymous —
/// callers can pass an authenticated `GithubClient` when rate-
/// limited) and reads the requested lines out of the working tree.
/// Returns the raw text. The caller decides whether to embed,
/// display, or just count it.
///
/// This is the deferred half of the pattern documented at the top
/// of the module: ingest captures refs cheaply, retrieval pulls
/// the snippet on demand.
///
/// # Errors
///
/// Returns [`IngestError::Source`] if the clone fails or the file
/// is missing; [`IngestError::Other`] if line numbers fall outside
/// the file.
pub async fn fetch_snippet(
    cache: &basilisk_git::RepoCache,
    code_ref: &CodeRef,
    github: Option<basilisk_github::GithubClient>,
) -> Result<String, crate::error::IngestError> {
    use crate::error::IngestError;

    let opts = basilisk_git::FetchOptions {
        strategy: basilisk_git::CloneStrategy::Shallow,
        force_refresh: false,
        github,
    };
    let fetched = cache
        .fetch(
            &code_ref.owner,
            &code_ref.repo,
            Some(basilisk_core::GitRef::Branch(code_ref.git_ref.clone())),
            opts,
        )
        .await
        .map_err(|e| IngestError::Source(format!("clone {}/{}: {e}", code_ref.owner, code_ref.repo)))?;
    let path = fetched.working_tree.join(&code_ref.path);
    let body = std::fs::read_to_string(&path).map_err(|e| {
        IngestError::Source(format!("read {}: {e}", path.display()))
    })?;
    let lines: Vec<&str> = body.lines().collect();
    let start = code_ref.line_start.saturating_sub(1) as usize;
    let end = code_ref
        .line_end
        .map_or(start + 1, |e| e as usize)
        .min(lines.len());
    if start >= lines.len() {
        return Err(IngestError::Other(format!(
            "{}#L{}: file has only {} lines",
            code_ref.path,
            code_ref.line_start,
            lines.len()
        )));
    }
    Ok(lines[start..end].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_line_ref() {
        let body =
            "see <https://github.com/code-423n4/2023-05-ajna/blob/abc123/contracts/Foo.sol#L42>";
        let refs = extract_code_refs(body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].owner, "code-423n4");
        assert_eq!(refs[0].repo, "2023-05-ajna");
        assert_eq!(refs[0].git_ref, "abc123");
        assert_eq!(refs[0].path, "contracts/Foo.sol");
        assert_eq!(refs[0].line_start, 42);
        assert_eq!(refs[0].line_end, None);
    }

    #[test]
    fn extracts_line_range() {
        let body = "https://github.com/code-423n4/2023-05-ajna/blob/276942bc2f97488d07b887c8edceaaab7a5c3964/ajna-core/src/PositionManager.sol#L262-L323";
        let refs = extract_code_refs(body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].line_start, 262);
        assert_eq!(refs[0].line_end, Some(323));
        assert_eq!(refs[0].path, "ajna-core/src/PositionManager.sol");
    }

    #[test]
    fn extracts_multiple_distinct_refs() {
        let body = "
            see [PoolHelper.sol#L222-L236](https://github.com/code-423n4/2023-05-ajna/blob/276942bc2f97488d07b887c8edceaaab7a5c3964/ajna-core/src/libraries/helpers/PoolHelper.sol#L222-L236)
            and [LenderActions.sol#L711](https://github.com/code-423n4/2023-05-ajna/blob/276942bc2f97488d07b887c8edceaaab7a5c3964/ajna-core/src/libraries/external/LenderActions.sol#L711)
        ";
        let refs = extract_code_refs(body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].path, "ajna-core/src/libraries/helpers/PoolHelper.sol");
        assert_eq!(refs[0].line_end, Some(236));
        assert_eq!(refs[1].path, "ajna-core/src/libraries/external/LenderActions.sol");
        assert_eq!(refs[1].line_end, None);
    }

    #[test]
    fn dedupes_repeated_citations() {
        let body = "
            first cite https://github.com/x/y/blob/main/a.sol#L1
            same again https://github.com/x/y/blob/main/a.sol#L1
            different line https://github.com/x/y/blob/main/a.sol#L2
        ";
        let refs = extract_code_refs(body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].line_start, 1);
        assert_eq!(refs[1].line_start, 2);
    }

    #[test]
    fn ignores_non_blob_github_urls() {
        let body = "
            issue link https://github.com/code-423n4/2023-05-ajna/issues/42
            tree link https://github.com/code-423n4/2023-05-ajna/tree/main/contracts
            blob ok https://github.com/code-423n4/2023-05-ajna/blob/main/contracts/Foo.sol#L1
        ";
        let refs = extract_code_refs(body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "contracts/Foo.sol");
    }

    #[test]
    fn url_round_trips() {
        let original = "https://github.com/code-423n4/2023-05-ajna/blob/276942bc2f97488d07b887c8edceaaab7a5c3964/ajna-core/src/PositionManager.sol#L262-L323";
        let refs = extract_code_refs(original);
        assert_eq!(refs[0].url(), original);
    }

    #[test]
    fn repo_slug_matches_clone_owner_repo() {
        let body = "https://github.com/code-423n4/2024-04-renzo/blob/abc/contracts/X.sol#L1";
        let refs = extract_code_refs(body);
        assert_eq!(refs[0].repo_slug(), "code-423n4/2024-04-renzo");
    }

    #[test]
    fn empty_body_returns_no_refs() {
        let refs = extract_code_refs("nothing here");
        assert!(refs.is_empty());
    }

    #[test]
    fn ignores_malformed_line_anchor() {
        // No `#L<digits>` — not a code ref.
        let body = "https://github.com/x/y/blob/main/a.sol";
        let refs = extract_code_refs(body);
        assert!(refs.is_empty());
    }
}
