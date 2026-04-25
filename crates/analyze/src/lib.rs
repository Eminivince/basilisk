//! Analytical tools the agent uses during vulnerability reasoning.
//!
//! Three capabilities ship in Set 9:
//!
//! - [`callers::find_callers_of`] — locate every caller of a target
//!   `(address, selector)` within a [`ResolvedSystem`], falling back
//!   from AST-precision (when source is available) to bytecode pattern
//!   matching, with a per-hit confidence rating.
//! - [`state_deps::trace_state_dependencies`] — for one function in
//!   one contract, list the storage slots it reads, the slots it
//!   writes, and the external calls it makes. The substrate for
//!   reentrancy and state-machine reasoning. *(CP9.4)*
//! - [`simulate::simulate_call_chain`] — run an ordered sequence of
//!   `(from, to, calldata, value)` tuples against a forked EVM state
//!   and return per-call success / revert reasons / state diffs.
//!   *(CP9.5)*
//!
//! All three are `pub` functions returning typed structs; the
//! agent-facing tool wrappers live in `basilisk-agent`'s
//! `crates/agent/src/tools/` (CP9.8).

pub mod bytecode;
pub mod callers;
pub mod error;
pub mod simulate;
pub mod state_deps;

pub use callers::{find_callers_of, CallerEvidence, CallerHit, CallerSearch, CallerSearchResult};
pub use error::AnalyzeError;
pub use simulate::{
    simulate_call_chain, BalanceReading, CallStep, SimulationInput, SimulationResult,
    StepOutcome, StorageReading,
};
pub use state_deps::{trace_state_dependencies, ExternalCall, Precision, SlotRef, StateDeps};
