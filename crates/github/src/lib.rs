//! Thin GitHub REST v3 client.
//!
//! We wrap the four endpoints the rest of Basilisk needs:
//! - `GET /repos/{owner}/{repo}` for default-branch + existence checks
//! - `GET /repos/{owner}/{repo}/branches/{branch}` to test a name as a branch
//! - `GET /repos/{owner}/{repo}/git/ref/tags/{tag}` to test a name as a tag
//! - `GET /repos/{owner}/{repo}/commits/{sha}` for short-SHA expansion
//!
//! Everything goes through one [`GithubClient`]. Token auth is `Authorization:
//! Bearer <token>` when supplied. Responses for `resolve_ref` and
//! `default_branch` are cached under the `github` namespace with a 1-hour TTL.

use std::time::{Duration, SystemTime};

use basilisk_cache::Cache;
use basilisk_core::GitRef;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default GitHub API host.
pub const DEFAULT_BASE: &str = "https://api.github.com";

/// Cache namespace for GitHub lookups.
pub const CACHE_NAMESPACE: &str = "github";

/// TTL for cached ref + default-branch lookups.
pub const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Typed errors surfaced to callers.
#[derive(Debug, Clone, Error)]
pub enum GithubError {
    /// A 404 from the API (repo / ref / commit doesn't exist).
    #[error("not found: {what}")]
    NotFound { what: String },

    /// Token missing or invalid.
    #[error("unauthorized (token missing or invalid)")]
    Unauthorized,

    /// GitHub said 403 with rate-limit headers.
    #[error("rate limited by GitHub; resets at {reset_at:?}")]
    RateLimited { reset_at: SystemTime },

    /// Transport-level failure.
    #[error("network error: {0}")]
    NetworkError(String),

    /// Anything else.
    #[error("GitHub error: {0}")]
    Other(String),
}

/// GitHub REST client.
#[derive(Debug, Clone)]
pub struct GithubClient {
    base: String,
    token: Option<String>,
    client: reqwest::Client,
    cache: Option<Cache>,
}

impl GithubClient {
    /// Construct a client, optionally with a personal-access token.
    pub fn new(token: Option<&str>) -> Result<Self, GithubError> {
        Self::new_with_base(DEFAULT_BASE, token)
    }

