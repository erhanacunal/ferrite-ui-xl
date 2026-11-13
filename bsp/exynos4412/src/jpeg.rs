//! JPEG images (FI mode 3) decoded by the CPU with `zune-jpeg` (MIT / Apache-2.0 / Zlib), once,
//! when the image loads (`ImageList::find_or_load`); drawing then copies the RGB565 result.
//!
//! The SoC's own codec (HAL `exynos_4412_jpeg`) is not used: that crate is GPL-2.0-only, and
//! linking it would make the firmware GPL-2.0.
//!
//! `zune-jpeg`'s NEON code is for aarch64 only, so the A9 runs its scalar IDCT and colour
//! conversion.

extern crate alloc;

use alloc::vec;
use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::println;
use ferrite_core::jpeg::JpegBackend;
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// Pictures decoded, and the microseconds the last one took (`/api/status` `jpeg`).
pub static DECODED: AtomicU32 = AtomicU32::new(0);
pub static LAST_US: AtomicU32 = AtomicU32::new(0);
/// Streams that did not decode.
pub static FAILED: AtomicU32 = AtomicU32::new(0);

pub struct SoftJpeg;

impl JpegBackend for SoftJpeg {
    fn decode_rgb565(&self, src: &[u8], out: &mut [u16], stride: u16) -> bool {
        let begin = crate::perf::cycles();
        let ok = decode(src, out, stride as usize);
        if ok {
            DECODED.fetch_add(1, Ordering::Relaxed);
            let us = crate::perf::cycles().wrapping_sub(begin) / (crate::perf::hz() / 1_000_000);
            LAST_US.store(us, Ordering::Relaxed);
        } else {
            FAILED.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
}

fn decode(src: &[u8], out: &mut [u16], stride: usize) -> bool {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(src), options);
    if let Err(e) = dec.decode_headers() {
        println!("ferrite: jpeg header: {:?}", e);
        return false;
    }
    let Some(info) = dec.info() else {
        return false;
    };
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || stride < w || out.len() < stride * h {
        println!("ferrite: jpeg {}x{} does not fit stride {}", w, h, stride);
        return false;
    }
    let Some(size) = dec.output_buffer_size() else {
        return false;
    };
    if size != w * h * 3 {
        println!("ferrite: jpeg output {} bytes for {}x{}", size, w, h);
        return false;
    }
    let mut rgb = vec![0u8; size];
    if let Err(e) = dec.decode_into(&mut rgb) {
        println!("ferrite: jpeg decode: {:?}", e);
        return false;
    }
    for (row, line) in rgb.chunks_exact(w * 3).zip(out.chunks_mut(stride)) {
        for (px, dst) in row.chunks_exact(3).zip(line.iter_mut()) {
            let (r, g, b) = (px[0] as u16, px[1] as u16, px[2] as u16);
            *dst = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
        }
    }
    true
}
