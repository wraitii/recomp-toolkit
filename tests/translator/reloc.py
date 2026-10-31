#!/usr/bin/env python3
"""Exercise synthetic relocatable templates through translation and runtime binding."""

import ctypes as C
import json
import platform
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

import run as fixture


def template(name, code, entries, masked=(), imports=()):
    return {"id": name, "code": code.hex(), "entries": entries, "masked": list(masked),
            "imports": list(imports), "sources": []}


def main():
    parent = fixture.KIT / "build/translator-tests"
    parent.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="reloc-", dir=parent))
    imported_source = root / "import_target.s"
    imported_source.write_text(".globl import_target\n.type import_target, @function\n"
                               "import_target: add eax, 13\nret\n.size import_target, .-import_target\n")
    cases = fixture.fixture([imported_source], root)
    exe = root / "fixture.exe"
    fixture.pe_image(cases, exe)
    maps = root / "map"
    fixture.write_map(cases, exe, maps)
    game = fixture.game_dir(root, exe, maps, fixture.DEFAULT_TRANSLATE)
    gen, _ = fixture.translate(game, root)

    local = bytes.fromhex("e8 01 00 00 00 c3 83 c0 07 c3 11 22 33 44")
    imported = bytes.fromhex("85 c9 74 05 e8 00 00 00 00 83 c0 09 c3")
    target = cases[0]["entries"][0]
    manifest = {"format": "recomp-reloc-manifest-v1", "source": {}, "templates": [
        template("local", local, [0, 6]),
        template("import", imported, [0], range(5, 9), [{"slot": 5, "target": "%08x" % target}]),
        template("masked", local, [0], [0]),
        template("indirect", bytes.fromhex("ff e0"), [0]),
    ]}
    manifest_path = root / "manifest.json"
    manifest_path.write_text(json.dumps(manifest))
    source = root / "translate-input.json"
    settings = json.loads(source.read_text())
    settings["reloc"] = str(manifest_path)
    source.write_text(json.dumps(settings))
    report = root / "reloc-report.json"
    binary = fixture.build_py.rust_translator(fixture.KIT / "build")
    subprocess.run([str(binary), "--input", str(source), "--out", str(gen), "--report", str(report),
                    "--reloc-only"], cwd=fixture.KIT, check=True)
    rows = {row["id"]: row for row in json.loads(report.read_text())["rows"]}
    assert {name: row["status"] for name, row in rows.items()} == {
        "local": "published", "import": "published", "masked": "refused", "indirect": "refused"}, rows
    assert rows["masked"]["reason"] == "masked-slot-overlap", rows["masked"]
    assert rows["indirect"]["reason"] == "indirect-jump", rows["indirect"]

    harness = root / ("harness.dylib" if platform.system() == "Darwin" else "harness.so")
    subprocess.run([sys.executable, str(fixture.KIT / "tools/build.py"), "--translator-harness",
                    "--translator-gen", str(gen), "--harness-out", str(harness)], check=True)
    lib = C.CDLL(str(harness))
    lib.harness_mem.restype = C.c_void_p
    memory = lib.harness_mem()
    lib.reloc_test_call.argtypes = [C.c_uint32, C.c_uint32, C.c_uint32, C.c_int, C.POINTER(C.c_uint32)]
    lib.reloc_test_call.restype = C.c_int

    def write(addr, data):
        C.memmove(memory + addr, data, len(data))

    def call(addr, eax=10, ecx=0, cached=False, match=True, result=None):
        out = (C.c_uint32 * 2)()
        lib.harness_reset_flags()
        matched = lib.reloc_test_call(addr, eax, ecx, cached, out)
        assert matched == match, (hex(addr), cached, matched)
        assert lib.harness_last_unknown() == 0, hex(lib.harness_last_unknown())
        if result is not None:
            assert out[0] == result and out[1] == 0x0efff004, (hex(addr), list(out))

    for base in (0x02000000, 0x02100000):
        write(base, local)
        call(base, result=17)
        call(base, cached=True, result=17)
        call(base + 6, result=17)
        write(base + 10, b"\xaa\xbb\xcc\xdd")
        call(base, cached=True, result=17)
        write(base + 6, b"\x90")
        call(base, cached=True, match=False)
        call(base, match=False)
        write(base + 6, local[6:7])
        call(base, result=17)

        other = base + 0x1000
        patched = bytearray(imported)
        struct.pack_into("<I", patched, 5, (target - other - 9) & 0xffffffff)
        write(other, bytes(patched))
        call(other, result=19)
        call(other, cached=True, result=19)
        call(other, ecx=1, cached=True, result=32)
        write(other + 5, b"\x00\x00\x00\x00")
        call(other, cached=True, match=False)
        call(other, match=False)
        write(other + 5, bytes(patched[5:9]))
        call(other, result=19)
    print("relocation integration: PASS (%s)" % root)


if __name__ == "__main__":
    main()
