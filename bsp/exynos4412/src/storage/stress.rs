//! Power-loss testing on USERDATA: a write load to cut the power under, and a check for after.
//!
//! [`set`] starts or stops the load (never across a restart); [`step`] runs operations for
//! 40 ms of each main-loop pass, on CPU0, in `/stress/` (outside `FERRITE/` and programs' reach):
//! - rewrites `f0.bin`…`f7.bin` the safe way (`ext4::save_in`: `NAME.new`, rename, commit),
//!   4 KiB to 260 KiB each, every one self-checking (header: magic, sequence, length, CRC-32);
//! - appends 32-byte records to `log.bin`, committing every fourth (as a program's writes);
//! - makes and deletes small files (directory and bitmap churn), left to the next commit;
//! - every 50th step rewrites `big.bin`, 4 MiB (a transaction committed in pieces).
//!
//! [`check`] (after the restart): `Ext4::check` over the whole file system, then every stress
//! file must be whole: a power loss may only lose what was not committed yet.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use exynos_4412_ext4::ext4_rs::Error as E;
use ferrite_core::fs::crc32_update;

use super::ext4;

const DIR: &str = "STRESS";
const FILE_MAGIC: &[u8; 4] = b"FSTR";
const LOG_MAGIC: &[u8; 4] = b"FLOG";
const RECORD: usize = 32;
const HEADER: usize = 16;
const FILES: u32 = 8;
const BIG: usize = 4 << 20;

static mut ON: bool = false;
static mut SEQ: u32 = 0;
static mut APPENDED: u32 = 0;

fn crc(data: &[u8]) -> u32 {
    !crc32_update(!0, data)
}

/// Deterministic bytes for sequence `seq`.
fn fill(seq: u32, out: &mut [u8]) {
    let mut x = (seq as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for b in out.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
}

/// A self-checking file of `len` bytes for sequence `seq`.
fn file(seq: u32, len: usize) -> Vec<u8> {
    let mut data = alloc::vec![0u8; len.max(HEADER)];
    fill(seq, &mut data[HEADER..]);
    let c = crc(&data[HEADER..]);
    data[..4].copy_from_slice(FILE_MAGIC);
    data[4..8].copy_from_slice(&seq.to_le_bytes());
    let len = data.len() as u32;
    data[8..12].copy_from_slice(&len.to_le_bytes());
    data[12..16].copy_from_slice(&c.to_le_bytes());
    data
}

/// Checks a stress file: its sequence, or why it is bad.
fn file_ok(data: &[u8]) -> Result<u32, &'static str> {
    if data.len() < HEADER || &data[..4] != FILE_MAGIC {
        return Err("no header");
    }
    let word = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    if word(8) as usize != data.len() {
        return Err("length");
    }
    if word(12) != crc(&data[HEADER..]) {
        return Err("CRC");
    }
    Ok(word(4))
}

fn record(seq: u32) -> [u8; RECORD] {
    let mut r = [0u8; RECORD];
    r[..4].copy_from_slice(LOG_MAGIC);
    r[4..8].copy_from_slice(&seq.to_le_bytes());
    fill(seq, &mut r[8..28]);
    let c = crc(&r[..28]);
    r[28..].copy_from_slice(&c.to_le_bytes());
    r
}

/// Starts or stops the load. Needs USERDATA.
pub fn set(on: bool) -> Result<String, String> {
    if on && !ext4::available() {
        return Err("stress needs USERDATA\n".into());
    }
    if on && unsafe { !ON } {
        // Carry on from the log's last record, so its sequence keeps rising.
        let last = ext4::read_in(DIR, "LOG.BIN", 64 << 20)
            .and_then(|log| log.chunks_exact(RECORD).last().map(|r| u32::from_le_bytes(r[4..8].try_into().unwrap())))
            .unwrap_or(0);
        unsafe { SEQ = last.wrapping_add(1000) };
        ext4::make_dirs(DIR);
    }
    unsafe { ON = on };
    Ok(format!("stress {} (sequence {})\n", if on { "on" } else { "off" }, unsafe { SEQ }))
}

/// The load's share of each pass: steps run back to back for this long, so the card is busy
/// nearly all the time and a power cut likely lands inside a write or a commit.
const PASS_MS: u32 = 40;

/// Operations of the load for [`PASS_MS`], when it is on. CPU0, once per pass.
pub fn step() {
    let start = crate::systick::millis();
    while unsafe { ON } && crate::systick::millis().wrapping_sub(start) < PASS_MS {
        one();
    }
}

