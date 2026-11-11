"""Device perf baseline (plan step 1c): per-phase main-loop profile of the
examples on the Exynos 4412 board.

    python scripts/perf_baseline.py [--host exynos] [--only bench,tablet] [--count 5] [--tag NAME]

Needs the XL firmware on the board (`.\\exynos4412.ps1 http`) and WiFi.
For each example: builds its image with ferrite-ui's compiler, uploads and
boots it (`ferrite_cli.py bootflash`), lets it settle, then records
`ferrite_cli.py perf` reports (time per phase, passes/s, VM instr/s) to
perf/<tag>/<example>.txt. Compare two tags with any diff tool; the default
tag is "baseline".
"""

import argparse
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FERRITE_UI = Path(os.environ.get("FERRITE_UI", ROOT.parent / "ferrite-ui"))
CLI = FERRITE_UI / "tools" / "ferrite_cli.py"
BUILD = FERRITE_UI / "tools" / "ferrite_build.py"
EXAMPLES = ["bench", "tablet", "scroll", "paint", "layers", "multitask", "launcher", "model3d"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default=os.environ.get("EXYNOS_HOST", "exynos"))
    ap.add_argument("--only", help="comma-separated examples")
    ap.add_argument("--count", type=int, default=5, help="perf reports per example (2 s each)")
    ap.add_argument("--settle", type=float, default=5.0, help="seconds after boot before measuring")
    ap.add_argument("--tag", default="baseline")
    a = ap.parse_args()

    examples = a.only.split(",") if a.only else EXAMPLES
    out = ROOT / "perf" / a.tag
    out.mkdir(parents=True, exist_ok=True)
    img_dir = ROOT / "target" / "perf"
    img_dir.mkdir(parents=True, exist_ok=True)
    for ex in examples:
        img = img_dir / f"{ex}.img"
        r = subprocess.run([sys.executable, str(BUILD), str(ROOT / "examples" / "exynos4412" / ex / "project.json"),
                            "-o", str(img)], capture_output=True, text=True)
        if r.returncode != 0:
            print(f"SKIP {ex}: build failed: {(r.stderr or r.stdout).strip().splitlines()[-1:]}")
            continue
        print(f"{ex}: upload + boot")
        subprocess.run([sys.executable, str(CLI), "--host", a.host, "bootflash", str(img), "--wait", "30"], check=True)
        time.sleep(a.settle)
        r = subprocess.run([sys.executable, str(CLI), "--host", a.host, "perf", "--interval", "2",
                            "--count", str(a.count)], capture_output=True, text=True, timeout=60 + 3 * a.count)
        (out / f"{ex}.txt").write_text(r.stdout + r.stderr, encoding="utf-8")
        print(f"{ex}: {out / (ex + '.txt')}")


if __name__ == "__main__":
    main()
