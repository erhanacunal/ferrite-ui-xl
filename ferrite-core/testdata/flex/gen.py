"""Regenerate the native-flex test programs (runtime::flex_tests).

    python ferrite-core/testdata/flex/gen.py

Compiles flex_test.fl with ferrite-ui's compiler twice: flexLayout() as the
lib/flex.fl function (flex_fl.prg) and as the FLEX_LAYOUT opcode
(flex_native.prg); and the FLEX_AUTO scenario (flex_auto_body.fl) as
flex_auto.prg / flex_manual.prg. Run it again after changing these sources
or lib/flex.fl.
The compiler is found in FERRITE_UI (default: ../ferrite-ui next to this repo).
"""

import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[2].parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))

from ferrite_lang import build_image  # noqa: E402

# (source, native_flex, output)
PROGRAMS = (
    ("flex_test.fl", False, "flex_fl.prg"),
    ("flex_test.fl", True, "flex_native.prg"),
    ("flex_auto.fl", True, "flex_auto.prg"),
    ("flex_manual.fl", True, "flex_manual.prg"),
)

for src, native, name in PROGRAMS:
    src_path = HERE / src
    image = build_image(src_path.read_text(encoding="utf-8"), filename=str(src_path),
                        include_dirs=[str(FERRITE_UI / "lib"), str(HERE)],
                        superops=True, native_flex=native)
    (HERE / name).write_bytes(bytes(image))
    print(f"{name}: {len(image)} bytes")
