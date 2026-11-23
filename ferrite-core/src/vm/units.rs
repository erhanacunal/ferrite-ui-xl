//! Code units: a program and the libraries it calls (docs/apps.md "Step 3").
//!
//! A library's code and function table are loaded once ([`LibImage`], shared
//! through an `Arc` by every task that imports it); each importing VM keeps
//! its own copy of the library's globals. Unit 0 is the program, 1.. its
//! libraries in the order of the image's library table. The running unit's
//! code and functions are `Vm::code` / `Vm::functions` (its slot here parked
//! empty), so the hot path reads them as before; code addresses that cross
//! units (return addresses, callback pcs, queued callbacks, `CALL_IMP`
//! targets) carry the unit in bits 24..32 and `Vm::goto` switches.
//!
//! Variable slots: the program's globals, then each library's (`gbase`),
//! then the locals of every unit. The VM sizes `var_slots` for the program
//! plus, per callback level, each library's deepest locals, and the call
//! stack for each level's deepest library chain.

use super::*;
use alloc::string::String;

/// At most this many libraries per program (unit ids 1..=MAX_LIBS).
pub const MAX_LIBS: usize = 7;
/// Type code of a void return (ferrite-ui ferrite_cc.py TYPE_CODES).
pub const TYPE_VOID: u8 = 0;

/// An imported function (image v6 import table).
#[derive(Clone, Debug, PartialEq)]
pub struct LibImport {
    /// Index into the library table.
    pub lib: u8,
    pub nargs: u8,
    pub ret: u8,
    pub name: Vec<u8>,
}

/// An exported function (a library's export table).
#[derive(Clone, Debug, PartialEq)]
pub struct LibExport {
    pub func_id: u16,
    pub ret: u8,
    pub params: Vec<u8>,
    pub name: Vec<u8>,
}

/// An image v6's library tables.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LibTables {
    /// The image is a library (header flags bit 4).
    pub library: bool,
    /// The libraries a program calls: (id, major version).
    pub libs: Vec<(String, u8)>,
    pub imports: Vec<LibImport>,
    pub exports: Vec<LibExport>,
    /// A library's deepest call chain, in frames.
    pub call_depth: u8,
}

/// Parses the v6 tables (counts in the header at 11..17) from `at` (the end
/// of the function table); the tables and where the opcodes start.
pub(super) fn parse_tables(data: &[u8], at: usize, library: bool) -> Option<(LibTables, usize)> {
    let lib_count = data[11] as usize;
    let import_count = u16::from_le_bytes([data[12], data[13]]) as usize;
    let export_count = u16::from_le_bytes([data[14], data[15]]) as usize;
    let mut r = Reader { data, at };
    let mut t = LibTables { library, call_depth: data[16], ..Default::default() };
    for _ in 0..lib_count {
        let id = String::from_utf8(r.name()?).ok()?;
        t.libs.push((id, r.u8()?));
    }
    for _ in 0..import_count {
        let (lib, nargs, ret) = (r.u8()?, r.u8()?, r.u8()?);
        if lib as usize >= lib_count {
            return None;
        }
        t.imports.push(LibImport { lib, nargs, ret, name: r.name()? });
    }
    for _ in 0..export_count {
        let func_id = u16::from_le_bytes([r.u8()?, r.u8()?]);
        let (nargs, ret) = (r.u8()?, r.u8()?);
        let params = r.take(nargs as usize)?.to_vec();
        t.exports.push(LibExport { func_id, ret, params, name: r.name()? });
    }
    Some((t, r.at))
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.data.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn name(&mut self) -> Option<Vec<u8>> {
        let n = self.u8()? as usize;
        if n == 0 {
            return None;
        }
        self.take(n).map(|s| s.to_vec())
    }
}

/// A loaded library: its code and tables, shared by every VM importing it.
pub struct LibImage {
    pub id: String,
    /// major << 16 | minor << 8 | patch (its libinfo).
    pub version: u32,
    code: VmCode,
    functions: Vec<FuncEntry>,
    globals: u32,
    /// Its globals plus one call chain's locals.
    var_slots: u32,
    call_depth: u8,
    pub exports: Vec<LibExport>,
}

impl LibImage {
    /// A library's program image (`lib`, header v6 with the library flag).
    pub fn parse(id: &str, version: u32, mut image: Vec<u8>) -> Option<Self> {
        let h = parse_image_header(&image)?;
        let tables = h.libs?;
        if !tables.library || !tables.libs.is_empty() || !tables.imports.is_empty() {
            return None;
        }
        let code_len = image.len() - h.opcode_start;
        if !code_addressable(h.wide, code_len) || code_len >= 1 << 24 {
            return None;
        }
        image.drain(..h.opcode_start);
        let globals = h.global_count as u32;
        Some(Self {
            id: id.into(),
            version,
            code: VmCode::new(image),
            functions: h.functions,
            globals,
            var_slots: (h.var_slots.unwrap_or(0) as u32).max(globals),
            call_depth: tables.call_depth,
            exports: tables.exports,
        })
    }

    pub fn major(&self) -> u8 {
        (self.version >> 16) as u8
    }

    /// Bytes of code (for the memory report).
    pub fn code_len(&self) -> usize {
        self.code.len()
    }
}

/// A unit of a running program (see the module docs).
pub(super) struct Unit {
    /// None while it runs (then in `Vm::code`).
    code: Option<VmCode>,
    functions: Vec<FuncEntry>,
    gbase: u32,
    /// Keeps a library loaded while this VM uses it.
    _lib: Option<Arc<LibImage>>,
}

