//! Touch backend — Atmel mXT1664S (T9 multitouch) on I2C5.
//!
//! Every finger down is reported (T9 touch ids 0..15 as the pointer ids) for
//! ferrite-core's gesture recognizers; `read_screen` is the pointer finger: the
//! first one down, until it lifts (then the next finger that moves).
//!
//! Two ways to read the controller:
//! * **Interrupt-driven (the usual one):** [`run`], a task on CPU1 next to
//!   UART RX, sleeps on the CHG line's interrupt (low while the controller
//!   holds messages), reads each message with an async I2C transfer (~1.3 ms
//!   at 100 kHz, byte by byte on the bus's interrupt) and queues it with the
//!   time it came ([`TouchBackend::read_samples`]): CPU0 spends no time on the
//!   bus, and every move reaches the gesture recognizers with its own time
//!   instead of one position per main-loop pass. CPU0's idle sleep wakes when
//!   something is queued ([`sleep_until_touch`]).
//! * **Polled (fallback):** without CPU1, CPU0 drains the messages itself on
//!   every poll while CHG is low, as before.
//!
//! Positions come in T9's range (`x_range`/`y_range` from the controller) and
//! are scaled to the panel; `CalParams` only contributes swap/flip, so a
//! capacitive panel needs no calibration.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use core::task::{Poll, Waker};

use critical_section::Mutex;
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::gpio::Pull;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::mxt::{self, Message, Mxt, Touch};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::eint::EintInput;
use ferrite_core::touch::{CalParams, SampleKind, TouchBackend, TouchPoint, TouchSample};

use crate::lcd::PANEL;

/// T9 reports up to 16 touch ids (the low 4 bits of a report id).
const FINGERS: usize = 16;
/// No pointer finger ([`POINTER`]).
const NO_POINTER: u8 = 0xFF;

/// The controller, set once at boot before CPU1 starts; then read-only, except that its bus
/// is used by one side at a time ([`QUEUED`]).
struct State {
    mxt: Mxt,
    x_range: u16,
    y_range: u16,
}

static mut STATE: Option<State> = None;

fn state() -> Option<&'static State> {
    // SAFETY: written once in `init`, before any reader runs.
    unsafe { (*(&raw const STATE)).as_ref() }
}

// --- What the reading side knows (either core), for `read_screen` / `read_points` and resyncs ---

/// Bit n = T9 touch n is down.
static DOWN: AtomicU32 = AtomicU32::new(0);
/// Raw position of each touch id, x << 16 | y.
static AT: [AtomicU32; FINGERS] = [const { AtomicU32::new(0) }; FINGERS];
/// T9 id of the pointer finger, [`NO_POINTER`] while none is down.
static POINTER: AtomicU8 = AtomicU8::new(NO_POINTER);

/// A T9 touch message: updates what is down and where. Returns what it was for
/// the finger (`None` for a release of a finger that was not down).
fn record(t: &Touch) -> Option<SampleKind> {
    let id = t.id & 15;
    let bit = 1u32 << id;
    if t.down() {
        AT[usize::from(id)].store(u32::from(t.x) << 16 | u32::from(t.y), Ordering::Relaxed);
        let was = DOWN.fetch_or(bit, Ordering::AcqRel);
        let _ = POINTER.compare_exchange(NO_POINTER, id, Ordering::AcqRel, Ordering::Relaxed);
        Some(if was & bit != 0 { SampleKind::Move } else { SampleKind::Down })
    } else {
        let was = DOWN.fetch_and(!bit, Ordering::AcqRel);
        let _ = POINTER.compare_exchange(id, NO_POINTER, Ordering::AcqRel, Ordering::Relaxed);
        (was & bit != 0).then_some(SampleKind::Up)
    }
}

fn raw_at(id: usize) -> (u16, u16) {
    let v = AT[id].load(Ordering::Relaxed);
    ((v >> 16) as u16, v as u16)
}

// --- The queue CPU1's task fills and CPU0 empties ---

/// Samples are queued by [`run`] (CPU1); false = CPU0 polls the controller.
static QUEUED: AtomicBool = AtomicBool::new(false);

const RING: usize = 256;
const MASK: usize = RING - 1;
/// Slots a move may not take, kept for downs and ups: a finger's down or up is lost only after
/// this many of them wait unread.
const RESERVE: usize = 32;

#[derive(Clone, Copy)]
struct Entry {
    kind: SampleKind,
    id: u8,
    x: u16,
    y: u16,
    ms: u32,
}

