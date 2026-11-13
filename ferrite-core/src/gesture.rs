//! Gesture recognition after Flutter's (`packages/flutter/lib/src/gestures`):
//! pointer events go to recognizers, which compete for each pointer in a
//! gesture arena.
//!
//! * Pointers ([`Ptr`]): Down / Move / Up / Cancel per finger id, from
//!   `TouchImpl::poll_pointers`.
//! * On a pointer's Down, the input handler lists the [`Candidate`]s under it,
//!   deepest widget first (Flutter's hit-test order). Each becomes (or reuses)
//!   a recognizer instance ([`Rec`]), which may join the pointer's arena.
//! * Arena (`arena.dart`): it closes after the Down has been routed; a lone
//!   member wins at once; a member may accept (eager win while the arena is
//!   open, else at once) or reject itself; at Up the arena is swept: the first
//!   member (the deepest widget's) wins. `hold`/`release` let a double tap
//!   delay that sweep.
//! * Recognizers: tap (`tap.dart`, also the legacy `on_click`: [`Kind::Tap`]
//!   with `click`), double tap (`multitap.dart`), long press
//!   (`long_press.dart`), pan / horizontal / vertical drag and drag-to-scroll
//!   (`monodrag.dart`), scale (`scale.dart`) and tap-and-drag
//!   (`tap_and_drag.dart`, `TapAndPanGestureRecognizer`).
//!
//! The engine is plain state: it knows widgets only by id and emits [`Out`]s
//! for the input handler to act on (VM callbacks, clicks, scrolling).
//! Constants are Flutter's (`constants.dart`), in pixels and milliseconds.

use alloc::vec::Vec;

use crate::widget::WidgetId;

// --- Recognizer bits (`onGesture` mask, lib/gesture.fl GESTURE_*) ---

pub const G_TAP: u16 = 1 << 0;
pub const G_DOUBLE_TAP: u16 = 1 << 1;
pub const G_LONG_PRESS: u16 = 1 << 2;
pub const G_PAN: u16 = 1 << 3;
pub const G_HDRAG: u16 = 1 << 4;
pub const G_VDRAG: u16 = 1 << 5;
pub const G_SCALE: u16 = 1 << 6;
pub const G_TAP_DRAG: u16 = 1 << 7;
/// Every recognizer bit.
pub const G_ALL: u16 = 0xFF;

/// Event codes passed to a gesture handler (`fn h(widget, event)`),
/// lib/gesture.fl `GE_*` — keep in sync.
pub mod ev {
    pub const TAP_DOWN: u8 = 1;
    pub const TAP_UP: u8 = 2;
    pub const TAP: u8 = 3;
    pub const TAP_CANCEL: u8 = 4;
    pub const DOUBLE_TAP: u8 = 5;
    pub const LONG_PRESS_START: u8 = 6;
    pub const LONG_PRESS_MOVE: u8 = 7;
    pub const LONG_PRESS_END: u8 = 8;
    pub const DRAG_START: u8 = 9;
    pub const DRAG_UPDATE: u8 = 10;
    pub const DRAG_END: u8 = 11;
    pub const SCALE_START: u8 = 12;
    pub const SCALE_UPDATE: u8 = 13;
    pub const SCALE_END: u8 = 14;
    pub const TAP_DRAG_DOWN: u8 = 15;
    pub const TAP_DRAG_UP: u8 = 16;
    pub const TAP_DRAG_START: u8 = 17;
    pub const TAP_DRAG_UPDATE: u8 = 18;
    pub const TAP_DRAG_END: u8 = 19;
    pub const TAP_DRAG_CANCEL: u8 = 20;
}

/// Fields of an event's details (`gestureInfo(field)`), lib/gesture.fl `GI_*`
/// — keep in sync.
pub mod field {
    /// Screen position (scale: the focal point).
    pub const X: usize = 0;
    pub const Y: usize = 1;
    /// Position relative to the widget's box.
    pub const LOCAL_X: usize = 2;
    pub const LOCAL_Y: usize = 3;
    /// Movement since the last update (drags, scale focal point, long press).
    pub const DX: usize = 4;
    pub const DY: usize = 5;
    /// Offset from where the gesture started.
    pub const TOTAL_X: usize = 6;
    pub const TOTAL_Y: usize = 7;
    /// Velocity in px/s (ends of drags, scale and long press; 0 = no fling).
    pub const VX: usize = 8;
    pub const VY: usize = 9;
    /// Scale × 1000 (span ratio since the start), horizontal / vertical spans.
    pub const SCALE: usize = 10;
    pub const HSCALE: usize = 11;
    pub const VSCALE: usize = 12;
    /// Rotation since the start in milliradians (two or more pointers).
    pub const ROTATION: usize = 13;
    /// Pointers down (scale).
    pub const POINTERS: usize = 14;
    /// Consecutive taps (tap and drag: 1, 2 = double tap, …).
    pub const TAPS: usize = 15;
    /// The recognizer bit (`G_*`) that sent the event.
    pub const KIND: usize = 16;
    /// Milliseconds since the gesture's first pointer went down.
    pub const TIME: usize = 17;
    pub const COUNT: usize = 18;
}

/// An event's details, indexed by [`field`].
pub type Detail = [i32; field::COUNT];

// --- Flutter's constants (constants.dart) ---

pub const PRESS_TIMEOUT_MS: u32 = 100;
pub const LONG_PRESS_MS: u32 = 500;
pub const DOUBLE_TAP_MS: u32 = 300;
pub const DOUBLE_TAP_MIN_MS: u32 = 40;
pub const DOUBLE_TAP_SLOP: f32 = 100.0;
pub const TOUCH_SLOP: f32 = 18.0;
pub const PAN_SLOP: f32 = TOUCH_SLOP * 2.0;
pub const SCALE_SLOP: f32 = TOUCH_SLOP;
pub const MIN_FLING_VELOCITY: f32 = 50.0;
pub const MAX_FLING_VELOCITY: f32 = 8000.0;

// --- Pointer events ---

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PKind {
    Down,
    Move,
    Up,
    Cancel,
}

/// One pointer event. `id` < 32.
#[derive(Clone, Copy, Debug)]
pub struct Ptr {
    pub kind: PKind,
    pub id: u8,
    pub x: i32,
    pub y: i32,
    pub ms: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Axis {
    /// Any direction (pan: `PAN_SLOP`).
    Free,
    Horizontal,
    Vertical,
}

/// What a recognizer is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// `click`: the legacy on_click / on_tap / built-in widget actions
    /// ([`Out::Click`]); `events`: the widget's `G_TAP` handler.
    Tap { click: bool, events: bool },
    DoubleTap,
    LongPress,
    Drag(Axis),
    /// A scroll container's vertical drag ([`Out::Scroll`] / [`Out::Fling`]).
    Scroll,
    Scale,
    TapDrag,
}

impl Kind {
    /// The `G_*` bit its events carry (0: none).
    fn bit(self) -> u16 {
        match self {
            Kind::Tap { .. } => G_TAP,
            Kind::DoubleTap => G_DOUBLE_TAP,
            Kind::LongPress => G_LONG_PRESS,
            Kind::Drag(Axis::Free) => G_PAN,
            Kind::Drag(Axis::Horizontal) => G_HDRAG,
            Kind::Drag(Axis::Vertical) => G_VDRAG,
            Kind::Scroll => 0,
            Kind::Scale => G_SCALE,
            Kind::TapDrag => G_TAP_DRAG,
        }
    }
}

/// A recognizer a pointer's Down may start: `widget`'s `kind`, the widget's
/// box at `origin` (for local positions).
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub widget: WidgetId,
    pub kind: Kind,
    pub origin: (i32, i32),
}

/// What the engine asks the input handler to do.
#[derive(Clone, Copy, Debug)]
pub enum Out {
    /// A gesture handler event for `widget` (its `G_*` bit is `d[KIND]`).
    Event { widget: WidgetId, ev: u8, d: Detail },
    /// The legacy click of `widget` at (x, y): on_click, on_tap, checkbox…
    Click { widget: WidgetId, x: i32, y: i32 },
    /// The legacy click lost (the press highlight goes).
    ClickCancel { widget: WidgetId },
    /// Drag-to-scroll: the content follows the finger by `dy`.
    Scroll { widget: WidgetId, dy: i32 },
    /// Drag-to-scroll ended; fling at `vy` px/s (0 = none).
    Fling { widget: WidgetId, vy: f32 },
}

// ============================================================================
//  Velocity (velocity_tracker.dart): least squares over the last 100 ms
// ============================================================================

const VT_HISTORY: usize = 20;
const VT_HORIZON_MS: u32 = 100;
const VT_STOPPED_MS: u32 = 40;
const VT_MIN_SAMPLES: usize = 3;

#[derive(Clone, Copy, Default)]
struct Velocity {
    t: [u32; VT_HISTORY],
    x: [f32; VT_HISTORY],
    y: [f32; VT_HISTORY],
    len: usize,
    /// Index of the newest sample.
    head: usize,
}

/// A velocity estimate: px/s, and the offset covered by its samples.
#[derive(Clone, Copy, Default, Debug)]
pub struct Estimate {
    pub vx: f32,
    pub vy: f32,
    pub ox: f32,
    pub oy: f32,
}

impl Velocity {
    fn reset(&mut self) {
        self.len = 0;
    }

    fn add(&mut self, ms: u32, x: i32, y: i32) {
        self.head = if self.len == 0 { 0 } else { (self.head + 1) % VT_HISTORY };
        self.t[self.head] = ms;
        self.x[self.head] = x as f32;
        self.y[self.head] = y as f32;
        self.len = (self.len + 1).min(VT_HISTORY);
    }

    /// `getVelocityEstimate`: samples newer than the horizon with no gap over
    /// 40 ms, fitted by a degree 2 polynomial (velocity = its slope now).
    fn estimate(&self) -> Estimate {
        if self.len == 0 {
            return Estimate::default();
        }
        let newest = self.head;
        let (mut ts, mut xs, mut ys) = ([0f64; VT_HISTORY], [0f64; VT_HISTORY], [0f64; VT_HISTORY]);
        let mut n = 0;
        let mut prev_t = self.t[newest];
        let mut oldest = newest;
        let mut i = newest;
        while n < self.len {
            let age = self.t[newest].wrapping_sub(self.t[i]);
            let gap = prev_t.wrapping_sub(self.t[i]);
            prev_t = self.t[i];
            if age > VT_HORIZON_MS || gap > VT_STOPPED_MS {
                break;
            }
            oldest = i;
            ts[n] = -(age as f64);
            xs[n] = self.x[i] as f64;
            ys[n] = self.y[i] as f64;
            n += 1;
            i = if i == 0 { VT_HISTORY - 1 } else { i - 1 };
        }
        let ox = self.x[newest] - self.x[oldest];
        let oy = self.y[newest] - self.y[oldest];
        if n >= VT_MIN_SAMPLES {
            if let (Some(vx), Some(vy)) = (slope(&ts[..n], &xs[..n]), slope(&ts[..n], &ys[..n])) {
                return Estimate { vx: (vx * 1000.0) as f32, vy: (vy * 1000.0) as f32, ox, oy };
            }
        }
        Estimate { vx: 0.0, vy: 0.0, ox, oy }
    }
}

