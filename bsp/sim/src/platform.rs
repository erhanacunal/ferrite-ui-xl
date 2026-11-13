//! `SimPlatform` — binds the host (minifb) backends to the framework and
//! implements the runtime hooks that `ferrite_core::run` calls.
//!
//! minifb's `Window` is `!Send` and the framebuffer / mouse state are shared
//! `Rc`s, so the window loop state lives in a thread-local (`HOST`) that
//! `frame_end` pumps once per iteration: sample the mouse for the next frame and
//! present the framebuffer.

use std::cell::RefCell;

use ferrite_core::platform::{CAP_AUDIO, CAP_JPEG, Platform};

use ferrite_core::ctx::Ctx;
use ferrite_core::runtime::{FullInput, PlatformRuntime};
use ferrite_core::pool::ObjPool;
use ferrite_core::types::{COLOR_BLACK, COLOR_WHITE};

use minifb::{Key, MouseButton, MouseMode, Window};

use crate::lcd::sim::Framebuffer;
use crate::lcd::{HEIGHT, WIDTH};
use crate::systick;
use crate::touch::sim::MouseState;

/// Marker type implementing `Platform` + `PlatformRuntime` for the host sim.
pub struct SimPlatform;

impl Platform for SimPlatform {
    type LcdB = crate::lcd::sim::SimLcd;
    type FlashB = crate::flash::sim::FileFlash;
    type TouchB = crate::touch::sim::MouseTouch;
    type BacklightB = crate::backlight::sim::StubBacklight;
    type RtcB = crate::rtc::sim::SystemRtc;
    type UsartB = crate::usart::sim::Stdio;
    type SystickB = crate::systick::SimSystick;
    type AudioB = crate::audio::SimAudio;
    type JpegB = crate::jpeg::SoftJpeg;
    type GpioB = crate::gpio::SimGpio;
    type EepromB = crate::eeprom::SimEeprom;
    type StorageB = crate::storage::SimStorage;
    type Renderer = ferrite_core::renderer::DefaultRenderer;

    // Software JPEG decoder (sim/src/jpeg.rs) + silent real-time audio
    // output (sim/src/audio.rs).
    const CAPS: u32 = CAP_AUDIO | CAP_JPEG | ferrite_core::platform::CAP_MULTI_PROGRAM | ferrite_core::platform::CAP_SUPEROPS | ferrite_core::platform::CAP_NATIVE_FLEX | ferrite_core::platform::CAP_STORAGE | ferrite_core::platform::CAP_GESTURES;
    // The tablet demo animates more widgets at once (exynos4412 has 64).
    const ANIM_SLOTS: usize = 64;

    const VERIFY_FS_ON_BOOT: bool = true;
}

// --- Host window state (thread-local; minifb Window is !Send) ---

struct Host {
    window: Window,
    fb: Framebuffer,
    mouse: MouseState,
    taps: Vec<ScriptedTap>,
    dump: Option<Dump>,
    /// Right button held in the last pass (a right click prints the display
    /// lists of the widget under the cursor).
    right: bool,
}

/// Deterministic run with frame dumps (`--dump DIR --dump-at MS,MS,..
/// [--step-ms N]`): the clock is virtual (each frame advances it by
/// `step_ms`, `delay` advances it instead of sleeping, the RTC starts at
/// 2026-01-01 00:00), the real mouse is ignored (scripted `--tap`/`--swipe`
/// only) and the window is not paced. At each `--dump-at` time the shown
/// screen is written to `DIR/frame_MS.bmp`; after the last one the sim exits.
/// With `--dl-dump` also `DIR/dl_MS.txt`: every widget's display lists
/// (`render::dl_dump`). `scripts/golden.py` drives it.
pub struct Dump {
    pub dir: std::path::PathBuf,
    pub at: Vec<u32>,
    pub step_ms: u32,
    next: usize,
    dl: bool,
}

impl Dump {
    /// Parse the dump options from the command line; `None` without `--dump`.
    pub fn from_args() -> Option<Dump> {
        let args: Vec<String> = std::env::args().collect();
        let val = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
        let dir = val("--dump")?;
        let mut at: Vec<u32> = val("--dump-at")
            .unwrap_or_default()
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        at.sort_unstable();
        if at.is_empty() {
            at.push(0);
        }
        let step_ms = val("--step-ms").and_then(|v| v.parse().ok()).unwrap_or(16).max(1);
        let dl = args.iter().any(|a| a == "--dl-dump");
        Some(Dump { dir: dir.into(), at, step_ms, next: 0, dl })
    }
}

