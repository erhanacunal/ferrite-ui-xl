//! Native `lib/flex.fl` `flexLayout(box)`: opcode `FLEX_LAYOUT` (0xDB),
//! emitted by the compiler for devices with `caps.native_flex`
//! (`CAP_NATIVE_FLEX`).
//!
//! A line-by-line port of the .fl function: the same reads (`get`), writes
//! (`set`, through `Vm::w_set`, so dirty marking and layer roots behave the
//! same), `target` changes and integer arithmetic (wrapping i32, truncating
//! division), in the same order. A program therefore ends up with the same
//! widget tree and the same VM target as with the .fl function; only the
//! instruction count differs. Keep it in sync with `lib/flex.fl` in ferrite-ui
//! (`flex_tests` compares the two on compiled programs).

use super::{Platform, Vm};
use crate::ctx::Ctx;
use crate::pool::ObjPtr;
use crate::proto::{
    PROP_BORDER_B, PROP_BORDER_L, PROP_BORDER_R, PROP_BORDER_T, PROP_FIRST_CHILD, PROP_FLEX,
    PROP_FLEX_GAP, PROP_FLEX_GROW, PROP_LOC_X, PROP_LOC_Y, PROP_NEXT_SIBLING, PROP_PADDING_B,
    PROP_PADDING_L, PROP_PADDING_R, PROP_PADDING_T, PROP_SIZE_H, PROP_SIZE_W, PROP_VISIBLE,
};
use crate::widget::WidgetId;

// lib/flex.fl constants.
const FLEX_ROW: i32 = 1;
const FLEX_COLUMN: i32 = 2;
const JUSTIFY_CENTER: i32 = 4;
const JUSTIFY_END: i32 = 8;
const JUSTIFY_SPACE_BETWEEN: i32 = 12;
const JUSTIFY_STRETCH: i32 = 16;
const ITEMS_CENTER: i32 = 32;
const ITEMS_END: i32 = 64;
const ITEMS_STRETCH: i32 = 96;

/// VM `OP_DIV` / `OP_MOD`: wrapping, 0 for a zero divisor.
#[inline]
fn div(a: i32, b: i32) -> i32 {
    if b != 0 { a.wrapping_div(b) } else { 0 }
}
#[inline]
fn rem(a: i32, b: i32) -> i32 {
    if b != 0 { a.wrapping_rem(b) } else { 0 }
}

impl Vm {
    /// `target(id)` with an id taken from a VM value (`W_TARGET_S`).
    #[inline]
    fn flex_target(&mut self, v: i32) {
        self.target = WidgetId(v as ObjPtr);
    }

    /// `get(prop)` on the current target (`W_GET`).
    #[inline]
    fn flex_get<P: Platform>(&self, ctx: &Ctx<P>, prop: u8) -> i32 {
        if self.target.is_some() { self.get_scalar_prop(ctx, prop) } else { 0 }
    }

