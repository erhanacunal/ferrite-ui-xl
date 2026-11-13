//! The gesture builtins (`gesture.rs`): opcodes 0xE6..=0xE7, `caps.gestures`
//! (`CAP_GESTURES`).
//!
//! `onGesture(widget, mask, @handler)` sets which recognizers (`G_*` bits)
//! `widget` takes part in and the handler they report to, `fn h(widget,
//! event)`; mask 0 removes them. Inside the handler, `gestureInfo(field)`
//! reads that event's details (`gesture::field`): each queued event carries
//! its own, so a handler sees the values of the event it was called for.

use super::{Platform, Vm};
use crate::ctx::Ctx;
use crate::gesture::{Detail, Reg, field};
use crate::pool::ObjPtr;
use crate::widget::WidgetId;

/// `onGesture(widget, mask, @handler)`: pop handler, mask, widget.
pub(super) const OP_ON_GESTURE: u8 = 0xE6;
/// `gestureInfo(field)` → the running gesture event's `field` (0 outside one).
pub(super) const OP_GESTURE_INFO: u8 = 0xE7;

impl Vm {
    pub(super) fn gesture_op<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        match op {
            OP_ON_GESTURE => {
                let func = self.pop();
                let (widget, mask) = self.pop2();
                let w = WidgetId(widget as ObjPtr);
                let mask = if ctx.objs.is_live(w) && func > 0 { mask as u16 & crate::gesture::G_ALL } else { 0 };
                let task = ctx.objs.cur_task;
                ctx.objs.gestures.set(w, Reg { mask, func: func as u16, task });
            }
            _ => {
                let f = self.pop();
                let v = if (0..field::COUNT as i32).contains(&f) { self.gesture_cur[f as usize] } else { 0 };
                self.push(v);
            }
        }
    }

    /// Queue gesture handler `func` (a 1-based func id) with `(widget,
    /// event)` and the event's details. False if the VM has no such function.
    pub fn enqueue_gesture(&mut self, func: u16, widget: WidgetId, ev: u8, d: &Detail) -> bool {
        let Some(off) = self.find_func(func).map(|f| f.offset as super::CodeAddr) else {
            return false;
        };
        let slot = self.cb_tail as usize;
        if self.cb_details.len() != self.cb_queue.len() {
            self.cb_details = alloc::vec![[0; field::COUNT]; self.cb_queue.len()];
        }
        let before = self.cb_tail;
        self.enqueue_callback(off, &[widget.0 as i32, ev as i32]);
        if self.cb_tail != before {
            self.cb_details[slot] = *d;
            self.cb_queue[slot].gesture = true;
        }
        true
    }
}
