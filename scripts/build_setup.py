#!/usr/bin/env python3
"""Builds the setup screen's program into system/setup/setup.img.

    python scripts/build_setup.py [--check]

The setup screen (system/setup/main.fl) is embedded in the exynos4412
firmware (bsp/exynos4412/src/setup.rs, include_bytes!), so setup.img is kept
in the repository: rebuild it after changing main.fl or ferrite-ui's
lib/system.fl, then rebuild the firmware. --check only tells whether the
committed image matches a fresh build (exit 1 if not).

Needs ferrite-ui next to this repository (or FERRITE_UI): its compiler
(tools/ferrite_build.py), font converter and fonts/Ubuntu-R.ttf (Ubuntu Font
Licence). The fonts go to system/setup/build/ (git-ignored).
"""

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SETUP = ROOT / "system" / "setup"
FERRITE_UI = Path(os.environ.get("FERRITE_UI", ROOT.parent / "ferrite-ui"))
RANGES = "32-126,0xA0-0x17F,0xB0,0xB7,0x2022,0x2026"
FONTS = [("f20", 20), ("f26", 26), ("f36", 36)]


def build(out: Path):
    conv = FERRITE_UI / "tools" / "ferrite_font_converter.py"
    ttf = FERRITE_UI / "fonts" / "Ubuntu-R.ttf"
    for tool in (conv, ttf, FERRITE_UI / "tools" / "ferrite_build.py"):
        if not tool.exists():
            sys.exit(f"build_setup: {tool} not found (set FERRITE_UI)")
    fonts = SETUP / "build"
    fonts.mkdir(exist_ok=True)
    for i, (name, size) in enumerate(FONTS):
        subprocess.run([sys.executable, str(conv), "resource", str(ttf), str(size), "--bpp", "2",
                        "--font-id", str(i + 1), "-r", RANGES, "-o", str(fonts / f"{name}.font")],
                       check=True, stdout=subprocess.DEVNULL)
    subprocess.run([sys.executable, str(FERRITE_UI / "tools" / "ferrite_build.py"),
                    str(SETUP / "project.json"), "-o", str(out)], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="compare with the committed image only")
    args = parser.parse_args()
    target = SETUP / "setup.img"
    if not args.check:
        build(target)
        print(f"build_setup: {target.relative_to(ROOT)}, {target.stat().st_size} bytes")
        return
    with tempfile.TemporaryDirectory() as tmp:
        fresh = Path(tmp) / "setup.img"
        build(fresh)
        same = target.exists() and target.read_bytes() == fresh.read_bytes()
    print("build_setup: setup.img is " + ("up to date" if same else "STALE: run scripts/build_setup.py"))
    sys.exit(0 if same else 1)


if __name__ == "__main__":
    main()
