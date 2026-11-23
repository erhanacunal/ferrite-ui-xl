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
//! Handlers are generic over the platform `P` (one table per platform,
//! `Table::<P>::T`; a firmware has one): the burst's `Ctx<P>` is at hand
//! (`ctx`), so a handler may reach any of it, as `exec_one` does.
//!
//! Quickening: an instruction with a quickened form (`QUICK`: a
//! superinstruction by its cond / op and slot kind, LOAD / STORE / ADD_VI /
//! ARR_LOAD_V by their slot's kind) rewrites its opcode to that form the first time it runs here
//! (`quicken!`); the form's handler has the operation / slot kind built in,
//! with no dispatch on the operand (`q_*`). `exec_one` runs both the same.
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
/// just run included).
macro_rules! next {
    ($vm:ident, $pc:expr, $sp:expr, $n:ident) => {{
        let pc: *const u8 = $pc;
        if $n == 1 {
            become budget_out::<P>($vm, pc, $sp, 1)
        }
        // SAFETY: inside the code buffer (code + pad, see the module docs).
        let op = unsafe { *pc };
        become tab::<P>()[op as usize]($vm, unsafe { pc.add(1) }, $sp, $n - 1)
    }};
}

/// Continue at code offset `$t` (checked).
macro_rules! jump {
    ($vm:ident, $t:expr, $sp:expr, $n:ident) => {{
        let t: usize = $t;
        let sp: *mut i32 = $sp;
        if t >= $vm.code.len() {
            return jump_out::<P>($vm, t, sp, $n);
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

/// If `$q` is a quickened form (not 0), rewrite the running instruction's
/// opcode to it and run it.
macro_rules! quicken {
    ($vm:ident, $pc:ident, $sp:ident, $n:ident, $q:expr) => {
        let q: u8 = $q;
        if q != 0 {
            // SAFETY: the opcode is inside the code (module docs); one
            // thread runs the VMs (`VmCode`).
            unsafe { $vm.code.quicken($pc.sub(1), q) };
            become tab::<P>()[q as usize]($vm, $pc, $sp, $n)
        }
    };
}

/// A handler: the VM, the code pointer just past the opcode, the stack
/// pointer (next free slot), the budget left (this instruction included).
type Handler = fn(&mut Vm, *const u8, *mut i32, u32);

/// The running burst's `Ctx` (`Vm::tc_ctx`).
#[inline(always)]
fn ctx<'a, P: Platform>(vm: &Vm) -> &'a mut Ctx<P> {
    // SAFETY: set by `run` from its `&mut Ctx<P>`, which outlives the burst;
    // nothing else holds it while a handler runs.
    unsafe { &mut *(vm.tc_ctx as *mut Ctx<P>) }
}

/// The instruction at `vm.pc` through `exec_one` (inlined here once).
#[inline(never)]
fn exec_slow<P: Platform>(vm: &mut Vm) {
    vm.exec_one(ctx::<P>(vm));
}

/// Runs `vm` (state `Running`, `budget` >= 1) until it leaves `Running` or
/// `budget` instructions ran (more in a critical section); the count run.
pub(super) fn run<P: Platform>(vm: &mut Vm, ctx: &mut Ctx<P>, budget: u32) -> u32 {
    let cp = ctx as *mut Ctx<P>;
    vm.tc_ctx = cp as usize;
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
    tab::<P>()[op](vm, unsafe { pc.add(1) }, sp, budget);
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
fn budget_out<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    if vm.critical {
        vm.tc_base = ran(vm, n);
        vm.tc_limit = 1;
        // SAFETY: as in `next!`.
        let op = unsafe { *pc };
        become tab::<P>()[op as usize](vm, unsafe { pc.add(1) }, sp, 1)
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
fn jump_out<P: Platform>(vm: &mut Vm, target: usize, sp: *mut i32, n: u32) {
    vm.pc = target as CodeAddr;
    save_sp(vm, sp);
    vm.tc_n = ran(vm, n);
    if n > 1 || vm.critical {
        exec_slow::<P>(vm); // CODE_OOB, as the next instruction
        vm.tc_n += 1;
    }
}

/// Any opcode without a handler here: through `exec_one`.
#[inline(never)]
fn slow<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    vm.pc = off(vm, pc) - 1; // at the opcode: exec_one reads it again
    save_sp(vm, sp);
    exec_slow::<P>(vm);
    if vm.state != VmState::Running {
        vm.tc_n = ran(vm, n);
        return;
    }
    if vm.pc as usize >= vm.code.len() {
        // Out of the code (a RET, say): leave; the checked path faults it.
        vm.tc_n = ran(vm, n);
        if n > 1 || vm.critical {
            exec_slow::<P>(vm); // CODE_OOB, as the next instruction
            vm.tc_n += 1;
        }
        return;
    }
    refresh(vm);
    // SAFETY: as in `run`.
    let (pc, sp) = unsafe { (vm.code.ptr().add(vm.pc as usize), vm.stack.as_mut_ptr().add(vm.sp as usize)) };
    next!(vm, pc, sp, n)
}

/// a b → a <op> b.
macro_rules! bin {
    ($name:ident, |$a:ident, $b:ident| $e:expr) => {
        fn $name<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
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
        fn $name<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
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
        fn $name<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
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
        fn $name<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
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
        fn $name<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
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

/// A jump/call target at `pc` (u32) and the pointer past it.
#[inline(always)]
unsafe fn addr(pc: *const u8) -> (usize, *const u8) {
    unsafe { (u32::from_le_bytes(bytes(pc)) as usize, pc.add(4)) }
}

fn op_pop<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    next!(vm, pc, unsafe { sp.sub(1) }, n)
}

fn op_dup<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    // As `Vm::peek`: an empty stack reads 0.
    let v = if (sp as usize) > vm.tc_lo { unsafe { *sp.sub(1) } } else { 0 };
    push!(vm, pc, sp, n, v, pc)
}

fn op_swap<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 2);
    unsafe {
        core::ptr::swap(sp.sub(1), sp.sub(2));
    }
    next!(vm, pc, sp, n)
}

fn op_push_i8<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, unsafe { *pc } as i8 as i32, unsafe { pc.add(1) })
}

fn op_push_i16<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, i16::from_le_bytes(unsafe { bytes(pc) }) as i32, unsafe { pc.add(2) })
}