/// Slope at t = 0 (per ms) of the least-squares quadratic through (t, v),
/// or of the line when the quadratic is degenerate (samples within a
/// millisecond of each other).
fn slope(t: &[f64], v: &[f64]) -> Option<f64> {
    // Normal equations for c0 + c1·t + c2·t².
    let mut s = [0f64; 5];
    let mut r = [0f64; 3];
    for (&ti, &vi) in t.iter().zip(v) {
        let mut p = 1.0;
        for k in 0..5 {
            s[k] += p;
            if k < 3 {
                r[k] += p * vi;
            }
            p *= ti;
        }
    }
    let m = [[s[0], s[1], s[2]], [s[1], s[2], s[3]], [s[2], s[3], s[4]]];
    let det3 = |a: [[f64; 3]; 3]| {
        a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
    };
    let d = det3(m);
    let scale = s[0] * s[2] * s[4];
    if d.abs() > 1e-9 * scale.abs().max(1.0) {
        let mut m1 = m;
        for k in 0..3 {
            m1[k][1] = r[k];
        }
        return Some(det3(m1) / d);
    }
    let d2 = s[0] * s[2] - s[1] * s[1];
    if d2.abs() > 1e-9 {
        return Some((s[0] * r[1] - s[1] * r[0]) / d2);
    }
    None
}

/// Flutter's fling test and clamp: a velocity worth flinging (over
/// `MIN_FLING_VELOCITY`, after moving over `TOUCH_SLOP` within the estimate)
/// at most `MAX_FLING_VELOCITY`, else zero.
fn fling(e: Estimate, axis: Axis) -> (f32, f32) {
    let (vx, vy, ox, oy) = match axis {
        Axis::Free => (e.vx, e.vy, e.ox, e.oy),
        Axis::Horizontal => (e.vx, 0.0, e.ox, 0.0),
        Axis::Vertical => (0.0, e.vy, 0.0, e.oy),
    };
    let v2 = vx * vx + vy * vy;
    if v2 <= MIN_FLING_VELOCITY * MIN_FLING_VELOCITY || ox * ox + oy * oy <= TOUCH_SLOP * TOUCH_SLOP {
        return (0.0, 0.0);
    }
    if v2 > MAX_FLING_VELOCITY * MAX_FLING_VELOCITY {
        let k = MAX_FLING_VELOCITY / libm::sqrtf(v2);
        return (vx * k, vy * k);
    }
    (vx, vy)
}

fn dist(a: (i32, i32), b: (i32, i32)) -> f32 {
    let (dx, dy) = ((a.0 - b.0) as f32, (a.1 - b.1) as f32);
    libm::sqrtf(dx * dx + dy * dy)
}

fn bit(p: u8) -> u32 {
    1u32 << (p & 31)
}

// ============================================================================
//  Recognizer state
// ============================================================================

#[derive(Clone, Copy, PartialEq, Debug)]
enum PState {
    Ready,
    Possible,
    Defunct,
}

/// `PrimaryPointerGestureRecognizer`: tap and long press.
#[derive(Clone, Copy)]
struct Primary {
    state: PState,
    primary: Option<u8>,
    initial: (i32, i32),
    accepted: bool,
    deadline: Option<u32>,
}

impl Primary {
    const NEW: Self = Self { state: PState::Ready, primary: None, initial: (0, 0), accepted: false, deadline: None };
}

#[derive(Clone, Copy)]
struct TapSt {
    pp: Primary,
    down: Option<(i32, i32)>,
    up: Option<(i32, i32)>,
    sent_down: bool,
    won: bool,
}

#[derive(Clone, Copy)]
struct LongSt {
    pp: Primary,
    lp_accepted: bool,
    origin: (i32, i32),
    last: (i32, i32),
    vel: Velocity,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum DState {
    Ready,
    Possible,
    Accepted,
}

#[derive(Clone, Copy)]
struct DragSt {
    axis: Axis,
    state: DState,
    primary: Option<u8>,
    initial: (i32, i32),
    last: (i32, i32),
    moved: f32,
    vel: Velocity,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SState {
    Ready,
    Possible,
    Accepted,
    Started,
}

const SCALE_PTRS: usize = 10;

#[derive(Clone, Copy)]
struct Line {
    a: u8,
    b: u8,
    pa: (f32, f32),
    pb: (f32, f32),
}

#[derive(Clone, Copy)]
struct ScaleSt {
    state: SState,
    /// (pointer, x, y) in the order they went down (`_pointerQueue`).
    pts: [(u8, f32, f32); SCALE_PTRS],
    n: usize,
    vel: [(u8, Velocity); SCALE_PTRS],
    focal: Option<(f32, f32)>,
    initial_focal: (f32, f32),
    delta: (f32, f32),
    span: f32,
    hspan: f32,
    vspan: f32,
    initial_span: f32,
    initial_hspan: f32,
    initial_vspan: f32,
    line: Option<Line>,
    initial_line: Option<Line>,
}

#[derive(Clone, Copy)]
struct TapTrack {
    p: u8,
    at: (i32, i32),
}

#[derive(Clone, Copy)]
struct DoubleSt {
    /// The first tap, once up: pointer, position, time of its up.
    first: Option<(TapTrack, u32)>,
    trackers: [Option<TapTrack>; 4],
    timer: Option<u32>,
}

#[derive(Clone, Copy)]
struct TapDragSt {
    // `_TapStatusTrackerMixin`
    count: i32,
    down: Option<(i32, i32)>,
    up: bool,
    origin: Option<(i32, i32)>,
    last_tap: Option<(i32, i32)>,
    /// Consecutive-tap timer: when it runs out (`kDoubleTapTimeout` after an up).
    tap_timer: Option<u32>,
    // `BaseTapAndDragGestureRecognizer`
    primary: Option<u8>,
    sent_down: bool,
    won: bool,
    state: DState,
    start: Option<(i32, i32)>,
    initial: (i32, i32),
    current: (i32, i32),
    moved_all: f32,
    past_slop: bool,
    accepted_active: u32,
    deadline: Option<u32>,
    vel: Velocity,
}

#[derive(Clone, Copy)]
enum St {
    Tap(TapSt),
    Double(DoubleSt),
    Long(LongSt),
    Drag(DragSt),
    Scale(ScaleSt),
    TapDrag(TapDragSt),
}

/// A recognizer instance: one per (widget, kind) while it has work.
#[derive(Clone, Copy)]
struct Rec {
    widget: WidgetId,
    kind: Kind,
    origin: (i32, i32),
    /// Pointers it receives events of (`startTrackingPointer`).
    tracked: u32,
    /// Pointers whose arena it is still in.
    entries: u32,
    /// When its gesture began (detail `TIME`).
    t0: u32,
    st: St,
}

#[derive(Clone, Copy, PartialEq)]
enum Disp {
    Accepted,
    Rejected,
}

struct Arena {
    pointer: u8,
    members: Vec<usize>,
    open: bool,
    held: bool,
    pending_sweep: bool,
    eager: Option<usize>,
}

/// The gesture engine: recognizer instances, the arenas, and the outputs
/// waiting for the input handler.
pub struct Engine {
    recs: Vec<Option<Rec>>,
    arenas: Vec<Arena>,
    /// Arenas left with one member: resolved after the current event
    /// (Flutter's microtask).
    defaults: Vec<u8>,
    out: Vec<Out>,
    now: u32,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub const fn new() -> Self {
        Self { recs: Vec::new(), arenas: Vec::new(), defaults: Vec::new(), out: Vec::new(), now: 0 }
    }

    /// Forget everything (program switch).
    pub fn reset(&mut self) {
        self.recs.clear();
        self.arenas.clear();
        self.defaults.clear();
        self.out.clear();
    }

    /// Nothing waits for a timer or a pointer.
    pub fn idle(&self) -> bool {
        self.recs.iter().all(|r| r.is_none()) && self.arenas.is_empty()
    }

    /// Outputs since the last call, in order.
    pub fn take(&mut self) -> Vec<Out> {
        core::mem::take(&mut self.out)
    }

    /// `widget` has a recognizer of `kind` with a pointer down (or waiting
    /// for one: a pending double tap).
    pub fn has(&self, widget: WidgetId, kind: Kind) -> bool {
        self.recs.iter().flatten().any(|r| r.widget == widget && r.kind == kind)
    }

    /// Drop the recognizers of a widget that is gone, without events.
    pub fn forget(&mut self, mut gone: impl FnMut(WidgetId) -> bool) {
        for i in 0..self.recs.len() {
            if self.recs[i].map_or(false, |r| gone(r.widget)) {
                self.recs[i] = None;
                for a in 0..self.arenas.len() {
                    self.arenas[a].members.retain(|&m| m != i);
                    if self.arenas[a].eager == Some(i) {
                        self.arenas[a].eager = None;
                    }
                }
            }
        }
        let ps: Vec<u8> = self.arenas.iter().filter(|a| !a.open).map(|a| a.pointer).collect();
        for p in ps {
            self.try_resolve(p);
        }
        self.finish();
    }

    /// One pointer event. `cands` (deepest widget first) only for a Down.
    pub fn pointer(&mut self, e: Ptr, cands: &[Candidate]) {
        self.now = e.ms;
        match e.kind {
            PKind::Down => {
                self.arenas.retain(|a| a.pointer != e.id);
                self.arenas.push(Arena {
                    pointer: e.id,
                    members: Vec::new(),
                    open: true,
                    held: false,
                    pending_sweep: false,
                    eager: None,
                });
                for c in cands {
                    let r = self.instance(c);
                    self.add_pointer(r, e);
                }
                self.route(e);
                self.close(e.id);
            }
            PKind::Move => self.route(e),
            PKind::Up | PKind::Cancel => {
                self.route(e);
                self.sweep(e.id);
            }
        }
        self.finish();
    }

    /// Timers (tap down, long press, double tap, tap-and-drag) due by `now`.
    pub fn tick(&mut self, now: u32) {
        self.now = now;
        let due = |t: Option<u32>| t.map_or(false, |t| now.wrapping_sub(t) < 0x8000_0000);
        for r in 0..self.recs.len() {
            let Some(rec) = self.recs[r] else { continue };
            match rec.st {
                St::Tap(s) if due(s.pp.deadline) => {
                    self.with_tap(r, |s| s.pp.deadline = None);
                    self.tap_check_down(r);
                }
                St::Long(s) if due(s.pp.deadline) => {
                    self.with_long(r, |s| s.pp.deadline = None);
                    self.long_deadline(r);
                }
                St::Double(s) if due(s.timer) => {
                    self.with_double(r, |s| s.timer = None);
                    self.double_reset(r);
                }
                St::TapDrag(s) => {
                    if due(s.deadline) {
                        self.with_td(r, |s| s.deadline = None);
                        self.td_deadline(r);
                    }
                    if due(s.tap_timer) {
                        // The series is over; the next down starts a new one.
                        self.with_td(r, |s| s.tap_timer = Some(now.wrapping_sub(1)));
                    }
                }
                _ => {}
            }
        }
        self.finish();
    }

    /// The earliest timer, if any (for idle sleep).
    pub fn next_timer(&self) -> Option<u32> {
        let mut best: Option<u32> = None;
        let mut put = |t: Option<u32>| {
            if let Some(t) = t {
                best = Some(match best {
                    Some(b) if b.wrapping_sub(t) >= 0x8000_0000 => b,
                    _ => t,
                });
            }
        };
        for r in self.recs.iter().flatten() {
            match r.st {
                St::Tap(s) => put(s.pp.deadline),
                St::Long(s) => put(s.pp.deadline),
                St::Double(s) => put(s.timer),
                St::TapDrag(s) => {
                    put(s.deadline);
                    if s.tap_timer.map_or(false, |t| t.wrapping_sub(self.now) < 0x8000_0000) {
                        put(s.tap_timer);
                    }
                }
                _ => {}
            }
        }
        best
    }

