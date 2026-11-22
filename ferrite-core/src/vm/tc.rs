//! Threaded dispatch of the hot opcodes (`explicit_tail_calls`).
//!
//! `Vm::tick`'s burst runs here. Each handled opcode is a function that ends
//! by tail-calling the next opcode's handler (`become`): no shared dispatch
//! point, so each handler's indirect branch is predicted on its own, and the
//! code pointer, the eval-stack pointer and the budget left travel in
//! argument registers (r1..r3 on ARM) instead of `Vm`'s `pc` / `sp` fields.
//! They are written back to `Vm` only when the burst leaves this loop.
//!
//! Opcodes without a handler here (the tables' default, `slow`) write the
//! state back, run the instruction through `Vm::exec_one` (the reference
//! implementation, which every other path still uses) and continue. The
//! burst ends when the VM leaves `Running` or the budget is spent, as in
//! the `exec_one` loop (a critical section continues one instruction at a
//! time past it, `budget_out`).
//!
//! Two tables, by the image's address width (`W` = `Vm::wide_addr`: u32
//! jump / call targets), so no handler tests it; `run` and `slow` (a unit
//! switch may change it) pick one.
//!
//! Quickening: a superinstruction whose cond / op operand has a quickened
//! form (`QUICK`) rewrites its opcode to that form the first time it runs
//! here (`quicken!`); the form's handler has the operation built in, with no
//! dispatch on the operand (`q_*`). `exec_one` runs both forms the same.
//!
//! Cold paths (faults) are tail calls with the handler's own four arguments
//! (the fault code a const parameter): no argument goes on the stack, so the
//! hot path sets up no frame beyond the target's frame-pointer push.
//!
//! Code reads are unchecked: `VmCode` keeps `CODE_PAD` bytes of an undefined
//! opcode after the code, so an instruction cut off by the end still reads
//! inside the buffer and the next dispatch lands on `slow`, whose
//! `exec_one` faults CODE_OOB. Jump and call targets are checked. Such an
//! instruction (only in malformed bytecode) runs with operands from the pad
//! (or faults on the stack first), and the fault pc is past the end rather
//! than at it. An opcode a handler here runs is always inside the code (the
//! pad's opcodes go to `slow`), so `quicken!` writes inside it.

use super::*;

// --- Handler building blocks ---

/// Dispatch the instruction at `pc` (`$n`: the budget left, the instruction
/// just run included), in the handler's table (`W`).
macro_rules! next {
    ($vm:ident, $pc:expr, $sp:expr, $n:ident) => {{
        let pc: *const u8 = $pc;
        if $n == 1 {
            become budget_out::<W>($vm, pc, $sp, 1)
        }
        // SAFETY: inside the code buffer (code + pad, see the module docs).
        let op = unsafe { *pc };
        become tab::<W>()[op as usize]($vm, unsafe { pc.add(1) }, $sp, $n - 1)
    }};
}

/// Continue at code offset `$t` (checked).
macro_rules! jump {
    ($vm:ident, $t:expr, $sp:expr, $n:ident) => {{
        let t: usize = $t;
        let sp: *mut i32 = $sp;
        if t >= $vm.code.len() {
            return jump_out($vm, t, sp, $n);
        }
        next!($vm, unsafe { $vm.code.ptr().add(t) }, sp, $n)
    }};
}

/// The instruction (opcode just before `$pc`) faults with `$code`.
macro_rules! fail {
    ($vm:ident, $pc:expr, $sp:expr, $n:ident, $code:expr) => {
        become fail_at::<{ $code }>($vm, $pc, $sp, $n)
    };
}

/// At least `$k` values on the stack, or STACK_UNDERFLOW.
macro_rules! need {
    ($vm:ident, $pc:ident, $sp:ident, $n:ident, $k:expr) => {
        if ($sp as usize) < $vm.tc_lo + 4 * $k {
            fail!($vm, $pc, $sp, $n, fault::STACK_UNDERFLOW);
        }
    };
}

/// Push `$v` and continue at `$next_pc`.
macro_rules! push {
    ($vm:ident, $pc:ident, $sp:ident, $n:ident, $v:expr, $next_pc:expr) => {{
        if ($sp as usize) >= $vm.tc_hi {
            fail!($vm, $pc, $sp, $n, fault::STACK_OVERFLOW);
        }
        let v: i32 = $v;
        unsafe {
            *$sp = v;
            next!($vm, $next_pc, $sp.add(1), $n)
        }
    }};
}