fn op_push_i32<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, i32::from_le_bytes(unsafe { bytes(pc) }), unsafe { pc.add(4) })
}

/// The variable slot of a local slot operand (bit 7 ignored).
#[inline(always)]
fn local(vm: &Vm, raw: u8) -> u32 {
    vm.frame_base + (raw & 0x7F) as u32
}

/// The variable slot of a global slot operand (bit 7 ignored).
#[inline(always)]
fn global(vm: &Vm, raw: u8) -> u32 {
    vm.gbase + (raw & 0x7F) as u32
}

/// LOAD / STORE: always quickened (by the slot's kind).
fn op_load<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let q = if unsafe { *pc } & 0x80 != 0 { Q_LOAD_L } else { Q_LOAD_G };
    // SAFETY: as in `quicken!`.
    unsafe { vm.code.quicken(pc.sub(1), q) };
    become tab::<P>()[q as usize](vm, pc, sp, n)
}

fn op_store<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let q = if unsafe { *pc } & 0x80 != 0 { Q_STORE_L } else { Q_STORE_G };
    // SAFETY: as in `quicken!`.
    unsafe { vm.code.quicken(pc.sub(1), q) };
    become tab::<P>()[q as usize](vm, pc, sp, n)
}

fn op_frame<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let count = unsafe { *pc };
    if vm.frame_base + count as u32 > vm.var_slots {
        fail!(vm, pc, sp, n, fault::VARS_FULL);
    }
    vm.frame_size = count;
    next!(vm, unsafe { pc.add(1) }, sp, n)
}

fn op_frame_a<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
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

fn op_jmp<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let (t, _) = unsafe { addr(pc) };
    jump!(vm, t, sp, n)
}

fn op_jz<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let (t, after) = addr(pc);
        let sp = sp.sub(1);
        if *sp == 0 {
            jump!(vm, t, sp, n)
        }
        next!(vm, after, sp, n)
    }
}

fn op_jnz<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let (t, after) = addr(pc);
        let sp = sp.sub(1);
        if *sp != 0 {
            jump!(vm, t, sp, n)
        }
        next!(vm, after, sp, n)
    }
}