static mut BUF: [Entry; RING] = [Entry { kind: SampleKind::Down, id: 0, x: 0, y: 0, ms: 0 }; RING];
// Single producer (`run`) / single consumer (`read_samples`), on different cores: the
// Release/Acquire pairs order the entries against the indices.
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);
/// Something was dropped (a full queue, or the task restarted): the consumer compares what it
/// reported with [`DOWN`] once the queue is empty.
static LOST: AtomicBool = AtomicBool::new(false);
/// CPU0 only: bit n = touch n reported down by `read_samples`.
static REPORTED: AtomicU32 = AtomicU32::new(0);

/// CPU0's UI task, asleep until something is queued ([`sleep_until_touch`]).
static WAKER: Mutex<RefCell<Option<Waker>>> = Mutex::new(RefCell::new(None));

// DIAG (`/api/status` `touch`): messages read, samples dropped, I2C errors, CHG interrupts.
pub static MESSAGES: AtomicU32 = AtomicU32::new(0);
pub static DROPPED: AtomicU32 = AtomicU32::new(0);
pub static ERRORS: AtomicU32 = AtomicU32::new(0);
pub static INTERRUPTS: AtomicU32 = AtomicU32::new(0);
/// Waits for CHG that ended on the interrupt / on the backstop timer ([`BACKSTOP_MS`]), and
/// backstop looks that found messages (the interrupt missed them).
pub static WAKES_IRQ: AtomicU32 = AtomicU32::new(0);
pub static WAKES_TIMER: AtomicU32 = AtomicU32::new(0);
pub static TIMER_FOUND: AtomicU32 = AtomicU32::new(0);

/// The reader looks at the controller this often even without a CHG interrupt, so that a
/// missed interrupt costs at most this much latency.
const BACKSTOP_MS: u64 = 20;

/// CHG's level and its interrupt's mask / pending bits, for `/api/status`.
pub fn chg_bits() -> (bool, bool, bool) {
    let level = mxt::CHG.read();
    match mxt::CHG.eint() {
        Some(e) => (level, e.is_masked(), e.is_pending()),
        None => (level, false, false),
    }
}

fn push(kind: SampleKind, id: u8, x: u16, y: u16, ms: u32) {
    let head = HEAD.load(Ordering::Relaxed);
    let tail = TAIL.load(Ordering::Acquire);
    let free = RING - 1 - (head.wrapping_sub(tail) & MASK);
    if free == 0 || (kind == SampleKind::Move && free <= RESERVE) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        LOST.store(true, Ordering::Release);
        return;
    }
    // SAFETY: slot `head` is outside [tail, head), which the consumer reads; it sees it only
    // after the Release store below.
    unsafe { (*(&raw mut BUF))[head] = Entry { kind, id, x, y, ms } };
    HEAD.store((head + 1) & MASK, Ordering::Release);
}

fn wake_ui() {
    if let Some(w) = critical_section::with(|cs| WAKER.borrow_ref_mut(cs).take()) {
        w.wake();
    }
}

fn queue_empty() -> bool {
    HEAD.load(Ordering::Acquire) == TAIL.load(Ordering::Relaxed)
}

/// CPU0, before it starts CPU1: the touch task will run there (true if there is a controller
/// to read). From here on CPU0 leaves the controller's bus alone.
pub fn hand_to_cpu1() -> bool {
    let on = state().is_some();
    QUEUED.store(on, Ordering::Release);
    on
}

/// The interrupt-driven reader is in use (`/api/status`).
pub fn queued_mode() -> bool {
    QUEUED.load(Ordering::Relaxed)
}

/// CPU0: CPU1 did not start, or stopped (standby): CPU0 polls the controller again.
pub fn take_back() {
    if QUEUED.swap(false, Ordering::AcqRel) {
        // The pin was in its interrupt function; polling reads its level.
        mxt::CHG.set_input(Pull::None);
    }
}

