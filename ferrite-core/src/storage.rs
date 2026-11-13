//! Files a program creates, writes and deletes, in directories it makes: the
//! board's writable storage (exynos4412: FAT32 on the eMMC; sim: a host
//! directory), as opposed to `RES_FILE` resources, which are read-only parts
//! of the image (`fileOpen`).
//!
//! A program sees one tree whose root the backend chooses (exynos4412:
//! `FERRITE/USER` on CACHE), so the firmware's own files (`FERRITE.BIN`,
//! `FLASH.BIN`, the WiFi config, the EEPROM file) are outside it and cannot
//! be read, written or deleted. Paths are checked here, before the backend
//! sees them ([`normalize`]): `/` separated, a leading `/` optional, every
//! component an 8.3 name of `A-Z 0-9 _ - ~` (lower case is folded), no `.`
//! or `..`, at most [`MAX_DEPTH`] components.
//!
//! Handles: the VM numbers open storage files [`HANDLE_BASE`].., resource
//! files 1 and 2, so `fileRead` / `fileSize` / `fileSeek` / `fileClose` take
//! either. [`StorageImpl`] keeps which task opened each slot: a task reaches
//! only its own, and its files are closed when it ends.
//!
//! Errors are negative results ([`err`]); the VM faults only on a handle that
//! is not open (as for resource files).
extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Files open at once, all tasks together.
pub const MAX_OPEN: usize = 8;
/// The VM handle of slot 0.
pub const HANDLE_BASE: i32 = 16;
/// Components in a path.
pub const MAX_DEPTH: usize = 8;
/// Bytes of a normalized path: 8 components of 12 bytes and their `/`.
pub const PATH_MAX: usize = MAX_DEPTH * 13;

/// Negative results of the storage builtins. Keep in sync with ferrite-ui
/// `lib/storage.fl` (`DISK_ERR_*`).
pub mod err {
    /// No such file or directory (or a directory on the way is missing).
    pub const NOT_FOUND: i32 = -1;
    /// It exists already (`diskMkdir`).
    pub const EXISTS: i32 = -2;
    /// Not a valid path, or the file is open for reading only.
    pub const BAD_PATH: i32 = -3;
    /// The directory is not empty (`diskRmdir`).
    pub const NOT_EMPTY: i32 = -4;
    /// No space left, or too many files open.
    pub const FULL: i32 = -5;
    /// The card or file system failed.
    pub const IO: i32 = -6;
    /// This board has no writable storage.
    pub const NO_STORAGE: i32 = -7;
    /// A file where a directory was expected, or the other way round.
    pub const WRONG_KIND: i32 = -8;
    /// The file is open (delete, or open twice).
    pub const BUSY: i32 = -9;
}

/// How `diskOpen` opens a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0: read an existing file.
    Read,
    /// 1: write a new or emptied file (created if missing, truncated if not).
    Write,
    /// 2: write at the end of a file (created if missing).
    Append,
}

impl Mode {
    pub fn from_vm(mode: i32) -> Option<Self> {
        match mode {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Append),
            _ => None,
        }
    }
}

/// What a path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

/// A checked path: upper-case 8.3 components joined by `/`, no leading `/`;
/// empty = the root.
#[derive(Clone, Copy)]
pub struct Path {
    buf: [u8; PATH_MAX],
    len: usize,
}

impl Path {
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// As text (always ASCII).
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    pub fn is_root(&self) -> bool {
        self.len == 0
    }

    /// The components, root first.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.as_str().split('/').filter(|c| !c.is_empty())
    }

    /// The parent's components and the last one (None for the root).
    pub fn split_last(&self) -> Option<(&str, &str)> {
        let s = self.as_str();
        if s.is_empty() {
            return None;
        }
        Some(match s.rfind('/') {
            Some(i) => (&s[..i], &s[i + 1..]),
            None => ("", s),
        })
    }
}

fn name_char(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'~' => Some(c),
        b'a'..=b'z' => Some(c - 32),
        _ => None,
    }
}

