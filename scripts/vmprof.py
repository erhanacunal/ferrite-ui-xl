"""Dynamic VM opcode profile (ferrite-core `vm/prof.rs`): which opcodes run,
how often, with the superinstructions' cond / op operands and the slot kinds
(local / global) of the variable operands.

    python scripts/vmprof.py --sim                    # every golden scenario, in the sim
    python scripts/vmprof.py --sim tablet_home,grid   # some of them
    python scripts/vmprof.py --host 192.168.88.120 --secs 10   # the board, live (whatever runs)
    python scripts/vmprof.py --file report.txt        # a saved report (GET /api/vmprof)

--sim runs each scenario of golden/scenarios.json like scripts/golden.py
(release sim, virtual clock, the same taps / swipes) with
FERRITE_SIM_VMPROF=1 and reads the report the sim writes when it ends.
--host zeroes and starts the board's counters (POST /api/vmprof), waits,
stops them and reads them; the VM runs through exec_one meanwhile (slower),
the counts are the same. Names come from ferrite-ui's ferrite_cc.OP_NAMES;
the quickened forms (marked *) from vm.rs QUICK. --top N limits the opcode
list; --each prints every scenario, not only the sum.
"""

import argparse
import collections
import os
import re
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FERRITE_UI = Path(os.environ.get("FERRITE_UI", ROOT.parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))
sys.path.insert(0, str(ROOT / "scripts"))

from ferrite_cc import OP_NAMES  # noqa: E402

SUB_NAMES = {0x04: 'ADD', 0x05: 'SUB', 0x06: 'MUL', 0x07: 'DIV', 0x08: 'MOD', 0x0A: 'AND',
             0x0B: 'OR', 0x1E: 'SHL', 0x1F: 'SHR', 0x0D: 'EQ', 0x0E: 'NE', 0x0F: 'LT',
             0x10: 'LE', 0x11: 'GT', 0x12: 'GE'}


class Profile:
    def __init__(self):
        self.total = 0
        self.ops = collections.Counter()
        self.subs = collections.Counter()      # (op, sub)
        self.slots = collections.Counter()     # (op, 'L' | 'G')

    def add(self, other):
        self.total += other.total
        self.ops.update(other.ops)
        self.subs.update(other.subs)
        self.slots.update(other.slots)

    @staticmethod
    def parse(text):
        p = Profile()
        for line in text.splitlines():
            f = line.split()
            if not f:
                continue
            if f[0] == 'total':
                p.total = int(f[1])
            elif f[0] == 'op':
                p.ops[int(f[1], 16)] = int(f[2])
            elif f[0] == 'sub':
                p.subs[(int(f[1], 16), int(f[2], 16))] = int(f[3])
            elif f[0] == 'slot':
                p.slots[(int(f[1], 16), f[2])] = int(f[3])
        return p


def quick_forms():
    """(op name, sub name) pairs of vm.rs QUICK."""
    src = (ROOT / "ferrite-core" / "src" / "vm.rs").read_text(encoding="utf-8")
    block = re.search(r"const QUICK: \[\(u8, u8, u8\); \d+\] = \[(.*?)\];", src, re.S).group(1)
    return set(re.findall(r"\(OP_(\w+), OP_(\w+), SLOT_\w+\)", block))


def name(op):
    return OP_NAMES.get(op, f'0x{op:02X}')


def show(title, p, top):
    quick = quick_forms()
    t = max(p.total, 1)
    print(f'== {title}: {p.total} instructions')
    print('-- opcodes')
    for op, n in p.ops.most_common(top):
        print(f'  {name(op):14} {n:12} {100 * n / t:6.2f}%')
    if p.subs:
        print('-- superinstructions by cond / op (* = quickened form)')
        covered = 0
        for (op, sub), n in p.subs.most_common():
            star = '*' if (name(op), SUB_NAMES.get(sub, '?')) in quick else ' '
            covered += n if star == '*' else 0
            print(f'  {star} {name(op):10} {SUB_NAMES.get(sub, hex(sub)):4} {n:12} {100 * n / t:6.2f}%')
        print(f'  quickened: {100 * covered / max(sum(p.subs.values()), 1):.1f}% of these')
    if p.slots:
        print('-- slot operands (L local / G global)')
        ops = sorted({op for op, _ in p.slots}, key=lambda op: -(p.slots[(op, 'L')] + p.slots[(op, 'G')]))
        for op in ops:
            l, g = p.slots[(op, 'L')], p.slots[(op, 'G')]
            print(f'  {name(op):14} L {l:12}  G {g:12}  ({100 * l / max(l + g, 1):.0f}% local)')
    print()


def sim_profiles(only):
    import golden
    import json
    scenarios = json.loads(golden.SCENARIOS.read_text(encoding="utf-8"))["scenarios"]
    names = only.split(',') if only else list(scenarios)
    golden.build_sim()
    cache = {}
    env = dict(os.environ, FERRITE_SIM_VMPROF="1")
    out = {}
    for n in names:
        sc = scenarios[n]
        img, err = golden.build_image(sc["example"], cache)
        if img is None:
            print(f'skip {n}: {err}', file=sys.stderr)
            continue
        dump = golden.OUT / "vmprof" / n
        dump.mkdir(parents=True, exist_ok=True)
        args = [str(golden.SIM), "--fsimage", str(img), "--dump", str(dump),
                "--dump-at", ",".join(str(t) for t in sc["dump"])]
        for x, y, ms in sc.get("taps", []):
            args += ["--tap", f"{x},{y},{ms}"]
        for sw in sc.get("swipes", []):
            args += ["--swipe", ",".join(str(v) for v in sw)]
        r = subprocess.run(args, capture_output=True, text=True, timeout=600, env=env)
        m = re.search(r"sim: vmprof\n(.*?)sim: vmprof end", r.stderr, re.S)
        if r.returncode != 0 or not m:
            print(f'skip {n}: sim exited {r.returncode}, no profile', file=sys.stderr)
            continue
        out[n] = Profile.parse(m.group(1))
    return out


def board_profile(host, secs):
    def post(q):
        req = urllib.request.Request(f"http://{host}/api/vmprof?{q}", data=b"", method="POST")
        urllib.request.urlopen(req, timeout=5).read()
    post("reset=1&on=1")
    time.sleep(secs)
    post("on=0")
    with urllib.request.urlopen(f"http://{host}/api/vmprof", timeout=10) as r:
        return Profile.parse(r.read().decode())


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sim", nargs="?", const="", help="golden scenarios in the sim (comma-separated; none = all)")
    ap.add_argument("--host", help="the board's address")
    ap.add_argument("--secs", type=float, default=10, help="--host: seconds to count")
    ap.add_argument("--file", help="a saved report")
    ap.add_argument("--top", type=int, default=30)
    ap.add_argument("--each", action="store_true", help="--sim: every scenario too")
    a = ap.parse_args()
    if a.sim is not None:
        profiles = sim_profiles(a.sim)
        total = Profile()
        for n, p in profiles.items():
            total.add(p)
            if a.each:
                show(n, p, a.top)
        show(f'sum of {len(profiles)} scenarios', total, a.top)
    elif a.host:
        show(f'board {a.host}, {a.secs:g} s', board_profile(a.host, a.secs), a.top)
    elif a.file:
        show(a.file, Profile.parse(Path(a.file).read_text(encoding="utf-8")), a.top)
    else:
        ap.error("--sim, --host or --file")


if __name__ == "__main__":
    main()
