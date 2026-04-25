//! PUSH-aware EVM bytecode walker.
//!
//! Yields one [`Instruction`] per opcode, correctly skipping the
//! immediates of `PUSH1`..`PUSH32`. This is foundational for both
//! [`callers`](crate::callers) (selector + address pattern matching
//! near CALL-family ops) and [`state_deps`](crate::state_deps)
//! (SLOAD/SSTORE walking).
//!
//! We don't construct a CFG or do constant-folding — the goal is a
//! cheap linear scan good enough for heuristic pattern matching with
//! an explicit confidence rating.

use std::ops::Range;

/// One decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction<'a> {
    /// Position of the opcode byte in the original bytecode.
    pub pc: usize,
    pub opcode: u8,
    /// Immediate bytes, if any — only `PUSH*` opcodes carry these.
    pub immediate: &'a [u8],
}

impl Instruction<'_> {
    /// Mnemonic for the opcode. Returns `"INVALID"` for undefined
    /// codes — we don't try to validate fork rules.
    pub fn mnemonic(&self) -> &'static str {
        opcode_name(self.opcode)
    }

    /// `true` iff this instruction transfers control to another
    /// contract: `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`.
    pub fn is_external_call(&self) -> bool {
        matches!(self.opcode, 0xf1 | 0xf2 | 0xf4 | 0xfa)
    }

    /// `true` iff this instruction reads or writes storage.
    pub fn touches_storage(&self) -> bool {
        matches!(self.opcode, 0x54 | 0x55) // SLOAD | SSTORE
    }

    /// Range of bytes occupied by this instruction (opcode + immediates).
    pub fn span(&self) -> Range<usize> {
        self.pc..self.pc + 1 + self.immediate.len()
    }
}

/// Iterator over a bytecode slice. Skips PUSH immediates correctly.
#[derive(Debug, Clone)]
pub struct InstructionWalker<'a> {
    bytecode: &'a [u8],
    pc: usize,
}

impl<'a> InstructionWalker<'a> {
    pub fn new(bytecode: &'a [u8]) -> Self {
        Self { bytecode, pc: 0 }
    }
}

impl<'a> Iterator for InstructionWalker<'a> {
    type Item = Instruction<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pc >= self.bytecode.len() {
            return None;
        }
        let opcode = self.bytecode[self.pc];
        let pc = self.pc;
        let imm_len = push_immediate_len(opcode);
        // Truncate immediate at end of bytecode rather than panicking
        // — malformed/truncated bytecode is real and we want to keep
        // walking what's there.
        let imm_end = (pc + 1 + imm_len).min(self.bytecode.len());
        let immediate = &self.bytecode[pc + 1..imm_end];
        self.pc = imm_end;
        Some(Instruction {
            pc,
            opcode,
            immediate,
        })
    }
}

/// `0` for non-PUSH; `1..=32` for `PUSH1`..`PUSH32`.
pub fn push_immediate_len(opcode: u8) -> usize {
    if (0x60..=0x7f).contains(&opcode) {
        usize::from(opcode - 0x5f)
    } else {
        0
    }
}

/// Decode every instruction into a `Vec` for tests / inspection.
/// Production callers should use the iterator.
pub fn disassemble(bytecode: &[u8]) -> Vec<Instruction<'_>> {
    InstructionWalker::new(bytecode).collect()
}

/// Return `Some(immediate)` if `inst` is a `PUSH*` whose immediate
/// equals `target` (interpreted big-endian, padded). Convenience for
/// "find PUSH4 0xabcdef12 in this contract."
pub fn push_matches<'a>(inst: &Instruction<'a>, target: &[u8]) -> Option<&'a [u8]> {
    if !(0x60..=0x7f).contains(&inst.opcode) {
        return None;
    }
    if inst.immediate.len() != target.len() {
        return None;
    }
    if inst.immediate == target {
        Some(inst.immediate)
    } else {
        None
    }
}