    /// `flexLayout(box)`; leaves the target where the .fl function does.
    pub(super) fn flex_layout<P: Platform>(&mut self, ctx: &mut Ctx<P>, bx: i32) {
        self.flex_target(bx);
        let mode = self.flex_get(ctx, PROP_FLEX);
        let dir = mode & 3;
        if dir == 0 {
            return;
        }
        let justify = mode & 28;
        let items = mode & 96;
        let gap = self.flex_get(ctx, PROP_FLEX_GAP);

        // Content area size.
        let cw = self
            .flex_get(ctx, PROP_SIZE_W)
            .wrapping_sub(self.flex_get(ctx, PROP_BORDER_L))
            .wrapping_sub(self.flex_get(ctx, PROP_BORDER_R))
            .wrapping_sub(self.flex_get(ctx, PROP_PADDING_L))
            .wrapping_sub(self.flex_get(ctx, PROP_PADDING_R));
        let ch = self
            .flex_get(ctx, PROP_SIZE_H)
            .wrapping_sub(self.flex_get(ctx, PROP_BORDER_T))
            .wrapping_sub(self.flex_get(ctx, PROP_BORDER_B))
            .wrapping_sub(self.flex_get(ctx, PROP_PADDING_T))
            .wrapping_sub(self.flex_get(ctx, PROP_PADDING_B));
        let (main_len, cross_len) = if dir == FLEX_COLUMN { (ch, cw) } else { (cw, ch) };

        // Pass 1: count visible children, sum their main sizes and grow factors.
        let mut count = 0i32;
        let mut used = 0i32;
        let mut grow_total = 0i32;
        let mut c = self.flex_get(ctx, PROP_FIRST_CHILD);
        while c >= 0 {
            self.flex_target(c);
            if self.flex_get(ctx, PROP_VISIBLE) != 0 {
                count = count.wrapping_add(1);
                let mut g = self.flex_get(ctx, PROP_FLEX_GROW);
                if g == 0 && justify == JUSTIFY_STRETCH {
                    g = 1;
                }
                if g == 0 {
                    let main = if dir == FLEX_ROW {
                        self.flex_get(ctx, PROP_SIZE_W)
                    } else {
                        self.flex_get(ctx, PROP_SIZE_H)
                    };
                    used = used.wrapping_add(main);
                }
                grow_total = grow_total.wrapping_add(g);
            }
            c = self.flex_get(ctx, PROP_NEXT_SIBLING);
        }
        if count == 0 {
            return;
        }

        let free = main_len
            .wrapping_sub(used)
            .wrapping_sub(gap.wrapping_mul(count.wrapping_sub(1)));
        let mut pos = 0i32;
        let mut extra_gap = 0i32;
        let mut extra_rem = 0i32;
        if grow_total == 0 && free > 0 {
            if justify == JUSTIFY_CENTER {
                pos = div(free, 2);
            } else if justify == JUSTIFY_END {
                pos = free;
            } else if justify == JUSTIFY_SPACE_BETWEEN && count > 1 {
                extra_gap = div(free, count.wrapping_sub(1));
                extra_rem = rem(free, count.wrapping_sub(1));
            }
        }
        let mut free_left = if free > 0 { free } else { 0 };
        let mut grow_left = grow_total;

        // Pass 2: place the children.
        self.flex_target(bx);
        c = self.flex_get(ctx, PROP_FIRST_CHILD);
        while c >= 0 {
            self.flex_target(c);
            if self.flex_get(ctx, PROP_VISIBLE) != 0 {
                let mut size = if dir == FLEX_ROW {
                    self.flex_get(ctx, PROP_SIZE_W)
                } else {
                    self.flex_get(ctx, PROP_SIZE_H)
                };
                let mut g = self.flex_get(ctx, PROP_FLEX_GROW);
                if g == 0 && justify == JUSTIFY_STRETCH {
                    g = 1;
                }
                if g > 0 {
                    // Proportional share; the last grower gets the rounding rest.
                    size = div(free_left.wrapping_mul(g), grow_left);
                    free_left = free_left.wrapping_sub(size);
                    grow_left = grow_left.wrapping_sub(g);
                }

                let mut csize = if dir == FLEX_ROW {
                    self.flex_get(ctx, PROP_SIZE_H)
                } else {
                    self.flex_get(ctx, PROP_SIZE_W)
                };
                let mut cpos = 0i32;
                if items == ITEMS_CENTER {
                    cpos = div(cross_len.wrapping_sub(csize), 2);
                } else if items == ITEMS_END {
                    cpos = cross_len.wrapping_sub(csize);
                } else if items == ITEMS_STRETCH {
                    csize = cross_len;
                }

                if dir == FLEX_ROW {
                    self.w_set(ctx, PROP_LOC_X, pos);
                    self.w_set(ctx, PROP_LOC_Y, cpos);
                    self.w_set(ctx, PROP_SIZE_W, size);
                    self.w_set(ctx, PROP_SIZE_H, csize);
                } else {
                    self.w_set(ctx, PROP_LOC_X, cpos);
                    self.w_set(ctx, PROP_LOC_Y, pos);
                    self.w_set(ctx, PROP_SIZE_W, csize);
                    self.w_set(ctx, PROP_SIZE_H, size);
                }

                pos = pos.wrapping_add(size).wrapping_add(gap).wrapping_add(extra_gap);
                if extra_rem > 0 {
                    pos = pos.wrapping_add(1);
                    extra_rem = extra_rem.wrapping_sub(1);
                }

                // Nested container: its size is final now.
                if self.flex_get(ctx, PROP_FLEX) & 3 != 0 {
                    self.flex_layout(ctx, c);
                    self.flex_target(c);
                }
            }
            c = self.flex_get(ctx, PROP_NEXT_SIBLING);
        }

        // Children moved: repaint the container so their old spots are cleared.
        self.flex_target(bx);
        if self.target.is_some() {
            ctx.objs.mark_dirty(self.target);
        }
    }
}
