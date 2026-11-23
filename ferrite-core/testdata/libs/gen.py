"""Regenerate the library test packages (runtime::lib_tests).

    python ferrite-core/testdata/libs/gen.py

Builds util.fl as library package com.test.util 1.2.0 (util.bin), the same
as 2.0.0 (util_v2.bin), util_bad.fl (no `bump`) as 1.3.0 (util_bad.bin),
and app.fl, built against util.bin, as a program image (app.prg). Run it
again after changing them or the compiler's library support. The compiler
is found in FERRITE_UI (default: ../ferrite-ui next to this repo).
"""

import json
import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
FERRITE_UI = Path(os.environ.get("FERRITE_UI", HERE.parents[2].parent / "ferrite-ui"))
sys.path.insert(0, str(FERRITE_UI / "tools"))

import ferrite_build  # noqa: E402
import ferrite_lang  # noqa: E402

# Manifest caps 0: the tests run on the mock platform, which has none.
_libinfo = ferrite_build.build_libinfo
ferrite_build.build_libinfo = lambda project, caps: _libinfo(project, None)


def library(source, version, out):
    with tempfile.TemporaryDirectory() as tmp:
        project = {"version": 2, "device": "exynos4412-xl", "screen": [1280, 800],
                   "include_dirs": [str(FERRITE_UI / "lib")],
                   "programs": [{"name": "lib", "source": str(HERE / source)}],
                   "library_id": "com.test.util", "name": "Util", "app_version": version}
        path = Path(tmp) / "project.json"
        path.write_text(json.dumps(project), encoding="utf-8")
        binary = ferrite_build.build(str(path), None)["binary"]
    (HERE / out).write_bytes(binary)
    print(f"{out}: {len(binary)} bytes")


library("util.fl", "1.2.0", "util.bin")
library("util.fl", "2.0.0", "util_v2.bin")
library("util_bad.fl", "1.3.0", "util_bad.bin")

src = HERE / "app.fl"
image = ferrite_lang.build_image(src.read_text(encoding="utf-8"), filename=str(src),
                                 include_dirs=[str(FERRITE_UI / "lib")],
                                 superops=True, var_slots=True, wide=True,
                                 libraries=[ferrite_build.read_library(HERE / "util.bin")])
(HERE / "app.prg").write_bytes(bytes(image))
print(f"app.prg: {len(image)} bytes")
