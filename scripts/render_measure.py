"""Render hand-off measurement on the board (exynos, render jobs on CPU2).

    python scripts/render_measure.py --host 192.168.88.120 --tag NAME [--scenarios home,monitor-stress]
                                     [--windows 3] [--secs 6]

Runs the tablet example (the board's saved FLASH.BIN must be it) through each
scenario and measures `--windows` windows of `--secs` seconds each:

    home            the system image restarted, home screen idle (the clock ticks)
    monitor-stress  Monitor opened, "Start stress test" tapped (tiles bouncing)

Per window: CPU0's render phase and its `scene capture` / `scene handoff`
sub-phases (`GET /api/perf`), the scene syncs (`/api/status` `scene`,
cumulative), the render jobs (`rcore`, zeroed by each status read: busy,
apply, longest, pickup), presents and passes per second. Written to
perf/<tag>/render-<scenario>.json (a list, one object per window) and printed.
"""
import argparse
import json
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

MONITOR_ICON = (940, 468)
STRESS_BUTTON = (956, 696)


def get(host, path, timeout=10):
    with urllib.request.urlopen(f"http://{host}{path}", timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8", "replace"))


def post(host, path):
    req = urllib.request.Request(f"http://{host}{path}", data=b"", method="POST")
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read().decode("utf-8", "replace")


def tap(host, xy):
    """A press and a lift at one point (a swipe that doesn't move)."""
    x, y = xy
    post(host, f"/api/touch?swipe={x},{y},{x},{y},60")


def setup(host, scenario):
    post(host, "/api/apps/run")  # the system image, from scratch
    time.sleep(5)
    if scenario == "monitor-stress":
        tap(host, MONITOR_ICON)
        time.sleep(3)
        tap(host, STRESS_BUTTON)
        time.sleep(3)
    elif scenario != "home":
        raise SystemExit(f"unknown scenario {scenario}")


def window(host, secs):
    s0 = get(host, "/api/status")  # also zeroes `rcore`
    t0 = time.time()
    perf = get(host, f"/api/perf?ms={int(secs * 1000)}", timeout=secs + 10)
    s1 = get(host, "/api/status")
    dt = time.time() - t0
    ph = {p["name"]: p["us"] for p in perf["phases"]}
    win_s = perf["window_us"] / 1e6
    sc0, sc1 = s0["scene"], s1["scene"]
    full = sc1["full"] - sc0["full"]
    updates = sc1["updates"] - sc0["updates"]
    syncs = full + updates
    rc = s1["rcore"]
    jobs = rc["jobs"]
    per = lambda v, n: round(v / n, 1) if n else 0.0
    return {
        "syncs": syncs,
        "full": full,
        "repacked_per_update": per(sc1["repacked"] - sc0["repacked"], updates),
        "capture_us_per_sync": per(ph.get("scene capture", 0) * secs / win_s, syncs),
        "handoff_us_per_sync": per(ph.get("scene handoff", 0) * secs / win_s, syncs),
        "render_phase_ms_per_s": round(ph["render"] / 1000 / win_s, 3),
        "vm_phase_ms_per_s": round(ph["vm"] / 1000 / win_s, 3),
        "passes_s": round(perf["passes"] / win_s, 1),
        "max_pass_us": perf["max_pass_us"],
        "jobs": jobs,
        "job_us": per(rc["busy_us"], jobs),
        "apply_us_per_job": per(rc["apply_us"], jobs),
        "render_us_per_job": per(rc["render_us"], jobs),
        "max_us": rc["max_us"],
        "pickup_us": per(rc["pickup_us"], jobs),
        "presents_s": round((s1["frames"] - s0["frames"]) / dt, 1),
        "gpu_error": s1["gplay"]["gpu_error"] - s0["gplay"]["gpu_error"],
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", required=True)
    ap.add_argument("--tag", required=True)
    ap.add_argument("--scenarios", default="home,monitor-stress")
    ap.add_argument("--windows", type=int, default=3)
    ap.add_argument("--secs", type=float, default=6.0)
    a = ap.parse_args()
    out = ROOT / "perf" / a.tag
    out.mkdir(parents=True, exist_ok=True)
    for sc in a.scenarios.split(","):
        setup(a.host, sc)
        rows = [window(a.host, a.secs) for _ in range(a.windows)]
        (out / f"render-{sc}.json").write_text(json.dumps(rows, indent=1) + "\n", encoding="utf-8")
        print(f"== {sc}")
        for k in rows[0]:
            print(f"  {k:22} " + "  ".join(f"{r[k]:>9}" for r in rows))
    post(a.host, "/api/apps/run")


if __name__ == "__main__":
    main()