    /// Construct against an explicit base URL (used by wiremock tests).
    pub fn new_with_base(
        base: impl Into<String>,
        token: Option<&str>,
    ) -> Result<Self, GithubError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("basilisk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| GithubError::Other(e.to_string()))?;
        Ok(Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            client,
            cache: Cache::open(CACHE_NAMESPACE).ok(),
        })
    }

    /// Build a test-only client with caching disabled.
    #[must_use]
    pub fn without_cache(mut self) -> Self {
        self.cache = None;
        self
    }

    /// Returns `true` iff the client was constructed with a non-empty token.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Given an ambiguous ref (branch-or-tag string), resolve to a concrete
    /// [`GitRef::Branch`] or [`GitRef::Tag`]. GitHub's own convention prefers
    /// branches when both exist — we mirror that.
    pub async fn resolve_ref(
        &self,
        owner: &str,
        repo: &str,
        ambiguous: &str,
    ) -> Result<GitRef, GithubError> {
        let key = format!("ref:{owner}/{repo}:{ambiguous}");
        if let Some(cache) = &self.cache {
            if let Ok(Some(hit)) = cache.get::<GitRef>(&key).await {
                return Ok(hit.value);
            }
        }

        // Try branch first.
        let branch_url = format!(
            "{}/repos/{}/{}/branches/{}",
            self.base, owner, repo, ambiguous
        );
        let branch_status = self.head_status(&branch_url).await?;
        let resolved = if is_success(branch_status) {
            GitRef::Branch(ambiguous.to_string())
        } else if is_not_found(branch_status) {
            let tag_url = format!(
                "{}/repos/{}/{}/git/ref/tags/{}",
                self.base, owner, repo, ambiguous
            );
            let tag_status = self.head_status(&tag_url).await?;
            if is_success(tag_status) {
                GitRef::Tag(ambiguous.to_string())
            } else if is_not_found(tag_status) {
                return Err(GithubError::NotFound {
                    what: format!("{owner}/{repo}@{ambiguous} (neither branch nor tag)"),
                });
            } else {
                return Err(status_to_err(tag_status, "tag lookup"));
            }
        } else {
            return Err(status_to_err(branch_status, "branch lookup"));
        };

        if let Some(cache) = &self.cache {
            let _ = cache.put(&key, &resolved, CACHE_TTL).await;
        }
        Ok(resolved)
    }

    /// Expand a short commit SHA (7-11 chars) to the full 40-char form.
    pub async fn resolve_short_sha(
        &self,
        owner: &str,
        repo: &str,
        short: &str,
    ) -> Result<String, GithubError> {
        let url = format!("{}/repos/{}/{}/commits/{}", self.base, owner, repo, short);
        let text = self.get_body(&url).await?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| GithubError::Other(format!("commits response JSON: {e}")))?;
        v.get("sha")
            .and_then(serde_json::Value::as_str)
            .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_string)
            .ok_or_else(|| GithubError::Other("commit response missing or invalid sha".into()))
    }

    /// Get the default branch of a repo.
    pub async fn default_branch(&self, owner: &str, repo: &str) -> Result<String, GithubError> {
        let key = format!("default:{owner}/{repo}");
        if let Some(cache) = &self.cache {
            if let Ok(Some(hit)) = cache.get::<String>(&key).await {
                return Ok(hit.value);
            }
        }

        let url = format!("{}/repos/{}/{}", self.base, owner, repo);
        let text = self.get_body(&url).await?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| GithubError::Other(format!("repo response JSON: {e}")))?;
        let branch = v
            .get("default_branch")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| GithubError::Other("repo response missing default_branch".into()))?;

        if let Some(cache) = &self.cache {
            let _ = cache.put(&key, &branch, CACHE_TTL).await;
        }
        Ok(branch)
    }

    /// Check if a repo exists and is readable with the current auth.
    pub async fn repo_exists(&self, owner: &str, repo: &str) -> Result<bool, GithubError> {
        let url = format!("{}/repos/{}/{}", self.base, owner, repo);
        let status = self.head_status(&url).await?;
        if is_success(status) {
            Ok(true)
        } else if is_not_found(status) {
            Ok(false)
        } else {
            Err(status_to_err(status, "repo lookup"))
        }
    }

    async fn head_status(&self, url: &str) -> Result<u16, GithubError> {
        let mut req = self.client.get(url);
        req = self.with_headers(req);
        let res = req.send().await.map_err(|e| classify_reqwest_error(&e))?;
        let status = res.status();
        // GitHub 403 with rate-limit remaining = 0 is a rate-limit condition.
        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(reset) = rate_limit_reset(res.headers()) {
                return Err(GithubError::RateLimited { reset_at: reset });
            }
        }
        Ok(status.as_u16())
    }

    async fn get_body(&self, url: &str) -> Result<String, GithubError> {
        let mut req = self.client.get(url);
        req = self.with_headers(req);
        let res = req.send().await.map_err(|e| classify_reqwest_error(&e))?;
        let status = res.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(reset) = rate_limit_reset(res.headers()) {
                return Err(GithubError::RateLimited { reset_at: reset });
            }
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(GithubError::Unauthorized);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(GithubError::NotFound { what: url.into() });
        }
        if !status.is_success() {
            return Err(GithubError::Other(format!("HTTP {status} for {url}")));
        }
        res.text()
            .await
            .map_err(|e| GithubError::NetworkError(e.to_string()))
    }

    fn with_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = req
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            req.header("Authorization", format!("Bearer {token}"))
        } else {
            req
        }
    }
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

fn is_not_found(status: u16) -> bool {
    status == 404
}

fn status_to_err(status: u16, what: &str) -> GithubError {
    match status {
        401 => GithubError::Unauthorized,
        403 => GithubError::RateLimited {
            reset_at: SystemTime::now(),
        },
        _ => GithubError::Other(format!("HTTP {status} during {what}")),
    }
}

fn rate_limit_reset(headers: &reqwest::header::HeaderMap) -> Option<SystemTime> {
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())?;
    if remaining > 0 {
        return None;
    }
    let reset_epoch = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(reset_epoch))
}

fn classify_reqwest_error(e: &reqwest::Error) -> GithubError {
    if e.is_timeout() {
        GithubError::NetworkError("timed out".into())
    } else if e.is_connect() {
        GithubError::NetworkError(format!("connection failed: {e}"))
    } else {
        GithubError::NetworkError(e.to_string())
    }
}

