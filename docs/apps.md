# Application packages (design, 2026-11-19)

Step 1 of: apps → `main` as an ordinary task (launcher + app tasks) → shared
libraries (`ferrite/libs/`). This document covers step 1 and the decisions
the later steps depend on.

Goal: a project with an `application_id` builds into a package; uploading it
(writefs or the HTTP API) installs it under `ferrite/apps/{id}/` on the
board's storage instead of replacing `FLASH.BIN`; the board runs installed
apps from there. Several apps installed side by side, one of them running.

## Where things are today

- Build: `ferrite_build.py project.json` → one FS image (`FERR` v3: header,
  32-byte resource table, data; CRC-32 over all of it). Programs, pages,
  fonts, images, sounds, files, syscfg are resources found by name (≤ 15
  chars); fonts and images also by a u8 id in their own header
  (`Font::load_by_id`, `Image::load_by_id`), which widgets store
  (`PROP_FONT_ID` 0x1C, `PROP_IMAGE_ID` 0x1F).
- Board: the image lives in `ferrite/flash.bin` (USERDATA ext4) and is
  copied at boot into a 64 MB RAM region (`bsp/exynos4412/src/flash.rs`),
  which `ctx.flash` + `ctx.fs` read. Images are drawn from that region by
  physical address (G2D/GPU), so resources must sit in RAM.
- Upload: `writefs` (protocol bridge) writes sectors into the region, then
  `FsWriteComplete` verifies it and `hot_swap`s (same syscfg) or restarts;
  `POST /flash` (HTTP) saves `FLASH.BIN` and restarts.
- The setup screen already runs an image *for this boot only*:
  `flash::overlay` puts `setup.img` into the region without marking it to
  be saved. Step 1 reuses that.
- Tasks: every program runs in its own `Vm` (`main` = task 0, children via
  `spawnProgram`, by resource name in the one `ctx.fs`); objects belong to
  their task (`ObjPool::owner`). Program storage (`disk*` opcodes) is rooted
  at one directory for every program (`FERRITE/USER`).

## Package

A package is an ordinary FS image (same format, same tools, same CRC) with
one extra resource:

