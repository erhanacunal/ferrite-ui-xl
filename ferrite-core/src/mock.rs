//! Host-only mock platform — exists purely so the device-agnostic framework can be
//! monomorphized and type-checked on the host (`cargo check --features mock`) without
//! an embedded toolchain. Backends are no-ops; they must never be run, only compiled.
//!
//! Receivers mirror the real backends exactly (`&self` everywhere) so the mock cannot
//! hide a trait-bound mismatch that would only surface on real hardware.

use crate::backlight::BacklightBackend;
use crate::flash::FlashBackend;
use crate::gpio::{GpioBackend, MODE_INPUT_PULLDOWN, MODE_INPUT_PULLUP, MODE_OUTPUT};
use crate::lcd::LcdBackend;
use crate::platform::Platform;
use crate::rtc::{DateTime, RtcBackend};
use crate::systick::SystickBackend;
use crate::touch::{CalParams, TouchBackend};
use crate::usart::UsartBackend;

pub struct MockLcd;
impl LcdBackend for MockLcd {
    const WIDTH: u16 = 800;
    const HEIGHT: u16 = 480;
    fn begin_frame(&mut self) {}
    fn end_frame(&mut self) {}
    fn back_buf(&self) -> u8 {
        0
    }
    fn fill_rect(&self, _x: u16, _y: u16, _w: u16, _h: u16, _color: u16) {}
    fn begin_pixels(&self, _x: u16, _y: u16, _w: u16, _h: u16) {}
    fn write_pixel(&self, _color: u16) {}
    fn send_command(&self, _cmd: u16) {}
    fn send_data(&self, _data: u16) {}
}

#[derive(Clone)]
pub struct MockFlash;

#[cfg(feature = "host")]
std::thread_local! {
    /// What `MockFlash` reads, from address 0 (zeros past its end): empty
    /// unless a test sets an image with [`set_flash_image`].
    static FLASH_IMAGE: core::cell::RefCell<alloc::vec::Vec<u8>> =
        const { core::cell::RefCell::new(alloc::vec::Vec::new()) };
}

/// Make `MockFlash` read `image` on this thread (tests with a filesystem).
#[cfg(feature = "host")]
pub fn set_flash_image(image: alloc::vec::Vec<u8>) {
    FLASH_IMAGE.with(|f| *f.borrow_mut() = image);
}

impl FlashBackend for MockFlash {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        buf.fill(0);
        #[cfg(feature = "host")]
        FLASH_IMAGE.with(|f| {
            let image = f.borrow();
            let start = (addr as usize).min(image.len());
            let n = buf.len().min(image.len() - start);
            buf[..n].copy_from_slice(&image[start..start + n]);
        });
        #[cfg(not(feature = "host"))]
        let _ = addr;
    }
    fn write(&self, _addr: u32, _data: &[u8]) {}
    fn erase_sector(&self, _addr: u32) {}
    fn read_id(&self) -> (u8, u8, u8) {
        (0xEF, 0x40, 0x19)
    }
}

#[cfg(feature = "host")]
std::thread_local! {
    /// The fingers `MockTouch` reports ([`set_touch`]) and the time
    /// `MockSystick` reports ([`set_millis`]).
    static TOUCH: core::cell::RefCell<alloc::vec::Vec<crate::touch::TouchPoint>> =
        const { core::cell::RefCell::new(alloc::vec::Vec::new()) };
    static MILLIS: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    /// Queued samples ([`queue_touch`]): `Some` = `MockTouch` is a queued backend.
    static SAMPLES: core::cell::RefCell<Option<alloc::collections::VecDeque<crate::touch::TouchSample>>> =
        const { core::cell::RefCell::new(None) };
}

/// Makes this thread's `MockTouch` a queued backend (`read_samples`) and
/// queues `samples` (kind, id, x, y, ms) after the ones still waiting.
#[cfg(feature = "host")]
pub fn queue_touch(samples: &[(crate::touch::SampleKind, u8, u16, u16, u32)]) {
    SAMPLES.with(|q| {
        let mut q = q.borrow_mut();
        let q = q.get_or_insert_with(Default::default);
        for &(kind, id, x, y, ms) in samples {
            q.push_back(crate::touch::TouchSample { kind, id, x, y, ms });
        }
    });
}

