# CLAUDE.md — ferrite-ui-xl

ferrite-ui for RAM-rich, multi-core devices (first target: Exynos 4412 tablet, 1 GiB, 4× Cortex-A9, Mali-400). Bare-metal Rust, a bytecode VM running `.fl` programs on a widget tree. Plan: `docs/plan.md`.

Working branch: `main` (commit and push there).

## Detailed notes (read the relevant part before changing a subsystem)

- `docs/notes/xl.md` — XL-specific subsystems, dated, with board measurements: display lists (`dl.rs`), GPU player / render modes, render pool / scene / commits, render context and render core (CPU2), JPEG, storage, gestures, interrupt-driven touch, G2D, graphs, RAM layout, toolchain.
- `docs/notes/inherited.md` — ferrite-ui architecture: widgets, dirty rendering, layers, GPU compositor, VM, object pool, FS, syscfg, profiler, exynos BSP (LCD/G2D/present, WiFi, HTTP API, standby, CPU clock), audio, EEPROM, fonts. Parts about Nextion/epaper/N32903/SD boot/buffered mode are obsolete here.
- `../ferrite-ui/docs/bytecode.md`, `../ferrite-ui/docs/ferrite-lang.md`.

## Repo layout and differences from ferrite-ui

- Targets: `bsp/exynos4412` and `bsp/sim` only. The sim is always 1280×800 with a readback framebuffer.
- `ferrite-core` = the whole device-agnostic framework, generic over `P: Platform`; each BSP owns backends + entry point.
- Always compiled (not features): `layers`, `multi_program`, `perf`, `fast_vm`, `superops`, `render_cache`, `wide_obj`, `wide_code`, `pool_large`, `audio`. Remaining features: `host`, `mock`, `external_alloc`, `jpeg`.
- Removed: `epaper`, `sdcard`, `sdboot`, the write-only AA underlay, the sim's `no-readback`/`tablet` features, buffered render mode (dirty rendering only, one `FLAG_DIRTY`).
- Compiler, `.fl` library and Designer live in `../ferrite-ui` (single source). Examples include `../ferrite-ui/lib`; `sim.bat` uses `%FERRITE_UI%`. XL-only bytecode is gated by device caps (`caps.*` in ferrite-ui `tools/devices/exynos4412*.json`); XL examples use the `exynos4412-xl` profile.

## Commands

- Tests: `cargo test -p ferrite-core --features host,mock`
- Pixel regression: `python scripts/golden.py` (`--dl`, `--areas`, `--play`, `--plain`, `--stale`; `--update` only after an intended visual change)
- Device perf: `python scripts/perf_baseline.py`
- Firmware: `cd bsp/exynos4412 && cargo build --release` (or `./exynos4412.ps1` from the repo root)
- After changing `lib/flex.fl`: `python ferrite-core/testdata/flex/gen.py`
- After changing the compiler's class support: `python ferrite-core/testdata/classes/gen.py`
- After changing `system/setup/main.fl` or ferrite-ui `lib/system.fl`: `python scripts/build_setup.py` (rebuilds the embedded `system/setup/setup.img`; `--check` compares), then the firmware
- Board: `192.168.1.55` (HTTP API `/api/status`, `/api/screen`, `/api/perf`; protocol bridge `tcp://HOST:7000`; `ferrite_cli --host H savefw` / `bootflash`).

## Rules and invariants