/// Checks `raw` (a program's string) and folds it to a [`Path`]; None = not
/// a valid path. Every component is `NAME` or `NAME.EXT`, 1–8 and 1–3
/// characters.
pub fn normalize(raw: &[u8]) -> Option<Path> {
    let mut p = Path { buf: [0; PATH_MAX], len: 0 };
    let raw = raw.strip_prefix(b"/").unwrap_or(raw);
    if raw.is_empty() {
        return Some(p);
    }
    let raw = raw.strip_suffix(b"/").unwrap_or(raw);
    for (depth, comp) in raw.split(|&c| c == b'/').enumerate() {
        if depth >= MAX_DEPTH {
            return None;
        }
        let (base, ext) = match comp.iter().position(|&c| c == b'.') {
            Some(i) => (&comp[..i], Some(&comp[i + 1..])),
            None => (comp, None),
        };
        if base.is_empty() || base.len() > 8 || ext.is_some_and(|e| e.is_empty() || e.len() > 3) {
            return None;
        }
        if depth > 0 {
            p.buf[p.len] = b'/';
            p.len += 1;
        }
        for &c in base {
            p.buf[p.len] = name_char(c)?;
            p.len += 1;
        }
        if let Some(ext) = ext {
            p.buf[p.len] = b'.';
            p.len += 1;
            for &c in ext {
                p.buf[p.len] = name_char(c)?;
                p.len += 1;
            }
        }
    }
    Some(p)
}

/// A board's writable storage. Paths are already checked and relative to
/// the backend's root, which must hold none of the firmware's own files.
/// `slot` is `0..MAX_OPEN`, chosen by [`StorageImpl`] (free when opening).
/// Errors are [`err`] codes.
pub trait StorageBackend {
    /// `false` only for [`NoStorage`].
    const AVAILABLE: bool = true;

    fn open(&self, slot: usize, path: &Path, mode: Mode) -> Result<(), i32>;
    fn close(&self, slot: usize) -> Result<(), i32>;
    /// Reads from the file's position; 0 = at the end.
    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32>;
    /// Writes at the file's position (the end for [`Mode::Append`]).
    fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32>;
    /// Moves the position to `pos` (≤ the size).
    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32>;
    fn size(&self, slot: usize) -> Result<u32, i32>;

    /// What `path` is (None = nothing there). The root is a directory.
    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32>;
    /// Deletes a file that is not open.
    fn delete(&self, path: &Path) -> Result<(), i32>;
    /// Makes a directory in an existing one.
    fn make_dir(&self, path: &Path) -> Result<(), i32>;
    /// Deletes an empty directory (not the root).
    fn remove_dir(&self, path: &Path) -> Result<(), i32>;
    /// Entry `index` of directory `path` (`.` and `..` left out): its name
    /// into `name` (length returned) and whether it is a directory; None
    /// past the last.
    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32>;

    /// Stores buffered writes. The runtime calls this once per main-loop pass
    /// (`force` false: the backend may wait for writes to settle) and before
    /// a reset (`force` true).
    fn flush(&self, _force: bool) {}
}

/// Boards without writable storage: every call answers [`err::NO_STORAGE`].
pub struct NoStorage;

impl StorageBackend for NoStorage {
    const AVAILABLE: bool = false;
    fn open(&self, _: usize, _: &Path, _: Mode) -> Result<(), i32> {
        Err(err::NO_STORAGE)
    }
    fn close(&self, _: usize) -> Result<(), i32> {
        Err(err::NO_STORAGE)
    }
    fn read(&self, _: usize, _: &mut [u8]) -> Result<usize, i32> {
        Err(err::NO_STORAGE)
    }
    fn write(&self, _: usize, _: &[u8]) -> Result<usize, i32> {
        Err(err::NO_STORAGE)
    }
    fn seek(&self, _: usize, _: u32) -> Result<(), i32> {
        Err(err::NO_STORAGE)
    }
    fn size(&self, _: usize) -> Result<u32, i32> {
        Err(err::NO_STORAGE)
    }
    fn kind(&self, _: &Path) -> Result<Option<Kind>, i32> {
        Err(err::NO_STORAGE)
    }
    fn delete(&self, _: &Path) -> Result<(), i32> {
        Err(err::NO_STORAGE)
    }
    fn make_dir(&self, _: &Path) -> Result<(), i32> {
        Err(err::NO_STORAGE)
    }
    fn remove_dir(&self, _: &Path) -> Result<(), i32> {
        Err(err::NO_STORAGE)
    }
    fn entry(&self, _: &Path, _: u32, _: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
        Err(err::NO_STORAGE)
    }
}

/// No task holds the slot.
const FREE: u8 = 0xFE;

/// The backend plus who opened which slot.
pub struct StorageImpl<B: StorageBackend> {
    be: B,
    owners: RefCell<[u8; MAX_OPEN]>,
}