/// Superinstruction `$op` (cond / op operand at `$pc`): if that operand has
/// a quickened form, rewrite the opcode to it and run it.
macro_rules! quicken {
    ($vm:ident, $pc:ident, $sp:ident, $n:ident, $op:expr) => {
        let q = quick_of($op, unsafe { *$pc });
        if q != 0 {
            // SAFETY: the opcode is inside the code (module docs); one
            // thread runs the VMs (`VmCode`).
            unsafe { $vm.code.quicken($pc.sub(1), q) };
            become tab::<W>()[q as usize]($vm, $pc, $sp, $n)
        }
    };
}

/// A handler: the VM, the code pointer just past the opcode, the stack
/// pointer (next free slot), the budget left (this instruction included).
type Handler = fn(&mut Vm, *const u8, *mut i32, u32);

/// `exec_one` for the platform of the running burst (`Vm::tc_slow`).
pub(super) type Slow = fn(&mut Vm, usize);

pub(super) fn no_slow(_: &mut Vm, _: usize) {}

fn slow_exec<P: Platform>(vm: &mut Vm, ctx: usize) {
    // SAFETY: set by `run` from its `&mut Ctx<P>`, which outlives the burst.
    let ctx = unsafe { &mut *(ctx as *mut Ctx<P>) };
    vm.exec_one(ctx);
}

/// Runs `vm` (state `Running`, `budget` >= 1) until it leaves `Running` or
/// `budget` instructions ran (more in a critical section); the count run.
pub(super) fn run<P: Platform>(vm: &mut Vm, ctx: &mut Ctx<P>, budget: u32) -> u32 {
    let cp = ctx as *mut Ctx<P>;
    vm.tc_ctx = cp as usize;
    // SAFETY: from the same pointer, valid for the burst.
    vm.tc_objs = unsafe { &raw mut (*cp).objs };
    vm.tc_slow = slow_exec::<P>;
    vm.tc_limit = budget;
    vm.tc_base = 0;
    vm.tc_n = 0;
    if vm.pc as usize >= vm.code.len() {
        vm.exec_one(ctx); // CODE_OOB
        return 1;
    }
    refresh(vm);
    // SAFETY: pc < len; sp <= stack.len() (see `Vm::pop2_raw`).
    let (pc, sp) = unsafe { (vm.code.ptr().add(vm.pc as usize), vm.stack.as_mut_ptr().add(vm.sp as usize)) };
    let op = unsafe { *pc } as usize;
    let table = if vm.wide_addr { &TABLE_W } else { &TABLE_N };
    table[op](vm, unsafe { pc.add(1) }, sp, budget);
    vm.tc_n
}

/// Instructions run when the one with budget `n` left is the last.
#[inline(always)]
fn ran(vm: &Vm, n: u32) -> u32 {
    vm.tc_base + (vm.tc_limit - n + 1)
}

/// The budget is spent and `pc` is the next instruction: leave, unless in a
/// critical section, which goes on one instruction at a time (the `exec_one`
/// loop doesn't count its instructions against the budget).
#[cold]
#[inline(never)]
fn budget_out<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    if vm.critical {
        vm.tc_base = ran(vm, n);
        vm.tc_limit = 1;
        // SAFETY: as in `next!`.
        let op = unsafe { *pc };
        become tab::<W>()[op as usize](vm, unsafe { pc.add(1) }, sp, 1)
    }
    vm.pc = off(vm, pc);
    save_sp(vm, sp);
    vm.tc_n = ran(vm, n);
}

/// Stack bounds for the checks (`sp_floor` and the stack may change in a
/// slow instruction).
#[inline(always)]
fn refresh(vm: &mut Vm) {
    let base = vm.stack.as_mut_ptr() as usize;
    vm.tc_lo = base + vm.sp_floor as usize * 4;
    vm.tc_hi = base + vm.stack.len() * 4;
}

#[inline(always)]
fn off(vm: &Vm, p: *const u8) -> CodeAddr {
    (p as usize - vm.code.ptr() as usize) as CodeAddr
}

#[inline(always)]
fn save_sp(vm: &mut Vm, sp: *mut i32) {
    vm.sp = ((sp as usize - vm.stack.as_ptr() as usize) / 4) as u8;
}

