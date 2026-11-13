//! Writable storage for programs (`diskOpen`, `fileWrite`, `diskMkdir`…):
//! with `--storage DIR` a host directory (kept across runs), else a tree in
//! RAM (`ferrite_core::storage::MemStorage`, gone at exit).
//!
//! The core checks paths (8.3 components, upper case, no `..`), so a path
//! here never leaves `DIR`. A directory listing shows only names that are
//! valid paths, sorted (FAT lists them in creation order).

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use ferrite_core::storage::{Kind, MAX_OPEN, MemStorage, Mode, Path, StorageBackend, err, normalize};

pub enum SimStorage {
    Mem(MemStorage),
    Host(HostStorage),
}

impl SimStorage {
    pub fn new(dir: Option<String>) -> Self {
        match dir {
            Some(d) => {
                let root = PathBuf::from(&d);
                if let Err(e) = fs::create_dir_all(&root) {
                    eprintln!("--storage {d}: {e}; storage in RAM");
                    return Self::Mem(MemStorage::default());
                }
                println!("storage: {}", root.display());
                Self::Host(HostStorage { root, open: RefCell::new(Default::default()) })
            }
            None => Self::Mem(MemStorage::default()),
        }
    }

    fn be(&self) -> &dyn Backend {
        match self {
            Self::Mem(m) => m,
            Self::Host(h) => h,
        }
    }
}

/// `StorageBackend` without its associated const, so it can be a trait object.
trait Backend {
    fn open(&self, slot: usize, path: &Path, mode: Mode) -> Result<(), i32>;
    fn close(&self, slot: usize) -> Result<(), i32>;
    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32>;
    fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32>;
    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32>;
    fn size(&self, slot: usize) -> Result<u32, i32>;
    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32>;
    fn delete(&self, path: &Path) -> Result<(), i32>;
    fn make_dir(&self, path: &Path) -> Result<(), i32>;
    fn remove_dir(&self, path: &Path) -> Result<(), i32>;
    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32>;
}

macro_rules! forward {
    ($t:ty) => {
        impl Backend for $t {
            fn open(&self, slot: usize, path: &Path, mode: Mode) -> Result<(), i32> {
                StorageBackend::open(self, slot, path, mode)
            }
            fn close(&self, slot: usize) -> Result<(), i32> {
                StorageBackend::close(self, slot)
            }
            fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
                StorageBackend::read(self, slot, buf)
            }
            fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32> {
                StorageBackend::write(self, slot, data)
            }
            fn seek(&self, slot: usize, pos: u32) -> Result<(), i32> {
                StorageBackend::seek(self, slot, pos)
            }
            fn size(&self, slot: usize) -> Result<u32, i32> {
                StorageBackend::size(self, slot)
            }
            fn kind(&self, path: &Path) -> Result<Option<Kind>, i32> {
                StorageBackend::kind(self, path)
            }
            fn delete(&self, path: &Path) -> Result<(), i32> {
                StorageBackend::delete(self, path)
            }
            fn make_dir(&self, path: &Path) -> Result<(), i32> {
                StorageBackend::make_dir(self, path)
            }
            fn remove_dir(&self, path: &Path) -> Result<(), i32> {
                StorageBackend::remove_dir(self, path)
            }
            fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
                StorageBackend::entry(self, path, index, name)
            }
        }
    };
}
forward!(MemStorage);
forward!(HostStorage);

impl StorageBackend for SimStorage {
    fn open(&self, slot: usize, path: &Path, mode: Mode) -> Result<(), i32> {
        self.be().open(slot, path, mode)
    }
    fn close(&self, slot: usize) -> Result<(), i32> {
        self.be().close(slot)
    }
    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
        self.be().read(slot, buf)
    }
    fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32> {
        self.be().write(slot, data)
    }
    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32> {
        self.be().seek(slot, pos)
    }
    fn size(&self, slot: usize) -> Result<u32, i32> {
        self.be().size(slot)
    }
    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32> {
        self.be().kind(path)
    }
    fn delete(&self, path: &Path) -> Result<(), i32> {
        self.be().delete(path)
    }
    fn make_dir(&self, path: &Path) -> Result<(), i32> {
        self.be().make_dir(path)
    }
    fn remove_dir(&self, path: &Path) -> Result<(), i32> {
        self.be().remove_dir(path)
    }
    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
        self.be().entry(path, index, name)
    }
}

struct OpenHost {
    file: File,
    rel: String,
    write: bool,
}

pub struct HostStorage {
    root: PathBuf,
    open: RefCell<[Option<OpenHost>; MAX_OPEN]>,
}

fn io_err(e: std::io::Error) -> i32 {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => err::NOT_FOUND,
        AlreadyExists => err::EXISTS,
        DirectoryNotEmpty => err::NOT_EMPTY,
        StorageFull => err::FULL,
        IsADirectory | NotADirectory => err::WRONG_KIND,
        _ => err::IO,
    }
}

