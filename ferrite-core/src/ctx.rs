/// Shared application context — core resources passed to VM, render, and page functions.
///
/// Generic over `P: Platform` (ferrite-core): the hardware-backed fields use the
/// platform's associated backend types, so the framework is written once and a BSP
/// chooses concrete hardware by supplying `P`. Allocated on the heap via Box in main.
use crate::audio::AudioImpl;
use crate::backlight::BacklightImpl;
use crate::eeprom::EepromImpl;
use crate::flash::FlashImpl;
use crate::gpio::GpioImpl;
#[cfg(feature = "jpeg")]
use crate::jpeg::JpegImpl;
use crate::lcd::LcdImpl;
use crate::platform::Platform;
use crate::rtc::RtcImpl;
use crate::systick::SystickImpl;
use crate::usart::UsartImpl;

use crate::font::FontList;
use crate::fs::Fs;
use crate::image::ImageList;
use crate::pool::ObjPool;

extern crate alloc;

pub struct Ctx<P: Platform> {
    pub lcd: LcdImpl<P::LcdB>,
    pub flash: FlashImpl<P::FlashB>,
    pub fonts: FontList,
    pub images: ImageList,
    pub objs: ObjPool,
    /// The render pool: the renderer's copy of `objs` (`view.rs`), unless
    /// `render_ctx` holds it.
    pub view: ObjPool,
    /// The render side's own context (plan step 3c), when the board gives
    /// one: its `objs` is the render pool, its `lcd`/`flash` reach the same
    /// hardware, its fonts/images/fs are copies kept up to date at each sync
    /// (`render::with_view`). None = the renderer works on `self` with the
    /// pools swapped.
    pub render_ctx: Option<alloc::boxed::Box<Ctx<P>>>,
    /// `render_ctx` is away running a job (`Platform::render_start`): the
    /// screen belongs to the render side until `render::wait_idle`.
    pub render_away: bool,
    pub fs: Option<Fs>,
    pub backlight: BacklightImpl<P::BacklightB>,
    pub rtc: RtcImpl<P::RtcB>,
    pub usart: UsartImpl<P::UsartB>,
    pub systick: SystickImpl<P::SystickB>,
    pub audio: AudioImpl<P::AudioB>,
    #[cfg(feature = "jpeg")]
    pub jpeg: JpegImpl<P::JpegB>,
    pub gpio: GpioImpl<P::GpioB>,
    pub eeprom: EepromImpl<P::EepromB>,
    pub cursor_visible: bool,
    /// Task slots and the requests the VMs queue (`multi_program`).
    pub tasks: crate::task::TaskTable,
}

impl<P: Platform> Ctx<P> {
    /// The pool the renderer works on (`view.rs`).
    pub fn render_pool(&self) -> &ObjPool {
        match &self.render_ctx {
            Some(r) => &r.objs,
            None => &self.view,
        }
    }

    /// Where list replays here find the images and fonts they refer to.
    #[inline]
    pub fn refs(&self) -> crate::dl::Resources<'_, P::FlashB> {
        crate::dl::Resources { images: &self.images, fonts: &self.fonts, flash: &self.flash }
    }
}