/// The instruction whose opcode is just before `pc` failed with `CODE`:
/// state back to `Vm`, then `Vm::fail`.
#[cold]
#[inline(never)]
fn fail_at<const CODE: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let at = off(vm, pc) - 1;
    vm.op_pc = at;
    vm.pc = at + 1;
    save_sp(vm, sp);
    vm.tc_n = ran(vm, n);
    vm.fail(CODE);
}

/// A jump or call to a target outside the code: the checked path faults it.
#[cold]
#[inline(never)]
fn jump_out(vm: &mut Vm, target: usize, sp: *mut i32, n: u32) {
    vm.pc = target as CodeAddr;
    save_sp(vm, sp);
    vm.tc_n = ran(vm, n);
    if n > 1 || vm.critical {
        (vm.tc_slow)(vm, vm.tc_ctx); // CODE_OOB, as the next instruction
        vm.tc_n += 1;
    }
}

/// Any opcode without a handler here: through `exec_one`.
#[inline(never)]
fn slow<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    vm.pc = off(vm, pc) - 1; // at the opcode: exec_one reads it again
    save_sp(vm, sp);
    (vm.tc_slow)(vm, vm.tc_ctx);
    if vm.state != VmState::Running {
        vm.tc_n = ran(vm, n);
        return;
    }
    if vm.pc as usize >= vm.code.len() {
        // Out of the code (a RET, say): leave; the checked path faults it.
        vm.tc_n = ran(vm, n);
        if n > 1 || vm.critical {
            (vm.tc_slow)(vm, vm.tc_ctx); // CODE_OOB, as the next instruction
            vm.tc_n += 1;
        }
        return;
    }
    refresh(vm);
    // SAFETY: as in `run`.
    let (pc, sp) = unsafe { (vm.code.ptr().add(vm.pc as usize), vm.stack.as_mut_ptr().add(vm.sp as usize)) };
    // A unit switch (a library call or return) may change the address width.
    if vm.wide_addr {
        become resume::<true>(vm, pc, sp, n)
    }
    become resume::<false>(vm, pc, sp, n)
}

/// Continue at `pc` in table `W` (after `slow`).
fn resume<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    next!(vm, pc, sp, n)
}

/// a b → a <op> b.
macro_rules! bin {
    ($name:ident, |$a:ident, $b:ident| $e:expr) => {
        fn $name<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
            need!(vm, pc, sp, n, 2);
            unsafe {
                let $b = *sp.sub(1);
                let $a = *sp.sub(2);
                *sp.sub(2) = $e;
                next!(vm, pc, sp.sub(1), n)
            }
        }
    };
}

bin!(op_add, |a, b| a.wrapping_add(b));
bin!(op_sub, |a, b| a.wrapping_sub(b));
bin!(op_mul, |a, b| a.wrapping_mul(b));
bin!(op_div, |a, b| if b != 0 { a.wrapping_div(b) } else { 0 });
bin!(op_mod, |a, b| if b != 0 { a.wrapping_rem(b) } else { 0 });
bin!(op_and, |a, b| a & b);
bin!(op_or, |a, b| a | b);
bin!(op_shl, |a, b| a << (b as u32 & 31));
bin!(op_shr, |a, b| a >> (b as u32 & 31));
bin!(op_eq, |a, b| (a == b) as i32);
bin!(op_ne, |a, b| (a != b) as i32);
bin!(op_lt, |a, b| (a < b) as i32);
bin!(op_le, |a, b| (a <= b) as i32);
bin!(op_gt, |a, b| (a > b) as i32);
bin!(op_ge, |a, b| (a >= b) as i32);

/// a → <op> a.
macro_rules! unary {
    ($name:ident, |$a:ident| $e:expr) => {
        fn $name<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
            need!(vm, pc, sp, n, 1);
            unsafe {
                let $a = *sp.sub(1);
                *sp.sub(1) = $e;
                next!(vm, pc, sp, n)
            }
        }
    };
}

unary!(op_neg, |a| a.wrapping_neg());
unary!(op_not, |a| (a == 0) as i32);

macro_rules! push_const {
    ($name:ident, $v:expr) => {
        fn $name<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
            push!(vm, pc, sp, n, $v, pc)
        }
    };
}

