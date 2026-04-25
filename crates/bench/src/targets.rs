//! Five real post-exploit benchmark targets. *Set 9 calibration set.*
//!
//! Each target's `fork_block` is the block *before* the exploit —
//! the agent sees the code as it looked when vulnerable. The
//! `exploit_block` is reference only; we don't run against post-
//! exploit state.
//!
//! Keywords in `expected_findings` are deliberately generic (the
//! names of the core mechanic) so the scoring matcher doesn't
//! over-fit to the exact phrasing the agent chooses. Operators can
//! still adjudicate ambiguous matches via `audit bench review`.

use alloy_primitives::address;

use crate::types::{BenchmarkTarget, ExpectedFinding, Severity};

/// Every benchmark target shipped with Set 9.
pub fn all_targets() -> &'static [&'static BenchmarkTarget] {
    ALL
}

/// Find a target by id.
pub fn by_id(id: &str) -> Option<&'static BenchmarkTarget> {
    ALL.iter().copied().find(|t| t.id == id)
}

static ALL: &[&BenchmarkTarget] = &[
    &EULER_2023,
    &VISOR_2021,
    &CREAM_OCT_2021,
    &BEANSTALK_APRIL_2022,
    &NOMAD_AUG_2022,
];

// ---------- Euler Finance, March 2023 --------------------------------
//
// Flash-loan-funded donation attack exploiting the `donateToReserves`
// function: the attacker deposited eTokens, then donated to reserves
// making themselves insolvent on paper, then triggered liquidation
// against their own self-liquidating position. The liquidator
// received a disproportionate share due to a bad health-factor
// calculation. Loss: ~$197M.
//
// Set 9.5 / CP9.5.7 — re-targeted from the dispatcher
// (0x27182842…25d3) to the eDAI proxy. The dispatcher is just a
// router via delegatecall; the buggy code lives in the EToken
// module implementation that eDAI's proxy delegates to. From the
// eDAI address, `resolve_onchain_system` will pull in:
//   - the EToken module implementation (donateToReserves + the
//     bad balance-tracking math)
//   - the dispatcher itself (via Proxy.sol → Euler.dispatch)
//   - the Liquidation module (the second half of the exploit)
// Pointing at eDAI sets the agent up to find the donation primitive
// without requiring it to first guess "the dispatcher is just an
// indirection — descend into the modules".
//
// Run B (Opus, $8.78, 0% coverage on the Euler dispatcher) made the
// case for this re-target: Opus's self-critique explicitly noted
// it would have audited the modules on a re-run.