    // --- Bookkeeping ---

    /// Run the pending default resolutions, then drop the instances with
    /// nothing left to do.
    fn finish(&mut self) {
        while let Some(p) = self.defaults.pop() {
            self.resolve_by_default(p);
        }
        let now = self.now;
        for r in self.recs.iter_mut() {
            let idle = match r {
                Some(rec) if rec.tracked == 0 && rec.entries == 0 => match rec.st {
                    St::Tap(s) => s.pp.deadline.is_none(),
                    St::Long(s) => s.pp.deadline.is_none(),
                    St::Double(s) => s.first.is_none() && s.trackers.iter().all(|t| t.is_none()) && s.timer.is_none(),
                    St::TapDrag(s) => s.tap_timer.map_or(true, |t| now.wrapping_sub(t) < 0x8000_0000),
                    _ => true,
                },
                _ => false,
            };
            if idle {
                *r = None;
            }
        }
        while matches!(self.recs.last(), Some(None)) {
            self.recs.pop();
        }
    }

    /// The live instance for a candidate, or a new one.
    fn instance(&mut self, c: &Candidate) -> usize {
        if let Some(i) = self.recs.iter().position(|r| r.map_or(false, |r| r.widget == c.widget && r.kind == c.kind)) {
            let r = self.recs[i].as_mut().unwrap();
            r.origin = c.origin;
            return i;
        }
        let st = match c.kind {
            Kind::Tap { .. } => St::Tap(TapSt { pp: Primary::NEW, down: None, up: None, sent_down: false, won: false }),
            Kind::DoubleTap => St::Double(DoubleSt { first: None, trackers: [None; 4], timer: None }),
            Kind::LongPress => St::Long(LongSt {
                pp: Primary::NEW,
                lp_accepted: false,
                origin: (0, 0),
                last: (0, 0),
                vel: Velocity::default(),
            }),
            Kind::Drag(axis) => St::Drag(DragSt::new(axis)),
            Kind::Scroll => St::Drag(DragSt::new(Axis::Vertical)),
            Kind::Scale => St::Scale(ScaleSt::new()),
            Kind::TapDrag => St::TapDrag(TapDragSt::new()),
        };
        let rec = Rec { widget: c.widget, kind: c.kind, origin: c.origin, tracked: 0, entries: 0, t0: self.now, st };
        match self.recs.iter().position(|r| r.is_none()) {
            Some(i) => {
                self.recs[i] = Some(rec);
                i
            }
            None => {
                self.recs.push(Some(rec));
                self.recs.len() - 1
            }
        }
    }

    fn rec(&mut self, r: usize) -> &mut Rec {
        self.recs[r].as_mut().unwrap()
    }

    fn alive(&self, r: usize) -> bool {
        self.recs.get(r).map_or(false, |r| r.is_some())
    }

    fn emit(&mut self, r: usize, ev: u8, f: impl FnOnce(&mut Detail)) {
        let rec = self.recs[r].unwrap();
        let mut d: Detail = [0; field::COUNT];
        d[field::SCALE] = 1000;
        d[field::HSCALE] = 1000;
        d[field::VSCALE] = 1000;
        d[field::KIND] = rec.kind.bit() as i32;
        d[field::TIME] = self.now.wrapping_sub(rec.t0) as i32;
        f(&mut d);
        d[field::LOCAL_X] = d[field::X] - rec.origin.0;
        d[field::LOCAL_Y] = d[field::Y] - rec.origin.1;
        self.out.push(Out::Event { widget: rec.widget, ev, d });
    }

    // --- Pointer tracking (`OneSequenceGestureRecognizer`) ---

    fn start_tracking(&mut self, r: usize, p: u8) {
        self.rec(r).tracked |= bit(p);
        if let Some(a) = self.arenas.iter_mut().find(|a| a.pointer == p) {
            if !a.members.contains(&r) {
                a.members.push(r);
            }
            self.rec(r).entries |= bit(p);
        }
    }

    fn stop_tracking(&mut self, r: usize, p: u8) {
        if !self.alive(r) || self.rec(r).tracked & bit(p) == 0 {
            return;
        }
        self.rec(r).tracked &= !bit(p);
        if self.rec(r).tracked == 0 {
            self.did_stop_last(r, p);
        }
    }

    fn stop_if_up(&mut self, r: usize, e: Ptr) {
        if matches!(e.kind, PKind::Up | PKind::Cancel) {
            self.stop_tracking(r, e.id);
        }
    }

    /// `resolve`: in every arena it is in.
    fn resolve_all(&mut self, r: usize, d: Disp) {
        let entries = self.rec(r).entries;
        self.rec(r).entries = 0;
        for p in 0..32u8 {
            if entries & bit(p) != 0 {
                self.arena_resolve(p, r, d);
            }
        }
    }

    fn resolve_pointer(&mut self, r: usize, p: u8, d: Disp) {
        if self.rec(r).entries & bit(p) != 0 {
            self.rec(r).entries &= !bit(p);
            self.arena_resolve(p, r, d);
        }
    }

    // --- The arena (`GestureArenaManager`) ---

    fn arena(&mut self, p: u8) -> Option<usize> {
        self.arenas.iter().position(|a| a.pointer == p)
    }

    fn close(&mut self, p: u8) {
        if let Some(a) = self.arena(p) {
            self.arenas[a].open = false;
            self.try_resolve(p);
        }
    }

    fn sweep(&mut self, p: u8) {
        let Some(a) = self.arena(p) else { return };
        if self.arenas[a].open {
            return;
        }
        if self.arenas[a].held {
            self.arenas[a].pending_sweep = true;
            return;
        }
        let arena = self.arenas.remove(a);
        if let Some((&first, rest)) = arena.members.split_first() {
            self.accept(first, p);
            for &m in rest {
                self.reject(m, p);
            }
        }
    }

    fn hold(&mut self, p: u8) {
        if let Some(a) = self.arena(p) {
            self.arenas[a].held = true;
        }
    }

    fn release(&mut self, p: u8) {
        if let Some(a) = self.arena(p) {
            self.arenas[a].held = false;
            if self.arenas[a].pending_sweep {
                self.sweep(p);
            }
        }
    }

    fn arena_resolve(&mut self, p: u8, r: usize, d: Disp) {
        let Some(a) = self.arena(p) else { return };
        if !self.arenas[a].members.contains(&r) {
            return;
        }
        match d {
            Disp::Rejected => {
                self.arenas[a].members.retain(|&m| m != r);
                if self.arenas[a].eager == Some(r) {
                    self.arenas[a].eager = None;
                }
                let open = self.arenas[a].open;
                self.reject(r, p);
                if !open {
                    self.try_resolve(p);
                }
            }
            Disp::Accepted => {
                if self.arenas[a].open {
                    if self.arenas[a].eager.is_none() {
                        self.arenas[a].eager = Some(r);
                    }
                } else {
                    self.resolve_in_favor(p, r);
                }
            }
        }
    }

    fn try_resolve(&mut self, p: u8) {
        let Some(a) = self.arena(p) else { return };
        let arena = &self.arenas[a];
        if arena.members.len() == 1 {
            if !self.defaults.contains(&p) {
                self.defaults.push(p);
            }
        } else if arena.members.is_empty() {
            self.arenas.remove(a);
        } else if let Some(e) = arena.eager {
            self.resolve_in_favor(p, e);
        }
    }

    fn resolve_by_default(&mut self, p: u8) {
        let Some(a) = self.arena(p) else { return };
        if self.arenas[a].members.len() != 1 {
            return;
        }
        let arena = self.arenas.remove(a);
        self.accept(arena.members[0], p);
    }

    fn resolve_in_favor(&mut self, p: u8, winner: usize) {
        let Some(a) = self.arena(p) else { return };
        let arena = self.arenas.remove(a);
        for &m in &arena.members {
            if m != winner {
                self.reject(m, p);
            }
        }
        self.accept(winner, p);
    }

    // --- Dispatch ---

    fn route(&mut self, e: Ptr) {
        for r in 0..self.recs.len() {
            if self.alive(r) && self.rec(r).tracked & bit(e.id) != 0 {
                self.handle(r, e);
            }
        }
    }

    fn add_pointer(&mut self, r: usize, e: Ptr) {
        match self.recs[r].unwrap().st {
            St::Tap(_) => self.tap_add(r, e),
            St::Double(_) => self.double_add(r, e),
            St::Long(_) => self.long_add(r, e),
            St::Drag(_) => self.drag_add(r, e),
            St::Scale(_) => self.scale_add(r, e),
            St::TapDrag(_) => self.td_add(r, e),
        }
    }

    fn handle(&mut self, r: usize, e: Ptr) {
        match self.recs[r].unwrap().st {
            St::Tap(_) => self.tap_handle(r, e),
            St::Double(_) => self.double_handle(r, e),
            St::Long(_) => self.long_handle(r, e),
            St::Drag(_) => self.drag_handle(r, e),
            St::Scale(_) => self.scale_handle(r, e),
            St::TapDrag(_) => self.td_handle(r, e),
        }
    }

    fn accept(&mut self, r: usize, p: u8) {
        if !self.alive(r) {
            return;
        }
        self.rec(r).entries &= !bit(p);
        match self.recs[r].unwrap().st {
            St::Tap(_) => self.tap_accept(r, p),
            St::Double(_) => {}
            St::Long(_) => {}
            St::Drag(_) => self.drag_accept(r, p),
            St::Scale(_) => self.scale_accept(r, p),
            St::TapDrag(_) => self.td_accept(r, p),
        }
    }

    fn reject(&mut self, r: usize, p: u8) {
        if !self.alive(r) {
            return;
        }
        self.rec(r).entries &= !bit(p);
        match self.recs[r].unwrap().st {
            St::Tap(_) => self.tap_reject(r, p),
            St::Double(_) => self.double_reject(r, p),
            St::Long(_) => self.pp_reject(r, p),
            St::Drag(_) => self.stop_tracking(r, p),
            St::Scale(_) => self.scale_reject(r, p),
            St::TapDrag(_) => self.td_reject(r, p),
        }
    }

    fn did_stop_last(&mut self, r: usize, p: u8) {
        match self.recs[r].unwrap().st {
            St::Tap(_) | St::Long(_) => self.pp_stop_last(r),
            St::Double(_) => {}
            St::Drag(_) => self.drag_stop_last(r),
            St::Scale(_) => self.scale_stop_last(r),
            St::TapDrag(_) => self.td_stop_last(r, p),
        }
    }

    // ========================================================================
    //  PrimaryPointerGestureRecognizer (tap, long press)
    // ========================================================================

    fn pp(&mut self, r: usize) -> &mut Primary {
        match &mut self.rec(r).st {
            St::Tap(s) => &mut s.pp,
            St::Long(s) => &mut s.pp,
            _ => unreachable!(),
        }
    }

