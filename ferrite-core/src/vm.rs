extern crate alloc;

use crate::canvas::Canvas;
use crate::ctx::Ctx;
use crate::platform::Platform;
use crate::proto::{
    self, PROP_ALPHA, PROP_BG_COLOR, PROP_BORDER_B, PROP_BORDER_COLOR, PROP_BORDER_EDGES,
    PROP_BORDER_L, PROP_BORDER_R, PROP_BORDER_RADIUS, PROP_BORDER_T, PROP_CHECKED, PROP_CLICKABLE,
    PROP_CLIP_CHILDREN, PROP_CURSOR_POS, PROP_ENABLED, PROP_FIRST_CHILD, PROP_FLEX,
    PROP_FLEX_GAP, PROP_FLEX_GROW, PROP_FONT_ID, PROP_GRADIENT_COLOR,
    PROP_GRADIENT_DIR, PROP_GRAPH_ARR, PROP_GRAPH_COUNT, PROP_GRAPH_FLAGS, PROP_GRID_COLS, PROP_GROUP_ID,
    PROP_IMAGE_ID, PROP_KIND, PROP_LOC_X, PROP_LOC_Y, PROP_LOCATION, PROP_MAX_LENGTH,
    PROP_MULTI_LINE, PROP_RICH_TEXT, PROP_NEXT_SIBLING, PROP_ON_CHANGE, PROP_ON_CLICK, PROP_PARENT,
    PROP_ON_PAINT, PROP_ON_TAP, PROP_PADDING, PROP_PADDING_B, PROP_PADDING_L, PROP_PADDING_R,
    PROP_PADDING_T, PROP_PRESS_COLOR, PROP_SCROLL_Y, PROP_SIZE, PROP_SIZE_H, PROP_SIZE_W,
    PROP_TEXT, PROP_TEXT_ALIGN, PROP_TEXT_COLOR, PROP_VALUE, PROP_VISIBLE,
};
use crate::render;

#[path = "vm_flex.rs"]
mod flex;
#[path = "vm_storage.rs"]
mod disk;
#[path = "vm_gesture.rs"]
mod gesture_ops;
pub use flex::FLEX_AUTO;
pub(crate) use flex::flush as flex_flush;
use crate::strpool;
use crate::pool::{ObjPool, ObjPtr, PTR_NONE};
use crate::types::{Edges, Offset, Size};
use crate::widget::{
    FLAG_CHECKED, FLAG_CLICKABLE, FLAG_CLIP_CHILDREN, FLAG_ENABLED, FLAG_HAS_ON_CLICK,
    FLAG_HAS_ON_PAINT, FLAG_HAS_ON_TAP, FLAG_MULTI_LINE, FLAG_RICH_TEXT, FLAG_VISIBLE, KIND_BUTTON_GROUP, KIND_RADIO,
    WValue, WidgetId,
};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::syscfg::try_alloc_slice;

/// Run `$body` with `$c` = the canvas VM drawing goes to: the `on_paint`
/// recorder while one is recording (`Vm::run_paint`), the direct-drawing
/// recorder when the render side draws (`render::records`, step 3d), else
/// the screen.
macro_rules! canvas {
    ($ctx:expr, |$c:ident| $body:block) => {
        if $ctx.objs.render.painting.get() {
            let $c = &$ctx.objs.render.paint_rec;
            $body
        } else if render::records($ctx) {
            let $c = $ctx.objs.screen_rec(&$ctx.lcd);
            $body
        } else {
            let $c = &$ctx.lcd;
            $body
        }
    };
}

/// Dynamic opcode profile (vm/prof.rs).
pub mod prof;
/// Threaded dispatch of `tick`'s bursts (vm/tc.rs).
mod ops;
#[cfg(test)]
mod oracle;
mod tc;
/// Code units: a program's libraries (vm/units.rs).
mod units;
pub use units::{LibExport, LibImage, LibImport, LibTables, MAX_LIBS};

// === Code source — owned by VM ===

/// Bytes after the code: the threaded dispatch (`tc`) reads an instruction's
/// operands and the next opcode without a bounds check, so an instruction
/// cut off by the end of the code (at most 9 bytes) stays in the buffer.
/// They are `TC_PAD`, an undefined opcode, so running into them faults
/// (`tc::bad_op`: CODE_OOB).
pub(crate) const CODE_PAD: usize = 16;
const TC_PAD: u8 = 0xFF;

/// The program's bytecode. Always in RAM: XL has no execution from flash
/// (programs are megabytes at most; the heap holds them). `buf` holds the
/// `len` code bytes, then `CODE_PAD` bytes of `TC_PAD`. Shared: a library's
/// code is one buffer for every task that imports it (`vm/units.rs`); `ptr`
/// caches its start, so a jump reads one field as before.
///
/// The bytes change in one way only: the threaded dispatch rewrites a
/// superinstruction's opcode to its quickened form (`quicken`, `QUICK`),
/// which every path runs the same as the original.
#[derive(Clone)]
pub struct VmCode {
    buf: Arc<CodeBuf>,
    ptr: *mut u8,
    len: usize,
}

/// `VmCode`'s buffer: written in place by `VmCode::quicken`.
struct CodeBuf(core::cell::UnsafeCell<Vec<u8>>);

// SAFETY: `ptr` points into `buf`, whose allocation never changes after
// `new` and lives as long as any clone. Its bytes change only through
// `quicken`, from the thread that runs the VMs (the runtime loop runs every
// task, so no VM sharing the buffer reads it meanwhile), while no slice from
// `bytes` is alive (those are local to one out-of-line instruction,
// `vm/ops.rs`).
unsafe impl Send for VmCode {}
unsafe impl Sync for VmCode {}

impl Default for VmCode {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl VmCode {
    /// Takes the bytecode buffer (no copy unless it has no room for the pad).
    pub fn new(mut buf: Vec<u8>) -> Self {
        let len = buf.len();
        buf.resize(len + CODE_PAD, TC_PAD);
        let buf = Arc::new(CodeBuf(core::cell::UnsafeCell::new(buf)));
        // SAFETY: nothing else holds the buffer yet.
        let ptr = unsafe { (*buf.0.get()).as_mut_ptr() };
        Self { buf, ptr, len }
    }

    /// Replace the opcode at `at` (a superinstruction's, inside the code)
    /// with its quickened form `q` (`QUICK`): same length and operands, the
    /// same result on every path.
    ///
    /// # Safety
    /// `at` is `ptr() + i` for an `i < len()`; called as `VmCode`'s Sync note
    /// says.
    #[inline(always)]
    pub(crate) unsafe fn quicken(&self, at: *const u8, q: u8) {
        debug_assert!((at as usize) >= self.ptr as usize && (at as usize) < self.ptr as usize + self.len);
        // SAFETY: inside the buffer (caller), `ptr`'s provenance is the
        // UnsafeCell's (new).
        unsafe { *self.ptr.add(at as usize - self.ptr as usize) = q };
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// The code bytes (without the pad).
    #[inline(always)]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is `buf`'s start and len <= buf.len() (new).
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Start of the buffer (code, then the pad).
    #[inline(always)]
    pub(crate) fn ptr(&self) -> *const u8 {
        self.ptr as *const u8
    }

    /// The byte at `pos`, 0 past the end.
    #[inline]
    pub fn byte_at(&self, pos: usize) -> u8 {
        self.bytes().get(pos).copied().unwrap_or(0)
    }

    /// Run `f` on the `len` code bytes at `pos` (caller checks the range).
    fn with_bytes<R>(&self, pos: usize, len: usize, f: impl FnOnce(&[u8]) -> R) -> R {
        f(&self.bytes()[pos..pos + len])
    }

    /// Copy code bytes from `pos` into `buf`; the count copied.
    #[inline]
    pub fn read_bytes(&self, pos: usize, buf: &mut [u8]) -> usize {
        let Some(src) = self.bytes().get(pos..) else {
            return 0;
        };
        let n = buf.len().min(src.len());
        buf[..n].copy_from_slice(&src[..n]);
        n
    }
}

// === Function metadata (embedded in image header) ===

/// Function kind — determines how the VM dispatches the function.
#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum FunctionKind {
    Setup = 0,
    Loop = 1,
    UserFunction = 2,
    OnProgramStart = 3,
    OnPageChanging = 4,
    OnPageChanged = 5,
    OnUserMessage = 6,
    OnTouchDown = 7,
    OnTouchUp = 8,
    OnTouchMove = 9,
    /// Swipe gesture detected on release. Args: direction (0=left 1=right 2=up 3=down), widget_id, dominant_delta.
    OnSwipe = 10,
    /// Long press detected after holding still for 500 ms. Args: widget_id, x, y.
    OnLongPress = 11,
    /// Compile-time generated click dispatcher. Args: widget_id (plus optional extra args for sliders).
    OnClick = 12,
    /// Compile-time generated tap/on_change dispatcher. Args: widget_id, packed_xy (0 for on_change).
    OnTap = 13,
    /// Compile-time generated paint dispatcher. Arg: array_id of dirty widget ids (freed by generated body).
    OnPaint = 14,
    /// `multi_program`: a `sendMessage` arrived. Args: from_task, a, b.
    OnTaskMessage = 15,
    /// `multi_program`: a task this one spawned ended. Args: task, code
    /// (0 = exited or killed, > 0 = VM fault code, -1 = could not start).
    OnTaskExit = 16,
}

impl FunctionKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Setup),
            1 => Some(Self::Loop),
            2 => Some(Self::UserFunction),
            3 => Some(Self::OnProgramStart),
            4 => Some(Self::OnPageChanging),
            5 => Some(Self::OnPageChanged),
            6 => Some(Self::OnUserMessage),
            7 => Some(Self::OnTouchDown),
            8 => Some(Self::OnTouchUp),
            9 => Some(Self::OnTouchMove),
            10 => Some(Self::OnSwipe),
            11 => Some(Self::OnLongPress),
            12 => Some(Self::OnClick),
            13 => Some(Self::OnTap),
            14 => Some(Self::OnPaint),
            15 => Some(Self::OnTaskMessage),
            16 => Some(Self::OnTaskExit),
            _ => None,
        }
    }
}

/// Function entry from the image header.
#[derive(Clone, Copy)]
pub struct FuncEntry {
    pub func_id: u16,
    pub kind: FunctionKind,
    pub offset: u32,
    pub length: u32,
}

/// Image header size v1/v2: version(1) + function_count(2) + reserved(2) = 5 bytes.
/// Image header size v3: + flags(2) = 7 bytes.
/// Image header size v4: + widget_count(1) + ext_count(1) = 9 bytes.
const IMAGE_HEADER_SIZE_V2: usize = 5;
const IMAGE_HEADER_SIZE_V3: usize = 7;
const IMAGE_HEADER_SIZE_V4: usize = 9;
/// v5: + var_slots(u16): the globals + deepest locals the program needs.
const IMAGE_HEADER_SIZE_V5: usize = 11;
/// v6 (libraries): v5 + lib_count(1) + import_count(2) + export_count(2) + call_depth(1).
const IMAGE_HEADER_SIZE_V6: usize = 17;
/// Image header flags bit 4: a library (ferrite-ui ferrite_cc.py IMAGE_FLAG_LIBRARY).
pub(crate) const IMAGE_FLAG_LIBRARY: u16 = 0x10;
/// Each function entry: func_id(2) + kind(1) + pad(1) + offset(4) + length(4) = 12 bytes.
const FUNC_ENTRY_SIZE: usize = 12;
/// Image header flags bit 2: JMP/JZ/JNZ/CALL targets are u32 LE instead of u16.
/// Set by the compiler when the bytecode exceeds 64 KB. (Bit 1 = e-paper mode.)
const IMAGE_FLAG_WIDE_ADDR: u16 = 0x04;
/// Image header flags bit 3: portrait — the screen is rotated 90° (logical
/// 480x800 on the 800x480 panel, see `LcdImpl::set_portrait`).
const IMAGE_FLAG_PORTRAIT: u16 = 0x08;

/// Bytecode address: pc, return addresses, callback entry points.
pub type CodeAddr = u32;

// --- 1-byte opcode map (IL-style) ---

// No-arg instructions (1 byte total)
const OP_HALT: u8 = 0x00;
const OP_POP: u8 = 0x01;
const OP_DUP: u8 = 0x02;
const OP_SWAP: u8 = 0x03;
const OP_ADD: u8 = 0x04;
const OP_SUB: u8 = 0x05;
const OP_MUL: u8 = 0x06;
const OP_DIV: u8 = 0x07;
const OP_MOD: u8 = 0x08;
const OP_NEG: u8 = 0x09;
const OP_AND: u8 = 0x0A;
const OP_OR: u8 = 0x0B;
const OP_NOT: u8 = 0x0C;
const OP_EQ: u8 = 0x0D;
const OP_NE: u8 = 0x0E;
const OP_LT: u8 = 0x0F;
const OP_LE: u8 = 0x10;
const OP_GT: u8 = 0x11;
const OP_GE: u8 = 0x12;
const OP_RET: u8 = 0x13;
const OP_YIELD: u8 = 0x14;
const OP_W_DIRTY: u8 = 0x15;
const OP_W_RENDER: u8 = 0x16;
const OP_ARR_LOAD: u8 = 0x17;
const OP_ARR_STORE: u8 = 0x18;
const OP_ARR_LEN: u8 = 0x19;
const OP_W_ALLOC: u8 = 0x1A;
const OP_ARR_FREE: u8 = 0x1B;
const OP_W_ALLTAR: u8 = 0x1C; // alloc + target + store (combined)
const OP_FRAME: u8 = 0x1D; // + u8 local_count (function prologue)
const OP_SHL: u8 = 0x1E; // a b → a << (b & 31), logical left shift
const OP_SHR: u8 = 0x1F; // a b → a >> (b & 31), arithmetic right shift (sign-preserving)

// Specialized short forms (1 byte, no args)
const OP_PUSH_0: u8 = 0x20;
const OP_PUSH_1: u8 = 0x21;
const OP_PUSH_2: u8 = 0x22;
const OP_PUSH_M1: u8 = 0x23;
const OP_LOAD_0: u8 = 0x24;
const OP_LOAD_1: u8 = 0x25;
const OP_LOAD_2: u8 = 0x26;
const OP_LOAD_3: u8 = 0x27;
const OP_LOAD_4: u8 = 0x28;
const OP_STORE_0: u8 = 0x29;
const OP_STORE_1: u8 = 0x2A;
const OP_STORE_2: u8 = 0x2B;
const OP_STORE_3: u8 = 0x2C;
const OP_STORE_4: u8 = 0x2D;

// With fixed-size arguments
const OP_PUSH_I8: u8 = 0x30; // + i8
const OP_PUSH_I16: u8 = 0x31; // + i16 LE
const OP_PUSH_I32: u8 = 0x32; // + i32 LE
const OP_LOAD: u8 = 0x33; // + u8 slot
const OP_STORE: u8 = 0x34; // + u8 slot
const OP_JMP: u8 = 0x35; // + u32 target
const OP_JZ: u8 = 0x36; // + u32 target
const OP_JNZ: u8 = 0x37; // + u32 target
const OP_CALL: u8 = 0x38; // + u32 target
const OP_W_TARGET: u8 = 0x39; // + u8 widget_id
const OP_W_SET: u8 = 0x3A; // + u8 prop_id (value from stack)
const OP_W_GET: u8 = 0x3B; // + u8 prop_id
const OP_W_PARENT: u8 = 0x3C; // + u8 parent_id
const OP_W_SET_LEN: u8 = 0x3D; // + u8 prop_id + u8 len + data
const OP_ARR_ALLOC: u8 = 0x3E; // + u8 size
/// Pop size, push a zero-filled array (sizes over 255; `ARR_ALLOC` takes a u8).
const OP_ARR_ALLOC_S: u8 = 0x2E;
const OP_ARR_INIT: u8 = 0x3F; // + u8 count + i32 values LE
const OP_F_READ: u8 = 0x40; // + u32 addr + u16 len
const OP_F_WRITE: u8 = 0x41; // + u32 addr + u8 len + data
const OP_W_TARGET_S: u8 = 0x42; // widget id from stack (dynamic target)
const OP_W_PARENT_S: u8 = 0x43; // parent id from stack (dynamic parent)
const OP_W_LOAD: u8 = 0x44; // + u16 block offset in the widget page (RES_PAGE)

// Superinstructions (`superops` feature, 0x45..=0x4E). `cond`/`op` operand = the opcode
// of the fused binary op (OP_ADD..=OP_OR, OP_SHL, OP_SHR, OP_EQ..=OP_GE).
// Keep in sync with tools/ferrite_cc.py (Op, OP_NAMES, disassembler).
const OP_JCMP: u8 = 0x45; // + u8 cond + addr: pop a, b; jump if a <cond> b
const OP_JCMP_VI: u8 = 0x46; // + u8 cond + u8 slot + i16 imm + addr: jump if var <cond> imm
const OP_JCMP_VV: u8 = 0x47; // + u8 cond + u8 slot_a + u8 slot_b + addr
const OP_BIN_I: u8 = 0x48; // + u8 op + i16: top = top <op> imm
const OP_BIN_I32: u8 = 0x49; // + u8 op + i32: top = top <op> imm
const OP_BIN_V: u8 = 0x4A; // + u8 op + u8 slot: top = top <op> var
const OP_ADD_VI: u8 = 0x4B; // + u8 slot + i16: var += imm (no stack)
const OP_ARR_LOAD_V: u8 = 0x4C; // + u8 slot: pop index, push array(var)[index]
const OP_STORE_OP: u8 = 0x4D; // + u8 op + u8 slot: pop b; var = var <op> b
const OP_FRAME_A: u8 = 0x4E; // + u8 local_count + u8 nargs: FRAME, then pop the args into locals nargs-1..0
const OP_CALL_IMP: u8 = 0x4F; // + u16 import: call an imported library function (vm/units.rs)
const OP_FN_REF: u8 = 0x50; // + u16 func_id: push it tagged with the running unit
// Widget build: varint length + fields, each a varint key (prop << 3 | wire
// type) and its value: WT_VARINT zigzag varint, WT_LEN varint length +
// payload (as W_SET_LEN's), WT_STACK popped (the compiler pushes those values
// in reverse field order). Each field does what its W_SET / W_SET_LEN would,
// in field order, on the target.
const OP_W_SETALL: u8 = 0x51;

// Quickened forms: VM-internal opcodes 0x58..=0x7F, never emitted by the
// compiler (ferrite-ui `ferrite_cc.py` keeps the range free;
// `docs/bytecode.md`). The threaded dispatch (`vm/tc.rs`) rewrites an
// instruction it runs into its form here, which has its own handler: a
// superinstruction by its cond / op operand (no dispatch on it), and by the
// kind of its slot where it has one (BIN_V, JCMP_VI, STORE_OP), LOAD / STORE /
// ADD_VI / ARR_LOAD_V by the kind of their slot (no local-or-global choice). Same
// length and operands. `QUICK[q - Q_FIRST]` = (the instruction, the cond /
// op, the slot kind), and `tc` runs `q` as that instruction with that
// cond / op (the operand byte ignored) and the slot operand forced to that
// kind (`force_slot`), so any byte sequence runs the same quickened or not.
// Picked by the dynamic profile (`vm/prof.rs`, `scripts/vmprof.py`).
pub(crate) const Q_FIRST: u8 = 0x58;
/// Slot kinds of a quickened form: the operand as it is, forced local,
/// forced global.
pub(crate) const SLOT_ANY: u8 = 0;
pub(crate) const SLOT_L: u8 = 1;
pub(crate) const SLOT_G: u8 = 2;
pub(crate) const QUICK: [(u8, u8, u8); 39] = [
    (OP_JCMP, OP_EQ, SLOT_ANY), (OP_JCMP, OP_NE, SLOT_ANY), (OP_JCMP, OP_LT, SLOT_ANY),
    (OP_JCMP, OP_LE, SLOT_ANY), (OP_JCMP, OP_GT, SLOT_ANY), (OP_JCMP, OP_GE, SLOT_ANY),
    (OP_JCMP_VI, OP_EQ, SLOT_L), (OP_JCMP_VI, OP_NE, SLOT_L), (OP_JCMP_VI, OP_LT, SLOT_L),
    (OP_JCMP_VI, OP_LE, SLOT_L), (OP_JCMP_VI, OP_GT, SLOT_L), (OP_JCMP_VI, OP_GE, SLOT_L),
    (OP_JCMP_VV, OP_EQ, SLOT_ANY), (OP_JCMP_VV, OP_NE, SLOT_ANY), (OP_JCMP_VV, OP_LT, SLOT_ANY),
    (OP_JCMP_VV, OP_LE, SLOT_ANY), (OP_JCMP_VV, OP_GT, SLOT_ANY), (OP_JCMP_VV, OP_GE, SLOT_ANY),
    (OP_BIN_I, OP_ADD, SLOT_ANY), (OP_BIN_I, OP_SUB, SLOT_ANY), (OP_BIN_I, OP_MUL, SLOT_ANY),
    (OP_BIN_I, OP_DIV, SLOT_ANY), (OP_BIN_I, OP_MOD, SLOT_ANY), (OP_BIN_I, OP_AND, SLOT_ANY),
    (OP_BIN_I, OP_OR, SLOT_ANY), (OP_BIN_I, OP_SHL, SLOT_ANY), (OP_BIN_I, OP_SHR, SLOT_ANY),
    (OP_BIN_V, OP_ADD, SLOT_L), (OP_BIN_V, OP_ADD, SLOT_G),
    (OP_BIN_V, OP_SUB, SLOT_L), (OP_BIN_V, OP_SUB, SLOT_G),
    (OP_STORE_OP, OP_ADD, SLOT_L),
    (OP_LOAD, 0, SLOT_L), (OP_LOAD, 0, SLOT_G), (OP_STORE, 0, SLOT_L), (OP_STORE, 0, SLOT_G),
    (OP_ADD_VI, 0, SLOT_L),
    (OP_ARR_LOAD_V, 0, SLOT_L), (OP_ARR_LOAD_V, 0, SLOT_G),
];
pub(crate) const Q_LAST: u8 = Q_FIRST + QUICK.len() as u8 - 1;
const _: () = assert!(Q_LAST <= 0x7F);

/// The form of (`op`, `sub`, `slot`) in `QUICK`, at compile time.
const fn q_find(op: u8, sub: u8, slot: u8) -> u8 {
    let mut i = 0;
    while i < QUICK.len() {
        if QUICK[i].0 == op && QUICK[i].1 == sub && QUICK[i].2 == slot {
            return Q_FIRST + i as u8;
        }
        i += 1;
    }
    panic!("no such quickened form")
}

pub(crate) const Q_LOAD_L: u8 = q_find(OP_LOAD, 0, SLOT_L);
pub(crate) const Q_LOAD_G: u8 = q_find(OP_LOAD, 0, SLOT_G);
pub(crate) const Q_STORE_L: u8 = q_find(OP_STORE, 0, SLOT_L);
pub(crate) const Q_STORE_G: u8 = q_find(OP_STORE, 0, SLOT_G);
pub(crate) const Q_ADD_VI_L: u8 = q_find(OP_ADD_VI, 0, SLOT_L);
pub(crate) const Q_ARR_LOAD_V_L: u8 = q_find(OP_ARR_LOAD_V, 0, SLOT_L);
pub(crate) const Q_ARR_LOAD_V_G: u8 = q_find(OP_ARR_LOAD_V, 0, SLOT_G);

/// A slot operand as a form of slot kind `kind` takes it.
#[inline(always)]
pub(crate) fn force_slot(raw: u8, kind: u8) -> u8 {
    match kind {
        SLOT_L => raw | 0x80,
        SLOT_G => raw & 0x7F,
        _ => raw,
    }
}

/// The quickened form of superinstruction `op` with cond / op operand
/// `sub` and slot operand `slot` (any byte for those without one), 0 if it
/// has none (or only one for the other slot kind).
#[inline(always)]
pub(crate) fn quick_of(op: u8, sub: u8, slot: u8) -> u8 {
    QUICK_OF[op.wrapping_sub(OP_JCMP) as usize & 15][sub as usize & 31][(slot >> 7) as usize] * (sub < 32) as u8
}

/// `quick_of` as a table: [op - OP_JCMP][sub][slot bit 7] (a SLOT_ANY form
/// in both kinds; superinstructions without a quickened form: rows of 0).
static QUICK_OF: [[[u8; 2]; 32]; 16] = {
    let mut t = [[[0u8; 2]; 32]; 16];
    let mut i = 0;
    while i < QUICK.len() {
        let (op, sub, kind) = QUICK[i];
        if op >= OP_JCMP && op <= OP_STORE_OP && op != OP_ADD_VI && op != OP_ARR_LOAD_V {
            let q = Q_FIRST + i as u8;
            let e = &mut t[(op - OP_JCMP) as usize][sub as usize];
            if kind != SLOT_L {
                e[0] = q;
            }
            if kind != SLOT_G {
                e[1] = q;
            }
        }
        i += 1;
    }
    t
};

/// The integer binary ops (and comparisons, as 0/1) by opcode, as the plain
/// opcodes compute them. None = not a binary op.
#[inline(always)]
fn int_binop(op: u8, a: i32, b: i32) -> Option<i32> {
    Some(match op {
        OP_ADD => a.wrapping_add(b),
        OP_SUB => a.wrapping_sub(b),
        OP_MUL => a.wrapping_mul(b),
        OP_DIV => {
            if b != 0 {
                a.wrapping_div(b)
            } else {
                0
            }
        }
        OP_MOD => {
            if b != 0 {
                a.wrapping_rem(b)
            } else {
                0
            }
        }
        OP_AND => a & b,
        OP_OR => a | b,
        OP_SHL => a << (b as u32 & 31),
        OP_SHR => a >> (b as u32 & 31),
        _ => return int_cmp(op, a, b).map(|c| c as i32),
    })
}

/// The comparisons by opcode (OP_EQ..=OP_GE).
#[inline(always)]
fn int_cmp(op: u8, a: i32, b: i32) -> Option<bool> {
    Some(match op {
        OP_EQ => a == b,
        OP_NE => a != b,
        OP_LT => a < b,
        OP_LE => a <= b,
        OP_GT => a > b,
        OP_GE => a >= b,
        _ => return None,
    })
}

// Widget page (RES_PAGE) ops: static widget setup the compiler moved out of
// the bytecode. 0x01..=0x3F = W_SET <tag> + zigzag varint, 0x40..=0x7F =
// W_SET_LEN <tag> + u8 len + payload, the rest below + operands.
// Keep in sync with tools/ferrite_lang.py (PAGE_*).
// Target/parent ops: bit 0 = operand is a var slot holding the widget id
// (else the id itself), bit 1 = parent (else target).
const PG_STORE: u8 = 0x80; // + u8 slot + zigzag varint
const PG_ALLTAR: u8 = 0x81; // + u8 slot
const PG_TARGET_ID: u8 = 0x84; // + u8 widget id
const PG_TARGET: u8 = 0x85; // + u8 slot
const PG_PARENT_ID: u8 = 0x86; // + u8 widget id
const PG_PARENT: u8 = 0x87; // + u8 slot