/// CPU1 (or whichever core runs it): reads the controller as its messages come, until the
/// task is ended. Returns at once without a controller.
pub async fn run() {
    let Some(s) = state() else { return };
    // Whatever CPU0 reported before (another run of this task, or polling) is checked again.
    DOWN.store(0, Ordering::Release);
    POINTER.store(NO_POINTER, Ordering::Release);
    LOST.store(true, Ordering::Release);
    // The pin's interrupt goes to the core that makes it (`Gic::enable` targets the caller).
    let Some(chg) = EintInput::new(mxt::CHG, Pull::None) else { return };
    let mut queued = false;
    // The last wait ended on the backstop timer (and nothing was found yet).
    let mut by_timer = false;
    loop {
        let read = s.mxt.read_message_async().await;
        if by_timer && matches!(read, Ok(Some(_))) {
            TIMER_FOUND.fetch_add(1, Ordering::Relaxed);
        }
        by_timer = false;
        match read {
            Ok(Some(Message::Touch(t))) => {
                MESSAGES.fetch_add(1, Ordering::Relaxed);
                let ms = crate::systick::millis();
                if let Some(kind) = record(&t) {
                    push(kind, t.id & 15, t.x, t.y, ms);
                    queued = true;
                }
            }
            Ok(Some(_)) => {
                MESSAGES.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => {
                // The controller has nothing more: hand over what came, then wait for CHG.
                if queued || LOST.load(Ordering::Acquire) {
                    wake_ui();
                    queued = false;
                }
                let timer = embassy_time::Timer::after_millis(BACKSTOP_MS);
                match embassy_futures::select::select(chg.wait_low(), timer).await {
                    embassy_futures::select::Either::First(()) => {
                        WAKES_IRQ.fetch_add(1, Ordering::Relaxed);
                    }
                    embassy_futures::select::Either::Second(()) => {
                        WAKES_TIMER.fetch_add(1, Ordering::Relaxed);
                        by_timer = true;
                    }
                }
                INTERRUPTS.store(chg.interrupts(), Ordering::Relaxed);
            }
            Err(_) => {
                ERRORS.fetch_add(1, Ordering::Relaxed);
                embassy_time::Timer::after_millis(10).await;
            }
        }
    }
}

/// CPU1, when its tasks have ended: no wake-up from the touch pin or bus for the ended task.
pub fn quiesce() {
    let gic = Gic::new();
    if let Some(eint) = mxt::CHG.eint() {
        gic.disable(eint.irq());
    }
    gic.disable(Bus::I2c5.irq());
}

/// Raw T9 position → panel pixels, with the calibration's swap/flips.
fn to_screen(s: &State, (mut rx, mut ry): (u16, u16), cal: &CalParams) -> (u16, u16) {
    let (mut xr, mut yr) = (s.x_range, s.y_range);
    if cal.xy_swap {
        core::mem::swap(&mut rx, &mut ry);
        core::mem::swap(&mut xr, &mut yr);
    }
    let mut x = scale(rx, xr, PANEL.width);
    let mut y = scale(ry, yr, PANEL.height);
    if cal.x_flip {
        x = PANEL.width as u16 - 1 - x;
    }
    if cal.y_flip {
        y = PANEL.height as u16 - 1 - y;
    }
    (x, y)
}

fn scale(v: u16, range: u16, size: usize) -> u16 {
    ((v as u32 * (size as u32 - 1)) / range.max(1) as u32).min(size as u32 - 1) as u16
}

/// Polled mode (CPU0): reads the messages waiting (CHG low).
fn drain(s: &State) {
    if QUEUED.load(Ordering::Acquire) {
        return;
    }
    while !mxt::CHG.read() {
        match s.mxt.read_message() {
            Ok(Some(Message::Touch(t))) => {
                MESSAGES.fetch_add(1, Ordering::Relaxed);
                record(&t);
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                ERRORS.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
}

pub struct MxtTouch;

impl TouchBackend for MxtTouch {
    fn is_active(&self) -> bool {
        let Some(s) = state() else { return false };
        drain(s);
        DOWN.load(Ordering::Acquire) != 0 || (QUEUED.load(Ordering::Acquire) && !queue_empty())
    }

    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)> {
        let s = state()?;
        drain(s);
        let p = POINTER.load(Ordering::Acquire);
        (p != NO_POINTER).then(|| to_screen(s, raw_at(usize::from(p)), cal))
    }

    fn read_points(&self, cal: &CalParams, out: &mut [TouchPoint]) -> usize {
        let Some(s) = state() else { return 0 };
        drain(s);
        let down = DOWN.load(Ordering::Acquire);
        let mut n = 0;
        for id in 0..FINGERS {
            if down & (1 << id) != 0 && n < out.len() {
                let (x, y) = to_screen(s, raw_at(id), cal);
                out[n] = TouchPoint { id: id as u8, x, y };
                n += 1;
            }
        }
        n
    }

    fn release_debounce(&self) -> u8 {
        // Releases are explicit T9 messages, not missing samples.
        1
    }

    fn read_samples(&self, cal: &CalParams, out: &mut [TouchSample]) -> Option<usize> {
        if !QUEUED.load(Ordering::Acquire) {
            return None;
        }
        let s = state()?;
        let lost = LOST.swap(false, Ordering::AcqRel);
        let mut reported = REPORTED.load(Ordering::Relaxed);
        let mut tail = TAIL.load(Ordering::Relaxed);
        let head = HEAD.load(Ordering::Acquire);
        let mut n = 0;
        while tail != head && n < out.len() {
            // SAFETY: [tail, head) was written before the Acquire load of `head`.
            let e = unsafe { (*(&raw const BUF))[tail] };
            tail = (tail + 1) & MASK;
            let (x, y) = to_screen(s, (e.x, e.y), cal);
            out[n] = TouchSample { kind: e.kind, id: e.id, x, y, ms: e.ms };
            n += 1;
            match e.kind {
                SampleKind::Down | SampleKind::Move => reported |= 1 << e.id,
                SampleKind::Up => reported &= !(1 << e.id),
            }
        }
        TAIL.store(tail, Ordering::Release);
        if lost {
            if tail != head || out.len() - n < FINGERS {
                // Not all read yet: compare next time.
                LOST.store(true, Ordering::Release);
            } else {
                // Samples were dropped: bring what was reported in line with what is down.
                // Repeats (a down for a finger down, an up for one up) are ignored by the core.
                let ms = crate::systick::millis();
                let down = DOWN.load(Ordering::Acquire);
                for id in 0..FINGERS {
                    let bit = 1u32 << id;
                    let kind = match (reported & bit != 0, down & bit != 0) {
                        (true, false) => SampleKind::Up,
                        (false, true) => SampleKind::Down,
                        _ => continue,
                    };
                    let (x, y) = to_screen(s, raw_at(id), cal);
                    out[n] = TouchSample { kind, id: id as u8, x, y, ms };
                    n += 1;
                    reported ^= bit;
                }
            }
        }
        REPORTED.store(reported, Ordering::Relaxed);
        Some(n)
    }
}

/// Sleeps (CPU0 in the executor) until `ms` have passed or a touch comes. Interrupt-driven:
/// until CPU1's task queues something. Polled: until CHG goes low, through its wake-up
/// interrupt; the pin is the interrupt's only meanwhile and a plain input again after, as
/// [`drain`] reads its level (not relied on while the pin is in its interrupt function). A
/// message already waiting ends the sleep at once. Without a controller, just the timer.
pub async fn sleep_until_touch(ms: u32) {
    let timer = embassy_time::Timer::after_millis(u64::from(ms));
    if state().is_none() {
        return timer.await;
    }
    if QUEUED.load(Ordering::Acquire) {
        embassy_futures::select::select(timer, queued()).await;
        return;
    }
    let Some(chg) = EintInput::new(mxt::CHG, Pull::None) else { return timer.await };
    embassy_futures::select::select(timer, chg.wait_low()).await;
    // Back to the polled input. If the interrupt is still unmasked (the timer ended the
    // sleep), the input function keeps it from firing; a late one only wakes nothing.
    mxt::CHG.set_input(Pull::None);
}

/// Resolves once a sample is queued (or a resync is due).
fn queued() -> impl core::future::Future<Output = ()> {
    core::future::poll_fn(|cx| {
        let ready = || !queue_empty() || LOST.load(Ordering::Acquire);
        if ready() {
            return Poll::Ready(());
        }
        critical_section::with(|cs| *WAKER.borrow_ref_mut(cs) = Some(cx.waker().clone()));
        // It may have come before the waker was there.
        if ready() { Poll::Ready(()) } else { Poll::Pending }
    })
}

/// Identity mapping: the controller's T9 ORIENT already matches the panel.
/// Flip here if a board turns out mounted otherwise.
pub const CAL: CalParams = CalParams {
    xy_swap: false,
    x_flip: false,
    y_flip: false,
    x_min: 0,
    x_max: PANEL.width as u16,
    y_min: 0,
    y_max: PANEL.height as u16,
};

/// Powers the controller (PMIC on I2C0), reads its object table and makes it
/// report. On failure touch stays off; the UI still runs.
pub fn init(pmic: Max77686, mct: Mct) -> Result<(), mxt::Error> {
    mxt::power_up(pmic, mct)?;
    let mxt = Mxt::open(I2c::new(Bus::I2c5))?;
    mxt.wake()?;
    let cfg = mxt.touch_config()?;
    // SAFETY: boot, one core, before any reader.
    unsafe {
        *(&raw mut STATE) = Some(State { mxt, x_range: cfg.x_range, y_range: cfg.y_range });
    }
    Ok(())
}