fn op_call<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let (t, after) = unsafe { addr(pc) };
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
fn op_ret<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let depth = vm.call_sp as usize;
    if depth == 0 || !vm.cb_frames.is_empty() || !vm.modal_stack.is_empty() {
        become slow::<P>(vm, pc, sp, n)
    }
    // SAFETY: 0 < depth <= call_stack.len() (op_call / exec_one check).
    let f = unsafe { *vm.call_stack.get_unchecked(depth - 1) };
    // A return into another unit (a library's caller): `exec_one` switches.
    let ret = f.ret_addr ^ vm.utag;
    if ret >> 24 != 0 {
        become slow::<P>(vm, pc, sp, n)
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
fn op_syscall<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let Some(f) = vm.syscall_fn else {
        become slow::<P>(vm, pc, sp, n)
    };
    let (id, argc) = unsafe { (*pc, *pc.add(1) as usize) };
    if (crate::apps::SYS_APP_COUNT..=crate::apps::SYS_APP_SET_BOOT).contains(&id) {
        become slow::<P>(vm, pc, sp, n)
    }
    let k = argc.min(MAX_SYSCALL_ARGS);
    if (sp as usize) < vm.tc_lo + 4 * k {
        become slow::<P>(vm, pc, sp, n)
    }
    let mut args = [0i32; MAX_SYSCALL_ARGS];
    // SAFETY: k values on the stack (checked); `args` holds k.
    let sp = unsafe {
        let sp = sp.sub(k);
        core::ptr::copy_nonoverlapping(sp, args.as_mut_ptr(), k);
        sp
    };
    let objs = &mut ctx::<P>(vm).objs;
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

fn op_jcmp<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, quick_of(OP_JCMP, unsafe { *pc }, 0));
    need!(vm, pc, sp, n, 2);
    unsafe {
        let cond = *pc;
        let (t, after) = addr(pc.add(1));
        let (a, b) = (*sp.sub(2), *sp.sub(1));
        let sp = sp.sub(2);
        jump_if!(vm, pc, sp, n, cond, a, b, t, after)
    }
}

fn op_jcmp_vi<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, unsafe { quick_of(OP_JCMP_VI, *pc, *pc.add(1)) });
    unsafe {
        let [cond, raw, lo, hi] = bytes(pc);
        let (t, after) = addr(pc.add(4));
        let a = vm.var_get(vm.resolve_slot(raw));
        jump_if!(vm, pc, sp, n, cond, a, i16::from_le_bytes([lo, hi]) as i32, t, after)
    }
}

fn op_jcmp_vv<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, quick_of(OP_JCMP_VV, unsafe { *pc }, 0));
    unsafe {
        let [cond, ra, rb] = bytes(pc);
        let (t, after) = addr(pc.add(3));
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

fn op_bin_i<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, quick_of(OP_BIN_I, unsafe { *pc }, 0));
    let [op, lo, hi] = unsafe { bytes(pc) };
    top_binop!(vm, pc, sp, n, op, i16::from_le_bytes([lo, hi]) as i32, unsafe { pc.add(3) })
}

fn op_bin_i32<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let [op, a, b, c, d] = unsafe { bytes(pc) };
    top_binop!(vm, pc, sp, n, op, i32::from_le_bytes([a, b, c, d]), unsafe { pc.add(5) })
}

fn op_bin_v<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, unsafe { quick_of(OP_BIN_V, *pc, *pc.add(1)) });
    let [op, raw] = unsafe { bytes(pc) };
    let b = vm.var_get(vm.resolve_slot(raw));
    top_binop!(vm, pc, sp, n, op, b, unsafe { pc.add(2) })
}

fn op_add_vi<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, if unsafe { *pc } & 0x80 != 0 { Q_ADD_VI_L } else { 0 });
    let [raw, lo, hi] = unsafe { bytes(pc) };
    let slot = vm.resolve_slot(raw);
    vm.var_set(slot, vm.var_get(slot).wrapping_add(i16::from_le_bytes([lo, hi]) as i32));
    next!(vm, unsafe { pc.add(3) }, sp, n)
}

fn op_store_op<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    quicken!(vm, pc, sp, n, unsafe { quick_of(OP_STORE_OP, *pc, *pc.add(1)) });
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

fn q_jcmp<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 2);
    unsafe {
        let (t, after) = addr(pc.add(1));
        let (a, b) = (*sp.sub(2), *sp.sub(1));
        let sp = sp.sub(2);
        branch!(vm, sp, n, cmp::<C>(a, b), t, after)
    }
}