    /// `addAllowedPointer`: only while not tracking another pointer.
    fn pp_add(&mut self, r: usize, e: Ptr, deadline_ms: u32) -> bool {
        if self.pp(r).state != PState::Ready {
            return false;
        }
        self.start_tracking(r, e.id);
        let now = self.now;
        let pp = self.pp(r);
        pp.state = PState::Possible;
        pp.primary = Some(e.id);
        pp.initial = (e.x, e.y);
        pp.accepted = false;
        pp.deadline = Some(now.wrapping_add(deadline_ms));
        self.rec(r).t0 = now;
        true
    }

    /// `handleEvent`: the slop checks, then `handle_primary`, then stop on up.
    fn pp_handle(&mut self, r: usize, e: Ptr, post_slop: Option<f32>, handle_primary: fn(&mut Self, usize, Ptr)) {
        let pp = *self.pp(r);
        if pp.state == PState::Possible && pp.primary == Some(e.id) {
            let d = dist((e.x, e.y), pp.initial);
            let pre_past = !pp.accepted && d > TOUCH_SLOP;
            let post_past = pp.accepted && post_slop.map_or(false, |s| d > s);
            if e.kind == PKind::Move && (pre_past || post_past) {
                self.pp_resolve_rejected(r);
                self.stop_tracking(r, e.id);
            } else {
                handle_primary(self, r, e);
            }
        }
        self.stop_if_up(r, e);
    }

    fn pp_resolve_rejected(&mut self, r: usize) {
        if let St::Tap(_) = self.rec(r).st {
            self.tap_resolve_rejected(r);
        } else {
            self.resolve_all(r, Disp::Rejected);
        }
    }

    fn pp_accept(&mut self, r: usize, p: u8) {
        let pp = self.pp(r);
        if pp.primary == Some(p) {
            pp.deadline = None;
            pp.accepted = true;
        }
    }

    fn pp_reject(&mut self, r: usize, p: u8) {
        let pp = self.pp(r);
        if pp.primary == Some(p) && pp.state == PState::Possible {
            pp.deadline = None;
            pp.state = PState::Defunct;
        }
    }

    fn pp_stop_last(&mut self, r: usize) {
        let pp = self.pp(r);
        pp.deadline = None;
        pp.state = PState::Ready;
    }

    // ========================================================================
    //  Tap (BaseTapGestureRecognizer / TapGestureRecognizer)
    // ========================================================================

    fn with_tap(&mut self, r: usize, f: impl FnOnce(&mut TapSt)) {
        if let St::Tap(s) = &mut self.rec(r).st {
            f(s);
        }
    }

    fn tap(&self, r: usize) -> TapSt {
        match self.recs[r].unwrap().st {
            St::Tap(s) => s,
            _ => unreachable!(),
        }
    }

    fn tap_add(&mut self, r: usize, e: Ptr) {
        if self.pp(r).state == PState::Ready {
            self.with_tap(r, |s| {
                *s = TapSt { pp: s.pp, down: Some((e.x, e.y)), up: None, sent_down: false, won: false };
            });
            self.pp_add(r, e, PRESS_TIMEOUT_MS);
        }
    }

    fn tap_handle(&mut self, r: usize, e: Ptr) {
        self.pp_handle(r, e, Some(TOUCH_SLOP), |me, r, e| match e.kind {
            PKind::Up => {
                me.with_tap(r, |s| s.up = Some((e.x, e.y)));
                me.tap_check_up(r);
            }
            PKind::Cancel => {
                me.tap_resolve_rejected(r);
                me.tap_cancel(r);
                me.tap_reset(r);
            }
            _ => {}
        });
    }

    /// BaseTap's `resolve(rejected)`: a tap that had won is cancelled.
    fn tap_resolve_rejected(&mut self, r: usize) {
        if self.tap(r).won {
            self.tap_cancel(r);
            self.tap_reset(r);
        }
        self.resolve_all(r, Disp::Rejected);
    }

    fn tap_accept(&mut self, r: usize, p: u8) {
        self.pp_accept(r, p);
        if self.pp(r).primary == Some(p) {
            self.tap_check_down(r);
            self.with_tap(r, |s| s.won = true);
            self.tap_check_up(r);
        }
    }

    fn tap_reject(&mut self, r: usize, p: u8) {
        self.pp_reject(r, p);
        if self.pp(r).primary == Some(p) {
            self.tap_cancel(r);
            self.tap_reset(r);
        }
    }

    fn tap_check_down(&mut self, r: usize) {
        let s = self.tap(r);
        if s.sent_down {
            return;
        }
        let Some(down) = s.down else { return };
        if let Kind::Tap { events: true, .. } = self.rec(r).kind {
            self.emit(r, ev::TAP_DOWN, |d| {
                d[field::X] = down.0;
                d[field::Y] = down.1;
            });
        }
        self.with_tap(r, |s| s.sent_down = true);
    }

    fn tap_check_up(&mut self, r: usize) {
        let s = self.tap(r);
        let (Some(up), Some(down), true) = (s.up, s.down, s.won) else { return };
        let rec = *self.rec(r);
        if let Kind::Tap { click, events } = rec.kind {
            if events {
                for code in [ev::TAP_UP, ev::TAP] {
                    self.emit(r, code, |d| {
                        d[field::X] = up.0;
                        d[field::Y] = up.1;
                        d[field::TOTAL_X] = up.0 - down.0;
                        d[field::TOTAL_Y] = up.1 - down.1;
                    });
                }
            }
            if click {
                self.out.push(Out::Click { widget: rec.widget, x: up.0, y: up.1 });
            }
        }
        self.tap_reset(r);
    }

    /// The tap lost after its down: TAP_CANCEL if TAP_DOWN went out, and
    /// the click's press highlight goes either way.
    fn tap_cancel(&mut self, r: usize) {
        let s = self.tap(r);
        if s.down.is_none() {
            return;
        }
        let rec = *self.rec(r);
        if let Kind::Tap { click, events } = rec.kind {
            if events && s.sent_down {
                self.emit(r, ev::TAP_CANCEL, |d| {
                    d[field::X] = s.down.unwrap().0;
                    d[field::Y] = s.down.unwrap().1;
                });
            }
            if click {
                self.out.push(Out::ClickCancel { widget: rec.widget });
            }
        }
    }

    fn tap_reset(&mut self, r: usize) {
        self.with_tap(r, |s| {
            s.sent_down = false;
            s.won = false;
            s.up = None;
            s.down = None;
        });
    }

    // ========================================================================
    //  Long press (LongPressGestureRecognizer)
    // ========================================================================

    fn with_long(&mut self, r: usize, f: impl FnOnce(&mut LongSt)) {
        if let St::Long(s) = &mut self.rec(r).st {
            f(s);
        }
    }

    fn long(&self, r: usize) -> LongSt {
        match self.recs[r].unwrap().st {
            St::Long(s) => s,
            _ => unreachable!(),
        }
    }

    fn long_add(&mut self, r: usize, e: Ptr) {
        if self.pp_add(r, e, LONG_PRESS_MS) {
            let now = self.now;
            self.with_long(r, |s| {
                s.lp_accepted = false;
                s.origin = (e.x, e.y);
                s.last = (e.x, e.y);
                s.vel.reset();
                s.vel.add(now, e.x, e.y);
            });
        }
    }

    fn long_deadline(&mut self, r: usize) {
        if self.pp(r).state != PState::Possible {
            return;
        }
        self.resolve_all(r, Disp::Accepted);
        self.with_long(r, |s| s.lp_accepted = true);
        if let Some(p) = self.pp(r).primary {
            self.pp_accept(r, p);
        }
        let s = self.long(r);
        self.emit(r, ev::LONG_PRESS_START, |d| {
            d[field::X] = s.origin.0;
            d[field::Y] = s.origin.1;
        });
    }

    fn long_handle(&mut self, r: usize, e: Ptr) {
        self.pp_handle(r, e, None, |me, r, e| {
            let s = me.long(r);
            match e.kind {
                PKind::Move => {
                    let now = me.now;
                    me.with_long(r, |s| {
                        s.vel.add(now, e.x, e.y);
                        s.last = (e.x, e.y);
                    });
                    if s.lp_accepted {
                        me.emit(r, ev::LONG_PRESS_MOVE, |d| {
                            d[field::X] = e.x;
                            d[field::Y] = e.y;
                            d[field::DX] = e.x - s.last.0;
                            d[field::DY] = e.y - s.last.1;
                            d[field::TOTAL_X] = e.x - s.origin.0;
                            d[field::TOTAL_Y] = e.y - s.origin.1;
                        });
                    }
                }
                PKind::Up => {
                    if s.lp_accepted {
                        let est = s.vel.estimate();
                        me.emit(r, ev::LONG_PRESS_END, |d| {
                            d[field::X] = e.x;
                            d[field::Y] = e.y;
                            d[field::TOTAL_X] = e.x - s.origin.0;
                            d[field::TOTAL_Y] = e.y - s.origin.1;
                            d[field::VX] = est.vx as i32;
                            d[field::VY] = est.vy as i32;
                        });
                    } else {
                        me.resolve_all(r, Disp::Rejected);
                    }
                    me.with_long(r, |s| s.lp_accepted = false);
                }
                PKind::Cancel => {
                    me.resolve_all(r, Disp::Rejected);
                    me.with_long(r, |s| s.lp_accepted = false);
                }
                PKind::Down => {}
            }
        });
    }

    // ========================================================================
    //  Drags (DragGestureRecognizer): pan, horizontal, vertical, scroll
    // ========================================================================

    fn with_drag(&mut self, r: usize, f: impl FnOnce(&mut DragSt)) {
        if let St::Drag(s) = &mut self.rec(r).st {
            f(s);
        }
    }

    fn drag(&self, r: usize) -> DragSt {
        match self.recs[r].unwrap().st {
            St::Drag(s) => s,
            _ => unreachable!(),
        }
    }

    fn drag_add(&mut self, r: usize, e: Ptr) {
        if self.drag(r).state != DState::Ready {
            return;
        }
        self.start_tracking(r, e.id);
        let now = self.now;
        self.rec(r).t0 = now;
        self.with_drag(r, |s| {
            s.state = DState::Possible;
            s.primary = Some(e.id);
            s.initial = (e.x, e.y);
            s.last = (e.x, e.y);
            s.moved = 0.0;
            s.vel.reset();
            s.vel.add(now, e.x, e.y);
        });
    }

    fn drag_handle(&mut self, r: usize, e: Ptr) {
        let s = self.drag(r);
        if s.primary != Some(e.id) {
            return;
        }
        if e.kind == PKind::Move {
            let now = self.now;
            let (dx, dy) = (e.x - s.last.0, e.y - s.last.1);
            self.with_drag(r, |s| {
                s.vel.add(now, e.x, e.y);
                s.last = (e.x, e.y);
            });
            match s.state {
                DState::Accepted => self.drag_update(r, dx, dy),
                DState::Possible => {
                    let (moved, slop) = match s.axis {
                        Axis::Free => (s.moved + libm::sqrtf((dx * dx + dy * dy) as f32), PAN_SLOP),
                        Axis::Horizontal => (s.moved + dx as f32, TOUCH_SLOP),
                        Axis::Vertical => (s.moved + dy as f32, TOUCH_SLOP),
                    };
                    self.with_drag(r, |s| s.moved = moved);
                    if moved.abs() > slop {
                        self.resolve_all(r, Disp::Accepted);
                    }
                }
                DState::Ready => {}
            }
        }
        self.stop_if_up(r, e);
    }