- **Pixel-identical:** golden hashes are shared by all modes (default, `--dl`, `--areas`, play/plain). A change that alters pixels in one mode only is a bug.
- **Display lists:** anything new that `render::draw_widget_body` reads must go into `render::dl_key`. `dl::VERIFY` panics on stale lists.
- **Scene:** every write to an object goes through `ObjPool::payload_mut` (so `scene::VERIFY` sees it); anything new the scene packs must be reached that way.
- **Render pool never allocates** (debug assert). Render-owned state lives in `ctx.view` / the render context, not in the widget blocks.
- Draw through `canvas::Canvas`, not the LCD directly (keyboard/error/boot screens excepted). Code that draws outside the renderer on CPU0 calls `render::wait_idle` first.
- **License:** don't link the GPL-2.0 `exynos_4412_jpeg` crate (or other GPL code) into the firmware.
- **Toolchain** is pinned in `rust-toolchain.toml`; changing it means re-running `perf_baseline.py` and `golden.py`.
- Don't build several BSPs in one cargo invocation (`--workspace`): feature unification breaks `Platform` impls.
- Use `lcd.width()/height()`, not `LcdBackend::WIDTH/HEIGHT`, for screen bounds.
- **Keep in sync** (duplicated numbers/layouts): opcodes (`vm.rs` ↔ ferrite-ui `ferrite_cc.py` ↔ `docs/bytecode.md`); widget kinds (`widget.rs` ↔ `ferrite_designer.py` ↔ `lib/core.fl`); `CAPS` bits ↔ `tools/devices/*.json` + `ferrite_devices.py` `_FALLBACK`; syscfg (`syscfg.rs` ↔ `ferrite_build.py` `SYSCFG_FIELDS`); perf phases / mem pools (`perf.rs`, `protocol.rs` ↔ `ferrite_cli.py` ↔ `net/http.rs`); VM fault names (`ferrite_cli.py` `VM_FAULT_NAMES`); `storage::err` ↔ `DISK_ERR_*`; syscall ids (BSP ↔ sim `platform.rs` ↔ `lib/*.fl`); protocol tags (`protocol.rs` ↔ `ferrite_cli.py`); firmware header (`net::install_firmware` ↔ uart_boot `cache.rs`); flex bit layout (`lib/flex.fl` ↔ docs); setup syscalls 0x70–0x7A and codes (`ferrite-core/src/setup.rs` ↔ ferrite-ui `lib/system.fl` ↔ sim `setup.rs`); `SYSTEM/` file names + `WIFI.TXT` format (`setup.rs`, `net/mod.rs` ↔ uart_boot `settings.rs`, `cache.rs`); `/system/NAME` upload (`net/http.rs` ↔ uart_boot `net.rs` ↔ `ferrite_cli.py system`); font resource V1/V2/V3 layout (`font.rs` `Header::read` ↔ ferrite-ui `ferrite_font_converter.py` `parse_header`/`write_resource` ↔ `ferrite_designer.py` `GfxFontPreview`); rich text markup (`rich.rs` ↔ ferrite-ui `tools/ferrite_rich.py` ↔ `docs/ferrite-lang.md` "Rich text"); `PROP_*` ids (`proto.rs` ↔ `ferrite_cc.py` `Prop`).

## Board pitfalls

- Boot keys: **volume up** held at start → ferrite's setup screen (WiFi settings → `SYSTEM/WIFI.TXT`; also shown when no program is saved); **volume down** → uart_boot stays in its loader (no autoboot). WiFi settings / chip firmware come only from `SYSTEM/` on CACHE (`WIFI.TXT`, `WIFIFW.BIN`, `WIFINV.TXT`), not from the FS image.
- uart_boot is installed with fastboot only (`fastboot flash boot uart_boot.img`; power + volume down held 2 s under ferrite restarts into fastboot). The one on the board (2026-11-15) autoboots even while polled.
- A program with an empty `fn loop() {}` keeps the runtime busy (no idle sleep).
- G2D/GIC: `Gic::enable` routes an SPI to the calling core; code enabling interrupts on CPU1–3 must set the target explicitly where CPU0 should take it.
- `.fl` pitfalls: a generated `on_click` dispatcher calls every handler registered for a widget; arrays can't be passed to functions; a global may not share a name with any function's param/local.

## Debug switches (sim env vars)

`FERRITE_SIM_DL=1`, `FERRITE_SIM_DL_VERIFY=1`, `FERRITE_SIM_AREAS=1`, `FERRITE_SIM_PLAY=1`, `FERRITE_SIM_PLAIN=1`, `FERRITE_SIM_SCENE_VERIFY=1`, `FERRITE_SIM_SCENE_FULL=1`, `FERRITE_SIM_ONE_CTX=1`, `FERRITE_SIM_STALE_READS=1`, `FERRITE_SIM_NO_MOVE=1`, `FERRITE_SIM_CLOCK=HH:MM`, `FERRITE_SIM_BATTERY=P,C`. Sim flags: `--dl-dump`, `--storage DIR`, `--eeprom FILE`, `--pinch …`, `--swipe …`, `--tap`, `--prof SECS`. exynos equivalents are under `/api/rcore`, `/api/dl`, `/api/compose`, `/api/snapshot` (see the notes).