/// 24-bit bottom-up BMP of an ARGB screen.
fn write_bmp(path: &std::path::Path, px: &[u32], w: usize, h: usize) -> std::io::Result<()> {
    let row = (w * 3 + 3) & !3;
    let size = 54 + row * h;
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&(size as u32).to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&54u32.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&(w as i32).to_le_bytes());
    b.extend_from_slice(&(h as i32).to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(&[0u8; 24]);
    for y in (0..h).rev() {
        let start = b.len();
        for &p in &px[y * w..y * w + w] {
            b.extend_from_slice(&[p as u8, (p >> 8) as u8, (p >> 16) as u8]);
        }
        b.resize(start + row, 0);
    }
    std::fs::write(path, b)
}

/// A scripted press: `--tap X,Y,MS` holds (x, y) for `TAP_HOLD_MS`;
/// `--swipe X0,Y0,X1,Y1,MS[,DUR]` presses at (x, y) and moves to (x1, y1)
/// over `dur` ms (default 250), then releases; `--pinch CX,CY,R0,R1,MS[,DUR[,DEG]]`
/// puts two fingers on opposite sides of (cx, cy), r0 from it, and moves
/// them to r1 while turning the pair by `deg` degrees (clockwise) over
/// `dur` ms. Times are from launch. minifb samples the real mouse, so
/// synthetic OS input cannot drive the sim — this lets tests click, swipe
/// and pinch.
#[derive(Clone, Copy)]
pub struct ScriptedTap {
    pub x: u16,
    pub y: u16,
    pub x1: u16,
    pub y1: u16,
    pub at_ms: u32,
    pub dur_ms: u32,
    pub pinch: Option<Pinch>,
}

/// Two fingers round a centre (`--pinch`): radius r0 → r1, turning by `deg`.
#[derive(Clone, Copy)]
pub struct Pinch {
    pub cx: f32,
    pub cy: f32,
    pub r0: f32,
    pub r1: f32,
    pub deg: f32,
}

impl ScriptedTap {
    /// Where the press is `t` ms after it started, and the second finger.
    fn at(&self, t: u32) -> ((u16, u16), Option<(u16, u16)>) {
        let f = t.min(self.dur_ms) as f32 / self.dur_ms.max(1) as f32;
        if let Some(p) = self.pinch {
            let r = p.r0 + (p.r1 - p.r0) * f;
            let a = (p.deg * f).to_radians();
            let (dx, dy) = (r * a.cos(), r * a.sin());
            let px = |v: f32, max: u32| v.round().clamp(0.0, (max - 1) as f32) as u16;
            let one = (px(p.cx + dx, WIDTH as u32), px(p.cy + dy, HEIGHT as u32));
            let two = (px(p.cx - dx, WIDTH as u32), px(p.cy - dy, HEIGHT as u32));
            return (one, Some(two));
        }
        let lerp = |a: u16, b: u16| (a as i64 + (b as i64 - a as i64) * t.min(self.dur_ms) as i64 / self.dur_ms.max(1) as i64) as u16;
        ((lerp(self.x, self.x1), lerp(self.y, self.y1)), None)
    }
}

/// How long a scripted tap holds the press (well under long-press timing).
pub const TAP_HOLD_MS: u32 = 150;

/// Main-loop passes (`sysinfo::SYS_PASSES`).
static PASSES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
use std::sync::atomic::Ordering;

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Hand the window, framebuffer and mouse state to the runtime so `frame_end`
/// can pump them. Call once, before `ferrite_core::run`.
pub fn install_host(window: Window, fb: Framebuffer, mouse: MouseState, taps: Vec<ScriptedTap>, dump: Option<Dump>) {
    HOST.with(|h| *h.borrow_mut() = Some(Host { window, fb, mouse, taps, dump, right: false }));
}

impl PlatformRuntime for SimPlatform {
    const BG_COLOR: u16 = COLOR_BLACK;
    const FG_COLOR: u16 = COLOR_WHITE;
    // The window present paces the loop to ~60 Hz; run a hardware-like burst of
    // VM instructions per frame so `millis()`-timed animation advances at the
    // intended rate instead of one instruction per frame. A program that yields
    // each frame stops the burst early, so this only speeds up busy loops.
    const VM_STEPS_PER_TICK: u32 = 50_000;
    // Plenty of RAM: load any program image (u16 VM address space).
    const MAX_CODE_SIZE: usize = 64 * 1024;
    type Input = FullInput;

    fn reset() -> ! {
        std::process::exit(0)
    }