push_const!(op_push_0, 0);
push_const!(op_push_1, 1);
push_const!(op_push_2, 2);
push_const!(op_push_m1, -1);

macro_rules! load_k {
    ($name:ident, $k:expr) => {
        fn $name<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
            push!(vm, pc, sp, n, vm.var_get(vm.frame_base + $k), pc)
        }
    };
}

load_k!(op_load_0, 0);
load_k!(op_load_1, 1);
load_k!(op_load_2, 2);
load_k!(op_load_3, 3);
load_k!(op_load_4, 4);

macro_rules! store_k {
    ($name:ident, $k:expr) => {
        fn $name<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
            need!(vm, pc, sp, n, 1);
            unsafe {
                let sp = sp.sub(1);
                vm.var_set(vm.frame_base + $k, *sp);
                next!(vm, pc, sp, n)
            }
        }
    };
}

store_k!(op_store_0, 0);
store_k!(op_store_1, 1);
store_k!(op_store_2, 2);
store_k!(op_store_3, 3);
store_k!(op_store_4, 4);

/// The `N` operand bytes at `pc`.
#[inline(always)]
unsafe fn bytes<const N: usize>(pc: *const u8) -> [u8; N] {
    unsafe { core::ptr::read_unaligned(pc as *const [u8; N]) }
}

/// A jump/call target at `pc` (u16, u32 in wide-address images: `W`) and
/// the pointer past it.
#[inline(always)]
unsafe fn addr<const W: bool>(pc: *const u8) -> (usize, *const u8) {
    unsafe {
        if W {
            (u32::from_le_bytes(bytes(pc)) as usize, pc.add(4))
        } else {
            (u16::from_le_bytes(bytes(pc)) as usize, pc.add(2))
        }
    }
}

fn op_pop<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    next!(vm, pc, unsafe { sp.sub(1) }, n)
}

fn op_dup<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    // As `Vm::peek`: an empty stack reads 0.
    let v = if (sp as usize) > vm.tc_lo { unsafe { *sp.sub(1) } } else { 0 };
    push!(vm, pc, sp, n, v, pc)
}

fn op_swap<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 2);
    unsafe {
        core::ptr::swap(sp.sub(1), sp.sub(2));
    }
    next!(vm, pc, sp, n)
}

fn op_push_i8<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, unsafe { *pc } as i8 as i32, unsafe { pc.add(1) })
}

fn op_push_i16<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, i16::from_le_bytes(unsafe { bytes(pc) }) as i32, unsafe { pc.add(2) })
}

fn op_push_i32<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, i32::from_le_bytes(unsafe { bytes(pc) }), unsafe { pc.add(4) })
}

fn op_load<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let slot = vm.resolve_slot(unsafe { *pc });
    push!(vm, pc, sp, n, vm.var_get(slot), unsafe { pc.add(1) })
}

fn op_store<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let sp = sp.sub(1);
        vm.var_set(vm.resolve_slot(*pc), *sp);
        next!(vm, pc.add(1), sp, n)
    }
}

fn op_frame<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let count = unsafe { *pc };
    if vm.frame_base + count as u32 > vm.var_slots {
        fail!(vm, pc, sp, n, fault::VARS_FULL);
    }
    vm.frame_size = count;
    next!(vm, unsafe { pc.add(1) }, sp, n)
}

fn op_frame_a<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    // FRAME n; STORE nargs-1; ..; STORE 0 (the arguments, last on top)
    let [count, nargs] = unsafe { bytes(pc) };
    if vm.frame_base + count as u32 > vm.var_slots {
        fail!(vm, pc, sp, n, fault::VARS_FULL);
    }
    need!(vm, pc, sp, n, nargs as usize);
    vm.frame_size = count;
    // SAFETY: `frame_base + nargs` stays below `vars.len()` (VAR_PAD), the
    // `nargs` values are on the stack (need!), and the two buffers are apart.
    unsafe {
        let sp = sp.sub(nargs as usize);
        let dst = vm.vars.as_mut_ptr().add(vm.frame_base as usize);
        // Most functions take few arguments: no generic copy loop for them.
        match nargs {
            0 => {}
            1 => *dst = *sp,
            2 => {
                *dst = *sp;
                *dst.add(1) = *sp.add(1);
            }
            3 => {
                *dst = *sp;
                *dst.add(1) = *sp.add(1);
                *dst.add(2) = *sp.add(2);
            }
            _ => core::ptr::copy_nonoverlapping(sp, dst, nargs as usize),
        }
        next!(vm, pc.add(2), sp, n)
    }
}