    fn drag_update(&mut self, r: usize, dx: i32, dy: i32) {
        let s = self.drag(r);
        let (dx, dy) = match s.axis {
            Axis::Free => (dx, dy),
            Axis::Horizontal => (dx, 0),
            Axis::Vertical => (0, dy),
        };
        if dx == 0 && dy == 0 {
            return;
        }
        let rec = *self.rec(r);
        if rec.kind == Kind::Scroll {
            self.out.push(Out::Scroll { widget: rec.widget, dy });
            return;
        }
        self.emit(r, ev::DRAG_UPDATE, |d| {
            d[field::X] = s.last.0;
            d[field::Y] = s.last.1;
            d[field::DX] = dx;
            d[field::DY] = dy;
            d[field::TOTAL_X] = s.last.0 - s.initial.0;
            d[field::TOTAL_Y] = s.last.1 - s.initial.1;
        });
    }

    fn drag_accept(&mut self, r: usize, p: u8) {
        let s = self.drag(r);
        if s.primary != Some(p) || s.state == DState::Accepted {
            return;
        }
        // DragStartBehavior.start: the drag starts where it was recognised.
        self.with_drag(r, |s| {
            s.state = DState::Accepted;
            s.initial = s.last;
        });
        if self.rec(r).kind != Kind::Scroll {
            self.emit(r, ev::DRAG_START, |d| {
                d[field::X] = s.last.0;
                d[field::Y] = s.last.1;
            });
        }
    }

    fn drag_stop_last(&mut self, r: usize) {
        let s = self.drag(r);
        match s.state {
            DState::Possible => self.resolve_all(r, Disp::Rejected),
            DState::Accepted => {
                let (vx, vy) = fling(s.vel.estimate(), s.axis);
                let rec = *self.rec(r);
                if rec.kind == Kind::Scroll {
                    self.out.push(Out::Fling { widget: rec.widget, vy });
                } else {
                    self.emit(r, ev::DRAG_END, |d| {
                        d[field::X] = s.last.0;
                        d[field::Y] = s.last.1;
                        d[field::TOTAL_X] = s.last.0 - s.initial.0;
                        d[field::TOTAL_Y] = s.last.1 - s.initial.1;
                        d[field::VX] = vx as i32;
                        d[field::VY] = vy as i32;
                    });
                }
            }
            DState::Ready => {}
        }
        self.with_drag(r, |s| {
            s.state = DState::Ready;
            s.primary = None;
        });
    }

    // ========================================================================
    //  Scale (ScaleGestureRecognizer)
    // ========================================================================

    fn with_scale(&mut self, r: usize, f: impl FnOnce(&mut ScaleSt)) {
        if let St::Scale(s) = &mut self.rec(r).st {
            f(s);
        }
    }

    fn scale(&self, r: usize) -> ScaleSt {
        match self.recs[r].unwrap().st {
            St::Scale(s) => s,
            _ => unreachable!(),
        }
    }

    fn scale_add(&mut self, r: usize, e: Ptr) {
        if self.scale(r).n >= SCALE_PTRS {
            return;
        }
        self.start_tracking(r, e.id);
        let now = self.now;
        self.with_scale(r, |s| {
            if s.state == SState::Ready {
                *s = ScaleSt::new();
                s.state = SState::Possible;
            }
            let mut v = Velocity::default();
            v.add(now, e.x, e.y);
            if let Some(slot) = s.vel.iter_mut().find(|v| v.0 == e.id || v.0 == 0xFF) {
                *slot = (e.id, v);
            }
        });
        if self.scale(r).n == 0 {
            self.rec(r).t0 = now;
        }
    }

    fn scale_handle(&mut self, r: usize, e: Ptr) {
        let now = self.now;
        let mut changed = false;
        let mut start = false;
        self.with_scale(r, |s| match e.kind {
            PKind::Move => {
                if let Some(v) = s.vel.iter_mut().find(|v| v.0 == e.id) {
                    v.1.add(now, e.x, e.y);
                }
                if let Some(p) = s.pts[..s.n].iter_mut().find(|p| p.0 == e.id) {
                    p.1 = e.x as f32;
                    p.2 = e.y as f32;
                }
                start = true;
            }
            PKind::Down => {
                if s.n < SCALE_PTRS {
                    s.pts[s.n] = (e.id, e.x as f32, e.y as f32);
                    s.n += 1;
                }
                changed = true;
                start = true;
            }
            PKind::Up | PKind::Cancel => {
                s.remove(e.id);
                changed = true;
            }
        });
        self.with_scale(r, |s| {
            s.update_lines();
            s.update();
        });
        if !changed || self.scale_reconfigure(r, e.id) {
            self.scale_advance(r, start);
        }
        self.stop_if_up(r, e);
    }

    /// `_reconfigure`: the pointers changed; a started scale ends here.
    fn scale_reconfigure(&mut self, r: usize, p: u8) -> bool {
        let s = self.scale(r);
        self.with_scale(r, |s| s.rebase());
        if s.state == SState::Started {
            let est = s.vel.iter().find(|v| v.0 == p).map(|v| v.1.estimate()).unwrap_or_default();
            let (vx, vy) = fling(est, Axis::Free);
            let focal = s.focal.unwrap_or((0.0, 0.0));
            let n = self.scale(r).n;
            self.emit(r, ev::SCALE_END, |d| {
                d[field::X] = focal.0 as i32;
                d[field::Y] = focal.1 as i32;
                d[field::VX] = vx as i32;
                d[field::VY] = vy as i32;
                d[field::POINTERS] = n as i32;
            });
            self.with_scale(r, |s| s.state = SState::Accepted);
            return false;
        }
        true
    }

    fn scale_advance(&mut self, r: usize, should_start: bool) {
        if self.scale(r).state == SState::Ready {
            self.with_scale(r, |s| s.state = SState::Possible);
        }
        let s = self.scale(r);
        if s.state == SState::Possible {
            let focal = s.focal.unwrap_or((0.0, 0.0));
            let span_delta = (s.span - s.initial_span).abs();
            let (fx, fy) = (focal.0 - s.initial_focal.0, focal.1 - s.initial_focal.1);
            if span_delta > SCALE_SLOP || libm::sqrtf(fx * fx + fy * fy) > PAN_SLOP {
                self.resolve_all(r, Disp::Accepted);
            }
        } else if matches!(s.state, SState::Accepted | SState::Started) {
            self.resolve_all(r, Disp::Accepted);
        }
        if self.scale(r).state == SState::Accepted && should_start {
            self.with_scale(r, |s| s.state = SState::Started);
            self.scale_emit_start(r);
        }
        if self.scale(r).state == SState::Started {
            let s = self.scale(r);
            let focal = s.focal.unwrap_or((0.0, 0.0));
            let ratio = |cur: f32, init: f32| if init > 0.0 { cur / init } else { 1.0 };
            let rot = s.rotation();
            self.emit(r, ev::SCALE_UPDATE, |d| {
                d[field::X] = focal.0 as i32;
                d[field::Y] = focal.1 as i32;
                d[field::DX] = s.delta.0 as i32;
                d[field::DY] = s.delta.1 as i32;
                d[field::TOTAL_X] = (focal.0 - s.initial_focal.0) as i32;
                d[field::TOTAL_Y] = (focal.1 - s.initial_focal.1) as i32;
                d[field::SCALE] = (ratio(s.span, s.initial_span) * 1000.0) as i32;
                d[field::HSCALE] = (ratio(s.hspan, s.initial_hspan) * 1000.0) as i32;
                d[field::VSCALE] = (ratio(s.vspan, s.initial_vspan) * 1000.0) as i32;
                d[field::ROTATION] = (rot * 1000.0) as i32;
                d[field::POINTERS] = s.n as i32;
            });
        }
    }

    fn scale_emit_start(&mut self, r: usize) {
        let s = self.scale(r);
        let focal = s.focal.unwrap_or((0.0, 0.0));
        self.emit(r, ev::SCALE_START, |d| {
            d[field::X] = focal.0 as i32;
            d[field::Y] = focal.1 as i32;
            d[field::POINTERS] = s.n as i32;
        });
    }

    fn scale_accept(&mut self, r: usize, _p: u8) {
        if self.scale(r).state == SState::Possible {
            self.with_scale(r, |s| s.state = SState::Started);
            self.scale_emit_start(r);
            // DragStartBehavior.start: measure from here.
            self.with_scale(r, |s| s.rebase());
        }
    }

    fn scale_reject(&mut self, r: usize, p: u8) {
        self.with_scale(r, |s| {
            s.remove(p);
        });
        self.stop_tracking(r, p);
    }

    fn scale_stop_last(&mut self, r: usize) {
        if self.scale(r).state == SState::Possible {
            self.resolve_all(r, Disp::Rejected);
        }
        self.with_scale(r, |s| s.state = SState::Ready);
    }

    // ========================================================================
    //  Double tap (DoubleTapGestureRecognizer)
    // ========================================================================

    fn with_double(&mut self, r: usize, f: impl FnOnce(&mut DoubleSt)) {
        if let St::Double(s) = &mut self.rec(r).st {
            f(s);
        }
    }

    fn double(&self, r: usize) -> DoubleSt {
        match self.recs[r].unwrap().st {
            St::Double(s) => s,
            _ => unreachable!(),
        }
    }

    fn double_add(&mut self, r: usize, e: Ptr) {
        if let Some((first, up_ms)) = self.double(r).first {
            if dist((e.x, e.y), first.at) > DOUBLE_TAP_SLOP {
                return; // out of bounds: not a second tap
            }
            if self.now.wrapping_sub(up_ms) < DOUBLE_TAP_MIN_MS {
                self.double_reset(r);
            }
        } else {
            self.rec(r).t0 = self.now;
        }
        // `_trackTap`
        self.with_double(r, |s| s.timer = None);
        if !self.double(r).trackers.iter().any(|t| t.is_none()) {
            return;
        }
        self.start_tracking(r, e.id);
        self.with_double(r, |s| {
            if let Some(t) = s.trackers.iter_mut().find(|t| t.is_none()) {
                *t = Some(TapTrack { p: e.id, at: (e.x, e.y) });
            }
        });
    }

    fn double_handle(&mut self, r: usize, e: Ptr) {
        let Some(t) = self.double(r).trackers.iter().flatten().find(|t| t.p == e.id).copied() else { return };
        match e.kind {
            PKind::Up => {
                if self.double(r).first.is_none() {
                    // `_registerFirstTap`
                    let now = self.now;
                    self.with_double(r, |s| s.timer = Some(now.wrapping_add(DOUBLE_TAP_MS)));
                    self.hold(t.p);
                    self.double_untrack(r, t.p);
                    self.double_clear_trackers(r);
                    self.with_double(r, |s| s.first = Some((t, now)));
                } else {
                    // `_registerSecondTap`
                    let first = self.double(r).first.unwrap().0;
                    self.resolve_pointer(r, first.p, Disp::Accepted);
                    self.resolve_pointer(r, t.p, Disp::Accepted);
                    self.double_untrack(r, t.p);
                    self.emit(r, ev::DOUBLE_TAP, |d| {
                        d[field::X] = e.x;
                        d[field::Y] = e.y;
                        d[field::TAPS] = 2;
                    });
                    self.double_reset(r);
                }
            }
            PKind::Move => {
                if dist((e.x, e.y), t.at) > TOUCH_SLOP {
                    self.double_reject_tracker(r, t);
                }
            }
            PKind::Cancel => self.double_reject_tracker(r, t),
            PKind::Down => {}
        }
    }