// Builtins as first-class opcodes (args on stack)
const OP_FILL_RECT: u8 = 0x80;
const OP_RECT: u8 = 0x81;
const OP_LINE: u8 = 0x82;
const OP_CIRCLE: u8 = 0x83;
const OP_FILL_CIRCLE: u8 = 0x84;
const OP_DRAW_IMAGE: u8 = 0x85;
const OP_DRAW_TEXT_LIT: u8 = 0x86; // + u8 len + text
const OP_DELAY: u8 = 0x87;
const OP_STR_LIT: u8 = 0x88; // + u8 len + text
const OP_ITOS: u8 = 0x89;
const OP_FTOS: u8 = 0x8A;
const OP_CONCAT: u8 = 0x8B;
const OP_PARSE_INT: u8 = 0x8C;
const OP_PARSE_FLOAT: u8 = 0x8D;
const OP_STR_LEN: u8 = 0x8E;
const OP_SET_TEXT: u8 = 0x8F;
const OP_DRAW_STR: u8 = 0x90;
const OP_STR_CLEAR: u8 = 0x91;
const OP_STR_FREE: u8 = 0x92;
const OP_ROUNDED_RECT: u8 = 0x93;
const OP_FILL_ROUNDED_RECT: u8 = 0x94;
const OP_ARC: u8 = 0x95;
const OP_BEGIN_FRAME: u8 = 0x96;
const OP_END_FRAME: u8 = 0x97;
const OP_SEND_USART: u8 = 0x98;
const OP_SEND_USART_STR: u8 = 0x99;
const OP_RTC_READ: u8 = 0x9A;
const OP_RTC_WRITE: u8 = 0x9B;
const OP_MILLIS: u8 = 0x9C;
const OP_FPGA_CMD: u8 = 0x9D; // pop cmd, pop data → send_command(cmd), send_data(data)
const OP_FPGA_DAT: u8 = 0x9E; // pop data → send_data(data)
const OP_CRITICAL: u8 = 0x9F; // enter critical section — run until next yield
const OP_SET_BRIGHTNESS: u8 = 0xA0; // pop percent → set backlight
const OP_BRIGHTNESS: u8 = 0xA1; // push current backlight percent
const OP_STR_CMP: u8 = 0xAC; // pop str_id, pop str_id → push -1/0/1
const OP_FILE_OPEN: u8 = 0xA2; // pop str_id (name) → push handle (1|2|0xFF)
const OP_FILE_READ: u8 = 0xA3; // pop handle → push byte (0..255) or -1 on EOF
const OP_FILE_SIZE: u8 = 0xA4; // pop handle → push size
const OP_FILE_CLOSE: u8 = 0xA5; // pop handle, release slot
const OP_FILE_SEEK: u8 = 0xAA; // pop handle, pop pos → seek to byte position (clamped to [0, size])
const OP_SYSCALL: u8 = 0xAB; // + u8 syscall_id + u8 argc — pop argc args → call handler, push i32 result
const OP_ARR_TO_STR: u8 = 0xA6; // pop len, pop arr_id → alloc string from low bytes of arr elements (len<0 = full array)
const OP_SHOW_MODAL: u8 = 0xA7; // pop click_fn, pop builder_fn → suspend until setDialogResult, push result on resume
const OP_SET_DIALOG_RESULT: u8 = 0xA8; // pop result → record on top modal frame; void return
const OP_SPRINTF: u8 = 0xA9; // + u8 argc — pop argc args + fmt str_id → push result str_id
const OP_ANIMATE: u8 = 0xAD; // pop on_end, ctrl, duration, to, prop, widget → start tween
const OP_STOP_ANIM: u8 = 0xAE; // pop widget → cancel all tweens on the widget
const OP_STOP_ANIM_PROP: u8 = 0xAF; // pop prop, pop widget → cancel that one tween
const OP_PIN_MODE: u8 = 0xB0; // pop mode, pop pin → configure user GPIO pin
const OP_PIN_WRITE: u8 = 0xB1; // pop value, pop pin → drive output pin
const OP_PIN_READ: u8 = 0xB2; // pop pin → push 0/1
const OP_EEPROM_SIZE: u8 = 0xB3; // push EEPROM size in bytes (0 = none)
const OP_EEPROM_READ: u8 = 0xB4; // pop addr → push byte 0..255
const OP_EEPROM_WRITE: u8 = 0xB5; // pop value, pop addr → store low byte
const OP_EEPROM_READ_ARR: u8 = 0xB6; // pop count, pop arr, pop addr → arr[0..count] = bytes
const OP_EEPROM_WRITE_ARR: u8 = 0xB7; // pop count, pop arr, pop addr → store low bytes of arr[0..count]
const OP_STR_BYTE: u8 = 0xB8; // pop index, pop str_id → push byte 0..255, or -1 past the end
// Programs (RES_PROGRAM resources): switching and listing.
const OP_RUN_PROGRAM: u8 = 0xBD; // pop arg, pop str_id (program name) → push 0 if missing; else exit, switch
const OP_PROGRAM_ARG: u8 = 0xBE; // push the arg this program was started with (0 at boot)
const OP_PROGRAM_NAME: u8 = 0xBF; // pop index → push str_id: nth program's name, "" past the end, -1 = own
// Tasks (`multi_program`; elsewhere bad opcodes). Requests go to `Ctx::tasks`
// and the runtime acts on them after the burst.
const OP_TASK_SPAWN: u8 = 0xD4; // pop arg, pop str_id → push task id, -1 (no such program / no free slot)
const OP_TASK_KILL: u8 = 0xD5; // pop id → push 1 if it will end, 0 (main / not running)
const OP_TASK_ID: u8 = 0xD6; // push the running task's id (0 = main)
const OP_TASK_ALIVE: u8 = 0xD7; // pop id → push 1 while starting / running, else 0
const OP_TASK_SEND: u8 = 0xD8; // pop b, pop a, pop id → push 1 queued / 0
const OP_TASK_SET_FG: u8 = 0xD9; // pop id → push 1 if it now gets the global input events
const OP_TASK_FG: u8 = 0xDA; // push the foreground task's id
// Native lib/flex.fl flexLayout (CAP_NATIVE_FLEX, `vm_flex.rs`).
const OP_FLEX_LAYOUT: u8 = 0xDB; // pop box → lay out its children; target = box; push 0
// Sound opcodes exist only with the `audio` feature; elsewhere they are
// bad opcodes.
const OP_SOUND_PLAY: u8 = 0xB9; // pop looping, pop str_id (RES_SOUND name) → push 1 started / 0
const OP_SOUND_STOP: u8 = 0xBA; // stop the playing sound
const OP_SOUND_VOLUME: u8 = 0xBB; // pop volume 0..100 → output volume
const OP_SOUND_PLAYING: u8 = 0xBC; // push 1 while a sound plays, else 0

// Float ops (all no-arg)
const OP_ITOF: u8 = 0xC0;
const OP_FTOI: u8 = 0xC1;
const OP_FADD: u8 = 0xC2;
const OP_FSUB: u8 = 0xC3;
const OP_FMUL: u8 = 0xC4;
const OP_FDIV: u8 = 0xC5;
const OP_FNEG: u8 = 0xC6;
const OP_FEQ: u8 = 0xC7;
const OP_FLT: u8 = 0xC8;
const OP_FLE: u8 = 0xC9;
const OP_FGT: u8 = 0xCA;
const OP_FGE: u8 = 0xCB;
const OP_FNE: u8 = 0xCC;

// Float math (trig / sqrt / rounding) — all no-arg, operate on f32 stack values
const OP_FSIN: u8 = 0xCD;
const OP_FCOS: u8 = 0xCE;
const OP_FSQRT: u8 = 0xCF;
const OP_FABS: u8 = 0xD0;
const OP_FATAN2: u8 = 0xD1; // pop x, pop y → push atan2(y,x)
const OP_FFLOOR: u8 = 0xD2;
const OP_FCEIL: u8 = 0xD3;

// --- VM fault codes ---

/// Why the VM entered `VmState::Error`. Read with `Vm::fault()`; the matching
/// bytecode address is `Vm::fault_pc()` (start of the faulting instruction).
pub mod fault {
    pub const NONE: u8 = 0;
    /// Bytecode read past the end of the program (truncated operand / bad jump).
    pub const CODE_OOB: u8 = 1;
    /// Unknown opcode.
    pub const BAD_OPCODE: u8 = 2;
    /// Eval stack overflow (more values than the configured eval stack holds).
    pub const STACK_OVERFLOW: u8 = 3;
    /// Eval stack underflow (pop from empty stack).
    pub const STACK_UNDERFLOW: u8 = 4;
    /// Call stack overflow (call depth past the configured call stack, e.g.
    /// runaway recursion).
    pub const CALL_OVERFLOW: u8 = 5;
    /// RET with an empty call stack outside a callback.
    pub const RET_EMPTY: u8 = 6;
    /// Object pool full: a widget could not be allocated (W_ALLOC / W_ALLTAR).
    pub const WIDGET_ALLOC: u8 = 7;
    /// Object pool full: a string could not be allocated.
    pub const STR_ALLOC: u8 = 8;
    /// Unknown array id (never allocated or already freed).
    pub const BAD_ARRAY: u8 = 9;
    /// Array index out of range.
    pub const ARRAY_INDEX: u8 = 10;
    /// Invalid or unopened file handle.
    pub const BAD_FILE: u8 = 11;
    /// Syscall failed or no syscall handler installed.
    pub const SYSCALL: u8 = 12;
    /// Invalid operand value (array too short / too long for the operation).
    pub const BAD_OPERAND: u8 = 13;
    /// GPIO pin the board doesn't have, or a mode it doesn't support.
    pub const GPIO: u8 = 14;
    /// Widget page (RES_PAGE) missing, truncated or malformed (OP_W_LOAD).
    pub const PAGE: u8 = 15;
    /// EEPROM address range outside the chip, no EEPROM on the board, or an
    /// I2C bus error.
    pub const EEPROM: u8 = 16;
    /// Object pool full: an array could not be allocated.
    pub const ARR_ALLOC: u8 = 17;
    /// A function's locals don't fit the program's variable slots (FRAME past
    /// the image header's `var_slots`: calls nested deeper than the compiler
    /// planned for).
    pub const VARS_FULL: u8 = 18;
    /// A library the program calls is not installed (in that major version).
    pub const LIB_MISSING: u8 = 19;
    /// An installed library lacks a function the program calls, or its
    /// arguments / return kind differ.
    pub const LIB_MISMATCH: u8 = 20;

    /// Short human-readable name for the error screen.
    pub fn name(code: u8) -> &'static [u8] {
        match code {
            CODE_OOB => b"code out of bounds",
            BAD_OPCODE => b"bad opcode",
            STACK_OVERFLOW => b"stack overflow",
            STACK_UNDERFLOW => b"stack underflow",
            CALL_OVERFLOW => b"call stack overflow",
            RET_EMPTY => b"ret without call",
            WIDGET_ALLOC => b"pool full: widget",
            STR_ALLOC => b"pool full: string",
            BAD_ARRAY => b"invalid array",
            ARRAY_INDEX => b"array index range",
            BAD_FILE => b"invalid file handle",
            SYSCALL => b"syscall failed",
            BAD_OPERAND => b"bad operand",
            GPIO => b"invalid gpio pin/mode",
            PAGE => b"bad widget page",
            EEPROM => b"eeprom error",
            ARR_ALLOC => b"pool full: array",
            VARS_FULL => b"variable slots full",
            LIB_MISSING => b"library missing",
            LIB_MISMATCH => b"library mismatch",
            _ => b"unknown fault",
        }
    }
}

// --- VM State ---

#[derive(Clone, Copy, PartialEq)]
pub enum VmState {
    Ready,
    Running,
    Halted,
    Yielded,
    Waiting,
    /// Suspended after `showModal`; the user program is paused until
    /// `setDialogResult` fires (typically from a callback) and the main loop
    /// calls `try_resume_modal`.
    AwaitingModal,
    Error,
    /// `runProgram` succeeded: nothing of this program runs any more (main
    /// code, callbacks, modals); the runtime loads [`Vm::next_program`].
    Exit,
}

/// A program by resource name, and the argument it is started with.
#[derive(Clone, Copy)]
pub struct ProgramRef {
    name: [u8; crate::fs::NAME_LEN],
    len: u8,
    pub arg: i32,
}

impl ProgramRef {
    /// No program (a free task slot).
    pub const EMPTY: Self = Self { name: [0; crate::fs::NAME_LEN], len: 0, arg: 0 };

    /// `name` is cut to the 16 bytes a resource name can hold.
    pub fn new(name: &[u8], arg: i32) -> Self {
        let len = name.len().min(crate::fs::NAME_LEN);
        let mut buf = [0u8; crate::fs::NAME_LEN];
        buf[..len].copy_from_slice(&name[..len]);
        Self { name: buf, len: len as u8, arg }
    }

    pub fn name(&self) -> &[u8] {
        &self.name[..self.len as usize]
    }
}

// --- VM ---

/// Default evaluation stack depth (overridable via RES_SYSCFG `eval_stack`).
const STACK_SIZE: usize = 32;
const MAX_SYSCALL_ARGS: usize = 16;

/// Variable slots of an image without a `var_slots` header field (v1..v4):
/// what the old fixed table held (a u8 `frame_base` + a local index).
const LEGACY_VAR_SLOTS: u16 = 384;
/// Slots `Vm::vars` holds past `var_slots`: an operand (a global, or a local
/// index on top of `frame_base`, both 0..=127) and FRAME_A's argument count
/// (0..=255) never reach past them, as `frame_base <= var_slots` (FRAME
/// checks it). So `var_get`/`var_set` need no bounds check.
const VAR_PAD: usize = 256;
/// Largest `var_slots` an image may ask for (`frame_base + frame_size` stays
/// in a u16).
const VAR_SLOTS_MAX: u16 = 0xFF00;
/// Default call stack depth (overridable via RES_SYSCFG `call_stack`).
const CALL_STACK_SIZE: usize = 16;

/// Call stack frame: return address + saved frame state.
#[derive(Clone, Copy)]
struct CallFrame {
    ret_addr: CodeAddr,
    frame_base: u16,
    frame_size: u8,
}

/// What a callback interrupted — the VM's "exception frame". Only registers
/// are saved: the interrupted code's eval-stack values and call frames stay
/// in place, and the callback runs above them (`sp_floor`, and `call_sp` as
/// the depth at which its OP_RET returns).
#[derive(Clone, Copy)]
struct CbFrame {
    pc: CodeAddr,
    sp: u8,
    sp_floor: u8,
    call_sp: u8,
    target: WidgetId,
    state: VmState,
    wait_until: u32,
    critical: bool,
    frame_base: u16,
    frame_size: u8,
    /// The interrupted callback's gesture event details (`gestureInfo`).
    gesture: crate::gesture::Detail,
}

/// Maximum callback nesting: a callback suspended on each open modal, the
/// dialog-button callback on top of them, and an on_paint pass.
const CB_NEST_MAX: usize = MODAL_STACK_MAX + 2;

/// Maximum number of arguments per queued callback.
const CB_MAX_ARGS: usize = 3;
/// Default callback queue length (overridable via RES_SYSCFG `cb_queue`).
const CB_QUEUE_SIZE: usize = 8;

/// A queued callback request.
#[derive(Clone, Copy)]
struct CbRequest {
    offset: CodeAddr,
    arg_count: u8,
    args: [i32; CB_MAX_ARGS],
    /// A gesture event: its details are `cb_details[slot]`.
    gesture: bool,
}

/// Open file slot — absolute flash offset, total size, current read position.
/// Max 2 files open concurrently (handles 1 and 2; 0xFF means error).
#[derive(Clone, Copy)]
struct OpenFile {
    offset: u32,
    size: u32,
    pos: u32,
}

/// Returned by fileOpen on error. fileRead/Size/Close with this handle → VM Error.
const FILE_HANDLE_ERR: i32 = 0xFF;
/// EOF marker returned by fileRead when past end-of-file.
const FILE_EOF: i32 = -1;

// --- Modal dialog state ---

/// Standard dialog result codes (mirrored in lib/modal.fl).
/// Programs can pass arbitrary i32 values to setDialogResult — these are the
/// conventional set used by Cancel auto-close and by lib/modal.fl handlers.
pub const DLG_OK: i32 = 1;
pub const DLG_CANCEL: i32 = 2;

/// Maximum nested modals. A second `showModal` from inside a callback while
/// a modal is open pushes another frame; this caps that depth.
const MODAL_STACK_MAX: usize = 4;

/// Phase of an open modal frame.
#[derive(Clone, Copy, PartialEq)]
enum ModalState {
    /// `showModal` issued the call to the builder; we're waiting for the
    /// builder to return its overlay widget id via OP_RET.
    AwaitingBuilderReturn,
    /// Builder returned. Overlay is wired and visible. We wait for
    /// `setDialogResult` to set `result`, after which the main loop's
    /// `try_resume_modal` destroys the overlay and resumes the user program.
    AwaitingResult,
}

/// One frame on `Vm::modal_stack`. Each `showModal` pushes a frame; each
/// completed dialog pops one.
#[derive(Clone, Copy)]
struct ModalFrame {
    /// Widget id returned by the builder, or NONE while
    /// `state == AwaitingBuilderReturn`.
    overlay_id: WidgetId,
    /// Function id (from `@func_name`) wired into overlay.on_click. The
    /// compiler injects `@modalDefaultOverlayClick` from lib/modal.fl when the
    /// caller omits the second showModal argument.
    overlay_click_fn: u16,
    /// Result value latched by `setDialogResult`. Only valid when
    /// `has_result == true`.
    result: i32,
    /// Set true the first time `setDialogResult` fires for this frame.
    /// Subsequent calls are ignored (first-write-wins).
    has_result: bool,
    state: ModalState,
    /// `call_sp` at the moment we issued the call to the builder. When the
    /// builder finally returns (its OP_RET drops `call_sp` below this value),
    /// we capture its return value as `overlay_id`.
    builder_call_depth: u8,
}

// --- Animation engine ---

/// Easing curve selector — low 4 bits of `Anim::ctrl`. Mirrored in lib/anim.fl.
const EASE_CURVE_MASK: u8 = 0x0F;
const EASE_IN: u8 = 1;
const EASE_OUT: u8 = 2;
const EASE_IN_OUT: u8 = 3;
/// Repeat-mode flags — high bits of `Anim::ctrl`. Mirrored in lib/anim.fl.
const ANIM_LOOP: u8 = 0x10;
const ANIM_YOYO: u8 = 0x20;

/// One active property tween (~24 B). `Copy` so the tick loop can snapshot a
/// slot by value, release the borrow on `Vm::anims`, then call the
/// mutably-borrowing property setter without aliasing.
#[derive(Clone, Copy)]
struct Anim {
    target: WidgetId,
    prop_id: u8,
    /// Low 4 bits = easing curve; `ANIM_LOOP` / `ANIM_YOYO` in the high bits.
    ctrl: u8,
    from: i32,
    to: i32,
    start_ms: u32,
    duration_ms: u16,
    /// Completion callback func_id (1-based, as pushed by `@fn`); 0 = none.
    /// Resolved to a bytecode offset when fired. Only fires for non-repeating
    /// tweens.
    on_end: u16,
    active: bool,
}

pub struct Vm {
    pc: CodeAddr,
    // Eval / call stacks and the callback queue are heap slices sized by
    // `configure` (RES_SYSCFG); each is at most 255 deep (u8 indices).
    stack: Box<[i32]>,
    sp: u8,
    /// Globals, then the locals of the running call chains (`frame_base`):
    /// `var_slots + VAR_PAD` slots, indexed directly.
    vars: Box<[i32]>,
    call_stack: Box<[CallFrame]>,
    call_sp: u8,
    target: WidgetId,
    pub state: VmState,
    /// Fault code (`fault::*`) of the most recent error; valid while `state == Error`.
    fault: u8,
    /// Address of the instruction that raised `fault`.
    fault_pc: CodeAddr,
    /// Start address of the instruction currently executing.
    op_pc: CodeAddr,
    pub wait_until: u32,
    // Code source — owned by VM
    code: VmCode,
    // Function table from image header
    functions: Vec<FuncEntry>,
    // Active callbacks, innermost last (empty = the main program is running).
    // A callback that suspends (delay, yield, showModal) keeps its frame
    // here and continues on a later tick; while it waits on a modal, the
    // dialog's callbacks run on top of it.
    cb_frames: Vec<CbFrame>,
    // Lowest eval-stack slot the running code may pop: a callback cannot
    // reach the values of the code it interrupted.
    sp_floor: u8,
    // Callback queue — FIFO ring buffer
    cb_queue: Box<[CbRequest]>,
    cb_head: u8,
    cb_tail: u8,
    // Gesture details of the queued callbacks, by queue slot (allocated on
    // the first gesture event), and of the running gesture callback.
    cb_details: Vec<crate::gesture::Detail>,
    gesture_cur: crate::gesture::Detail,
    // Critical section — when true, VM runs until next YIELD without returning to main loop
    critical: bool,
    // Stack-based locals: frame_base = first local slot for current function.
    // A u32 (the frames store a u16): a word load takes any field offset in
    // one instruction (ldrh only 0..255) and a slot sum needs no masking.
    frame_base: u32,
    frame_size: u8,
    // Slots before the locals: the program's globals, then its libraries'.
    global_count: u32,
    // Code units (vm/units.rs): empty without libraries; else unit 0 is the
    // program, 1.. its libraries, the running one's code and functions in
    // `code` / `functions` (its slot here parked empty). `utag` = unit << 24,
    // the tag on code addresses saved across units; `gbase` its first global.
    units: Vec<units::Unit>,
    unit: u8,
    utag: u32,
    gbase: u32,
    // CALL_IMP index -> tagged entry address.
    links: Vec<CodeAddr>,
    // The configured call-stack depth (`link` grows it for libraries).
    call_base: u8,
    /// A loaded program's library tables (image v6), until the runtime links
    /// it (`Vm::link`).
    pub lib_tables: Option<Box<LibTables>>,
    // Slots the program's globals and locals may use (image header v5, or
    // LEGACY_VAR_SLOTS): FRAME faults past it.
    var_slots: u32,
    // Portrait screen from the image header (IMAGE_FLAG_PORTRAIT).
    pub portrait: bool,
    // `(widgets, extensions)` the program allocates, from a v4 image header
    // (exact — 0 means none). `None` = older image without counts.
    pub tree_hint: Option<(u8, u8)>,
    // Open file slots — index 0 = handle 1, index 1 = handle 2.
    open_files: [Option<OpenFile>; 2],
    /// The running program and its start argument; set by the runtime when it
    /// loads one (kept across `reset`, which the runtime does before).
    pub program: ProgramRef,
    /// Set with `VmState::Exit` by `runProgram`; taken by the runtime.
    pub next_program: Option<ProgramRef>,
    /// The image its programs come from (`domain.rs`: 0 = the system image, else an app's):
    /// its `Fs`, `F_READ` / `F_WRITE` range, EEPROM and storage root. Set by the runtime,
    /// kept across `reset`; a `sibling` starts with this one's.
    pub domain: u8,
    // Open modal dialogs (innermost = last). Empty when no dialog is showing.
    modal_stack: Vec<ModalFrame>,
    // Active property tweens, capped at `anim_slots` (0 = P::ANIM_SLOTS) on
    // insert. Ticked once per main-loop iteration by tick_animations,
    // independent of VM state.
    anims: Vec<Anim>,
    anim_slots: u8,
    // Device-specific syscall handler. Called by OP_SYSCALL; returns None → VM Error.
    pub syscall_fn: Option<fn(id: u8, args: &[i32], strpool: &mut ObjPool) -> Option<i32>>,
    // True while `run_paint` executes on_paint: render() is then a no-op (the
    // frame being painted is presented by run_paint's caller).
    in_paint: bool,
    // The program's widget page (RES_PAGE) in flash; page_len 0 = none.
    page_base: u32,
    page_len: u32,
    /// Instructions executed (`perf` feature; read and reset by the
    /// runtime's perf report).
    pub instructions: u64,
    // Threaded dispatch (vm/tc.rs), valid during a burst: the budget the
    // handlers count down from and the instructions run before it, the count
    // when it left, the eval stack's bounds as addresses (sp_floor and end),
    // the burst's `Ctx<P>` (the handlers are generic over `P`), the callback
    // depth it runs to (`tc::run`'s floor) and whether it runs one
    // instruction (`step`).
    tc_limit: u32,
    tc_base: u32,
    tc_n: u32,
    tc_lo: usize,
    tc_hi: usize,
    tc_ctx: usize,
    tc_floor: usize,
    tc_one: bool,
}

const EMPTY_CB: CbRequest = CbRequest {
    offset: 0,
    arg_count: 0,
    args: [0; CB_MAX_ARGS],
    gesture: false,
};
const EMPTY_FRAME: CallFrame = CallFrame {
    ret_addr: 0,
    frame_base: 0,
    frame_size: 0,
};

/// Top two eval-stack slots as laid out in memory (`a` pushed first, so lower).
/// Two `i32`s come back from a call in r0:r1, not through a stack slot.
#[repr(C)]
#[derive(Clone, Copy)]
struct StackPair {
    a: i32,
    b: i32,
}

impl Vm {
    pub fn new() -> Self {
        Self {
            pc: 0,
            stack: alloc::vec![0; STACK_SIZE].into_boxed_slice(),
            sp: 0,
            vars: alloc::vec![0; LEGACY_VAR_SLOTS as usize + VAR_PAD].into_boxed_slice(),
            call_stack: alloc::vec![EMPTY_FRAME; CALL_STACK_SIZE].into_boxed_slice(),
            call_sp: 0,
            target: WidgetId::NONE,
            state: VmState::Ready,
            fault: fault::NONE,
            fault_pc: 0,
            op_pc: 0,
            wait_until: 0,
            code: VmCode::default(),
            functions: Vec::new(),
            cb_frames: Vec::new(),
            sp_floor: 0,
            cb_queue: alloc::vec![EMPTY_CB; CB_QUEUE_SIZE].into_boxed_slice(),
            cb_head: 0,
            cb_tail: 0,
            cb_details: Vec::new(),
            gesture_cur: [0; crate::gesture::field::COUNT],
            critical: false,
            frame_base: 0,
            frame_size: 0,
            global_count: 0,
            units: Vec::new(),
            unit: 0,
            utag: 0,
            gbase: 0,
            links: Vec::new(),
            call_base: CALL_STACK_SIZE as u8,
            lib_tables: None,
            var_slots: LEGACY_VAR_SLOTS as u32,
            portrait: false,
            in_paint: false,
            page_base: 0,
            page_len: 0,
            instructions: 0,
            tc_limit: 0,
            tc_base: 0,
            tc_n: 0,
            tc_lo: 0,
            tc_hi: 0,
            tc_ctx: 0,
            tc_floor: 0,
            tc_one: false,
            tree_hint: None,
            open_files: [None, None],
            program: ProgramRef::new(b"main", 0),
            next_program: None,
            domain: crate::domain::SYSTEM,
            modal_stack: Vec::new(),
            anims: Vec::new(),
            anim_slots: 0,
            syscall_fn: None,
        }
    }

    /// Resize the eval stack, call stack and callback queue and set the
    /// animation slot cap (0 in any argument = built-in default). Depths are
    /// clamped to what the u8 indices address (eval >= 4, queue >= 2). Meant
    /// for boot, before any code runs — resets the VM. Returns false if the
    /// heap cannot hold a requested size (that pool keeps its old size).
    pub fn configure(&mut self, eval: usize, call: usize, cb_queue: usize, anim_slots: usize) -> bool {
        let depth = |v: usize, def: usize, min: usize| if v == 0 { def } else { v.clamp(min, 255) };
        self.reset();
        let mut ok = true;
        match try_alloc_slice(depth(eval, STACK_SIZE, 4), 0) {
            Some(b) => self.stack = b,
            None => ok = false,
        }
        self.call_base = depth(call, CALL_STACK_SIZE, 1) as u8;
        match try_alloc_slice(self.call_base as usize, EMPTY_FRAME) {
            Some(b) => self.call_stack = b,
            None => ok = false,
        }
        match try_alloc_slice(depth(cb_queue, CB_QUEUE_SIZE, 2), EMPTY_CB) {
            Some(b) => self.cb_queue = b,
            None => ok = false,
        }
        self.anim_slots = anim_slots.min(255) as u8;
        self.anims = Vec::new();
        if self.anim_slots > 0 && self.anims.try_reserve_exact(self.anim_slots as usize).is_err() {
            ok = false;
        }
        ok
    }

    /// Current `(eval_stack, call_stack, cb_queue)` depths.
    pub fn limits(&self) -> (usize, usize, usize) {
        (self.stack.len(), self.call_stack.len(), self.cb_queue.len())
    }

    /// Diagnostics: `(used, capacity, heap bytes)` of the eval stack, call
    /// stack, callback queue and animation pool, in that order. Stack depths
    /// are current values (≈0 between main-loop iterations).
    pub fn mem_stats<P: Platform>(&self) -> [(usize, usize, usize); 4] {
        use core::mem::size_of;
        let qlen = self.cb_queue.len();
        let queued = if qlen == 0 {
            0
        } else {
            (self.cb_tail as usize + qlen - self.cb_head as usize) % qlen
        };
        [
            (self.sp as usize, self.stack.len(), self.stack.len() * size_of::<i32>()),
            (self.call_sp as usize, self.call_stack.len(), self.call_stack.len() * size_of::<CallFrame>()),
            (queued, qlen, qlen * size_of::<CbRequest>()),
            (self.anims.len(), self.anim_cap::<P>(), self.anims.capacity() * size_of::<Anim>()),
        ]
    }

    // === Variable access (sparse map) ===

    /// Resolve slot byte: bit 7 set = local (frame_base + index), else global.
    #[inline]
    fn resolve_slot(&self, slot: u8) -> u32 {
        if slot & 0x80 != 0 {
            self.frame_base + (slot & 0x7F) as u32
        } else {
            self.gbase + slot as u32
        }
    }



    // A slot is a global (< 128), `frame_base` + a local index (< 128) or
    // + a FRAME_A argument (< 255), and `frame_base <= var_slots` (load,
    // FRAME): always below `var_slots + VAR_PAD` = `vars.len()`.
    #[inline(always)]
    fn var_get(&self, slot: u32) -> i32 {
        debug_assert!((slot as usize) < self.vars.len());
        // SAFETY: in range, see above.
        unsafe { *self.vars.get_unchecked(slot as usize) }
    }

    #[inline(always)]
    fn var_set(&mut self, slot: u32, val: i32) {
        debug_assert!((slot as usize) < self.vars.len());
        // SAFETY: as in var_get.
        unsafe { *self.vars.get_unchecked_mut(slot as usize) = val };
    }

    /// Size the variable table for `slots` (cleared). False if it can't be
    /// allocated.
    fn set_var_slots(&mut self, slots: u16) -> bool {
        let len = slots as usize + VAR_PAD;
        if self.vars.len() == len {
            self.vars.fill(0);
        } else {
            match try_alloc_slice(len, 0) {
                Some(b) => self.vars = b,
                None => return false,
            }
        }
        self.var_slots = slots as u32;
        true
    }

    // === Code loading ===

    /// Load a VM image from RAM bytes. Parses the image header and keeps
    /// opcodes in a Vec<u8>. Returns false if the header is invalid.
    pub fn load_ram(&mut self, image: &[u8]) -> bool {
        self.load_ram_owned(image.to_vec()).is_ok()
    }

    /// Like `load_ram`, but takes the image buffer and strips the header in
    /// place, so loading needs no second copy of the program. On failure the
    /// buffer comes back unchanged.
    pub fn load_ram_owned(&mut self, mut image: Vec<u8>) -> Result<(), Vec<u8>> {
        if let Some(h) = parse_image_header(&image) {
            let ImageHeader { functions: funcs, opcode_start, global_count: gc, tree_hint: hint, wide, portrait, var_slots: slots, libs } = h;
            // A library runs only as a program's import.
            if !code_addressable(wide, image.len() - opcode_start) || libs.as_ref().is_some_and(|l| l.library) {
                return Err(image);
            }
            let slots = slots.unwrap_or(LEGACY_VAR_SLOTS).max(gc as u16);
            if slots > VAR_SLOTS_MAX || !self.set_var_slots(slots) {
                return Err(image);
            }
            image.drain(..opcode_start);
            let opcodes = image;
            // Back in the program's unit before its code is replaced.
            self.clear_units();
            self.code = VmCode::new(opcodes);
            self.functions = funcs;
            self.global_count = gc as u32;
            self.lib_tables = libs.filter(|l| !l.libs.is_empty());
            // No frame of other code survives (it could sit past the new
            // `var_slots`).
            self.call_sp = 0;
            self.cb_frames.clear();
            self.frame_base = gc as u32;
            self.frame_size = 0;
            self.portrait = portrait;
            self.tree_hint = hint;
            Ok(())
        } else {
            Err(image)
        }
    }

    /// Load raw bytecode (no image header). Used for page bytecode.
    pub fn load_raw(&mut self, data: Vec<u8>) {
        // No header: the old fixed table (not a smaller one left by an image).
        if self.var_slots < LEGACY_VAR_SLOTS as u32 && self.set_var_slots(LEGACY_VAR_SLOTS) {
            self.call_sp = 0;
            self.cb_frames.clear();
            self.frame_size = 0;
        }
        self.clear_units();
        self.code = VmCode::new(data);
        self.tree_hint = None;
        self.functions.clear();
    }

