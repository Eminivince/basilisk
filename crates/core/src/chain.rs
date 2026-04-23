//! EVM chains that Basilisk knows by name.
//!
//! `Chain` covers a small set of canonical mainnets/testnets we plan to
//! support out of the box, plus an `Other` escape hatch for chains the user
//! supplies by raw chain ID. Parsing accepts canonical short names, common
//! aliases (case-insensitive), and decimal chain IDs as strings.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A supported EVM chain, or an `Other` carrying a raw chain ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Chain {
    EthereumMainnet,
    Sepolia,
    Arbitrum,
    ArbitrumSepolia,
    Base,
    BaseSepolia,
    Optimism,
    OptimismSepolia,
    Polygon,
    Bnb,
    Avalanche,
    Other { chain_id: u64, name: String },
}

/// Errors returned by [`Chain::from_str`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChainParseError {
    /// A textual name was supplied but isn't recognized.
    #[error("unknown chain name: {0:?}")]
    UnknownName(String),
    /// A numeric chain ID was supplied but isn't recognized.
    #[error("unknown chain id: {0}")]
    UnknownChainId(u64),
}

impl Chain {
    /// Canonical EIP-155 chain ID.
    pub fn chain_id(&self) -> u64 {
        match self {
            Self::EthereumMainnet => 1,
            Self::Sepolia => 11_155_111,
            Self::Arbitrum => 42_161,
            Self::ArbitrumSepolia => 421_614,
            Self::Base => 8_453,
            Self::BaseSepolia => 84_532,
            Self::Optimism => 10,
            Self::OptimismSepolia => 11_155_420,
            Self::Polygon => 137,
            Self::Bnb => 56,
            Self::Avalanche => 43_114,
            Self::Other { chain_id, .. } => *chain_id,
        }
    }

    /// Short, kebab-case canonical name used in CLI output and serialization.
    pub fn canonical_name(&self) -> &str {
        match self {
            Self::EthereumMainnet => "ethereum",
            Self::Sepolia => "sepolia",
            Self::Arbitrum => "arbitrum",
            Self::ArbitrumSepolia => "arbitrum-sepolia",
            Self::Base => "base",
            Self::BaseSepolia => "base-sepolia",
            Self::Optimism => "optimism",
            Self::OptimismSepolia => "optimism-sepolia",
            Self::Polygon => "polygon",
            Self::Bnb => "bnb",
            Self::Avalanche => "avalanche",
            Self::Other { name, .. } => name,
        }
    }

    /// Look up a known variant by its canonical EIP-155 chain ID.
    /// Returns `None` for unknown IDs (callers may construct [`Chain::Other`] explicitly).
    pub fn from_chain_id(id: u64) -> Option<Self> {
        Some(match id {
            1 => Self::EthereumMainnet,
            11_155_111 => Self::Sepolia,
            42_161 => Self::Arbitrum,
            421_614 => Self::ArbitrumSepolia,
            8_453 => Self::Base,
            84_532 => Self::BaseSepolia,
            10 => Self::Optimism,
            11_155_420 => Self::OptimismSepolia,
            137 => Self::Polygon,
            56 => Self::Bnb,
            43_114 => Self::Avalanche,
            _ => return None,
        })
    }

    /// Look up a known variant by canonical name or common alias (case-insensitive).
    fn from_name_lower(name: &str) -> Option<Self> {
        Some(match name {
            "ethereum" | "mainnet" | "eth" | "ethereum-mainnet" => Self::EthereumMainnet,
            "sepolia" | "ethereum-sepolia" => Self::Sepolia,
            "arbitrum" | "arb" | "arbitrum-one" => Self::Arbitrum,
            "arbitrum-sepolia" | "arb-sepolia" => Self::ArbitrumSepolia,
            "base" => Self::Base,
            "base-sepolia" => Self::BaseSepolia,
            "optimism" | "op" => Self::Optimism,
            "optimism-sepolia" | "op-sepolia" => Self::OptimismSepolia,
            "polygon" | "matic" => Self::Polygon,
            "bnb" | "bsc" | "binance" | "bnb-smart-chain" => Self::Bnb,
            "avalanche" | "avax" | "avalanche-c" => Self::Avalanche,
            _ => return None,
        })
    }
}

