/// Backend abstraction for touch input.
/// Hardware backends provide raw press detection and sample reading;
/// the framework provides state-machine debouncing.

// === Calibration ===

#[derive(Clone, Copy)]
pub struct CalParams {
    pub xy_swap: bool,
    pub x_flip: bool,
    pub y_flip: bool,
    pub x_min: u16,
    pub x_max: u16,
    pub y_min: u16,
    pub y_max: u16,
}

impl CalParams {
    pub fn default() -> Self {
        Self {
            xy_swap: true,
            x_flip: true,
            y_flip: true,
            x_min: 0,
            x_max: 4095,
            y_min: 0,
            y_max: 4095,
        }
    }

    pub fn to_bytes(&self) -> [u8; 9] {
        let mut buf = [0u8; 9];
        buf[0] = (self.xy_swap as u8) | ((self.x_flip as u8) << 1) | ((self.y_flip as u8) << 2);
        buf[1..3].copy_from_slice(&self.x_min.to_le_bytes());
        buf[3..5].copy_from_slice(&self.x_max.to_le_bytes());
        buf[5..7].copy_from_slice(&self.y_min.to_le_bytes());
        buf[7..9].copy_from_slice(&self.y_max.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < 9 {
            return None;
        }
        Some(Self {
            xy_swap: buf[0] & 0x01 != 0,
            x_flip: buf[0] & 0x02 != 0,
            y_flip: buf[0] & 0x04 != 0,
            x_min: u16::from_le_bytes([buf[1], buf[2]]),
            x_max: u16::from_le_bytes([buf[3], buf[4]]),
            y_min: u16::from_le_bytes([buf[5], buf[6]]),
            y_max: u16::from_le_bytes([buf[7], buf[8]]),
        })
    }
}

// === Backend trait ===

pub trait TouchBackend {
    /// Fast path: true if the input device reports a press right now.
    fn is_active(&self) -> bool;

    /// Read a screen-space (x, y) sample if currently pressed.
    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)>;

    /// Consecutive `None` samples that end a press. Backends that debounce
    /// lift-off themselves (e.g. in an interrupt) can return 1.
    fn release_debounce(&self) -> u8 {
        RELEASE_DEBOUNCE
    }

    /// Every finger down (multitouch panels): screen-space points with ids
    /// that stay the same while a finger stays down (< 32). Returns how many
    /// were written to `out`. Default: the single `read_screen` point, id 0.
    fn read_points(&self, cal: &CalParams, out: &mut [TouchPoint]) -> usize {
        match self.read_screen(cal) {
            Some((x, y)) if !out.is_empty() => {
                out[0] = TouchPoint { id: 0, x, y };
                1
            }
            _ => 0,
        }
    }

    /// Backends that queue finger samples as they come (an interrupt, another
    /// core) instead of being polled: moves the queued samples, oldest first,
    /// into `out` and returns how many (0 = none waiting). `None` (the
    /// default) = this backend is polled through [`read_points`](Self::read_points)
    /// right now; a backend may switch between the two at run time.
    ///
    /// Every finger's Down comes before its Moves and its Up. A Down for a
    /// finger already down counts as a Move, an Up or Move for one not down
    /// is ignored (a backend resyncing after it dropped samples may repeat).
    fn read_samples(&self, _cal: &CalParams, _out: &mut [TouchSample]) -> Option<usize> {
        None
    }
}

/// What happened to a finger in a [`TouchSample`].
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum SampleKind {
    #[default]
    Down,
    Move,
    Up,
}

/// One queued finger event (`TouchBackend::read_samples`): screen-space
/// position and the `SystickBackend::millis` time it was read at.
#[derive(Clone, Copy, Default, Debug)]
pub struct TouchSample {
    pub kind: SampleKind,
    pub id: u8,
    pub x: u16,
    pub y: u16,
    pub ms: u32,
}

/// Samples taken from a queued backend per poll (the rest wait for the next).
pub const SAMPLES_PER_POLL: usize = 64;

/// One finger of a multitouch sample.
#[derive(Clone, Copy, Default, Debug)]
pub struct TouchPoint {
    pub id: u8,
    pub x: u16,
    pub y: u16,
}

/// Most fingers tracked at once.
pub const MAX_POINTERS: usize = 10;

