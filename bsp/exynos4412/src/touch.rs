//! Touch backend — Atmel mXT1664S (T9 multitouch) on I2C5.
//!
//! Every finger down is reported (`read_points`, T9 touch ids 0..15 as the
//! pointer ids) for ferrite-core's gesture recognizers; `read_screen` is the
//! pointer finger: the first one down, until it lifts (then the next finger
//! that moves). Messages are drained on every `is_active` poll while CHG is
//! low.
//!
//! Positions come in T9's range (`x_range`/`y_range` from the controller) and
//! are scaled to the panel; `CalParams` only contributes swap/flip, so a
//! capacitive panel needs no calibration.

use exynos_4412_rs::drivers::gpio::Pull;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::embassy::eint::EintInput;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::mxt::{self, Message, Mxt};
use exynos_4412_rs::drivers::timer::Mct;
use ferrite_core::touch::{CalParams, TouchBackend, TouchPoint};

use crate::lcd::PANEL;

/// T9 reports up to 16 touch ids (the low 4 bits of a report id).
const FINGERS: usize = 16;

struct State {
    mxt: Mxt,
    x_range: u16,
    y_range: u16,
    /// Bit n = T9 touch n is down.
    down: u16,
    /// T9 id of the pointer finger, while one is down.
    pointer: Option<u8>,
    /// Last raw position of each touch id.
    at: [(u16, u16); FINGERS],
}

static mut STATE: Option<State> = None;

fn state() -> Option<&'static mut State> {
    unsafe { (*(&raw mut STATE)).as_mut() }
}

impl State {
    fn drain(&mut self) {
        // CHG low = the controller holds messages.
        while !mxt::CHG.read() {
            match self.mxt.read_message() {
                Ok(Some(Message::Touch(t))) => {
                    let id = t.id & 15;
                    let bit = 1u16 << id;
                    if t.down() {
                        self.down |= bit;
                        self.at[id as usize] = (t.x, t.y);
                        if self.pointer.is_none() {
                            self.pointer = Some(id);
                        }
                    } else {
                        self.down &= !bit;
                        if self.pointer == Some(id) {
                            self.pointer = None;
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }

    /// Raw T9 position → panel pixels, with the calibration's swap/flips.
    fn to_screen(&self, (mut rx, mut ry): (u16, u16), cal: &CalParams) -> (u16, u16) {
        let (mut xr, mut yr) = (self.x_range, self.y_range);
        if cal.xy_swap {
            core::mem::swap(&mut rx, &mut ry);
            core::mem::swap(&mut xr, &mut yr);
        }
        let mut x = scale(rx, xr, PANEL.width);
        let mut y = scale(ry, yr, PANEL.height);
        if cal.x_flip {
            x = PANEL.width as u16 - 1 - x;
        }
        if cal.y_flip {
            y = PANEL.height as u16 - 1 - y;
        }
        (x, y)
    }
}

pub struct MxtTouch;

fn scale(v: u16, range: u16, size: usize) -> u16 {
    ((v as u32 * (size as u32 - 1)) / range.max(1) as u32).min(size as u32 - 1) as u16
}

impl TouchBackend for MxtTouch {
    fn is_active(&self) -> bool {
        let Some(s) = state() else { return false };
        s.drain();
        s.down != 0
    }

    fn read_screen(&self, cal: &CalParams) -> Option<(u16, u16)> {
        let s = state()?;
        s.drain();
        let p = s.pointer?;
        Some(s.to_screen(s.at[p as usize], cal))
    }

    fn read_points(&self, cal: &CalParams, out: &mut [TouchPoint]) -> usize {
        let Some(s) = state() else { return 0 };
        s.drain();
        let mut n = 0;
        for id in 0..FINGERS {
            if s.down & (1 << id) != 0 && n < out.len() {
                let (x, y) = s.to_screen(s.at[id], cal);
                out[n] = TouchPoint { id: id as u8, x, y };
                n += 1;
            }
        }
        n
    }

    fn release_debounce(&self) -> u8 {
        // Releases are explicit T9 messages, not missing samples.
        1
    }
}

/// Sleeps (CPU0 in the executor) until `ms` have passed or the controller has a message for
/// us: CHG low, through its wake-up interrupt. The pin is the interrupt's only meanwhile and a
/// plain input again after, as [`State::drain`] reads its level (not relied on while the pin
/// is in its interrupt function). A message already waiting ends the sleep at once. Without a
/// controller, just the timer.
pub async fn sleep_until_touch(ms: u32) {
    let timer = embassy_time::Timer::after_millis(u64::from(ms));
    if state().is_none() {
        return timer.await;
    }
    let Some(chg) = EintInput::new(mxt::CHG, Pull::None) else { return timer.await };
    embassy_futures::select::select(timer, chg.wait_low()).await;
    // Back to the polled input. If the interrupt is still unmasked (the timer ended the
    // sleep), the input function keeps it from firing; a late one only wakes nothing.
    mxt::CHG.set_input(Pull::None);
}

/// Identity mapping: the controller's T9 ORIENT already matches the panel.
/// Flip here if a board turns out mounted otherwise.
pub const CAL: CalParams = CalParams {
    xy_swap: false,
    x_flip: false,
    y_flip: false,
    x_min: 0,
    x_max: PANEL.width as u16,
    y_min: 0,
    y_max: PANEL.height as u16,
};

/// Powers the controller (PMIC on I2C0), reads its object table and makes it
/// report. On failure touch stays off; the UI still runs.
pub fn init(pmic: Max77686, mct: Mct) -> Result<(), mxt::Error> {
    mxt::power_up(pmic, mct)?;
    let mxt = Mxt::open(I2c::new(Bus::I2c5))?;
    mxt.wake()?;
    let cfg = mxt.touch_config()?;
    unsafe {
        *(&raw mut STATE) = Some(State {
            mxt,
            x_range: cfg.x_range,
            y_range: cfg.y_range,
            down: 0,
            pointer: None,
            at: [(0, 0); FINGERS],
        });
    }
    Ok(())
}
