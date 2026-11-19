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

1. **HTTP:** `POST /api/apps` (body = the package, `X-CRC32` optional) →
   install, JSON `{"id","name","version","entry","icon","caps","replaced"}`
   or `{"code","error"}`. `GET /api/apps` →
   `{"storage":bool,"apps":[...]}`, `DELETE /api/apps/{id}`; phase 3 adds
   `POST /api/apps/{id}/run`. The body goes to a heap buffer (up to the
   region size, 64 MB), not into the region; CPU0 installs it between
   passes (`net::install_app`), the running program goes on.
2. **writefs (protocol bridge / USART):** a new header flag
   `FS_FLAG_APP` = 4. With it the device takes the sectors into a staging
   buffer instead of the region, and `FsWriteComplete` calls
   `apps::install`, answers its error code or the last ACK, and starts the
   system image's `main` again (`hot_swap`: a writefs stops the running
   program when it starts). `FS_FLAG_RUN` = 8 (phase 3) will run the app
   instead. Without the flag writefs is today's system-image upload. Old
   firmware ignores unknown flags, so the device echoes the ones it knows
   in its FsCaps reply and `ferrite_cli` stops after the header when
   `FS_FLAG_APP` is missing (that firmware then waits for an image: restart
   it).
3. **CLI:** `ferrite_cli app install PKG` (HTTP with `--host`, else
   writefs with `FS_FLAG_APP` over `-p`; a system image is refused before
   sending), `app list`, `app remove ID` (`--host`). Phase 3 adds
   `install --run`, `app run ID`, `app default ID|--none`. `ferrite_build`
   prints whether it built an app (`App: com.acme.clock 1.2.3`) or a system
   image.

## Running an app (step 1: one at a time)

`apps::run(id)`:

1. Read `apps/{id}/app.img` into a heap buffer, mount + verify.
2. `render::wait_idle`, `flash::overlay(FS_BASE, image)` (this boot only;
   `FLASH.BIN` stays as it is), then the `hot_swap` sequence: children
   dropped, fonts/images reset (their epochs), `P::fs_replaced`, objects
   cleared, the entry program loaded into `main` and started.
3. `ctx.app = Some(AppInfo { id, .. })`: the storage root becomes
   `apps/{id}/data` and the EEPROM `apps/{id}/data/eeprom.bin` (saved,
   then swapped, like `ferrite/eeprom.bin` today); `/api/status` reports
   it.

Boot: `settings.txt` `app=` names an installed app → `apps::run` after the
mount; a missing / bad one logs, clears nothing, and boots the system image.
Volume-up still opens setup. `runProgram` / `spawnProgram` inside an app
find programs in that app's image, as today (the app is the whole FS).

New `.fl` syscalls (`lib/device.fl`, device caps `apps`): `appCount()`,
`appId(i)` / `appName(i)` (strings), `runApp(id)` (like `runProgram`:
nothing of the caller runs after it), `currentApp()`. That is enough for a
launcher written in `.fl` (step 2 makes it a task that stays alive).

## Step 2 and 3 constraints (decided now, built later)

- **Several apps at once** (launcher task + app tasks) need one `Fs` per
  task: `ctx.fs` becomes the running task's (`Tasks` keeps each VM's), and
  each loaded app gets its own range of the RAM region (a range allocator
  over it; `FlashImpl` addresses stay absolute, `Fs::mount(flash, base)`
  already takes a base).
- **Font and image ids** are u8 per image, so two apps both have a font 1.
  Widgets and the renderer keep global ids; the VM translates at its
  boundary with a per-task table (local id → global slot in
  `FontList`/`ImageList`, slots freed when the task ends): `W_SET`/`W_GET`
  of `PROP_FONT_ID`/`PROP_IMAGE_ID`, the draw builtins, rich text's font
  references and V3 font variants. 17 call sites in `vm.rs` to audit.
  Global slots stay u8 (255 fonts, `MAX_IMAGES` images across all running
  apps).
- **syscfg** is the system's for everything (already so in step 1).
- **Libraries** reuse the package format with a `RES_LIBINFO` manifest and
  `ferrite/libs/{id}/lib.img`; their globals and deepest frame chain add to
  the importer's `var_slots` (header v5).

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
3. **Run:** `apps::run`, boot `app=`, per-app storage root, the syscalls,
   sim support (`--storage DIR` holds `ferrite/apps/`). Golden: unchanged
   (examples stay system images); one new scenario that installs and runs
   an app in the sim.
4. Notes (`xl.md`), CLAUDE.md keep-in-sync entries (`RES_APPINFO`,
   `appinfo` layout, `FS_FLAG_APP`/`RUN`, `APP_ERR_*`, app syscall ids,
   `settings.txt` `app=`).

## Decisions (2026-11-19)

1. `FLASH.BIN` stays: the system image keeps its role (setup, programs
   without an `application_id`), so moving to apps is gradual.
2. EEPROM is per app (`apps/{id}/data/eeprom.bin`, same 4 KB and API): no
   app reads another's data (storage root and EEPROM both inside `data/`).
   The system image keeps `ferrite/eeprom.bin`.
3. Trust: the CRC only, no signing.
4. No rollback copy: an install replaces the previous version.