    /// `_freezeTracker` + remove: no more events from the pointer.
    fn double_untrack(&mut self, r: usize, p: u8) {
        self.rec(r).tracked &= !bit(p);
        self.with_double(r, |s| {
            for t in s.trackers.iter_mut() {
                if t.map_or(false, |t| t.p == p) {
                    *t = None;
                }
            }
        });
    }

    fn double_clear_trackers(&mut self, r: usize) {
        for t in self.double(r).trackers.iter().flatten().copied().collect::<Vec<_>>() {
            self.double_reject_tracker(r, t);
        }
    }

    fn double_reject_tracker(&mut self, r: usize, t: TapTrack) {
        self.double_untrack(r, t.p);
        self.resolve_pointer(r, t.p, Disp::Rejected);
        if let Some((first, _)) = self.double(r).first {
            if first.p == t.p {
                self.double_reset(r);
            } else if self.double(r).trackers.iter().all(|t| t.is_none()) {
                self.double_reset(r);
            }
        }
    }

    fn double_reset(&mut self, r: usize) {
        self.with_double(r, |s| s.timer = None);
        if let Some((first, _)) = self.double(r).first {
            self.with_double(r, |s| s.first = None);
            self.double_reject_tracker(r, first);
            self.release(first.p);
        }
        self.double_clear_trackers(r);
    }

    fn double_reject(&mut self, r: usize, p: u8) {
        let s = self.double(r);
        let t = s.trackers.iter().flatten().find(|t| t.p == p).copied().or_else(|| s.first.map(|f| f.0).filter(|f| f.p == p));
        if let Some(t) = t {
            self.double_reject_tracker(r, t);
        }
    }

    // ========================================================================
    //  Tap and drag (BaseTapAndDragGestureRecognizer, TapAndPan)
    // ========================================================================

    fn with_td(&mut self, r: usize, f: impl FnOnce(&mut TapDragSt)) {
        if let St::TapDrag(s) = &mut self.rec(r).st {
            f(s);
        }
    }

    fn td(&self, r: usize) -> TapDragSt {
        match self.recs[r].unwrap().st {
            St::TapDrag(s) => s,
            _ => unreachable!(),
        }
    }

    fn td_timer_active(&self, s: &TapDragSt) -> bool {
        s.tap_timer.map_or(false, |t| t.wrapping_sub(self.now).wrapping_sub(1) < 0x7FFF_FFFF)
    }

    fn td_add(&mut self, r: usize, e: Ptr) {
        let s = self.td(r);
        if s.primary.map_or(false, |p| p != e.id) || s.state != DState::Ready {
            return;
        }
        // `_TapStatusTrackerMixin.addAllowedPointer`
        self.start_tracking(r, e.id);
        let active = self.td_timer_active(&s);
        if s.tap_timer.is_some() && !active {
            self.td_tracker_reset(r);
        }
        let s = self.td(r);
        let same = active && s.last_tap.map_or(false, |l| dist(l, (e.x, e.y)) <= DOUBLE_TAP_SLOP);
        let now = self.now;
        self.with_td(r, |s| {
            s.up = false;
            if s.down.is_some() && !same {
                s.count = 1;
            } else {
                s.count += 1;
            }
            s.tap_timer = None;
            s.down = Some((e.x, e.y));
            s.last_tap = Some((e.x, e.y));
            s.origin = Some((e.x, e.y));
            // `BaseTapAndDrag.addAllowedPointer`
            s.primary = Some(e.id);
            s.moved_all = 0.0;
            s.state = DState::Possible;
            s.initial = (e.x, e.y);
            s.current = (e.x, e.y);
            s.deadline = Some(now.wrapping_add(PRESS_TIMEOUT_MS));
            s.vel.reset();
            s.vel.add(now, e.x, e.y);
        });
        if self.td(r).count == 1 {
            self.rec(r).t0 = now;
        }
    }

    fn td_handle(&mut self, r: usize, e: Ptr) {
        let s = self.td(r);
        if s.primary != Some(e.id) {
            return;
        }
        let now = self.now;
        // The mixin's `handleEvent`.
        match e.kind {
            PKind::Move => {
                if s.origin.map_or(false, |o| dist((e.x, e.y), o) > TOUCH_SLOP) {
                    self.with_td(r, |s| {
                        s.tap_timer = None;
                        s.last_tap = None;
                    });
                }
            }
            PKind::Up => self.with_td(r, |s| {
                s.up = true;
                if s.down.is_some() {
                    s.tap_timer = Some(now.wrapping_add(DOUBLE_TAP_MS));
                }
            }),
            PKind::Cancel => self.td_tracker_reset(r),
            PKind::Down => {}
        }
        match e.kind {
            PKind::Move => {
                let s = self.td(r);
                let past = s.past_slop || dist((e.x, e.y), s.initial) > TOUCH_SLOP;
                let (dx, dy) = (e.x - s.current.0, e.y - s.current.1);
                self.with_td(r, |s| {
                    s.past_slop = past;
                    s.vel.add(now, e.x, e.y);
                });
                match s.state {
                    DState::Accepted => {
                        self.with_td(r, |s| s.current = (e.x, e.y));
                        let s = self.td(r);
                        self.emit(r, ev::TAP_DRAG_UPDATE, |d| {
                            d[field::X] = e.x;
                            d[field::Y] = e.y;
                            d[field::DX] = dx;
                            d[field::DY] = dy;
                            d[field::TOTAL_X] = e.x - s.initial.0;
                            d[field::TOTAL_Y] = e.y - s.initial.1;
                            d[field::TAPS] = s.count;
                        });
                    }
                    DState::Possible => {
                        self.with_td(r, |s| s.current = (e.x, e.y));
                        if s.start.is_none() {
                            self.td_check_drag(r, e, dx, dy);
                        }
                        let s = self.td(r);
                        if let (Some(start), true) = (s.start, s.won) {
                            self.with_td(r, |s| s.state = DState::Accepted);
                            self.td_accept_drag(r, start);
                        }
                    }
                    DState::Ready => {}
                }
            }
            PKind::Up => match s.state {
                DState::Possible => self.stop_if_up(r, e),
                DState::Accepted => self.td_give_up(r, e.id),
                DState::Ready => {}
            },
            PKind::Cancel => {
                self.with_td(r, |s| s.state = DState::Ready);
                self.td_give_up(r, e.id);
            }
            PKind::Down => {}
        }
    }

    fn td_check_drag(&mut self, r: usize, e: Ptr, dx: i32, dy: i32) {
        let moved = self.td(r).moved_all + libm::sqrtf((dx * dx + dy * dy) as f32);
        self.with_td(r, |s| s.moved_all = moved);
        if moved > PAN_SLOP {
            // eagerVictoryOnDrag
            self.with_td(r, |s| {
                s.start = Some((e.x, e.y));
                s.state = DState::Accepted;
            });
            if !self.td(r).won {
                self.resolve_all(r, Disp::Accepted);
            }
        }
    }

    fn td_accept_drag(&mut self, r: usize, start: (i32, i32)) {
        if !self.td(r).won {
            return;
        }
        // DragStartBehavior.start
        self.with_td(r, |s| {
            s.initial = start;
            s.current = start;
            s.start = None;
        });
        let s = self.td(r);
        self.emit(r, ev::TAP_DRAG_START, |d| {
            d[field::X] = start.0;
            d[field::Y] = start.1;
            d[field::TAPS] = s.count;
        });
    }

    fn td_accept(&mut self, r: usize, p: u8) {
        let s = self.td(r);
        if s.primary != Some(p) {
            return;
        }
        self.with_td(r, |s| {
            s.deadline = None;
            s.accepted_active |= bit(p);
        });
        if s.down.is_some() {
            self.td_check_tap_down(r);
        }
        self.with_td(r, |s| s.won = true);
        if let Some(start) = self.td(r).start {
            self.td_accept_drag(r, start);
        }
        if self.td(r).up {
            self.td_check_tap_up(r, p);
        }
    }

    fn td_stop_last(&mut self, r: usize, p: u8) {
        let s = self.td(r);
        match s.state {
            DState::Ready => {
                self.td_check_cancel(r);
                self.resolve_all(r, Disp::Rejected);
            }
            DState::Possible => {
                if s.past_slop {
                    if s.won {
                        if let Some(down) = s.down {
                            if s.accepted_active & bit(p) == 0 {
                                self.resolve_pointer(r, p, Disp::Rejected);
                            }
                            self.with_td(r, |s| {
                                s.accepted_active &= !bit(p);
                                s.state = DState::Accepted;
                            });
                            self.td_accept_drag(r, down);
                            self.td_check_drag_end(r);
                        }
                    } else {
                        self.td_check_cancel(r);
                        self.resolve_all(r, Disp::Rejected);
                    }
                } else if s.up {
                    self.td_check_tap_up(r, p);
                }
            }
            DState::Accepted => self.td_check_drag_end(r),
        }
        self.with_td(r, |s| {
            s.deadline = None;
            s.start = None;
            s.state = DState::Ready;
            s.past_slop = false;
        });
    }

    fn td_reject(&mut self, r: usize, p: u8) {
        if self.td(r).primary != Some(p) {
            return;
        }
        // Flutter fires no onCancel here; lib/gesture.fl promises one when a
        // TAP_DRAG_DOWN went out (as the class documentation says).
        self.td_check_cancel(r);
        self.td_tracker_reset(r);
        self.with_td(r, |s| s.deadline = None);
        self.td_give_up(r, p);
        self.td_reset_taps(r);
    }

    fn td_deadline(&mut self, r: usize) {
        if self.td(r).down.is_some() {
            self.td_check_tap_down(r);
            if self.td(r).count > 1 {
                self.resolve_all(r, Disp::Accepted);
            }
        }
    }

    fn td_give_up(&mut self, r: usize, p: u8) {
        self.stop_tracking(r, p);
        if !self.alive(r) {
            return;
        }
        if self.td(r).accepted_active & bit(p) == 0 {
            self.resolve_pointer(r, p, Disp::Rejected);
        }
        self.with_td(r, |s| s.accepted_active &= !bit(p));
    }

    fn td_check_tap_down(&mut self, r: usize) {
        let s = self.td(r);
        if s.sent_down {
            return;
        }
        let Some(down) = s.down else { return };
        self.emit(r, ev::TAP_DRAG_DOWN, |d| {
            d[field::X] = down.0;
            d[field::Y] = down.1;
            d[field::TAPS] = s.count;
        });
        self.with_td(r, |s| s.sent_down = true);
    }

    fn td_check_tap_up(&mut self, r: usize, p: u8) {
        let s = self.td(r);
        if !s.won {
            return;
        }
        let at = s.current;
        self.emit(r, ev::TAP_DRAG_UP, |d| {
            d[field::X] = at.0;
            d[field::Y] = at.1;
            d[field::TAPS] = s.count;
        });
        self.td_reset_taps(r);
        if self.td(r).accepted_active & bit(p) == 0 {
            self.resolve_pointer(r, p, Disp::Rejected);
        }
        self.with_td(r, |s| s.accepted_active &= !bit(p));
    }

    fn td_check_drag_end(&mut self, r: usize) {
        let s = self.td(r);
        let (vx, vy) = fling(s.vel.estimate(), Axis::Free);
        self.emit(r, ev::TAP_DRAG_END, |d| {
            d[field::X] = s.current.0;
            d[field::Y] = s.current.1;
            d[field::TOTAL_X] = s.current.0 - s.initial.0;
            d[field::TOTAL_Y] = s.current.1 - s.initial.1;
            d[field::VX] = vx as i32;
            d[field::VY] = vy as i32;
            d[field::TAPS] = s.count;
        });
        self.td_reset_taps(r);
    }