    fn syscall(id: u8, args: &[i32], objs: &mut ObjPool) -> Option<i32> {
        match id {
            0x00 => Some(systick::millis() as i32), // SYS_UPTIME
            // Battery (lib/battery.fl): none on the host (-1 = unknown, charger 2 = powered),
            // unless `FERRITE_SIM_BATTERY=PERCENT,CHARGER` fakes one.
            0x05 | 0x08 | 0x09 => Some(sim_battery(id)),
            // 3D model syscalls (lib/model3d.fl, exynos4412 model3d.rs): the sim has no GPU,
            // so a model widget shows its background; setters succeed, stats are 0.
            0x60..=0x63 | 0x65 => Some(1),
            0x64 => Some(0),
            // No heap figures on the host; frames = passes.
            _ => ferrite_core::sysinfo::syscall(id, args, objs, || {
                let passes = PASSES.load(Ordering::Relaxed);
                ferrite_core::sysinfo::Info { passes, frames: passes, heap_used: 0, heap_total: 0, cpu_mhz: 0 }
            }),
        }
    }

    fn frame_end(ctx: &mut Ctx<Self>) {
        PASSES.fetch_add(1, Ordering::Relaxed);
        HOST.with(|h| {
            let mut guard = h.borrow_mut();
            let host = match guard.as_mut() {
                Some(h) => h,
                None => return,
            };

            if !host.window.is_open() || host.window.is_key_down(Key::Escape) {
                std::process::exit(0);
            }

            // Sample the mouse for the next frame's input dispatch (scripted
            // presses only in a dump run).
            let pressed = host.window.get_mouse_down(MouseButton::Left) && host.dump.is_none();
            let pos = host.window.get_mouse_pos(MouseMode::Discard);
            let (mx, my) = match pos {
                Some((x, y)) => (
                    x.clamp(0.0, (WIDTH - 1) as f32) as u16,
                    y.clamp(0.0, (HEIGHT - 1) as f32) as u16,
                ),
                None => (0, 0),
            };
            let now = systick::millis();
            // Right click (interactive runs): the display lists of the widget under the cursor.
            let right = host.window.get_mouse_down(MouseButton::Right) && host.dump.is_none();
            if right && !host.right && pos.is_some() {
                print_widget_lists(ctx, mx as i16, my as i16);
            }
            host.right = right;
            // `--dl-dump`: the lists behind the frames about to be dumped.
            if let Some(d) = host.dump.as_ref().filter(|d| d.dl) {
                for &at in d.at[d.next..].iter().take_while(|&&at| now >= at) {
                    let path = d.dir.join(format!("dl_{at:06}.txt"));
                    if let Err(e) = std::fs::write(&path, all_lists(ctx)) {
                        eprintln!("dump {}: {e}", path.display());
                    }
                }
            }
            match host.taps.iter().find(|t| now >= t.at_ms && now < t.at_ms + t.dur_ms) {
                Some(t) => {
                    let ((x, y), second) = t.at(now - t.at_ms);
                    host.mouse.update2(x, y, true, second)
                }
                None => {
                    // Ctrl + drag: a second finger mirrored round the screen's
                    // centre (pinch towards / away from it, turn round it).
                    let ctrl = host.window.is_key_down(Key::LeftCtrl) || host.window.is_key_down(Key::RightCtrl);
                    let second = ctrl.then(|| {
                        let mirror = |v: u16, max: u32| (max as i32 - 1 - v as i32).clamp(0, max as i32 - 1) as u16;
                        (mirror(mx, WIDTH as u32), mirror(my, HEIGHT as u32))
                    });
                    host.mouse.update2(mx, my, pressed && pos.is_some(), second)
                }
            }

            // Present the front (displayed) buffer (blocks to the target FPS
            // for pacing). In buffered mode this is whichever buffer the last
            // end_frame published; in dirty mode it is always buffer 0.
            let window = &mut host.window;
            let dump = &mut host.dump;
            host.fb.with_shown(|buf| {
                let _ = window.update_with_buffer(buf, WIDTH as usize, HEIGHT as usize);
                if let Some(d) = dump.as_mut() {
                    while d.next < d.at.len() && now >= d.at[d.next] {
                        let path = d.dir.join(format!("frame_{:06}.bmp", d.at[d.next]));
                        if let Err(e) = write_bmp(&path, buf, WIDTH as usize, HEIGHT as usize) {
                            eprintln!("dump {}: {e}", path.display());
                            std::process::exit(2);
                        }
                        d.next += 1;
                    }
                    if d.next == d.at.len() {
                        print_dl_stats();
                        print_snapshot_stats();
                        std::process::exit(0);
                    }
                    systick::sim::advance(d.step_ms);
                }
            });
        });
    }
}

