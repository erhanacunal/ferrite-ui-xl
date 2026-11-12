"""Sim golden frames: pixel regression check for the examples.

    python scripts/golden.py            # check against golden/hashes.json
    python scripts/golden.py --update   # record the current frames as golden
    python scripts/golden.py --only tablet_clock,scroll
    python scripts/golden.py --dl       # same check, widgets drawn through cached display lists
                                        # (every cache hit re-recorded and compared: FERRITE_SIM_DL_VERIFY)

Each scenario in golden/scenarios.json builds its example with ferrite-ui's
compiler (FERRITE_UI, default ../ferrite-ui), runs the release sim on its
virtual clock (`--dump`: same frames on every machine) with scripted taps and
swipes, and hashes the dumped frames. Frames are kept in target/golden/
<scenario>/ (failed ones also as a diff mask, *_diff.png when Pillow is
installed) for a look. Exit status 1 when a frame differs from the golden one.

Examples that need git-ignored files (tablet: private/wifi.cfg + WiFi
firmware) are skipped with a warning when those are missing.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FERRITE_UI = Path(os.environ.get("FERRITE_UI", ROOT.parent / "ferrite-ui"))
SCENARIOS = ROOT / "golden" / "scenarios.json"
HASHES = ROOT / "golden" / "hashes.json"
OUT = ROOT / "target" / "golden"
SIM = ROOT / "target" / "release" / ("sim.exe" if os.name == "nt" else "sim")


def build_sim():
    subprocess.run(["cargo", "build", "-p", "bsp-sim", "--release", "--quiet"], cwd=ROOT, check=True)


def build_image(example, cache):
    if example in cache:
        return cache[example]
    project = ROOT / "examples" / "exynos4412" / example / "project.json"
    img = OUT / "images" / f"{example}.img"
    img.parent.mkdir(parents=True, exist_ok=True)
    r = subprocess.run([sys.executable, str(FERRITE_UI / "tools" / "ferrite_build.py"), str(project), "-o", str(img)],
                       capture_output=True, text=True)
    if r.returncode != 0:
        msg = (r.stderr or r.stdout).strip().splitlines()[-1:] or ["?"]
        cache[example] = None, msg[0]
    else:
        cache[example] = img, None
    return cache[example]


def run(name, sc, img, env=None):
    out = OUT / name
    out.mkdir(parents=True, exist_ok=True)
    for old in out.glob("frame_*"):
        old.unlink()
    args = [str(SIM), "--fsimage", str(img), "--dump", str(out),
            "--dump-at", ",".join(str(t) for t in sc["dump"])]
    for x, y, ms in sc.get("taps", []):
        args += ["--tap", f"{x},{y},{ms}"]
    for sw in sc.get("swipes", []):
        args += ["--swipe", ",".join(str(v) for v in sw)]
    r = subprocess.run(args, capture_output=True, text=True, timeout=300, env=env)
    if r.returncode != 0:
        raise RuntimeError(f"sim exited {r.returncode}: {r.stderr.strip()[-300:]}")
    for line in r.stderr.splitlines():
        if line.startswith(("sim: dl ", "sim: scene ")) or (line.startswith("sim: snapshot ") and not line.startswith("sim: snapshot reads 0 ")):
            print(f"     {name}: {line[5:]}")
    frames = {}
    for t in sc["dump"]:
        f = out / f"frame_{t:06d}.bmp"
        frames[str(t)] = hashlib.sha256(f.read_bytes()).hexdigest()
    return frames


def diff_mask(name, t):
    """Write *_diff.png (changed pixels in red) next to a failed frame, if a
    golden copy is around (target/golden/<name>.golden/)."""
    try:
        from PIL import Image, ImageChops
    except ImportError:
        return
    gold = OUT / f"{name}.golden" / f"frame_{t:06d}.bmp"
    cur = OUT / name / f"frame_{t:06d}.bmp"
    if not gold.exists():
        return
    a, b = Image.open(gold).convert("RGB"), Image.open(cur).convert("RGB")
    d = ImageChops.difference(a, b).convert("L").point(lambda v: 255 if v else 0)
    mask = Image.new("RGB", a.size, (255, 0, 0))
    Image.composite(mask, b, d).save(OUT / name / f"frame_{t:06d}_diff.png")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--update", action="store_true", help="record the current frames as golden")
    ap.add_argument("--only", help="comma-separated scenario names")
    ap.add_argument("--dl", action="store_true",
                    help="draw widgets through cached display lists, verifying every hit (FERRITE_SIM_DL=1, "
                         "FERRITE_SIM_DL_VERIFY=1); must match the same golden frames")
    ap.add_argument("--stale", action="store_true",
                    help="answer reads of render-owned values (animated props, scroll offsets) from the last "
                         "render's snapshot, as the render core will (FERRITE_SIM_STALE_READS=1); frames that "
                         "differ show programs depending on reading them live")
    a = ap.parse_args()

    scenarios = json.loads(SCENARIOS.read_text(encoding="utf-8"))["scenarios"]
    if a.only:
        want = set(a.only.split(","))
        unknown = want - scenarios.keys()
        if unknown:
            sys.exit(f"unknown scenario(s): {', '.join(sorted(unknown))}")
        scenarios = {k: v for k, v in scenarios.items() if k in want}
    golden = json.loads(HASHES.read_text(encoding="utf-8")) if HASHES.exists() else {}

    build_sim()
    cache, failed, skipped = {}, [], []
    for name, sc in scenarios.items():
        img, err = build_image(sc["example"], cache)
        if img is None:
            print(f"SKIP {name}: {err}")
            skipped.append(name)
            continue
        env = dict(os.environ)
        # Every incremental scene update is checked against a full capture (scene.rs).
        env.update(FERRITE_SIM_SCENE_VERIFY="1")
        if a.dl:
            env.update(FERRITE_SIM_DL="1", FERRITE_SIM_DL_VERIFY="1")
        if a.stale:
            env.update(FERRITE_SIM_STALE_READS="1")
        frames = run(name, sc, img, env)
        if a.update:
            golden[name] = frames
            gdir = OUT / f"{name}.golden"
            gdir.mkdir(parents=True, exist_ok=True)
            for f in (OUT / name).glob("frame_*.bmp"):
                (gdir / f.name).write_bytes(f.read_bytes())
            print(f"REC  {name}: {len(frames)} frames")
            continue
        want = golden.get(name)
        if want is None:
            print(f"NEW  {name}: no golden frames (run with --update)")
            failed.append(name)
            continue
        bad = [t for t, h in frames.items() if want.get(t) != h]
        if bad:
            for t in bad:
                diff_mask(name, int(t))
            print(f"FAIL {name}: frames {', '.join(bad)} differ ({OUT / name})")
            failed.append(name)
        else:
            print(f"ok   {name}")

    if a.update:
        HASHES.write_text(json.dumps(golden, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"wrote {HASHES.relative_to(ROOT)}")
    if skipped:
        print(f"skipped: {', '.join(skipped)}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
