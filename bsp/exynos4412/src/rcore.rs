//! The render core (plan step 3c): CPU2 runs the render jobs
//! (`ferrite_core::render::run_job`) on the render context the runtime hands
//! over (`Platform::render_start`), while CPU0 goes on with the VM.
//!
//! Hand-over: `STATE` IDLE → BUSY (CPU0 stores the context and the job,
//! `sev`) → DONE (CPU2, `sev`) → IDLE (CPU0 takes the context back,
//! `render_poll`). While BUSY the screen is CPU2's: CPU0 draws nothing and
//! presents nothing (`frame_end` skips `lcd::flush`; the core waits with
//! `render::wait_idle` before VM drawing, `on_paint`, the keyboard, error
//! screens). The present stays CPU0's (`lcd::flush` + the G2D interrupt);
//! a present still copying when a job starts is waited for by CPU2's drawing
//! (`lcd::fence`), which leaves the band ends to CPU0's interrupt.
//!
//! Caches: the cores' L1 caches are coherent with each other, but cleans by
//! address or by set/way only reach the cleaning core's L1, and the G2D,
//! GPU and FIMD read DRAM. So each core cleans its whole L1 when it hands
//! the screen over: CPU0 before a job (capture sprites, flash writes), CPU2
//! after one (layer surfaces, sprites, textures drawn there).
//!
//! No interrupts on CPU2 (as the old CPU2 presenter): `wfe` until CPU0's
//! `sev`. `POST /api/rcore?on=0|1` switches it at run time (`ENABLED`; off =
//! the jobs run on CPU0 as before).

extern crate alloc;

use alloc::boxed::Box;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, Ordering};

use exynos_4412_rs::{mmu, smp};
use ferrite_core::ctx::Ctx;
use ferrite_core::render::{self, RenderJob};

use crate::platform::Exynos4412Platform as P;

const IDLE: u8 = 0;
const BUSY: u8 = 1;
const DONE: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(IDLE);
static CTX: AtomicPtr<Ctx<P>> = AtomicPtr::new(null_mut());
static JOB: AtomicU8 = AtomicU8::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Jobs go to CPU2 (`POST /api/rcore?on=`); off = CPU0 runs them.
pub static ENABLED: AtomicBool = AtomicBool::new(true);

/// Jobs run on CPU2, and the time they took (profiler clock ticks).
pub static JOBS: AtomicU32 = AtomicU32::new(0);
pub static BUSY_TICKS: AtomicU32 = AtomicU32::new(0);
pub static MAX_TICKS: AtomicU32 = AtomicU32::new(0);

/// Starts CPU2 as the render core (CPU0, at boot). Not fatal: the jobs then
/// run on CPU0.
pub fn start() -> Result<(), smp::StartError> {
    smp::start_cpu(2, render_core)?;
    RUNNING.store(true, Ordering::Release);
    Ok(())
}

/// CPU2 runs the jobs.
pub fn running() -> bool {
    RUNNING.load(Ordering::Acquire) && ENABLED.load(Ordering::Relaxed)
}

fn render_core(_cpu: usize) -> ! {
    loop {
        while STATE.load(Ordering::Acquire) != BUSY {
            // SAFETY: sleeps until an event (CPU0's `sev`); a spurious wake
            // just checks again.
            unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) };
        }
        let t0 = crate::perf::cycles();
        // SAFETY: CPU0 stored a live context before BUSY (Release) and does
        // not touch it until DONE.
        let r = unsafe { &mut *CTX.load(Ordering::Relaxed) };
        let job = if JOB.load(Ordering::Relaxed) == 0 { RenderJob::Dirty } else { RenderJob::All };
        render::run_job(r, job);
        // What this core drew in cached memory reaches L2/DRAM for the
        // devices CPU0 drives next (layer copies, present cleans).
        mmu::clean_l1_all();
        let dt = crate::perf::cycles().wrapping_sub(t0);
        JOBS.fetch_add(1, Ordering::Relaxed);
        BUSY_TICKS.fetch_add(dt, Ordering::Relaxed);
        MAX_TICKS.fetch_max(dt, Ordering::Relaxed);
        STATE.store(DONE, Ordering::Release);
        // SAFETY: wakes CPU0 if it waits in `wait`.
        unsafe { core::arch::asm!("dsb", "sev", options(nostack, preserves_flags)) };
    }
}

/// `Platform::render_start`.
pub fn render_start(r: Box<Ctx<P>>, job: RenderJob) -> Result<(), Box<Ctx<P>>> {
    if !running() || STATE.load(Ordering::Acquire) != IDLE {
        return Err(r);
    }
    // What CPU0 wrote in cached memory that a device may read during the job.
    mmu::clean_l1_all();
    CTX.store(Box::into_raw(r), Ordering::Relaxed);
    JOB.store(if job == RenderJob::Dirty { 0 } else { 1 }, Ordering::Relaxed);
    STATE.store(BUSY, Ordering::Release);
    // SAFETY: wakes CPU2 from its `wfe`.
    unsafe { core::arch::asm!("dsb", "sev", options(nostack, preserves_flags)) };
    Ok(())
}

/// `Platform::render_poll`: also presents what the job drew at once (the
/// runtime may hand the next job over before this pass's `frame_end`, and
/// `lcd::render_due` holds it back only once a present is in flight).
pub fn render_poll() -> Option<Box<Ctx<P>>> {
    if STATE.load(Ordering::Acquire) != DONE {
        return None;
    }
    let p = CTX.swap(null_mut(), Ordering::Relaxed);
    STATE.store(IDLE, Ordering::Release);
    crate::lcd::flush();
    // SAFETY: from `Box::into_raw` in `render_start`; CPU2 is done with it.
    Some(unsafe { Box::from_raw(p) })
}

/// `Platform::render_wait`: until CPU2's `sev` (or any event).
pub fn render_wait() {
    // SAFETY: an event or interrupt ends it; the caller checks again.
    unsafe { core::arch::asm!("wfe", options(nomem, nostack, preserves_flags)) };
}

/// Jobs, total and longest job (µs) since the last call, for `/api/status`.
pub fn take_stats() -> (u32, u32, u32) {
    let hz = crate::perf::hz().max(1) as u64;
    let us = |t: u32| (t as u64 * 1_000_000 / hz) as u32;
    (
        JOBS.swap(0, Ordering::Relaxed),
        us(BUSY_TICKS.swap(0, Ordering::Relaxed)),
        us(MAX_TICKS.swap(0, Ordering::Relaxed)),
    )
}
