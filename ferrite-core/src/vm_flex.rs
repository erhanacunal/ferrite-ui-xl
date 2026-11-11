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

/// The VM target while `layout` runs (`target`/`get`/`set` like the .fl code).
struct Cursor {
    t: WidgetId,
}

impl Cursor {
    /// `target(id)` with an id taken from a VM value (`W_TARGET_S`).
    #[inline]
    fn target(&mut self, v: i32) {
        self.t = WidgetId(v as ObjPtr);
    }

    /// `get(prop)` (`W_GET`).
    #[inline]
    fn get<P: Platform>(&self, ctx: &Ctx<P>, prop: u8) -> i32 {
        if self.t.is_some() { Vm::get_scalar_prop_on(ctx, self.t, prop) } else { 0 }
    }

    /// `set(prop, v)` for the geometry props layout writes (`Vm::w_set`).
    #[inline]
    fn set<P: Platform>(&self, ctx: &mut Ctx<P>, prop: u8, v: i32) {
        if self.t.is_none() {
            return;
        }
        Vm::set_scalar_prop_on(ctx, self.t, prop, v);
        if matches!(prop, PROP_LOC_X | PROP_LOC_Y) && ctx.objs.is_active_layer_root(self.t) {
            return;
        }
        if super::prop_is_visual(prop) {
            ctx.objs.mark_dirty(self.t);
        }
    }
}

impl Vm {
    /// `FLEX_LAYOUT`: `flexLayout(box)`, leaving the VM target where the .fl
    /// function does.
    pub(super) fn flex_layout<P: Platform>(&mut self, ctx: &mut Ctx<P>, bx: i32) {
        let mut l = Cursor { t: self.target };
        run(ctx, &mut l, bx);
        self.target = l.t;
    }
}

/// `layout` with automatic re-layout suppressed: its own writes do not mark
/// containers, and `bx` is up to date afterwards.
fn run<P: Platform>(ctx: &mut Ctx<P>, l: &mut Cursor, bx: i32) {
    let busy = core::mem::replace(&mut ctx.objs.flex_busy, true);
    layout(ctx, l, bx);
    ctx.objs.flex_busy = busy;
    let id = WidgetId(bx as ObjPtr);
    ctx.objs.flex_pending.retain(|&w| w != id);
}

/// `flexLayout(box)` with `l` as the VM target; leaves it where the .fl
/// function does.
fn layout<P: Platform>(ctx: &mut Ctx<P>, l: &mut Cursor, bx: i32) {
    l.target(bx);
    let mode = l.get(ctx, PROP_FLEX);
    let dir = mode & 3;
    if dir == 0 {
        return;
    }
    let justify = mode & 28;
    let items = mode & 96;
    let gap = l.get(ctx, PROP_FLEX_GAP);

    // Content area size.
    let cw = l
        .get(ctx, PROP_SIZE_W)
        .wrapping_sub(l.get(ctx, PROP_BORDER_L))
        .wrapping_sub(l.get(ctx, PROP_BORDER_R))
        .wrapping_sub(l.get(ctx, PROP_PADDING_L))
        .wrapping_sub(l.get(ctx, PROP_PADDING_R));
    let ch = l
        .get(ctx, PROP_SIZE_H)
        .wrapping_sub(l.get(ctx, PROP_BORDER_T))
        .wrapping_sub(l.get(ctx, PROP_BORDER_B))
        .wrapping_sub(l.get(ctx, PROP_PADDING_T))
        .wrapping_sub(l.get(ctx, PROP_PADDING_B));
    let (main_len, cross_len) = if dir == FLEX_COLUMN { (ch, cw) } else { (cw, ch) };

    // Pass 1: count visible children, sum their main sizes and grow factors.
    let mut count = 0i32;
    let mut used = 0i32;
    let mut grow_total = 0i32;
    let mut c = l.get(ctx, PROP_FIRST_CHILD);
    while c >= 0 {
        l.target(c);
        if l.get(ctx, PROP_VISIBLE) != 0 {
            count = count.wrapping_add(1);
            let mut g = l.get(ctx, PROP_FLEX_GROW);
            if g == 0 && justify == JUSTIFY_STRETCH {
                g = 1;
            }
            if g == 0 {
                let main = if dir == FLEX_ROW {
                    l.get(ctx, PROP_SIZE_W)
                } else {
                    l.get(ctx, PROP_SIZE_H)
                };
                used = used.wrapping_add(main);
            }
            grow_total = grow_total.wrapping_add(g);
        }
        c = l.get(ctx, PROP_NEXT_SIBLING);
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
    l.target(bx);
    c = l.get(ctx, PROP_FIRST_CHILD);
    while c >= 0 {
        l.target(c);
        if l.get(ctx, PROP_VISIBLE) != 0 {
            let mut size = if dir == FLEX_ROW {
                l.get(ctx, PROP_SIZE_W)
            } else {
                l.get(ctx, PROP_SIZE_H)
            };
            let mut g = l.get(ctx, PROP_FLEX_GROW);
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
                l.get(ctx, PROP_SIZE_H)
            } else {
                l.get(ctx, PROP_SIZE_W)
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
                l.set(ctx, PROP_LOC_X, pos);
                l.set(ctx, PROP_LOC_Y, cpos);
                l.set(ctx, PROP_SIZE_W, size);
                l.set(ctx, PROP_SIZE_H, csize);
            } else {
                l.set(ctx, PROP_LOC_X, cpos);
                l.set(ctx, PROP_LOC_Y, pos);
                l.set(ctx, PROP_SIZE_W, csize);
                l.set(ctx, PROP_SIZE_H, size);
            }

            pos = pos.wrapping_add(size).wrapping_add(gap).wrapping_add(extra_gap);
            if extra_rem > 0 {
                pos = pos.wrapping_add(1);
                extra_rem = extra_rem.wrapping_sub(1);
            }

            // Nested container: its size is final now.
            if l.get(ctx, PROP_FLEX) & 3 != 0 {
                layout(ctx, l, c);
                l.target(c);
            }
        }
        c = l.get(ctx, PROP_NEXT_SIBLING);
    }

    // Children moved: repaint the container so their old spots are cleared.
    l.target(bx);
    if l.t.is_some() {
        ctx.objs.mark_dirty(l.t);
    }
}

