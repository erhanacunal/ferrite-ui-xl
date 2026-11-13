//! Simulator BSP — host-side desktop simulator.
//!
//! Builds the application context against the host (minifb) backends and hands
//! it to the shared `ferrite_core::run` loop. The window event loop is pumped from
//! `SimPlatform::frame_end` once per iteration.
//!
//! Build:  cargo build -p bsp-sim --release
//! Run:    cargo run -p bsp-sim -- --fsimage examples/widget_demo/fs.bin

use std::env;
use std::fs;

use minifb::{Window, WindowOptions};

use ferrite_core::ctx::Ctx;
use ferrite_core::font::FontList;
use ferrite_core::fs::{self as ferrite_fs, Fs};
use ferrite_core::image::ImageList;
use ferrite_core::pool::ObjPool;

// --- Concrete host backends (type aliases + free-fn constructors) ---
mod audio;
mod backlight;
mod eeprom;
mod storage;
mod flash;
mod gpio;
mod jpeg;
mod lcd;
mod rtc;
mod systick;
mod touch;
mod usart;

mod platform;
mod prof;

use lcd::sim::new_framebuffer;
use lcd::{HEIGHT, WIDTH};
use platform::SimPlatform;
use touch::sim::MouseState;
use touch::CalParams;

fn main() {
    let (fsimage_path, taps, eeprom_path, audio_out) = parse_args();
    let dump = platform::Dump::from_args();
    // `FERRITE_SIM_DL=1`: widget drawing goes through cached display lists;
    // `FERRITE_SIM_DL_VERIFY=1` also re-records every cache hit and panics on a stale list.
    if std::env::var_os("FERRITE_SIM_DL_VERIFY").is_some() {
        ferrite_core::dl::VERIFY.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    if std::env::var_os("FERRITE_SIM_DL").is_some() {
        ferrite_core::dl::ENABLED.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    // Render-owned reads (`snapshot.rs`) are always counted here (printed at the end of a
    // dump run); `FERRITE_SIM_STALE_READS=1` answers them from the last render's snapshot.
    ferrite_core::snapshot::TRACK.store(true, core::sync::atomic::Ordering::Relaxed);
    if std::env::var_os("FERRITE_SIM_STALE_READS").is_some() {
        ferrite_core::snapshot::STALE.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    // `FERRITE_SIM_SCENE_VERIFY=1`: every incremental update of the render pool's scene is
    // compared with a full capture (`scene.rs`), panicking on a difference;
    // `FERRITE_SIM_SCENE_FULL=1`: a full capture every sync, as before.
    if std::env::var_os("FERRITE_SIM_SCENE_VERIFY").is_some() {
        ferrite_core::scene::VERIFY.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    if std::env::var_os("FERRITE_SIM_SCENE_FULL").is_some() {
        ferrite_core::scene::INCREMENTAL.store(false, core::sync::atomic::Ordering::Relaxed);
    }

    systick::init();
    if let Some(d) = &dump {
        systick::sim::set_virtual();
        if let Err(e) = fs::create_dir_all(&d.dir) {
            eprintln!("--dump {}: {e}", d.dir.display());
            std::process::exit(1);
        }
    }

    let flash = match load_flash(fsimage_path.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("failed to load fsimage: {e}");
            std::process::exit(1);
        }
    };

    // Report what the filesystem contains (informational; `run` mounts it again).
    match Fs::mount(&flash, ferrite_fs::DEFAULT_FS_BASE) {
        Ok(f) => {
            let progs = f.count_by_kind(&flash, ferrite_fs::RES_PROGRAM);
            let fonts = f.count_by_kind(&flash, ferrite_fs::RES_FONT);
            let imgs = f.count_by_kind(&flash, ferrite_fs::RES_IMAGE);
            println!("fs mounted: programs={progs} fonts={fonts} images={imgs}");
        }
        Err(_) => println!("fs mount failed; no program will be loaded"),
    }

    let fb = new_framebuffer();
    let mouse = MouseState::new();
    let mut touch = touch::new(mouse.clone());
    touch.cal = identity_cal();

    let window = match Window::new(
        "ferrite-ui sim",
        WIDTH as usize,
        HEIGHT as usize,
        WindowOptions::default(),
    ) {
        Ok(mut w) => {
            // A dump run is not paced (virtual clock).
            w.set_target_fps(if dump.is_some() { 0 } else { 60 });
            w
        }
        Err(e) => {
            eprintln!("window open failed: {e}");
            std::process::exit(1);
        }
    };

    // The render side's own context (plan step 3c): same framebuffer and
    // flash, everything else idle. `FERRITE_SIM_ONE_CTX=1`: none (the
    // renderer swaps pools inside the VM's context instead).
    let render_ctx = if std::env::var("FERRITE_SIM_ONE_CTX").is_ok_and(|v| v == "1") {
        None
    } else {
        Some(Box::new(Ctx::<SimPlatform> {
            lcd: lcd::new(fb.clone()),
            flash: flash.clone(),
            fonts: FontList::new(),
            images: ImageList::new(),
            objs: ferrite_core::pool::ObjPool::view(),
            view: ferrite_core::pool::ObjPool::view(),
            render_ctx: None,
            render_away: false,
            fs: None,
            backlight: backlight::new(),
            rtc: rtc::init(),
            usart: usart::init(),
            systick: systick::Systick::handle(),
            audio: ferrite_core::audio::AudioImpl::with_backend(audio::SimAudio::new(None)),
            jpeg: ferrite_core::jpeg::JpegImpl::with_backend(jpeg::SoftJpeg),
            gpio: ferrite_core::gpio::GpioImpl::with_backend(gpio::SimGpio::default()),
            eeprom: ferrite_core::eeprom::EepromImpl::with_backend(eeprom::SimEeprom::new(None)),
            storage: ferrite_core::storage::StorageImpl::with_backend(storage::SimStorage::new(None)),
            cursor_visible: false,
            tasks: ferrite_core::task::TaskTable::new(),
        }))
    };
    let ctx = Box::new(Ctx::<SimPlatform> {
        lcd: lcd::new(fb.clone()),
        flash,
        fonts: FontList::new(),
        images: ImageList::new(),
        objs: ObjPool::new(),
        view: ferrite_core::pool::ObjPool::view(),
        render_ctx,
        render_away: false,
        fs: None,
        backlight: backlight::new(),
        rtc: rtc::init(),
        usart: usart::init(),
        systick: systick::Systick::handle(),
        audio: ferrite_core::audio::AudioImpl::with_backend(audio::SimAudio::new(audio_out)),
        jpeg: ferrite_core::jpeg::JpegImpl::with_backend(jpeg::SoftJpeg),
        gpio: ferrite_core::gpio::GpioImpl::with_backend(gpio::SimGpio::default()),
        eeprom: ferrite_core::eeprom::EepromImpl::with_backend(eeprom::SimEeprom::new(eeprom_path)),
        storage: ferrite_core::storage::StorageImpl::with_backend(storage::SimStorage::new(arg_value("--storage"))),
        cursor_visible: false,
        tasks: ferrite_core::task::TaskTable::new(),
    });

    // `FERRITE_SIM_PLAY=1`: the render side only plays the display list of each frame, which
    // the VM's side records (`render::MODE_PLAY`); `FERRITE_SIM_PLAIN=1`: its baseline, the
    // same plain drawing rendered the normal way (`MODE_PLAIN`).
    // `FERRITE_SIM_AREAS=1`: as play, recorded in compositor order (`render::MODE_AREAS`).
    let mut ctx = ctx;
    if std::env::var_os("FERRITE_SIM_AREAS").is_some() {
        ctx.objs.set_render_mode(ferrite_core::render::MODE_AREAS);
    } else if std::env::var_os("FERRITE_SIM_PLAY").is_some() {
        ctx.objs.set_render_mode(ferrite_core::render::MODE_PLAY);
    } else if std::env::var_os("FERRITE_SIM_PLAIN").is_some() {
        ctx.objs.set_render_mode(ferrite_core::render::MODE_PLAIN);
    }
    // `FERRITE_SIM_OP_STATS=1`: count the played frames' calls by opcode (printed at exit).
    if std::env::var_os("FERRITE_SIM_OP_STATS").is_some() {
        ferrite_core::dl::OP_STATS.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    platform::install_host(window, fb.clone(), mouse.clone(), taps, dump);

    #[cfg(all(windows, target_arch = "x86_64"))]
    prof::start_from_args();

    ferrite_core::runtime::run::<SimPlatform>(ctx, touch)
}

/// The value after `name` on the command line.
fn arg_value(name: &str) -> Option<String> {
    let mut args = env::args().skip(1);
    args.by_ref().find(|a| a == name)?;
    args.next()
}

/// `--storage <dir>` (read by `arg_value`): programs' files there, else in RAM.
/// `--fsimage <path>`, any number of `--tap X,Y,MS` / `--swipe X0,Y0,X1,Y1,MS[,DUR]`
/// / `--pinch CX,CY,R0,R1,MS[,DUR[,DEG]]` (scripted presses; Ctrl + drag
/// adds a second finger mirrored round the screen's centre),
/// `--eeprom <path>` (file that keeps the virtual EEPROM across runs) and
/// `--audio-out <path.wav>` (records the audio output).
fn parse_args() -> (Option<String>, Vec<platform::ScriptedTap>, Option<String>, Option<String>) {
    let mut fsimage = None;
    let mut eeprom = None;
    let mut audio_out = None;
    let mut taps = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fsimage" => fsimage = args.next(),
            "--eeprom" => eeprom = args.next(),
            "--audio-out" => audio_out = args.next(),
            "--tap" => {
                let spec = args.next().unwrap_or_default();
                let parts: Vec<u32> = spec.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                match parts[..] {
                    [x, y, at_ms] => taps.push(platform::ScriptedTap {
                        x: x as u16,
                        y: y as u16,
                        x1: x as u16,
                        y1: y as u16,
                        at_ms,
                        dur_ms: platform::TAP_HOLD_MS,
                        pinch: None,
                    }),
                    _ => eprintln!("ignoring --tap {spec:?}: expected X,Y,MS"),
                }
            }
            "--swipe" => {
                let spec = args.next().unwrap_or_default();
                let parts: Vec<u32> = spec.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                let (x, y, x1, y1, at_ms, dur_ms) = match parts[..] {
                    [x, y, x1, y1, at] => (x, y, x1, y1, at, 250),
                    [x, y, x1, y1, at, dur] => (x, y, x1, y1, at, dur),
                    _ => {
                        eprintln!("ignoring --swipe {spec:?}: expected X0,Y0,X1,Y1,MS[,DUR]");
                        continue;
                    }
                };
                taps.push(platform::ScriptedTap {
                    x: x as u16,
                    y: y as u16,
                    x1: x1 as u16,
                    y1: y1 as u16,
                    at_ms,
                    dur_ms,
                    pinch: None,
                });
            }
            "--pinch" => {
                let spec = args.next().unwrap_or_default();
                let parts: Vec<f32> = spec.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                let (cx, cy, r0, r1, at, dur, deg) = match parts[..] {
                    [cx, cy, r0, r1, at] => (cx, cy, r0, r1, at, 400.0, 0.0),
                    [cx, cy, r0, r1, at, dur] => (cx, cy, r0, r1, at, dur, 0.0),
                    [cx, cy, r0, r1, at, dur, deg] => (cx, cy, r0, r1, at, dur, deg),
                    _ => {
                        eprintln!("ignoring --pinch {spec:?}: expected CX,CY,R0,R1,MS[,DUR[,DEG]]");
                        continue;
                    }
                };
                taps.push(platform::ScriptedTap {
                    x: 0,
                    y: 0,
                    x1: 0,
                    y1: 0,
                    at_ms: at as u32,
                    dur_ms: dur as u32,
                    pinch: Some(platform::Pinch { cx, cy, r0, r1, deg }),
                });
            }
            _ => {}
        }
    }
    (fsimage, taps, eeprom, audio_out)
}

fn load_flash(path: Option<&str>) -> Result<flash::Flash, String> {
    match path {
        Some(p) => {
            let mut bytes = fs::read(p).map_err(|e| format!("{p}: {e}"))?;
            let raw_len = bytes.len();

            // An fs-only image (starts with "FERR" at offset 0) is padded with
            // 0x1000 reserved bytes so the header lands at FS_BASE.
            if bytes.len() >= 4 && &bytes[..4] == b"FERR" {
                let mut padded = std::vec![0xFFu8; ferrite_fs::DEFAULT_FS_BASE as usize];
                padded.extend_from_slice(&bytes);
                bytes = padded;
                println!(
                    "loaded {raw_len} bytes from {p} (fs-only image — padded with {}B prefix)",
                    ferrite_fs::DEFAULT_FS_BASE
                );
            } else {
                println!("loaded {raw_len} bytes from {p}");
            }
            Ok(flash::from_image(bytes))
        }
        None => {
            println!("no --fsimage specified; starting with blank 32MB flash");
            Ok(flash::new())
        }
    }
}

fn identity_cal() -> CalParams {
    CalParams {
        xy_swap: false,
        x_flip: false,
        y_flip: true,
        x_min: 0,
        x_max: WIDTH - 1,
        y_min: 0,
        y_max: HEIGHT - 1,
    }
}
