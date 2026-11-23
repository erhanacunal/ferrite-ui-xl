//! The instructions threaded dispatch (`tc.rs`) runs out of line: each is a
//! `Vm` method on the VM's own `pc` / `sp`, which reads its operands with
//! `read_*` / `fetch` and leaves the next instruction at `pc`. `tc`'s handler
//! for it (`cold!`) writes the burst's state back, calls it and resumes. The
//! hot instructions are `tc` handlers only; RET and SYSCALL are both (their
//! handlers run the common case and fall back to these).

use super::*;

impl Vm {
    pub(super) fn op_halt(&mut self) {
        // HALT inside a callback ends just that callback.
        if !self.cb_frames.is_empty() {
            self.leave_callback();
        } else {
            self.state = VmState::Halted;
        }
    }

    pub(super) fn op_ret<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        if self.cb_frames.last().is_some_and(|f| f.call_sp == self.call_sp) {
            // Return from the callback's own entry function.
            self.leave_callback();
        } else if self.call_sp > 0 {
            self.call_sp -= 1;
            let frame = self.call_stack[self.call_sp as usize];
            self.goto(frame.ret_addr);
            self.frame_base = frame.frame_base as u32;
            self.frame_size = frame.frame_size;

            // Modal builder return interception: if we just popped the
            // call frame for a modal dialog's builder, capture its
            // returned overlay id and suspend the VM until
            // setDialogResult fires.
            let intercept = match self.modal_stack.last() {
                Some(top) => {
                    top.state == ModalState::AwaitingBuilderReturn
                        && self.call_sp < top.builder_call_depth
                }
                None => false,
            };
            if intercept {
                self.handle_builder_return(ctx);
            }
        } else {
            self.fail(fault::RET_EMPTY);
        }
    }

    pub(super) fn op_yield(&mut self) {
        self.critical = false;
        self.state = VmState::Yielded;
    }

    pub(super) fn op_w_dirty<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        if self.target.is_some() {
            ctx.objs.mark_dirty(self.target);
        }
    }

    pub(super) fn op_w_render<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // render() from inside on_paint: the render that runs it is
        // already going on.
        if !self.in_paint {
            let mut paint = render::PaintList::new();
            render::collect_dirty_paint(ctx, &mut paint);
            self.run_paint(ctx, &paint);
            render::render_dirty(ctx);
        }
    }

    pub(super) fn op_arr_store<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (idx, val) = self.pop2();
        let arr_id = self.pop();
        self.arr_store(ctx, arr_id, idx, val);
    }

    pub(super) fn op_arr_len<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let arr_id = self.pop();
        self.arr_len(ctx, arr_id);
    }

    pub(super) fn op_w_alloc<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        if let Some(id) = ctx.objs.alloc_widget() {
            self.push(id.0 as i32);
        } else {
            self.fail(fault::WIDGET_ALLOC);
        }
    }

    pub(super) fn op_w_alltar<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let raw = self.read_u8();
        self.w_alltar(ctx, raw);
    }

    pub(super) fn op_arr_free<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let arr_id = self.pop();
        if arr_id >= 0 {
            ctx.objs.arr_free(arr_id as ObjPtr);
        }
    }

    pub(super) fn op_w_target<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Static target with u8-encoded id (0..254). Used for widgets
        // allocated at fixed positions (e.g. root). Dynamically
        // allocated widgets use W_TARGET_S, which takes a u16 id from
        // the stack.
        let wid = self.read_u8();
        self.target = self.widget_ref(ctx, wid as ObjPtr);
    }

    pub(super) fn op_w_target_s<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let wid = self.pop() as ObjPtr;
        self.target = self.widget_ref(ctx, wid);
    }

    pub(super) fn op_w_set<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let prop_id = self.read_u8();
        let val = self.pop();
        self.w_set(ctx, prop_id, val);
    }

    pub(super) fn op_w_get<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let prop_id = self.read_u8();
        // FLEX_AUTO: a position or size reads the laid-out value.
        if matches!(prop_id, PROP_LOC_X | PROP_LOC_Y | PROP_SIZE_W | PROP_SIZE_H) {
            flex::flush(ctx);
        }
        if self.target.is_some() {
            let v = self.get_scalar_prop(ctx, prop_id);
            // A render-owned value (an animated prop, a scroll offset):
            // counted, and answered from the last render's snapshot when
            // `snapshot::STALE` models the render core (`snapshot.rs`).
            let v = ctx.objs.render.snapshot.read(self.target, prop_id, v);
            // Font / image ids as this program's image numbers them (`domain.rs`).
            let v = match prop_id {
                PROP_FONT_ID => i32::from(ctx.domains.font_local(self.domain, v as u8)),
                PROP_IMAGE_ID => i32::from(ctx.domains.image_local(self.domain, v as u8)),
                _ => v,
            };
            self.push(v);
        } else {
            self.push(0);
        }
    }

    pub(super) fn op_w_parent<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Static parent id (u8). Used for parent(0) and other literal
        // widget references that fit in u8.
        let parent = self.read_u8();
        self.w_parent(ctx, parent as ObjPtr);
    }

    pub(super) fn op_w_parent_s<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Dynamic parent — id from stack. Used when the parent is a
        // widget variable (its slot stores the runtime id from alloc).
        let parent = self.pop() as ObjPtr;
        self.w_parent(ctx, parent);
    }

    pub(super) fn op_w_load<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let off = self.read_u16();
        self.load_page_block(ctx, off as u32);
    }

    pub(super) fn op_w_setall<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.w_setall(ctx);
    }

    pub(super) fn op_w_set_len<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let prop_id = self.read_u8();
        let len = self.read_u8() as usize;
        let start = self.pc as usize;
        let end = start + len;
        if end <= self.code.len() {
            // Compound values fit the stack buffer; longer payloads
            // (text literals up to 255 bytes) go through the heap.
            let mut small = [0u8; 32];
            let mut big = Vec::new();
            let buf: Option<&mut [u8]> = if len <= small.len() {
                Some(&mut small[..len])
            } else if big.try_reserve_exact(len).is_ok() {
                big.resize(len, 0);
                Some(&mut big)
            } else {
                None
            };
            self.pc = end as CodeAddr;
            if let Some(buf) = buf {
                self.code.read_bytes(start, buf);
                self.w_set_len(ctx, prop_id, buf);
            } else {
                self.fail(fault::STR_ALLOC);
            }
        } else {
            self.fail(fault::CODE_OOB);
        }
    }

    pub(super) fn op_arr_alloc<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let size = self.read_u8() as usize;
        match ctx.objs.arr_alloc(size) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::ARR_ALLOC),
        }
    }

    pub(super) fn op_arr_alloc_s<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let size = self.pop();
        match usize::try_from(size).ok().and_then(|n| ctx.objs.arr_alloc(n)) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::ARR_ALLOC),
        }
    }

    pub(super) fn op_arr_init<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let count = self.read_u8() as usize;
        let start = self.pc as usize;
        let byte_len = count * 4;
        if start + byte_len <= self.code.len() {
            self.pc = (start + byte_len) as CodeAddr;
            let Some(id) = ctx.objs.arr_alloc(count) else {
                return self.fail(fault::ARR_ALLOC);
            };
            let values = ctx.objs.arr_mut(id).unwrap();
            let mut tmp = [0u8; 4];
            for (i, v) in values.iter_mut().enumerate() {
                self.code.read_bytes(start + i * 4, &mut tmp);
                *v = i32::from_le_bytes(tmp);
            }
            self.push(id as i32);
        } else {
            self.fail(fault::CODE_OOB);
        }
    }

    pub(super) fn op_f_read<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let addr = self.read_u32();
        let len = self.read_u16() as usize;
        if len > (self.stack.len() - self.sp as usize) {
            self.fail(fault::STACK_OVERFLOW);
            return;
        }
        // Read in fixed chunks: the stack depth is a runtime value.
        let Some(addr) = self.flash_addr(ctx, addr, len as u32) else {
            return self.fail(fault::BAD_OPERAND);
        };
        let mut buf = [0u8; 16];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(buf.len());
            ctx.flash.read(addr + done as u32, &mut buf[..n]);
            for &b in &buf[..n] {
                self.push(b as i32);
            }
            done += n;
        }
    }

    pub(super) fn op_f_write<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let addr = self.read_u32();
        let len = self.read_u8() as usize;
        let start = self.pc as usize;
        if start + len <= self.code.len() {
            let mut buf = [0u8; 64];
            let n = len.min(buf.len());
            self.code.read_bytes(start, &mut buf[..n]);
            self.pc = (start + len) as CodeAddr;
            if n > 0 {
                let Some(addr) = self.flash_addr(ctx, addr, n as u32) else {
                    return self.fail(fault::BAD_OPERAND);
                };
                ctx.flash.write(addr, &buf[..n]);
            }
        } else {
            self.fail(fault::CODE_OOB);
        }
    }

    // --- Builtins (args on stack) ---
    pub(super) fn op_draw<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        self.draw_op(ctx, op);
    }

    pub(super) fn op_delay<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let ms = self.pop() as u32;
        self.wait_until = ctx.systick.millis().wrapping_add(ms);
        self.state = VmState::Waiting;
    }

    pub(super) fn op_str_lit<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Inline text: u8 len + text bytes → interned pool string (an
        // identical live literal is reused), push str_id
        let len = self.read_u8() as usize;
        let start = self.pc as usize;
        if start + len <= self.code.len() {
            self.pc = (start + len) as CodeAddr;
            match self.code.with_bytes(start, len, |text| ctx.objs.str_intern(text)) {
                Some(id) => self.push(id as i32),
                None => self.fail(fault::STR_ALLOC),
            }
        } else {
            self.fail(fault::CODE_OOB);
        }
    }

    pub(super) fn op_itos<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let val = self.pop();
        match strpool::itos(&mut ctx.objs, val) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::STR_ALLOC),
        }
    }

    pub(super) fn op_ftos<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let bits = self.pop() as u32;
        match strpool::ftos(&mut ctx.objs, bits) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::STR_ALLOC),
        }
    }

    pub(super) fn op_concat<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (a, b) = self.pop2();
        let (a, b) = (a as ObjPtr, b as ObjPtr);
        match ctx.objs.str_concat(a, b) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::STR_ALLOC),
        }
    }

    pub(super) fn op_parse_int<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let id = self.pop() as ObjPtr;
        self.push(strpool::parse_int(&ctx.objs, id));
    }

    pub(super) fn op_parse_float<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let id = self.pop() as ObjPtr;
        self.push(strpool::parse_float(&ctx.objs, id) as i32);
    }

    pub(super) fn op_str_len<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let id = self.pop() as ObjPtr;
        self.push(ctx.objs.str_len(id) as i32);
    }

    pub(super) fn op_str_byte<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (id, idx) = self.pop2();
        let bytes = ctx.objs.str_get(id as ObjPtr);
        let b = usize::try_from(idx).ok().and_then(|i| bytes.get(i));
        self.push(b.map_or(-1, |&b| b as i32));
    }

    pub(super) fn op_str_cmp<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (a, b) = self.pop2();
        let (a, b) = (a as ObjPtr, b as ObjPtr);
        self.push(ctx.objs.str_compare(a, b));
    }

    pub(super) fn op_set_text<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let str_id = self.pop() as ObjPtr;
        if self.target.is_some() {
            ctx.objs.update_ext(self.target, |ext| {
                ext.text_id = str_id;
            });
            // Text change repaints on the next render() — same as the
            // W_SET text path.
            ctx.objs.mark_dirty(self.target);
        }
    }

    pub(super) fn op_str_clear<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        ctx.objs.str_sweep();
    }

    pub(super) fn op_str_free<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let str_id = self.pop() as ObjPtr;
        ctx.objs.str_free(str_id);
    }

    pub(super) fn op_begin_frame<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        render::wait_idle(ctx);
        ctx.objs.screen_touched();
        ctx.lcd.begin_frame();
    }

    pub(super) fn op_end_frame<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        render::wait_idle(ctx);
        ctx.objs.screen_touched();
        ctx.lcd.end_frame();
    }

    pub(super) fn op_send_usart<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let arr_id = self.pop() as ObjPtr;
        if let Some(arr) = ctx.objs.arr(arr_id) {
            for &val in arr {
                ctx.usart.write_byte(val as u8);
            }
        }
    }

    pub(super) fn op_send_usart_str<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let str_id = self.pop() as ObjPtr;
        let bytes = ctx.objs.str_get(str_id);
        ctx.usart.write(bytes);
    }

    pub(super) fn op_rtc_read<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Read RTC → allocate array [sec, min, hour, day, weekday, month, year]
        let dt = ctx.rtc.read_time();
        let data = [
            dt.second as i32,
            dt.minute as i32,
            dt.hour as i32,
            dt.day as i32,
            dt.weekday as i32,
            dt.month as i32,
            dt.year as i32,
        ];
        match ctx.objs.arr_from(&data) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::ARR_ALLOC),
        }
    }

    pub(super) fn op_rtc_write<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Pop arr_id, read [sec, min, hour, day, weekday, month, year], set RTC
        let arr_id = self.pop();
        match Self::find_array(ctx, arr_id) {
            Some(arr) => {
                if arr.len() >= 7 {
                    let dt = crate::rtc::DateTime {
                        second: arr[0] as u8,
                        minute: arr[1] as u8,
                        hour: arr[2] as u8,
                        day: arr[3] as u8,
                        weekday: arr[4] as u8,
                        month: arr[5] as u8,
                        year: arr[6] as u8,
                    };
                    ctx.rtc.set_time(&dt);
                } else {
                    self.fail(fault::BAD_OPERAND);
                }
            }
            None => self.fail(fault::BAD_ARRAY),
        }
    }

    pub(super) fn op_millis<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.push(ctx.systick.millis() as i32);
    }

    pub(super) fn op_fpga_cmd<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        render::wait_idle(ctx);
        ctx.objs.screen_touched();
        // fpgaCmd(cmd, data) — raw FPGA command + data
        let (cmd, data) = self.pop2();
        let (cmd, data) = (cmd as u16, data as u16);
        ctx.lcd.send_command(cmd);
        ctx.lcd.send_data(data);
    }

    pub(super) fn op_fpga_dat<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        render::wait_idle(ctx);
        ctx.objs.screen_touched();
        // fpgaData(data) — raw FPGA data (after a cmd)
        let data = self.pop() as u16;
        ctx.lcd.send_data(data);
    }

    pub(super) fn op_critical(&mut self) {
        self.critical = true;
    }

    pub(super) fn op_set_brightness<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let percent = self.pop() as u8;
        ctx.backlight.set_brightness(percent);
        // Saved by the board, applied at the next start.
        crate::settings::note_brightness(percent, ctx.systick.millis());
    }

    pub(super) fn op_brightness<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.push(ctx.backlight.brightness() as i32);
    }

    // --- User GPIO ---
    pub(super) fn op_pin_mode<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (pin, mode) = self.pop2();
        if !ctx.gpio.set_mode(pin, mode) {
            self.fail(fault::GPIO);
        }
    }

    pub(super) fn op_pin_write<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (pin, value) = self.pop2();
        if !ctx.gpio.write(pin, value) {
            self.fail(fault::GPIO);
        }
    }

    pub(super) fn op_pin_read<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let pin = self.pop();
        match ctx.gpio.read(pin) {
            Some(level) => self.push(level),
            None => self.fail(fault::GPIO),
        }
    }

    // --- User EEPROM ---
    pub(super) fn op_eeprom_size<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.push(ctx.eeprom.size());
    }

    pub(super) fn op_eeprom_read<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let addr = self.pop();
        let mut b = [0u8; 1];
        if self.eeprom_io(ctx, addr, &mut b, false) {
            self.push(b[0] as i32);
        } else {
            self.fail(fault::EEPROM);
        }
    }

    pub(super) fn op_eeprom_write<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (addr, value) = self.pop2();
        if !self.eeprom_io(ctx, addr, &mut [value as u8], true) {
            self.fail(fault::EEPROM);
        }
    }

    pub(super) fn op_eeprom_array<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        let count = self.pop();
        let (addr, arr_id) = self.pop2();
        self.eeprom_array(ctx, op == OP_EEPROM_WRITE_ARR, addr, arr_id, count);
    }

    // --- Programs ---
    pub(super) fn op_run_program<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (str_id, arg) = self.pop2();
        let name = ctx.objs.str_get(str_id as ObjPtr);
        let found = name.len() <= crate::fs::NAME_LEN
            && ctx.fs_of(self.domain).is_some_and(|fs| {
                fs.find_kind(&ctx.flash, name, crate::fs::RES_PROGRAM).is_some()
            });
        if found {
            // Ends everything at once: no instruction of this program
            // runs after this one.
            self.next_program = Some(ProgramRef::new(name, arg));
            self.state = VmState::Exit;
        } else {
            self.push(0);
        }
    }

    pub(super) fn op_program_arg(&mut self) {
        self.push(self.program.arg);
    }

    pub(super) fn op_task<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        self.task_op(ctx, op);
    }

    pub(super) fn op_flex_layout<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let bx = self.pop();
        self.flex_layout(ctx, bx);
        self.push(0);
    }

    pub(super) fn op_disk<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        self.storage_op(ctx, op);
    }

    pub(super) fn op_gesture<P: Platform>(&mut self, ctx: &mut Ctx<P>, op: u8) {
        self.gesture_op(ctx, op);
    }

    pub(super) fn op_program_name<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let index = self.pop();
        let mut buf = [0u8; crate::fs::NAME_LEN];
        let len = if index < 0 {
            let own = self.program.name();
            buf[..own.len()].copy_from_slice(own);
            own.len()
        } else {
            ctx.fs_of(self.domain)
                .and_then(|fs| {
                    let n = index.min(u16::MAX as i32) as u16;
                    fs.nth_name_by_kind(&ctx.flash, crate::fs::RES_PROGRAM, n, &mut buf)
                })
                .unwrap_or(0)
        };
        match ctx.objs.str_alloc(&buf[..len]) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::STR_ALLOC),
        }
    }

    // --- Sound (RES_SOUND resources, streamed by runtime's audio pump) ---
    pub(super) fn op_sound_play<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (str_id, looping) = self.pop2();
        let name = ctx.objs.str_get(str_id as ObjPtr);
        let ok = match ctx.fs_of(self.domain).cloned() {
            Some(fs) => ctx.audio.play(&fs, &ctx.flash, name, looping != 0),
            None => false,
        };
        self.push(ok as i32);
    }

    pub(super) fn op_sound_stop<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        ctx.audio.stop();
    }

    pub(super) fn op_sound_volume<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let vol = self.pop().clamp(0, 100) as u8;
        ctx.audio.set_volume(vol);
        crate::settings::note_volume(vol, ctx.systick.millis());
    }

    pub(super) fn op_sound_playing<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.push(ctx.audio.playing() as i32);
    }

    // --- Animation engine ---
    pub(super) fn op_animate<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        // Pushed order: widget, prop, to, duration, ctrl, on_end.
        let (ctrl, on_end) = self.pop2();
        let (ctrl, on_end) = (ctrl as u8, on_end as u16);
        let duration = self.pop() as u16;
        let to = self.pop();
        let (widget, prop) = self.pop2();
        let (widget, prop) = (WidgetId(widget as ObjPtr), prop as u8);
        let now = ctx.systick.millis();
        flex::flush(ctx); // FLEX_AUTO: tween from the laid-out value
        self.start_anim(ctx, widget, prop, to, duration, ctrl, on_end, now);
    }

    pub(super) fn op_stop_anim(&mut self) {
        let widget = WidgetId(self.pop() as ObjPtr);
        self.stop_anim(widget);
    }

    pub(super) fn op_stop_anim_prop(&mut self) {
        let (widget, prop) = self.pop2();
        let (widget, prop) = (WidgetId(widget as ObjPtr), prop as u8);
        self.stop_anim_prop(widget, prop);
    }

    // --- File ops (flash filesystem, read-only) ---
    pub(super) fn op_file_open<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let str_id = self.pop() as ObjPtr;
        let name = ctx.objs.str_get(str_id);
        let result = self.file_open(ctx, name);
        self.push(result);
    }

    pub(super) fn op_file_read<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let handle = self.pop();
        if let Some(slot) = self.storage_slot(ctx, handle) {
            let mut b = [0u8; 1];
            let r = match ctx.storage.read(slot, &mut b) {
                Ok(1) => b[0] as i32,
                _ => FILE_EOF,
            };
            return self.push(r);
        }
        match self.file_slot_mut(handle) {
            Some(f) => {
                if f.pos >= f.size {
                    self.push(FILE_EOF);
                } else {
                    let addr = f.offset + f.pos;
                    let mut buf = [0u8; 1];
                    ctx.flash.read(addr, &mut buf);
                    f.pos += 1;
                    self.push(buf[0] as i32);
                }
            }
            None => self.fail(fault::BAD_FILE),
        }
    }

    pub(super) fn op_file_size<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let handle = self.pop();
        if let Some(slot) = self.storage_slot(ctx, handle) {
            let r = ctx.storage.size(slot).map_or_else(|e| e, |n| n as i32);
            return self.push(r);
        }
        match self.file_slot(handle) {
            Some(f) => self.push(f.size as i32),
            None => self.fail(fault::BAD_FILE),
        }
    }

    pub(super) fn op_file_close<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let handle = self.pop();
        if let Some(slot) = self.storage_slot(ctx, handle) {
            ctx.storage.close(slot);
            return;
        }
        match self.file_slot_index(handle) {
            Some(idx) => self.open_files[idx] = None,
            None => self.fail(fault::BAD_FILE),
        }
    }

    pub(super) fn op_file_seek<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (handle, pos) = self.pop2();
        if let Some(slot) = self.storage_slot(ctx, handle) {
            let _ = ctx.storage.seek(slot, pos);
            return;
        }
        match self.file_slot_mut(handle) {
            Some(f) => f.pos = (pos.max(0) as u32).min(f.size),
            None => self.fail(fault::BAD_FILE),
        }
    }

    pub(super) fn op_show_modal<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.handle_show_modal(ctx);
    }

    pub(super) fn op_set_dialog_result(&mut self) {
        let result = self.pop();
        if let Some(top) = self.modal_stack.last_mut() {
            if !top.has_result {
                top.result = result;
                top.has_result = true;
            }
        }
    }

    pub(super) fn op_arr_to_str<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let (arr_id, len) = self.pop2();
        let mut buf = [0u8; 256];
        let take = match Self::find_array(ctx, arr_id) {
            Some(data) => {
                let take = if len < 0 {
                    data.len()
                } else {
                    (len as usize).min(data.len())
                };
                if take <= buf.len() {
                    for i in 0..take {
                        buf[i] = data[i] as u8;
                    }
                }
                take
            }
            None => return self.fail(fault::BAD_ARRAY),
        };
        if take > buf.len() {
            self.fail(fault::BAD_OPERAND);
        } else {
            match ctx.objs.str_alloc(&buf[..take]) {
                Some(id) => self.push(id as i32),
                None => self.fail(fault::STR_ALLOC),
            }
        }
    }

    pub(super) fn op_sprintf<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let argc = self.read_u8() as usize;
        let mut args = [0i32; 8];
        let actual = argc.min(8);
        for i in (0..actual).rev() {
            args[i] = self.pop();
        }
        let fmt_id = self.pop() as ObjPtr;
        // Copied: `sprintf` allocates in the pool the format lives in.
        let fmt = ctx.objs.str_get(fmt_id).to_vec();
        match strpool::sprintf(&mut ctx.objs, &fmt, &args[..actual]) {
            Some(id) => self.push(id as i32),
            None => self.fail(fault::STR_ALLOC),
        }
    }

    pub(super) fn op_syscall<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        let syscall_id = self.read_u8();
        let argc = self.read_u8() as usize;
        let actual = argc.min(MAX_SYSCALL_ARGS);
        let mut args = [0i32; MAX_SYSCALL_ARGS];
        for i in (0..actual).rev() {
            args[i] = self.pop();
        }
        // App syscalls (0x80..=0x8A) are the core's: `runApp` ends the task.
        if matches!(syscall_id, crate::apps::SYS_APP_BOOT | crate::apps::SYS_APP_SET_BOOT) {
            let r = crate::apps::boot_syscall(
                syscall_id,
                &args[..actual],
                &mut ctx.objs,
                P::app_files(),
                P::saved_boot_app,
                P::set_boot_app,
            );
            match r {
                Some(v) => self.push(v),
                None => self.fail(fault::STR_ALLOC),
            }
            return;
        }
        if (crate::apps::SYS_APP_CURRENT..=crate::apps::SYS_APP_EXIT).contains(&syscall_id) {
            return self.app_call(ctx, syscall_id, &args[..actual]);
        }
        if (crate::apps::SYS_APP_COUNT..crate::apps::SYS_APP_CURRENT).contains(&syscall_id) {
            match crate::apps::syscall(syscall_id, &args[..actual], &mut ctx.objs, P::app_files()) {
                Some(crate::apps::Call::Value(v)) => self.push(v),
                Some(crate::apps::Call::Exit) => {
                    self.next_program = None;
                    self.state = VmState::Exit;
                }
                None => self.fail(fault::STR_ALLOC),
            }
            return;
        }
        match self.syscall_fn {
            Some(f) => match f(syscall_id, &args[..actual], &mut ctx.objs) {
                Some(v) => self.push(v),
                None => self.fail(fault::SYSCALL),
            },
            None => self.fail(fault::SYSCALL),
        }
    }

    pub(super) fn op_call_imp(&mut self) {
        let i = self.read_u16() as usize;
        match self.links.get(i).copied() {
            Some(target) if (self.call_sp as usize) < self.call_stack.len() => {
                self.call_stack[self.call_sp as usize] = CallFrame {
                    ret_addr: self.pc | self.utag,
                    frame_base: self.frame_base as u16,
                    frame_size: self.frame_size,
                };
                self.call_sp += 1;
                self.frame_base += self.frame_size as u32;
                self.frame_size = 0;
                self.goto(target);
            }
            Some(_) => self.fail(fault::CALL_OVERFLOW),
            None => self.fail(fault::BAD_OPERAND),
        }
    }

    pub(super) fn op_fn_ref(&mut self) {
        let id = self.read_u16();
        self.push(((self.unit as i32) << 12) | (id & 0x0FFF) as i32);
    }
}
