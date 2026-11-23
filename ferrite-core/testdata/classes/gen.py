"""Regenerate the class test program (runtime::class_tests).

    python ferrite-core/testdata/classes/gen.py

Compiles classes.fl and inherit.fl with ferrite-ui's compiler, with and
without superinstructions. Run it again after changing them or the compiler's
class support. The compiler is found in FERRITE_UI (default: ../ferrite-ui
next to this repo).
"""

import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[2].parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))

from ferrite_lang import build_image  # noqa: E402

for src in ("classes", "inherit"):
    src_path = HERE / f"{src}.fl"
    for superops, name in ((False, f"{src}.prg"), (True, f"{src}_super.prg")):
        image = build_image(src_path.read_text(encoding="utf-8"), filename=str(src_path),
                            include_dirs=[str(FERRITE_UI / "lib"), str(HERE)], superops=superops,
                            var_slots=superops, wide=True)
        (HERE / name).write_bytes(bytes(image))
        print(f"{name}: {len(image)} bytes")
