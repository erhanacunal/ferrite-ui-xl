"""Device perf baseline (plan step 1c): per-phase main-loop profile of the
examples on the Exynos 4412 board.

    python scripts/perf_baseline.py --fw FIRMWARE.bin [--host exynos] [--only bench,tablet] [--count 5] [--tag NAME]
    python scripts/perf_baseline.py --fw ../ferrite-ui/target/armv7a-none-eabihf/release/ferrite-ui.bin \
        --examples ../ferrite-ui/examples/exynos4412 --tag ferrite-ui

Needs the board on WiFi, running ferrite or waiting in uart_boot. For each
example: builds its image with ferrite-ui's compiler, gets the board into
uart_boot (`POST /restart` if ferrite runs; polling uart_boot's HTTP also
cancels its autoboot), saves the image as FLASH.BIN and runs `--fw` from RAM
(`ferrite_cli.py bootflash --app`: the saved firmware is left alone), waits
for ferrite's /api/status, lets it settle, then records
`ferrite_cli.py perf` reports (time per phase, passes/s, VM instr/s) to
perf/<tag>/<example>.txt. Compare two tags with any diff tool; the default
tag is "baseline". `--examples` takes the examples from another checkout (ferrite-ui's own,
built for its `exynos4412` device: the reference numbers on the ferrite-ui firmware).
"""

import argparse
import os
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FERRITE_UI = Path(os.environ.get("FERRITE_UI", ROOT.parent / "ferrite-ui"))
CLI = FERRITE_UI / "tools" / "ferrite_cli.py"
BUILD = FERRITE_UI / "tools" / "ferrite_build.py"
EXAMPLES = ["bench", "tablet", "scroll", "paint", "layers", "multitask", "launcher", "model3d"]


def get(host, path):
    """Body of GET path, or None (no answer / an HTTP error)."""
    try:
        with urllib.request.urlopen(f"http://{host}{path}", timeout=3) as r:
            return r.read().decode("utf-8", "replace")
    except OSError:
        return None


def in_uart_boot(host):
    body = get(host, "/")
    return body is not None and body.startswith("uart_boot")


def to_uart_boot(host, timeout=90):
    """Board waiting in uart_boot (restarting a running ferrite first)."""
    if in_uart_boot(host):
        return True
    try:
        urllib.request.urlopen(urllib.request.Request(f"http://{host}/restart", data=b"", method="POST"), timeout=5)
    except OSError:
        pass
    end = time.time() + timeout
    while time.time() < end:
        if in_uart_boot(host):  # this connection also stops the autoboot
            return True
        time.sleep(0.5)
    return False


def ferrite_up(host, timeout=90):
    """Until ferrite's /api/status answers."""
    end = time.time() + timeout
    while time.time() < end:
        body = get(host, "/api/status")
        if body and body.startswith("{"):
            return True
        time.sleep(1)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default=os.environ.get("EXYNOS_HOST", "exynos"))
    ap.add_argument("--only", help="comma-separated examples")
    ap.add_argument("--count", type=int, default=5, help="perf reports per example (2 s each)")
    ap.add_argument("--settle", type=float, default=5.0, help="seconds after boot before measuring")
    ap.add_argument("--tag", default="baseline")
    ap.add_argument("--post", action="append", default=[],
                    help="POST this path after each boot, e.g. /api/dl?on=1 (repeatable)")
    ap.add_argument("--fw", type=Path, required=True, help="firmware .bin run from RAM for each example")
    ap.add_argument("--examples", type=Path, default=ROOT / "examples" / "exynos4412",
                    help="directory holding the example projects")
    a = ap.parse_args()

    examples = a.only.split(",") if a.only else EXAMPLES
    out = ROOT / "perf" / a.tag
    out.mkdir(parents=True, exist_ok=True)
    img_dir = ROOT / "target" / "perf"
    img_dir.mkdir(parents=True, exist_ok=True)
    for ex in examples:
        img = img_dir / f"{ex}.img"
        r = subprocess.run([sys.executable, str(BUILD), str(a.examples.resolve() / ex / "project.json"),
                            "-o", str(img)], capture_output=True, text=True)
        if r.returncode != 0:
            print(f"SKIP {ex}: build failed: {(r.stderr or r.stdout).strip().splitlines()[-1:]}")
            continue
        print(f"{ex}: upload + boot")
        if not to_uart_boot(a.host):
            sys.exit("board did not reach uart_boot")
        subprocess.run([sys.executable, str(CLI), "--host", a.host, "bootflash", str(img), "--wait", "30",
                        "--app", str(a.fw.resolve())], check=True)
        if not ferrite_up(a.host):
            print(f"SKIP {ex}: ferrite did not come up")
            continue
        for path in a.post:
            urllib.request.urlopen(urllib.request.Request(f"http://{a.host}{path}", data=b"", method="POST"), timeout=5)
        time.sleep(a.settle)
        for _ in range(3):
            r = subprocess.run([sys.executable, str(CLI), "-p", f"tcp://{a.host}", "perf", "--interval", "2",
                                "--count", str(a.count)], capture_output=True, text=True, timeout=60 + 3 * a.count)
            if r.returncode == 0:
                break
            time.sleep(3)
        (out / f"{ex}.txt").write_text(r.stdout + r.stderr, encoding="utf-8")
        print(f"{ex}: {out / (ex + '.txt')}")


if __name__ == "__main__":
    main()