fn q_jcmp_vi<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    unsafe {
        let [_, raw, lo, hi] = bytes(pc);
        let (t, after) = addr(pc.add(4));
        let a = vm.var_get(local(vm, raw));
        branch!(vm, sp, n, cmp::<C>(a, i16::from_le_bytes([lo, hi]) as i32), t, after)
    }
}

fn q_jcmp_vv<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    unsafe {
        let [_, ra, rb] = bytes(pc);
        let (t, after) = addr(pc.add(3));
        let a = vm.var_get(vm.resolve_slot(ra));
        let b = vm.var_get(vm.resolve_slot(rb));
        branch!(vm, sp, n, cmp::<C>(a, b), t, after)
    }
}

fn q_bin_i<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let [_, lo, hi] = bytes(pc);
        let top = sp.sub(1);
        *top = binop::<C>(*top, i16::from_le_bytes([lo, hi]) as i32);
        next!(vm, pc.add(3), sp, n)
    }
}

/// BIN_V on a local / global slot.
fn q_bin_v_l<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let b = vm.var_get(local(vm, unsafe { *pc.add(1) }));
    need!(vm, pc, sp, n, 1);
    unsafe {
        let top = sp.sub(1);
        *top = binop::<C>(*top, b);
        next!(vm, pc.add(2), sp, n)
    }
}

fn q_bin_v_g<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let b = vm.var_get(global(vm, unsafe { *pc.add(1) }));
    need!(vm, pc, sp, n, 1);
    unsafe {
        let top = sp.sub(1);
        *top = binop::<C>(*top, b);
        next!(vm, pc.add(2), sp, n)
    }
}

fn q_store_op<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let sp = sp.sub(1);
        let slot = local(vm, *pc.add(1));
        vm.var_set(slot, binop::<C>(vm.var_get(slot), *sp));
        next!(vm, pc.add(2), sp, n)
    }
}

/// LOAD of a local / global slot (`C`: unused).
fn q_load_l<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, vm.var_get(local(vm, unsafe { *pc })), unsafe { pc.add(1) })
}

fn q_load_g<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    push!(vm, pc, sp, n, vm.var_get(global(vm, unsafe { *pc })), unsafe { pc.add(1) })
}

/// STORE to a local / global slot.
fn q_store_l<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let sp = sp.sub(1);
        vm.var_set(local(vm, *pc), *sp);
        next!(vm, pc.add(1), sp, n)
    }
}

fn q_store_g<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    unsafe {
        let sp = sp.sub(1);
        vm.var_set(global(vm, *pc), *sp);
        next!(vm, pc.add(1), sp, n)
    }
}

/// ADD_VI on a local slot.
fn q_add_vi_l<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let [raw, lo, hi] = unsafe { bytes(pc) };
    let slot = local(vm, raw);
    vm.var_set(slot, vm.var_get(slot).wrapping_add(i16::from_le_bytes([lo, hi]) as i32));
    next!(vm, unsafe { pc.add(3) }, sp, n)
}

/// `*$res` = element `$idx` of the array `$id`, the stack ending past it
/// (ARR_LOAD, ARR_LOAD_V); a fault (BAD_ARRAY / ARRAY_INDEX) with the
/// operands popped (the stack ending at `$res`), as `exec_one`.
macro_rules! arr_load {
    ($vm:ident, $pc:ident, $res:ident, $n:ident, $id:expr, $idx:expr, $after:expr) => {{
        let (id, idx): (i32, i32) = ($id, $idx);
        let objs = &ctx::<P>($vm).objs;
        let Some(arr) = (if id < 0 { None } else { objs.arr(id as ObjPtr) }) else {
            fail!($vm, $pc, $res, $n, fault::BAD_ARRAY)
        };
        let Some(&v) = arr.get(idx as usize) else {
            fail!($vm, $pc, $res, $n, fault::ARRAY_INDEX)
        };
        unsafe {
            *$res = v;
            next!($vm, $after, $res.add(1), $n)
        }
    }};
}

fn op_arr_load<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 2);
    let res = unsafe { sp.sub(2) };
    arr_load!(vm, pc, res, n, unsafe { *res }, unsafe { *sp.sub(1) }, pc)
}

/// ARR_LOAD_V: always quickened (by the slot's kind).
fn op_arr_load_v<P: Platform>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    let q = if unsafe { *pc } & 0x80 != 0 { Q_ARR_LOAD_V_L } else { Q_ARR_LOAD_V_G };
    // SAFETY: as in `quicken!`.
    unsafe { vm.code.quicken(pc.sub(1), q) };
    become tab::<P>()[q as usize](vm, pc, sp, n)
}