// --- Automatic layout (FLEX_AUTO) ---
//
// A container whose flex mode has `FLEX_AUTO` is laid out by the firmware
// whenever its layout may have changed, without a `flexLayout()` call:
// `flex_note` marks it pending on writes that move its children (its own
// box, a child's size / visibility / grow, children added or removed), and
// `flush` lays out the pending ones before anything looks at the result (the
// render passes, and VM reads of a position or size). Containers without the
// bit behave exactly as before. lib/flex.fl ignores the bit, so on boards
// without `caps.native_flex` programs still call flexLayout() themselves.

use crate::pool::ObjPool;
use crate::proto::{PROP_BORDER_EDGES, PROP_PADDING, PROP_SIZE};

/// lib/flex.fl `FLEX_AUTO` (flex mode bit 7).
pub const FLEX_AUTO: i32 = 128;

impl ObjPool {
    /// A live flex container with `FLEX_AUTO`.
    fn flex_is_auto(&self, id: WidgetId) -> bool {
        if id.is_none() || !self.is_live(id) {
            return false;
        }
        let m = self.ext(id).flex as i32;
        m & FLEX_AUTO != 0 && m & 3 != 0
    }

    /// Queue `id` for automatic layout if it is an auto container.
    pub(crate) fn flex_mark(&mut self, id: WidgetId) {
        if !self.flex_busy && self.flex_is_auto(id) && !self.flex_pending.contains(&id) {
            self.flex_pending.push(id);
        }
    }

    /// Property `prop` of `id` was written: queue the containers whose
    /// layout it changes.
    #[inline(never)]
    pub(crate) fn flex_note(&mut self, id: WidgetId, prop: u8) {
        if self.flex_busy || id.is_none() {
            return;
        }
        // The container's own content box or mode.
        if matches!(
            prop,
            PROP_SIZE_W
                | PROP_SIZE_H
                | PROP_SIZE
                | PROP_BORDER_T
                | PROP_BORDER_R
                | PROP_BORDER_B
                | PROP_BORDER_L
                | PROP_BORDER_EDGES
                | PROP_PADDING_T
                | PROP_PADDING_R
                | PROP_PADDING_B
                | PROP_PADDING_L
                | PROP_PADDING
                | PROP_FLEX
                | PROP_FLEX_GAP
        ) {
            self.flex_mark(id);
        }
        // A child's share of its parent's main axis.
        if matches!(prop, PROP_SIZE_W | PROP_SIZE_H | PROP_SIZE | PROP_VISIBLE | PROP_FLEX_GROW) && self.is_live(id) {
            let p = self.get(id).parent;
            self.flex_mark(p);
        }
    }

    fn depth(&self, mut id: WidgetId) -> usize {
        let mut d = 0;
        while id.is_some() && d <= self.count() {
            id = self.get(id).parent;
            d += 1;
        }
        d
    }
}

/// Lay out the pending auto containers, outermost first (a container's
/// layout sizes its nested ones). Cheap when nothing is pending: only that
/// check is inlined (into `Vm::step` among others; the layout itself inlined
/// there cost the VM 2.8% on the board).
#[inline]
pub(crate) fn flush<P: Platform>(ctx: &mut Ctx<P>) {
    if !ctx.objs.flex_pending.is_empty() {
        flush_pending(ctx);
    }
}

#[cold]
#[inline(never)]
fn flush_pending<P: Platform>(ctx: &mut Ctx<P>) {
    let mut list = core::mem::take(&mut ctx.objs.flex_pending);
    list.sort_by_key(|&w| ctx.objs.depth(w));
    for w in list {
        if ctx.objs.flex_is_auto(w) {
            let mut l = Cursor { t: w };
            run(ctx, &mut l, w.to_vm());
        }
    }
}