/// Serialize/deserialize helper used by the cache.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBranch(String);

#[cfg(test)]
mod tests {
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    async fn server() -> MockServer {
        MockServer::start().await
    }

    fn client_for(server: &MockServer) -> GithubClient {
        GithubClient::new_with_base(server.uri(), None)
            .unwrap()
            .without_cache()
    }

    #[tokio::test]
    async fn resolve_ref_branch_wins() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/branches/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "main"})),
            )
            .mount(&s)
            .await;
        let c = client_for(&s);
        let r = c.resolve_ref("foo", "bar", "main").await.unwrap();
        assert!(matches!(r, GitRef::Branch(ref b) if b == "main"));
    }

    #[tokio::test]
    async fn resolve_ref_tag_when_branch_missing() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/branches/v1.0.0"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/git/ref/tags/v1.0.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&s)
            .await;
        let c = client_for(&s);
        let r = c.resolve_ref("foo", "bar", "v1.0.0").await.unwrap();
        assert!(matches!(r, GitRef::Tag(ref t) if t == "v1.0.0"));
    }

    #[tokio::test]
    async fn resolve_ref_not_found_when_neither_exists() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/branches/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/git/ref/tags/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let c = client_for(&s);
        let err = c.resolve_ref("foo", "bar", "gone").await.unwrap_err();
        assert!(matches!(err, GithubError::NotFound { .. }));
    }

    #[tokio::test]
    async fn resolve_ref_branch_preferred_when_both_exist() {
        // GitHub's own convention: if a ref is both a branch and a tag, tools
        // generally prefer the branch. Our implementation short-circuits on
        // branch success and never queries the tag endpoint — we assert no
        // tag mock is needed.
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/branches/dual"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&s)
            .await;
        let c = client_for(&s);
        let r = c.resolve_ref("foo", "bar", "dual").await.unwrap();
        assert!(matches!(r, GitRef::Branch(_)));
    }

    #[tokio::test]
    async fn resolve_short_sha_expands() {
        let s = server().await;
        let full = "abcdef1234567890abcdef1234567890abcdef12";
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/commits/abcdef1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sha": full,
            })))
            .mount(&s)
            .await;
        let c = client_for(&s);
        let got = c.resolve_short_sha("foo", "bar", "abcdef1").await.unwrap();
        assert_eq!(got, full);
    }

    #[tokio::test]
    async fn resolve_short_sha_rejects_invalid_response() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar/commits/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sha": "not-a-sha",
            })))
            .mount(&s)
            .await;
        let c = client_for(&s);
        let err = c.resolve_short_sha("foo", "bar", "bad").await.unwrap_err();
        assert!(matches!(err, GithubError::Other(_)));
    }

    #[tokio::test]
    async fn default_branch_reads_from_repo_payload() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_branch": "develop",
            })))
            .mount(&s)
            .await;
        let c = client_for(&s);
        assert_eq!(c.default_branch("foo", "bar").await.unwrap(), "develop");
    }

    #[tokio::test]
    async fn repo_exists_true_on_200() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&s)
            .await;
        let c = client_for(&s);
        assert!(c.repo_exists("foo", "bar").await.unwrap());
    }

    #[tokio::test]
    async fn repo_exists_false_on_404() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s)
            .await;
        let c = client_for(&s);
        assert!(!c.repo_exists("foo", "bar").await.unwrap());
    }

    #[tokio::test]
    async fn unauthorized_classified() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let c = client_for(&s);
        let err = c.default_branch("foo", "bar").await.unwrap_err();
        assert!(matches!(err, GithubError::Unauthorized));
    }

    #[tokio::test]
    async fn rate_limited_on_403_with_remaining_zero() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/repos/foo/bar"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", "1700000000"),
            )
            .mount(&s)
            .await;
        let c = client_for(&s);
        let err = c.default_branch("foo", "bar").await.unwrap_err();
        assert!(matches!(err, GithubError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn has_token_tracks_construction() {
        let empty = GithubClient::new_with_base("http://example", None).unwrap();
        assert!(!empty.has_token());
        let whitespace = GithubClient::new_with_base("http://example", Some("   ")).unwrap();
        assert!(!whitespace.has_token());
        let real = GithubClient::new_with_base("http://example", Some("ghp_abc")).unwrap();
        assert!(real.has_token());
    }
}