impl<B: StorageBackend> StorageImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be, owners: RefCell::new([FREE; MAX_OPEN]) }
    }

    pub fn backend(&self) -> &B {
        &self.be
    }

    #[inline]
    pub fn available(&self) -> bool {
        B::AVAILABLE
    }

    /// Whether `handle` is a storage handle at all (else a resource file's).
    pub fn is_handle(handle: i32) -> bool {
        (HANDLE_BASE..HANDLE_BASE + MAX_OPEN as i32).contains(&handle)
    }

    /// The slot of `handle` if `task` opened it.
    pub fn slot(&self, handle: i32, task: u8) -> Option<usize> {
        if !Self::is_handle(handle) {
            return None;
        }
        let slot = (handle - HANDLE_BASE) as usize;
        (self.owners.borrow()[slot] == task).then_some(slot)
    }

    /// Opens `raw` for `task`: a handle, or an [`err`] code.
    pub fn open(&self, task: u8, raw: &[u8], mode: i32) -> i32 {
        let (Some(path), Some(mode)) = (normalize(raw), Mode::from_vm(mode)) else {
            return err::BAD_PATH;
        };
        if path.is_root() {
            return err::WRONG_KIND;
        }
        let Some(slot) = self.owners.borrow().iter().position(|&o| o == FREE) else {
            return err::FULL;
        };
        match self.be.open(slot, &path, mode) {
            Ok(()) => {
                self.owners.borrow_mut()[slot] = task;
                HANDLE_BASE + slot as i32
            }
            Err(e) => e,
        }
    }

    pub fn close(&self, slot: usize) -> i32 {
        self.owners.borrow_mut()[slot] = FREE;
        self.be.close(slot).map_or_else(|e| e, |()| 0)
    }

    /// Closes every file `task` has open (it ended or was reset).
    pub fn close_task(&self, task: u8) {
        for slot in 0..MAX_OPEN {
            if self.owners.borrow()[slot] == task {
                let _ = self.close(slot);
            }
        }
    }

    /// Closes every open file (the main program restarts: no task survives).
    pub fn close_all(&self) {
        for slot in 0..MAX_OPEN {
            if self.owners.borrow()[slot] != FREE {
                let _ = self.close(slot);
            }
        }
    }

    pub fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
        self.be.read(slot, buf)
    }

    /// Writes all of `data`; the bytes written, or an [`err`] code.
    pub fn write(&self, slot: usize, data: &[u8]) -> i32 {
        match self.be.write(slot, data) {
            Ok(n) => n as i32,
            Err(e) => e,
        }
    }

    pub fn seek(&self, slot: usize, pos: i32) -> Result<(), i32> {
        let size = self.be.size(slot)?;
        self.be.seek(slot, (pos.max(0) as u32).min(size))
    }

    pub fn size(&self, slot: usize) -> Result<u32, i32> {
        self.be.size(slot)
    }

    /// 0 nothing there, 1 a file, 2 a directory, or an [`err`] code.
    pub fn exists(&self, raw: &[u8]) -> i32 {
        let Some(path) = normalize(raw) else {
            return err::BAD_PATH;
        };
        match self.be.kind(&path) {
            Ok(None) => 0,
            Ok(Some(Kind::File)) => 1,
            Ok(Some(Kind::Dir)) => 2,
            Err(e) => e,
        }
    }

    fn with_path(raw: &[u8], root_ok: bool, f: impl FnOnce(&Path) -> Result<(), i32>) -> i32 {
        match normalize(raw) {
            Some(p) if root_ok || !p.is_root() => f(&p).map_or_else(|e| e, |()| 0),
            Some(_) => err::WRONG_KIND,
            None => err::BAD_PATH,
        }
    }

    pub fn delete(&self, raw: &[u8]) -> i32 {
        Self::with_path(raw, false, |p| self.be.delete(p))
    }

    pub fn make_dir(&self, raw: &[u8]) -> i32 {
        Self::with_path(raw, false, |p| self.be.make_dir(p))
    }

    pub fn remove_dir(&self, raw: &[u8]) -> i32 {
        Self::with_path(raw, false, |p| self.be.remove_dir(p))
    }

    /// Entry `index` of directory `raw` as a name, a directory's ending in
    /// `/`; None past the last or on any error.
    pub fn entry(&self, raw: &[u8], index: i32, out: &mut [u8; 13]) -> Option<usize> {
        let path = normalize(raw)?;
        let mut name = [0u8; 12];
        let (n, dir) = self.be.entry(&path, u32::try_from(index).ok()?, &mut name).ok()??;
        out[..n].copy_from_slice(&name[..n]);
        if dir {
            out[n] = b'/';
            return Some(n + 1);
        }
        Some(n)
    }

    pub fn flush(&self, force: bool) {
        self.be.flush(force);
    }
}

