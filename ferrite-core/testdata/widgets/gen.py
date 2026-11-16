"""Regenerate the widget constructor test program (runtime::widget_ctor_tests).

    python ferrite-core/testdata/widgets/gen.py

Compiles ctor.fl with ferrite-ui's compiler, with and without
superinstructions. Run it again after changing it or the compiler's widget
constructors (`widget` declarations). The compiler is found in FERRITE_UI
(default: ../ferrite-ui next to this repo).
"""

import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[2].parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))

from ferrite_lang import build_image  # noqa: E402

for src in ("ctor",):
    src_path = HERE / f"{src}.fl"
    for superops, name in ((False, f"{src}.prg"), (True, f"{src}_super.prg")):
        image = build_image(src_path.read_text(encoding="utf-8"), filename=str(src_path),
                            include_dirs=[str(FERRITE_UI / "lib"), str(HERE)], superops=superops)
        (HERE / name).write_bytes(bytes(image))
        print(f"{name}: {len(image)} bytes")