    // === Function table lookup ===

    /// The program's function of `kind` (its first; never a library's: their
    /// dispatchers come through `dispatchers`).
    pub fn find_by_kind(&self, kind: FunctionKind) -> Option<&FuncEntry> {
        self.funcs_of(0)?.iter().find(|f| f.kind as u8 == kind as u8)
    }

    /// The entry address of function `func_id` as `@fn` / `FN_REF` gave it
    /// (a library's tagged with its unit, `unit << 12 | id`), tagged with its
    /// unit for `enqueue_callback` / `call_now`.
    pub fn func_addr(&self, func_id: u16) -> Option<CodeAddr> {
        let unit = (func_id >> 12) as u8;
        let id = func_id & 0x0FFF;
        let f = self.funcs_of(unit)?.iter().find(|f| f.func_id == id)?;
        Some(f.offset as CodeAddr | (unit as CodeAddr) << 24)
    }

    // === Callback queue ===

    /// Enqueue a callback to be executed later via drain_callbacks().
    /// Dropped silently if the queue is full.
    pub fn enqueue_callback(&mut self, offset: CodeAddr, args: &[i32]) {
        let next_tail = (self.cb_tail + 1) % self.cb_queue.len() as u8;
        if next_tail == self.cb_head {
            return; // queue full — drop
        }
        let mut req = CbRequest {
            offset,
            arg_count: args.len().min(CB_MAX_ARGS) as u8,
            args: [0; CB_MAX_ARGS],
            gesture: false,
        };
        for i in 0..req.arg_count as usize {
            req.args[i] = args[i];
        }
        self.cb_queue[self.cb_tail as usize] = req;
        self.cb_tail = next_tail;
    }

    /// Returns true if there are pending callbacks in the queue.
    pub fn has_pending_callbacks(&self) -> bool {
        self.cb_head != self.cb_tail
    }

    /// Some property animation is running (`tick_animations` changes a value
    /// every pass).
    pub fn animating(&self) -> bool {
        self.anims.iter().any(|a| a.active)
    }

    /// Milliseconds from `now` until the running code's `delay` ends (0 if it
    /// has), None if it is not waiting on one.
    pub fn wait_left(&self, now: u32) -> Option<u32> {
        if self.state != VmState::Waiting {
            return None;
        }
        let left = self.wait_until.wrapping_sub(now);
        Some(if left < 0x8000_0000 { left } else { 0 })
    }

    /// True if a queued callback may start now. The main program can be
    /// interrupted anywhere outside a critical section. A suspended callback
    /// (delay, yield) holds the queue until it finishes, so handlers stay in
    /// FIFO order — except while it waits on a modal, whose buttons must be
    /// able to run their callbacks on top of it.
    fn can_take_callback(&self) -> bool {
        if self.critical || self.in_paint || self.cb_frames.len() >= CB_NEST_MAX {
            return false;
        }
        match self.state {
            VmState::Error | VmState::Ready | VmState::Exit => false,
            VmState::AwaitingModal => true,
            _ => self.cb_frames.is_empty(),
        }
    }

    /// Start queued callbacks. Each runs until it returns or suspends; one
    /// that suspends continues from `tick` on a later iteration. Returns
    /// true if any callback ran.
    #[inline(never)]
    pub fn drain_callbacks<P: Platform>(&mut self, ctx: &mut Ctx<P>) -> bool {
        let mut ran = false;
        while self.cb_head != self.cb_tail && self.can_take_callback() {
            let slot = self.cb_head as usize;
            let req = self.cb_queue[slot];
            self.cb_head = (self.cb_head + 1) % self.cb_queue.len() as u8;
            if self.enter_callback(req.offset, &req.args[..req.arg_count as usize]) {
                self.gesture_cur = match self.cb_details.get(slot) {
                    Some(d) if req.gesture => *d,
                    _ => [0; crate::gesture::field::COUNT],
                };
            }
            self.run_active_callback(ctx);
            ran = true;
        }
        ran
    }

    /// One main-loop slice of execution. A suspended callback whose delay
    /// expired (or that yielded, or whose modal closed) continues until it
    /// returns or suspends again; otherwise the main program runs `budget`
    /// instructions. A critical section always runs to its YIELD.
    #[inline(never)]
    pub fn tick<P: Platform>(&mut self, ctx: &mut Ctx<P>, budget: u32) {
        match self.state {
            VmState::Running => {}
            VmState::Yielded => self.state = VmState::Running,
            VmState::Waiting if self.wait_expired(ctx) => self.state = VmState::Running,
            _ => return,
        }
        if !self.cb_frames.is_empty() {
            self.run_active_callback(ctx);
            return;
        }
        let n = self.burst(ctx, budget.max(1), 0);
        self.instructions = self.instructions.wrapping_add(n);
    }

    /// A burst (`tc::run`): until the VM leaves `Running`, `budget` (>= 1)
    /// instructions outside a critical section ran, or the running callbacks
    /// drop below `floor` (0: never); the count run. While the opcode profile
    /// is on, one instruction at a time, each counted first (`prof`).
    fn burst<P: Platform>(&mut self, ctx: &mut Ctx<P>, budget: u32, floor: usize) -> u64 {
        if !prof::ON.load(core::sync::atomic::Ordering::Relaxed) {
            return tc::run(self, ctx, budget, floor, false) as u64;
        }
        let mut budget = budget;
        let mut n: u64 = 0;
        loop {
            prof::count(&self.code, self.pc as usize);
            tc::run(self, ctx, 1, floor, true);
            n += 1;
            if self.state != VmState::Running || self.cb_frames.len() < floor {
                break;
            }
            if self.critical {
                continue;
            }
            budget -= 1;
            if budget == 0 {
                break;
            }
        }
        n
    }

    /// Run a function to completion right now, on top of whatever is running:
    /// setup before the main loop starts, on_paint inside a render pass.
    /// Nothing else runs meanwhile, so `delay()` busy-waits and `yield` is a
    /// no-op. A function that opens a modal stays suspended on it; the main
    /// loop resumes it like any callback.
    pub fn call_now<P: Platform>(&mut self, offset: CodeAddr, args: &[i32], ctx: &mut Ctx<P>) {
        if !self.enter_callback(offset, args) {
            return;
        }
        let depth = self.cb_frames.len();
        while self.cb_frames.len() >= depth {
            match self.state {
                VmState::Running => {
                    let n = self.burst(ctx, u32::MAX, depth);
                    self.instructions = self.instructions.wrapping_add(n);
                }
                VmState::Yielded => self.state = VmState::Running,
                VmState::Waiting => {
                    if self.wait_expired(ctx) {
                        self.state = VmState::Running;
                    }
                }
                _ => break, // Error, or suspended on a modal
            }
        }
    }

    /// Point the main program at `offset` and make it runnable. If setup left
    /// a callback suspended on a dialog, the main program starts once that
    /// callback returns.
    pub fn start_main(&mut self, offset: CodeAddr) {
        match self.cb_frames.first_mut() {
            Some(f) => {
                f.pc = offset;
                f.state = VmState::Running;
            }
            None => {
                self.goto(offset);
                self.state = VmState::Running;
            }
        }
    }

    /// Run the on_paint dispatcher now (not queued) for the widgets in
    /// `paint` that are still visible. Called right after the render pass
    /// that drew them, so the custom drawing lands in the same frame — and in
    /// buffered mode the same back buffer, before the swap — as the widget's
    /// background. Queueing it instead let it run after a callback queued
    /// earlier had already hidden the widget (e.g. a tab switch), painting
    /// over whatever replaced it.
    pub fn run_paint<P: Platform>(&mut self, ctx: &mut Ctx<P>, paint: &render::PaintList) {
        if self.in_paint
            || paint.is_empty()
            || self.state == VmState::Error
            || self.cb_frames.len() >= CB_NEST_MAX
        {
            return;
        }
        let mut offsets = [0; MAX_LIBS + 1];
        let units = self.handlers(FunctionKind::OnPaint, &mut offsets);
        if units == 0 {
            return;
        }
        let mut ids = [0i32; render::PAINT_MAX];
        let mut n = 0;
        for &id in paint.ids() {
            if ctx.objs.is_tree_visible(id) {
                ids[n] = id.0 as i32;
                n += 1;
            }
        }
        if n == 0 {
            return;
        }
        // Recorded, then drawn with each widget by the render
        // (`render::draw_paint`): handed to the render side with a render
        // context (`render::run_job`), else kept here.
        let direct = !render::records(ctx);
        self.record_paint(ctx, &offsets[..units], &ids[..n], direct);
    }

    /// Run the `on_paint` dispatcher once per widget with the drawing
    /// builtins recording (`draw_op`). `direct` (no render context): keep
    /// each list as the widget's paint list (`ObjPool::render.paints`); else
    /// queue it for the render side (`view::Draw::Paint`). The render draws
    /// it with the widget (`render::draw_paint`), so `on_paint` runs before
    /// the render that shows it.
    fn record_paint<P: Platform>(&mut self, ctx: &mut Ctx<P>, offsets: &[CodeAddr], ids: &[i32], direct: bool) {
        if !direct {
            // Direct drawing before it stays before it.
            ctx.objs.close_screen_rec();
        }
        for &raw in ids {
            if self.state == VmState::Error {
                break;
            }
            let Some(arr_id) = ctx.objs.arr_from(&[raw]) else {
                return;
            };
            let state = &ctx.objs.render;
            let recording = state.paint_rec.begin(&ctx.lcd);
            state.painting.set(recording);
            self.in_paint = true;
            // Each unit's dispatcher (a library paints its own widgets).
            for &offset in offsets {
                self.call_now(offset, &[arr_id as i32], ctx);
            }
            self.in_paint = false;
            ctx.objs.arr_free(arr_id);
            let state = &ctx.objs.render;
            if !recording {
                continue;
            }
            state.painting.set(false);
            state.paint_rec.end();
            let list = state.paint_rec.list().compact();
            crate::dl::PAINTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let id = WidgetId::from_vm(raw);
            if !direct {
                ctx.objs.render.draws.push(crate::view::Draw::Paint(id, list));
                continue;
            }
            let state = &ctx.objs.render;
            let mut paints = state.paints.borrow_mut();
            paints.insert(id, list);
            // Forget widgets that are gone or no longer paint.
            if paints.len() > 2 * render::PAINT_MAX {
                paints.retain(|&w, _| ctx.objs.get(w).flags & crate::widget::FLAG_HAS_ON_PAINT != 0);
            }
        }
    }

