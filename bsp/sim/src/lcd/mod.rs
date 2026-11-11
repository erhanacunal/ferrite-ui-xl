// BSP LCD module (sim) — re-exports framework traits, provides sim type aliases.

pub use ferrite_core::lcd::*;

/// Screen size: the Exynos 4412 tablet (1280×800).
pub const WIDTH: u16 = 1280;
pub const HEIGHT: u16 = 800;

pub mod sim;

pub type Lcd = LcdImpl<sim::SimLcd>;

pub fn new(fb: sim::Framebuffer) -> Lcd {
    Lcd::with_backend(sim::SimLcd::new(fb))
}