#[cfg(feature = "mock")]
pub type Storage = StorageImpl<MemStorage>;

// ---------------------------------------------------------------------------
// In RAM: tests, and the sim without `--storage DIR`.
// ---------------------------------------------------------------------------

enum Node {
    File(Vec<u8>),
    Dir,
}

struct OpenMem {
    path: Vec<u8>,
    pos: usize,
    write: bool,
}

/// A storage tree in RAM, gone when dropped. Behaves like the FAT backend:
/// one open handle a file, empty directories only are removed.
#[derive(Default)]
pub struct MemStorage {
    nodes: RefCell<BTreeMap<Vec<u8>, Node>>,
    open: RefCell<[Option<OpenMem>; MAX_OPEN]>,
}

impl MemStorage {
    fn parent_is_dir(&self, path: &Path) -> Result<(), i32> {
        let (parent, _) = path.split_last().ok_or(err::WRONG_KIND)?;
        if parent.is_empty() {
            return Ok(());
        }
        match self.nodes.borrow().get(parent.as_bytes()) {
            Some(Node::Dir) => Ok(()),
            Some(Node::File(_)) => Err(err::WRONG_KIND),
            None => Err(err::NOT_FOUND),
        }
    }

    fn is_open(&self, path: &[u8]) -> bool {
        self.open.borrow().iter().flatten().any(|o| o.path == path)
    }

    fn with_open<R>(&self, slot: usize, f: impl FnOnce(&mut OpenMem, &mut Vec<u8>) -> R) -> Result<R, i32> {
        let mut open = self.open.borrow_mut();
        let o = open[slot].as_mut().ok_or(err::IO)?;
        let mut nodes = self.nodes.borrow_mut();
        match nodes.get_mut(&o.path) {
            Some(Node::File(data)) => Ok(f(o, data)),
            _ => Err(err::IO),
        }
    }
}

impl StorageBackend for MemStorage {
    fn open(&self, slot: usize, path: &Path, mode: Mode) -> Result<(), i32> {
        self.parent_is_dir(path)?;
        let key = path.as_bytes().to_vec();
        if self.is_open(&key) {
            return Err(err::BUSY);
        }
        let mut nodes = self.nodes.borrow_mut();
        let pos = match (nodes.get_mut(&key), mode) {
            (Some(Node::Dir), _) => return Err(err::WRONG_KIND),
            (None, Mode::Read) => return Err(err::NOT_FOUND),
            (Some(Node::File(_)), Mode::Read) => 0,
            (Some(Node::File(d)), Mode::Write) => {
                d.clear();
                0
            }
            (Some(Node::File(d)), Mode::Append) => d.len(),
            (None, _) => {
                nodes.insert(key.clone(), Node::File(Vec::new()));
                0
            }
        };
        self.open.borrow_mut()[slot] = Some(OpenMem { path: key, pos, write: mode != Mode::Read });
        Ok(())
    }

