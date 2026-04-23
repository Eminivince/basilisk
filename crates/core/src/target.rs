//! The `Target` enum and its supporting types.
//!
//! A `Target` is the structured result of running [`crate::detect::detect`]
//! over an arbitrary input string. It describes a thing to audit — a GitHub
//! repository, an on-chain contract, a local project — or carries a clear
//! reason why the input couldn't be classified.

use std::{fmt, path::PathBuf, str::FromStr};

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chain::Chain;

/// A thing to audit (or a structured "I don't know" with a reason).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// A GitHub repository, optionally pinned to a ref and/or a subpath.
    Github {
        owner: String,
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<GitRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<PathBuf>,
    },
    /// A deployed contract, identified by a 20-byte EVM address on a specific chain.
    OnChain { address: Address, chain: Chain },
    /// A local filesystem project. `root` is canonicalized to an absolute path.
    LocalPath {
        root: PathBuf,
        project_kind: ProjectKind,
    },
    /// The detector could not classify the input.
    Unknown {
        input: String,
        reason: UnknownReason,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        suggestions: Vec<String>,
    },
}

/// A git ref attached to a [`Target::Github`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitRef {
    /// Explicit `refs/heads/<name>` branch ref.
    Branch(String),
    /// Explicit `refs/tags/<name>` tag ref.
    Tag(String),
    /// 7-40 hex-character commit SHA.
    Commit(String),
    /// Bare `/tree/<name>` ref — could be either a branch or a tag.
    Ambiguous(String),
}

/// What flavor of Solidity project a local directory looks like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectKind {
    /// `foundry.toml` present.
    Foundry,
    /// `hardhat.config.{js,ts,cjs,mjs}` present.
    Hardhat,
    /// `truffle-config.js` present.
    Truffle,
    /// More than one project marker found at the root.
    Mixed(Vec<ProjectKind>),
    /// Directory contains `.sol` files but no recognized config.
    Unknown,
    /// Directory exists but contains no `.sol` files anywhere meaningful.
    NoSolidity,
}

/// Why a [`Target::Unknown`] was returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnknownReason {
    /// Input was empty or whitespace-only.
    Empty,
    /// Input partially matched several heuristics; none won.
    Ambiguous { hints: Vec<String> },
    /// Input looked like a hex address but failed parsing.
    MalformedAddress { detail: String },
    /// Input was a URL to a host we don't yet support.
    UnsupportedUrlHost { host: String },
    /// Input parsed as a URL but didn't fit any known shape.
    MalformedUrl { detail: String },
    /// Input was a path that doesn't exist (or wasn't readable).
    PathDoesNotExist,
    /// Input was a path that exists but isn't a directory.
    PathNotADirectory,
    /// Anything else with a free-form message.
    Other(String),
}

