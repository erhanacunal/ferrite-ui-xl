"""Regenerate the native-flex test programs (runtime::flex_tests).

    python ferrite-core/testdata/flex/gen.py

Compiles flex_test.fl with ferrite-ui's compiler twice: flexLayout() as the
lib/flex.fl function (flex_fl.prg) and as the FLEX_LAYOUT opcode
(flex_native.prg). Run it again after changing flex_test.fl or lib/flex.fl.
The compiler is found in FERRITE_UI (default: ../ferrite-ui next to this repo).
"""

import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[2].parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))

from ferrite_lang import build_image  # noqa: E402

src_path = HERE / "flex_test.fl"
source = src_path.read_text(encoding="utf-8")
for native, name in ((False, "flex_fl.prg"), (True, "flex_native.prg")):
    image = build_image(source, filename=str(src_path),
                        include_dirs=[str(FERRITE_UI / "lib")],
                        superops=True, native_flex=native)
    (HERE / name).write_bytes(bytes(image))
    print(f"{name}: {len(image)} bytes")