    fn close(&self, slot: usize) -> Result<(), i32> {
        self.open.borrow_mut()[slot].take().map(|_| ()).ok_or(err::IO)
    }

    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
        self.with_open(slot, |o, data| {
            let n = buf.len().min(data.len().saturating_sub(o.pos));
            buf[..n].copy_from_slice(&data[o.pos..o.pos + n]);
            o.pos += n;
            n
        })
    }

    fn write(&self, slot: usize, src: &[u8]) -> Result<usize, i32> {
        self.with_open(slot, |o, data| {
            if !o.write {
                return Err(err::BAD_PATH);
            }
            let end = o.pos + src.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[o.pos..end].copy_from_slice(src);
            o.pos = end;
            Ok(src.len())
        })?
    }

    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32> {
        self.with_open(slot, |o, data| o.pos = (pos as usize).min(data.len()))
    }

    fn size(&self, slot: usize) -> Result<u32, i32> {
        self.with_open(slot, |_, data| data.len() as u32)
    }

    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32> {
        if path.is_root() {
            return Ok(Some(Kind::Dir));
        }
        Ok(self.nodes.borrow().get(path.as_bytes()).map(|n| match n {
            Node::File(_) => Kind::File,
            Node::Dir => Kind::Dir,
        }))
    }

    fn delete(&self, path: &Path) -> Result<(), i32> {
        let key = path.as_bytes();
        match self.nodes.borrow().get(key) {
            Some(Node::File(_)) => {}
            Some(Node::Dir) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        if self.is_open(key) {
            return Err(err::BUSY);
        }
        self.nodes.borrow_mut().remove(key);
        Ok(())
    }

    fn make_dir(&self, path: &Path) -> Result<(), i32> {
        self.parent_is_dir(path)?;
        let mut nodes = self.nodes.borrow_mut();
        if nodes.contains_key(path.as_bytes()) {
            return Err(err::EXISTS);
        }
        nodes.insert(path.as_bytes().to_vec(), Node::Dir);
        Ok(())
    }

    fn remove_dir(&self, path: &Path) -> Result<(), i32> {
        let key = path.as_bytes();
        let mut nodes = self.nodes.borrow_mut();
        match nodes.get(key) {
            Some(Node::Dir) => {}
            Some(Node::File(_)) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        let mut prefix = key.to_vec();
        prefix.push(b'/');
        if nodes.keys().any(|k| k.starts_with(&prefix)) {
            return Err(err::NOT_EMPTY);
        }
        nodes.remove(key);
        Ok(())
    }

    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
        match self.kind(path)? {
            Some(Kind::Dir) => {}
            Some(Kind::File) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        let mut prefix = path.as_bytes().to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let nodes = self.nodes.borrow();
        let found = nodes
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix) && !k[prefix.len()..].contains(&b'/'))
            .nth(index as usize);
        Ok(found.map(|(k, n)| {
            let leaf = &k[prefix.len()..];
            name[..leaf.len()].copy_from_slice(leaf);
            (leaf.len(), matches!(n, Node::Dir))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_checked_and_folded() {
        let ok = |s: &str| normalize(s.as_bytes()).map(|p| alloc::string::String::from(p.as_str()));
        assert_eq!(ok("/").as_deref(), Some(""));
        assert_eq!(ok("").as_deref(), Some(""));
        assert_eq!(ok("/logs/day1.txt").as_deref(), Some("LOGS/DAY1.TXT"));
        assert_eq!(ok("a/b/").as_deref(), Some("A/B"));
        assert_eq!(ok("save_1-x~"), None, "9 characters");
        for bad in ["..", "a/../b", "a//b", ".", "a/.b", "a.", "toolongname.txt", "a.text", "a b", "a\\b", "x.y.z"] {
            assert_eq!(ok(bad), None, "{bad}");
        }
        assert!(ok("1/2/3/4/5/6/7/8").is_some());
        assert_eq!(ok("1/2/3/4/5/6/7/8/9"), None);
    }

    #[test]
    fn handles_belong_to_the_task_that_opened_them() {
        let s = StorageImpl::with_backend(MemStorage::default());
        assert_eq!(s.make_dir(b"logs"), 0);
        assert_eq!(s.make_dir(b"logs"), err::EXISTS);
        let h = s.open(3, b"logs/a.txt", 1);
        assert_eq!(h, HANDLE_BASE);
        assert_eq!(s.slot(h, 0), None);
        let slot = s.slot(h, 3).unwrap();
        assert_eq!(s.write(slot, b"hello"), 5);
        assert_eq!(s.open(0, b"logs/a.txt", 0), err::BUSY);
        assert_eq!(s.delete(b"logs/a.txt"), err::BUSY);
        s.close_task(3);
        assert_eq!(s.slot(h, 3), None);
        assert_eq!(s.remove_dir(b"logs"), err::NOT_EMPTY);
        let h = s.open(0, b"logs/a.txt", 2);
        let slot = s.slot(h, 0).unwrap();
        assert_eq!(s.write(slot, b"!"), 1);
        assert_eq!(s.size(slot), Ok(6));
        s.close_all();
        let mut name = [0u8; 13];
        let n = s.entry(b"/", 0, &mut name).unwrap();
        assert_eq!(&name[..n], b"LOGS/");
        let n = s.entry(b"logs", 0, &mut name).unwrap();
        assert_eq!(&name[..n], b"A.TXT");
        assert_eq!(s.entry(b"logs", 1, &mut name), None);
        assert_eq!(s.delete(b"logs/a.txt"), 0);
        assert_eq!(s.remove_dir(b"logs"), 0);
        assert_eq!(s.exists(b"logs"), 0);
        assert_eq!(s.open(0, b"none/a.txt", 1), err::NOT_FOUND);
        assert_eq!(s.open(0, b"/", 0), err::WRONG_KIND);
    }
}