fn op_jmp<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let (t, _) = unsafe { addr::<W>(pc) };
    jump!(vm, t, sp, n)
}

fn op_jz<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let (t, after) = addr::<W>(pc);
        let sp = sp.sub(1);
        if *sp == 0 {
            jump!(vm, t, sp, n)
        }
        next!(vm, after, sp, n)
    }
}

fn op_jnz<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let (t, after) = addr::<W>(pc);
        let sp = sp.sub(1);
        if *sp != 0 {
            jump!(vm, t, sp, n)
        }
        next!(vm, after, sp, n)
    }
}

fn op_call<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let (t, after) = unsafe { addr::<W>(pc) };
    let depth = vm.call_sp as usize;
    if depth >= vm.call_stack.len() {
        fail!(vm, pc, sp, n, fault::CALL_OVERFLOW);
    }
    let ret_addr = off(vm, after) | vm.utag;
    // SAFETY: depth < call_stack.len().
    unsafe {
        *vm.call_stack.get_unchecked_mut(depth) =
            CallFrame { ret_addr, frame_base: vm.frame_base as u16, frame_size: vm.frame_size };
    }
    vm.call_sp += 1;
    // The callee's locals go past the caller's; its FRAME sets their count.
    vm.frame_base += vm.frame_size as u32;
    vm.frame_size = 0;
    jump!(vm, t, sp, n)
}

/// A plain return: no callback or modal dialog running (their returns need
/// `exec_one`'s bookkeeping), a frame to pop.
fn op_ret<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let depth = vm.call_sp as usize;
    if depth == 0 || !vm.cb_frames.is_empty() || !vm.modal_stack.is_empty() {
        become slow::<W>(vm, pc, sp, n)
    }
    // SAFETY: 0 < depth <= call_stack.len() (op_call / exec_one check).
    let f = unsafe { *vm.call_stack.get_unchecked(depth - 1) };
    // A return into another unit (a library's caller): `exec_one` switches.
    let ret = f.ret_addr ^ vm.utag;
    if ret >> 24 != 0 {
        become slow::<W>(vm, pc, sp, n)
    }
    vm.call_sp = (depth - 1) as u8;
    vm.frame_base = f.frame_base as u32;
    vm.frame_size = f.frame_size;
    jump!(vm, ret as usize, sp, n)
}

/// SYSCALL id argc: the platform's handler on the top `argc` values (fewer
/// on the stack, or no handler: `exec_one`, which faults). The app syscalls
/// (`SYS_APP_COUNT..=SYS_APP_SET_BOOT`) are the core's, not the platform's:
/// `exec_one` answers them.
fn op_syscall<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let Some(f) = vm.syscall_fn else {
        become slow::<W>(vm, pc, sp, n)
    };
    let (id, argc) = unsafe { (*pc, *pc.add(1) as usize) };
    if (crate::apps::SYS_APP_COUNT..=crate::apps::SYS_APP_SET_BOOT).contains(&id) {
        become slow::<W>(vm, pc, sp, n)
    }
    let k = argc.min(MAX_SYSCALL_ARGS);
    if (sp as usize) < vm.tc_lo + 4 * k {
        become slow::<W>(vm, pc, sp, n)
    }
    let mut args = [0i32; MAX_SYSCALL_ARGS];
    // SAFETY: k values on the stack (checked); `args` holds k.
    let sp = unsafe {
        let sp = sp.sub(k);
        core::ptr::copy_nonoverlapping(sp, args.as_mut_ptr(), k);
        sp
    };
    // SAFETY: set by `run` from its `Ctx`, which outlives the burst; nothing
    // else holds the pool while a handler runs.
    let objs = unsafe { &mut *vm.tc_objs };
    match f(id, &args[..k], objs) {
        Some(v) => push!(vm, pc, sp, n, v, unsafe { pc.add(2) }),
        None => fail!(vm, pc, sp, n, fault::SYSCALL),
    }
}

// --- Superinstructions: the general forms (quickened when they can be) ---