- `RES_APPINFO` = 8, name `appinfo`: the manifest, written by
  `ferrite_build` when `project.json` has `application_id`. Binary,
  little-endian, versioned:

  | Offset | Size | Field |
  | --- | --- | --- |
  | 0 | 2 | magic `"AI"` |
  | 2 | 1 | version = 1 |
  | 3 | 1 | id length n (1..32) |
  | 4 | 32 | id, n bytes, zero padded |
  | 36 | 32 | display name (UTF-8, zero padded) |
  | 68 | 4 | app version (u32; `project.json` `"app_version"`, e.g. 1.2.3 → 0x010203; `"version"` is the project format's) |
  | 72 | 16 | entry program name (default `main`) |
  | 88 | 16 | icon image resource name (optional, empty = none) |
  | 104 | 4 | caps the app needs (the device caps bits it was built for) |

- `application_id`: `[a-z0-9][a-z0-9._-]{0,31}` (lower case: ext4 names on
  the board are lower case; no `/`). Reverse-DNS is allowed
  (`com.acme.clock`), not required. Unique per board.
- A project without `application_id` builds exactly as today (no
  `appinfo`): it is a *system image* and goes to `FLASH.BIN` as now.
- The app's `RES_SYSCFG` is ignored on the board: pool sizes stay the
  system image's (apps install without a restart). `ferrite_build` warns
  when an app's syscfg differs from the defaults.

## On the board

```
ferrite/
  flash.bin              system image (as today; boots when no app is chosen)
  apps/
    {id}/
      app.img            the package, as uploaded (verified)
      appinfo.bin        its manifest (108 bytes), so listing reads no app.img
      data/              the app's storage root (disk* opcodes) and eeprom.bin
  ferrite.bin, eeprom.bin, ...
system/settings.txt      + key `app={id}`: the app to start at boot
```

- Install = write `apps/{id}/app.img.new`, then rename over `app.img` (the
  USERDATA "NAME.new + rename" rule; power loss leaves the old one or the
  new one), then `appinfo.bin` the same way. `app.img`'s own manifest is
  what runs; `appinfo.bin` only lists it. `data/` is created once and kept across updates; removing an
  app deletes the whole directory.
- Apps need the ext4 USERDATA (the CACHE FAT32 fallback has 8.3 names);
  without it installing answers "no app storage".
- No unpacking into separate resource files for now: one file keeps the
  CRC check whole and a load is one read. (Separate files become worth it
  with libraries, which share resources; revisit in step 3.)

## Install paths

Every path ends in the same `apps::install` (`ferrite-core/src/apps.rs`):
mount the image as an `Fs` at `FS_BASE` (`apps::SliceFlash`), `verify`
(CRC), read `appinfo` (magic, version, id rules), check the caps against
the firmware's, check it has its entry program, then write as above
through `apps::AppFiles` (exynos4412: `storage::BoardApps`, the ext4
USERDATA only). Errors are `apps::err` codes, which are also protocol
error codes (ferrite_cli `ERROR_DESCRIPTIONS`):

| Code | Meaning | HTTP |
| --- | --- | --- |
| 11 | not a valid FS image (format or CRC) | 400 |
| 12 | no appinfo (a system image) | 400 |
| 13 | bad `application_id` | 400 |
| 14 | needs caps the firmware lacks | 400 |
| 15 | no app storage (USERDATA not ext4) | 503 |
| 16 | writing to storage failed | 500 |
| 17 | no such app | 404 |
| 18 | entry program not in the image | 400 |
| 19 | package too large | 413 |
| 20 | four apps already run | 409 |

1. **HTTP:** `POST /api/apps` (body = the package, `X-CRC32` optional) →
   install, JSON `{"id","name","version","entry","icon","caps","replaced"}`
   or `{"code","error"}`. `GET /api/apps` →
   `{"storage":bool,"running":ID,"boot":ID,"apps":[...]}` ("" = the system
   image), `DELETE /api/apps/{id}` (409 while it runs; clears it as the boot
   app), `POST /api/apps?run=1` (install, then run), `POST /api/apps/run?id=ID`
   and `POST /api/apps/boot?id=ID` (no `id`: the system image; 404 for an app
   not installed). `/api/status` has `"app"`. The body goes to a heap buffer (up to the
   region size, 64 MB), not into the region; CPU0 installs it between
   passes (`net::install_app`), the running program goes on.
2. **writefs (protocol bridge / USART):** a new header flag
   `FS_FLAG_APP` = 4. With it the device takes the sectors into a staging
   buffer instead of the region, and `FsWriteComplete` calls
   `apps::install`, answers its error code or the last ACK, and starts the
   system image's `main` again (`hot_swap`: a writefs stops the running
   program when it starts), or, with `FS_FLAG_RUN` = 8, the new app; an
   update of the app that was running runs it again. Without the flag writefs is today's system-image upload. Old
   firmware ignores unknown flags, so the device echoes the ones it knows
   in its FsCaps reply and `ferrite_cli` stops after the header when
   `FS_FLAG_APP` is missing (that firmware then waits for an image: restart
   it).
3. **CLI:** `ferrite_cli app install PKG` (HTTP with `--host`, else
   writefs with `FS_FLAG_APP` over `-p`; a system image is refused before
   sending; `--run` runs it), `app list` (marks `[running]`, `[boot]`),
   `app run [ID]`, `app boot [ID]`, `app remove ID` (`--host`). `ferrite_build`
   prints whether it built an app (`App: com.acme.clock 1.2.3`) or a system
   image.

## Running an app (step 1: one at a time)

A run request (`runApp`, `/api/apps/run`, an install with run) is a CPU0
static (`apps::request_run`); the runtime's next pass takes it
(`Runtime::switch_app`): every program is dropped, then `enter_app`:

1. `open_app` (since step 2a, see "Step 2"): `apps::load` reads
   `apps/{id}/app.img` (at most `Platform::APP_MAX_BYTES`) and checks it as
   an install does; the image goes into a free range of the region above the
   system image (`domain::rebase` moves its table's offsets, then
   `PlatformRuntime::place_image`), mounted there as a new domain with its
   EEPROM read. A failure leaves everything as it was (the running image
   starts again).
2. Every other app domain closes (EEPROM saved, range freed), `main`'s
   domain becomes the app's, `PlatformRuntime::app_changed` tells the board
   (`/api/status`), `apps::set_current`.
3. `hot_swap` starts the app's `entry` program (`main` for the system
   image). The system image stays at `FS_BASE` all along: nothing of an app
   is ever saved into `FLASH.BIN`.

A system-image writefs while an app runs leaves the app first (`FsReady`),
so the upload lands in the system image. Boot: `settings.txt` `app=` (set
with `/api/apps/boot`) names the app to enter after the system image's
syscfg sized the pools; a missing or bad one boots the system image.
Volume-up (setup) skips it. `runProgram` / `spawnProgram` inside an app
find programs in that app's image (the app is the whole FS).

`.fl` syscalls (handled in the core VM before `syscall_fn`, ferrite-ui
`lib/apps.fl`, keep in sync with `apps::SYS_APP_*`):

| Id | Call | Result |
| --- | --- | --- |
| 0x80 | `appCount()` | installed apps (0 without app storage) |
| 0x81 | `appId(i)` | string, sorted by id; "" past the end |
| 0x82 | `appName(i)` | string |
| 0x83 | `runApp(id)` | like `runProgram`: nothing of the caller runs after it; 0 if not installed. `""` = the system image |
| 0x84 | `currentApp()` | string, "" for the system image |

The list is cached until an install or removal (`apps::list_cached`).

Sim (`bsp/sim/src/apps.rs`): with `--storage DIR` apps live in
`DIR/ferrite/apps/`, an app's files and EEPROM in its `data/`; else in RAM.
`--install PKG` (repeatable) installs at start, `--app ID` boots one. The
system image's flash part and EEPROM are kept aside while an app runs.
Example: `examples/exynos4412/apps` (a launcher system image, one app source
built as two apps). A complete game as a package: `examples/exynos4412/apps/freecell`
(`make_assets.py` draws the cards and fonts first; started from the tablet's
app drawer).

## Step 2: several apps at once (design, 2026-11-19)

User decisions: one app on screen at a time, full screen; hidden apps keep
running (`loop()`, timers, messages); back to the launcher with a system
gesture or from code; at most 4 apps at once (the launcher, i.e. the system
image, not counted).

**Domains.** A domain is one loaded image: domain 0 the system image (at
`FS_BASE`, as today, never moved), domains 1..=4 apps. `ctx.domains`
(`ferrite-core/src/domain.rs`) holds per app domain: its `AppInfo`, its
range of the RAM region, its `Fs`, its font / image id maps, its EEPROM
buffer, its container widget and its first task. Every VM carries its
`domain` (a task spawned by `spawnProgram` inherits its parent's).

- **Region:** an app's image goes into a free range of the region above the
  system image (first fit over 4 slots, 64 KB aligned), through
  `PlatformRuntime::place_image(ctx, addr, image)` (exynos4412: copied into
  the shadow, not a change to save: `FLASH.BIN` keeps `[..USED]`, the
  system image only; sim: the flash backend). No overlay of the system
  image any more: step 1's `flash::set_app_image` / `APP_ACTIVE` go away.
  A system-image writefs ends every app first (it may grow over them).
- **Fs per domain:** the VM's `ctx.fs` uses (program load, `runProgram`,
  `programName`, `spawnProgram`, sounds, `fileOpen`) take the VM's domain.
  `F_READ` / `F_WRITE` addresses in an app are moved by its range
  (`addr - FS_BASE + base`), bounded to it: an app sees its image where it
  was built for and reaches nothing else; its writes last until it ends.