/// Backend for boards without a touch screen: never pressed.
pub struct NoTouch;

impl TouchBackend for NoTouch {
    fn is_active(&self) -> bool {
        false
    }

    fn read_screen(&self, _cal: &CalParams) -> Option<(u16, u16)> {
        None
    }
}

// === Touch state machine ===

pub const RELEASE_DEBOUNCE: u8 = 5;

#[derive(Clone, Copy, PartialEq)]
enum TouchState {
    Idle,
    Pressed,
    Held,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TouchEventKind {
    Press,
    Hold,
    Release,
}

#[derive(Clone, Copy)]
pub struct TouchEvent {
    pub kind: TouchEventKind,
    pub x: u16,
    pub y: u16,
}

/// A finger's event (`TouchImpl::poll_pointers`): Press = down, Hold = still
/// down (every poll, moved or not, and every queued move), Release = up.
#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
    pub kind: TouchEventKind,
    pub id: u8,
    pub x: u16,
    pub y: u16,
    /// When a queued backend read it (`TouchSample::ms`); None = now.
    pub ms: Option<u32>,
}

#[derive(Clone, Copy)]
struct Finger {
    id: u8,
    x: u16,
    y: u16,
    fails: u8,
}

pub struct TouchImpl<B: TouchBackend> {
    be: B,
    state: TouchState,
    fail_count: u8,
    last_x: u16,
    last_y: u16,
    pub cal: CalParams,
    /// Fingers down (`poll_pointers`).
    fingers: [Option<Finger>; MAX_POINTERS],
}