impl Default for Chain {
    /// Defaults to Ethereum mainnet.
    fn default() -> Self {
        Self::EthereumMainnet
    }
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.canonical_name())
    }
}

impl FromStr for Chain {
    type Err = ChainParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim();
        if let Ok(id) = trimmed.parse::<u64>() {
            return Self::from_chain_id(id).ok_or(ChainParseError::UnknownChainId(id));
        }
        let lower = trimmed.to_ascii_lowercase();
        Self::from_name_lower(&lower)
            .ok_or_else(|| ChainParseError::UnknownName(trimmed.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_parse() {
        assert_eq!("ethereum".parse::<Chain>().unwrap(), Chain::EthereumMainnet);
        assert_eq!("Mainnet".parse::<Chain>().unwrap(), Chain::EthereumMainnet);
        assert_eq!("ETH".parse::<Chain>().unwrap(), Chain::EthereumMainnet);
        assert_eq!("arb".parse::<Chain>().unwrap(), Chain::Arbitrum);
        assert_eq!(
            "Arbitrum-Sepolia".parse::<Chain>().unwrap(),
            Chain::ArbitrumSepolia
        );
        assert_eq!("base".parse::<Chain>().unwrap(), Chain::Base);
        assert_eq!("OP".parse::<Chain>().unwrap(), Chain::Optimism);
        assert_eq!("matic".parse::<Chain>().unwrap(), Chain::Polygon);
        assert_eq!("bsc".parse::<Chain>().unwrap(), Chain::Bnb);
        assert_eq!("avax".parse::<Chain>().unwrap(), Chain::Avalanche);
    }

    #[test]
    fn known_chain_ids_parse() {
        assert_eq!("1".parse::<Chain>().unwrap(), Chain::EthereumMainnet);
        assert_eq!("42161".parse::<Chain>().unwrap(), Chain::Arbitrum);
        assert_eq!("8453".parse::<Chain>().unwrap(), Chain::Base);
        assert_eq!("11155111".parse::<Chain>().unwrap(), Chain::Sepolia);
    }

    #[test]
    fn unknown_name_errors() {
        assert_eq!(
            "fakechain".parse::<Chain>().unwrap_err(),
            ChainParseError::UnknownName("fakechain".to_string()),
        );
    }

    #[test]
    fn unknown_chain_id_errors() {
        assert_eq!(
            "999999".parse::<Chain>().unwrap_err(),
            ChainParseError::UnknownChainId(999_999),
        );
    }

    #[test]
    fn display_round_trips_with_from_str() {
        for chain in [
            Chain::EthereumMainnet,
            Chain::Sepolia,
            Chain::Arbitrum,
            Chain::ArbitrumSepolia,
            Chain::Base,
            Chain::BaseSepolia,
            Chain::Optimism,
            Chain::OptimismSepolia,
            Chain::Polygon,
            Chain::Bnb,
            Chain::Avalanche,
        ] {
            let s = chain.to_string();
            let parsed: Chain = s.parse().expect("display output should parse back");
            assert_eq!(parsed, chain, "round trip failed for {s}");
        }
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(
            "  ethereum  ".parse::<Chain>().unwrap(),
            Chain::EthereumMainnet
        );
    }

    #[test]
    fn other_chain_id_passthrough() {
        let other = Chain::Other {
            chain_id: 31_337,
            name: "anvil".to_string(),
        };
        assert_eq!(other.chain_id(), 31_337);
        assert_eq!(other.canonical_name(), "anvil");
    }

    #[test]
    fn default_is_ethereum_mainnet() {
        assert_eq!(Chain::default(), Chain::EthereumMainnet);
    }
}
