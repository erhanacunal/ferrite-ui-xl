//! The render side (plan step 3c): what plays a recorded frame, on whichever
//! core the board gives it (`Platform::render_start`). The VM's context
//! renders the widget tree and records the frame (`render::submit`); this
//! holds only what playing it needs: the screen and flash (the same
//! hardware), the fonts and images the list refers to (shared copies kept up
//! to date at each hand-over, `render::sync_render_ctx`), the overlay, and
//! the frame itself. A full `Ctx` until 2026-11-23 (docs/notes/xl.md "One
//! tree").

use crate::dl;
use crate::flash::FlashImpl;
use crate::font::FontList;
use crate::image::ImageList;
use crate::lcd::LcdImpl;
use crate::platform::Platform;
use crate::renderer::WidgetRenderer;
use crate::types::Rect;

pub struct Player<P: Platform> {
    pub lcd: LcdImpl<P::LcdB>,
    pub flash: FlashImpl<P::FlashB>,
    pub fonts: FontList,
    pub images: ImageList,
    /// The VM's overlay (`ObjPool::overlay`) when the frame was handed over.
    pub overlay: Option<Rect>,
    /// The frame to play; swapped with the VM's recorder at each hand-over,
    /// so the last one played comes back as the next frame's buffer.
    pub(crate) frame: dl::Recorder,
}

impl<P: Platform> Player<P> {
    /// A render side drawing through `lcd` and reading `flash` (the same
    /// hardware as the VM's context).
    pub fn new(lcd: LcdImpl<P::LcdB>, flash: FlashImpl<P::FlashB>) -> Self {
        Self {
            lcd,
            flash,
            fonts: FontList::new(),
            images: ImageList::new(),
            overlay: None,
            frame: dl::Recorder::new(),
        }
    }

    /// Where the frame's images and fonts are found.
    #[inline]
    pub fn refs(&self) -> dl::Resources<'_, P::FlashB> {
        dl::Resources { images: &self.images, fonts: &self.fonts, flash: &self.flash }
    }

    /// The frame handed over.
    pub fn frame(&self) -> core::cell::Ref<'_, dl::List> {
        self.frame.list()
    }

    /// Plays the frame handed over: the board's player
    /// (`WidgetRenderer::play`), else a replay on the CPU.
    pub fn play(&mut self) {
        let t0 = crate::perf::now();
        let list = self.frame.list();
        if !<P::Renderer as WidgetRenderer<P>>::play(self, &list) {
            list.replay(&self.lcd, &self.refs());
        }
        crate::render::JOB_TICKS[crate::render::JOB_RENDER]
            .fetch_add(crate::perf::now().wrapping_sub(t0) as u64, core::sync::atomic::Ordering::Relaxed);
    }
}