impl<B: TouchBackend> TouchImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self {
            be,
            state: TouchState::Idle,
            fail_count: 0,
            last_x: 0,
            last_y: 0,
            cal: CalParams::default(),
            fingers: [None; MAX_POINTERS],
        }
    }

    /// No touch is down or being released (nothing to track between polls).
    pub fn is_idle(&self) -> bool {
        matches!(self.state, TouchState::Idle) && self.fingers.iter().all(|f| f.is_none())
    }

    /// Multitouch poll: one event per finger — Release for the ones gone
    /// (after the backend's release debounce, at their last position), Hold
    /// for the ones still down, then Press for the new ones. A queued backend
    /// (`read_samples`) gives each sample in order with its time instead,
    /// then a Hold for each finger still down that the samples didn't move.
    /// Don't mix with [`poll`](Self::poll) on one `TouchImpl`.
    pub fn poll_pointers(&mut self, mut f: impl FnMut(PointerEvent)) {
        let mut buf = [TouchSample::default(); SAMPLES_PER_POLL];
        if let Some(n) = self.be.read_samples(&self.cal, &mut buf) {
            self.apply_samples(&buf[..n.min(SAMPLES_PER_POLL)], &mut f);
            return;
        }
        let any = self.fingers.iter().any(|f| f.is_some());
        if !any && !self.be.is_active() {
            return;
        }
        let mut pts = [TouchPoint::default(); MAX_POINTERS];
        let n = self.be.read_points(&self.cal, &mut pts).min(MAX_POINTERS);
        let pts = &pts[..n];
        let debounce = self.be.release_debounce().max(1);
        for slot in self.fingers.iter_mut() {
            let Some(fg) = slot else { continue };
            match pts.iter().find(|p| p.id == fg.id) {
                Some(p) => {
                    (fg.x, fg.y, fg.fails) = (p.x, p.y, 0);
                    f(PointerEvent { kind: TouchEventKind::Hold, id: fg.id, x: fg.x, y: fg.y, ms: None });
                }
                None => {
                    fg.fails += 1;
                    if fg.fails >= debounce {
                        f(PointerEvent { kind: TouchEventKind::Release, id: fg.id, x: fg.x, y: fg.y, ms: None });
                        *slot = None;
                    }
                }
            }
        }
        for p in pts {
            if self.fingers.iter().flatten().any(|fg| fg.id == p.id) {
                continue;
            }
            if let Some(slot) = self.fingers.iter_mut().find(|s| s.is_none()) {
                *slot = Some(Finger { id: p.id & 31, x: p.x, y: p.y, fails: 0 });
                f(PointerEvent { kind: TouchEventKind::Press, id: p.id & 31, x: p.x, y: p.y, ms: None });
            }
        }
    }

    /// [`poll_pointers`](Self::poll_pointers) for a queued backend.
    fn apply_samples(&mut self, samples: &[TouchSample], f: &mut impl FnMut(PointerEvent)) {
        // Bit n = slot n got a sample this poll.
        let mut seen = 0u32;
        for s in samples {
            let id = s.id & 31;
            let at = self.fingers.iter().position(|fg| matches!(fg, Some(fg) if fg.id == id));
            let ev = |kind| PointerEvent { kind, id, x: s.x, y: s.y, ms: Some(s.ms) };
            match (s.kind, at) {
                (SampleKind::Down | SampleKind::Move, Some(i)) => {
                    self.fingers[i] = Some(Finger { id, x: s.x, y: s.y, fails: 0 });
                    seen |= 1 << i;
                    f(ev(TouchEventKind::Hold));
                }
                (SampleKind::Down, None) => {
                    if let Some(i) = self.fingers.iter().position(|fg| fg.is_none()) {
                        self.fingers[i] = Some(Finger { id, x: s.x, y: s.y, fails: 0 });
                        seen |= 1 << i;
                        f(ev(TouchEventKind::Press));
                    }
                }
                (SampleKind::Up, Some(i)) => {
                    // Released where the finger last was (the Up may carry no position).
                    let fg = self.fingers[i].take().unwrap();
                    seen &= !(1 << i);
                    f(PointerEvent { kind: TouchEventKind::Release, id, x: fg.x, y: fg.y, ms: Some(s.ms) });
                }
                (SampleKind::Move | SampleKind::Up, None) => {}
            }
        }
        for (i, fg) in self.fingers.iter().enumerate() {
            if let Some(fg) = fg {
                if seen & (1 << i) == 0 {
                    f(PointerEvent { kind: TouchEventKind::Hold, id: fg.id, x: fg.x, y: fg.y, ms: None });
                }
            }
        }
    }

    pub fn poll(&mut self) -> Option<TouchEvent> {
        if self.state == TouchState::Idle && !self.be.is_active() {
            return None;
        }

        let sample = self.be.read_screen(&self.cal);

        match self.state {
            TouchState::Idle => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.fail_count = 0;
                    self.state = TouchState::Pressed;
                    Some(TouchEvent {
                        kind: TouchEventKind::Press,
                        x,
                        y,
                    })
                } else {
                    None
                }
            }
            TouchState::Pressed | TouchState::Held => {
                if let Some((x, y)) = sample {
                    self.last_x = x;
                    self.last_y = y;
                    self.fail_count = 0;
                    self.state = TouchState::Held;
                    Some(TouchEvent {
                        kind: TouchEventKind::Hold,
                        x,
                        y,
                    })
                } else {
                    self.fail_count += 1;
                    if self.fail_count >= self.be.release_debounce() {
                        self.state = TouchState::Idle;
                        self.fail_count = 0;
                        Some(TouchEvent {
                            kind: TouchEventKind::Release,
                            x: self.last_x,
                            y: self.last_y,
                        })
                    } else {
                        None
                    }
                }
            }
        }
    }
}

// Hit test — pure widget-tree logic (ObjPool lives in this crate).
pub fn hit_test(tree: &mut crate::pool::ObjPool, x: u16, y: u16) -> crate::widget::WidgetId {
    use crate::widget::FLAG_CLICKABLE;
    let dfs = tree.dfs_order();
    let mut result = crate::widget::WidgetId::NONE;

    for i in 0..dfs.len() {
        let id = dfs[i];
        let w = tree.get(id);
        if w.flags & FLAG_CLICKABLE == 0 || !tree.is_tree_visible(id) {
            continue;
        }
        let abs = tree.absolute_rect(id);
        if (x as i16) >= abs.x
            && (x as i16) < abs.right()
            && (y as i16) >= abs.y
            && (y as i16) < abs.bottom()
        {
            let sp = tree.scroll_parent(id);
            if sp.is_some() {
                let viewport = tree.scroll_viewport(sp);
                if !abs.intersects(&viewport) {
                    continue;
                }
            }
            result = id;
        }
    }
    result
}

#[cfg(feature = "mock")]
pub type Touch = TouchImpl<crate::mock::MockTouch>;