    fn td_check_cancel(&mut self, r: usize) {
        let s = self.td(r);
        if !s.sent_down {
            return;
        }
        let at = s.current;
        self.emit(r, ev::TAP_DRAG_CANCEL, |d| {
            d[field::X] = at.0;
            d[field::Y] = at.1;
            d[field::TAPS] = s.count;
        });
        self.td_reset_taps(r);
    }

    fn td_reset_taps(&mut self, r: usize) {
        self.with_td(r, |s| {
            s.sent_down = false;
            s.won = false;
            s.primary = None;
        });
    }

    fn td_tracker_reset(&mut self, r: usize) {
        self.with_td(r, |s| {
            s.tap_timer = None;
            s.origin = None;
            s.last_tap = None;
            s.count = 0;
            s.down = None;
            s.up = false;
        });
    }
}

impl DragSt {
    fn new(axis: Axis) -> Self {
        Self {
            axis,
            state: DState::Ready,
            primary: None,
            initial: (0, 0),
            last: (0, 0),
            moved: 0.0,
            vel: Velocity::default(),
        }
    }
}

impl TapDragSt {
    fn new() -> Self {
        Self {
            count: 0,
            down: None,
            up: false,
            origin: None,
            last_tap: None,
            tap_timer: None,
            primary: None,
            sent_down: false,
            won: false,
            state: DState::Ready,
            start: None,
            initial: (0, 0),
            current: (0, 0),
            moved_all: 0.0,
            past_slop: false,
            accepted_active: 0,
            deadline: None,
            vel: Velocity::default(),
        }
    }
}

impl ScaleSt {
    fn new() -> Self {
        Self {
            state: SState::Ready,
            pts: [(0, 0.0, 0.0); SCALE_PTRS],
            n: 0,
            vel: [(0xFF, Velocity::default()); SCALE_PTRS],
            focal: None,
            initial_focal: (0.0, 0.0),
            delta: (0.0, 0.0),
            span: 0.0,
            hspan: 0.0,
            vspan: 0.0,
            initial_span: 0.0,
            initial_hspan: 0.0,
            initial_vspan: 0.0,
            line: None,
            initial_line: None,
        }
    }

    fn remove(&mut self, p: u8) {
        if let Some(i) = self.pts[..self.n].iter().position(|q| q.0 == p) {
            self.pts.copy_within(i + 1..self.n, i);
            self.n -= 1;
        }
    }

    /// `_update`: focal point, its delta, the spans.
    fn update(&mut self) {
        let n = self.n.max(1) as f32;
        let (mut fx, mut fy) = (0.0, 0.0);
        for p in &self.pts[..self.n] {
            fx += p.1;
            fy += p.2;
        }
        let focal = (fx / n, fy / n);
        self.delta = match self.focal {
            Some(prev) => (focal.0 - prev.0, focal.1 - prev.1),
            None => (0.0, 0.0),
        };
        self.focal = Some(focal);
        let (mut t, mut h, mut v) = (0.0, 0.0, 0.0);
        for p in &self.pts[..self.n] {
            let (dx, dy) = (focal.0 - p.1, focal.1 - p.2);
            t += libm::sqrtf(dx * dx + dy * dy);
            h += dx.abs();
            v += dy.abs();
        }
        if self.n > 0 {
            let n = self.n as f32;
            (self.span, self.hspan, self.vspan) = (t / n, h / n, v / n);
        } else {
            (self.span, self.hspan, self.vspan) = (0.0, 0.0, 0.0);
        }
    }

    /// `_updateLines`: the line between the first two pointers.
    fn update_lines(&mut self) {
        if self.n < 2 {
            self.initial_line = self.line;
            return;
        }
        let (a, b) = (self.pts[0], self.pts[1]);
        let line = Line { a: a.0, b: b.0, pa: (a.1, a.2), pb: (b.1, b.2) };
        match self.initial_line {
            Some(l) if l.a == a.0 && l.b == b.0 => self.line = Some(line),
            _ => {
                self.initial_line = Some(line);
                self.line = Some(line);
            }
        }
    }

    /// The current values become the reference (`_reconfigure`, accept).
    fn rebase(&mut self) {
        self.initial_focal = self.focal.unwrap_or((0.0, 0.0));
        self.initial_span = self.span;
        self.initial_hspan = self.hspan;
        self.initial_vspan = self.vspan;
        self.initial_line = self.line;
    }

    /// `_computeRotationFactor`, radians, clockwise on the screen. Kept in
    /// (-π, π] (Flutter's difference of two `atan2`s jumps by 2π when the
    /// line crosses the negative x axis).
    fn rotation(&self) -> f32 {
        use core::f32::consts::PI;
        match (self.initial_line, self.line) {
            (Some(i), Some(c)) => {
                let a1 = libm::atan2f(i.pa.1 - i.pb.1, i.pa.0 - i.pb.0);
                let a2 = libm::atan2f(c.pa.1 - c.pb.1, c.pa.0 - c.pb.0);
                let mut a = a2 - a1;
                if a > PI {
                    a -= 2.0 * PI;
                } else if a <= -PI {
                    a += 2.0 * PI;
                }
                a
            }
            _ => 0.0,
        }
    }
}

// ============================================================================
//  Registrations (`onGesture`)
// ============================================================================

/// A widget's `onGesture`: the recognizers it wants, the handler (a 1-based
/// func id, as `@fn` pushes) and the task that registered it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reg {
    pub mask: u16,
    pub func: u16,
    pub task: u8,
}

/// Every widget's registration, by widget pointer. Lives in the VM's pool
/// (`ObjPool::gestures`); a reused pointer starts without one
/// (`alloc_widget`).
#[derive(Default)]
pub struct Registry {
    regs: Vec<(WidgetId, Reg)>,
}

impl Registry {
    pub const fn new() -> Self {
        Self { regs: Vec::new() }
    }

    pub fn get(&self, w: WidgetId) -> Option<Reg> {
        self.regs.iter().find(|r| r.0 == w).map(|r| r.1)
    }

    /// Set (mask ≠ 0) or remove `w`'s registration.
    pub fn set(&mut self, w: WidgetId, reg: Reg) {
        self.regs.retain(|r| r.0 != w);
        if reg.mask != 0 {
            self.regs.push((w, reg));
        }
    }

    pub fn forget(&mut self, w: WidgetId) {
        if !self.regs.is_empty() {
            self.regs.retain(|r| r.0 != w);
        }
    }

    pub fn clear(&mut self) {
        self.regs.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.regs.is_empty()
    }

    /// Drop the registrations of task `task` (it ended).
    pub fn forget_task(&mut self, task: u8) {
        self.regs.retain(|r| r.1.task != task);
    }
}

/// The candidates a widget's registration adds, in Flutter's order for one
/// widget: tap (merged with the widget's click), double tap, long press,
/// drags, scale, tap-and-drag.
pub fn push_candidates(out: &mut Vec<Candidate>, widget: WidgetId, origin: (i32, i32), mask: u16, click: bool) {
    let mut add = |kind| out.push(Candidate { widget, kind, origin });
    if click || mask & G_TAP != 0 {
        add(Kind::Tap { click, events: mask & G_TAP != 0 });
    }
    if mask & G_DOUBLE_TAP != 0 {
        add(Kind::DoubleTap);
    }
    if mask & G_LONG_PRESS != 0 {
        add(Kind::LongPress);
    }
    if mask & G_PAN != 0 {
        add(Kind::Drag(Axis::Free));
    }
    if mask & G_HDRAG != 0 {
        add(Kind::Drag(Axis::Horizontal));
    }
    if mask & G_VDRAG != 0 {
        add(Kind::Drag(Axis::Vertical));
    }
    if mask & G_SCALE != 0 {
        add(Kind::Scale);
    }
    if mask & G_TAP_DRAG != 0 {
        add(Kind::TapDrag);
    }
}

// ============================================================================
//  Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const CHILD: WidgetId = WidgetId(5);
    const PARENT: WidgetId = WidgetId(2);

    fn down(id: u8, x: i32, y: i32, ms: u32) -> Ptr {
        Ptr { kind: PKind::Down, id, x, y, ms }
    }
    fn mv(id: u8, x: i32, y: i32, ms: u32) -> Ptr {
        Ptr { kind: PKind::Move, id, x, y, ms }
    }
    fn up(id: u8, x: i32, y: i32, ms: u32) -> Ptr {
        Ptr { kind: PKind::Up, id, x, y, ms }
    }

    fn cand(widget: WidgetId, kind: Kind) -> Candidate {
        Candidate { widget, kind, origin: (0, 0) }
    }

    fn tap_ev() -> Kind {
        Kind::Tap { click: false, events: true }
    }

    /// The (widget, event) pairs of the outputs, clicks as event 100,
    /// click cancels 101, scrolls 102, flings 103.
    fn events(e: &mut Engine) -> Vec<(WidgetId, u8)> {
        e.take()
            .into_iter()
            .map(|o| match o {
                Out::Event { widget, ev, .. } => (widget, ev),
                Out::Click { widget, .. } => (widget, 100),
                Out::ClickCancel { widget } => (widget, 101),
                Out::Scroll { widget, .. } => (widget, 102),
                Out::Fling { widget, .. } => (widget, 103),
            })
            .collect()
    }