impl Target {
    /// Short label suitable for logs and status output.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Github { .. } => "github",
            Self::OnChain { .. } => "on_chain",
            Self::LocalPath { .. } => "local_path",
            Self::Unknown { .. } => "unknown",
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Github {
                owner,
                repo,
                reference,
                subpath,
            } => {
                writeln!(f, "Target: GitHub repository")?;
                writeln!(f, "  owner: {owner}")?;
                writeln!(f, "  repo:  {repo}")?;
                match reference {
                    Some(GitRef::Branch(b)) => writeln!(f, "  ref:   branch {b}")?,
                    Some(GitRef::Tag(t)) => writeln!(f, "  ref:   tag {t}")?,
                    Some(GitRef::Commit(c)) => writeln!(f, "  ref:   commit {c}")?,
                    Some(GitRef::Ambiguous(r)) => writeln!(f, "  ref:   {r} (branch or tag)")?,
                    None => writeln!(f, "  ref:   <default>")?,
                }
                if let Some(sp) = subpath {
                    writeln!(f, "  subpath: {}", sp.display())?;
                }
                Ok(())
            }
            Self::OnChain { address, chain } => {
                writeln!(f, "Target: on-chain contract")?;
                writeln!(f, "  address: {address}")?;
                writeln!(f, "  chain:   {} (id {})", chain, chain.chain_id())
            }
            Self::LocalPath { root, project_kind } => {
                writeln!(f, "Target: local project")?;
                writeln!(f, "  root: {}", root.display())?;
                writeln!(f, "  kind: {project_kind}")
            }
            Self::Unknown {
                input,
                reason,
                suggestions,
            } => {
                writeln!(f, "Target: unknown")?;
                writeln!(f, "  input:  {input:?}")?;
                writeln!(f, "  reason: {reason}")?;
                if !suggestions.is_empty() {
                    writeln!(f, "  suggestions:")?;
                    for s in suggestions {
                        writeln!(f, "    - {s}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for ProjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Foundry => f.write_str("foundry"),
            Self::Hardhat => f.write_str("hardhat"),
            Self::Truffle => f.write_str("truffle"),
            Self::Mixed(kinds) => {
                let names: Vec<String> = kinds.iter().map(ToString::to_string).collect();
                write!(f, "mixed({})", names.join(", "))
            }
            Self::Unknown => f.write_str("unknown (has .sol, no config)"),
            Self::NoSolidity => f.write_str("no Solidity sources"),
        }
    }
}

impl fmt::Display for UnknownReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("input was empty"),
            Self::Ambiguous { hints } => {
                write!(f, "ambiguous input")?;
                for h in hints {
                    write!(f, "; {h}")?;
                }
                Ok(())
            }
            Self::MalformedAddress { detail } => write!(f, "malformed address: {detail}"),
            Self::UnsupportedUrlHost { host } => write!(f, "unsupported URL host: {host}"),
            Self::MalformedUrl { detail } => write!(f, "malformed URL: {detail}"),
            Self::PathDoesNotExist => f.write_str("path does not exist"),
            Self::PathNotADirectory => f.write_str("path is not a directory"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

/// Errors returned by [`parse_address`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AddressParseError {
    /// Hex payload had the wrong number of characters (expected 40).
    #[error("address must be 40 hex characters, got {actual}")]
    WrongLength { actual: usize },
    /// Payload contained non-hex characters.
    #[error("address contains non-hex characters: {0}")]
    InvalidHex(String),
    /// Mixed-case input failed EIP-55 checksum validation.
    #[error("EIP-55 checksum mismatch (expected {expected}, got {got})")]
    BadChecksum { expected: String, got: String },
}

/// Parse a hex address into an [`alloy_primitives::Address`].
///
/// Accepts:
/// - `0x`-prefixed (preferred) or unprefixed 40-character hex.
/// - All-lowercase or all-uppercase: treated as unchecksummed input and accepted.
/// - Mixed-case: enforced as EIP-55 and rejected on checksum mismatch.
pub fn parse_address(input: &str) -> Result<Address, AddressParseError> {
    let trimmed = input.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);

    if body.len() != 40 {
        return Err(AddressParseError::WrongLength { actual: body.len() });
    }
    if !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AddressParseError::InvalidHex(body.to_string()));
    }

    // Always parse from the lowercased form to obtain the canonical address.
    // Length + hex were validated above, so this cannot fail.
    let lower = format!("0x{}", body.to_ascii_lowercase());
    let canonical =
        Address::from_str(&lower).map_err(|e| AddressParseError::InvalidHex(e.to_string()))?;

    // Mixed case: require strict EIP-55 match against alloy's canonical rendering.
    let has_upper = body.chars().any(|c| c.is_ascii_uppercase());
    let has_lower_letters = body.chars().any(|c| c.is_ascii_lowercase());
    if has_upper && has_lower_letters {
        let got = format!("0x{body}");
        let expected = canonical.to_string();
        if expected != got {
            return Err(AddressParseError::BadChecksum { expected, got });
        }
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical EIP-55 vector from the spec.
    const VITALIK_LOWER: &str = "0xfb6916095ca1df60bb79ce92ce3ea74c37c5d359";
    const VITALIK_CHECKSUM: &str = "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359";

    #[test]
    fn eip55_known_vector() {
        let addr = parse_address(VITALIK_LOWER).expect("lowercase parses");
        assert_eq!(addr.to_string(), VITALIK_CHECKSUM);
    }

    #[test]
    fn parse_accepts_all_lower_and_upper() {
        let lower = parse_address(VITALIK_LOWER).unwrap();
        let upper = parse_address(&VITALIK_LOWER.to_ascii_uppercase().replace("0X", "0x")).unwrap();
        assert_eq!(lower, upper);
    }

    #[test]
    fn parse_accepts_correct_checksum() {
        let addr = parse_address(VITALIK_CHECKSUM).expect("checksum parses");
        assert_eq!(addr.to_string(), VITALIK_CHECKSUM);
    }

    #[test]
    fn parse_rejects_bad_checksum() {
        // Flip one hex letter case to break the checksum.
        let broken = "0xFB6916095ca1df60bB79Ce92cE3Ea74c37c5d359";
        let err = parse_address(broken).unwrap_err();
        assert!(
            matches!(err, AddressParseError::BadChecksum { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_rejects_wrong_length() {
        let err = parse_address("0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d35").unwrap_err();
        assert_eq!(err, AddressParseError::WrongLength { actual: 39 });
        let err = parse_address("0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d35900").unwrap_err();
        assert_eq!(err, AddressParseError::WrongLength { actual: 42 });
    }

    #[test]
    fn parse_rejects_non_hex() {
        let err = parse_address("0xZZ6916095ca1df60bB79Ce92cE3Ea74c37c5d359").unwrap_err();
        assert!(matches!(err, AddressParseError::InvalidHex(_)));
    }

    #[test]
    fn parse_accepts_unprefixed() {
        let addr = parse_address("fB6916095ca1df60bB79Ce92cE3Ea74c37c5d359").unwrap();
        assert_eq!(addr.to_string(), VITALIK_CHECKSUM);
    }

    #[test]
    fn target_serde_round_trip_on_chain() {
        let addr = parse_address(VITALIK_CHECKSUM).unwrap();
        let t = Target::OnChain {
            address: addr,
            chain: Chain::Arbitrum,
        };
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("OnChain"), "json was {json}");
        let parsed: Target = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }
}