/// ARR_LOAD_V of an array in a local / global slot.
fn q_arr_load_v_l<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    let res = unsafe { sp.sub(1) };
    arr_load!(vm, pc, res, n, vm.var_get(local(vm, unsafe { *pc })), unsafe { *res }, unsafe { pc.add(1) })
}

fn q_arr_load_v_g<P: Platform, const C: u8>(vm: &mut Vm, pc: *const u8, sp: *mut i32, n: u32) {
    need!(vm, pc, sp, n, 1);
    let res = unsafe { sp.sub(1) };
    arr_load!(vm, pc, res, n, vm.var_get(global(vm, unsafe { *pc })), unsafe { *res }, unsafe { pc.add(1) })
}

// --- The tables ---

/// The dispatch table of platform `P` (one per platform: a board's firmware
/// has one).
struct Table<P>(core::marker::PhantomData<P>);

impl<P: Platform> Table<P> {
    const T: [Handler; 256] = table::<P>();
}

#[inline(always)]
fn tab<P: Platform>() -> &'static [Handler; 256] {
    &Table::<P>::T
}

const fn table<P: Platform>() -> [Handler; 256] {
    let mut t: [Handler; 256] = [slow::<P> as Handler; 256];
    t[OP_POP as usize] = op_pop::<P>;
    t[OP_DUP as usize] = op_dup::<P>;
    t[OP_SWAP as usize] = op_swap::<P>;
    t[OP_ADD as usize] = op_add::<P>;
    t[OP_SUB as usize] = op_sub::<P>;
    t[OP_MUL as usize] = op_mul::<P>;
    t[OP_DIV as usize] = op_div::<P>;
    t[OP_MOD as usize] = op_mod::<P>;
    t[OP_NEG as usize] = op_neg::<P>;
    t[OP_AND as usize] = op_and::<P>;
    t[OP_OR as usize] = op_or::<P>;
    t[OP_NOT as usize] = op_not::<P>;
    t[OP_EQ as usize] = op_eq::<P>;
    t[OP_NE as usize] = op_ne::<P>;
    t[OP_LT as usize] = op_lt::<P>;
    t[OP_LE as usize] = op_le::<P>;
    t[OP_GT as usize] = op_gt::<P>;
    t[OP_GE as usize] = op_ge::<P>;
    t[OP_SHL as usize] = op_shl::<P>;
    t[OP_SHR as usize] = op_shr::<P>;
    t[OP_FRAME as usize] = op_frame::<P>;
    t[OP_PUSH_0 as usize] = op_push_0::<P>;
    t[OP_PUSH_1 as usize] = op_push_1::<P>;
    t[OP_PUSH_2 as usize] = op_push_2::<P>;
    t[OP_PUSH_M1 as usize] = op_push_m1::<P>;
    t[OP_LOAD_0 as usize] = op_load_0::<P>;
    t[OP_LOAD_1 as usize] = op_load_1::<P>;
    t[OP_LOAD_2 as usize] = op_load_2::<P>;
    t[OP_LOAD_3 as usize] = op_load_3::<P>;
    t[OP_LOAD_4 as usize] = op_load_4::<P>;
    t[OP_STORE_0 as usize] = op_store_0::<P>;
    t[OP_STORE_1 as usize] = op_store_1::<P>;
    t[OP_STORE_2 as usize] = op_store_2::<P>;
    t[OP_STORE_3 as usize] = op_store_3::<P>;
    t[OP_STORE_4 as usize] = op_store_4::<P>;
    t[OP_PUSH_I8 as usize] = op_push_i8::<P>;
    t[OP_PUSH_I16 as usize] = op_push_i16::<P>;
    t[OP_PUSH_I32 as usize] = op_push_i32::<P>;
    t[OP_LOAD as usize] = op_load::<P>;
    t[OP_STORE as usize] = op_store::<P>;
    t[OP_JMP as usize] = op_jmp::<P>;
    t[OP_JZ as usize] = op_jz::<P>;
    t[OP_JNZ as usize] = op_jnz::<P>;
    t[OP_CALL as usize] = op_call::<P>;
    t[OP_JCMP as usize] = op_jcmp::<P>;
    t[OP_JCMP_VI as usize] = op_jcmp_vi::<P>;
    t[OP_JCMP_VV as usize] = op_jcmp_vv::<P>;
    t[OP_BIN_I as usize] = op_bin_i::<P>;
    t[OP_BIN_I32 as usize] = op_bin_i32::<P>;
    t[OP_BIN_V as usize] = op_bin_v::<P>;
    t[OP_ADD_VI as usize] = op_add_vi::<P>;
    t[OP_STORE_OP as usize] = op_store_op::<P>;
    t[OP_FRAME_A as usize] = op_frame_a::<P>;
    t[OP_RET as usize] = op_ret::<P>;
    t[OP_SYSCALL as usize] = op_syscall::<P>;
    t[OP_ARR_LOAD as usize] = op_arr_load::<P>;
    t[OP_ARR_LOAD_V as usize] = op_arr_load_v::<P>;

    // The quickened forms, in `QUICK`'s order (checked here, at compile
    // time).
    let mut i = 0;
    macro_rules! quick {
        ($($h:ident $op:ident $c:tt $k:ident),* $(,)?) => {$(
            assert!(QUICK[i].0 == $op && QUICK[i].1 == $c && QUICK[i].2 == $k, "tc::table and QUICK differ");
            t[Q_FIRST as usize + i] = $h::<P, $c>;
            i += 1;
        )*};
    }
    quick!(
        q_jcmp OP_JCMP OP_EQ SLOT_ANY, q_jcmp OP_JCMP OP_NE SLOT_ANY, q_jcmp OP_JCMP OP_LT SLOT_ANY,
        q_jcmp OP_JCMP OP_LE SLOT_ANY, q_jcmp OP_JCMP OP_GT SLOT_ANY, q_jcmp OP_JCMP OP_GE SLOT_ANY,
        q_jcmp_vi OP_JCMP_VI OP_EQ SLOT_L, q_jcmp_vi OP_JCMP_VI OP_NE SLOT_L,
        q_jcmp_vi OP_JCMP_VI OP_LT SLOT_L, q_jcmp_vi OP_JCMP_VI OP_LE SLOT_L,
        q_jcmp_vi OP_JCMP_VI OP_GT SLOT_L, q_jcmp_vi OP_JCMP_VI OP_GE SLOT_L,
        q_jcmp_vv OP_JCMP_VV OP_EQ SLOT_ANY, q_jcmp_vv OP_JCMP_VV OP_NE SLOT_ANY,
        q_jcmp_vv OP_JCMP_VV OP_LT SLOT_ANY, q_jcmp_vv OP_JCMP_VV OP_LE SLOT_ANY,
        q_jcmp_vv OP_JCMP_VV OP_GT SLOT_ANY, q_jcmp_vv OP_JCMP_VV OP_GE SLOT_ANY,
        q_bin_i OP_BIN_I OP_ADD SLOT_ANY, q_bin_i OP_BIN_I OP_SUB SLOT_ANY, q_bin_i OP_BIN_I OP_MUL SLOT_ANY,
        q_bin_i OP_BIN_I OP_DIV SLOT_ANY, q_bin_i OP_BIN_I OP_MOD SLOT_ANY, q_bin_i OP_BIN_I OP_AND SLOT_ANY,
        q_bin_i OP_BIN_I OP_OR SLOT_ANY, q_bin_i OP_BIN_I OP_SHL SLOT_ANY, q_bin_i OP_BIN_I OP_SHR SLOT_ANY,
        q_bin_v_l OP_BIN_V OP_ADD SLOT_L, q_bin_v_g OP_BIN_V OP_ADD SLOT_G,
        q_bin_v_l OP_BIN_V OP_SUB SLOT_L, q_bin_v_g OP_BIN_V OP_SUB SLOT_G,
        q_store_op OP_STORE_OP OP_ADD SLOT_L,
        q_load_l OP_LOAD 0 SLOT_L, q_load_g OP_LOAD 0 SLOT_G,
        q_store_l OP_STORE 0 SLOT_L, q_store_g OP_STORE 0 SLOT_G,
        q_add_vi_l OP_ADD_VI 0 SLOT_L,
        q_arr_load_v_l OP_ARR_LOAD_V 0 SLOT_L, q_arr_load_v_g OP_ARR_LOAD_V 0 SLOT_G,
    );
    assert!(i == QUICK.len(), "tc::table and QUICK differ");
    t
}
