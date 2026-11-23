//! Test-only reference interpreter: `exec_one` as it was before the one
//! execution path (commit 8cf3ef5), the independent check of `tc.rs`'s
//! handlers. The instructions `tc` runs in its own handlers (stack,
//! arithmetic, comparisons, loads / stores and their short forms, jumps,
//! CALL, frames, array loads, the superinstructions and quickened forms)
//! have their own bodies here, written apart from `tc`; the rest call the
//! same `ops.rs` methods `tc` does. Nothing here quickens: a quickened form
//! met in the code runs as its instruction. A change to an instruction's
//! meaning goes here and into `tc` both.

use super::*;

impl Vm {
    /// One instruction, as `step` (the instruction count included).
    pub(super) fn step_ref<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.instructions = self.instructions.wrapping_add(1);
        self.exec_ref(ctx);
    }

    fn exec_ref<P: Platform>(&mut self, ctx: &mut Ctx<P>) {
        self.op_pc = self.pc;
        let op = self.read_u8();
        if self.state == VmState::Error {
            return;
        }

        match op {
            OP_HALT => self.op_halt(),
            OP_POP => {
                self.pop();
            }
            OP_DUP => {
                let v = self.peek();
                self.push(v);
            }
            OP_SWAP => {
                if self.sp - self.sp_floor >= 2 {
                    let i = (self.sp - 1) as usize;
                    let j = (self.sp - 2) as usize;
                    self.stack.swap(i, j);
                } else {
                    self.fail(fault::STACK_UNDERFLOW);
                }
            }
            OP_ADD => {
                let (a, b) = self.pop2();
                self.push(a.wrapping_add(b));
            }
            OP_SUB => {
                let (a, b) = self.pop2();
                self.push(a.wrapping_sub(b));
            }
            OP_MUL => {
                let (a, b) = self.pop2();
                self.push(a.wrapping_mul(b));
            }
            OP_DIV => {
                let (a, b) = self.pop2();
                self.push(if b != 0 { a.wrapping_div(b) } else { 0 });
            }
            OP_MOD => {
                let (a, b) = self.pop2();
                self.push(if b != 0 { a.wrapping_rem(b) } else { 0 });
            }
            OP_NEG => {
                let a = self.pop();
                self.push(a.wrapping_neg());
            }
            OP_AND => {
                let (a, b) = self.pop2();
                self.push(a & b);
            }
            OP_OR => {
                let (a, b) = self.pop2();
                self.push(a | b);
            }
            OP_SHL => {
                let (a, b) = self.pop2();
                self.push(a << (b as u32 & 31));
            }
            OP_SHR => {
                let (a, b) = self.pop2();
                self.push(a >> (b as u32 & 31));
            }
            OP_NOT => {
                let a = self.pop();
                self.push(if a == 0 { 1 } else { 0 });
            }
            OP_EQ => {
                let (a, b) = self.pop2();
                self.push(if a == b { 1 } else { 0 });
            }
            OP_NE => {
                let (a, b) = self.pop2();
                self.push(if a != b { 1 } else { 0 });
            }
            OP_LT => {
                let (a, b) = self.pop2();
                self.push(if a < b { 1 } else { 0 });
            }
            OP_LE => {
                let (a, b) = self.pop2();
                self.push(if a <= b { 1 } else { 0 });
            }
            OP_GT => {
                let (a, b) = self.pop2();
                self.push(if a > b { 1 } else { 0 });
            }
            OP_GE => {
                let (a, b) = self.pop2();
                self.push(if a >= b { 1 } else { 0 });
            }
            OP_RET => self.op_ret(ctx),
            OP_YIELD => self.op_yield(),
            OP_W_DIRTY => self.op_w_dirty(ctx),
            OP_W_RENDER => self.op_w_render(ctx),
            OP_ARR_LOAD => {
                let (arr_id, idx) = self.pop2();
                self.arr_load(ctx, arr_id, idx);
            }
            OP_ARR_STORE => self.op_arr_store(ctx),
            OP_ARR_LEN => self.op_arr_len(ctx),
            OP_W_ALLOC => self.op_w_alloc(ctx),
            OP_W_ALLTAR => self.op_w_alltar(ctx),
            OP_FRAME => {
                // Function prologue: set frame_size for this function
                let count = self.read_u8();
                if self.frame_base + count as u32 > self.var_slots {
                    self.fail(fault::VARS_FULL);
                } else {
                    self.frame_size = count;
                }
            }
            OP_ARR_FREE => self.op_arr_free(ctx),

            // --- Specialized short forms ---
            OP_PUSH_0 => {
                self.push(0);
            }
            OP_PUSH_1 => {
                self.push(1);
            }
            OP_PUSH_2 => {
                self.push(2);
            }
            OP_PUSH_M1 => {
                self.push(-1);
            }
            // Short forms: LOAD/STORE local slots 0-4
            OP_LOAD_0 => {
                let s = self.frame_base;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_1 => {
                let s = self.frame_base + 1;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_2 => {
                let s = self.frame_base + 2;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_3 => {
                let s = self.frame_base + 3;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_LOAD_4 => {
                let s = self.frame_base + 4;
                let v = self.var_get(s);
                self.push(v);
            }
            OP_STORE_0 => {
                let s = self.frame_base;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_1 => {
                let s = self.frame_base + 1;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_2 => {
                let s = self.frame_base + 2;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_3 => {
                let s = self.frame_base + 3;
                let v = self.pop();
                self.var_set(s, v);
            }
            OP_STORE_4 => {
                let s = self.frame_base + 4;
                let v = self.pop();
                self.var_set(s, v);
            }

            // --- With arguments ---
            OP_PUSH_I8 => {
                let val = self.read_i8() as i32;
                self.push(val);
            }
            OP_PUSH_I16 => {
                let val = self.read_i16() as i32;
                self.push(val);
            }
            OP_PUSH_I32 => {
                let val = self.read_i32();
                self.push(val);
            }
            OP_LOAD => {
                let raw = self.read_u8();
                let slot = self.resolve_slot(raw);
                let v = self.var_get(slot);
                self.push(v);
            }
            OP_STORE => {
                let raw = self.read_u8();
                let slot = self.resolve_slot(raw);
                let v = self.pop();
                self.var_set(slot, v);
            }
            OP_JMP => {
                let target = self.read_addr();
                self.pc = target;
            }
            OP_JZ => {
                let target = self.read_addr();
                if self.pop() == 0 {
                    self.pc = target;
                }
            }
            OP_JNZ => {
                let target = self.read_addr();
                if self.pop() != 0 {
                    self.pc = target;
                }
            }
            OP_CALL => {
                let target = self.read_addr();
                if (self.call_sp as usize) < self.call_stack.len() {
                    self.call_stack[self.call_sp as usize] = CallFrame {
                        ret_addr: self.pc | self.utag,
                        frame_base: self.frame_base as u16,
                        frame_size: self.frame_size,
                    };
                    self.call_sp += 1;
                    // Advance frame_base past caller's locals
                    self.frame_base += self.frame_size as u32;
                    self.frame_size = 0; // callee's FRAME will set this
                    self.pc = target;
                } else {
                    self.fail(fault::CALL_OVERFLOW);
                }
            }
            OP_W_TARGET => self.op_w_target(ctx),
            OP_W_TARGET_S => self.op_w_target_s(ctx),
            OP_W_SET => self.op_w_set(ctx),
            OP_W_GET => self.op_w_get(ctx),
            OP_W_PARENT => self.op_w_parent(ctx),
            OP_W_PARENT_S => self.op_w_parent_s(ctx),
            OP_W_LOAD => self.op_w_load(ctx),
            OP_W_SETALL => self.op_w_setall(ctx),
            OP_W_SET_LEN => self.op_w_set_len(ctx),
            OP_ARR_ALLOC => self.op_arr_alloc(ctx),
            OP_ARR_ALLOC_S => self.op_arr_alloc_s(ctx),
            OP_ARR_INIT => self.op_arr_init(ctx),
            OP_F_READ => self.op_f_read(ctx),
            OP_F_WRITE => self.op_f_write(ctx),
            OP_FILL_RECT | OP_RECT | OP_LINE | OP_CIRCLE | OP_FILL_CIRCLE | OP_DRAW_IMAGE
            | OP_DRAW_TEXT_LIT | OP_DRAW_STR | OP_ROUNDED_RECT | OP_FILL_ROUNDED_RECT | OP_ARC => {
                self.op_draw(ctx, op)
            }
            OP_DELAY => self.op_delay(ctx),
            OP_STR_LIT => self.op_str_lit(ctx),
            OP_ITOS => self.op_itos(ctx),
            OP_FTOS => self.op_ftos(ctx),
            OP_CONCAT => self.op_concat(ctx),
            OP_PARSE_INT => self.op_parse_int(ctx),
            OP_PARSE_FLOAT => self.op_parse_float(ctx),
            OP_STR_LEN => self.op_str_len(ctx),
            OP_STR_BYTE => self.op_str_byte(ctx),
            OP_STR_CMP => self.op_str_cmp(ctx),
            OP_SET_TEXT => self.op_set_text(ctx),
            OP_STR_CLEAR => self.op_str_clear(ctx),
            OP_STR_FREE => self.op_str_free(ctx),
            OP_BEGIN_FRAME => self.op_begin_frame(ctx),
            OP_END_FRAME => self.op_end_frame(ctx),
            OP_SEND_USART => self.op_send_usart(ctx),
            OP_SEND_USART_STR => self.op_send_usart_str(ctx),
            OP_RTC_READ => self.op_rtc_read(ctx),
            OP_RTC_WRITE => self.op_rtc_write(ctx),
            OP_MILLIS => self.op_millis(ctx),
            OP_FPGA_CMD => self.op_fpga_cmd(ctx),
            OP_FPGA_DAT => self.op_fpga_dat(ctx),
            OP_CRITICAL => self.op_critical(),
            OP_SET_BRIGHTNESS => self.op_set_brightness(ctx),
            OP_BRIGHTNESS => self.op_brightness(ctx),
            OP_PIN_MODE => self.op_pin_mode(ctx),
            OP_PIN_WRITE => self.op_pin_write(ctx),
            OP_PIN_READ => self.op_pin_read(ctx),
            OP_EEPROM_SIZE => self.op_eeprom_size(ctx),
            OP_EEPROM_READ => self.op_eeprom_read(ctx),
            OP_EEPROM_WRITE => self.op_eeprom_write(ctx),
            OP_EEPROM_READ_ARR | OP_EEPROM_WRITE_ARR => self.op_eeprom_array(ctx, op),
            OP_RUN_PROGRAM => self.op_run_program(ctx),
            OP_PROGRAM_ARG => self.op_program_arg(),
            OP_TASK_SPAWN..=OP_TASK_FG => self.op_task(ctx, op),
            OP_FLEX_LAYOUT => self.op_flex_layout(ctx),
            disk::OP_DISK_OPEN..=disk::OP_FILE_READ_ARR => self.op_disk(ctx, op),
            gesture_ops::OP_ON_GESTURE..=gesture_ops::OP_GESTURE_INFO => self.op_gesture(ctx, op),
            OP_PROGRAM_NAME => self.op_program_name(ctx),
            OP_SOUND_PLAY => self.op_sound_play(ctx),
            OP_SOUND_STOP => self.op_sound_stop(ctx),
            OP_SOUND_VOLUME => self.op_sound_volume(ctx),
            OP_SOUND_PLAYING => self.op_sound_playing(ctx),
            OP_ANIMATE => self.op_animate(ctx),
            OP_STOP_ANIM => self.op_stop_anim(),
            OP_STOP_ANIM_PROP => self.op_stop_anim_prop(),
            OP_FILE_OPEN => self.op_file_open(ctx),
            OP_FILE_READ => self.op_file_read(ctx),
            OP_FILE_SIZE => self.op_file_size(ctx),
            OP_FILE_CLOSE => self.op_file_close(ctx),
            OP_FILE_SEEK => self.op_file_seek(ctx),
            OP_SHOW_MODAL => self.op_show_modal(ctx),
            OP_SET_DIALOG_RESULT => self.op_set_dialog_result(),
            OP_ARR_TO_STR => self.op_arr_to_str(ctx),
            OP_SPRINTF => self.op_sprintf(ctx),
            OP_SYSCALL => self.op_syscall(ctx),
            OP_ITOF => self.op_itof(),
            OP_FTOI => self.op_ftoi(),
            OP_FADD => self.op_fadd(),
            OP_FSUB => self.op_fsub(),
            OP_FMUL => self.op_fmul(),
            OP_FDIV => self.op_fdiv(),
            OP_FNEG => self.op_fneg(),
            OP_FEQ => self.op_feq(),
            OP_FLT => self.op_flt(),
            OP_FLE => self.op_fle(),
            OP_FGT => self.op_fgt(),
            OP_FGE => self.op_fge(),
            OP_FNE => self.op_fne(),
            OP_FSIN => self.op_fsin(),
            OP_FCOS => self.op_fcos(),
            OP_FSQRT => self.op_fsqrt(),
            OP_FABS => self.op_fabs(),
            OP_FATAN2 => self.op_fatan2(),
            OP_FFLOOR => self.op_ffloor(),
            OP_FCEIL => self.op_fceil(),

            OP_JCMP | OP_JCMP_VI | OP_JCMP_VV | OP_BIN_I | OP_BIN_I32 | OP_BIN_V | OP_STORE_OP => {
                self.exec_super(op, None, SLOT_ANY)
            }
            Q_FIRST..=Q_LAST => {
                self.exec_quick(ctx, op);
            }
            OP_ADD_VI => {
                let [raw, lo, hi] = self.fetch();
                let imm = i16::from_le_bytes([lo, hi]) as i32;
                if self.state != VmState::Error {
                    let slot = self.resolve_slot(raw);
                    let v = self.var_get(slot).wrapping_add(imm);
                    self.var_set(slot, v);
                }
            }
            OP_CALL_IMP => self.op_call_imp(),
            OP_FN_REF => self.op_fn_ref(),
            OP_FRAME_A => {
                // FRAME n; STORE nargs-1; ..; STORE 0 (the arguments, last on top)
                let [count, nargs] = self.fetch();
                if self.frame_base + count as u32 > self.var_slots {
                    self.fail(fault::VARS_FULL);
                } else {
                    self.frame_size = count;
                    let base = self.frame_base;
                    for i in (0..nargs as u32).rev() {
                        let v = self.pop();
                        self.var_set(base + i, v);
                    }
                }
            }
            OP_ARR_LOAD_V => {
                let raw = self.read_u8();
                let idx = self.pop();
                if self.state != VmState::Error {
                    let arr_id = self.var_get(self.resolve_slot(raw));
                    self.arr_load(ctx, arr_id, idx);
                }
            }

            _ => {
                self.fail(fault::BAD_OPCODE);
            }
        }
    }

    fn read_i8(&mut self) -> i8 {
        self.read_u8() as i8
    }

    fn read_i16(&mut self) -> i16 {
        self.read_u16() as i16
    }

    /// JMP/JZ/JNZ/CALL target: u32 (XL reads wide-address images only).
    fn read_addr(&mut self) -> CodeAddr {
        self.read_u32() as CodeAddr
    }

    fn peek(&self) -> i32 {
        if self.sp > self.sp_floor {
            self.stack[(self.sp - 1) as usize]
        } else {
            0
        }
    }

    /// Superinstruction tail: jump to `target` if `a <cond> b`.
    fn jump_if(&mut self, cond: u8, a: i32, b: i32, target: CodeAddr) {
        if self.state == VmState::Error {
            return;
        }
        match int_cmp(cond, a, b) {
            Some(true) => self.pc = target,
            Some(false) => {}
            None => self.fail(fault::BAD_OPCODE),
        }
    }

    /// Superinstruction tail: top of stack = top <op> b, in place.
    fn top_binop(&mut self, op: u8, b: i32) {
        if self.state == VmState::Error {
            return;
        }
        if self.sp <= self.sp_floor {
            self.fail(fault::STACK_UNDERFLOW);
            return;
        }
        let i = (self.sp - 1) as usize;
        // SAFETY: sp <= stack.len() (see pop2_raw) and sp > sp_floor >= 0.
        let top = unsafe { self.stack.get_unchecked_mut(i) };
        match int_binop(op, *top, b) {
            Some(v) => *top = v,
            None => self.fail(fault::BAD_OPCODE),
        }
    }

    /// A quickened form (`QUICK`; pc past its opcode): its instruction with
    /// the form's cond / op and slot kind.
    fn exec_quick<P: Platform>(&mut self, ctx: &Ctx<P>, q: u8) {
        let (op, sub, kind) = QUICK[(q - Q_FIRST) as usize];
        match op {
            // As the plain LOAD / STORE / ADD_VI in `exec_one`.
            OP_LOAD => {
                let raw = force_slot(self.read_u8(), kind);
                let slot = self.resolve_slot(raw);
                let v = self.var_get(slot);
                self.push(v);
            }
            OP_STORE => {
                let raw = force_slot(self.read_u8(), kind);
                let slot = self.resolve_slot(raw);
                let v = self.pop();
                self.var_set(slot, v);
            }
            OP_ADD_VI => {
                let [raw, lo, hi] = self.fetch();
                let imm = i16::from_le_bytes([lo, hi]) as i32;
                if self.state != VmState::Error {
                    let slot = self.resolve_slot(force_slot(raw, kind));
                    let v = self.var_get(slot).wrapping_add(imm);
                    self.var_set(slot, v);
                }
            }
            OP_ARR_LOAD_V => {
                let raw = force_slot(self.read_u8(), kind);
                let idx = self.pop();
                if self.state != VmState::Error {
                    let arr_id = self.var_get(self.resolve_slot(raw));
                    self.arr_load(ctx, arr_id, idx);
                }
            }
            _ => self.exec_super(op, Some(sub), kind),
        }
    }

    /// A superinstruction (pc past its opcode), its cond / op operand
    /// replaced by `sub` when given (a quickened form, `QUICK`: the operand
    /// byte is still read, then ignored).
    fn exec_super(&mut self, op: u8, sub: Option<u8>, kind: u8) {
        match op {
            OP_JCMP => {
                let cond = sub.unwrap_or(self.read_u8());
                let target = self.read_addr();
                let (a, b) = self.pop2();
                self.jump_if(cond, a, b, target);
            }
            OP_JCMP_VI => {
                let [cond, raw, lo, hi] = self.fetch();
                let imm = i16::from_le_bytes([lo, hi]) as i32;
                let target = self.read_addr();
                let a = self.var_get(self.resolve_slot(force_slot(raw, kind)));
                self.jump_if(sub.unwrap_or(cond), a, imm, target);
            }
            OP_JCMP_VV => {
                let [cond, ra, rb] = self.fetch();
                let target = self.read_addr();
                let a = self.var_get(self.resolve_slot(ra));
                let b = self.var_get(self.resolve_slot(rb));
                self.jump_if(sub.unwrap_or(cond), a, b, target);
            }
            OP_BIN_I => {
                let [op, lo, hi] = self.fetch();
                self.top_binop(sub.unwrap_or(op), i16::from_le_bytes([lo, hi]) as i32);
            }
            OP_BIN_I32 => {
                let [op, a, b, c, d] = self.fetch();
                self.top_binop(sub.unwrap_or(op), i32::from_le_bytes([a, b, c, d]));
            }
            OP_BIN_V => {
                let [op, raw] = self.fetch();
                let b = self.var_get(self.resolve_slot(force_slot(raw, kind)));
                self.top_binop(sub.unwrap_or(op), b);
            }
            _ => {
                // OP_STORE_OP
                let [op, raw] = self.fetch();
                let b = self.pop();
                if self.state != VmState::Error {
                    let slot = self.resolve_slot(force_slot(raw, kind));
                    match int_binop(sub.unwrap_or(op), self.var_get(slot), b) {
                        Some(v) => self.var_set(slot, v),
                        None => self.fail(fault::BAD_OPCODE),
                    }
                }
            }
        }
    }

    fn arr_load<P: Platform>(&mut self, ctx: &Ctx<P>, arr_id: i32, idx: i32) {
        match Self::find_array(ctx, arr_id) {
            Some(arr) => match arr.get(idx as usize) {
                Some(&v) if idx >= 0 => self.push(v),
                _ => self.fail(fault::ARRAY_INDEX),
            },
            None => self.fail(fault::BAD_ARRAY),
        }
    }

    // --- Float32 (a `tc` handler each) ---
    fn op_itof(&mut self) {
        let i = self.pop();
        self.push((i as f32).to_bits() as i32);
    }

    fn op_ftoi(&mut self) {
        let bits = self.pop() as u32;
        self.push(f32::from_bits(bits) as i32);
    }

    fn op_fadd(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push((a + b).to_bits() as i32);
    }

    fn op_fsub(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push((a - b).to_bits() as i32);
    }

    fn op_fmul(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push((a * b).to_bits() as i32);
    }

    fn op_fdiv(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push(if b != 0.0 { a / b } else { 0.0 }.to_bits() as i32);
    }

    fn op_fneg(&mut self) {
        let a = f32::from_bits(self.pop() as u32);
        self.push((-a).to_bits() as i32);
    }

    fn op_feq(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push(if a == b { 1 } else { 0 });
    }

    fn op_flt(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push(if a < b { 1 } else { 0 });
    }

    fn op_fle(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push(if a <= b { 1 } else { 0 });
    }

    fn op_fgt(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push(if a > b { 1 } else { 0 });
    }

    fn op_fge(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push(if a >= b { 1 } else { 0 });
    }

    fn op_fne(&mut self) {
        let (a, b) = self.pop2_f32();
        self.push(if a != b { 1 } else { 0 });
    }

    // --- Float math (trig / sqrt / rounding) ---
    fn op_fsin(&mut self) {
        let v = f32::from_bits(self.pop() as u32);
        self.push(libm::sinf(v).to_bits() as i32);
    }

    fn op_fcos(&mut self) {
        let v = f32::from_bits(self.pop() as u32);
        self.push(libm::cosf(v).to_bits() as i32);
    }

    fn op_fsqrt(&mut self) {
        let v = f32::from_bits(self.pop() as u32);
        self.push(libm::sqrtf(v).to_bits() as i32);
    }

    fn op_fabs(&mut self) {
        let v = f32::from_bits(self.pop() as u32);
        self.push(libm::fabsf(v).to_bits() as i32);
    }

    fn op_fatan2(&mut self) {
        let (y, x) = self.pop2_f32();
        self.push(libm::atan2f(y, x).to_bits() as i32);
    }

    fn op_ffloor(&mut self) {
        let v = f32::from_bits(self.pop() as u32);
        self.push(libm::floorf(v).to_bits() as i32);
    }

    fn op_fceil(&mut self) {
        let v = f32::from_bits(self.pop() as u32);
        self.push(libm::ceilf(v).to_bits() as i32);
    }
}