/// Superinstruction tail: jump to `t` if `a <cond> b`, else on at `after`.
macro_rules! jump_if {
    ($vm:ident, $pc:ident, $sp:ident, $n:ident, $cond:expr, $a:expr, $b:expr, $t:expr, $after:expr) => {
        match int_cmp($cond, $a, $b) {
            Some(true) => jump!($vm, $t, $sp, $n),
            Some(false) => next!($vm, $after, $sp, $n),
            None => fail!($vm, $pc, $sp, $n, fault::BAD_OPCODE),
        }
    };
}

fn op_jcmp<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, OP_JCMP);
    need!(vm, pc, sp, n, 2);
    unsafe {
        let cond = *pc;
        let (t, after) = addr::<W>(pc.add(1));
        let (a, b) = (*sp.sub(2), *sp.sub(1));
        let sp = sp.sub(2);
        jump_if!(vm, pc, sp, n, cond, a, b, t, after)
    }
}

fn op_jcmp_vi<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, OP_JCMP_VI);
    unsafe {
        let [cond, raw, lo, hi] = bytes(pc);
        let (t, after) = addr::<W>(pc.add(4));
        let a = vm.var_get(vm.resolve_slot(raw));
        jump_if!(vm, pc, sp, n, cond, a, i16::from_le_bytes([lo, hi]) as i32, t, after)
    }
}

fn op_jcmp_vv<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, OP_JCMP_VV);
    unsafe {
        let [cond, ra, rb] = bytes(pc);
        let (t, after) = addr::<W>(pc.add(3));
        let a = vm.var_get(vm.resolve_slot(ra));
        let b = vm.var_get(vm.resolve_slot(rb));
        jump_if!(vm, pc, sp, n, cond, a, b, t, after)
    }
}

/// Superinstruction tail: top = top <op> b, then on at `after`.
macro_rules! top_binop {
    ($vm:ident, $pc:ident, $sp:ident, $n:ident, $op:expr, $b:expr, $after:expr) => {{
        need!($vm, $pc, $sp, $n, 1);
        unsafe {
            let top = $sp.sub(1);
            match int_binop($op, *top, $b) {
                Some(v) => *top = v,
                None => fail!($vm, $pc, $sp, $n, fault::BAD_OPCODE),
            }
        }
        next!($vm, $after, $sp, $n)
    }};
}

fn op_bin_i<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, OP_BIN_I);
    let [op, lo, hi] = unsafe { bytes(pc) };
    top_binop!(vm, pc, sp, n, op, i16::from_le_bytes([lo, hi]) as i32, unsafe { pc.add(3) })
}

fn op_bin_i32<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let [op, a, b, c, d] = unsafe { bytes(pc) };
    top_binop!(vm, pc, sp, n, op, i32::from_le_bytes([a, b, c, d]), unsafe { pc.add(5) })
}

fn op_bin_v<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, OP_BIN_V);
    let [op, raw] = unsafe { bytes(pc) };
    let b = vm.var_get(vm.resolve_slot(raw));
    top_binop!(vm, pc, sp, n, op, b, unsafe { pc.add(2) })
}

fn op_add_vi<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let [raw, lo, hi] = unsafe { bytes(pc) };
    let slot = vm.resolve_slot(raw);
    vm.var_set(slot, vm.var_get(slot).wrapping_add(i16::from_le_bytes([lo, hi]) as i32));
    next!(vm, unsafe { pc.add(3) }, sp, n)
}

fn op_store_op<const W: bool>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, OP_STORE_OP);
    need!(vm, pc, sp, n, 1);
    unsafe {
        let [op, raw] = bytes(pc);
        let sp = sp.sub(1);
        let slot = vm.resolve_slot(raw);
        match int_binop(op, vm.var_get(slot), *sp) {
            Some(v) => vm.var_set(slot, v),
            None => fail!(vm, pc, sp, n, fault::BAD_OPCODE),
        }
        next!(vm, pc.add(2), sp, n)
    }
}

// --- Quickened superinstructions (`QUICK`): the cond / op built in (`C`),
// the operand byte skipped. Same checks, in the same order, as the general
// forms above. ---

/// `a <C> b` (C a comparison opcode).
#[inline(always)]
fn cmp<const C: u8>(a: i32, b: i32) -> bool {
    matches!(int_cmp(C, a, b), Some(true))
}

/// `a <C> b` (C a binary-op opcode).
#[inline(always)]
fn binop<const C: u8>(a: i32, b: i32) -> i32 {
    match int_binop(C, a, b) {
        Some(v) => v,
        None => 0,
    }
}