impl Vm {
    /// Continue at `addr`: a code address, tagged with its unit when the
    /// program has libraries.
    #[inline]
    pub(super) fn goto(&mut self, addr: CodeAddr) {
        if self.units.is_empty() {
            self.pc = addr;
        } else {
            let u = (addr >> 24) as u8;
            if u != self.unit {
                self.switch_unit(u);
            }
            self.pc = addr & 0x00FF_FFFF;
        }
    }

    #[cold]
    #[inline(never)]
    fn switch_unit(&mut self, u: u8) {
        let (cur, new) = (self.unit as usize, u as usize);
        let Some(code) = self.units.get_mut(new).and_then(|n| n.code.take()) else {
            self.fail(fault::BAD_OPERAND);
            return;
        };
        let functions = core::mem::take(&mut self.units[new].functions);
        let old = &mut self.units[cur];
        old.code = Some(core::mem::replace(&mut self.code, code));
        old.functions = core::mem::replace(&mut self.functions, functions);
        self.gbase = self.units[new].gbase;
        self.unit = u;
        self.utag = (u as u32) << 24;
    }

    /// Drop the libraries (back to the program's unit first).
    pub(super) fn clear_units(&mut self) {
        // (Unit 0's slot is parked empty while it runs: no switch then.)
        if self.unit != 0 {
            self.switch_unit(0);
        }
        self.units.clear();
        self.links.clear();
        self.unit = 0;
        self.utag = 0;
        self.gbase = 0;
    }

    /// Unit `u`'s function table.
    pub(super) fn funcs_of(&self, u: u8) -> Option<&[FuncEntry]> {
        if u == self.unit {
            Some(&self.functions)
        } else {
            self.units.get(u as usize).map(|n| n.functions.as_slice())
        }
    }

    /// The entry addresses of every unit's `kind` function, the program's
    /// first (a widget event goes to each unit's dispatcher: each checks its
    /// own widgets). Returns how many it wrote to `out`.
    pub fn handlers(&self, kind: FunctionKind, out: &mut [CodeAddr; MAX_LIBS + 1]) -> usize {
        let mut n = 0;
        for u in 0..self.units.len().max(1) {
            let found = self.funcs_of(u as u8).and_then(|f| f.iter().find(|f| f.kind as u8 == kind as u8));
            if let Some(f) = found {
                out[n] = f.offset as CodeAddr | (u as CodeAddr) << 24;
                n += 1;
            }
        }
        n
    }

    /// The libraries the loaded program calls (id, major), until linked.
    pub fn needed_libs(&self) -> &[(String, u8)] {
        self.lib_tables.as_ref().map_or(&[], |t| t.libs.as_slice())
    }

    /// Links the loaded program with `libs` (one per [`Vm::needed_libs`],
    /// same order): resolves its imports, places the libraries' globals,
    /// grows the variable slots and call stack. A `fault::LIB_*` /
    /// `VARS_FULL` code on failure (the program can't start).
    pub fn link(&mut self, libs: &[Arc<LibImage>]) -> Result<(), u8> {
        let Some(t) = self.lib_tables.take() else {
            return Ok(());
        };
        if libs.len() != t.libs.len() || libs.len() > MAX_LIBS || self.code.len() >= 1 << 24 {
            return Err(fault::LIB_MISMATCH);
        }
        for (lib, (id, major)) in libs.iter().zip(&t.libs) {
            if lib.id != *id || lib.major() != *major {
                return Err(fault::LIB_MISSING);
            }
        }
        let mut links = Vec::with_capacity(t.imports.len());
        for imp in &t.imports {
            let lib = &libs[imp.lib as usize];
            let e = lib.exports.iter().find(|e| e.name == imp.name).ok_or(fault::LIB_MISMATCH)?;
            if e.params.len() != imp.nargs as usize || e.ret != imp.ret {
                return Err(fault::LIB_MISMATCH);
            }
            let f = lib.functions.iter().find(|f| f.func_id == e.func_id).ok_or(fault::LIB_MISMATCH)?;
            links.push(f.offset as CodeAddr | (imp.lib as CodeAddr + 1) << 24);
        }
        // Slots: the program's globals, the libraries', then the locals.
        let levels = CB_NEST_MAX as u32 + 1;
        let mut gbase = self.global_count;
        let mut slots = self.var_slots;
        let mut depth = 0u32;
        let mut units = Vec::with_capacity(libs.len() + 1);
        units.push(Unit { code: None, functions: Vec::new(), gbase: 0, _lib: None });
        for lib in libs {
            units.push(Unit {
                code: Some(lib.code.clone()),
                functions: lib.functions.clone(),
                gbase,
                _lib: Some(lib.clone()),
            });
            gbase += lib.globals;
            slots += lib.globals + levels * (lib.var_slots - lib.globals);
            depth = depth.max(lib.call_depth as u32);
        }
        if slots > VAR_SLOTS_MAX as u32 || !self.set_var_slots(slots as u16) {
            return Err(fault::VARS_FULL);
        }
        let calls = (self.call_base as u32 + levels * depth).min(255) as usize;
        if calls > self.call_stack.len() {
            match try_alloc_slice(calls, EMPTY_FRAME) {
                Some(b) => self.call_stack = b,
                None => return Err(fault::VARS_FULL),
            }
        }
        self.units = units;
        self.links = links;
        self.global_count = gbase;
        self.frame_base = gbase;
        Ok(())
    }

    /// Each library's `setup` (global initializers), tagged, to run before
    /// the program's.
    pub fn lib_setups(&self) -> Vec<CodeAddr> {
        (1..self.units.len())
            .filter_map(|u| {
                let f = self.funcs_of(u as u8)?.iter().find(|f| f.kind as u8 == FunctionKind::Setup as u8)?;
                Some(f.offset as CodeAddr | (u as CodeAddr) << 24)
            })
            .collect()
    }

    /// The running unit (0 = the program).
    pub fn unit(&self) -> u8 {
        self.unit
    }
}
