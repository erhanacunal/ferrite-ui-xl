//! The storage builtins (`storage.rs`): opcodes 0xDC..=0xE5, `caps.storage`
//! (`CAP_STORAGE`). Paths are strings; results are the values in
//! `storage::err` when negative. A handle that is not open (or another
//! task's) faults with `BAD_FILE`, as for resource files; writing to a
//! resource file's handle too.

use super::{Platform, Vm, fault};
use crate::ctx::Ctx;
use crate::pool::ObjPtr;
use crate::storage::PATH_MAX;

/// `diskOpen(path, mode)` → handle (16..) or an error.
pub(super) const OP_DISK_OPEN: u8 = 0xDC;
/// `diskDelete(path)` → 0 or an error (files only).
pub(super) const OP_DISK_DELETE: u8 = 0xDD;
/// `diskMkdir(path)` → 0 or an error.
pub(super) const OP_DISK_MKDIR: u8 = 0xDE;
/// `diskRmdir(path)` → 0 or an error (empty directories only).
pub(super) const OP_DISK_RMDIR: u8 = 0xDF;
/// `diskExists(path)` → 0 none, 1 file, 2 directory, or an error.
pub(super) const OP_DISK_EXISTS: u8 = 0xE0;
/// `diskList(dir, index)` → entry name (a directory's ends in `/`), "" past the last.
pub(super) const OP_DISK_LIST: u8 = 0xE1;
/// `fileWrite(handle, byte)` → 1 or an error.
pub(super) const OP_FILE_WRITE: u8 = 0xE2;
/// `fileWriteArr(handle, arr, count)` → bytes written (low byte of each element) or an error.
pub(super) const OP_FILE_WRITE_ARR: u8 = 0xE3;
/// `fileWriteStr(handle, str)` → bytes written or an error.
pub(super) const OP_FILE_WRITE_STR: u8 = 0xE4;
/// `fileReadArr(handle, arr, count)` → bytes read into arr[0..] (0 at the end) or an error.
/// Also takes a resource file's handle.
pub(super) const OP_FILE_READ_ARR: u8 = 0xE5;

/// Bytes moved per backend call by the array / string builtins.
const CHUNK: usize = 256;

impl Vm {
    /// One of the storage opcodes.
    pub(super) fn storage_op<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        match op {
            OP_DISK_OPEN => {
                let (str_id, mode) = self.pop2();
                let path = path_of(ctx, str_id);
                let r = ctx.storage.open(ctx.objs.cur_task, &path.0[..path.1], mode);
                self.push(r);
            }
            OP_DISK_DELETE | OP_DISK_MKDIR | OP_DISK_RMDIR | OP_DISK_EXISTS => {
                let str_id = self.pop();
                let (buf, len) = path_of(ctx, str_id);
                let path = &buf[..len];
                let r = match op {
                    OP_DISK_DELETE => ctx.storage.delete(path),
                    OP_DISK_MKDIR => ctx.storage.make_dir(path),
                    OP_DISK_RMDIR => ctx.storage.remove_dir(path),
                    _ => ctx.storage.exists(path),
                };
                self.push(r);
            }
            OP_DISK_LIST => {
                let (str_id, index) = self.pop2();
                let (buf, len) = path_of(ctx, str_id);
                let mut name = [0u8; 13];
                let n = ctx.storage.entry(&buf[..len], index, &mut name).unwrap_or(0);
                match ctx.objs.str_alloc(&name[..n]) {
                    Some(id) => self.push(id as i32),
                    None => self.fail(fault::STR_ALLOC),
                }
            }
            OP_FILE_WRITE => {
                let (handle, byte) = self.pop2();
                let Some(slot) = self.storage_slot(ctx, handle) else {
                    return self.fail(fault::BAD_FILE);
                };
                let r = ctx.storage.write(slot, &[byte as u8]);
                self.push(r);
            }
            OP_FILE_WRITE_STR => {
                let (handle, str_id) = self.pop2();
                let Some(slot) = self.storage_slot(ctx, handle) else {
                    return self.fail(fault::BAD_FILE);
                };
                let data = ctx.objs.str_get(str_id as ObjPtr);
                let r = if data.is_empty() { 0 } else { ctx.storage.write(slot, data) };
                self.push(r);
            }
            OP_FILE_WRITE_ARR | OP_FILE_READ_ARR => {
                let count = self.pop();
                let (handle, arr_id) = self.pop2();
                self.file_array(ctx, op == OP_FILE_WRITE_ARR, handle, arr_id, count);
            }
            _ => self.fail(fault::BAD_OPCODE),
        }
    }

    /// The storage slot of `handle` if the running task opened it.
    pub(super) fn storage_slot<P: Platform>(&self, ctx: &Ctx<P>, handle: i32) -> Option<usize> {
        ctx.storage.slot(handle, ctx.objs.cur_task)
    }

    /// `fileReadArr` / `fileWriteArr`.
    fn file_array<P: Platform>(&mut self, ctx: &mut Ctx<P>, write: bool, handle: i32, arr_id: i32, count: i32) {
        let Some(len) = Self::find_array(ctx, arr_id).map(|a| a.len()) else {
            return self.fail(fault::BAD_ARRAY);
        };
        if count < 0 || count as usize > len {
            return self.fail(fault::BAD_OPERAND);
        }
        let count = count as usize;
        let slot = self.storage_slot(ctx, handle);
        // Reading also takes a resource file's handle (1 or 2).
        if slot.is_none() && (write || self.file_slot(handle).is_none()) {
            return self.fail(fault::BAD_FILE);
        }
        let mut buf = [0u8; CHUNK];
        let mut done = 0;
        while done < count {
            let n = (count - done).min(CHUNK);
            if write {
                let data = &ctx.objs.arr_mut(arr_id as ObjPtr).unwrap()[done..done + n];
                for (b, &v) in buf.iter_mut().zip(data) {
                    *b = v as u8;
                }
                let r = ctx.storage.write(slot.unwrap(), &buf[..n]);
                if r < 0 {
                    return self.push(r);
                }
                done += n;
            } else {
                let got = match slot {
                    Some(slot) => match ctx.storage.read(slot, &mut buf[..n]) {
                        Ok(got) => got,
                        Err(e) => return self.push(e),
                    },
                    None => self.resource_read(ctx, handle, &mut buf[..n]),
                };
                let data = &mut ctx.objs.arr_mut(arr_id as ObjPtr).unwrap()[done..done + got];
                for (v, &b) in data.iter_mut().zip(&buf[..got]) {
                    *v = b as i32;
                }
                done += got;
                if got < n {
                    break;
                }
            }
        }
        self.push(done as i32);
    }
}

/// The string `str_id` copied out of the pool (paths longer than a valid
/// one are cut, which `storage::normalize` then refuses).
fn path_of<P: Platform>(ctx: &Ctx<P>, str_id: i32) -> ([u8; PATH_MAX + 2], usize) {
    let s = ctx.objs.str_get(str_id as ObjPtr);
    let mut buf = [0u8; PATH_MAX + 2];
    let n = s.len().min(buf.len());
    buf[..n].copy_from_slice(&s[..n]);
    (buf, n)
}
