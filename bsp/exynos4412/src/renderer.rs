//! How this board draws widgets (`Platform::Renderer`).
//!
//! The board renders in `render::MODE_AREAS` only (`rcore::service`): the VM's core records
//! each frame in compositor order, the GPU player draws it ([`WidgetRenderer::play`],
//! `compose.rs`, with the areas it repaints merged by [`WidgetRenderer::play_areas`]); a list
//! it declines is replayed on the CPU by the core's own drawing (the G2D helping through
//! `lcd.rs`). The board adds nothing to the core's widget drawing. (Mode 0's GPU compositor
//! of the tree, its sprites, picture copies and `on_paint` captures were removed with the
//! other render modes, 2026-11-17.)

use ferrite_core::renderer::WidgetRenderer;
use ferrite_core::types::Rect;

use crate::platform::Exynos4412Platform as P;

pub struct ExynosRenderer;

impl WidgetRenderer<P> for ExynosRenderer {
    fn play_areas(areas: &[Rect]) -> alloc::vec::Vec<Rect> {
        #[cfg(feature = "gpu_compose")]
        {
            crate::compose::play_areas(areas)
        }
        #[cfg(not(feature = "gpu_compose"))]
        {
            areas.to_vec()
        }
    }

    fn play(ctx: &ferrite_core::player::Player<P>, list: &ferrite_core::dl::List) -> bool {
        #[cfg(feature = "gpu_compose")]
        {
            crate::compose::play(ctx, list)
        }
        #[cfg(not(feature = "gpu_compose"))]
        {
            let _ = (ctx, list);
            false
        }
    }
}

/// Pictures whose pixels were cleaned from the data cache, with the flash
/// write generation at the time (`flash::generation`).
const CLEANED: usize = 8;
static mut CLEANED_AT: [Option<(u32, u32)>; CLEANED] = [None; CLEANED];
static mut CLEANED_NEXT: usize = 0;

/// The physical address of the `len` picture bytes at flash `addr`, cleaned
/// from the data cache once per flash write generation (the GPU reads DRAM).
/// None if the G2D cannot take it (outside the region, or not 4-byte
/// aligned: an FS image from before `ferrite_build` aligned image pixels).
pub(crate) fn picture_addr(addr: u32, len: usize) -> Option<usize> {
    let phys = crate::flash::phys(addr, len)?;
    if phys % 4 != 0 {
        return None;
    }
    let generation = crate::flash::generation();
    // SAFETY: one core at a time (the render job's), never reentrant.
    let (seen, next) = unsafe { (&mut *(&raw mut CLEANED_AT), &mut *(&raw mut CLEANED_NEXT)) };
    if !seen.contains(&Some((addr, generation))) {
        exynos_4412_rs::mmu::clean_dcache_range(phys, len);
        seen[*next] = Some((addr, generation));
        *next = (*next + 1) % CLEANED;
    }
    Some(phys)
}