/// Mnemonic table — only the opcodes we name explicitly. Anything
/// else maps to `"INVALID"`.
fn opcode_name(op: u8) -> &'static str {
    match op {
        0x00 => "STOP",
        0x01 => "ADD",
        0x02 => "MUL",
        0x03 => "SUB",
        0x04 => "DIV",
        0x10 => "LT",
        0x11 => "GT",
        0x14 => "EQ",
        0x16 => "AND",
        0x17 => "OR",
        0x35 => "CALLDATALOAD",
        0x36 => "CALLDATASIZE",
        0x50 => "POP",
        0x51 => "MLOAD",
        0x52 => "MSTORE",
        0x54 => "SLOAD",
        0x55 => "SSTORE",
        0x56 => "JUMP",
        0x57 => "JUMPI",
        0x5b => "JUMPDEST",
        0x60..=0x7f => "PUSH",
        0x80..=0x8f => "DUP",
        0x90..=0x9f => "SWAP",
        0xa0..=0xa4 => "LOG",
        0xf0 => "CREATE",
        0xf1 => "CALL",
        0xf2 => "CALLCODE",
        0xf3 => "RETURN",
        0xf4 => "DELEGATECALL",
        0xf5 => "CREATE2",
        0xfa => "STATICCALL",
        0xfd => "REVERT",
        0xfe => "INVALID",
        0xff => "SELFDESTRUCT",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push1_immediate_skipped() {
        // PUSH1 0xff PUSH1 0x00 ADD STOP
        let bytecode = &[0x60, 0xff, 0x60, 0x00, 0x01, 0x00];
        let insts = disassemble(bytecode);
        assert_eq!(insts.len(), 4);
        assert_eq!(insts[0].opcode, 0x60);
        assert_eq!(insts[0].immediate, &[0xff]);
        assert_eq!(insts[1].opcode, 0x60);
        assert_eq!(insts[1].immediate, &[0x00]);
        assert_eq!(insts[2].opcode, 0x01); // ADD
        assert_eq!(insts[3].opcode, 0x00); // STOP
    }

    #[test]
    fn push32_consumes_full_thirty_two_bytes() {
        let mut bytes = vec![0x7f]; // PUSH32
        bytes.extend(std::iter::repeat_n(0xab, 32));
        bytes.push(0x00); // STOP
        let insts = disassemble(&bytes);
        assert_eq!(insts.len(), 2);
        assert_eq!(insts[0].immediate.len(), 32);
        assert!(insts[0].immediate.iter().all(|b| *b == 0xab));
        assert_eq!(insts[1].opcode, 0x00);
    }

    #[test]
    fn truncated_push_does_not_panic() {
        // PUSH4 0x12 0x34 — only 2 of 4 bytes
        let bytes = &[0x63, 0x12, 0x34];
        let insts = disassemble(bytes);
        assert_eq!(insts.len(), 1);
        assert_eq!(insts[0].immediate, &[0x12, 0x34]);
    }

    #[test]
    fn external_call_classifier() {
        for op in [0xf1u8, 0xf2, 0xf4, 0xfa] {
            let bytes = [op];
            let inst = disassemble(&bytes).remove(0);
            assert!(inst.is_external_call(), "0x{op:02x}");
        }
        let bytes = [0x01u8]; // ADD
        let inst = disassemble(&bytes).remove(0);
        assert!(!inst.is_external_call());
    }

    #[test]
    fn storage_classifier() {
        let load = disassemble(&[0x54]).remove(0);
        let store = disassemble(&[0x55]).remove(0);
        let other = disassemble(&[0x01]).remove(0);
        assert!(load.touches_storage());
        assert!(store.touches_storage());
        assert!(!other.touches_storage());
    }

    #[test]
    fn push_matches_returns_immediate_on_hit() {
        let bytes = [0x63, 0xde, 0xad, 0xbe, 0xef];
        let inst = disassemble(&bytes).remove(0);
        let m = push_matches(&inst, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(m, Some(&[0xde, 0xad, 0xbe, 0xef][..]));
    }

    #[test]
    fn push_matches_returns_none_on_size_or_value_mismatch() {
        let bytes = [0x63, 0xde, 0xad, 0xbe, 0xef];
        let inst = disassemble(&bytes).remove(0);
        assert!(push_matches(&inst, &[0xde, 0xad]).is_none());
        assert!(push_matches(&inst, &[0x00, 0x00, 0x00, 0x00]).is_none());
    }

    #[test]
    fn push_matches_rejects_non_push_opcode() {
        let inst = disassemble(&[0x01]).remove(0);
        assert!(push_matches(&inst, &[]).is_none());
    }

    #[test]
    fn span_covers_opcode_and_immediate() {
        let bytes = [0x60, 0xff]; // PUSH1 0xff
        let inst = disassemble(&bytes).remove(0);
        assert_eq!(inst.span(), 0..2);
    }

    #[test]
    fn empty_bytecode_yields_empty_walker() {
        assert!(disassemble(&[]).is_empty());
    }

    #[test]
    fn mnemonic_lookup_returns_known_names() {
        assert_eq!(opcode_name(0x54), "SLOAD");
        assert_eq!(opcode_name(0x55), "SSTORE");
        assert_eq!(opcode_name(0xf1), "CALL");
        assert_eq!(opcode_name(0xf4), "DELEGATECALL");
        assert_eq!(opcode_name(0x60), "PUSH");
        assert_eq!(opcode_name(0xee), "?");
    }
}