/// `FERRITE_SIM_BATTERY=PERCENT,CHARGER` (e.g. `64,1`): a fake battery for the battery
/// syscalls (millivolts from a 3.0-4.2 V line); unset = none.
fn sim_battery(id: u8) -> i32 {
    static FAKE: std::sync::OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
    let fake = FAKE.get_or_init(|| {
        let v = std::env::var("FERRITE_SIM_BATTERY").ok()?;
        let (p, c) = v.split_once(',')?;
        Some((p.trim().parse().ok()?, c.trim().parse().ok()?))
    });
    match (id, fake) {
        (0x05, Some((p, _))) => 3000 + 12 * p,
        (0x08, Some((p, _))) => *p,
        (0x09, Some((_, c))) => *c,
        (0x09, None) => 2,
        _ => -1,
    }
}

/// Right click: the display lists of the topmost widget at (`x`, `y`), on stderr.
fn print_widget_lists(ctx: &mut Ctx<SimPlatform>, x: i16, y: i16) {
    let mut s = String::new();
    match ferrite_core::render::widget_at(ctx, x, y) {
        Some(id) => {
            let _ = ferrite_core::render::dl_dump(ctx, Some(id), &mut s);
        }
        None => s.push_str("no widget there\n"),
    }
    eprint!("sim: widget at {x},{y}: {s}");
}

/// `--dl-dump`: the summary, then each widget's lists.
fn all_lists(ctx: &mut Ctx<SimPlatform>) -> String {
    use ferrite_core::render::dl_dump;
    let mut s = String::new();
    let _ = dl_dump(ctx, None, &mut s);
    ctx.objs.ensure_dfs();
    let ids: Vec<_> = (0..ctx.objs.dfs_len()).map(|i| ctx.objs.dfs_at(i)).collect();
    for id in ids {
        s.push('\n');
        let _ = dl_dump(ctx, Some(id), &mut s);
    }
    s
}

/// Render-owned reads (`snapshot.rs`, stderr).
fn print_snapshot_stats() {
    use core::sync::atomic::Ordering::Relaxed;
    use ferrite_core::snapshot;
    eprintln!(
        "sim: snapshot reads {} stale-differing {}{}",
        snapshot::READS.load(Relaxed),
        snapshot::STALE_DIFF.load(Relaxed),
        if snapshot::stale() { " (stale reads on)" } else { "" }
    );
    {
        let p = &ferrite_core::render::PLAYS;
        if p[0].load(Relaxed) != 0 {
            eprintln!(
                "sim: play frames {} commands {} bytes {}",
                p[0].load(Relaxed),
                p[1].load(Relaxed),
                p[2].load(Relaxed)
            );
        }
        // Calls of the played frames by opcode (`FERRITE_SIM_OP_STATS`).
        if ferrite_core::dl::OP_STATS.load(Relaxed) {
            use ferrite_core::dl;
            let mut s = String::from("sim: dl ops");
            for i in 0..dl::OPCODES {
                let n = dl::OP_CALLS[i].load(Relaxed);
                if n != 0 {
                    s += &format!(" {}={}/{}w", dl::OP_NAMES[i], n, dl::OP_WORDS[i].load(Relaxed));
                }
            }
            eprintln!("{s}");
        }
        // Lists that referred to an image or font the replaying side didn't hold.
        let misses = ferrite_core::dl::REF_MISSES.load(Relaxed);
        if misses != 0 {
            eprintln!("sim: dl ref misses {misses}");
        }
    }
    use ferrite_core::scene;
    eprintln!(
        "sim: scene syncs: {} full, {} updates ({} widgets repacked){}",
        scene::FULLS.load(Relaxed),
        scene::UPDATES.load(Relaxed),
        scene::UPDATED.load(Relaxed),
        if scene::VERIFY.load(Relaxed) { " (verified)" } else { "" }
    );
}

/// Display-list cache counters (stderr; with `FERRITE_SIM_DL`).
fn print_dl_stats() {
    use core::sync::atomic::Ordering::Relaxed;
    use ferrite_core::dl;
    if dl::enabled() {
        eprintln!(
            "sim: dl hits {} direct {} stored {} evicted {} bytes {} paints {}",
            dl::HITS.load(Relaxed),
            dl::DIRECT.load(Relaxed),
            dl::STORES.load(Relaxed),
            dl::EVICTS.load(Relaxed),
            dl::BYTES.load(Relaxed),
            dl::PAINTS.load(Relaxed)
        );
    }
}