fn one() {
    let seq = unsafe {
        SEQ += 1;
        SEQ
    };
    let r = match seq % 4 {
        _ if seq % 50 == 0 => ext4::save_in(DIR, "BIG.BIN", &file(seq, BIG)).then_some(()).ok_or(E::Io),
        0 | 1 => {
            let len = 4096 * (1 + (seq.wrapping_mul(2_654_435_761) >> 26) as usize) + (seq as usize * 977) % 4096;
            ext4::save_in(DIR, &format!("F{}.BIN", (seq / 4 * 2 + seq % 4) % FILES), &file(seq, len)).then_some(()).ok_or(E::Io)
        }
        2 => {
            let commit = unsafe {
                APPENDED += 1;
                APPENDED % 4 == 0
            };
            ext4::append(DIR, "LOG.BIN", &record(seq), commit)
        }
        _ => {
            // Eight names in turn: each step makes one and deletes the one made four steps ago.
            let k = seq / 4;
            let made = ext4::write_new(DIR, &format!("T{}.TMP", k % 8), &file(seq, 100 + (seq as usize % 3000)));
            made.and_then(|()| ext4::remove(DIR, &format!("T{}.TMP", (k + 4) % 8)))
        }
    };
    if let Err(e) = r {
        exynos_4412_rs::println!("ferrite: stress step {} ERROR {}; stopping", seq, e);
        unsafe { ON = false };
    }
}

/// The whole file system checked, then every stress file. The text is the report; `Ok` when
/// nothing is wrong.
pub fn check() -> Result<String, String> {
    let mut s = String::new();
    let (unclean, replayed) = ext4::mount_state().ok_or("check needs USERDATA\n")?;
    let _ = writeln!(s, "mount: unclean={} replayed={}", unclean, replayed);
    let report = ext4::check().map_err(|e| format!("{}check failed: {}\n", s, e))?;
    let _ = writeln!(
        s,
        "fs: {} dirs, {} files, {} bytes, {} blocks and {} inodes used",
        report.dirs, report.files, report.bytes, report.used_blocks, report.used_inodes
    );
    let mut bad = report.problems.len();
    for p in &report.problems {
        let _ = writeln!(s, "PROBLEM {}", p);
    }
    let names = ext4::list_dir(DIR).unwrap_or_default();
    let (mut files, mut temporaries) = (0, 0);
    for (name, _) in &names {
        let upper = name.to_ascii_uppercase();
        let Some(data) = ext4::read_in(DIR, &upper, 64 << 20) else {
            let _ = writeln!(s, "PROBLEM {}: unreadable", name);
            bad += 1;
            continue;
        };
        if upper == "LOG.BIN" {
            if data.len() % RECORD != 0 {
                let _ = writeln!(s, "PROBLEM log.bin: {} bytes, not whole records", data.len());
                bad += 1;
            }
            let mut last = None;
            for (i, r) in data.chunks_exact(RECORD).enumerate() {
                let seq = u32::from_le_bytes(r[4..8].try_into().unwrap());
                if r[..] != record(seq)[..] {
                    let _ = writeln!(s, "PROBLEM log.bin: record {} damaged", i);
                    bad += 1;
                    break;
                }
                if last.is_some_and(|l| seq <= l) {
                    let _ = writeln!(s, "PROBLEM log.bin: record {} out of order", i);
                    bad += 1;
                    break;
                }
                last = Some(seq);
            }
            let _ = writeln!(s, "log.bin: {} records, last sequence {:?}", data.len() / RECORD, last);
        } else if upper.ends_with(".NEW") {
            // A save cut before its rename: anything may be in it.
            temporaries += 1;
        } else if upper.ends_with(".TMP") && data.is_empty() {
            // Made, and committed by another step before its data was.
            files += 1;
        } else {
            match file_ok(&data) {
                Ok(_) => files += 1,
                Err(why) => {
                    let _ = writeln!(s, "PROBLEM {}: {} ({} bytes)", name, why, data.len());
                    bad += 1;
                }
            }
        }
    }
    let _ = writeln!(s, "stress files: {} whole, {} temporaries left by a cut save", files, temporaries);
    let _ = writeln!(s, "{}", if bad == 0 { "OK" } else { "FAILED" });
    if bad == 0 { Ok(s) } else { Err(s) }
}