/// Fingers down on this thread's `MockTouch`: (id, x, y).
#[cfg(feature = "host")]
pub fn set_touch(points: &[(u8, u16, u16)]) {
    TOUCH.with(|t| *t.borrow_mut() = points.iter().map(|&(id, x, y)| crate::touch::TouchPoint { id, x, y }).collect());
}

/// What `MockSystick::millis` returns on this thread (0 by default).
#[cfg(feature = "host")]
pub fn set_millis(ms: u32) {
    MILLIS.with(|m| m.set(ms));
}

pub struct MockTouch;
impl TouchBackend for MockTouch {
    fn is_active(&self) -> bool {
        #[cfg(feature = "host")]
        if let Some(waiting) = SAMPLES.with(|q| q.borrow().as_ref().map(|q| !q.is_empty())) {
            return waiting;
        }
        #[cfg(feature = "host")]
        return TOUCH.with(|t| !t.borrow().is_empty());
        #[cfg(not(feature = "host"))]
        false
    }
    fn read_screen(&self, _cal: &CalParams) -> Option<(u16, u16)> {
        #[cfg(feature = "host")]
        return TOUCH.with(|t| t.borrow().first().map(|p| (p.x, p.y)));
        #[cfg(not(feature = "host"))]
        None
    }
    fn release_debounce(&self) -> u8 {
        1
    }
    #[cfg(feature = "host")]
    fn read_points(&self, _cal: &CalParams, out: &mut [crate::touch::TouchPoint]) -> usize {
        TOUCH.with(|t| {
            let t = t.borrow();
            let n = t.len().min(out.len());
            out[..n].copy_from_slice(&t[..n]);
            n
        })
    }
    #[cfg(feature = "host")]
    fn read_samples(&self, _cal: &CalParams, out: &mut [crate::touch::TouchSample]) -> Option<usize> {
        SAMPLES.with(|q| {
            let mut q = q.borrow_mut();
            let q = q.as_mut()?;
            let n = q.len().min(out.len());
            for (o, s) in out.iter_mut().zip(q.drain(..n)) {
                *o = s;
            }
            Some(n)
        })
    }
}

pub struct MockBacklight;
impl BacklightBackend for MockBacklight {
    fn set_brightness(&self, _percent: u8) {}
    fn brightness(&self) -> u8 {
        0
    }
}

pub struct MockRtc;
impl RtcBackend for MockRtc {
    fn read_time(&self) -> DateTime {
        DateTime::zero()
    }
    fn set_time(&self, _dt: &DateTime) {}
    fn is_valid(&self) -> bool {
        false
    }
    fn clear_vl_flag(&self) {}
}


/// Four pins with remembered mode and level (pull-down unsupported, like the
/// N32903). An input reads its pull level; a floating input reads low.
#[derive(Default)]
pub struct MockGpio {
    pub mode: [core::cell::Cell<u8>; 4],
    pub level: [core::cell::Cell<bool>; 4],
}
impl GpioBackend for MockGpio {
    const PIN_COUNT: u8 = 4;
    fn set_mode(&self, pin: u8, mode: u8) -> bool {
        if mode == MODE_INPUT_PULLDOWN {
            return false;
        }
        self.mode[pin as usize].set(mode);
        true
    }
    fn write(&self, pin: u8, high: bool) {
        if self.mode[pin as usize].get() == MODE_OUTPUT {
            self.level[pin as usize].set(high);
        }
    }
    fn read(&self, pin: u8) -> bool {
        match self.mode[pin as usize].get() {
            MODE_OUTPUT => self.level[pin as usize].get(),
            m => m == MODE_INPUT_PULLUP,
        }
    }
}

/// 64-byte RAM EEPROM (erased = 0xFF).
pub struct MockEeprom {
    pub mem: core::cell::RefCell<[u8; 64]>,
}
impl Default for MockEeprom {
    fn default() -> Self {
        Self { mem: core::cell::RefCell::new([0xFF; 64]) }
    }
}
impl crate::eeprom::EepromBackend for MockEeprom {
    const SIZE: u16 = 64;
    fn read(&self, addr: u16, buf: &mut [u8]) -> bool {
        let a = addr as usize;
        buf.copy_from_slice(&self.mem.borrow()[a..a + buf.len()]);
        true
    }
    fn write(&self, addr: u16, data: &[u8]) -> bool {
        let a = addr as usize;
        self.mem.borrow_mut()[a..a + data.len()].copy_from_slice(data);
        true
    }
}