impl HostStorage {
    fn host(&self, path: &Path) -> PathBuf {
        path.components().fold(self.root.clone(), |p, c| p.join(c))
    }

    fn kind_at(&self, path: &Path) -> Option<Kind> {
        if path.is_root() {
            return Some(Kind::Dir);
        }
        let m = fs::metadata(self.host(path)).ok()?;
        Some(if m.is_dir() { Kind::Dir } else { Kind::File })
    }

    fn parent_is_dir(&self, path: &Path) -> Result<(), i32> {
        let (parent, _) = path.split_last().ok_or(err::WRONG_KIND)?;
        match self.kind_at(&normalize(parent.as_bytes()).ok_or(err::BAD_PATH)?) {
            Some(Kind::Dir) => Ok(()),
            Some(Kind::File) => Err(err::WRONG_KIND),
            None => Err(err::NOT_FOUND),
        }
    }

    fn is_open(&self, rel: &str) -> bool {
        self.open.borrow().iter().flatten().any(|o| o.rel == rel)
    }

    fn with_open<R>(&self, slot: usize, f: impl FnOnce(&mut OpenHost) -> Result<R, i32>) -> Result<R, i32> {
        let mut open = self.open.borrow_mut();
        f(open[slot].as_mut().ok_or(err::IO)?)
    }
}

impl StorageBackend for HostStorage {
    fn open(&self, slot: usize, path: &Path, mode: Mode) -> Result<(), i32> {
        self.parent_is_dir(path)?;
        if self.is_open(path.as_str()) {
            return Err(err::BUSY);
        }
        if self.kind_at(path) == Some(Kind::Dir) {
            return Err(err::WRONG_KIND);
        }
        let mut o = OpenOptions::new();
        match mode {
            Mode::Read => o.read(true),
            Mode::Write => o.read(true).write(true).create(true).truncate(true),
            Mode::Append => o.read(true).append(true).create(true),
        };
        let file = o.open(self.host(path)).map_err(io_err)?;
        self.open.borrow_mut()[slot] = Some(OpenHost { file, rel: path.as_str().into(), write: mode != Mode::Read });
        Ok(())
    }

    fn close(&self, slot: usize) -> Result<(), i32> {
        self.open.borrow_mut()[slot].take().map(|_| ()).ok_or(err::IO)
    }

    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
        self.with_open(slot, |o| o.file.read(buf).map_err(io_err))
    }

    fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32> {
        self.with_open(slot, |o| {
            if !o.write {
                return Err(err::BAD_PATH);
            }
            o.file.write_all(data).map_err(io_err)?;
            Ok(data.len())
        })
    }

    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32> {
        self.with_open(slot, |o| o.file.seek(SeekFrom::Start(pos as u64)).map(|_| ()).map_err(io_err))
    }

    fn size(&self, slot: usize) -> Result<u32, i32> {
        self.with_open(slot, |o| o.file.metadata().map(|m| m.len() as u32).map_err(io_err))
    }

    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32> {
        Ok(self.kind_at(path))
    }

    fn delete(&self, path: &Path) -> Result<(), i32> {
        match self.kind_at(path) {
            Some(Kind::File) => {}
            Some(Kind::Dir) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        if self.is_open(path.as_str()) {
            return Err(err::BUSY);
        }
        fs::remove_file(self.host(path)).map_err(io_err)
    }

    fn make_dir(&self, path: &Path) -> Result<(), i32> {
        self.parent_is_dir(path)?;
        if self.kind_at(path).is_some() {
            return Err(err::EXISTS);
        }
        fs::create_dir(self.host(path)).map_err(io_err)
    }

    fn remove_dir(&self, path: &Path) -> Result<(), i32> {
        match self.kind_at(path) {
            Some(Kind::Dir) => {}
            Some(Kind::File) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        let dir = self.host(path);
        if fs::read_dir(&dir).map_err(io_err)?.next().is_some() {
            return Err(err::NOT_EMPTY);
        }
        fs::remove_dir(dir).map_err(io_err)
    }

    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
        match self.kind_at(path) {
            Some(Kind::Dir) => {}
            Some(Kind::File) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        let mut names: Vec<(String, bool)> = fs::read_dir(self.host(path))
            .map_err(io_err)?
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().into_string().ok()?;
                // Only names a program could have made (upper-case 8.3).
                let p = normalize(n.as_bytes())?;
                (p.as_str() == n && !n.contains('/')).then(|| (n, e.path().is_dir()))
            })
            .collect();
        names.sort();
        Ok(names.get(index as usize).map(|(n, dir)| {
            name[..n.len()].copy_from_slice(n.as_bytes());
            (n.len(), *dir)
        }))
    }
}