- **Font and image ids:** widgets and the renderer keep global `u8` ids.
  Domain 0 maps ids to themselves (golden unchanged). An app domain maps its
  ids to free global ids (from 255 down, skipping the system image's,
  found by scanning its resources once) when it opens: `W_SET` / `W_GET`
  of `PROP_FONT_ID` / `PROP_IMAGE_ID` (the widget page goes through
  `w_set` too) and the draw builtins translate; a font's bold / italic
  variant ids are translated when it loads. Closing a domain unloads its
  fonts and images (`FontList` / `ImageList` remove; the render copy
  follows through its epoch).
- **EEPROM:** domain 0 is the board's EEPROM, as today. An app domain's is a
  4 KB buffer in the core, read from `apps/{id}/data/eeprom.bin` through
  `AppFiles` when it opens, saved once writes settle, when it closes and
  before a restart (`apps::flush`). Step 1's `eeprom::switch` goes away.
- **Files:** `StorageBackend::set_root(Option<&str>)` before an operation of
  another domain than the last (exynos4412: `ext4::set_user_root`, with the
  roots already made remembered; sim: `HostStorage`). Open files keep their
  inode, so tasks of different domains interleave.
- **Screen:** an app domain gets a full-screen container widget (opaque
  background, owned by the app's first task) under the root; its `parent(0)`
  means that container, so an app is built exactly as a system image is.
  `showApp` makes one container visible and last among the root's children
  and hides the others; the launcher's own widgets show when none is
  visible. Hidden containers are neither drawn nor hit. The shown app's
  first task is the foreground task (`ctx.tasks.fg`, global input).
  The container is clickable (no `on_click`), so touches on it stay in the
  app and never reach the launcher below. `startApp` reserves a task slot
  (domain `PENDING`) and the runtime opens the domain after the burst
  (`Runtime::service_apps`); a failure there ends the slot with code -1.
- **Home gesture:** a pointer that goes down in the bottom 24 px while an app
  is shown and is released 80 px higher shows the launcher (the touch also
  reaches the app as usual; `FullInput::home_from`).
- **End:** the app ends when its first task ends (`exitApp()`, a
  `killTask` of it, a fault): its other tasks end, then its domain closes
  (fonts / images unloaded, EEPROM saved, range freed). The launcher (the
  starting task) gets `on_task_exit(task, code)`. A `runProgram` in the
  first task keeps the app (its container is made again). `main` switching
  program, a system writefs or `runApp` end every started app.

`.fl` (`lib/apps.fl`, syscalls 0x85..):

| Call | Result |
| --- | --- |
| `startApp(id)` | the app's first task id, started if not running, and shows it; -1 not installed, -2 four apps already run, -3 no room |
| `showApp(task)` | shows the app that task belongs to (0 = the launcher); 0 if not an app |
| `appTask(id)` | its first task id if it runs, else -1 |
| `exitApp()` | ends the calling app (every task of its domain) |

`runApp(id)` keeps step 1's meaning (everything ends, the app runs as
`main`, now as domain 1 with the system image left in place); the boot app
(`app=`) too: a kiosk with no launcher behind it. `currentApp()` is the
calling task's app.

Phases:

- **2a** (done 2026-11-19; board: run, Back, boot app, a restart and a
  system writefs while an app runs, app EEPROMs carried over from step 1):
  domains with one app at a time: `domain.rs`, `place_image`, Fs /
  EEPROM / files per domain, `F_READ` / `F_WRITE` ranges; `runApp` and the
  boot app rebuilt on them (step 1's overlay removed). Board: step 1's tests
  again.
- **2b** (done 2026-11-19): font / image id maps (`IdMaps`, `Domains::assign_ids`;
  `FontList` / `ImageList` `find_or_load_mapped`, `remove_ids`). Check: the
  tablet built as an app renders pixel-identical to the tablet as the system
  image (sim, also after opening Clock); board: it runs as an app.
- **2c** (done 2026-11-19): `startApp` / `showApp` / `appTask` / `exitApp`
  (VM `app_call`), containers (`ObjPool::raise`), the home gesture, the
  4-app limit, `/api/apps` `started` (beside `running`, the app run as
  `main`), the launcher example on `startApp`. Sim: start, Home, a second
  app, the gesture, show again (the hidden app kept counting), Exit;
  pixel-identical with `FERRITE_SIM_DL_VERIFY` / `SCENE_VERIFY` / `AREAS`.
  Board: the same, and four apps at once (heap 94 MB); a system writefs
  ends them.
- **2d** (done 2026-11-19): notes (`docs/notes/xl.md`), CLAUDE.md keep-in-sync
  (app syscalls 0x80–0x88, `apps::err` 11–20).

## Step 3: libraries (design, 2026-11-19)

User decisions: shared code (a library is in RAM once, however many tasks
import it); a library may hold code, its own globals, make widgets and take
callbacks (`@fn`, `on_click`), but has no fonts or images of its own (it
uses its importer's ids); libraries are installed on their own
(`ferrite/libs/{id}/`) and an importer asks for an id and a major version.

**Writing one.** A library is a project with `"library_id"` (and `name`,
`app_version`) in its project.json and one program, `lib`. Its functions marked
`export fn` are what importers call; the rest are its own. It may have
globals (initialised by its `setup()`, which runs once per importing task,
before the task's own `setup()`), widget callbacks and `@fn` references to
its functions. It may not have `loop()` or the other lifecycle functions, a
widget page, `F_READ` / `F_WRITE`, or import libraries itself. Built, it is
a package like an app's: an FS image with the program and `RES_LIBINFO`
(the `appinfo` layout: id, name, version), installed with
`ferrite_cli lib install` (`/api/libs`).

**Using one.** A project lists the library packages it builds against in
`"libraries": ["../util/util.bin", ...]`; their exported functions are then
called by name like the program's own (a name clash is a compile error).
Only the libraries a program calls are recorded in its image: each with its
id and major version, and every imported function by name (with its
argument count and return kind), so a library can be rebuilt (same major)
without rebuilding its users.

**Image v6** (devices with `caps.libraries`): after `var_slots`,
`lib_count` u8, `import_count` u16, `export_count` u16, `call_depth` u8;
after the function table, the library table (`u8 len, id, u8 major` each),
the import table (`u8 lib, u8 nargs, u8 ret, u8 len, name`) and the export
table (`u16 func_id, u8 nargs, u8 ret, nargs × u8 param type, u8 len,
name`, libraries only), then
the code. Flags bit 4: a library. A library's `var_slots` is its globals
plus its deepest locals along one call chain from an export or callback,
`call_depth` its deepest call chain (frames).

**Opcodes.** `CALL_IMP u16 import` calls an imported function. `FN_REF u16
func_id` (libraries only, for `@fn`) pushes the id tagged with the running
unit (`unit << 12 | id`; ids stay below 4096), so a library's callbacks find
its own function. Programs keep pushing plain ids (unit 0).

**VM.** A task's VM runs units: unit 0 its program, 1.. its libraries
(`Vm::units`: code, function table, `gbase`). The code buffer is shared
(`VmCode` holds an `Arc`); switching units swaps the current `code` /
`functions` with the parked ones (no refcounting per call). Code addresses
saved across a switch (call frames' return address, callback frames, queued
callbacks) carry the unit in bits 24..31; `RET` to another unit switches
back (the threaded `RET` leaves that to `exec_one`). Globals: a library's
are placed after the program's (`gbase`), `resolve_slot` adds `gbase` to a
global slot (0 for the program); locals start after all of them. Sizes:
`var_slots` grows by each library's globals plus its locals per callback
level, the call stack by its `call_depth` per level. Callback dispatchers
(`on_click`, `on_tap`, `on_paint`) run in every unit that has one; the
lifecycle functions only in the program.

**Linking.** When a task starts a program with a library table
(`runtime::start_program`), each library is taken from the registry
(`libs.rs`: weak references to the loaded `LibImage`s, so one goes with its
last user; an install or remove empties it) or loaded from
`ferrite/libs/{id}/lib.img` (CRC-checked), its major checked, every import
resolved by name (argument count and return kind must match) into a link
table (`Vm::link`), then each library's `setup()` runs. Failures: the
program does not start, fault 19 (`library missing`) or 20 (`library
mismatch`), shown like any fault (`libs::load` answers `apps::err` 21 /
22, logged as such).

Phases:

- **3a (done 2026-11-19):** compiler and build (ferrite-ui): `export`,
  library projects, `"libraries"`, image v6, `CALL_IMP` / `FN_REF`;
  disassembler; tests.
- **3b (done 2026-11-19):** VM and linking (ferrite-core, `vm/units.rs`,
  `libs.rs`): units, tagged addresses, the registry, faults 19 / 20;
  `runtime::lib_tests` with compiled testdata (`testdata/libs/gen.py`).
- **3c (done 2026-11-19):** install (`apps::install_package` takes
  packages of both kinds: writefs `FS_FLAG_APP`, `POST /api/apps` or
  `/api/libs`, sim `--install`; `GET /api/libs` with the loaded ones,
  `DELETE /api/libs/{id}`; `ferrite_cli lib install|list|remove`).
- **3d (done 2026-11-19):** example `examples/exynos4412/apps/tapper` (a
  button counting its taps with its own `on_click`, used by `hello` and
  `world`), sim (two apps at once count apart), board, `perf_baseline.py`
  (`perf/pre3b` vs `perf/libs`: bench VM 20.2 → 20.0 Mi/s), notes,
  CLAUDE.md.

## Phases (step 1)

1. **Build** (done 2026-11-19): ferrite-ui `ferrite_build` writes `appinfo` from
   `project.json` (`application_id`, `name`, `version`, `entry`, `icon`);
   `RES_APPINFO` in `fs.rs` / `ferrite_build.py` (keep in sync);
   `ferrite_cli` and the Designer show it. Test: build an example as an app,
   parse it back.
2. **Install on the board** (done 2026-11-19, board: HTTP and writefs
   installs, update keeps `data/`, list, remove, device-side rejections,
   ext4 check clean; a power cut during an install is still to try):
   `ferrite-core/src/apps.rs` (`install`,
   `remove`, `list`, through `StorageBackend`, testable with `MemStorage`),
   HTTP endpoints, `FS_FLAG_APP` in `protocol.rs` + `ferrite_cli`.
   Tests: install / replace / remove / bad packages on `MemStorage`; board:
   upload two apps, list, power cut during an install (old or new, never
   half).
3. **Run** (done 2026-11-19, board: install --run, `runApp` both ways from
   `.fl`, per-app EEPROM counters, boot app across a restart, a restart
   while an app runs, a system writefs while an app runs, the boot CRC
   check of `FLASH.BIN` clean after each): see "Running an app". Golden
   unchanged; sim scenario with the `apps` example.
4. Notes (`xl.md`), CLAUDE.md keep-in-sync entries (done).

## Decisions (2026-11-19)

1. `FLASH.BIN` stays: the system image keeps its role (setup, programs
   without an `application_id`), so moving to apps is gradual.
2. EEPROM is per app (`apps/{id}/data/eeprom.bin`, same 4 KB and API): no
   app reads another's data (storage root and EEPROM both inside `data/`).
   The system image keeps `ferrite/eeprom.bin`.
3. Trust: the CRC only, no signing.
4. No rollback copy: an install replaces the previous version.
