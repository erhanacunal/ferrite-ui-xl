//! Dynamic opcode profile (board `GET|POST /api/vmprof`, sim
//! `FERRITE_SIM_VMPROF=SECS`; `scripts/vmprof.py` names and sorts it).
//!
//! While `ON`, `Vm::tick` runs its burst through `exec_one` (not the
//! threaded dispatch) and `exec_one` counts each instruction before running
//! it: the opcode, a superinstruction's cond / op operand, and for each
//! variable-slot operand whether it is a local or a global. Every VM (all
//! tasks, callbacks, setup) adds to the same counters. A quickened form
//! (`QUICK`) counts as its instruction, cond / op and slot kind, so the
//! profile reads the same before and after quickening. Off, `exec_one` pays one
//! relaxed load and a branch.
//!
//! The counters are written on CPU0 (the runtime loop) and read anywhere.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::*;

/// Counting (see the module docs).
pub static ON: AtomicBool = AtomicBool::new(false);

/// Counter layout: opcodes, then the superinstructions' cond / op operands
/// (`SUPERS` superinstructions from OP_JCMP, 32 values each), then per
/// opcode the local and the global slot operands.
const OPS: usize = 0;
const SUPERS: usize = 9; // OP_JCMP..=OP_STORE_OP
const SUBS: usize = OPS + 256;
const LOCALS: usize = SUBS + SUPERS * 32;
const GLOBALS: usize = LOCALS + 256;
const COUNTERS: usize = GLOBALS + 256;

static COUNTS: [AtomicU32; COUNTERS] = [const { AtomicU32::new(0) }; COUNTERS];

/// Instructions counted since the last reset.
static TOTAL: AtomicU32 = AtomicU32::new(0);

#[inline(always)]
fn bump(i: usize) {
    // One writer (CPU0): a load and a store, no atomic read-modify-write.
    let c = &COUNTS[i];
    c.store(c.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
}

/// Zero every counter.
pub fn reset() {
    for c in COUNTS.iter() {
        c.store(0, Ordering::Relaxed);
    }
    TOTAL.store(0, Ordering::Relaxed);
}

/// Counts the instruction at `pc` in `code` (its operands read through the
/// checked `byte_at`: 0 past the end).
#[cold]
#[inline(never)]
pub(super) fn count(code: &VmCode, pc: usize) {
    TOTAL.store(TOTAL.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    let raw = code.byte_at(pc);
    let (op, sub, kind) = if (Q_FIRST..=Q_LAST).contains(&raw) {
        let (op, sub, kind) = QUICK[(raw - Q_FIRST) as usize];
        (op, Some(sub), kind)
    } else {
        (raw, None, SLOT_ANY)
    };
    bump(OPS + op as usize);
    if (OP_JCMP..=OP_STORE_OP).contains(&op) && op != OP_ADD_VI && op != OP_ARR_LOAD_V {
        let sub = sub.unwrap_or_else(|| code.byte_at(pc + 1));
        if sub < 32 {
            bump(SUBS + (op - OP_JCMP) as usize * 32 + sub as usize);
        }
    }
    // Where the slot operands are (offsets from the opcode).
    let slots: &[usize] = match op {
        OP_LOAD | OP_STORE | OP_ADD_VI | OP_ARR_LOAD_V => &[1],
        OP_JCMP_VI | OP_BIN_V | OP_STORE_OP => &[2],
        OP_JCMP_VV => &[2, 3],
        _ => &[],
    };
    for &k in slots {
        let base = if force_slot(code.byte_at(pc + k), kind) & 0x80 != 0 { LOCALS } else { GLOBALS };
        bump(base + op as usize);
    }
}

/// The counters as text, one line each, nonzero only:
/// `total N`, `op 0xOP N`, `sub 0xOP 0xSUB N`, `slot 0xOP L|G N`.
pub fn report() -> String {
    let mut s = String::new();
    let get = |i: usize| COUNTS[i].load(Ordering::Relaxed);
    let _ = writeln!(s, "total {}", TOTAL.load(Ordering::Relaxed));
    for op in 0..256 {
        let n = get(OPS + op);
        if n != 0 {
            let _ = writeln!(s, "op 0x{op:02x} {n}");
        }
    }
    for k in 0..SUPERS {
        for sub in 0..32 {
            let n = get(SUBS + k * 32 + sub);
            if n != 0 {
                let _ = writeln!(s, "sub 0x{:02x} 0x{sub:02x} {n}", OP_JCMP as usize + k);
            }
        }
    }
    for (base, kind) in [(LOCALS, 'L'), (GLOBALS, 'G')] {
        for op in 0..256 {
            let n = get(base + op);
            if n != 0 {
                let _ = writeln!(s, "slot 0x{op:02x} {kind} {n}");
            }
        }
    }
    s
}