static EULER_EXPECTED: [ExpectedFinding; 2] = [
    ExpectedFinding {
        class: "donation_attack",
        must_mention: &["donation", "donateToReserves", "self-liquid"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
    ExpectedFinding {
        class: "flash_loan",
        must_mention: &["flash loan", "flash-loan", "flashloan"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
];

static EULER_2023: BenchmarkTarget = BenchmarkTarget {
    id: "euler-2023",
    name: "Euler Finance — donation + self-liquidation",
    chain: "ethereum",
    // eDAI proxy. Re-targeted from the dispatcher in CP9.5.7 to
    // give the agent direct access to the EToken module surface.
    target_address: address!("e025E3ca2bE02316033184551D4d3Aa22024D9DC"),
    // Block 16_817_995 is immediately before the exploit (16_817_996
    // contained the first malicious tx).
    fork_block: 16_817_995,
    exploit_block: 16_817_996,
    vulnerability_classes: &["donation_attack", "flash_loan", "liquidation", "math"],
    severity: Severity::Critical,
    expected_findings: &EULER_EXPECTED,
    references: &[
        "https://medium.com/@omniscia.io/euler-finance-incident-post-mortem-1ce077c28454",
        "https://blog.chainalysis.com/reports/euler-finance-flash-loan-attack/",
    ],
    notes: "Re-targeted (CP9.5.7) from the dispatcher (0x27182842…25d3) to the eDAI proxy (0xe025E3ca…D9DC). The dispatcher is module-routing only — the buggy donateToReserves + liquidation interaction lives in the EToken module that eDAI delegates to. Complex multi-step exploit: flash loan → deposit → self-liquidate after donateToReserves. Agent may surface just the donateToReserves weakness OR the liquidation mis-pricing; either counts.",
};

// ---------- Visor Finance, December 2021 -----------------------------
//
// Reentrancy via custom token (ERC20-like) used as "owner". The
// attacker deployed a contract that returned a crafted address in
// response to `owner()` queries during the vulnerable function's
// execution, manipulating authorization checks. Loss: ~$8.2M.
//
// Simpler target than Euler — good "smoke test" for the vuln-
// reasoning run (the bug is well-documented and narrow in scope).

static VISOR_EXPECTED: [ExpectedFinding; 2] = [
    ExpectedFinding {
        class: "reentrancy",
        must_mention: &["reentrancy", "re-enter", "callback"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
    ExpectedFinding {
        class: "access_control",
        must_mention: &["owner", "authorization", "access"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
];

static VISOR_2021: BenchmarkTarget = BenchmarkTarget {
    id: "visor-2021",
    name: "Visor Finance — reentrancy via owner() callback",
    chain: "ethereum",
    target_address: address!("c9f27a50f82571c1c8423a42970613b8dbdbd5a0"),
    fork_block: 13_840_149,
    exploit_block: 13_840_150,
    vulnerability_classes: &["reentrancy", "access_control"],
    severity: Severity::High,
    expected_findings: &VISOR_EXPECTED,
    references: &[
        "https://visor.finance/posts/vault-compromise-post-mortem",
        "https://rekt.news/visor-rekt/",
    ],
    notes: "DEFERRED (CP9.5.7): Visor's vault was selfdestructed post-exploit. The agent's resolve_onchain_contract reads chain state at `latest`, not at fork_block — so against this target it sees `is_contract: false` and reports the address as an EOA (Run #4 confirmed this behavior). The fix is threading fork_block awareness through the agent's chain-reading tools, which lands in a future set. Until then, this benchmark is structurally broken; `audit bench run` still includes it for completeness, but expect 0% coverage. Skip via `audit bench run <other-id>` if you want a meaningful suite total.",
};

// ---------- Cream Finance, October 2021 ------------------------------
//
// Flash-loan-enabled price manipulation of a thinly-traded collateral
// token. The attacker borrowed $1.5B via flash loan, manipulated the
// price oracle (AMM-based, no TWAP), then liquidated CREAM
// positions at the manipulated price. Loss: ~$130M.

static CREAM_EXPECTED: [ExpectedFinding; 2] = [
    ExpectedFinding {
        class: "oracle_manipulation",
        must_mention: &["oracle", "price", "TWAP"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
    ExpectedFinding {
        class: "flash_loan",
        must_mention: &["flash loan", "flash-loan", "flashloan"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
];

static CREAM_OCT_2021: BenchmarkTarget = BenchmarkTarget {
    id: "cream-oct-2021",
    name: "Cream Finance — flash-loan price oracle manipulation",
    chain: "ethereum",
    // CREAM token contract. CP9.5.7 audit: the *vulnerable contract*
    // is the IronBank lending pool / cyToken (one of several
    // markets exploited via the manipulated oracle), not the CREAM
    // token itself. Keeping this address because (a) the post-mortem
    // references it directly, (b) `resolve_onchain_system` from
    // CREAM will pull in the lending pool via its references, (c) a
    // future re-target to the specific cyToken proxy is a task for
    // when the suite is run regularly enough to justify the
    // research.
    target_address: address!("d06527D5e56A3495252A528C4987003b712860eE"),
    fork_block: 13_412_088,
    exploit_block: 13_412_089,
    vulnerability_classes: &["oracle_manipulation", "flash_loan", "liquidation"],
    severity: Severity::Critical,
    expected_findings: &CREAM_EXPECTED,
    references: &[
        "https://rekt.news/cream-rekt-2/",
        "https://medium.com/cream-finance/c-r-e-a-m-finance-post-mortem-amp-exploit-6ceb20a630c5",
    ],
    notes: "Oracle-manipulation class. Agent should surface the missing TWAP and the spot-price dependency. Note: target_address points at the CREAM token, not the directly-exploited IronBank cyToken — see code comment for re-target rationale.",
};

// ---------- Beanstalk Farms, April 2022 ------------------------------
//
// Governance attack. The attacker used a flash loan to acquire
// voting power, then executed a malicious proposal that drained the
// protocol's reserves via an `emergencyCommit` function that didn't
// properly check proposal state. Loss: ~$182M.

static BEANSTALK_EXPECTED: [ExpectedFinding; 2] = [
    ExpectedFinding {
        class: "governance",
        must_mention: &["governance", "voting", "proposal", "timelock"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
    ExpectedFinding {
        class: "flash_loan",
        must_mention: &["flash loan", "flash-loan", "flashloan", "voting power"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
];

static BEANSTALK_APRIL_2022: BenchmarkTarget = BenchmarkTarget {
    id: "beanstalk-apr-2022",
    name: "Beanstalk Farms — governance via flash-loaned voting weight",
    chain: "ethereum",
    // Beanstalk's diamond proxy. The vulnerable function
    // (emergencyCommit + the missing timelock check on flash-loan-
    // acquired voting power) lives in a governance facet attached
    // to this diamond. CP9.5.7 keeps this address because a diamond
    // IS the entry point — `resolve_onchain_system` walks
    // DiamondCut events (or facet enumeration calls) to surface
    // the governance facet from here. Unlike Euler's dispatcher,
    // there's no clearer "vulnerable module" address to point at.
    target_address: address!("C1E088fC1323b20BCBee9bd1B9fC9546db5624C5"),
    fork_block: 14_602_788,
    exploit_block: 14_602_789,
    vulnerability_classes: &["governance", "flash_loan", "timelock"],
    severity: Severity::Critical,
    expected_findings: &BEANSTALK_EXPECTED,
    references: &[
        "https://rekt.news/beanstalk-rekt/",
        "https://bean.money/blog/beanstalk-governance-exploit",
    ],
    notes: "Governance class. Diamond pattern — the buggy emergencyCommit logic is in a governance facet reached via DiamondCut. Agent should connect the missing timelock with flash-loan voting-weight acquisition.",
};

// ---------- Nomad Bridge, August 2022 --------------------------------
//
// Replay attack enabled by an initializer bug: the bridge's `Replica`
// contract treated the zero-hash as a valid confirmed-message root,
// so any message with zero proof bytes was accepted. After initial
// discovery, many actors piled in and copy-pasted each other's
// exploit txs. Loss: ~$190M.

static NOMAD_EXPECTED: [ExpectedFinding; 2] = [
    ExpectedFinding {
        class: "initialization",
        must_mention: &["initial", "zero", "root"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
    ExpectedFinding {
        class: "replay",
        must_mention: &["replay", "signature", "message"],
        must_not_mention_only: &[],
        severity_min: Severity::High,
    },
];

static NOMAD_AUG_2022: BenchmarkTarget = BenchmarkTarget {
    id: "nomad-aug-2022",
    name: "Nomad Bridge — zero-root replay via bad initialization",
    chain: "ethereum",
    // Replica proxy. CP9.5.7 re-target — the original
    // target_address (0x88A69B4E…30A3) was the bridge home contract;
    // the bug actually lived in the Replica (the message-verifier
    // side) where the zero-hash was misclassified as a valid
    // confirmed root during initialization. Pointing at the Replica
    // proxy gets the agent directly to the buggy `process` /
    // `messages` storage path. The Replica is upgrade-managed;
    // resolve_onchain_system pulls in the implementation.
    target_address: address!("b92336759618f55bd0f8313bd843604592e27bd8"),
    fork_block: 15_259_100,
    exploit_block: 15_259_101,
    vulnerability_classes: &["initialization", "replay", "bridge", "signature"],
    severity: Severity::Critical,
    expected_findings: &NOMAD_EXPECTED,
    references: &[
        "https://rekt.news/nomad-rekt/",
        "https://medium.com/nomad-xyz-blog/nomad-bridge-hack-root-cause-analysis-875ad2e5aacd",
    ],
    notes: "Re-targeted (CP9.5.7) from the home contract (0x88A69B4E…30A3) to the Replica proxy. The bug lives on the message-verification side: the zero-hash was treated as a valid confirmed-message root after a bad initialization, so any message with zero proof bytes succeeded `process()`. Initialization or replay class — both count.",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_targets_have_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for t in all_targets() {
            assert!(seen.insert(t.id), "duplicate id: {}", t.id);
        }
    }

    #[test]
    fn all_targets_have_at_least_one_expected_finding() {
        for t in all_targets() {
            assert!(
                !t.expected_findings.is_empty(),
                "target {} has no expected findings",
                t.id,
            );
        }
    }

    #[test]
    fn all_targets_are_ethereum_mainnet() {
        for t in all_targets() {
            assert_eq!(t.chain, "ethereum", "{}", t.id);
        }
    }

    #[test]
    fn fork_block_precedes_exploit_block() {
        for t in all_targets() {
            assert!(
                t.fork_block < t.exploit_block,
                "{}: fork_block {} !< exploit_block {}",
                t.id,
                t.fork_block,
                t.exploit_block,
            );
        }
    }

    #[test]
    fn by_id_finds_each_target() {
        for t in all_targets() {
            assert!(by_id(t.id).is_some(), "by_id missed {}", t.id);
        }
        assert!(by_id("nonesuch").is_none());
    }

    #[test]
    fn each_target_declares_vuln_classes_matching_expectations() {
        for t in all_targets() {
            for expected in t.expected_findings {
                assert!(
                    t.vulnerability_classes.contains(&expected.class),
                    "{}: expected class {} not in vuln_classes",
                    t.id,
                    expected.class,
                );
            }
        }
    }

    #[test]
    fn five_targets_ship() {
        assert_eq!(all_targets().len(), 5);
    }
}