/// Jump to `t` if `$c`, else on at `after`.
macro_rules! branch {
    ($vm:ident, $sp:ident, $n:ident, $c:expr, $t:expr, $after:expr) => {{
        if $c {
            jump!($vm, $t, $sp, $n)
        }
        next!($vm, $after, $sp, $n)
    }};
}

fn q_jcmp<const W: bool, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 2);
    unsafe {
        let (t, after) = addr::<W>(pc.add(1));
        let (a, b) = (*sp.sub(2), *sp.sub(1));
        let sp = sp.sub(2);
        branch!(vm, sp, n, cmp::<C>(a, b), t, after)
    }
}

fn q_jcmp_vi<const W: bool, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    unsafe {
        let [_, raw, lo, hi] = bytes(pc);
        let (t, after) = addr::<W>(pc.add(4));
        let a = vm.var_get(vm.resolve_slot(raw));
        branch!(vm, sp, n, cmp::<C>(a, i16::from_le_bytes([lo, hi]) as i32), t, after)
    }
}

fn q_jcmp_vv<const W: bool, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    unsafe {
        let [_, ra, rb] = bytes(pc);
        let (t, after) = addr::<W>(pc.add(3));
        let a = vm.var_get(vm.resolve_slot(ra));
        let b = vm.var_get(vm.resolve_slot(rb));
        branch!(vm, sp, n, cmp::<C>(a, b), t, after)
    }
}

fn q_bin_i<const W: bool, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let [_, lo, hi] = bytes(pc);
        let top = sp.sub(1);
        *top = binop::<C>(*top, i16::from_le_bytes([lo, hi]) as i32);
        next!(vm, pc.add(3), sp, n)
    }
}

fn q_bin_v<const W: bool, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let b = vm.var_get(vm.resolve_slot(unsafe { *pc.add(1) }));
    need!(vm, pc, sp, n, 1);
    unsafe {
        let top = sp.sub(1);
        *top = binop::<C>(*top, b);
        next!(vm, pc.add(2), sp, n)
    }
}

fn q_store_op<const W: bool, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let sp = sp.sub(1);
        let slot = vm.resolve_slot(*pc.add(1));
        vm.var_set(slot, binop::<C>(vm.var_get(slot), *sp));
        next!(vm, pc.add(2), sp, n)
    }
}

// --- The tables ---

static TABLE_N: [Handler; 256] = table::<false>();
static TABLE_W: [Handler; 256] = table::<true>();

/// The dispatch table for address width `W`.
#[inline(always)]
fn tab<const W: bool>() -> &'static [Handler; 256] {
    if W { &TABLE_W } else { &TABLE_N }
}

