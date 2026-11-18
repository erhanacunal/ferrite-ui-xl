"""Regenerate the call test programs (runtime::call_tests).

    python ferrite-core/testdata/calls/gen.py

Compiles calls.fl with ferrite-ui's compiler, with and without
superinstructions, and checks that it has void functions to test. Run it
again after changing it or the compiler's calling convention. The compiler
is found in FERRITE_UI (default: ../ferrite-ui next to this repo).
"""

import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[2].parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))

import ferrite_lang  # noqa: E402

voids = []
_orig = ferrite_lang._void_functions


def _spy(program):
    v = _orig(program)
    voids.append(v)
    return v


ferrite_lang._void_functions = _spy

src_path = HERE / "calls.fl"
for superops, name in ((False, "calls.prg"), (True, "calls_super.prg")):
    image = ferrite_lang.build_image(src_path.read_text(encoding="utf-8"), filename=str(src_path),
                                     include_dirs=[str(FERRITE_UI / "lib"), str(HERE)],
                                     superops=superops)
    (HERE / name).write_bytes(bytes(image))
    print(f"{name}: {len(image)} bytes")

expected = {"bump", "bump_twice", "early", "rec"}
assert all(v == expected for v in voids), f"void functions: {voids}, expected {expected}"