    /// The VM's drawing builtins. They draw on the screen, or into the
    /// `on_paint` recorder while `run_paint` records a widget's paint list
    /// (display lists, `ObjPool::render.painting`). Out of line (one copy
    /// per canvas type).
    #[inline(never)]
    fn draw_op<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        // `on_paint` drawing being recorded touches the screen when its list
        // is replayed (after the render, as it ran before).
        if !ctx.objs.render.painting.get() {
            ctx.objs.screen_touched();
        }
        match op {
            OP_FILL_RECT => canvas!(ctx, |c| {
                let (wh, color) = self.pop2();
                let (w, h) = unpack_pair(wh);
                let (x, y) = unpack_pair(self.pop());
                let color = color as u16;
                c.fill_rect(x, y, w, h, color);
            }),
            OP_RECT => canvas!(ctx, |c| {
                let (wh, color) = self.pop2();
                let (w, h) = unpack_pair(wh);
                let (x, y) = unpack_pair(self.pop());
                let color = color as u16;
                c.draw_rect(x, y, w, h, color);
            }),
            OP_LINE => canvas!(ctx, |c| {
                let (xy1, color) = self.pop2();
                let (x1, y1) = unpack_pair(xy1);
                let color = color as u16;
                let (x0, y0) = unpack_pair(self.pop());
                c.draw_line(x0 as i16, y0 as i16, x1 as i16, y1 as i16, color);
            }),
            OP_CIRCLE => canvas!(ctx, |c| {
                let (radius, color) = self.pop2();
                let (radius, color) = (radius as i16, color as u16);
                let (cx, cy) = unpack_pair(self.pop());
                c.draw_circle(cx as i16, cy as i16, radius, color);
            }),
            OP_FILL_CIRCLE => canvas!(ctx, |c| {
                let (radius, color) = self.pop2();
                let (radius, color) = (radius as i16, color as u16);
                let (cx, cy) = unpack_pair(self.pop());
                c.fill_circle(cx as i16, cy as i16, radius, color);
            }),
            OP_DRAW_IMAGE => canvas!(ctx, |c| {
                let (xy, image_id) = self.pop2();
                let (x, y) = unpack_pair(xy);
                let image_id = ctx.domains.image(self.domain, image_id as u8);
                if let Some(img) = ctx.images.find(image_id) {
                    c.draw_image(img, &ctx.flash, x as i16, y as i16);
                }
            }),
            OP_DRAW_TEXT_LIT => canvas!(ctx, |c| {
                // Inline text: u8 len + text bytes, stack: [colors, font_id, loc]
                let len = self.read_u8() as usize;
                let start = self.pc as usize;
                if start + len <= self.code.len() {
                    self.pc = (start + len) as CodeAddr;
                    let (font_id, colors) = self.pop2();
                    let (fg, bg) = unpack_pair(colors);
                    let font_id = ctx.domains.font(self.domain, font_id as u8);
                    let (x, y) = unpack_pair(self.pop());
                    if let Some(font) = ctx.fonts.resolve(font_id) {
                        let bg_opt = if bg == 0 { None } else { Some(bg) };
                        self.code.with_bytes(start, len, |text| {
                            c.draw_text(font, &ctx.flash, text, x as i16, y as i16, fg, bg_opt);
                        });
                    }
                } else {
                    self.fail(fault::CODE_OOB);
                }
            }),
            OP_DRAW_STR => canvas!(ctx, |c| {
                let str_id = self.pop() as ObjPtr;
                let (font_id, colors) = self.pop2();
                let (fg, bg) = unpack_pair(colors);
                let font_id = ctx.domains.font(self.domain, font_id as u8);
                let (x, y) = unpack_pair(self.pop());
                let data = ctx.objs.str_get(str_id);
                if let Some(font) = ctx.fonts.resolve(font_id) {
                    let bg_opt = if bg == 0 { None } else { Some(bg) };
                    c.draw_text(font, &ctx.flash, data, x as i16, y as i16, fg, bg_opt);
                }
            }),
            OP_ROUNDED_RECT => canvas!(ctx, |c| {
                let (r, color) = self.pop2();
                let (r, color) = (r as u16, color as u16);
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                c.draw_rounded_rect(x, y, w, h, r, color);
            }),
            OP_FILL_ROUNDED_RECT => canvas!(ctx, |c| {
                let (r, color) = self.pop2();
                let (r, color) = (r as u16, color as u16);
                let (w, h) = unpack_pair(self.pop());
                let (x, y) = unpack_pair(self.pop());
                c.fill_rounded_rect(x, y, w, h, r, color);
            }),
            OP_ARC => canvas!(ctx, |c| {
                let (end, color) = self.pop2();
                let (end, color) = (end as i16, color as u16);
                let start = self.pop() as i16;
                let radius = self.pop() as i16;
                let (cx, cy) = unpack_pair(self.pop());
                c.draw_arc(cx as i16, cy as i16, radius, start, end, color);
            }),
            _ => {}
        }
    }

    // === Modal dialog support ===

    /// Implements OP_SHOW_MODAL. Pops `overlay_click_fn` then `builder_fn`
    /// from the eval stack, pushes a modal frame, and issues a CALL to the
    /// builder function. The user program is left dangling at the byte after
    /// OP_SHOW_MODAL — the builder's eventual OP_RET drops us into
    /// `handle_builder_return`, which transitions VM state to AwaitingModal.
    fn handle_show_modal<P: Platform>(&mut self, _ctx: &mut Ctx<P>) {
        let click_fn = self.pop() as u16;
        let builder_fn = self.pop() as u16;

        if self.modal_stack.len() >= MODAL_STACK_MAX {
            // Nested too deep — emulate immediate Cancel.
            self.push(DLG_CANCEL);
            return;
        }

        // Resolve builder function id → entry offset.
        let builder_offset = match self.func_addr(builder_fn) {
            Some(a) => a,
            None => {
                // Bad fn id; emulate Cancel rather than VM error so the user
                // program keeps running.
                self.push(DLG_CANCEL);
                return;
            }
        };

        if (self.call_sp as usize) >= self.call_stack.len() {
            self.fail(fault::CALL_OVERFLOW);
            return;
        }

        // Mirror OP_CALL: push frame, advance frame_base past caller's locals,
        // jump to builder. The builder runs as a normal function call; its
        // OP_RET will eventually pop this frame back into the post-showModal pc.
        self.call_stack[self.call_sp as usize] = CallFrame {
            ret_addr: self.pc | self.utag,
            frame_base: self.frame_base as u16,
            frame_size: self.frame_size,
        };
        self.call_sp += 1;
        self.frame_base += self.frame_size as u32;
        self.frame_size = 0;
        self.goto(builder_offset);

        self.modal_stack.push(ModalFrame {
            overlay_id: WidgetId::NONE,
            overlay_click_fn: click_fn,
            result: 0,
            has_result: false,
            state: ModalState::AwaitingBuilderReturn,
            builder_call_depth: self.call_sp,
        });
    }

    /// Called from OP_RET when the modal builder has just returned. Pops the
    /// overlay widget id from the eval stack, wires it for clicks, and
    /// suspends VM execution until `setDialogResult` fires.
    fn handle_builder_return<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let overlay_raw = self.pop();
        if self.modal_stack.last().is_none() {
            return;
        }

        let overlay = if overlay_raw >= 0 && (overlay_raw as i64) < PTR_NONE as i64 {
            WidgetId(overlay_raw as ObjPtr)
        } else {
            WidgetId::NONE
        };

        if !ctx.objs.is_live(overlay) {
            // Builder returned a bogus id — pop modal, emulate Cancel.
            self.modal_stack.pop();
            self.push(DLG_CANCEL);
            return;
        }

        // Wire overlay click → setDialogResult(CANCEL) by default, or the
        // user-supplied handler (passed as the 2nd showModal arg). The
        // compiler injects @modalDefaultOverlayClick from lib/modal.fl when
        // omitted, so this path always has a real func_id.
        // Mark the overlay as having a click handler so main.rs dispatches it.
        // The actual func_id lives in ModalFrame.overlay_click_fn, not in ext.
        ctx.objs.get_mut(overlay).flags |= FLAG_CLICKABLE | FLAG_HAS_ON_CLICK;
        // Repaint the whole screen on next frame so the overlay's full-screen
        // backdrop covers everything underneath in both buffers.
        ctx.objs.mark_dirty(WidgetId(0));

        let top = self.modal_stack.last_mut().unwrap();
        top.overlay_id = overlay;
        top.state = ModalState::AwaitingResult;

        self.state = VmState::AwaitingModal;
    }

    /// If `widget_id` is the current modal overlay awaiting a result, return
    /// the overlay's click handler func_id from the modal frame. Returns None
    /// for any non-overlay widget so callers can fall through to the global dispatcher.
    pub fn modal_overlay_click_fn(&self, widget_id: WidgetId) -> Option<u16> {
        self.modal_stack.last().and_then(|frame| {
            if frame.state == ModalState::AwaitingResult && frame.overlay_id == widget_id {
                Some(frame.overlay_click_fn)
            } else {
                None
            }
        })
    }

    /// Called by the main loop each tick. If a modal has had its result set
    /// by `setDialogResult`, destroy the overlay subtree, push the result onto
    /// the eval stack as `showModal`'s return value, and make the suspended
    /// code runnable again. A callback that opened the dialog keeps its frame
    /// throughout, so it finishes like any other suspended callback.
    #[inline(never)]
    pub fn try_resume_modal<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        if !matches!(self.state, VmState::AwaitingModal) {
            return;
        }
        let (overlay, result) = match self.modal_stack.last() {
            Some(top) if top.has_result => (top.overlay_id, top.result),
            _ => return,
        };
        self.modal_stack.pop();

        if ctx.objs.is_live(overlay) {
            ctx.objs.destroy_subtree(overlay);
            // Repaint background under the now-gone overlay in both buffers.
            ctx.objs.mark_dirty(WidgetId(0));
        }

        // The code that called showModal — a callback or the main program —
        // continues from the next `tick`.
        self.push(result);
        self.state = VmState::Running;
    }

    /// True if a modal dialog is open (regardless of phase). Useful for the
    /// main loop, e.g. to skip routine input if a dialog has visual focus.
    pub fn modal_active(&self) -> bool {
        !self.modal_stack.is_empty()
    }

    // --- Internal: callback entry/exit ---

    /// Enter the function at `offset` as a callback, on top of whatever is
    /// running — like OP_CALL, but it also saves the registers OP_CALL
    /// doesn't (target, state, wait timer). Its args go on the eval stack
    /// above `sp_floor`. Returns false (nothing entered) at the nesting cap.
    fn enter_callback(&mut self, offset: CodeAddr, args: &[i32]) -> bool {
        if self.cb_frames.len() >= CB_NEST_MAX {
            return false;
        }
        self.cb_frames.push(CbFrame {
            pc: self.pc | self.utag,
            sp: self.sp,
            sp_floor: self.sp_floor,
            call_sp: self.call_sp,
            target: self.target,
            state: self.state,
            wait_until: self.wait_until,
            critical: self.critical,
            frame_base: self.frame_base as u16,
            frame_size: self.frame_size,
            gesture: self.gesture_cur,
        });
        self.sp_floor = self.sp;
        self.goto(offset);
        self.target = WidgetId::NONE;
        self.state = VmState::Running;
        self.critical = false;
        for &a in args {
            self.push(a);
        }
        // Place the callback's frame above the interrupted function's live
        // locals so its L0.. slots don't alias (and overwrite) them. Matches
        // the OP_CALL convention: new frame_base = frame_base + frame_size.
        // Clamp to at least global_count so the very first callback (setup)
        // doesn't collide with the globals when frame_base is still 0.
        let above = self.frame_base + self.frame_size as u32;
        self.frame_base = above.max(self.global_count);
        self.frame_size = 0;
        true
    }

    /// The active callback finished (OP_RET at its entry depth, or OP_HALT):
    /// drop whatever it left on the eval stack and resume the code it
    /// interrupted, in the state it was in.
    fn leave_callback(&mut self) {
        if let Some(f) = self.cb_frames.pop() {
            self.goto(f.pc);
            self.sp = f.sp;
            self.sp_floor = f.sp_floor;
            self.call_sp = f.call_sp;
            self.target = f.target;
            self.state = f.state;
            self.wait_until = f.wait_until;
            self.critical = f.critical;
            self.frame_base = f.frame_base as u32;
            self.frame_size = f.frame_size;
            self.gesture_cur = f.gesture;
        }
    }

    /// Run the active callback until it returns or suspends (delay, yield,
    /// showModal), or the VM faults.
    fn run_active_callback<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let depth = self.cb_frames.len();
        while self.state == VmState::Running && self.cb_frames.len() >= depth {
            let n = self.burst(ctx, u32::MAX, depth);
            self.instructions = self.instructions.wrapping_add(n);
        }
    }

    fn wait_expired<P: Platform>(&self, ctx: &Ctx<P>) -> bool {
        ctx.systick.millis().wrapping_sub(self.wait_until) < 0x8000_0000
    }

    // === Public API ===

    pub fn set_target(&mut self, id: WidgetId) {
        self.target = id;
    }

    pub fn set_pc(&mut self, pc: CodeAddr) {
        self.pc = pc;
    }

    pub fn pc(&self) -> CodeAddr {
        self.pc
    }

    /// Fault code (`fault::*`) of the most recent VM error.
    pub fn fault(&self) -> u8 {
        self.fault
    }

    /// Variable slot `i` (globals first, in declaration order).
    #[cfg(test)]
    pub(crate) fn var(&self, i: usize) -> i32 {
        self.vars[i]
    }

    /// Address of the instruction that raised the most recent VM error.
    /// Unlike `pc()`, this stays correct after a failed callback restores
    /// the interrupted state.
    pub fn fault_pc(&self) -> CodeAddr {
        self.fault_pc
    }

    /// Enter `VmState::Error` with a reason. The first fault wins: follow-on
    /// failures within the same instruction (e.g. a push after a failed pop)
    /// don't overwrite the root cause.
    #[cold]
    #[inline(never)]
    fn fail(&mut self, code: u8) {
        if self.state != VmState::Error {
            self.fault = code;
            self.fault_pc = self.op_pc | self.utag;
        }
        self.state = VmState::Error;
    }

    /// Fault the VM now (not at an instruction: a program that could not
    /// be linked).
    pub fn fault_now(&mut self, code: u8) {
        self.op_pc = 0;
        self.fail(code);
    }

    pub fn is_critical(&self) -> bool {
        self.critical
    }

    pub fn pop_result(&mut self) -> i32 {
        if self.sp > self.sp_floor {
            self.sp -= 1;
            self.stack[self.sp as usize]
        } else {
            0
        }
    }

    pub fn push_arg(&mut self, val: i32) {
        if (self.sp as usize) < self.stack.len() {
            self.stack[self.sp as usize] = val;
            self.sp += 1;
        }
    }

    pub fn reset(&mut self) {
        self.pc = 0;
        self.sp = 0;
        self.call_sp = 0;
        self.target = WidgetId::NONE;
        self.state = VmState::Ready;
        self.fault = fault::NONE;
        self.vars.fill(0);
        self.wait_until = 0;
        self.clear_units();
        self.lib_tables = None;
        self.code = VmCode::default();
        self.functions.clear();
        self.cb_frames.clear();
        self.sp_floor = 0;
        self.frame_base = 0;
        self.frame_size = 0;
        self.global_count = 0;
        self.cb_head = 0;
        self.cb_tail = 0;
        self.critical = false;
        self.portrait = false;
        self.open_files = [None, None];
        self.next_program = None;
        self.modal_stack.clear();
        self.anims.clear();
        self.page_len = 0;
    }

    /// Attach the program's widget page (RES_PAGE resource at `base` in
    /// flash), read by OP_W_LOAD. Call after loading the program.
    pub fn set_page(&mut self, base: u32, len: u32) {
        self.page_base = base;
        self.page_len = len;
    }

    pub fn has_code(&self) -> bool {
        self.code.len() > 0
    }

    pub fn run<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.state = VmState::Running;
        loop {
            match self.state {
                VmState::Running => self.step(ctx),
                VmState::Waiting => {
                    // Busy-wait for the delay to expire
                    if self.wait_expired(ctx) {
                        self.state = VmState::Running;
                    }
                }
                _ => break, // Halted, Yielded, Error → exit
            }
        }
    }

    // --- Byte reading helpers (read from self.code) ---

    /// Read inline; the out-of-range fault goes through `read_u8_slow`.
    #[inline(always)]
    fn read_u8(&mut self) -> u8 {
        if let Some(&b) = self.code.bytes().get(self.pc as usize) {
            self.pc += 1;
            return b;
        }
        self.read_u8_slow()
    }


    #[inline(never)]
    fn read_u8_slow(&mut self) -> u8 {
        let pos = self.pc as usize;
        if pos < self.code.len() {
            self.pc += 1;
            self.code.byte_at(pos)
        } else {
            self.fail(fault::CODE_OOB);
            0
        }
    }

    /// The next `N` code bytes, with one bounds check; the out-of-range fault
    /// goes byte by byte through `read_u8_slow` (first fault wins, as before).
    #[inline(always)]
    fn fetch<const N: usize>(&mut self) -> [u8; N] {
        let pc = self.pc as usize;
        if let Some(s) = self.code.bytes().get(pc..pc + N) {
            let mut out = [0u8; N];
            out.copy_from_slice(s);
            self.pc += N as CodeAddr;
            return out;
        }
        self.fetch_slow()
    }

    #[inline(never)]
    fn fetch_slow<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        for b in &mut out {
            *b = self.read_u8_slow();
        }
        out
    }



    #[inline(always)]
    fn read_u16(&mut self) -> u16 {
        u16::from_le_bytes(self.fetch())
    }


    #[inline(always)]
    fn read_i32(&mut self) -> i32 {
        i32::from_le_bytes(self.fetch())
    }

    #[inline(always)]
    fn read_u32(&mut self) -> u32 {
        self.read_i32() as u32
    }

    // --- Execute a single instruction ---

    /// Run the instruction at `pc` (state `Running`).
    pub fn step<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.instructions = self.instructions.wrapping_add(1);
        if prof::ON.load(core::sync::atomic::Ordering::Relaxed) {
            prof::count(&self.code, self.pc as usize);
        }
        tc::run(self, ctx, 1, 0, true);
    }

    /// The `multi_program` task opcodes (0xD4..=0xDA).
    #[inline(never)]
    fn task_op<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        let me = ctx.objs.cur_task;
        match op {
            OP_TASK_SPAWN => {
                let (str_id, arg) = self.pop2();
                let name = ctx.objs.str_get(str_id as ObjPtr);
                let found = !name.is_empty()
                    && name.len() <= crate::fs::NAME_LEN
                    && ctx.fs_of(self.domain).is_some_and(|fs| {
                        fs.find_kind(&ctx.flash, name, crate::fs::RES_PROGRAM).is_some()
                    });
                let id = if found { ctx.tasks.spawn(ProgramRef::new(name, arg), me, self.domain) } else { None };
                self.push(id.map_or(-1, |i| i as i32));
            }
            OP_TASK_KILL => {
                let id = self.pop();
                if id == me as i32 && me != crate::task::TASK_MAIN {
                    // Ending itself: nothing after this instruction runs.
                    self.next_program = None;
                    self.state = VmState::Exit;
                    self.push(1);
                } else {
                    let ok = ctx.tasks.request_kill(id);
                    self.push(ok as i32);
                }
            }
            OP_TASK_ID => self.push(me as i32),
            OP_TASK_ALIVE => {
                let id = self.pop();
                self.push(ctx.tasks.alive(id) as i32);
            }
            OP_TASK_SEND => {
                let b = self.pop();
                let (to, a) = self.pop2();
                let ok = u8::try_from(to)
                    .is_ok_and(|to| ctx.tasks.send(crate::task::Msg { to, from: me, a, b }));
                self.push(ok as i32);
            }
            OP_TASK_SET_FG => {
                let id = self.pop();
                self.push(ctx.tasks.set_fg(id) as i32);
            }
            _ => self.push(ctx.tasks.fg as i32), // OP_TASK_FG
        }
    }

    /// A fresh VM for another task, with this one's pool sizes and platform
    /// hooks. None if the heap can't hold its stacks.
    pub fn sibling(&self) -> Option<Self> {
        let mut vm = Self::new();
        let (eval, call, cbq) = self.limits();
        if !vm.configure(eval, call, cbq, self.anim_slots as usize) {
            return None;
        }
        vm.syscall_fn = self.syscall_fn;
        vm.domain = self.domain;
        Some(vm)
    }

    // --- Stack operations ---

    #[inline(always)]
    fn push(&mut self, val: i32) {
        if (self.sp as usize) < self.stack.len() {
            self.stack[self.sp as usize] = val;
            self.sp += 1;
        } else {
            self.fail(fault::STACK_OVERFLOW);
        }
    }

    // Out of line: once unchecked it is small enough for LLVM to inline at
    // some call sites, which costs more flash than the `bl` it replaces.
    // `fast_vm` (RAM-rich boards) inlines it everywhere.
    #[inline(always)]
    fn pop(&mut self) -> i32 {
        if self.sp > self.sp_floor {
            self.sp -= 1;
            let i = self.sp as usize;
            debug_assert!(i < self.stack.len());
            // SAFETY: sp <= stack.len() invariant (see pop2_raw), so the slot
            // below the old sp is in bounds.
            unsafe { *self.stack.get_unchecked(i) }
        } else {
            self.fail(fault::STACK_UNDERFLOW);
            0
        }
    }

    /// Pops two values with a single underflow check, in push order:
    /// `push a; push b` → `(a, b)`. On underflow the VM faults, `sp` is left
    /// alone and both read as 0.
    #[inline(always)]
    fn pop2(&mut self) -> (i32, i32) {
        let v = self.pop2_raw();
        (v.a, v.b)
    }

    /// `pop2` for two f32 operands (stored as raw bits).
    #[inline(always)]
    fn pop2_f32(&mut self) -> (f32, f32) {
        let (a, b) = self.pop2();
        (f32::from_bits(a as u32), f32::from_bits(b as u32))
    }

    /// `pop2` body, out of line so each call site is just `mov r0, vm; bl`
    /// (inlined with `fast_vm`).
    #[inline(always)]
    fn pop2_raw(&mut self) -> StackPair {
        if self.sp as usize >= self.sp_floor as usize + 2 {
            self.sp -= 2;
            let i = self.sp as usize;
            debug_assert!(i + 2 <= self.stack.len());
            // SAFETY: sp <= stack.len() always holds — sp only grows through the
            // bounds-checked push/push_arg, leave_callback restores an older sp,
            // and configure resets sp before swapping the stack. So the two
            // slots below the old sp are in bounds; skipping the slice check
            // saves ~30 B here.
            unsafe { *(self.stack.as_ptr().add(i) as *const StackPair) }
        } else {
            self.fail(fault::STACK_UNDERFLOW);
            StackPair { a: 0, b: 0 }
        }
    }

    // --- Widget ops (shared by the opcodes and the widget page) ---

    /// Alloc a widget, store its id in var `raw` and make it the target.
    #[inline(never)]
    fn w_alltar<P: Platform>(&mut self, ctx: &mut Ctx<P>, raw: u8) {
        let slot = self.resolve_slot(raw);
        if let Some(id) = ctx.objs.alloc_widget() {
            self.var_set(slot, id.0 as i32);
            self.target = id;
        } else {
            self.fail(fault::WIDGET_ALLOC);
        }
    }

    #[inline(never)]
    fn w_parent<P: Platform>(&mut self, ctx: &mut Ctx<P>, parent: ObjPtr) {
        if self.target.is_some() {
            let parent = self.widget_ref(ctx, parent);
            ctx.objs.add_child(parent, self.target);
        }
    }

    /// Widget `id` as the program means it: 0 (the root) is an app's own container
    /// (`domain.rs`).
    #[inline]
    fn widget_ref<P: Platform>(&self, ctx: &Ctx<P>, id: ObjPtr) -> WidgetId {
        if id == 0
            && let Some(c) = ctx.domains.root_of(self.domain)
        {
            return c;
        }
        WidgetId(id)
    }

    /// Scalar property write on the target (W_SET).
    #[inline(never)]
    fn w_set<P: Platform>(&mut self, ctx: &mut Ctx<P>, prop_id: u8, val: i32) {
        if self.target.is_none() {
            return;
        }
        // Widgets keep global font / image ids; an app's are mapped (`domain.rs`).
        let local = val;
        let val = match prop_id {
            PROP_FONT_ID => i32::from(ctx.domains.font(self.domain, local as u8)),
            PROP_IMAGE_ID => i32::from(ctx.domains.image(self.domain, local as u8)),
            _ => val,
        };
        self.set_scalar_prop(ctx, prop_id, val);
        ctx.objs.flex_note(self.target, prop_id);
        if prop_id == PROP_FONT_ID || prop_id == PROP_IMAGE_ID {
            self.load_resource(ctx, prop_id, local as u8);
        }
        // An active layer root's position/visibility only places its surface.
        if matches!(prop_id, PROP_LOC_X | PROP_LOC_Y | PROP_VISIBLE) && ctx.objs.is_active_layer_root(self.target) {
            return;
        }
        // A property write that changes appearance schedules a redraw, so
        // `render()` repaints without an explicit `dirty(widget)` call.
        if prop_is_visual(prop_id) {
            ctx.objs.mark_dirty(self.target);
        }
    }

    /// Compound property write on the target (W_SET_LEN).
    #[inline(never)]
    fn w_set_len<P: Platform>(&mut self, ctx: &mut Ctx<P>, prop_id: u8, data: &[u8]) {
        if self.target.is_some() {
            self.set_compound_prop(prop_id, data, ctx);
            ctx.objs.flex_note(self.target, prop_id);
            if prop_id == PROP_LOCATION && ctx.objs.is_active_layer_root(self.target) {
                return;
            }
            if prop_is_visual(prop_id) {
                ctx.objs.mark_dirty(self.target);
            }
        }
    }

    /// OP_W_SETALL: the fields after the varint length, in order.
    #[inline(never)]
    fn w_setall<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Length: an unsigned varint of at most 5 bytes.
        let mut len = 0usize;
        let mut shift = 0;
        loop {
            let b = self.read_u8();
            if self.state == VmState::Error {
                return;
            }
            len |= ((b & 0x7F) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return self.fail(fault::BAD_OPERAND);
            }
        }
        let start = self.pc as usize;
        let end = start + len;
        if end > self.code.len() {
            return self.fail(fault::CODE_OOB);
        }
        // The fields are copied out: each write needs `&mut self`.
        let mut small = [0u8; 64];
        let mut big = Vec::new();
        let buf: &mut [u8] = if len <= small.len() {
            &mut small[..len]
        } else if big.try_reserve_exact(len).is_ok() {
            big.resize(len, 0);
            &mut big
        } else {
            return self.fail(fault::STR_ALLOC);
        };
        self.code.read_bytes(start, buf);
        self.pc = end as CodeAddr;
        self.run_setall(ctx, buf);
    }

    /// W_SETALL fields (`buf` = what follows the length).
    fn run_setall<P: Platform>(&mut self, ctx: &mut Ctx<P>, buf: &[u8]) {
        let mut i = 0;
        while i < buf.len() && self.state != VmState::Error {
            let Some((key, n)) = proto::decode_varint(buf, i) else {
                return self.fail(fault::CODE_OOB);
            };
            i += n;
            let Ok(prop) = u8::try_from(key >> 3) else {
                return self.fail(fault::BAD_OPERAND);
            };
            match (key & 7) as u8 {
                proto::WT_VARINT => {
                    let Some((val, n)) = proto::decode_signed_varint(buf, i) else {
                        return self.fail(fault::CODE_OOB);
                    };
                    i += n;
                    self.w_set(ctx, prop, val);
                }
                proto::WT_STACK => {
                    let val = self.pop();
                    if self.state == VmState::Error {
                        return;
                    }
                    self.w_set(ctx, prop, val);
                }
                proto::WT_LEN => {
                    let Some((len, n)) = proto::decode_varint(buf, i) else {
                        return self.fail(fault::CODE_OOB);
                    };
                    i += n;
                    let Some(data) = buf.get(i..i + len as usize) else {
                        return self.fail(fault::CODE_OOB);
                    };
                    i += len as usize;
                    self.w_set_len(ctx, prop, data);
                }
                _ => return self.fail(fault::BAD_OPERAND),
            }
        }
    }

    /// OP_W_LOAD: run one block of the widget page — a run of static widget
    /// setup statements the compiler moved out of the bytecode. Block at
    /// `off`: u16 LE length + ops (see `PG_*`). Each op does exactly what its
    /// bytecode form would, so ids, tree order and the final target match.
    #[inline(never)]
    fn load_page_block<P: Platform>(&mut self, ctx: &mut Ctx<P>, off: u32) {
        let mut hdr = [0u8; 2];
        if off + 2 > self.page_len {
            return self.fail(fault::PAGE);
        }
        ctx.flash.read(self.page_base + off, &mut hdr);
        let len = u16::from_le_bytes(hdr) as usize;
        if off as usize + 2 + len > self.page_len as usize {
            return self.fail(fault::PAGE);
        }
        let mut buf = Vec::new();
        if buf.try_reserve_exact(len).is_err() {
            return self.fail(fault::PAGE);
        }
        buf.resize(len, 0);
        ctx.flash.read(self.page_base + off + 2, &mut buf);
        self.run_page_ops(ctx, &buf);
    }

    #[inline]
    fn run_page_ops<P: Platform>(&mut self, ctx: &mut Ctx<P>, buf: &[u8]) {
        let len = buf.len();
        let mut i = 0;
        while i < len && self.state != VmState::Error {
            let op = buf[i];
            // Every op has at least one operand byte.
            let Some(&arg) = buf.get(i + 1) else {
                return self.fail(fault::PAGE);
            };
            i += 2;
            match op {
                0x01..=0x3F | PG_STORE => {
                    let pos = if op == PG_STORE { i } else { i - 1 };
                    let Some((val, n)) = proto::decode_signed_varint(buf, pos) else {
                        return self.fail(fault::PAGE);
                    };
                    i = pos + n;
                    if op == PG_STORE {
                        let slot = self.resolve_slot(arg);
                        self.var_set(slot, val);
                    } else {
                        self.w_set(ctx, op, val);
                    }
                }
                0x40..=0x7F => {
                    let end = i + arg as usize;
                    let Some(data) = buf.get(i..end) else {
                        return self.fail(fault::PAGE);
                    };
                    i = end;
                    self.w_set_len(ctx, op, data);
                }
                PG_ALLTAR => self.w_alltar(ctx, arg),
                PG_TARGET_ID..=PG_PARENT => {
                    let id = if op & 1 != 0 {
                        self.var_get(self.resolve_slot(arg)) as ObjPtr
                    } else {
                        arg as ObjPtr
                    };
                    if op & 2 != 0 {
                        self.w_parent(ctx, id);
                    } else {
                        self.target = WidgetId(id);
                    }
                }
                _ => return self.fail(fault::PAGE),
            }
        }
    }

    // --- Property R/W ---

    fn set_scalar_prop<P: Platform>(&mut self, ctx: &mut Ctx<P>, prop_id: u8, val: i32) {
        Self::set_scalar_prop_on(ctx, self.target, prop_id, val);
    }

    /// Write a scalar property of an explicit `target` widget. Body of
    /// `set_scalar_prop` lifted to a target-parameterized associated function
    /// so the animation engine can drive any widget without disturbing the
    /// VM's current `self.target`. Marks the widget dirty exactly as before.
    pub(crate) fn set_scalar_prop_on<P: Platform>(ctx: &mut Ctx<P>, target: WidgetId, prop_id: u8, val: i32) {
        // Layer props live in the pool's layer table, not in the widget.
        if matches!(prop_id, crate::proto::PROP_LAYER | crate::proto::PROP_LAYER_ALPHA) {
            if prop_id == crate::proto::PROP_LAYER {
                ctx.objs.set_layer(target, val);
            } else {
                ctx.objs.set_layer_alpha(target, val);
            }
            return;
        }
        // Boolean flag props: one table lookup + one set_flag instead of a
        // full match arm (and get_mut bounds check) per flag.
        let flag = match prop_id {
            PROP_VISIBLE => FLAG_VISIBLE,
            PROP_ENABLED => FLAG_ENABLED,
            PROP_CLICKABLE => FLAG_CLICKABLE,
            PROP_CHECKED => FLAG_CHECKED,
            PROP_CLIP_CHILDREN => FLAG_CLIP_CHILDREN,
            PROP_MULTI_LINE => FLAG_MULTI_LINE,
            PROP_RICH_TEXT => FLAG_RICH_TEXT,
            PROP_ON_CLICK => FLAG_HAS_ON_CLICK,
            PROP_ON_PAINT => FLAG_HAS_ON_PAINT,
            PROP_ON_TAP | PROP_ON_CHANGE => FLAG_HAS_ON_TAP,
            _ => 0,
        };
        if flag != 0 {
            // Checking a group button, or a radio with a group_id, unchecks
            // the rest of its group. Radios in group 0 keep the plain flag.
            if flag == FLAG_CHECKED
                && val != 0
                && match ctx.objs.get(target).kind {
                    KIND_BUTTON_GROUP => true,
                    KIND_RADIO => ctx.objs.group_id(target) != 0,
                    _ => false,
                }
            {
                ctx.objs.select_in_group(target);
                return;
            }
            set_flag(&mut ctx.objs.get_mut(target).flags, flag, val != 0);
            // The task that wires a handler gets the widget's events.
            if val != 0 && flag & (FLAG_HAS_ON_CLICK | FLAG_HAS_ON_TAP | FLAG_HAS_ON_PAINT) != 0 {
                ctx.objs.take_handler(target);
            }
            // Showing/hiding an active layer's root only shows/hides its surface.
            if flag == FLAG_VISIBLE && ctx.objs.layer_placement_changed(target) {
                return;
            }
            if flag & (FLAG_VISIBLE | FLAG_ENABLED) != 0 {
                ctx.objs.mark_dirty(target);
            }
            return;
        }

        // Base widget fields (no extension needed)
        let mut dirty = false;
        let mut prop_set = false;
        if matches!(prop_id, PROP_LOC_X | PROP_LOC_Y | PROP_SIZE_W | PROP_SIZE_H) {
            ctx.objs.will_move(target);
        }
        match prop_id {
            PROP_LOC_X => {
                ctx.objs.get_mut(target).location.x = val as i16;
                dirty = true;
                prop_set = true;
                
            }
            PROP_LOC_Y => {
                ctx.objs.get_mut(target).location.y = val as i16;
                dirty = true;   
                prop_set = true;
            }
            PROP_SIZE_W => {
                ctx.objs.get_mut(target).size.w = val as u16;
                dirty = true;
                prop_set = true;
            }
            PROP_SIZE_H => {
                ctx.objs.get_mut(target).size.h = val as u16;
                dirty = true;
                prop_set = true;
            }
            PROP_BG_COLOR => {
                ctx.objs.get_mut(target).background_color = val as u16;
                prop_set = true;
            }
            PROP_BORDER_COLOR => {
                ctx.objs.get_mut(target).border_color = val as u16;
                prop_set = true;
            }
            PROP_KIND => {
                ctx.objs.get_mut(target).kind = val as u8;
                prop_set = true;
            }
            _ => {}
        }

        if dirty {
            if matches!(prop_id, PROP_LOC_X | PROP_LOC_Y) {
                // Only moved: its pixels may be shifted (`render::shift_moved`).
                ctx.objs.mark_moved(target);
            } else {
                ctx.objs.mark_dirty(target);
            }
        }
        if prop_set {
            return;
        }

        // Extension fields (allocate on demand)
        ctx.objs.update_ext(target, |ext| {
            match prop_id {
                PROP_BORDER_T => ext.border.top = val as u8,
                PROP_BORDER_R => ext.border.right = val as u8,
                PROP_BORDER_B => ext.border.bottom = val as u8,
                PROP_BORDER_L => ext.border.left = val as u8,
                PROP_PADDING_T => ext.padding.top = val as u8,
                PROP_PADDING_R => ext.padding.right = val as u8,
                PROP_PADDING_B => ext.padding.bottom = val as u8,
                PROP_PADDING_L => ext.padding.left = val as u8,
                PROP_TEXT_COLOR => ext.text_color = val as u16,
                PROP_FONT_ID => ext.font_id = val as u8,
                PROP_TEXT_ALIGN => ext.text_align = val as u8,
                PROP_PRESS_COLOR => ext.press_color = val as u16,
                PROP_IMAGE_ID => ext.image_id = val as u8,
                PROP_BORDER_RADIUS => ext.border_radius = val as u16,
                PROP_VALUE => ext.value = val as WValue,
                PROP_MAX_LENGTH => ext.max_length = val as u8,
                PROP_CURSOR_POS => ext.value = val as WValue,
                PROP_SCROLL_Y => ext.value = val as WValue,
                PROP_GRADIENT_COLOR => ext.gradient_color = val as u16,
                PROP_GRADIENT_DIR => ext.gradient_dir = val as u8,
                PROP_ALPHA => ext.alpha = val as u8,
                PROP_GROUP_ID => ext.group_id = val as u8,
                PROP_FLEX => ext.flex = val as u8,
                PROP_FLEX_GAP => ext.flex_gap = val as u8,
                PROP_FLEX_GROW => ext.flex_grow = val as u8,
                PROP_GRID_COLS => ext.grid_cols = val as u8,
                // KIND_GRAPH props share storage with value/max_length/image_id;
                // safe because a single widget kind is exclusive.
                PROP_GRAPH_ARR => ext.value = val as WValue,
                PROP_GRAPH_COUNT => ext.max_length = val as u8,
                PROP_GRAPH_FLAGS => ext.image_id = val as u8,
                _ => {}
            }
        });
    }

    fn get_scalar_prop<P: Platform>(&self, ctx: &Ctx<P>, prop_id: u8) -> i32 {
        Self::get_scalar_prop_on(ctx, self.target, prop_id)
    }

    /// Read a scalar property of an explicit `target` widget. Body of
    /// `get_scalar_prop` lifted to a target-parameterized associated function
    /// so the animation engine can sample any widget without disturbing the
    /// VM's current `self.target`.
    pub(crate) fn get_scalar_prop_on<P: Platform>(ctx: &Ctx<P>, target: WidgetId, prop_id: u8) -> i32 {
        let w = ctx.objs.get(target);
        // One extension lookup for all ext props; no extension reads as the defaults.
        let e = &ctx.objs.ext(target);
        let link = |id: WidgetId| if id.is_none() { -1 } else { id.0 as i32 };
        match prop_id {
            crate::proto::PROP_LAYER => ctx.objs.layer_of(target),
            crate::proto::PROP_LAYER_ALPHA => ctx.objs.layer_alpha(target),
            // Base fields
            PROP_LOC_X => w.location.x as i32,
            PROP_LOC_Y => w.location.y as i32,
            PROP_SIZE_W => w.size.w as i32,
            PROP_SIZE_H => w.size.h as i32,
            PROP_VISIBLE => {
                if w.flags & FLAG_VISIBLE != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_ENABLED => {
                if w.flags & FLAG_ENABLED != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_CLICKABLE => {
                if w.flags & FLAG_CLICKABLE != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_CHECKED => {
                if w.flags & FLAG_CHECKED != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_CLIP_CHILDREN => {
                if w.flags & FLAG_CLIP_CHILDREN != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_MULTI_LINE => {
                if w.flags & FLAG_MULTI_LINE != 0 {
                    1
                } else {
                    0
                }
            }
            PROP_RICH_TEXT => (w.flags & FLAG_RICH_TEXT != 0) as i32,
            PROP_BG_COLOR => w.background_color as i32,
            PROP_BORDER_COLOR => w.border_color as i32,
            PROP_KIND => w.kind as i32,
            // Tree links (read-only): -1 = none.
            PROP_PARENT => link(w.parent),
            PROP_FIRST_CHILD => link(w.first_child),
            PROP_NEXT_SIBLING => link(w.next_sibling),
            // Extension fields (return defaults when no extension)
            PROP_BORDER_T => e.border.top as i32,
            PROP_BORDER_R => e.border.right as i32,
            PROP_BORDER_B => e.border.bottom as i32,
            PROP_BORDER_L => e.border.left as i32,
            PROP_PADDING_T => e.padding.top as i32,
            PROP_PADDING_R => e.padding.right as i32,
            PROP_PADDING_B => e.padding.bottom as i32,
            PROP_PADDING_L => e.padding.left as i32,
            // String id of the widget's text (PTR_NONE = none → reads as "").
            PROP_TEXT => e.text_id as i32,
            PROP_TEXT_COLOR => e.text_color as i32,
            PROP_FONT_ID => e.font_id as i32,
            PROP_TEXT_ALIGN => e.text_align as i32,
            PROP_PRESS_COLOR => e.press_color as i32,
            PROP_IMAGE_ID => e.image_id as i32,
            PROP_BORDER_RADIUS => e.border_radius as i32,
            PROP_VALUE => e.value as i32,
            PROP_ON_CLICK => (w.flags & FLAG_HAS_ON_CLICK != 0) as i32,
            PROP_ON_PAINT => (w.flags & FLAG_HAS_ON_PAINT != 0) as i32,
            PROP_ON_TAP | PROP_ON_CHANGE => (w.flags & FLAG_HAS_ON_TAP != 0) as i32,
            PROP_MAX_LENGTH => e.max_length as i32,
            PROP_CURSOR_POS => e.value as i32,
            PROP_SCROLL_Y => e.value as i32,
            PROP_GRADIENT_COLOR => e.gradient_color as i32,
            PROP_GRADIENT_DIR => e.gradient_dir as i32,
            PROP_ALPHA => e.alpha as i32,
            PROP_GROUP_ID => e.group_id as i32,
            PROP_FLEX => e.flex as i32,
            PROP_FLEX_GAP => e.flex_gap as i32,
            PROP_FLEX_GROW => e.flex_grow as i32,
            PROP_GRID_COLS => e.grid_cols as i32,
            PROP_GRAPH_ARR => e.value as i32,
            PROP_GRAPH_COUNT => e.max_length as i32,
            PROP_GRAPH_FLAGS => e.image_id as i32,
            _ => 0,
        }
    }

    // === Animation engine ===

    /// Animation slot cap: RES_SYSCFG `anim_slots`, else the BSP's `P::ANIM_SLOTS`.
    fn anim_cap<P: Platform>(&self) -> usize {
        if self.anim_slots > 0 {
            self.anim_slots as usize
        } else {
            P::ANIM_SLOTS
        }
    }

    /// Begin (or replace) a tween on `target`'s `prop_id`. The starting value
    /// is the property's current value (sampled now). A tween already running
    /// on the same (target, prop) is overwritten so the newest call wins.
    /// Silently dropped when the pool (`anim_cap`) is full.
    fn start_anim<P: Platform>(
        &mut self,
        ctx: &Ctx<P>,
        target: WidgetId,
        prop_id: u8,
        to: i32,
        duration_ms: u16,
        ctrl: u8,
        on_end: u16,
        now: u32,
    ) {
        if target.is_none() {
            return;
        }
        let anim = Anim {
            target,
            prop_id,
            ctrl,
            from: Self::get_scalar_prop_on(ctx, target, prop_id),
            to,
            start_ms: now,
            duration_ms: duration_ms.max(1),
            on_end,
            active: true,
        };
        // A new tween on a property already animating supersedes the old one.
        for a in self.anims.iter_mut() {
            if a.active && a.target == target && a.prop_id == prop_id {
                *a = anim;
                return;
            }
        }
        // Reuse a finished slot, else append while under the per-BSP cap.
        if let Some(slot) = self.anims.iter_mut().find(|a| !a.active) {
            *slot = anim;
        } else if self.anims.len() < self.anim_cap::<P>() {
            self.anims.push(anim);
        }
    }

    /// Cancel all tweens on `target`.
    fn stop_anim(&mut self, target: WidgetId) {
        for a in self.anims.iter_mut() {
            if a.target == target {
                a.active = false;
            }
        }
    }

    /// Cancel the tween on `target`'s specific `prop_id`.
    fn stop_anim_prop(&mut self, target: WidgetId, prop_id: u8) {
        for a in self.anims.iter_mut() {
            if a.target == target && a.prop_id == prop_id {
                a.active = false;
            }
        }
    }

    /// Advance all active tweens to time `now` (millis). Applies each
    /// interpolated value through the same property setter + dirty-mark path
    /// as W_SET, but only when the integer value actually changes — the device
    /// loop spins far faster than one visible step per frame, so this gate
    /// avoids redundant redraws. Repeating tweens restart (loop) or reverse
    /// (yoyo); one-shot tweens deactivate and fire their `on_end` callback.
    /// Called every main-loop iteration regardless of `self.state`.
    #[inline(never)]
    /// Record every prop under a running tween, as it is now, in `snap`
    /// (render-owned in step 3: `snapshot.rs`, `runtime::snapshot_fill`).
    pub(crate) fn snapshot_anims<P: Platform>(&self, ctx: &Ctx<P>, snap: &mut crate::snapshot::Snapshot) {
        for a in self.anims.iter().filter(|a| a.active) {
            snap.put(a.target, a.prop_id, Self::get_scalar_prop_on(ctx, a.target, a.prop_id));
        }
    }

    pub fn tick_animations<P: Platform>(&mut self, ctx: &mut Ctx<P>, now: u32) {
        let mut any_done = false;
        let n = self.anims.len();
        for i in 0..n {
            let a = self.anims[i]; // snapshot — releases the borrow on self.anims
            if !a.active {
                continue;
            }
            let dur = a.duration_ms.max(1) as u32;
            let elapsed = now.wrapping_sub(a.start_ms);
            let done = elapsed >= dur;
            // `elapsed < dur <= u16::MAX`: the shift stays in a u32.
            let t = if done { T_ONE } else { ((elapsed << T_BITS) / dur) as i32 };
            let te = ease(a.ctrl, t);
            let val = if prop_is_color(a.prop_id) {
                crate::lcd::lerp_rgb565(a.from as u16, a.to as u16, te as u16, T_ONE as u16) as i32
            } else {
                a.from + ((a.to as i64 - a.from as i64) * te as i64 / T_ONE as i64) as i32
            };
            if val != Self::get_scalar_prop_on(ctx, a.target, a.prop_id) {
                Self::set_scalar_prop_on(ctx, a.target, a.prop_id, val);
                ctx.objs.flex_note(a.target, a.prop_id);
                // The setter marked a location change as a move
                // (`mark_moved`); `mark_dirty` would list it as changed and
                // keep its pixels from being shifted.
                if prop_is_visual(a.prop_id) && !matches!(a.prop_id, PROP_LOC_X | PROP_LOC_Y) {
                    ctx.objs.mark_dirty(a.target);
                }
            }
            if done {
                if a.ctrl & ANIM_YOYO != 0 {
                    let s = &mut self.anims[i];
                    core::mem::swap(&mut s.from, &mut s.to);
                    s.start_ms = now;
                } else if a.ctrl & ANIM_LOOP != 0 {
                    self.anims[i].start_ms = now;
                } else {
                    self.anims[i].active = false;
                    any_done = true;
                    // `on_end` is a 1-based func_id (as pushed by `@fn`);
                    // resolve to a bytecode offset before queueing, mirroring
                    // the widget-callback path. Scope the immutable borrow so
                    // the mutable enqueue can follow.
                    if a.on_end != 0 {
                        if let Some(off) = self.func_addr(a.on_end) {
                            self.enqueue_callback(off, &[a.target.0 as i32, a.prop_id as i32]);
                        }
                    }
                }
            }
        }
        if any_done {
            self.anims.retain(|a| a.active);
        }
    }

    fn set_compound_prop<P: Platform>(&mut self, prop_id: u8, data: &[u8], ctx: &mut Ctx<P>) {
        if prop_id == PROP_TEXT {
            if self.target.is_some() {
                // `set(text, "…")` literal: interned like OP_STR_LIT, so a
                // handler that runs it repeatedly reuses one slot.
                if let Some(str_id) = ctx.objs.str_intern(data) {
                    ctx.objs.update_ext(self.target, |ext| {
                        ext.text_id = str_id;
                    });
                } else {
                    self.fail(fault::STR_ALLOC);
                }
            }
            return;
        }

        let (vals, count) = proto::unpack_signed_varints(data);

        // Base fields
        match prop_id {
            PROP_LOCATION if count >= 2 => {
                ctx.objs.will_move(self.target);
                let w = ctx.objs.get_mut(self.target);
                w.location = Offset {
                    x: vals[0] as i16,
                    y: vals[1] as i16,
                };
                ctx.objs.mark_moved(self.target);
                return;
            }
            PROP_SIZE if count >= 2 => {
                ctx.objs.will_move(self.target);
                let w = ctx.objs.get_mut(self.target);
                w.size = Size {
                    w: vals[0] as u16,
                    h: vals[1] as u16,
                };
                ctx.objs.mark_dirty(self.target);
                return;
            }
            _ => {}
        }

        // Extension fields
        ctx.objs.update_ext(self.target, |ext| {
            match prop_id {
                PROP_BORDER_EDGES if count >= 4 => {
                    ext.border =
                        Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                PROP_PADDING if count >= 4 => {
                    ext.padding =
                        Edges::new(vals[0] as u8, vals[1] as u8, vals[2] as u8, vals[3] as u8);
                }
                _ => {}
            }
        });
    }

    // --- Array operations ---

    /// `eepromReadArr` / `eepromWriteArr`: move `count` bytes between the
    /// EEPROM at `addr` and `arr[0..count]` (written bytes are the low byte
    /// of each element). Bad array → BAD_ARRAY, `count` outside the array →
    /// BAD_OPERAND, range outside the EEPROM or a bus error → EEPROM.
    fn eeprom_array<P: Platform>(
        &mut self,
        ctx: &mut Ctx<P>,
        write: bool,
        addr: i32,
        arr_id: i32,
        count: i32,
    ) {
        let Some(len) = Self::find_array(ctx, arr_id).map(|a| a.len()) else {
            return self.fail(fault::BAD_ARRAY);
        };
        if count < 0 || count as usize > len {
            return self.fail(fault::BAD_OPERAND);
        }
        let count = count as usize;
        if ctx.eeprom.range(addr, count).is_none() {
            return self.fail(fault::EEPROM);
        }
        let mut buf = [0u8; crate::eeprom::CHUNK];
        let mut done = 0;
        while done < count {
            let n = (count - done).min(buf.len());
            let at = addr + done as i32;
            let ok = if write {
                let data = &ctx.objs.arr_mut(arr_id as ObjPtr).unwrap()[done..done + n];
                for (b, &v) in buf.iter_mut().zip(data.iter()) {
                    *b = v as u8;
                }
                self.eeprom_io(ctx, at, &mut buf[..n], true)
            } else {
                let ok = self.eeprom_io(ctx, at, &mut buf[..n], false);
                let data = &mut ctx.objs.arr_mut(arr_id as ObjPtr).unwrap()[done..done + n];
                for (v, &b) in data.iter_mut().zip(buf.iter()) {
                    *v = b as i32;
                }
                ok
            };
            if !ok {
                return self.fail(fault::EEPROM);
            }
            done += n;
        }
    }

    /// Elements of the array a VM value names (None: no array).
    fn find_array<P: Platform>(ctx: &Ctx<P>, arr_id: i32) -> Option<&[i32]> {
        if arr_id < 0 {
            return None;
        }
        ctx.objs.arr(arr_id as ObjPtr)
    }

    fn arr_store<P: Platform>(&mut self, ctx: &mut Ctx<P>, arr_id: i32, idx: i32, val: i32) {
        let arr = if arr_id < 0 { None } else { ctx.objs.arr_mut(arr_id as ObjPtr) };
        match arr {
            Some(arr) => match arr.get_mut(idx as usize) {
                Some(v) if idx >= 0 => *v = val,
                _ => self.fail(fault::ARRAY_INDEX),
            },
            None => self.fail(fault::BAD_ARRAY),
        }
    }

    fn arr_len<P: Platform>(&mut self, ctx: &Ctx<P>, arr_id: i32) {
        match Self::find_array(ctx, arr_id) {
            Some(arr) => self.push(arr.len() as i32),
            None => self.fail(fault::BAD_ARRAY),
        }
    }

    // --- File ops ---

    /// Map a handle (1 or 2) to the open_files index (0 or 1).
    /// Returns None if handle is out of range or slot is empty.
    fn file_slot_index(&self, handle: i32) -> Option<usize> {
        let idx = match handle {
            1 => 0,
            2 => 1,
            _ => return None,
        };
        if self.open_files[idx].is_some() {
            Some(idx)
        } else {
            None
        }
    }

    fn file_slot(&self, handle: i32) -> Option<&OpenFile> {
        self.file_slot_index(handle)
            .map(|i| self.open_files[i].as_ref().unwrap())
    }

    fn file_slot_mut(&mut self, handle: i32) -> Option<&mut OpenFile> {
        let idx = self.file_slot_index(handle)?;
        self.open_files[idx].as_mut()
    }

    /// Reads up to `buf.len()` bytes of resource file `handle` (open) from its
    /// position; the bytes read.
    fn resource_read<P: Platform>(&mut self, ctx: &Ctx<P>, handle: i32, buf: &mut [u8]) -> usize {
        let Some(f) = self.file_slot_mut(handle) else {
            return 0;
        };
        let n = buf.len().min(f.size.saturating_sub(f.pos) as usize);
        ctx.flash.read(f.offset + f.pos, &mut buf[..n]);
        f.pos += n as u32;
        n
    }

    /// The app syscalls that need the calling task (`apps::SYS_APP_CURRENT..=SYS_APP_EXIT`,
    /// docs/apps.md "Step 2"): the runtime acts on the requests after the burst.
    fn app_call<P: Platform>(&mut self, ctx: &mut Ctx<P>, id: u8, args: &[i32]) {
        use crate::apps as a;
        use crate::domain::{MAX_APPS, PENDING, SYSTEM};
        let arg0 = args.first().copied().unwrap_or(0);
        let text = |ctx: &Ctx<P>| alloc::string::String::from(core::str::from_utf8(ctx.objs.str_get(arg0 as ObjPtr)).unwrap_or(""));
        let pending = |ctx: &Ctx<P>, t: &str| ctx.domains.starts.iter().find(|s| s.1 == t).map(|s| s.0);
        let r = match id {
            a::SYS_APP_CURRENT => {
                let cur = ctx.domains.get(self.domain).map(|d| d.info.id.clone()).unwrap_or_default();
                match ctx.objs.str_alloc(cur.as_bytes()) {
                    Some(p) => p as i32,
                    None => return self.fail(fault::STR_ALLOC),
                }
            }
            a::SYS_APP_START => {
                let t = text(ctx);
                if a::find(P::app_files(), &t).is_none() {
                    -1
                } else if let Some(d) = ctx.domains.find(&t) {
                    // Running: shown (as `main` it is what the launcher shows).
                    let app = ctx.domains.get(d).map(|x| (x.task, x.container.is_some())).unwrap_or((0, false));
                    ctx.domains.show = Some(if app.1 { d } else { 0 });
                    i32::from(app.0)
                } else if let Some(slot) = pending(ctx, &t) {
                    i32::from(slot)
                } else if ctx.domains.ids().count() + ctx.domains.starts.len() >= MAX_APPS {
                    -2
                } else {
                    let me = ctx.objs.cur_task;
                    match ctx.tasks.spawn(ProgramRef::new(b"main", 0), me, PENDING) {
                        Some(slot) => {
                            ctx.domains.starts.push((slot, t));
                            i32::from(slot)
                        }
                        None => -3,
                    }
                }
            }
            a::SYS_APP_SHOW => {
                let task = u8::try_from(arg0).unwrap_or(0xFF);
                if arg0 == 0 {
                    ctx.domains.show = Some(0);
                    1
                } else if let Some(d) = ctx.domains.of_task(task) {
                    ctx.domains.show = Some(d);
                    1
                } else {
                    // A start on its way is shown when it opens.
                    i32::from(ctx.domains.starts.iter().any(|s| s.0 == task))
                }
            }
            a::SYS_APP_TASK => {
                let t = text(ctx);
                match ctx.domains.find(&t) {
                    Some(d) => ctx.domains.get(d).map_or(-1, |x| i32::from(x.task)),
                    None => pending(ctx, &t).map_or(-1, i32::from),
                }
            }
            _ => {
                // SYS_APP_EXIT
                if self.domain == SYSTEM || self.domain == PENDING {
                    0
                } else {
                    if ctx.domains.root_of(self.domain).is_some() {
                        ctx.domains.exits |= 1 << self.domain;
                    } else {
                        // Running as `main` (`runApp`): back to the system image.
                        crate::apps::request_run(None);
                    }
                    self.next_program = None;
                    self.state = VmState::Exit;
                    return;
                }
            }
        };
        self.push(r);
    }

    /// Loads font / image `local` (`prop_id` says which) of this domain's image, under its
    /// global id.
    fn load_resource<P: Platform>(&self, ctx: &mut Ctx<P>, prop_id: u8, local: u8) {
        let Some(fs) = ctx.fs_of(self.domain).cloned() else { return };
        let map = ctx.domains.get(self.domain).map(|a| a.ids.clone());
        match (prop_id == PROP_FONT_ID, map) {
            (true, None) => {
                ctx.fonts.find_or_load(local, &fs, &ctx.flash, P::CACHE_FONT_BITMAPS);
            }
            (true, Some(m)) => {
                ctx.fonts.find_or_load_mapped(local, &fs, &ctx.flash, P::CACHE_FONT_BITMAPS, &m.fonts);
            }
            (false, None) => {
                #[cfg(feature = "jpeg")]
                ctx.images.find_or_load(local, &fs, &ctx.flash, &ctx.jpeg);
                #[cfg(not(feature = "jpeg"))]
                ctx.images.find_or_load(local, &fs, &ctx.flash);
            }
            (false, Some(m)) => {
                #[cfg(feature = "jpeg")]
                ctx.images.find_or_load_mapped(local, &fs, &ctx.flash, &ctx.jpeg, &m.images);
                #[cfg(not(feature = "jpeg"))]
                ctx.images.find_or_load_mapped(local, &fs, &ctx.flash, &m.images);
            }
        }
    }

    /// `F_READ` / `F_WRITE`'s `addr..addr + len` in the flash region: as is for the system
    /// image, moved into its range (and bounded to it) for an app.
    fn flash_addr<P: Platform>(&self, ctx: &Ctx<P>, addr: u32, len: u32) -> Option<u32> {
        if self.domain == crate::domain::SYSTEM {
            return Some(addr);
        }
        ctx.domains.get(self.domain)?.flash_addr(P::FS_BASE, addr, len)
    }

    /// Reads (or with `write`, writes) `buf` at `addr` of this domain's EEPROM: the board's
    /// for the system image, the app's own (`domain.rs`) for an app.
    fn eeprom_io<P: Platform>(&self, ctx: &Ctx<P>, addr: i32, buf: &mut [u8], write: bool) -> bool {
        if self.domain == crate::domain::SYSTEM {
            return if write { ctx.eeprom.write(addr, buf) } else { ctx.eeprom.read(addr, buf) };
        }
        if ctx.eeprom.range(addr, buf.len()).is_none() {
            return false;
        }
        if write {
            crate::domain::eeprom_write(self.domain, addr, buf, ctx.systick.millis())
        } else {
            crate::domain::eeprom_read(self.domain, addr, buf)
        }
    }

    /// Open a file by name. Returns handle 1 or 2, or 0xFF on error:
    ///   - fs not mounted
    ///   - name not found
    ///   - resource is not a file (wrong kind)
    ///   - both slots in use
    fn file_open<P: Platform>(&mut self, ctx: &Ctx<P>, name: &[u8]) -> i32 {
        let fs = match ctx.fs_of(self.domain) {
            Some(f) => f,
            None => return FILE_HANDLE_ERR,
        };
        let entry = match fs.find(&ctx.flash, name) {
            Some(e) if e.kind == crate::fs::RES_FILE => e,
            _ => return FILE_HANDLE_ERR,
        };
        let slot = if self.open_files[0].is_none() {
            0
        } else if self.open_files[1].is_none() {
            1
        } else {
            return FILE_HANDLE_ERR;
        };
        self.open_files[slot] = Some(OpenFile {
            offset: entry.offset,
            size: entry.size,
            pos: 0,
        });
        (slot + 1) as i32
    }
}

// === Image header parser ===

/// Parse VM image header. Returns (function table, opcode start, global_count,
/// (widget_count, ext_count) — `None` before v4, wide_addr, portrait). widget_count/ext_count are 0 for v1-v3 images.
///
/// Format v1/v2:
///   version(u8) + function_count(u16 LE) + global_count(u16 LE)
///   + [func_id(u16) + kind(u8) + pad(u8) + offset(u32) + length(u32)] × N
/// Format v3 (adds flags):
///   version(u8) + function_count(u16 LE) + global_count(u16 LE) + flags(u16 LE)
///   + [...]
/// Format v4 (adds widget/ext counts):
///   v3 header + widget_count(u8) + ext_count(u8) + [...]
///   flags bit 0: render_mode (0=dirty, 1=buffered; ignored: XL renders dirty only)
///   flags bit 2: wide_addr (jump/call targets are u32; XL loads no image
///   without it)
///   flags bit 3: portrait (screen rotated 90°)
#[allow(clippy::type_complexity)]
/// A parsed image header (`parse_image_header`).
struct ImageHeader {
    functions: Vec<FuncEntry>,
    opcode_start: usize,
    global_count: u8,
    tree_hint: Option<(u8, u8)>,
    wide: bool,
    portrait: bool,
    var_slots: Option<u16>,
    /// v6: the library tables.
    libs: Option<Box<LibTables>>,
}

fn parse_image_header(data: &[u8]) -> Option<ImageHeader> {
    if data.len() < IMAGE_HEADER_SIZE_V2 {
        return None;
    }

    let version = data[0];
    if version < 1 || version > 6 {
        return None;
    }

    let func_count = u16::from_le_bytes([data[1], data[2]]) as usize;
    // v1: reserved=0, v2+: global_count
    let global_count = if version >= 2 {
        u16::from_le_bytes([data[3], data[4]]) as u8
    } else {
        0
    };

    // v3+: flags field after global_count; v4 adds widget/ext counts.
    let header_size = match version {
        6 => IMAGE_HEADER_SIZE_V6,
        5 => IMAGE_HEADER_SIZE_V5,
        4 => IMAGE_HEADER_SIZE_V4,
        3 => IMAGE_HEADER_SIZE_V3,
        _ => IMAGE_HEADER_SIZE_V2,
    };
    if data.len() < header_size {
        return None;
    }
    let flags = if version >= 3 {
        u16::from_le_bytes([data[5], data[6]])
    } else {
        0
    };
    let wide = flags & IMAGE_FLAG_WIDE_ADDR != 0;
    let portrait = flags & IMAGE_FLAG_PORTRAIT != 0;
    let tree_hint = if version >= 4 { Some((data[7], data[8])) } else { None };
    let var_slots = if version >= 5 { Some(u16::from_le_bytes([data[9], data[10]])) } else { None };

    let table_size = func_count * FUNC_ENTRY_SIZE;
    let funcs_end = header_size + table_size;

    if data.len() < funcs_end {
        return None;
    }
    let (libs, opcode_start) = if version >= 6 {
        let (t, end) = units::parse_tables(data, funcs_end, flags & IMAGE_FLAG_LIBRARY != 0)?;
        (Some(Box::new(t)), end)
    } else {
        (None, funcs_end)
    };

    let mut functions = Vec::with_capacity(func_count);
    // Fixed-size chunks: the field reads below need no bounds checks.
    for e in data[header_size..funcs_end].chunks_exact(FUNC_ENTRY_SIZE) {
        let func_id = u16::from_le_bytes([e[0], e[1]]);
        let kind_byte = e[2];
        // e[3] = pad
        let offset = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
        let length = u32::from_le_bytes([e[8], e[9], e[10], e[11]]);

        let kind = FunctionKind::from_u8(kind_byte).unwrap_or(FunctionKind::UserFunction);

        functions.push(FuncEntry {
            func_id,
            kind,
            offset,
            length,
        });
    }

    Some(ImageHeader { functions, opcode_start, global_count, tree_hint, wide, portrait, var_slots, libs })
}

/// Whether the VM can run an image's bytecode: a wide-address image (u32
/// jump/call targets, the only kind XL reads; ferrite-ui `caps.wide_addr`)
/// whose every offset in `code_len` fits `CodeAddr`.
fn code_addressable(wide: bool, code_len: usize) -> bool {
    wide && code_len <= CodeAddr::MAX as usize
}

// --- Packed u32 helper ---

#[inline]
fn unpack_pair(packed: i32) -> (u16, u16) {
    let val = packed as u32;
    ((val >> 16) as u16, (val & 0xFFFF) as u16)
}

// --- Flag helper ---

#[inline]
fn set_flag(flags: &mut u16, mask: u16, val: bool) {
    if val {
        *flags |= mask;
    } else {
        *flags &= !mask;
    }
}

/// Whether writing `prop_id` changes the widget's on-screen appearance and so
/// should schedule a redraw. Defaults to `true`; only behaviour-only props
/// (state flags, event-handler hooks, input metadata) are excluded so they
/// don't trigger needless repaints.
fn prop_is_visual(prop_id: u8) -> bool {
    !matches!(
        prop_id,
        PROP_ENABLED
            | PROP_CLICKABLE
            | PROP_ON_CLICK
            | PROP_ON_PAINT
            | PROP_ON_TAP
            | PROP_ON_CHANGE
            | PROP_MAX_LENGTH
            | PROP_CURSOR_POS
            // Placements, not redraws (the setter marks what it needs).
            | crate::proto::PROP_LAYER
            | crate::proto::PROP_LAYER_ALPHA
    )
}

// --- Animation helpers ---

/// Animation progress, fixed point: `T_ONE` = done. Q12, so an animation of
/// any duration (`u16` ms) moves at least every 16 ms; Q8 moved in steps of
/// 1/256 of its range (a 21 s scroll: 125 px every 83 ms).
const T_BITS: u32 = 12;
const T_ONE: i32 = 1 << T_BITS;

/// Apply an easing curve to `t` (Q12 fixed-point, 0..=`T_ONE`), returning the
/// eased position in the same range. Curve picked from the low 4 bits of `ctrl`.
/// Quadratic in/out/in-out; anything else is linear.
fn ease(ctrl: u8, t: i32) -> i32 {
    match ctrl & EASE_CURVE_MASK {
        EASE_IN => (t * t) >> T_BITS,
        EASE_OUT => {
            let u = T_ONE - t;
            T_ONE - ((u * u) >> T_BITS)
        }
        EASE_IN_OUT => {
            if t < T_ONE / 2 {
                (2 * t * t) >> T_BITS
            } else {
                let u = T_ONE - t;
                T_ONE - ((2 * u * u) >> T_BITS)
            }
        }
        _ => t,
    }
}

/// True for properties stored as RGB565 colors — these tween channel-wise via
/// `lerp_rgb565` rather than by linear integer interpolation.
fn prop_is_color(prop_id: u8) -> bool {
    matches!(
        prop_id,
        PROP_BG_COLOR | PROP_BORDER_COLOR | PROP_TEXT_COLOR | PROP_PRESS_COLOR | PROP_GRADIENT_COLOR
    )
}

#[cfg(all(test, feature = "mock"))]
mod anim_tests {
    use super::*;
    use crate::mock::{MockBacklight, MockFlash, MockGpio, MockLcd, MockPlatform, MockRtc, MockUsart};
    use crate::backlight::BacklightImpl;
    use crate::flash::FlashImpl;
    use crate::lcd::LcdImpl;
    use crate::rtc::RtcImpl;
    use crate::systick::SystickImpl;
    use crate::usart::UsartImpl;
    use crate::font::FontList;
    use crate::image::ImageList;
    use crate::pool::ObjPool;
    use crate::audio::AudioImpl;
    use crate::gpio::GpioImpl;

    fn mock_ctx() -> Ctx<MockPlatform> {
        Ctx {
            lcd: LcdImpl::with_backend(MockLcd),
            flash: FlashImpl::with_backend(MockFlash),
            fonts: FontList::new(),
            images: ImageList::new(),
            objs: ObjPool::new(),
            render_ctx: None,
            render_away: false,
            fs: None,
            backlight: BacklightImpl::with_backend(MockBacklight),
            rtc: RtcImpl::with_backend(MockRtc),
            usart: UsartImpl::with_backend(MockUsart),
            systick: SystickImpl::init(),
            audio: AudioImpl::none(),
            #[cfg(feature = "jpeg")]
            jpeg: crate::jpeg::JpegImpl::none(),
            gpio: GpioImpl::with_backend(MockGpio::default()),
            eeprom: crate::eeprom::EepromImpl::with_backend(crate::mock::MockEeprom::default()),
            storage: crate::storage::StorageImpl::with_backend(crate::storage::MemStorage::default()),
            cursor_visible: false,
            tasks: crate::task::TaskTable::new(),
            domains: crate::domain::Domains::new(),
        }
    }

    // Allocate one widget and return its id.
    fn one_widget(ctx: &mut Ctx<MockPlatform>) -> WidgetId {
        ctx.objs.alloc_widget().expect("alloc")
    }

    #[test]
    fn idle_render_skips_scan_until_marked() {
        let mut ctx = mock_ctx();
        let root = one_widget(&mut ctx);
        ctx.objs.root = root;
        let child = one_widget(&mut ctx);
        ctx.objs.add_child(root, child);
        assert!(ctx.objs.render.maybe_dirty, "new widgets start dirty");
        crate::render::render_dirty(&mut ctx);
        assert!(!ctx.objs.get(child).is_dirty());
        // The pass that drew keeps the hint; the next scan finds nothing.
        crate::render::render_dirty(&mut ctx);
        assert!(!ctx.objs.render.maybe_dirty);
        // Marking raises it again, and the widget is drawn.
        ctx.objs.mark_dirty(child);
        assert!(ctx.objs.render.maybe_dirty);
        let mut paint = crate::render::PaintList::new();
        crate::render::collect_dirty_paint(&mut ctx, &mut paint);
        crate::render::render_dirty(&mut ctx);
        assert!(!ctx.objs.get(child).is_dirty());
    }

    /// Root 800x480 + a 40x30 child at (100, 50), rendered once.
    fn rendered_pair(ctx: &mut Ctx<MockPlatform>) -> (WidgetId, WidgetId) {
        let root = one_widget(ctx);
        ctx.objs.root = root;
        ctx.objs.get_mut(root).size = crate::types::Size { w: 800, h: 480 };
        let child = one_widget(ctx);
        ctx.objs.add_child(root, child);
        ctx.objs.get_mut(child).location = crate::types::Offset { x: 100, y: 50 };
        ctx.objs.get_mut(child).size = crate::types::Size { w: 40, h: 30 };
        ctx.objs.get_mut(child).background_color = 0x1234;
        crate::render::render_dirty(ctx);
        (root, child)
    }

    /// The render works on the tree itself: its records are kept there and
    /// it clears the dirty bits it drew.
    #[test]
    fn the_render_keeps_its_records_in_the_tree() {
        let mut ctx = mock_ctx();
        let (_, child) = rendered_pair(&mut ctx);
        assert!(!ctx.objs.get(child).is_dirty());
        assert!(ctx.objs.rendered(child));
        // Settle (the pass that drew keeps the hint up).
        crate::render::render_dirty(&mut ctx);
        assert!(!ctx.objs.render.maybe_dirty);
        // A change nobody marked draws nothing; a marked one is drawn.
        ctx.objs.get_mut(child).background_color = 0x4321;
        assert!(!ctx.objs.needs_render());
        ctx.objs.mark_dirty(child);
        assert!(ctx.objs.needs_render());
        crate::render::render_dirty(&mut ctx);
        assert!(!ctx.objs.get(child).is_dirty() && ctx.objs.rendered(child));
    }

    #[test]
    fn a_move_keeps_the_old_on_screen_rect() {
        let mut ctx = mock_ctx();
        let (_, child) = rendered_pair(&mut ctx);
        ctx.objs.will_move(child);
        ctx.objs.get_mut(child).location.x = 300;
        ctx.objs.will_move(child);
        ctx.objs.get_mut(child).location.x = 310;
        ctx.objs.mark_moved(child);
        // The first move's rect (where it is drawn), not the second's.
        assert_eq!(ctx.objs.prev_rect(child), crate::types::Rect::new(100, 50, 40, 30));
        assert!(ctx.objs.get(child).is_dirty());
        crate::render::render_dirty(&mut ctx);
        assert!(ctx.objs.prev_rect(child).is_empty(), "repainted");
    }

    #[test]
    fn a_reparented_widget_repaints_its_old_place() {
        let mut ctx = mock_ctx();
        let (root, child) = rendered_pair(&mut ctx);
        let panel = one_widget(&mut ctx);
        ctx.objs.add_child(root, panel);
        ctx.objs.get_mut(panel).location = crate::types::Offset { x: 400, y: 200 };
        ctx.objs.get_mut(panel).size = crate::types::Size { w: 100, h: 100 };
        crate::render::render_dirty(&mut ctx);
        ctx.objs.add_child(panel, child);
        assert!(ctx.objs.get(child).is_dirty());
        assert_eq!(ctx.objs.prev_rect(child), crate::types::Rect::new(100, 50, 40, 30));
    }

    #[test]
    fn a_reused_pointer_and_a_clear_reset_the_render_records() {
        let mut ctx = mock_ctx();
        let (root, child) = rendered_pair(&mut ctx);
        ctx.objs.destroy_subtree(child);
        ctx.objs.mark_dirty(root);
        crate::render::render_dirty(&mut ctx);
        // FIFO reuse: allocate until the pointer comes back.
        let mut again = one_widget(&mut ctx);
        while again != child {
            again = one_widget(&mut ctx);
        }
        assert!(!ctx.objs.rendered(child));
        assert!(ctx.objs.rendered(root));
        ctx.objs.clear();
        assert!(!ctx.objs.rendered(root));
        assert!(ctx.objs.root.is_none());
    }

    /// A render context in `MODE_AREAS` (the board): renders are recorded on
    /// the tree and only the list goes to the render side.
    fn areas_ctx() -> Ctx<MockPlatform> {
        let mut ctx = mock_ctx();
        let mut r = mock_ctx();
        r.objs = ObjPool::without_storage();
        ctx.render_ctx = Some(alloc::boxed::Box::new(r));
        ctx.objs.set_render_mode(crate::render::MODE_AREAS);
        ctx
    }

    /// The frame the render side played last.
    fn played_ops(ctx: &Ctx<MockPlatform>) -> usize {
        ctx.render_ctx.as_ref().expect("back").objs.render.frame.list().len()
    }

    /// Step 3d: with a render context the drawing builtins record (no wait
    /// for a render running elsewhere); the next render hands the list over.
    #[test]
    fn direct_drawing_is_recorded_for_the_render_side() {
        crate::mock::render_elsewhere(true);
        let mut ctx = areas_ctx();
        let root = one_widget(&mut ctx);
        ctx.objs.root = root;
        // Something to draw: an empty frame isn't handed over.
        ctx.objs.get_mut(root).size = crate::types::Size { w: 800, h: 480 };
        crate::render::render_dirty(&mut ctx);
        assert!(ctx.render_away);
        let mut vm = Vm::new();
        for color in [1, 2] {
            vm.push((10 << 16) | 20);
            vm.push((30 << 16) | 40);
            vm.push(color);
            vm.draw_op(&mut ctx, OP_FILL_RECT);
        }
        assert!(ctx.render_away, "recorded, not waited for");
        assert!(ctx.objs.render.screen_rec.busy());
        assert_eq!(ctx.objs.render.screen_rec.list().len(), 2);
        assert!(ctx.objs.needs_render());
        crate::mock::finish_render();
        crate::render::render_dirty(&mut ctx);
        assert!(!ctx.objs.render.screen_rec.busy() && ctx.objs.render.draws.is_empty());
        assert!(ctx.render_away, "handed over with the next render");
        crate::mock::finish_render();
        crate::render::wait_idle(&mut ctx);
        assert!(played_ops(&ctx) >= 2, "in the frame played");
        crate::mock::render_elsewhere(false);
    }

    #[test]
    fn a_frame_playing_elsewhere_leaves_what_changed_meanwhile_to_the_next() {
        crate::mock::render_elsewhere(true);
        let mut ctx = areas_ctx();
        let root = one_widget(&mut ctx);
        ctx.objs.root = root;
        ctx.objs.get_mut(root).size = crate::types::Size { w: 800, h: 480 };
        let child = one_widget(&mut ctx);
        ctx.objs.add_child(root, child);
        ctx.objs.get_mut(child).size = crate::types::Size { w: 40, h: 30 };
        crate::render::render_dirty(&mut ctx);
        assert!(ctx.render_away && crate::mock::render_pending());
        assert!(ctx.render_ctx.is_none());
        // Changed while the frame plays: waits in the tree, still dirty.
        ctx.objs.get_mut(child).background_color = 0x0F0F;
        ctx.objs.mark_dirty(child);
        crate::render::render_dirty(&mut ctx);
        assert!(ctx.objs.get(child).is_dirty(), "waits for the render side");
        crate::mock::finish_render();
        // Taken back, then recorded and handed over.
        crate::render::render_dirty(&mut ctx);
        assert!(!ctx.objs.get(child).is_dirty());
        assert!(ctx.objs.rendered(child));
        assert!(ctx.render_away);
        crate::mock::finish_render();
        crate::render::wait_idle(&mut ctx);
        assert!(played_ops(&ctx) > 0);
        crate::mock::render_elsewhere(false);
    }

    /// The frame handed to the other core is a list that needs nothing of
    /// the tree: what the VM does meanwhile (strings edited and freed,
    /// widgets written, the whole tree dropped) does not change it.
    #[test]
    fn a_handed_over_frame_needs_nothing_of_the_tree() {
        crate::mock::render_elsewhere(true);
        let mut ctx = areas_ctx();
        let root = one_widget(&mut ctx);
        ctx.objs.root = root;
        ctx.objs.get_mut(root).size = crate::types::Size { w: 800, h: 480 };
        let (label, other, graph) = (one_widget(&mut ctx), one_widget(&mut ctx), one_widget(&mut ctx));
        for w in [label, other, graph] {
            ctx.objs.add_child(root, w);
            ctx.objs.get_mut(w).size = crate::types::Size { w: 40, h: 30 };
        }
        let text = ctx.objs.str_alloc(b"abc").unwrap();
        ctx.objs.set_text_id(label, text);
        let samples = ctx.objs.arr_from(&[1, 2, 3]).unwrap();
        ctx.objs.get_mut(graph).kind = crate::widget::KIND_GRAPH;
        ctx.objs.get_mut(graph).value = samples as _;
        crate::render::render_dirty(&mut ctx);
        assert!(ctx.render_away && crate::mock::render_pending());
        let handed = ctx.render_ctx.is_none();
        // Meanwhile, on the VM's side.
        ctx.objs.str_free(text);
        ctx.objs.arr_mut(samples).unwrap()[1] = 77;
        ctx.objs.get_mut(label).background_color = 0x1111;
        ctx.objs.mark_dirty(label);
        ctx.objs.clear();
        crate::mock::finish_render();
        crate::render::wait_idle(&mut ctx);
        assert!(handed && played_ops(&ctx) > 0);
        crate::mock::render_elsewhere(false);
    }

    #[test]
    fn get_text_reads_the_widgets_own_string() {
        // text_input demo: get(text) on an input after strClear() returned
        // string 0 (the title) because PROP_TEXT had no getter.
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let title = one_widget(&mut ctx);
        let input = one_widget(&mut ctx);
        let empty = one_widget(&mut ctx);
        vm.target = title;
        vm.set_compound_prop(PROP_TEXT, b"Text Input Demo", &mut ctx);
        vm.target = input;
        vm.set_compound_prop(PROP_TEXT, b"erhan", &mut ctx);
        ctx.objs.str_sweep();

        let id = Vm::get_scalar_prop_on(&ctx, input, PROP_TEXT);
        assert_eq!(ctx.objs.str_get(id as ObjPtr), b"erhan");
        let none = Vm::get_scalar_prop_on(&ctx, empty, PROP_TEXT);
        assert_eq!(ctx.objs.str_get(none as ObjPtr), b"", "no text reads as empty");
    }

    #[test]
    fn tree_links_and_flex_props_read_back() {
        // lib/flex.fl walks children via get(first_child)/get(next_sibling)
        // and reads its layout data from the flex props.
        let mut ctx = mock_ctx();
        let root = one_widget(&mut ctx);
        let a = one_widget(&mut ctx);
        let b = one_widget(&mut ctx);
        ctx.objs.add_child(root, a);
        ctx.objs.add_child(root, b);

        let get = |ctx: &Ctx<MockPlatform>, id, prop| Vm::get_scalar_prop_on(ctx, id, prop);
        assert_eq!(get(&ctx, root, PROP_FIRST_CHILD), a.0 as i32);
        assert_eq!(get(&ctx, a, PROP_NEXT_SIBLING), b.0 as i32);
        assert_eq!(get(&ctx, b, PROP_NEXT_SIBLING), -1, "none reads as -1");
        assert_eq!(get(&ctx, b, PROP_FIRST_CHILD), -1);
        assert_eq!(get(&ctx, b, PROP_PARENT), root.0 as i32);
        assert_eq!(get(&ctx, root, PROP_PARENT), -1);

        assert_eq!(get(&ctx, root, PROP_FLEX), 0, "no extension reads as 0");
        Vm::set_scalar_prop_on(&mut ctx, root, PROP_FLEX, 0x62);
        Vm::set_scalar_prop_on(&mut ctx, root, PROP_FLEX_GAP, 8);
        Vm::set_scalar_prop_on(&mut ctx, a, PROP_FLEX_GROW, 2);
        assert_eq!(get(&ctx, root, PROP_FLEX), 0x62);
        assert_eq!(get(&ctx, root, PROP_FLEX_GAP), 8);
        assert_eq!(get(&ctx, a, PROP_FLEX_GROW), 2);
        assert_eq!(get(&ctx, root, PROP_GROUP_ID), 0, "flex bytes don't touch group_id");
    }

    #[test]
    fn linear_tween_interpolates_and_completes() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_LOC_X, 0);

        // 0 -> 100 over 100ms, linear, starting at t=1000.
        vm.start_anim(&ctx, id, PROP_LOC_X, 100, 100, 0, 0, 1000);

        vm.tick_animations(&mut ctx, 1000);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 0);

        vm.tick_animations(&mut ctx, 1050);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 50);

        // At/after duration: snaps to target and the slot is freed.
        vm.tick_animations(&mut ctx, 1100);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 100);
        assert!(vm.anims.is_empty(), "completed one-shot tween must be retired");
    }

    /// `W_GET` of an animated prop: the live value, counted as a render-owned
    /// read; with `snapshot::STALE`, the value the last render's snapshot holds.
    #[test]
    fn reads_of_animated_props_come_from_the_render_snapshot_when_stale() {
        use crate::snapshot::{READS, STALE, STALE_DIFF, Snapshot};
        use core::sync::atomic::Ordering::Relaxed;
        let mut ctx = mock_ctx();
        let id = one_widget(&mut ctx);
        let other = one_widget(&mut ctx);
        assert!(id.0 < 256 && other.0 < 256);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_LOC_X, 0);
        Vm::set_scalar_prop_on(&mut ctx, other, PROP_LOC_X, 7);
        let mut tweens = Vm::new();
        tweens.start_anim(&ctx, id, PROP_LOC_X, 100, 100, 0, 0, 1000);
        tweens.tick_animations(&mut ctx, 1050);
        // The render pass: the snapshot holds 50.
        let mut snap = Snapshot::new();
        tweens.snapshot_anims(&ctx, &mut snap);
        assert_eq!(snap.get(id, PROP_LOC_X), Some(50));
        ctx.objs.render.snapshot = snap;
        tweens.tick_animations(&mut ctx, 1075);
        // g0 = id.loc_x, g1 = other.loc_x
        let code = [
            OP_W_TARGET, id.0 as u8, OP_W_GET, PROP_LOC_X, OP_STORE, 0,
            OP_W_TARGET, other.0 as u8, OP_W_GET, PROP_LOC_X, OP_STORE, 1, OP_HALT,
        ];
        let run = |ctx: &mut Ctx<MockPlatform>| {
            let mut vm = cb_vm(&code);
            vm.tick(ctx, 100);
            (vm.var_get(0), vm.var_get(1))
        };
        let reads = READS.load(Relaxed);
        assert_eq!(run(&mut ctx), (75, 7), "live by default");
        assert_eq!(READS.load(Relaxed) - reads, 1, "one render-owned read");
        STALE.store(true, Relaxed);
        let diff = STALE_DIFF.load(Relaxed);
        let got = run(&mut ctx);
        STALE.store(false, Relaxed);
        assert_eq!(got, (50, 7), "the last render's value; plain props stay live");
        assert_eq!(STALE_DIFF.load(Relaxed) - diff, 1);
    }

    #[test]
    fn color_tween_blends_channelwise() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        // RED (0xF800) -> BLUE (0x001F), linear.
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_BG_COLOR, 0xF800);
        vm.start_anim(&ctx, id, PROP_BG_COLOR, 0x001F, 100, 0, 0, 0);

        vm.tick_animations(&mut ctx, 50); // midpoint
        let mid = Vm::get_scalar_prop_on(&ctx, id, PROP_BG_COLOR) as u16;
        // Midpoint is a genuine blend: some red left, some blue gained.
        assert!(mid != 0xF800 && mid != 0x001F, "got 0x{mid:04X}");
        assert!((mid >> 11) > 0 && (mid >> 11) < 0x1F, "red channel mid");
        assert!((mid & 0x1F) > 0 && (mid & 0x1F) < 0x1F, "blue channel mid");
    }

    #[test]
    fn yoyo_reverses_and_keeps_running() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_LOC_X, 0);
        vm.start_anim(&ctx, id, PROP_LOC_X, 100, 100, ANIM_YOYO, 0, 0);

        // Reach the far end; yoyo swaps endpoints and restarts (stays active).
        vm.tick_animations(&mut ctx, 100);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 100);
        assert_eq!(vm.anims.len(), 1, "yoyo tween must not be retired");

        // Now heading back toward 0.
        vm.tick_animations(&mut ctx, 150);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_LOC_X), 50);
    }

    /// A long animation moves every frame: progress is Q12, not Q8 (a 20 s
    /// tween over 32 000 px moved 125 px every 78 ms).
    #[test]
    fn long_animations_move_every_frame() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_VALUE, 0);
        vm.start_anim(&ctx, id, PROP_VALUE, 32_000, 20_000, 0, 0, 0);
        let mut last = 0;
        for f in 1..=60 {
            vm.tick_animations(&mut ctx, f * 16);
            let v = Vm::get_scalar_prop_on(&ctx, id, PROP_VALUE);
            assert!(v > last, "frame {f}: still {v}");
            assert!((v - f as i32 * 16 * 32_000 / 20_000).abs() <= 8, "frame {f}: {v}");
            last = v;
        }
        vm.tick_animations(&mut ctx, 20_000);
        assert_eq!(Vm::get_scalar_prop_on(&ctx, id, PROP_VALUE), 32_000, "ends on its target");
        // Easing keeps its ends and midpoint.
        for c in [EASE_IN, EASE_OUT, EASE_IN_OUT] {
            assert_eq!((ease(c, 0), ease(c, T_ONE)), (0, T_ONE));
        }
        assert_eq!(ease(EASE_IN_OUT, T_ONE / 2), T_ONE / 2);
    }

    #[test]
    fn start_anim_replaces_same_property() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        let id = one_widget(&mut ctx);
        Vm::set_scalar_prop_on(&mut ctx, id, PROP_LOC_X, 0);
        vm.start_anim(&ctx, id, PROP_LOC_X, 100, 100, 0, 0, 0);
        vm.start_anim(&ctx, id, PROP_LOC_X, 200, 100, 0, 0, 0);
        assert_eq!(vm.anims.len(), 1, "second tween on same prop replaces the first");
    }

    #[test]
    fn pool_is_capped_per_platform() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        // MockPlatform inherits the default ANIM_SLOTS (8). Distinct props so
        // none replace each other.
        let props = [
            PROP_LOC_X, PROP_LOC_Y, PROP_SIZE_W, PROP_SIZE_H,
            PROP_BG_COLOR, PROP_BORDER_COLOR, PROP_TEXT_COLOR, PROP_PRESS_COLOR,
            PROP_BORDER_RADIUS, PROP_ALPHA, // two beyond the cap
        ];
        for &p in &props {
            let id = one_widget(&mut ctx);
            vm.start_anim(&ctx, id, p, 10, 100, 0, 0, 0);
        }
        assert_eq!(vm.anims.len(), MockPlatform::ANIM_SLOTS);
        assert_eq!(MockPlatform::ANIM_SLOTS, 8);
    }

    #[test]
    fn configured_anim_cap_overrides_platform() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        assert!(vm.configure(0, 0, 0, 3));
        let props = [PROP_LOC_X, PROP_LOC_Y, PROP_SIZE_W, PROP_SIZE_H, PROP_BG_COLOR];
        for &p in &props {
            let id = one_widget(&mut ctx);
            vm.start_anim(&ctx, id, p, 10, 100, 0, 0, 0);
        }
        assert_eq!(vm.anims.len(), 3);
    }

    #[test]
    fn configure_zero_keeps_defaults() {
        let mut vm = Vm::new();
        assert!(vm.configure(0, 0, 0, 0));
        assert_eq!(vm.limits(), (STACK_SIZE, CALL_STACK_SIZE, CB_QUEUE_SIZE));
    }

    #[test]
    fn configured_eval_stack_sets_overflow_depth() {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        assert!(vm.configure(4, 0, 0, 0));
        vm.code = VmCode::new(alloc::vec![OP_PUSH_1; 5]);
        vm.run(&mut ctx);
        assert_eq!(vm.fault(), fault::STACK_OVERFLOW);
        assert_eq!(vm.fault_pc(), 4, "5th push overflows a 4-deep stack");

        // Deeper than the default 32: 40 pushes then HALT run cleanly.
        let mut vm = Vm::new();
        assert!(vm.configure(48, 0, 0, 0));
        let mut code = alloc::vec![OP_PUSH_1; 40];
        code.push(OP_HALT);
        vm.code = VmCode::new(code);
        vm.run(&mut ctx);
        assert!(vm.state != VmState::Error);
        assert_eq!(vm.sp, 40);
    }

    // --- Callback frames ---

    /// VM with `code` loaded and 4 globals (callback locals start above them).
    fn cb_vm(code: &[u8]) -> Vm {
        let mut vm = Vm::new();
        vm.code = VmCode::new(code.to_vec());
        vm.global_count = 4;
        vm.state = VmState::Running;
        vm
    }

    /// `tick`'s burst on the reference interpreter (`oracle.rs`): one
    /// instruction at a time, the budget counted here (a critical section
    /// runs past it).
    fn burst_ref<P: Platform>(vm: &mut Vm, ctx: &mut Ctx<P>, mut budget: u32) -> u64 {
        let mut n: u64 = 0;
        loop {
            vm.step_ref(ctx);
            n += 1;
            if vm.state != VmState::Running {
                break;
            }
            if vm.critical {
                continue;
            }
            budget -= 1;
            if budget == 0 {
                break;
            }
        }
        n
    }

    /// A burst (`tc::run`) against the reference interpreter (`oracle.rs`,
    /// the old `exec_one`) on random programs: the
    /// hot opcodes with random operands, out-of-line ones (calls, returns,
    /// syscalls, yields, halts), jumps in and out of the code, cut-off
    /// instructions, each run with a few budgets. Same state, stack,
    /// variables, pc and instruction count; on a fault the same fault and
    /// fault pc (CODE_OOB not compared, see `tc`'s docs).
    #[test]
    fn threaded_dispatch_matches_reference() {
        const OPS: &[(u8, usize)] = &[
            (OP_POP, 0), (OP_DUP, 0), (OP_SWAP, 0), (OP_ADD, 0), (OP_SUB, 0), (OP_MUL, 0),
            (OP_DIV, 0), (OP_MOD, 0), (OP_NEG, 0), (OP_AND, 0), (OP_OR, 0), (OP_NOT, 0),
            (OP_EQ, 0), (OP_NE, 0), (OP_LT, 0), (OP_LE, 0), (OP_GT, 0), (OP_GE, 0),
            (OP_SHL, 0), (OP_SHR, 0), (OP_PUSH_0, 0), (OP_PUSH_1, 0), (OP_PUSH_2, 0),
            (OP_PUSH_M1, 0), (OP_LOAD_0, 0), (OP_LOAD_3, 0), (OP_STORE_1, 0), (OP_STORE_4, 0),
            (OP_PUSH_I8, 1), (OP_PUSH_I16, 2), (OP_PUSH_I32, 4), (OP_LOAD, 1), (OP_STORE, 1),
            (OP_FRAME, 1), (OP_FRAME_A, 2), (OP_JMP, 2), (OP_JZ, 2), (OP_JNZ, 2), (OP_CALL, 2),
            (OP_JCMP, 3), (OP_JCMP_VI, 6), (OP_JCMP_VV, 5), (OP_BIN_I, 3), (OP_BIN_I32, 5),
            (OP_BIN_V, 2), (OP_ADD_VI, 3), (OP_STORE_OP, 2), (OP_RET, 0), (OP_YIELD, 0),
            (OP_HALT, 0), (0xEE, 0), (OP_SYSCALL, 2),
            // Quickened forms (QUICK), junk in their cond / op byte, slots of
            // either kind.
            (Q_FIRST + 2, 3), (Q_FIRST + 7, 6), (Q_FIRST + 15, 5), (Q_FIRST + 18, 3),
            (Q_FIRST + 22, 3), (Q_FIRST + 26, 3), (Q_FIRST + 27, 2), (Q_FIRST + 30, 2),
            (Q_FIRST + 31, 2), (Q_LOAD_L, 1), (Q_LOAD_G, 1), (Q_STORE_L, 1), (Q_STORE_G, 1),
            (Q_ADD_VI_L, 3), (Q_ARR_LOAD_V_L, 1), (Q_ARR_LOAD_V_G, 1),
            // Arrays: global 1 / local 1 hold one, other variables mostly not.
            (OP_ARR_LOAD, 0), (OP_ARR_LOAD_V, 1), (OP_ARR_LOAD_V, 1),
            // f32 on the stack's raw bits (NaNs and infinities included).
            (OP_ITOF, 0), (OP_FTOI, 0), (OP_FADD, 0), (OP_FSUB, 0), (OP_FMUL, 0),
            (OP_FDIV, 0), (OP_FNEG, 0), (OP_FEQ, 0), (OP_FLT, 0), (OP_FLE, 0),
            (OP_FGT, 0), (OP_FGE, 0), (OP_FNE, 0), (OP_FSIN, 0), (OP_FCOS, 0),
            (OP_FSQRT, 0), (OP_FABS, 0), (OP_FATAN2, 0), (OP_FFLOOR, 0), (OP_FCEIL, 0),
        ];
        // The quickened jumps (JCMP, JCMP_VI, JCMP_VV forms).
        let q_jump = |op: u8| (Q_FIRST..Q_FIRST + 18).contains(&op);
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for prog in 0..2000 {
            // Targets are u32 (`aw` bytes; OPS counts 2).
            let aw = 4;
            let len = 4 + rnd() as usize % 60;
            let mut code = alloc::vec::Vec::new();
            // Instruction starts, and where each jump target goes.
            let mut starts = alloc::vec::Vec::new();
            let mut targets = alloc::vec::Vec::new();
            while code.len() < len {
                let (op, nargs) = OPS[rnd() as usize % OPS.len()];
                starts.push(code.len());
                code.push(op);
                let jumpy = matches!(op, OP_JMP | OP_JZ | OP_JNZ | OP_CALL | OP_JCMP | OP_JCMP_VI | OP_JCMP_VV)
                    || q_jump(op);
                let nargs = if jumpy { nargs + aw - 2 } else { nargs };
                for k in 0..nargs {
                    let b = match (op, k) {
                        (OP_ARR_LOAD_V | Q_ARR_LOAD_V_L | Q_ARR_LOAD_V_G, 0) if rnd() % 4 != 0 => [1, 0x81][rnd() as usize % 2],
                        // Mostly valid conditions / operators, a few bad ones.
                        (OP_JCMP | OP_JCMP_VI | OP_JCMP_VV, 0) if rnd() % 8 != 0 => OP_EQ + (rnd() % 6) as u8,
                        (OP_BIN_I | OP_BIN_I32 | OP_BIN_V | OP_STORE_OP, 0) if rnd() % 8 != 0 => OP_ADD + (rnd() % 7) as u8,
                        (OP_FRAME_A, 1) => (rnd() % 4) as u8,
                        (OP_SYSCALL, 1) => (rnd() % 5) as u8, // argc
                        (OP_SYSCALL, 0) => (rnd() % 8) as u8, // id 7 fails
                        _ => rnd() as u8,
                    };
                    code.push(b);
                }
                if jumpy {
                    targets.push(code.len() - aw);
                }
            }
            // Jump to an instruction, now and then past the code or past
            // 64 K.
            for &at in &targets {
                let t = match rnd() % 16 {
                    0 => len + rnd() as usize % 300,
                    1 => 0x1_0000 + starts[rnd() as usize % starts.len()],
                    _ => starts[rnd() as usize % starts.len()],
                };
                code[at..at + 4].copy_from_slice(&(t as u32).to_le_bytes());
            }
            if rnd() % 4 == 0 {
                code.truncate(code.len() - 1); // an instruction cut off
            }
            for budget in [1u32, 3, 17, 200] {
                let (mut ca, mut cb) = (mock_ctx(), mock_ctx());
                let mut a = Vm::new();
                a.load_raw(code.clone());
                let mut b = Vm::new();
                b.load_raw(code.clone());
                // Some values on the stack and in the variables, so that
                // most programs run a while before they underflow.
                let fill: alloc::vec::Vec<i32> = (0..24).map(|_| rnd() as i32 % 50).collect();
                fn sys(id: u8, args: &[i32], _: &mut ObjPool) -> Option<i32> {
                    (id != 7).then(|| args.iter().fold(id as i32, |s, &x| s.wrapping_mul(31).wrapping_add(x)))
                }
                for v in [&mut a, &mut b] {
                    v.syscall_fn = Some(sys);
                    v.global_count = 8;
                    v.frame_base = 8;
                    v.state = VmState::Running;
                    v.stack[..12].copy_from_slice(&fill[..12]);
                    v.sp = 12;
                    for (i, &x) in fill[12..].iter().enumerate() {
                        v.vars[i * 11] = x;
                    }
                }
                // An array of 32 in global 1 and local 1 (the same id in
                // both pools), now and then on the stack as ARR_LOAD's.
                for (v, c) in [(&mut a, &mut ca), (&mut b, &mut cb)] {
                    let id = c.objs.arr_alloc(32).unwrap();
                    for (i, e) in c.objs.arr_mut(id).unwrap().iter_mut().enumerate() {
                        *e = i as i32 * 7 - 50;
                    }
                    v.vars[1] = id as i32;
                    v.vars[9] = id as i32;
                    v.stack[3] = id as i32;
                    v.stack[7] = id as i32;
                }
                for _ in 0..4 {
                    if a.state != VmState::Running {
                        break;
                    }
                    let na = burst_ref(&mut a, &mut ca, budget);
                    let nb = tc::run(&mut b, &mut cb, budget, 0, false) as u64;
                    let what = alloc::format!("program {prog} budget {budget}: {code:02x?}");
                    if a.state == VmState::Error && a.fault == fault::CODE_OOB {
                        // Perhaps an instruction cut off by the end of the code:
                        // `tc` runs it with operands from the pad, then faults
                        // (past the end, on the stack, or a burst later).
                        break;
                    }
                    assert!(a.state == b.state, "state, {what}");
                    if a.state == VmState::Error {
                        assert_eq!((a.fault, a.fault_pc), (b.fault, b.fault_pc), "fault, {what}");
                        break;
                    }
                    assert_eq!(na, nb, "instructions, {what}");
                    assert_eq!((a.pc, a.sp, a.call_sp), (b.pc, b.sp, b.call_sp), "pc sp call_sp, {what}");
                    assert_eq!((a.frame_base, a.frame_size, a.critical), (b.frame_base, b.frame_size, b.critical), "frame, {what}");
                    assert_eq!(&a.stack[..a.sp as usize], &b.stack[..b.sp as usize], "stack, {what}");
                    assert_eq!(&a.vars[..], &b.vars[..], "vars, {what}");
                    if a.state == VmState::Yielded {
                        a.state = VmState::Running;
                        b.state = VmState::Running;
                    }
                }
            }
        }
        // A critical section runs past the budget up to its YIELD.
        // g0 = 0; CRITICAL; loop: g0 += 1; if g0 < 40 goto loop; YIELD; g0 = 99
        let code = [
            OP_PUSH_0, OP_STORE, 0, OP_CRITICAL, // 0..4
            OP_ADD_VI, 0, 1, 0, OP_JCMP_VI, OP_LT, 0, 40, 0, 4, 0, 0, 0, // 4..17
            OP_YIELD, OP_PUSH_I8, 99, OP_STORE, 0, OP_HALT, // 17..23
        ];
        for budget in [1u32, 2, 5] {
            let (mut ca, mut cb) = (mock_ctx(), mock_ctx());
            let (mut a, mut b) = (Vm::new(), Vm::new());
            a.load_raw(code.to_vec());
            b.load_raw(code.to_vec());
            a.state = VmState::Running;
            b.state = VmState::Running;
            loop {
                let na = burst_ref(&mut a, &mut ca, budget);
                let nb = tc::run(&mut b, &mut cb, budget, 0, false) as u64;
                assert_eq!((na, a.pc, a.var_get(0)), (nb, b.pc, b.var_get(0)), "budget {budget}");
                assert!(a.state == b.state);
                match a.state {
                    VmState::Yielded => {
                        assert_eq!(a.var_get(0), 40);
                        a.state = VmState::Running;
                        b.state = VmState::Running;
                    }
                    VmState::Running => {}
                    _ => break,
                }
            }
            assert_eq!(b.var_get(0), 99);
        }
    }

    /// The app syscalls are the core's: threaded dispatch hands them to
    /// `Vm::op_syscall` instead of the platform's handler (which knows none
    /// of them), so `appCount()` in `loop()` doesn't fault.
    #[test]
    fn threaded_dispatch_answers_app_syscalls() {
        fn sys(_: u8, _: &[i32], _: &mut ObjPool) -> Option<i32> {
            None
        }
        // appCount(); appTask(""): store both, halt.
        let code = [
            OP_SYSCALL, crate::apps::SYS_APP_COUNT, 0, OP_STORE, 0, //
            OP_PUSH_0, OP_SYSCALL, crate::apps::SYS_APP_TASK, 1, OP_STORE, 1, OP_HALT,
        ];
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        vm.load_raw(code.to_vec());
        vm.syscall_fn = Some(sys);
        vm.global_count = 2;
        vm.frame_base = 2;
        vm.state = VmState::Running;
        tc::run(&mut vm, &mut ctx, 100, 0, false);
        assert!(vm.state == VmState::Halted, "fault {}", vm.fault);
        // The installed-app list is cached process-wide (other tests install).
        assert!(vm.var_get(0) >= 0 && vm.var_get(1) == -1);
    }

    #[test]
    fn callback_preempts_main_and_keeps_its_stack() {
        // main @0: 7 9 ADD -> g0, HALT     callback @7: arg -> g1, leaves junk, RET
        let code = [
            OP_PUSH_I8, 7, OP_PUSH_I8, 9, OP_ADD, OP_STORE, 0, // 0..7
            OP_STORE, 1, OP_PUSH_1, OP_PUSH_2, OP_PUSH_0, OP_RET, // 7..13
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 2); // main stops mid-expression: [7, 9]
        assert_eq!(vm.sp, 2);

        vm.enqueue_callback(7, &[5]);
        assert!(vm.drain_callbacks(&mut ctx));
        assert_eq!(vm.var_get(1), 5, "callback got its arg");
        assert!(vm.cb_frames.is_empty());
        assert_eq!(vm.sp, 2, "callback leftovers dropped");
        assert_eq!(&vm.stack[..2], &[7, 9], "interrupted values untouched");
        assert_eq!(vm.pc, 4);
        assert!(vm.state == VmState::Running);

        vm.tick(&mut ctx, 100);
        assert_eq!(vm.var_get(0), 16, "main resumes where it was");
    }

    #[test]
    fn callback_cannot_pop_interrupted_values() {
        // main @0: PUSH 7, HALT     callback @3: pops two values, gets one arg
        let code = [OP_PUSH_I8, 7, OP_HALT, OP_POP, OP_POP, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(3, &[1]);
        vm.drain_callbacks(&mut ctx);
        assert!(vm.state == VmState::Error);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 4);
        assert_eq!(vm.stack[0], 7);
    }

    #[test]
    fn delay_in_callback_suspends_without_blocking() {
        // main @0: HALT
        // A @1: delay(1000); g0 = 1; RET      B @9: g1 = 2; RET
        let code = [
            OP_HALT,
            OP_PUSH_I16, 0xE8, 0x03, OP_DELAY, OP_PUSH_1, OP_STORE, 0, OP_RET, // 1..9
            OP_PUSH_2, OP_STORE, 1, OP_RET, // 9..13
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        assert!(vm.state == VmState::Halted);

        vm.enqueue_callback(1, &[]);
        assert!(vm.drain_callbacks(&mut ctx), "returns instead of busy-waiting");
        assert!(vm.state == VmState::Waiting);
        assert_eq!(vm.cb_frames.len(), 1);
        assert_eq!(vm.var_get(0), 0);

        // B queues behind the suspended A (FIFO order is kept).
        vm.enqueue_callback(9, &[]);
        assert!(!vm.drain_callbacks(&mut ctx));
        assert_eq!(vm.var_get(1), 0);
        vm.tick(&mut ctx, 1); // mock clock stands at 0: still waiting
        assert!(vm.state == VmState::Waiting);

        vm.wait_until = 0; // the delay expires
        vm.tick(&mut ctx, 1);
        assert_eq!(vm.var_get(0), 1, "A finished after its delay");
        assert!(vm.cb_frames.is_empty());
        assert!(vm.state == VmState::Halted, "main state restored");

        assert!(vm.drain_callbacks(&mut ctx));
        assert_eq!(vm.var_get(1), 2);
    }

    #[test]
    fn yield_in_callback_continues_on_next_tick() {
        // main @0: HALT     callback @1: g0 = 1; YIELD; g1 = 2; RET
        let code = [OP_HALT, OP_PUSH_1, OP_STORE, 0, OP_YIELD, OP_PUSH_2, OP_STORE, 1, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(1, &[]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.var_get(0), 1);
        assert!(vm.state == VmState::Yielded);
        assert_eq!(vm.cb_frames.len(), 1);

        vm.tick(&mut ctx, 1);
        assert_eq!(vm.var_get(1), 2, "code after yield runs");
        assert!(vm.cb_frames.is_empty());
        assert!(vm.state == VmState::Halted);
    }

    #[test]
    fn main_wait_survives_a_callback() {
        // main @0: delay(500); HALT     callback @5: g0 = 1; RET
        let code = [OP_PUSH_I16, 0xF4, 0x01, OP_DELAY, OP_HALT, OP_PUSH_1, OP_STORE, 0, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 10);
        assert!(vm.state == VmState::Waiting);
        vm.enqueue_callback(5, &[]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.var_get(0), 1);
        assert!(vm.state == VmState::Waiting);
        assert_eq!(vm.wait_until, 500);
    }

    #[test]
    fn wait_left_counts_down_to_the_delay_end() {
        // main @0: delay(500); HALT
        let code = [OP_PUSH_I16, 0xF4, 0x01, OP_DELAY, OP_HALT];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        assert_eq!(vm.wait_left(0), None, "not waiting yet");
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.wait_left(0), Some(500));
        assert_eq!(vm.wait_left(499), Some(1));
        assert_eq!(vm.wait_left(500), Some(0));
        assert_eq!(vm.wait_left(700), Some(0), "past the end: due now");
        assert!(!vm.animating());
    }

    #[test]
    fn halt_in_callback_ends_only_the_callback() {
        // main @0: g0 = 3; HALT     callback @5: g1 = 1; HALT
        let code = [OP_PUSH_I8, 3, OP_STORE, 0, OP_HALT, OP_PUSH_1, OP_STORE, 1, OP_HALT];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(5, &[]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.var_get(1), 1);
        assert!(vm.state == VmState::Running);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.var_get(0), 3);
    }

    #[test]
    fn modal_in_callback_lets_dialog_callbacks_run() {
        // builder fn 1 @0: return overlay id 1
        // A @2: g0 = showModal(1, 0); RET
        // B @8: setDialogResult(42); RET
        let code = [
            OP_PUSH_1, OP_RET, // 0..2
            OP_PUSH_1, OP_PUSH_0, OP_SHOW_MODAL, OP_STORE, 0, OP_RET, // 2..8
            OP_PUSH_I8, 42, OP_SET_DIALOG_RESULT, OP_RET, // 8..12
        ];
        let mut ctx = mock_ctx();
        let root = one_widget(&mut ctx);
        let overlay = one_widget(&mut ctx);
        assert_eq!((root.0, overlay.0), (0, 1));
        let mut vm = cb_vm(&code);
        vm.functions.push(FuncEntry { func_id: 1, kind: FunctionKind::UserFunction, offset: 0, length: 2 });
        vm.state = VmState::Halted;

        vm.enqueue_callback(2, &[]);
        vm.drain_callbacks(&mut ctx);
        assert!(vm.state == VmState::AwaitingModal);
        assert_eq!(vm.cb_frames.len(), 1, "A stays suspended on its frame");

        vm.enqueue_callback(8, &[]);
        assert!(vm.drain_callbacks(&mut ctx), "dialog callback runs on top of A");
        assert!(vm.state == VmState::AwaitingModal);
        assert_eq!(vm.cb_frames.len(), 1);

        vm.try_resume_modal(&mut ctx);
        assert!(!ctx.objs.is_live(overlay));
        vm.tick(&mut ctx, 1);
        assert_eq!(vm.var_get(0), 42, "A sees the dialog result");
        assert!(vm.cb_frames.is_empty());
        assert!(vm.state == VmState::Halted);
        assert!(!vm.modal_active());
    }

    #[test]
    fn call_now_runs_through_yield_and_nests_over_a_suspended_callback() {
        // main @0: HALT    A @1: delay(1000); RET    S @6: g0 = 1; YIELD; g1 = 2; RET
        let code = [
            OP_HALT,
            OP_PUSH_I16, 0xE8, 0x03, OP_DELAY, OP_RET, // 1..6
            OP_PUSH_1, OP_STORE, 0, OP_YIELD, OP_PUSH_2, OP_STORE, 1, OP_RET, // 6..14
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(1, &[]);
        vm.drain_callbacks(&mut ctx);
        assert!(vm.state == VmState::Waiting);

        vm.call_now(6, &[], &mut ctx); // e.g. on_paint during A's delay
        assert_eq!((vm.var_get(0), vm.var_get(1)), (1, 2));
        assert_eq!(vm.cb_frames.len(), 1);
        assert!(vm.state == VmState::Waiting, "A is still waiting");
        assert_eq!(vm.wait_until, 1000);
    }

    // --- Programs ---

    /// A filesystem at 0x1000 with programs `main` and `settings` and an image.
    fn ctx_with_programs() -> Ctx<MockPlatform> {
        const BASE: u32 = 0x1000;
        let names: [(&[u8], u8); 3] = [
            (b"main", crate::fs::RES_PROGRAM),
            (b"pic", crate::fs::RES_IMAGE),
            (b"settings", crate::fs::RES_PROGRAM),
        ];
        let mut img = alloc::vec![0u8; BASE as usize];
        let mut hdr = [0u8; 16];
        hdr[0..4].copy_from_slice(b"FERR");
        hdr[4..6].copy_from_slice(&3u16.to_le_bytes());
        hdr[10..12].copy_from_slice(&(names.len() as u16).to_le_bytes());
        img.extend_from_slice(&hdr);
        for (name, kind) in names {
            let mut e = [0u8; 32];
            e[..name.len()].copy_from_slice(name);
            e[16] = kind;
            img.extend_from_slice(&e);
        }
        crate::mock::set_flash_image(img);
        let mut ctx = mock_ctx();
        ctx.fs = crate::fs::Fs::mount(&ctx.flash, BASE).ok();
        assert!(ctx.fs.is_some());
        ctx
    }

    #[test]
    fn program_name_lists_programs_and_its_own() {
        // g0 = programName(0); g1 = programName(1); g2 = programName(2);
        // g3 = programName(-1); then programArg() stays on the stack
        let code = [
            OP_PUSH_0, OP_PROGRAM_NAME, OP_STORE, 0,
            OP_PUSH_1, OP_PROGRAM_NAME, OP_STORE, 1,
            OP_PUSH_2, OP_PROGRAM_NAME, OP_STORE, 2,
            OP_PUSH_I8, 0xFF, OP_PROGRAM_NAME, OP_STORE, 3,
            OP_PROGRAM_ARG,
            OP_HALT,
        ];
        let mut ctx = ctx_with_programs();
        let mut vm = cb_vm(&code);
        vm.program = ProgramRef::new(b"settings", 42);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Halted, "fault {}", vm.fault());
        let s = |vm: &Vm, slot| ctx.objs.str_get(vm.var_get(slot) as ObjPtr).to_vec();
        assert_eq!(s(&vm, 0), b"main");
        assert_eq!(s(&vm, 1), b"settings", "only programs count, the image is skipped");
        assert_eq!(s(&vm, 2), b"", "past the last program");
        assert_eq!(s(&vm, 3), b"settings", "-1 = the running program");
        assert_eq!(vm.pop_result(), 42);
    }

    #[test]
    fn run_program_exits_at_once_or_returns_0() {
        let mut ctx = ctx_with_programs();
        let nope = ctx.objs.str_alloc(b"nope").unwrap() as u8;
        let settings = ctx.objs.str_alloc(b"settings").unwrap() as u8;
        // g0 = runProgram("nope", 3); runProgram("settings", 7); g1 = 1 (never runs)
        let code = [
            OP_PUSH_I8, nope, OP_PUSH_I8, 3, OP_RUN_PROGRAM, OP_STORE, 0,
            OP_PUSH_I8, settings, OP_PUSH_I8, 7, OP_RUN_PROGRAM,
            OP_PUSH_1, OP_STORE, 1,
            OP_HALT,
        ];
        let mut vm = cb_vm(&code);
        vm.var_set(1, 0);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Exit, "fault {}", vm.fault());
        assert_eq!(vm.var_get(0), 0, "a missing program is reported, nothing switches");
        assert_eq!(vm.var_get(1), 0, "nothing after a successful runProgram runs");
        let next = vm.next_program.expect("switch requested");
        assert_eq!(next.name(), b"settings");
        assert_eq!(next.arg, 7);
        // Nothing runs any more: no tick, no callbacks.
        vm.enqueue_callback(0, &[]);
        assert!(!vm.drain_callbacks(&mut ctx));
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Exit);
    }

    #[test]
    fn run_program_inside_a_callback_ends_the_main_program_too() {
        let mut ctx = ctx_with_programs();
        let settings = ctx.objs.str_alloc(b"settings").unwrap() as u8;
        // main @0: loop forever (JMP 0)   callback @5: runProgram("settings", 0); RET
        let code = [
            OP_JMP, 0, 0, 0, 0,
            OP_PUSH_I8, settings, OP_PUSH_0, OP_RUN_PROGRAM, OP_RET,
        ];
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 10);
        vm.enqueue_callback(5, &[]);
        assert!(vm.drain_callbacks(&mut ctx));
        assert!(vm.state == VmState::Exit, "fault {}", vm.fault());
        vm.tick(&mut ctx, 10);
        assert!(vm.state == VmState::Exit, "the interrupted main program stays ended");
        vm.reset();
        assert!(vm.next_program.is_none() && vm.state == VmState::Ready);
    }

    // --- User GPIO ---

    #[test]
    fn gpio_ops_drive_and_read_pins() {
        // pinMode(1, OUTPUT); pinWrite(1, 5); g0 = pinRead(1);
        // pinMode(2, INPUT_PULLUP); g1 = pinRead(2); HALT
        let code = [
            OP_PUSH_1, OP_PUSH_1, OP_PIN_MODE,
            OP_PUSH_1, OP_PUSH_I8, 5, OP_PIN_WRITE,
            OP_PUSH_1, OP_PIN_READ, OP_STORE, 0,
            OP_PUSH_2, OP_PUSH_2, OP_PIN_MODE,
            OP_PUSH_2, OP_PIN_READ, OP_STORE, 1,
            OP_HALT,
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Halted);
        assert_eq!(vm.var_get(0), 1, "non-zero writes drive high");
        assert_eq!(vm.var_get(1), 1, "pull-up input reads high");
    }

    #[test]
    fn gpio_faults_on_a_missing_pin_or_mode() {
        let mut ctx = mock_ctx();
        // MockGpio has 4 pins: pinRead(4)
        let mut vm = cb_vm(&[OP_PUSH_I8, 4, OP_PIN_READ, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::GPIO);
        assert_eq!(vm.fault_pc(), 2);
        // pinMode(0, INPUT_PULLDOWN): unsupported on this board
        let mut vm = cb_vm(&[OP_PUSH_0, OP_PUSH_I8, 3, OP_PIN_MODE, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::GPIO);
        // pinWrite(-1, 1)
        let mut vm = cb_vm(&[OP_PUSH_I8, 0xFF, OP_PUSH_1, OP_PIN_WRITE, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::GPIO);
    }

    #[test]
    fn eeprom_byte_ops() {
        // g0 = eepromSize(); eepromWrite(5, 0x1AB); g1 = eepromRead(5); HALT
        let code = [
            OP_EEPROM_SIZE, OP_STORE, 0,
            OP_PUSH_I8, 5, OP_PUSH_I16, 0xAB, 0x01, OP_EEPROM_WRITE,
            OP_PUSH_I8, 5, OP_EEPROM_READ, OP_STORE, 1,
            OP_HALT,
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Halted);
        assert_eq!(vm.var_get(0), 64, "MockEeprom is 64 bytes");
        assert_eq!(vm.var_get(1), 0xAB, "only the low byte is stored");
    }

    #[test]
    fn eeprom_array_round_trip_across_chunks() {
        // g2 = [100..120); eepromWriteArr(30, g2, 20);
        // g3 = new arr[20]; eepromReadArr(30, g3, 20); HALT
        let mut code = alloc::vec![OP_ARR_INIT, 20];
        for v in 100i32..120 {
            code.extend_from_slice(&v.to_le_bytes());
        }
        code.extend_from_slice(&[
            OP_STORE, 2,
            OP_PUSH_I8, 30, OP_LOAD, 2, OP_PUSH_I8, 20, OP_EEPROM_WRITE_ARR,
            OP_ARR_ALLOC, 20, OP_STORE, 3,
            OP_PUSH_I8, 30, OP_LOAD, 3, OP_PUSH_I8, 20, OP_EEPROM_READ_ARR,
            OP_HALT,
        ]);
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Halted, "fault {}", vm.fault());
        let want: Vec<i32> = (100..120).collect();
        assert_eq!(ctx.objs.arr(vm.var_get(3) as ObjPtr), Some(&want[..]));
    }

    #[test]
    fn arrays_over_255_elements_take_their_size_from_the_stack() {
        // a = ARR_ALLOC_S(600); n = len(a); a[599] = 7; v = a[599]; HALT
        let [lo, hi] = 600i16.to_le_bytes();
        let code = [
            OP_PUSH_I16, lo, hi, OP_ARR_ALLOC_S, OP_STORE, 0,
            OP_LOAD, 0, OP_ARR_LEN, OP_STORE, 1,
            OP_LOAD, 0, OP_PUSH_I16, 0x57, 0x02, OP_PUSH_I8, 7, OP_ARR_STORE,
            OP_LOAD, 0, OP_PUSH_I16, 0x57, 0x02, OP_ARR_LOAD, OP_STORE, 2,
            OP_HALT,
        ];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Halted, "fault {}", vm.fault());
        assert_eq!(vm.var_get(1), 600);
        assert_eq!(vm.var_get(2), 7);

        // A negative size faults instead of allocating something.
        let code = [OP_PUSH_I8, 0xFF, OP_ARR_ALLOC_S, OP_HALT];
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 100);
        assert_eq!(vm.fault(), fault::ARR_ALLOC);
    }

    #[test]
    fn storage_files_and_directories() {
        use disk::*;
        let mut ctx = mock_ctx();
        let s = |ctx: &mut Ctx<MockPlatform>, t: &[u8]| ctx.objs.str_alloc(t).unwrap() as u8;
        let (logs, file, hello) = (s(&mut ctx, b"logs"), s(&mut ctx, b"/Logs/a.txt"), s(&mut ctx, b"hello"));
        let code = [
            OP_PUSH_I8, logs, OP_DISK_MKDIR, OP_STORE, 0,
            OP_PUSH_I8, file, OP_PUSH_1, OP_DISK_OPEN, OP_STORE, 1,
            OP_LOAD, 1, OP_PUSH_I8, hello, OP_FILE_WRITE_STR, OP_STORE, 2,
            OP_LOAD, 1, OP_PUSH_I8, b'!', OP_FILE_WRITE, OP_STORE, 3,
            OP_LOAD, 1, OP_FILE_CLOSE,
            OP_PUSH_I8, file, OP_PUSH_0, OP_DISK_OPEN, OP_STORE, 4,
            OP_LOAD, 4, OP_FILE_SIZE, OP_STORE, 5,
            OP_LOAD, 4, OP_FILE_READ, OP_STORE, 6,
            OP_ARR_ALLOC, 10, OP_STORE, 7,
            OP_LOAD, 4, OP_LOAD, 7, OP_PUSH_I8, 10, OP_FILE_READ_ARR, OP_STORE, 8,
            OP_LOAD, 4, OP_PUSH_I8, b'x', OP_FILE_WRITE, OP_STORE, 9,
            OP_LOAD, 4, OP_FILE_CLOSE,
            OP_PUSH_I8, logs, OP_PUSH_0, OP_DISK_LIST, OP_STORE, 10,
            OP_PUSH_I8, file, OP_DISK_EXISTS, OP_STORE, 11,
            OP_PUSH_I8, logs, OP_DISK_RMDIR, OP_STORE, 12,
            OP_PUSH_I8, file, OP_DISK_DELETE, OP_STORE, 13,
            OP_PUSH_I8, logs, OP_DISK_RMDIR, OP_STORE, 14,
            OP_PUSH_I8, logs, OP_DISK_EXISTS, OP_STORE, 15,
            OP_HALT,
        ];
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 200);
        assert!(vm.state == VmState::Halted, "fault {}", vm.fault());
        use crate::storage::{HANDLE_BASE, err};
        let g: Vec<i32> = (0..16).map(|i| vm.var_get(i)).collect();
        assert_eq!(g[0], 0, "mkdir");
        assert_eq!(g[1], HANDLE_BASE);
        assert_eq!((g[2], g[3]), (5, 1), "bytes written");
        assert_eq!(g[4], HANDLE_BASE, "the slot was freed by close");
        assert_eq!((g[5], g[6]), (6, b'h' as i32));
        assert_eq!(g[8], 5, "the rest of the file");
        assert_eq!(ctx.objs.arr(g[7] as ObjPtr).unwrap()[..5], [b'e', b'l', b'l', b'o', b'!'].map(i32::from));
        assert_eq!(g[9], err::BAD_PATH, "opened for reading");
        assert_eq!(ctx.objs.str_get(g[10] as ObjPtr), b"A.TXT");
        assert_eq!(g[11], 1, "a file");
        assert_eq!(g[12], err::NOT_EMPTY);
        assert_eq!((g[13], g[14], g[15]), (0, 0, 0));
    }

    #[test]
    fn storage_handles_fault_when_not_open_or_not_the_tasks() {
        use disk::*;
        let mut ctx = mock_ctx();
        let name = ctx.objs.str_alloc(b"a.bin").unwrap() as u8;
        // fileWrite(1, 0): a resource file's handle, not open
        let mut vm = cb_vm(&[OP_PUSH_1, OP_PUSH_0, OP_FILE_WRITE, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::BAD_FILE);
        // Task 3 opens a file; main (task 0) may not use its handle.
        ctx.objs.cur_task = 3;
        let mut vm = cb_vm(&[OP_PUSH_I8, name, OP_PUSH_1, OP_DISK_OPEN, OP_STORE, 0, OP_HALT]);
        vm.tick(&mut ctx, 10);
        let h = vm.var_get(0);
        assert_eq!(h, crate::storage::HANDLE_BASE);
        ctx.objs.cur_task = 0;
        let mut vm = cb_vm(&[OP_PUSH_I8, h as u8, OP_FILE_SIZE, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::BAD_FILE);
        ctx.storage.close_task(3);
        assert_eq!(ctx.storage.open(0, b"a.bin", 0), crate::storage::HANDLE_BASE);
    }

    #[test]
    fn str_byte_reads_bytes_and_minus_one_past_the_end() {
        let mut ctx = mock_ctx();
        let id = ctx.objs.str_alloc(b"Hi").unwrap() as i8;
        // g0 = strByte(s, 1); g1 = strByte(s, 2); g2 = strByte(s, -1)
        let code = [
            OP_PUSH_I8, id as u8, OP_PUSH_1, OP_STR_BYTE, OP_STORE, 0,
            OP_PUSH_I8, id as u8, OP_PUSH_2, OP_STR_BYTE, OP_STORE, 1,
            OP_PUSH_I8, id as u8, OP_PUSH_I8, 0xFF, OP_STR_BYTE, OP_STORE, 2,
            OP_HALT,
        ];
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 100);
        assert!(vm.state == VmState::Halted);
        assert_eq!(vm.var_get(0), b'i' as i32);
        assert_eq!(vm.var_get(1), -1);
        assert_eq!(vm.var_get(2), -1);
    }

    #[test]
    fn eeprom_faults() {
        let mut ctx = mock_ctx();
        // eepromRead(64): past the end of the 64-byte EEPROM
        let mut vm = cb_vm(&[OP_PUSH_I8, 64, OP_EEPROM_READ, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::EEPROM);
        // eepromReadArr(0, arr[4], 5): count longer than the array
        let mut vm = cb_vm(&[
            OP_PUSH_0, OP_ARR_ALLOC, 4, OP_PUSH_I8, 5, OP_EEPROM_READ_ARR, OP_HALT,
        ]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::BAD_OPERAND);
        // eepromWriteArr(62, arr[4], 4): range runs past the end
        let mut vm = cb_vm(&[
            OP_PUSH_I8, 62, OP_ARR_ALLOC, 4, OP_PUSH_I8, 4, OP_EEPROM_WRITE_ARR, OP_HALT,
        ]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::EEPROM);
        // eepromReadArr(0, 99, 1): no such array
        let mut vm = cb_vm(&[OP_PUSH_0, OP_PUSH_I8, 99, OP_PUSH_1, OP_EEPROM_READ_ARR, OP_HALT]);
        vm.tick(&mut ctx, 10);
        assert_eq!(vm.fault(), fault::BAD_ARRAY);
    }

    #[test]
    fn nesting_is_capped() {
        let mut vm = cb_vm(&[OP_HALT]);
        for _ in 0..CB_NEST_MAX {
            assert!(vm.enter_callback(0, &[]));
        }
        assert!(!vm.enter_callback(0, &[]));
        assert_eq!(vm.cb_frames.len(), CB_NEST_MAX);
    }

    #[test]
    fn start_main_waits_for_a_suspended_setup() {
        let mut vm = cb_vm(&[OP_HALT]);
        vm.state = VmState::Ready;
        assert!(vm.enter_callback(0, &[]));
        vm.state = VmState::AwaitingModal;
        vm.start_main(9);
        assert!(vm.state == VmState::AwaitingModal);
        vm.leave_callback();
        assert_eq!(vm.pc, 9);
        assert!(vm.state == VmState::Running);
    }

    #[test]
    fn configured_cb_queue_capacity() {
        let mut vm = Vm::new();
        assert!(vm.configure(0, 0, 3, 0));
        for i in 0..5 {
            vm.enqueue_callback(i, &[]);
        }
        // A ring of N slots holds N-1 entries.
        let mut n = 0;
        while vm.cb_head != vm.cb_tail {
            vm.cb_head = (vm.cb_head + 1) % vm.cb_queue.len() as u8;
            n += 1;
        }
        assert_eq!(n, 2);
    }

    fn run_code(code: &[u8]) -> Vm {
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        vm.code = VmCode::new(code.to_vec());
        vm.run(&mut ctx);
        vm
    }

    #[test]
    fn fault_reports_reason_and_instruction_pc() {
        // DUP, POP, POP → underflow raised by the instruction at pc 2.
        let vm = run_code(&[OP_DUP, OP_POP, OP_POP]);
        assert!(vm.state == VmState::Error);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 2);
    }

    /// FRAME_A against FRAME plus a STORE per argument (last argument on top).
    #[test]
    fn frame_a_matches_plain_prologue() {
        for nargs in 0..=4u8 {
            let mut args = std::vec::Vec::new();
            for i in 0..nargs {
                args.extend_from_slice(&[OP_PUSH_I8, 10 + i]);
            }
            let mut plain = args.clone();
            plain.extend_from_slice(&[OP_FRAME, 6]);
            for i in (0..nargs).rev() {
                plain.push(OP_STORE_0 + i);
            }
            let mut fused = args.clone();
            fused.extend_from_slice(&[OP_FRAME_A, 6, nargs]);
            let tail = [OP_LOAD_0, OP_LOAD_1, OP_LOAD_2, OP_LOAD_3, OP_HALT];
            plain.extend_from_slice(&tail);
            fused.extend_from_slice(&tail);
            let (p, f) = (run_code(&plain), run_code(&fused));
            assert!(f.state == VmState::Halted, "nargs {nargs}");
            assert_eq!((f.sp, f.frame_size), (p.sp, p.frame_size), "nargs {nargs}");
            assert_eq!(f.stack[..f.sp as usize], p.stack[..p.sp as usize], "nargs {nargs}");
        }
        // Too few arguments on the stack: a fault, as with the STOREs.
        let vm = run_code(&[OP_PUSH_1, OP_FRAME_A, 2, 2, OP_HALT]);
        assert!(vm.state == VmState::Error);
    }

    /// Each superinstruction against the plain sequence it replaces, over
    /// operand values around the edges (overflow, division by 0 and -1).
    #[test]
    fn superops_match_plain_sequences() {
        const VALS: [i32; 9] = [0, 1, -1, 7, -13, 255, 32767, i32::MAX, i32::MIN];
        const IMMS: [i16; 7] = [0, 1, -1, 3, -7, i16::MAX, i16::MIN];
        const OPS: [u8; 15] = [
            OP_ADD, OP_SUB, OP_MUL, OP_DIV, OP_MOD, OP_AND, OP_OR, OP_SHL, OP_SHR, OP_EQ, OP_NE, OP_LT,
            OP_LE, OP_GT, OP_GE,
        ];
        fn push32(c: &mut std::vec::Vec<u8>, v: i32) {
            c.push(OP_PUSH_I32);
            c.extend_from_slice(&v.to_le_bytes());
        }
        // Result of a program that leaves one value on the stack (or the
        // value of global 1 when `var`).
        fn result(code: &[u8], var: bool) -> i32 {
            let mut c = code.to_vec();
            c.push(OP_HALT);
            let vm = run_code(&c);
            assert!(vm.state == VmState::Halted, "{code:x?}");
            if var { vm.var_get(1) } else { vm.stack[vm.sp as usize - 1] }
        }
        for &a in &VALS {
            for &op in &OPS {
                for &imm in &IMMS {
                    let b = imm as i32;
                    // BIN_I vs PUSH a; PUSH b; op
                    let mut plain = std::vec::Vec::new();
                    push32(&mut plain, a);
                    push32(&mut plain, b);
                    plain.push(op);
                    let mut fused = std::vec::Vec::new();
                    push32(&mut fused, a);
                    fused.extend_from_slice(&[OP_BIN_I, op]);
                    fused.extend_from_slice(&imm.to_le_bytes());
                    let want = result(&plain, false);
                    assert_eq!(result(&fused, false), want, "BIN_I {op:#x} {a} {b}");
                    // BIN_V: global 1 = b
                    let mut fused = std::vec::Vec::new();
                    push32(&mut fused, b);
                    fused.extend_from_slice(&[OP_STORE, 1]);
                    push32(&mut fused, a);
                    fused.extend_from_slice(&[OP_BIN_V, op, 1]);
                    assert_eq!(result(&fused, false), want, "BIN_V {op:#x} {a} {b}");
                    // STORE_OP: global 1 = a; push b; var = var op b
                    let mut fused = std::vec::Vec::new();
                    push32(&mut fused, a);
                    fused.extend_from_slice(&[OP_STORE, 1]);
                    push32(&mut fused, b);
                    fused.extend_from_slice(&[OP_STORE_OP, op, 1]);
                    assert_eq!(result(&fused, true), want, "STORE_OP {op:#x} {a} {b}");
                }
                for &b in &VALS {
                    let mut plain = std::vec::Vec::new();
                    push32(&mut plain, a);
                    push32(&mut plain, b);
                    plain.push(op);
                    let mut fused = std::vec::Vec::new();
                    push32(&mut fused, a);
                    fused.extend_from_slice(&[OP_BIN_I32, op]);
                    fused.extend_from_slice(&b.to_le_bytes());
                    assert_eq!(result(&fused, false), result(&plain, false), "BIN_I32 {op:#x} {a} {b}");
                }
            }
            // ADD_VI vs LOAD; PUSH; ADD; STORE (global 1)
            for &imm in &IMMS {
                let mut c = std::vec::Vec::new();
                push32(&mut c, a);
                c.extend_from_slice(&[OP_STORE, 1, OP_ADD_VI, 1]);
                c.extend_from_slice(&imm.to_le_bytes());
                assert_eq!(result(&c, true), a.wrapping_add(imm as i32), "ADD_VI {a} {imm}");
            }
        }
        // Compare-and-branch: the jump lands on PUSH_1, else PUSH_0.
        // layout: <setup> JCMP.. @T ; PUSH_0 ; HALT ; T: PUSH_1 ; HALT
        fn branch(setup: &[u8], jcmp: &[u8]) -> i32 {
            let mut c = setup.to_vec();
            c.extend_from_slice(jcmp);
            let t = (c.len() + 4 + 2) as u32;
            c.extend_from_slice(&t.to_le_bytes());
            c.extend_from_slice(&[OP_PUSH_0, OP_HALT, OP_PUSH_1, OP_HALT]);
            let vm = run_code(&c);
            assert!(vm.state == VmState::Halted);
            vm.stack[vm.sp as usize - 1]
        }
        for &a in &VALS {
            for cond in OP_EQ..=OP_GE {
                for &b in &VALS {
                    let want = int_cmp(cond, a, b).unwrap() as i32;
                    let mut s = std::vec::Vec::new();
                    push32(&mut s, a);
                    push32(&mut s, b);
                    assert_eq!(branch(&s, &[OP_JCMP, cond]), want, "JCMP {cond:#x} {a} {b}");
                    let mut s = std::vec::Vec::new();
                    push32(&mut s, a);
                    s.extend_from_slice(&[OP_STORE, 1]);
                    push32(&mut s, b);
                    s.extend_from_slice(&[OP_STORE, 2]);
                    assert_eq!(branch(&s, &[OP_JCMP_VV, cond, 1, 2]), want, "JCMP_VV");
                }
                for &imm in &IMMS {
                    let want = int_cmp(cond, a, imm as i32).unwrap() as i32;
                    let mut s = std::vec::Vec::new();
                    push32(&mut s, a);
                    s.extend_from_slice(&[OP_STORE, 1]);
                    let mut j = std::vec![OP_JCMP_VI, cond, 1];
                    j.extend_from_slice(&imm.to_le_bytes());
                    assert_eq!(branch(&s, &j), want, "JCMP_VI {cond:#x} {a} {imm}");
                }
            }
        }
        // A sub-op that is not a binary op faults.
        let vm = run_code(&[OP_PUSH_1, OP_BIN_I, OP_POP, 0, 0]);
        assert_eq!(vm.fault(), fault::BAD_OPCODE);
        let vm = run_code(&[OP_PUSH_1, OP_PUSH_1, OP_JCMP, OP_ADD, 0, 0, 0, 0]);
        assert_eq!(vm.fault(), fault::BAD_OPCODE);
        // BIN_I on an empty stack underflows.
        let vm = run_code(&[OP_BIN_I, OP_ADD, 1, 0]);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
    }

    /// Dispatch speed on the host (`cargo test --release ... vm_dispatch_bench
    /// -- --ignored --nocapture`): bench mode 0's loop, plain and fused.
    #[test]
    #[ignore]
    fn vm_dispatch_bench() {
        const N: i16 = 20000;
        let [nl, nh] = N.to_le_bytes();
        // i = g1, acc = g2. Plain: while (i < N) { acc = acc + (i*7)%13; i = i + 1; }
        let mut plain = std::vec![OP_PUSH_0, OP_STORE, 1, OP_PUSH_0, OP_STORE, 2];
        let top = plain.len() as u32;
        plain.extend_from_slice(&[OP_LOAD, 1, OP_PUSH_I16, nl, nh, OP_LT, OP_JZ]);
        let exit_at = plain.len();
        plain.extend_from_slice(&[0, 0, 0, 0]);
        plain.extend_from_slice(&[
            OP_LOAD, 2, OP_LOAD, 1, OP_PUSH_I8, 7, OP_MUL, OP_PUSH_I8, 13, OP_MOD, OP_ADD, OP_STORE, 2,
            OP_LOAD, 1, OP_PUSH_1, OP_ADD, OP_STORE, 1, OP_JMP,
        ]);
        plain.extend_from_slice(&top.to_le_bytes());
        let end = plain.len() as u32;
        plain[exit_at..exit_at + 4].copy_from_slice(&end.to_le_bytes());
        plain.push(OP_HALT);
        // Fused, inverted: JMP test; body: ...; test: JCMP_VI i < N body
        let mut fused = std::vec![OP_PUSH_0, OP_STORE, 1, OP_PUSH_0, OP_STORE, 2, OP_JMP, 0, 0, 0, 0];
        let body = fused.len() as u32;
        fused.extend_from_slice(&[
            OP_LOAD, 1, OP_BIN_I, OP_MUL, 7, 0, OP_BIN_I, OP_MOD, 13, 0, OP_STORE_OP, OP_ADD, 2,
            OP_ADD_VI, 1, 1, 0,
        ]);
        let test = fused.len() as u32;
        fused[7..11].copy_from_slice(&test.to_le_bytes());
        fused.extend_from_slice(&[OP_JCMP_VI, OP_LT, 1, nl, nh]);
        fused.extend_from_slice(&body.to_le_bytes());
        fused.push(OP_HALT);
        for (name, code) in [("plain", &plain), ("fused", &fused)] {
            let mut ctx = mock_ctx();
            let mut vm = Vm::new();
            vm.code = VmCode::new(code.clone());
            let runs = 200;
            let t0 = std::time::Instant::now();
            let mut acc = 0;
            for _ in 0..runs {
                vm.pc = 0;
                vm.sp = 0;
                vm.state = VmState::Running;
                while vm.state == VmState::Running {
                    vm.tick(&mut ctx, 1 << 30);
                }
                assert!(vm.state == VmState::Halted);
                acc = vm.var_get(2);
            }
            let dt = t0.elapsed().as_secs_f64();
            let iters = runs as f64 * N as f64;
            std::println!("{name}: {:.1} M iterations/s ({:.2} ns/iter), acc {acc}", iters / dt / 1e6, dt / iters * 1e9);
        }
    }


    /// Each quickened form (`QUICK`) against its instruction, in single
    /// steps and in bursts, with a junk cond / op operand and the slot
    /// operand's kind bit flipped (both ignored by the form); running the
    /// instruction rewrites it to the form.
    #[test]
    fn quickened_superops_match() {
        const VALS: [i32; 6] = [0, 1, -1, 7, -13, i32::MIN];
        fn push32(c: &mut std::vec::Vec<u8>, v: i32) {
            c.push(OP_PUSH_I32);
            c.extend_from_slice(&v.to_le_bytes());
        }
        // (code, position of the instruction's opcode, position of its
        // forced slot operand). Leaves the result on the stack. Variable 1
        // is global 1 and local 1 (frame_base and gbase 0); `slot` is the
        // form's slot operand for it.
        fn program(op: u8, sub: u8, slot: u8, a: i32, b: i32) -> (std::vec::Vec<u8>, usize, Option<usize>) {
            let mut c = std::vec::Vec::new();
            let at;
            let mut slot_at = None;
            match op {
                OP_JCMP | OP_JCMP_VI | OP_JCMP_VV => {
                    push32(&mut c, a);
                    c.extend_from_slice(&[OP_STORE, 1]);
                    push32(&mut c, b);
                    c.extend_from_slice(&[OP_STORE, 2]);
                    if op == OP_JCMP {
                        c.extend_from_slice(&[OP_LOAD, 1, OP_LOAD, 2]);
                    }
                    at = c.len();
                    match op {
                        OP_JCMP => c.extend_from_slice(&[op, sub]),
                        OP_JCMP_VI => {
                            slot_at = Some(at + 2);
                            c.extend_from_slice(&[op, sub, slot]);
                            c.extend_from_slice(&(b as i16).to_le_bytes());
                        }
                        _ => c.extend_from_slice(&[op, sub, 1, 2]),
                    }
                    let t = (c.len() + 4 + 2) as u32;
                    c.extend_from_slice(&t.to_le_bytes());
                    c.extend_from_slice(&[OP_PUSH_0, OP_HALT, OP_PUSH_1, OP_HALT]);
                }
                OP_BIN_I => {
                    push32(&mut c, a);
                    at = c.len();
                    c.extend_from_slice(&[op, sub]);
                    c.extend_from_slice(&(b as i16).to_le_bytes());
                    c.push(OP_HALT);
                }
                OP_BIN_V => {
                    push32(&mut c, b);
                    c.extend_from_slice(&[OP_STORE, 1]);
                    push32(&mut c, a);
                    at = c.len();
                    slot_at = Some(at + 2);
                    c.extend_from_slice(&[op, sub, slot, OP_HALT]);
                }
                OP_ARR_LOAD_V => {
                    // Variable 1 = the array (built by `arr_ctx`), index a & 3.
                    push32(&mut c, a & 3);
                    at = c.len();
                    slot_at = Some(at + 1);
                    c.extend_from_slice(&[op, slot, OP_HALT]);
                }
                OP_STORE_OP => {
                    push32(&mut c, a);
                    c.extend_from_slice(&[OP_STORE, 1]);
                    push32(&mut c, b);
                    at = c.len();
                    slot_at = Some(at + 2);
                    c.extend_from_slice(&[op, sub, slot, OP_LOAD, 1, OP_HALT]);
                }
                OP_LOAD => {
                    push32(&mut c, a);
                    c.extend_from_slice(&[OP_STORE, 1]);
                    at = c.len();
                    slot_at = Some(at + 1);
                    c.extend_from_slice(&[op, slot, OP_HALT]);
                }
                OP_STORE => {
                    push32(&mut c, a);
                    at = c.len();
                    slot_at = Some(at + 1);
                    c.extend_from_slice(&[op, slot, OP_LOAD, 1, OP_HALT]);
                }
                _ => {
                    // OP_ADD_VI
                    push32(&mut c, a);
                    c.extend_from_slice(&[OP_STORE, 1]);
                    at = c.len();
                    slot_at = Some(at + 1);
                    c.extend_from_slice(&[op, slot]);
                    c.extend_from_slice(&(b as i16).to_le_bytes());
                    c.extend_from_slice(&[OP_LOAD, 1, OP_HALT]);
                }
            }
            (c, at, slot_at)
        }
        // Variable 1 holds an array [10, 20, 30, 40].
        fn arr_vm(code: &[u8]) -> (Vm, Ctx<MockPlatform>) {
            let mut ctx = mock_ctx();
            let id = ctx.objs.arr_alloc(4).unwrap();
            ctx.objs.arr_mut(id).unwrap().copy_from_slice(&[10, 20, 30, 40]);
            let mut vm = Vm::new();
            vm.code = VmCode::new(code.to_vec());
            vm.var_set(1, id as i32);
            vm.state = VmState::Running;
            (vm, ctx)
        }
        fn threaded(code: &[u8]) -> Vm {
            let (mut vm, mut ctx) = arr_vm(code);
            while vm.state == VmState::Running {
                vm.tick(&mut ctx, 1000);
            }
            vm
        }
        fn stepped(code: &[u8]) -> Vm {
            let (mut vm, mut ctx) = arr_vm(code);
            while vm.state == VmState::Running {
                vm.step(&mut ctx);
            }
            vm
        }
        fn reference(code: &[u8]) -> Vm {
            let (mut vm, mut ctx) = arr_vm(code);
            while vm.state == VmState::Running {
                vm.step_ref(&mut ctx);
            }
            vm
        }
        fn top(vm: &Vm) -> i32 {
            assert!(vm.state == VmState::Halted);
            vm.stack[vm.sp as usize - 1]
        }
        let has_sub = |op: u8| (OP_JCMP..=OP_STORE_OP).contains(&op) && op != OP_ADD_VI && op != OP_ARR_LOAD_V;
        for (i, &(op, sub, kind)) in QUICK.iter().enumerate() {
            let q = Q_FIRST + i as u8;
            let slot = if kind == SLOT_L { 0x81 } else { 0x01 };
            for a in VALS {
                for b in VALS {
                    let what = alloc::format!("{op:#x} {sub:#x} {kind} {a} {b}");
                    let (plain, at, slot_at) = program(op, sub, slot, a, b);
                    let mut quick = plain.clone();
                    quick[at] = q;
                    if has_sub(op) {
                        quick[at + 1] = 0xEE;
                    }
                    if let Some(k) = slot_at.filter(|_| kind != SLOT_ANY) {
                        quick[k] ^= 0x80;
                    }
                    let want = top(&reference(&plain));
                    assert_eq!(top(&reference(&quick)), want, "reference, {what}");
                    assert_eq!(top(&stepped(&quick)), want, "stepped, {what}");
                    assert_eq!(top(&threaded(&quick)), want, "threaded, {what}");
                    let vm = threaded(&plain);
                    assert_eq!(top(&vm), want, "threaded plain, {what}");
                    assert_eq!(vm.code.bytes()[at], q, "quickened, {what}");
                    // Its operands as they were (later LOADs / STOREs are quickened too).
                    for k in [Some(at + 1), slot_at].into_iter().flatten() {
                        assert_eq!(vm.code.bytes()[k], plain[k], "operand {k}, {what}");
                    }
                }
            }
        }
        // Without a quickened form (BIN_I EQ, a bad op, JCMP_VI / STORE_OP /
        // ADD_VI on a global): left as they are.
        for (op, sub) in [(OP_BIN_I, OP_EQ), (OP_BIN_I, 0x7F), (OP_JCMP_VI, OP_LT), (OP_STORE_OP, OP_ADD), (OP_ADD_VI, 0)] {
            let (plain, at, _) = program(op, sub, 0x01, 7, 2);
            let vm = threaded(&plain);
            assert_eq!(vm.code.bytes()[at], op, "{op:#x} {sub:#x}");
        }
    }

    /// A quickened form's slot kind is forced: a local form reads the local
    /// whatever bit 7 says, a global form the global, on both paths.
    #[test]
    fn quickened_slot_kind_is_forced() {
        // (code, the variable it reads / writes as its result): frame_base 4,
        // so local 1 = variable 5 (42) and global 1 = variable 1 (7).
        let cases: [(std::vec::Vec<u8>, i32); 8] = [
            (std::vec![Q_LOAD_L, 0x01, OP_HALT], 42),
            (std::vec![Q_LOAD_G, 0x81, OP_HALT], 7),
            (std::vec![OP_PUSH_2, Q_STORE_L, 0x01, OP_LOAD, 0x81, OP_HALT], 2),
            (std::vec![OP_PUSH_2, Q_STORE_G, 0x81, OP_LOAD, 0x01, OP_HALT], 2),
            (std::vec![Q_ADD_VI_L, 0x01, 3, 0, OP_LOAD, 0x81, OP_HALT], 45),
            (std::vec![OP_PUSH_1, quick_of(OP_STORE_OP, OP_ADD, 0x80), OP_ADD, 0x01, OP_LOAD, 0x81, OP_HALT], 43),
            (std::vec![OP_PUSH_1, quick_of(OP_BIN_V, OP_ADD, 0x80), OP_SUB, 0x01, OP_HALT], 43),
            (std::vec![OP_PUSH_1, quick_of(OP_BIN_V, OP_SUB, 0x00), OP_ADD, 0x81, OP_HALT], -6),
        ];
        for (code, want) in cases {
            for mode in ["reference", "stepped", "threaded"] {
                let mut ctx = mock_ctx();
                let mut vm = Vm::new();
                vm.code = VmCode::new(code.clone());
                vm.frame_base = 4;
                vm.var_set(5, 42);
                vm.var_set(1, 7);
                vm.state = VmState::Running;
                while vm.state == VmState::Running {
                    match mode {
                        "reference" => vm.step_ref(&mut ctx),
                        "stepped" => vm.step(&mut ctx),
                        _ => vm.tick(&mut ctx, 1000),
                    }
                }
                assert!(vm.state == VmState::Halted, "{code:x?}");
                assert_eq!(vm.stack[vm.sp as usize - 1], want, "{code:x?} {mode}");
            }
        }
    }

    /// The profile counts quickened code as its instruction, with the cond
    /// and the slot kinds. (Other tests running meanwhile may add to
    /// the shared counters: at least the loop's counts.)
    #[test]
    fn profile_counts_superinstructions() {
        // g1 = 0; loop: g1 += 1; if g1 != 100 goto loop
        let code = std::vec![
            OP_PUSH_0, OP_STORE, 0x81, OP_ADD_VI, 0x81, 1, 0, OP_JCMP_VI, OP_NE, 0x81, 100, 0, 3, 0, 0, 0, OP_HALT,
        ];
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        vm.code = VmCode::new(code);
        let run = |vm: &mut Vm, ctx: &mut Ctx<crate::mock::MockPlatform>| {
            vm.pc = 0;
            vm.sp = 0;
            vm.state = VmState::Running;
            while vm.state == VmState::Running {
                vm.tick(ctx, 1000);
            }
            assert_eq!(vm.var_get(1), 100);
        };
        run(&mut vm, &mut ctx); // threaded: quickens the JCMP_VI
        assert_eq!(vm.code.bytes()[7], quick_of(OP_JCMP_VI, OP_NE, 0x81));
        prof::reset();
        prof::ON.store(true, core::sync::atomic::Ordering::Relaxed);
        run(&mut vm, &mut ctx);
        prof::ON.store(false, core::sync::atomic::Ordering::Relaxed);
        let report = prof::report();
        let count = |key: &str| -> u32 {
            report.lines().find_map(|l| l.strip_prefix(key)).map_or(0, |n| n.trim().parse().unwrap())
        };
        assert!(count("total ") >= 203, "{report}");
        assert!(count(&alloc::format!("op 0x{OP_JCMP_VI:02x} ")) >= 100, "{report}");
        assert!(count(&alloc::format!("sub 0x{OP_JCMP_VI:02x} 0x{OP_NE:02x} ")) >= 100, "{report}");
        assert!(count(&alloc::format!("slot 0x{OP_ADD_VI:02x} L ")) >= 100, "{report}");
        assert_eq!(count(&alloc::format!("op 0x{:02x} ", quick_of(OP_JCMP_VI, OP_NE, 0x81))), 0, "{report}");
    }

    #[test]
    fn fault_bad_opcode() {
        let vm = run_code(&[0xFF]);
        assert_eq!(vm.fault(), fault::BAD_OPCODE);
        assert_eq!(vm.fault_pc(), 0);
    }


    #[test]
    fn first_fault_wins_within_instruction() {
        // ADD on an empty stack: two failed pops, then the push succeeds —
        // the recorded cause must stay STACK_UNDERFLOW.
        let vm = run_code(&[OP_ADD]);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
    }

    #[test]
    fn pop2_keeps_push_order() {
        // g0 = 9 - 2, g1 = 2 < 9
        let vm = run_code(&[
            OP_PUSH_I8, 9, OP_PUSH_I8, 2, OP_SUB, OP_STORE, 0,
            OP_PUSH_I8, 2, OP_PUSH_I8, 9, OP_LT, OP_STORE, 1, OP_HALT,
        ]);
        assert!(vm.state != VmState::Error);
        assert_eq!(vm.var_get(0), 7);
        assert_eq!(vm.var_get(1), 1);
    }

    #[test]
    fn pop2_underflow_with_one_value() {
        let vm = run_code(&[OP_PUSH_I8, 5, OP_SUB]);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 2);
        assert_eq!(vm.stack[0], 5);
    }

    #[test]
    fn callback_pop2_respects_floor() {
        // main @0: PUSH 7, HALT     callback @3: SUB with one arg
        let code = [OP_PUSH_I8, 7, OP_HALT, OP_SUB, OP_RET];
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 1);
        vm.enqueue_callback(3, &[1]);
        vm.drain_callbacks(&mut ctx);
        assert_eq!(vm.fault(), fault::STACK_UNDERFLOW);
        assert_eq!(vm.fault_pc(), 3);
        assert_eq!(vm.stack[0], 7);
    }

    #[test]
    fn page_ops_build_widgets_like_bytecode() {
        let mut ctx = mock_ctx();
        ctx.objs.root = ctx.objs.alloc_widget().unwrap();
        let mut vm = cb_vm(&[OP_HALT]);
        vm.frame_base = 4; // locals after the 4 globals
        #[rustfmt::skip]
        let ops = [
            PG_STORE, 1, 14,                    // g1 = 7
            PG_ALLTAR, 0,                       // g0 = alloc(); target(g0)
            PROP_SIZE, 3, 0xC0, 0x0C, 0x64,     // set(size, 800, 50)
            PROP_BG_COLOR, 0x82, 0x21,          // set(bg_color, 0x0841)
            PG_PARENT_ID, 0,                    // parent(0)
            PG_ALLTAR, 0x80,                    // var l = alloc(); target(l)
            PROP_TEXT, 2, b'H', b'i',           // set(text, "Hi")
            PG_PARENT, 0,                       // parent(g0)
        ];
        vm.run_page_ops(&mut ctx, &ops);
        assert_eq!(vm.fault(), fault::NONE);
        assert_eq!(vm.var_get(1), 7);
        let (a, b) = (WidgetId(1), WidgetId(2));
        assert_eq!(vm.var_get(0), 1);
        assert_eq!(vm.var_get(4), 2);
        let w = ctx.objs.get(a);
        assert!(w.parent == ctx.objs.root);
        assert_eq!((w.size.w, w.size.h), (800, 50));
        assert_eq!(w.background_color, 0x0841);
        assert!(ctx.objs.get(b).parent == a);
        assert_eq!(ctx.objs.str_get(ctx.objs.text_id(b)), b"Hi");
        assert!(vm.target == b);
    }

    #[test]
    fn malformed_page_ops_fault() {
        let cases: [&[u8]; 4] = [
            &[PROP_BG_COLOR, 0x82],   // truncated varint
            &[PROP_SIZE, 5, 1],       // payload past the block
            &[0x90, 0],               // unknown op
            &[PG_ALLTAR],             // missing operand
        ];
        for ops in cases {
            let mut ctx = mock_ctx();
            ctx.objs.root = ctx.objs.alloc_widget().unwrap();
            let mut vm = cb_vm(&[OP_HALT]);
            vm.run_page_ops(&mut ctx, ops);
            assert_eq!(vm.fault(), fault::PAGE, "{ops:?}");
        }
    }

    #[test]
    fn w_set_len_keeps_long_text() {
        let text = b"Spline graph (Catmull-Rom + fill, vs linear)";
        assert!(text.len() > 32);
        let mut code = alloc::vec![OP_W_TARGET, 1, OP_W_SET_LEN, PROP_TEXT, text.len() as u8];
        code.extend_from_slice(text);
        code.push(OP_HALT);
        let mut ctx = mock_ctx();
        ctx.objs.root = ctx.objs.alloc_widget().unwrap();
        let w = ctx.objs.alloc_widget().unwrap();
        let mut vm = cb_vm(&code);
        vm.tick(&mut ctx, 8);
        assert_eq!(vm.fault(), fault::NONE);
        assert_eq!(ctx.objs.str_get(ctx.objs.text_id(w)), text);
    }

    fn svarint(v: i32) -> Vec<u8> {
        let mut b = [0u8; 5];
        let n = proto::encode_signed_varint(&mut b, 0, v);
        b[..n].to_vec()
    }

    fn uvarint(v: u32) -> Vec<u8> {
        let mut b = [0u8; 5];
        let n = proto::encode_varint(&mut b, 0, v);
        b[..n].to_vec()
    }

    /// A W_SETALL field: key varint + value bytes.
    fn field(prop: u8, wt: u8, value: &[u8]) -> Vec<u8> {
        let mut f = uvarint(u32::from(prop) << 3 | u32::from(wt));
        f.extend_from_slice(value);
        f
    }

    /// Runs `code` on a fresh pool (root + widget 1); widget 1 afterwards.
    fn run_on_widget(code: &[u8]) -> (Vm, (i16, i16, u16, u16, u16, Vec<u8>, u8)) {
        let mut ctx = mock_ctx();
        ctx.objs.root = ctx.objs.alloc_widget().unwrap();
        let w = ctx.objs.alloc_widget().unwrap();
        let mut vm = cb_vm(code);
        vm.tick(&mut ctx, 64);
        let x = *ctx.objs.get(w);
        let text = ctx.objs.str_get(ctx.objs.text_id(w)).to_vec();
        let state = (x.location.x, x.location.y, x.size.w, x.size.h, x.background_color, text, ctx.objs.ext(w).flex_gap);
        (vm, state)
    }

    #[test]
    fn w_setall_matches_plain_sets() {
        let text = b"A text longer than the 64-byte buffer W_SETALL copies its fields to, \n                     and long enough for a two-byte length varint in front of them.";
        let mut size = svarint(300);
        size.extend(svarint(140));
        // Plain: bg (from a variable), loc_x, size, loc_y (computed), text, flex_gap.
        let mut plain = alloc::vec![OP_W_TARGET, 1, OP_PUSH_I16, 0x34, 0x12, OP_W_SET, PROP_BG_COLOR,
                                    OP_PUSH_I8, (-10i8) as u8, OP_W_SET, PROP_LOC_X,
                                    OP_W_SET_LEN, PROP_SIZE, size.len() as u8];
        plain.extend_from_slice(&size);
        plain.extend_from_slice(&[OP_PUSH_I8, 33, OP_W_SET, PROP_LOC_Y, OP_W_SET_LEN, PROP_TEXT, text.len() as u8]);
        plain.extend_from_slice(text);
        plain.extend_from_slice(&[OP_PUSH_I8, 6, OP_W_SET, PROP_FLEX_GAP, OP_HALT]);

        let mut fields = field(PROP_BG_COLOR, proto::WT_STACK, &[]);
        fields.extend(field(PROP_LOC_X, proto::WT_VARINT, &svarint(-10)));
        let mut len = uvarint(size.len() as u32);
        len.extend_from_slice(&size);
        fields.extend(field(PROP_SIZE, proto::WT_LEN, &len));
        fields.extend(field(PROP_LOC_Y, proto::WT_STACK, &[]));
        let mut t = uvarint(text.len() as u32);
        t.extend_from_slice(text);
        fields.extend(field(PROP_TEXT, proto::WT_LEN, &t));
        fields.extend(field(PROP_FLEX_GAP, proto::WT_VARINT, &svarint(6)));
        assert!(fields.len() > 128, "a two-byte length");
        // The stack values in reverse field order: loc_y, then bg on top.
        let mut setall = alloc::vec![OP_W_TARGET, 1, OP_PUSH_I8, 33, OP_PUSH_I16, 0x34, 0x12, OP_W_SETALL];
        setall.extend(uvarint(fields.len() as u32));
        setall.extend(fields);
        setall.push(OP_HALT);

        let (pvm, pstate) = run_on_widget(&plain);
        let (svm, sstate) = run_on_widget(&setall);
        assert_eq!(pvm.fault(), fault::NONE);
        assert_eq!(svm.fault(), fault::NONE);
        assert_eq!(pstate, (-10, 33, 300, 140, 0x1234, text.to_vec(), 6));
        assert_eq!(sstate, pstate);
        assert_eq!(svm.sp, 0, "the stack values were popped");
    }

    #[test]
    fn w_setall_rejects_malformed_fields() {
        let cases: [(&[u8], u8); 5] = [
            (&[OP_W_SETALL, 3, PROP_BG_COLOR << 3, 0x82], fault::CODE_OOB),       // past the code
            (&[OP_W_SETALL, 2, PROP_BG_COLOR << 3, 0x82, OP_HALT], fault::CODE_OOB), // truncated varint
            (&[OP_W_SETALL, 2, PROP_BG_COLOR << 3 | 7, 0, OP_HALT], fault::BAD_OPERAND), // wire type
            (&[OP_W_SETALL, 3, PROP_SIZE << 3 | 2, 5, 1, OP_HALT], fault::CODE_OOB), // payload past the fields
            (&[OP_W_SETALL, 1, PROP_BG_COLOR << 3 | 3, OP_HALT], fault::STACK_UNDERFLOW), // empty stack
        ];
        for (code, want) in cases {
            let (vm, _) = run_on_widget(code);
            assert_eq!(vm.fault(), want, "{code:?}");
        }
    }

    #[test]
    fn w_load_without_page_faults() {
        let mut ctx = mock_ctx();
        let mut vm = cb_vm(&[OP_W_LOAD, 0, 0, OP_HALT]);
        vm.tick(&mut ctx, 4);
        assert_eq!(vm.fault(), fault::PAGE);
        assert_eq!(vm.fault_pc(), 0);
    }

    /// v4 image with no functions: header flags + opcodes (pass
    /// IMAGE_FLAG_WIDE_ADDR for a loadable one).
    fn image(flags: u16, code: &[u8]) -> Vec<u8> {
        let [f0, f1] = flags.to_le_bytes();
        let mut img = alloc::vec![4, 0, 0, 0, 0, f0, f1, 0, 0];
        img.extend_from_slice(code);
        img
    }

    /// `OP_STR_LIT` with a `len`-byte literal, then HALT.
    fn long_literal(len: usize) -> (Vec<u8>, Vec<u8>) {
        let text: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let mut code = alloc::vec![OP_STR_LIT, len as u8];
        code.extend_from_slice(&text);
        code.push(OP_HALT);
        (code, text)
    }

    #[test]
    fn str_lit_keeps_literals_longer_than_64_bytes() {
        // Used to be cut to 64 bytes (a 128-byte URL lost its query string).
        let (code, text) = long_literal(255);
        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        vm.code = VmCode::new(code);
        vm.run(&mut ctx);
        assert!(vm.state == VmState::Halted);
        let id = vm.stack[vm.sp as usize - 1] as ObjPtr;
        assert_eq!(ctx.objs.str_get(id), &text[..]);
    }

    #[test]
    fn load_reads_the_whole_function_table() {
        // 300 functions — well past the old 64-entry stack buffer.
        const FUNCS: u16 = 300;
        let [c0, c1] = FUNCS.to_le_bytes();
        let mut img = alloc::vec![4, c0, c1, 0, 0, IMAGE_FLAG_WIDE_ADDR as u8, 0, 0, 0];
        for i in 0..FUNCS {
            img.extend_from_slice(&(i + 1).to_le_bytes()); // func_id
            img.extend_from_slice(&[FunctionKind::UserFunction as u8, 0]);
            img.extend_from_slice(&(i as u32).to_le_bytes()); // offset
            img.extend_from_slice(&1u32.to_le_bytes()); // length
        }
        let code = alloc::vec![OP_HALT; FUNCS as usize];
        img.extend_from_slice(&code);

        let mut vm = Vm::new();
        assert!(vm.load_ram(&img));
        assert_eq!(vm.functions.len(), FUNCS as usize);
        assert_eq!(vm.func_addr(FUNCS), Some(FUNCS as u32 - 1));
        assert_eq!(vm.code.len(), code.len());
    }

    #[test]
    fn load_rejects_a_truncated_function_table() {
        // Header announces 300 functions but the image is cut short.
        let img = alloc::vec![4, 0x2C, 0x01, 0, 0, 0, 0, 0, 0, 1, 0, 0];
        let mut vm = Vm::new();
        assert!(!vm.load_ram(&img));
    }

    #[test]
    fn load_ram_owned_keeps_the_buffer() {
        let mut vm = Vm::new();
        let mut img = image(IMAGE_FLAG_WIDE_ADDR, &[OP_PUSH_1, OP_HALT]);
        // Room for the pad (a real image's function table leaves it).
        img.reserve_exact(CODE_PAD);
        let ptr = img.as_ptr();
        assert!(vm.load_ram_owned(img).is_ok());
        // Header stripped in place: same allocation, opcodes only.
        assert_eq!(vm.code.bytes(), &[OP_PUSH_1, OP_HALT]);
        assert_eq!(vm.code.ptr(), ptr);
        // A bad header hands the buffer back untouched (raw-bytecode fallback).
        let raw = alloc::vec![0xFFu8, 1, 2];
        assert_eq!(vm.load_ram_owned(raw.clone()), Err(raw));
    }

    #[test]
    fn only_wide_address_images_load() {
        let mut vm = Vm::new();
        assert!(vm.load_ram(&image(IMAGE_FLAG_WIDE_ADDR, &[OP_HALT])));
        assert!(vm.load_ram(&image(IMAGE_FLAG_WIDE_ADDR, &alloc::vec![OP_HALT; 0x1_0001])));
        // u16 jump/call targets: a ferrite-ui image for another device.
        assert!(!vm.load_ram(&image(0, &[OP_HALT])));
        assert!(!vm.load_ram(&[5, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, OP_HALT]));
    }

    #[test]
    fn wide_jump_and_call_past_64k() {
        // 0x00000: JMP 0x10000
        // 0x10000: CALL 0x10010   → ret_addr 0x10005 must survive as u32
        // 0x10005: <bad opcode>   ← RET lands here
        // 0x10010: RET
        let mut code = alloc::vec![OP_HALT; 0x1_0011];
        code[..5].copy_from_slice(&[OP_JMP, 0x00, 0x00, 0x01, 0x00]);
        code[0x1_0000..0x1_0005].copy_from_slice(&[OP_CALL, 0x10, 0x00, 0x01, 0x00]);
        code[0x1_0005] = 0xFF;
        code[0x1_0010] = OP_RET;

        let mut ctx = mock_ctx();
        let mut vm = Vm::new();
        assert!(vm.load_ram(&image(IMAGE_FLAG_WIDE_ADDR, &code)));
        vm.run(&mut ctx);
        assert_eq!(vm.fault(), fault::BAD_OPCODE);
        assert_eq!(vm.fault_pc(), 0x1_0005);
    }

    /// v5 image: `globals` globals, `var_slots` variable slots.
    fn image_v5(globals: u8, var_slots: u16, code: &[u8]) -> Vec<u8> {
        let [s0, s1] = var_slots.to_le_bytes();
        let mut img = alloc::vec![5, 0, 0, globals, 0, IMAGE_FLAG_WIDE_ADDR as u8, 0, 0, 0, s0, s1];
        img.extend_from_slice(code);
        img
    }

    /// main: CALL f; HALT. f: FRAME 3; CALL g; RET. g: FRAME 2; RET.
    /// With 2 globals g's locals end at slot 2 + 3 + 2 = 7.
    const NESTED_FRAMES: [u8; 17] = [
        OP_CALL, 6, 0, 0, 0, OP_HALT, OP_FRAME, 3, OP_CALL, 14, 0, 0, 0, OP_RET, OP_FRAME, 2, OP_RET,
    ];

    #[test]
    fn v5_header_sizes_the_variable_table() {
        let mut vm = Vm::new();
        assert!(vm.load_ram(&image_v5(2, 7, &NESTED_FRAMES)));
        assert_eq!(vm.var_slots, 7);
        assert_eq!(vm.vars.len(), 7 + VAR_PAD);
        let mut ctx = mock_ctx();
        vm.run(&mut ctx);
        assert_eq!(vm.fault(), fault::NONE);
        // An older header keeps the old table; var_slots never undercuts the globals.
        assert!(vm.load_ram(&image(IMAGE_FLAG_WIDE_ADDR, &[OP_HALT])));
        assert_eq!(vm.var_slots, LEGACY_VAR_SLOTS as u32);
        assert!(vm.load_ram(&image_v5(9, 4, &[OP_HALT])));
        assert_eq!(vm.var_slots, 9);
        assert!(!vm.load_ram(&image_v5(0, VAR_SLOTS_MAX + 1, &[OP_HALT])));
    }

    #[test]
    fn frame_past_var_slots_faults() {
        let mut vm = Vm::new();
        assert!(vm.load_ram(&image_v5(2, 6, &NESTED_FRAMES)));
        let mut ctx = mock_ctx();
        vm.run(&mut ctx);
        assert_eq!(vm.fault(), fault::VARS_FULL);
        assert_eq!(vm.fault_pc(), 14);
    }

    #[test]
    fn frame_a_args_past_its_locals_stay_in_the_table() {
        // Malformed: FRAME_A 1 2 at the top of a 1-slot table. The second
        // argument lands in VAR_PAD (debug_assert in var_set otherwise).
        let mut vm = Vm::new();
        // PUSH_1; PUSH_1; CALL f; HALT. f: FRAME_A 1 2; RET.
        let code = [OP_PUSH_1, OP_PUSH_1, OP_CALL, 8, 0, 0, 0, OP_HALT, OP_FRAME_A, 1, 2, OP_RET];
        assert!(vm.load_ram(&image_v5(0, 1, &code)));
        let mut ctx = mock_ctx();
        vm.run(&mut ctx);
        assert_eq!(vm.fault(), fault::NONE);
        assert_eq!((vm.var(0), vm.var(1)), (1, 1));
    }
}