const fn table<const W: bool>() -> [Handler; 256] {
    let mut t: [Handler; 256] = [slow::<W> as Handler; 256];
    t[OP_POP as usize] = op_pop::<W>;
    t[OP_DUP as usize] = op_dup::<W>;
    t[OP_SWAP as usize] = op_swap::<W>;
    t[OP_ADD as usize] = op_add::<W>;
    t[OP_SUB as usize] = op_sub::<W>;
    t[OP_MUL as usize] = op_mul::<W>;
    t[OP_DIV as usize] = op_div::<W>;
    t[OP_MOD as usize] = op_mod::<W>;
    t[OP_NEG as usize] = op_neg::<W>;
    t[OP_AND as usize] = op_and::<W>;
    t[OP_OR as usize] = op_or::<W>;
    t[OP_NOT as usize] = op_not::<W>;
    t[OP_EQ as usize] = op_eq::<W>;
    t[OP_NE as usize] = op_ne::<W>;
    t[OP_LT as usize] = op_lt::<W>;
    t[OP_LE as usize] = op_le::<W>;
    t[OP_GT as usize] = op_gt::<W>;
    t[OP_GE as usize] = op_ge::<W>;
    t[OP_SHL as usize] = op_shl::<W>;
    t[OP_SHR as usize] = op_shr::<W>;
    t[OP_FRAME as usize] = op_frame::<W>;
    t[OP_PUSH_0 as usize] = op_push_0::<W>;
    t[OP_PUSH_1 as usize] = op_push_1::<W>;
    t[OP_PUSH_2 as usize] = op_push_2::<W>;
    t[OP_PUSH_M1 as usize] = op_push_m1::<W>;
    t[OP_LOAD_0 as usize] = op_load_0::<W>;
    t[OP_LOAD_1 as usize] = op_load_1::<W>;
    t[OP_LOAD_2 as usize] = op_load_2::<W>;
    t[OP_LOAD_3 as usize] = op_load_3::<W>;
    t[OP_LOAD_4 as usize] = op_load_4::<W>;
    t[OP_STORE_0 as usize] = op_store_0::<W>;
    t[OP_STORE_1 as usize] = op_store_1::<W>;
    t[OP_STORE_2 as usize] = op_store_2::<W>;
    t[OP_STORE_3 as usize] = op_store_3::<W>;
    t[OP_STORE_4 as usize] = op_store_4::<W>;
    t[OP_PUSH_I8 as usize] = op_push_i8::<W>;
    t[OP_PUSH_I16 as usize] = op_push_i16::<W>;
    t[OP_PUSH_I32 as usize] = op_push_i32::<W>;
    t[OP_LOAD as usize] = op_load::<W>;
    t[OP_STORE as usize] = op_store::<W>;
    t[OP_JMP as usize] = op_jmp::<W>;
    t[OP_JZ as usize] = op_jz::<W>;
    t[OP_JNZ as usize] = op_jnz::<W>;
    t[OP_CALL as usize] = op_call::<W>;
    t[OP_JCMP as usize] = op_jcmp::<W>;
    t[OP_JCMP_VI as usize] = op_jcmp_vi::<W>;
    t[OP_JCMP_VV as usize] = op_jcmp_vv::<W>;
    t[OP_BIN_I as usize] = op_bin_i::<W>;
    t[OP_BIN_I32 as usize] = op_bin_i32::<W>;
    t[OP_BIN_V as usize] = op_bin_v::<W>;
    t[OP_ADD_VI as usize] = op_add_vi::<W>;
    t[OP_STORE_OP as usize] = op_store_op::<W>;
    t[OP_FRAME_A as usize] = op_frame_a::<W>;
    t[OP_RET as usize] = op_ret::<W>;
    t[OP_SYSCALL as usize] = op_syscall::<W>;

    // The quickened forms, in `QUICK`'s order (checked here, at compile
    // time).
    let mut i = 0;
    macro_rules! quick {
        ($($h:ident $op:ident $c:ident),* $(,)?) => {$(
            assert!(QUICK[i].0 == $op && QUICK[i].1 == $c, "tc::table and QUICK differ");
            t[Q_FIRST as usize + i] = $h::<W, $c>;
            i += 1;
        )*};
    }
    quick!(
        q_jcmp OP_JCMP OP_EQ, q_jcmp OP_JCMP OP_NE, q_jcmp OP_JCMP OP_LT,
        q_jcmp OP_JCMP OP_LE, q_jcmp OP_JCMP OP_GT, q_jcmp OP_JCMP OP_GE,
        q_jcmp_vi OP_JCMP_VI OP_EQ, q_jcmp_vi OP_JCMP_VI OP_NE, q_jcmp_vi OP_JCMP_VI OP_LT,
        q_jcmp_vi OP_JCMP_VI OP_LE, q_jcmp_vi OP_JCMP_VI OP_GT, q_jcmp_vi OP_JCMP_VI OP_GE,
        q_jcmp_vv OP_JCMP_VV OP_EQ, q_jcmp_vv OP_JCMP_VV OP_NE, q_jcmp_vv OP_JCMP_VV OP_LT,
        q_jcmp_vv OP_JCMP_VV OP_LE, q_jcmp_vv OP_JCMP_VV OP_GT, q_jcmp_vv OP_JCMP_VV OP_GE,
        q_bin_i OP_BIN_I OP_ADD, q_bin_i OP_BIN_I OP_SUB, q_bin_i OP_BIN_I OP_MUL,
        q_bin_i OP_BIN_I OP_AND, q_bin_i OP_BIN_I OP_OR, q_bin_i OP_BIN_I OP_SHL,
        q_bin_i OP_BIN_I OP_SHR,
        q_bin_v OP_BIN_V OP_ADD, q_bin_v OP_BIN_V OP_SUB,
        q_store_op OP_STORE_OP OP_ADD,
    );
    assert!(i == QUICK.len(), "tc::table and QUICK differ");
    t
}
