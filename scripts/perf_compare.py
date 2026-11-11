"""Side-by-side summary of perf_baseline.py runs.

    python scripts/perf_compare.py ferrite-ui baseline dl

Per example and tag: the median over the recorded windows of passes/s,
longest pass, render and widget-draw time (ms per second), presents per
second and VM instructions per second.
"""

import re
import statistics
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def windows(text):
    for w in text.split("profiler clock")[1:]:
        win = float(re.search(r"window ([\d.]+) ms", w).group(1)) / 1000
        m = re.search(r"passes \d+ \(([\d.]+) (k?)passes/s, avg [\d.]+ us, max (\d+) us\)", w)
        vm = re.search(r"VM ([\d.]+) (k|M)instr/s", w)
        ms = lambda name: float(re.search(rf"^\s*{name}\s+([\d.]+) ms", w, re.M).group(1)) / win
        pres = re.search(r"presents: (\d+)", w)
        yield {
            "passes/s": float(m.group(1)) * (1000 if m.group(2) else 1),
            "max us": int(m.group(3)),
            "render ms/s": ms("render"),
            "draws ms/s": ms("widget draws"),
            "vm ms/s": ms("vm"),
            "presents/s": int(pres.group(1)) / win if pres else 0,
            "VM Mi/s": float(vm.group(1)) / (1000 if vm.group(2) == "k" else 1),
        }


def main():
    tags = sys.argv[1:] or ["ferrite-ui", "baseline", "dl"]
    exs = sorted({p.stem for t in tags for p in (ROOT / "perf" / t).glob("*.txt")})
    cols = ["passes/s", "max us", "render ms/s", "draws ms/s", "vm ms/s", "presents/s", "VM Mi/s"]
    print(f"{'example':10} {'tag':11}" + "".join(f"{c:>13}" for c in cols))
    for ex in exs:
        for t in tags:
            f = ROOT / "perf" / t / f"{ex}.txt"
            if not f.exists():
                continue
            ws = list(windows(f.read_text(encoding="utf-8")))
            if not ws:
                print(f"{ex:10} {t:11} (no reports)")
                continue
            row = [statistics.median(w[c] for w in ws) for c in cols]
            print(f"{ex:10} {t:11}" + "".join(f"{v:13.1f}" for v in row) + f"   n={len(ws)}")
        print()


if __name__ == "__main__":
    main()
