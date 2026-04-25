//! Static safety check: no code path in `basilisk-exec` may broadcast
//! a transaction to the upstream chain. Forking is fork-local only.
//!
//! Greps the crate sources for the JSON-RPC methods that *would*
//! broadcast (`eth_sendRawTransaction`, `eth_submitWork`, …) and the
//! Rust-side helpers that wrap them (`send_raw_transaction` etc.).
//! Found in code → test fails. The whitelist is empty: this crate
//! must never broadcast.
//!
//! `eth_sendTransaction` is allowed: anvil interprets it as
//! "execute against the local in-memory chain" — no broadcast.

use std::path::Path;

const FORBIDDEN: &[&str] = &[
    "eth_sendRawTransaction",
    "eth_submitWork",
    "send_raw_transaction",
    "submit_work",
];

#[test]
fn no_broadcast_calls_in_crate_sources() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits: Vec<String> = Vec::new();
    walk(&src, &mut hits);
    assert!(
        hits.is_empty(),
        "broadcast-shaped JSON-RPC method found in basilisk-exec sources: {hits:?}\n\
         every code path here must be fork-local — broadcasting is an incident.",
    );
}

fn walk(p: &Path, hits: &mut Vec<String>) {
    if p.is_dir() {
        for entry in std::fs::read_dir(p).unwrap().flatten() {
            walk(&entry.path(), hits);
        }
        return;
    }
    if p.extension().and_then(|s| s.to_str()) != Some("rs") {
        return;
    }
    let s = std::fs::read_to_string(p).unwrap_or_default();
    // Skip this very test file (it has to mention the forbidden
    // strings to know what to forbid).
    if p.file_name().and_then(|n| n.to_str()) == Some("no_broadcast.rs") {
        return;
    }
    for needle in FORBIDDEN {
        if s.contains(needle) {
            hits.push(format!("{} in {}", needle, p.display()));
        }
    }
}
