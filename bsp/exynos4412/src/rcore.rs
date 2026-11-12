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
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering};

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
pub static BUSY_TICKS: AtomicU64 = AtomicU64::new(0);
pub static MAX_TICKS: AtomicU32 = AtomicU32::new(0);
/// Inside the jobs: CPU2's waits for a present copy (`lcd::wait_rows`: count
/// and ticks) and its L1 clean after each job.
static WAITS: AtomicU32 = AtomicU32::new(0);
static WAIT_TICKS: AtomicU64 = AtomicU64::new(0);
static CLEAN_TICKS: AtomicU64 = AtomicU64::new(0);
/// Around the jobs, on CPU0: its L1 clean before each job, and how long a
/// finished job waited for CPU0 to take it back and present (`render_poll`;
/// total and longest).
static START_CLEAN_TICKS: AtomicU64 = AtomicU64::new(0);
static DONE_AT: AtomicU32 = AtomicU32::new(0);
static PICKUP_TICKS: AtomicU64 = AtomicU64::new(0);
static PICKUP_MAX: AtomicU32 = AtomicU32::new(0);
/// `render::JOB_TICKS` and the compositor's `TIMES` at the last `take_stats`.
static LAST_JOB: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static LAST_COMPOSE: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

/// A render core's wait for a present copy (`lcd::wait_rows`, not on CPU0).
pub fn add_wait(ticks: u32) {
    WAITS.fetch_add(1, Ordering::Relaxed);
    WAIT_TICKS.fetch_add(ticks as u64, Ordering::Relaxed);
}

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
        let t1 = crate::perf::cycles();
        mmu::clean_l1_all();
        let t2 = crate::perf::cycles();
        let dt = t2.wrapping_sub(t0);
        JOBS.fetch_add(1, Ordering::Relaxed);
        BUSY_TICKS.fetch_add(dt as u64, Ordering::Relaxed);
        MAX_TICKS.fetch_max(dt, Ordering::Relaxed);
        CLEAN_TICKS.fetch_add(t2.wrapping_sub(t1) as u64, Ordering::Relaxed);
        DONE_AT.store(t2, Ordering::Relaxed);
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
    let t0 = crate::perf::cycles();
    mmu::clean_l1_all();
    START_CLEAN_TICKS.fetch_add(crate::perf::cycles().wrapping_sub(t0) as u64, Ordering::Relaxed);
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
    let waited = crate::perf::cycles().wrapping_sub(DONE_AT.load(Ordering::Relaxed));
    PICKUP_TICKS.fetch_add(waited as u64, Ordering::Relaxed);
    PICKUP_MAX.fetch_max(waited, Ordering::Relaxed);
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

/// The render core since the last [`take_stats`], times in µs.
pub struct Stats {
    /// Jobs run on CPU2, their total and longest time.
    pub jobs: u32,
    pub busy_us: u32,
    pub max_us: u32,
    /// Inside the jobs (`render::run_job`, on whichever core): the VM's
    /// lists before the render, the render, the `on_paint` lists after it.
    pub lists_us: u32,
    pub render_us: u32,
    pub paints_us: u32,
    /// Inside the render: the GPU compositor (whole `compose` calls), of
    /// that gathering the pieces (CPU sprites included), its frames, and of
    /// those the GP and the PPs.
    pub compose_us: u32,
    pub collect_us: u32,
    pub frames: u32,
    pub frame_us: u32,
    pub gp_us: u32,
    pub pp_us: u32,
    /// CPU2's waits for a present copy (inside the render) and its L1 cleans
    /// (after it).
    pub waits: u32,
    pub wait_us: u32,
    pub clean_us: u32,
    /// CPU0: its L1 cleans before the jobs; finished jobs waiting to be
    /// taken back (total, longest).
    pub start_clean_us: u32,
    pub pickup_us: u32,
    pub pickup_max_us: u32,
}

/// For `/api/status`: the counters since the last call.
pub fn take_stats() -> Stats {
    let hz = crate::perf::hz().max(1) as u64;
    let us = |t: u64| (t * 1_000_000 / hz) as u32;
    let take = |a: &AtomicU64| us(a.swap(0, Ordering::Relaxed));
    // Free-running counters elsewhere: the difference to the last call.
    let since = |now: u64, last: &AtomicU64| now.wrapping_sub(last.swap(now, Ordering::Relaxed));
    let job = |i: usize| us(since(render::JOB_TICKS[i].load(Ordering::Relaxed), &LAST_JOB[i]));
    // Slots of `compose::TIME_NAMES`.
    let compose = |i: usize| since(crate::compose::TIMES[i].load(Ordering::Relaxed), &LAST_COMPOSE[i]) as u32;
    Stats {
        jobs: JOBS.swap(0, Ordering::Relaxed),
        busy_us: take(&BUSY_TICKS),
        max_us: us(MAX_TICKS.swap(0, Ordering::Relaxed) as u64),
        lists_us: job(render::JOB_LISTS),
        render_us: job(render::JOB_RENDER),
        paints_us: job(render::JOB_PAINTS),
        compose_us: compose(8),
        collect_us: compose(9),
        frames: compose(0),
        frame_us: compose(7),
        gp_us: compose(4),
        pp_us: compose(5),
        waits: WAITS.swap(0, Ordering::Relaxed),
        wait_us: take(&WAIT_TICKS),
        clean_us: take(&CLEAN_TICKS),
        start_clean_us: take(&START_CLEAN_TICKS),
        pickup_us: take(&PICKUP_TICKS),
        pickup_max_us: us(PICKUP_MAX.swap(0, Ordering::Relaxed) as u64),
    }
}