    fn details(e: &mut Engine, code: u8) -> Vec<Detail> {
        e.take()
            .into_iter()
            .filter_map(|o| match o {
                Out::Event { ev, d, .. } if ev == code => Some(d),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_lone_tap_wins_at_once_and_fires_on_up() {
        let mut e = Engine::new();
        e.pointer(down(0, 10, 10, 0), &[cand(CHILD, tap_ev())]);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_DOWN)]);
        e.pointer(up(0, 12, 10, 50), &[]);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_UP), (CHILD, ev::TAP)]);
        assert!(e.idle());
    }

    #[test]
    fn tap_down_waits_for_the_press_timeout_against_a_drag() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(PARENT, Kind::Drag(Axis::Horizontal))];
        e.pointer(down(0, 10, 10, 0), &c);
        assert!(events(&mut e).is_empty());
        e.tick(99);
        assert!(events(&mut e).is_empty());
        e.tick(100);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_DOWN)]);
        // Released without moving: the sweep gives it to the child.
        e.pointer(up(0, 10, 10, 150), &[]);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_UP), (CHILD, ev::TAP)]);
        assert!(e.idle());
    }

    #[test]
    fn a_parent_drag_beats_a_child_tap_past_the_slop() {
        let mut e = Engine::new();
        let c = [cand(CHILD, Kind::Tap { click: true, events: false }), cand(PARENT, Kind::Drag(Axis::Horizontal))];
        e.pointer(down(0, 100, 100, 0), &c);
        e.pointer(mv(0, 110, 100, 16), &[]);
        assert!(events(&mut e).is_empty());
        e.pointer(mv(0, 125, 102, 32), &[]);
        // The tap rejects itself at 18 px (pre-accept slop): the drag is the
        // only member left and wins (dx 25 > 18 as well).
        let ev = events(&mut e);
        assert_eq!(ev[0], (CHILD, 101));
        assert!(ev.contains(&(PARENT, ev::DRAG_START)));
        e.pointer(mv(0, 140, 103, 48), &[]);
        let d = details(&mut e, ev::DRAG_UPDATE);
        assert_eq!((d[0][field::DX], d[0][field::DY]), (15, 0));
        e.pointer(up(0, 140, 103, 60), &[]);
        assert_eq!(events(&mut e), vec![(PARENT, ev::DRAG_END)]);
        assert!(e.idle());
    }

    #[test]
    fn a_vertical_drag_ignores_horizontal_movement() {
        let mut e = Engine::new();
        let c = [cand(CHILD, Kind::Drag(Axis::Vertical)), cand(PARENT, Kind::Drag(Axis::Horizontal))];
        e.pointer(down(0, 100, 100, 0), &c);
        e.pointer(mv(0, 130, 105, 16), &[]);
        let ev = events(&mut e);
        assert_eq!(ev, vec![(PARENT, ev::DRAG_START)]);
        e.pointer(up(0, 130, 105, 32), &[]);
        e.take();
    }

    #[test]
    fn a_fling_reports_its_velocity() {
        let mut e = Engine::new();
        e.pointer(down(0, 0, 100, 0), &[cand(PARENT, Kind::Drag(Axis::Horizontal)), cand(CHILD, tap_ev())]);
        for i in 1..=6 {
            e.pointer(mv(0, i * 16, 100, i as u32 * 16), &[]);
        }
        e.take();
        e.pointer(up(0, 96, 100, 100), &[]);
        let d = details(&mut e, ev::DRAG_END);
        assert!((d[0][field::VX] - 1000).abs() <= 5, "vx {}", d[0][field::VX]);
        assert_eq!(d[0][field::VY], 0);
    }

    #[test]
    fn long_press_wins_after_500_ms_and_cancels_the_tap() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(CHILD, Kind::LongPress)];
        e.pointer(down(0, 50, 50, 0), &c);
        e.tick(100);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_DOWN)]);
        e.tick(500);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_CANCEL), (CHILD, ev::LONG_PRESS_START)]);
        e.pointer(mv(0, 60, 50, 550), &[]);
        assert_eq!(events(&mut e), vec![(CHILD, ev::LONG_PRESS_MOVE)]);
        e.pointer(up(0, 60, 50, 600), &[]);
        assert_eq!(events(&mut e), vec![(CHILD, ev::LONG_PRESS_END)]);
        assert!(e.idle());
    }

    #[test]
    fn a_quick_tap_beats_a_long_press() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(CHILD, Kind::LongPress)];
        e.pointer(down(0, 50, 50, 0), &c);
        e.pointer(up(0, 50, 50, 200), &[]);
        e.tick(200);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_DOWN), (CHILD, ev::TAP_UP), (CHILD, ev::TAP)]);
        e.tick(600);
        assert!(events(&mut e).is_empty());
        assert!(e.idle());
    }

    #[test]
    fn double_tap_holds_the_single_tap_until_it_times_out() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(CHILD, Kind::DoubleTap)];
        e.pointer(down(0, 50, 50, 0), &c);
        e.pointer(up(0, 50, 50, 60), &[]);
        assert!(events(&mut e).is_empty());
        e.tick(359);
        assert!(events(&mut e).is_empty());
        e.tick(360);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_DOWN), (CHILD, ev::TAP_UP), (CHILD, ev::TAP)]);
        assert!(e.idle());
    }

    #[test]
    fn double_tap_wins_on_the_second_up() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(CHILD, Kind::DoubleTap)];
        e.pointer(down(0, 50, 50, 0), &c);
        e.pointer(up(0, 50, 50, 60), &[]);
        e.pointer(down(1, 55, 52, 200), &c);
        e.pointer(up(1, 55, 52, 260), &[]);
        let ev = events(&mut e);
        assert!(ev.contains(&(CHILD, ev::DOUBLE_TAP)), "{ev:?}");
        assert!(!ev.contains(&(CHILD, ev::TAP)), "{ev:?}");
        e.tick(1000);
        assert!(!events(&mut e).contains(&(CHILD, ev::TAP)));
        assert!(e.idle());
    }

    #[test]
    fn a_far_second_tap_is_no_double_tap() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(CHILD, Kind::DoubleTap)];
        e.pointer(down(0, 50, 50, 0), &c);
        e.pointer(up(0, 50, 50, 60), &[]);
        e.pointer(down(1, 300, 50, 200), &c);
        e.pointer(up(1, 300, 50, 260), &[]);
        e.tick(1000);
        let ev = events(&mut e);
        // As in Flutter, the first tap is still undecided (held by the double
        // tap) when the far one starts, and the tap recognizer drops it.
        assert_eq!(ev.iter().filter(|x| x.1 == ev::TAP).count(), 1, "{ev:?}");
        assert!(!ev.contains(&(CHILD, ev::DOUBLE_TAP)));
        assert!(e.idle());
    }

    #[test]
    fn scale_measures_span_and_rotation_from_where_it_won() {
        let mut e = Engine::new();
        let c = [cand(PARENT, Kind::Scale)];
        e.pointer(down(0, 100, 100, 0), &c);
        e.pointer(down(1, 200, 100, 10), &c);
        // Alone in the first pointer's arena, the scale won at once (a
        // one-finger pan); the second finger ends that and starts over.
        assert_eq!(events(&mut e), vec![(PARENT, ev::SCALE_START), (PARENT, ev::SCALE_END)]);
        // Spread to 200 apart in steps.
        for (i, x) in [(1u32, 220), (2, 260), (3, 300)] {
            e.pointer(mv(1, x, 100, 10 + i * 16), &[]);
        }
        let all = e.take();
        let starts = all.iter().filter(|o| matches!(o, Out::Event { ev: ev::SCALE_START, .. })).count();
        assert_eq!(starts, 1);
        let last = all
            .iter()
            .rev()
            .find_map(|o| match o {
                Out::Event { ev: ev::SCALE_UPDATE, d, .. } => Some(*d),
                _ => None,
            })
            .unwrap();
        // Measured from the second finger's down: span 50, now 100.
        assert_eq!(last[field::SCALE], 2000);
        assert_eq!(last[field::POINTERS], 2);
        // Rotate the second finger a quarter turn round the first.
        e.pointer(mv(1, 100, 300, 80), &[]);
        let d = details(&mut e, ev::SCALE_UPDATE);
        assert!((d[0][field::ROTATION] - 1571).abs() <= 2, "{}", d[0][field::ROTATION]);
        e.pointer(up(1, 100, 300, 90), &[]);
        assert_eq!(events(&mut e), vec![(PARENT, ev::SCALE_END)]);
        e.pointer(up(0, 100, 100, 100), &[]);
        e.take();
        assert!(e.idle());
    }

    #[test]
    fn double_tap_wins_next_to_a_scale_on_the_same_widget() {
        let mut out = Vec::new();
        push_candidates(&mut out, CHILD, (0, 0), G_SCALE | G_DOUBLE_TAP, false);
        let mut e = Engine::new();
        e.pointer(down(0, 50, 50, 0), &out);
        e.pointer(up(0, 50, 50, 150), &[]);
        e.tick(200);
        e.pointer(down(1, 50, 50, 200), &out);
        e.pointer(up(1, 50, 50, 350), &[]);
        e.tick(1000);
        let ev = events(&mut e);
        assert!(ev.contains(&(CHILD, ev::DOUBLE_TAP)), "{ev:?}");
        assert!(e.idle());
    }

    #[test]
    fn a_child_tap_beats_a_parent_scale_when_nothing_moves() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(PARENT, Kind::Scale)];
        e.pointer(down(0, 100, 100, 0), &c);
        e.pointer(up(0, 100, 100, 50), &[]);
        assert_eq!(events(&mut e), vec![(CHILD, ev::TAP_DOWN), (CHILD, ev::TAP_UP), (CHILD, ev::TAP)]);
        assert!(e.idle());
    }

    #[test]
    fn tap_and_drag_counts_consecutive_taps_and_drags_after_them() {
        let mut e = Engine::new();
        let c = [cand(CHILD, Kind::TapDrag)];
        e.pointer(down(0, 50, 50, 0), &c);
        e.pointer(up(0, 50, 50, 40), &[]);
        let d = details(&mut e, ev::TAP_DRAG_UP);
        assert_eq!(d[0][field::TAPS], 1);
        // Second tap within 300 ms and 100 px: count 2, then a drag.
        e.pointer(down(0, 60, 55, 200), &c);
        let d = details(&mut e, ev::TAP_DRAG_DOWN);
        assert_eq!(d[0][field::TAPS], 2);
        e.pointer(mv(0, 60, 80, 216), &[]);
        e.pointer(mv(0, 60, 120, 232), &[]);
        let ev = events(&mut e);
        assert!(ev.contains(&(CHILD, ev::TAP_DRAG_START)), "{ev:?}");
        e.pointer(up(0, 60, 120, 250), &[]);
        let d = details(&mut e, ev::TAP_DRAG_END);
        assert_eq!(d[0][field::TAPS], 2);
        // After the timeout the series starts over.
        e.tick(1000);
        e.pointer(down(0, 60, 55, 1000), &c);
        let d = details(&mut e, ev::TAP_DRAG_DOWN);
        assert_eq!(d[0][field::TAPS], 1);
        e.pointer(up(0, 60, 55, 1040), &[]);
        e.tick(2000);
        e.take();
        assert!(e.idle());
    }

    #[test]
    fn scroll_drag_moves_and_flings() {
        let mut e = Engine::new();
        let c = [cand(CHILD, Kind::Tap { click: true, events: false }), cand(PARENT, Kind::Scroll)];
        e.pointer(down(0, 100, 300, 0), &c);
        let mut y = 300;
        for i in 1..=6u32 {
            y -= 20;
            e.pointer(mv(0, 100, y, i * 16), &[]);
        }
        let outs = e.take();
        assert!(matches!(outs[0], Out::ClickCancel { widget: CHILD }));
        let scrolled: i32 = outs
            .iter()
            .filter_map(|o| match o {
                Out::Scroll { dy, .. } => Some(*dy),
                _ => None,
            })
            .sum();
        // The slop (first 20 px step) is not scrolled: DragStartBehavior.start.
        assert_eq!(scrolled, -100);
        e.pointer(up(0, 100, y, 100), &[]);
        match e.take()[..] {
            [Out::Fling { widget: PARENT, vy }] => assert!((vy + 1250.0).abs() < 10.0, "vy {vy}"),
            ref o => panic!("{o:?}"),
        }
    }

    #[test]
    fn forgetting_a_widget_drops_its_recognizers() {
        let mut e = Engine::new();
        let c = [cand(CHILD, tap_ev()), cand(PARENT, Kind::Drag(Axis::Free))];
        e.pointer(down(0, 10, 10, 0), &c);
        e.forget(|w| w == CHILD);
        // The drag is alone now and wins.
        assert_eq!(events(&mut e), vec![(PARENT, ev::DRAG_START)]);
        e.pointer(up(0, 10, 10, 30), &[]);
        e.take();
        assert!(e.idle());
    }

    #[test]
    fn velocity_of_samples_on_a_line() {
        let mut v = Velocity::default();
        for i in 0..8u32 {
            v.add(i * 10, (i * 5) as i32, 0);
        }
        let est = v.estimate();
        assert!((est.vx - 500.0).abs() < 0.5, "{}", est.vx);
    }
}