pub struct MockUsart;
impl UsartBackend for MockUsart {
    fn write_byte(&self, _byte: u8) {}
    fn flush(&self) {}
}

pub struct MockSystick;
impl SystickBackend for MockSystick {
    fn init() {}
    fn delay_ms(_ms: u32) {}
    fn millis() -> u32 {
        #[cfg(feature = "host")]
        return MILLIS.with(|m| m.get());
        #[cfg(not(feature = "host"))]
        0
    }
}

// Convenience constructors so the (not-yet-generic) framework's concrete-alias call
// sites — e.g. `Flash::new()`, `Rtc::init()` — resolve under `--features mock`.
// Defined here because the wrappers are foreign to the framework crate.
impl crate::flash::FlashImpl<MockFlash> {
    pub fn new() -> Self {
        Self::with_backend(MockFlash)
    }
    pub fn init() -> Self {
        Self::with_backend(MockFlash)
    }
}
impl crate::rtc::RtcImpl<MockRtc> {
    pub fn init() -> Self {
        Self::with_backend(MockRtc)
    }
}

/// Marker platform binding every associated type to a mock backend.
pub struct MockPlatform;
impl Platform for MockPlatform {
    type LcdB = MockLcd;
    type FlashB = MockFlash;
    type TouchB = MockTouch;
    type BacklightB = MockBacklight;
    type RtcB = MockRtc;
    type UsartB = MockUsart;
    type SystickB = MockSystick;
    type AudioB = crate::audio::NoAudio;
    #[cfg(feature = "jpeg")]
    type JpegB = crate::jpeg::NoJpeg;
    type GpioB = MockGpio;
    type EepromB = MockEeprom;
    type StorageB = crate::storage::MemStorage;
    type Renderer = crate::renderer::DefaultRenderer;

    #[cfg(feature = "host")]
    fn render_start(
        r: alloc::boxed::Box<crate::ctx::Ctx<Self>>,
        job: crate::render::RenderJob,
    ) -> Result<(), alloc::boxed::Box<crate::ctx::Ctx<Self>>> {
        if !RENDER_ELSEWHERE.with(|e| e.get()) {
            return Err(r);
        }
        RENDER_SIDE.with(|s| *s.borrow_mut() = Some((r, job, false)));
        Ok(())
    }

    #[cfg(feature = "host")]
    fn render_poll() -> Option<alloc::boxed::Box<crate::ctx::Ctx<Self>>> {
        RENDER_SIDE.with(|s| {
            let mut s = s.borrow_mut();
            if s.as_ref().is_some_and(|(_, _, done)| *done) { s.take().map(|(r, _, _)| r) } else { None }
        })
    }

    #[cfg(feature = "host")]
    fn render_wait() {
        // Nobody else would run it: finish it here.
        finish_render();
    }
}

#[cfg(feature = "host")]
type RenderSide = Option<(alloc::boxed::Box<crate::ctx::Ctx<MockPlatform>>, crate::render::RenderJob, bool)>;

#[cfg(feature = "host")]
std::thread_local! {
    /// `MockPlatform::render_start` keeps jobs for [`finish_render`] (a
    /// render side on "another core") instead of declining them.
    static RENDER_ELSEWHERE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static RENDER_SIDE: core::cell::RefCell<RenderSide> = const { core::cell::RefCell::new(None) };
}

/// Make `MockPlatform` take render jobs and hold them until [`finish_render`].
#[cfg(feature = "host")]
pub fn render_elsewhere(on: bool) {
    RENDER_ELSEWHERE.with(|e| e.set(on));
}

/// A job is held and not run yet.
#[cfg(feature = "host")]
pub fn render_pending() -> bool {
    RENDER_SIDE.with(|s| s.borrow().as_ref().is_some_and(|(_, _, done)| !*done))
}

/// Run the held job ("the other core"); `render_poll` then hands it back.
#[cfg(feature = "host")]
pub fn finish_render() {
    RENDER_SIDE.with(|s| {
        if let Some((r, job, done)) = s.borrow_mut().as_mut() {
            if !*done {
                crate::render::run_job(r, *job);
                *done = true;
            }
        }
    });
}
