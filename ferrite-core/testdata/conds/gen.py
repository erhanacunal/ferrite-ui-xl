"""Regenerate the condition test programs (runtime::cond_tests).

    python ferrite-core/testdata/conds/gen.py

Compiles conds.fl with ferrite-ui's compiler, with and without
superinstructions (with them, `&&` / `||` / `!` conditions become
branches). Run it again after changing it or the compiler's condition
codegen. The compiler is found in FERRITE_UI (default: ../ferrite-ui next
to this repo).
"""

import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[2].parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))

import ferrite_lang  # noqa: E402

src_path = HERE / "conds.fl"
for superops, name in ((False, "conds.prg"), (True, "conds_super.prg")):
    image = ferrite_lang.build_image(src_path.read_text(encoding="utf-8"), filename=str(src_path),
                                     include_dirs=[str(FERRITE_UI / "lib"), str(HERE)],
                                     superops=superops, var_slots=superops)
    (HERE / name).write_bytes(bytes(image))
    print(f"{name}: {len(image)} bytes")
