#!/usr/bin/env python3
"""Fail when CPU flag state is read or written outside the files that settle it first."""

from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
DIRECTORIES = ("runtime", "dx", "graphics", "host", "mods", "games/civ4-bts/native")
SUFFIXES = {".c", ".cc", ".cpp", ".h", ".mm", ".m"}
ALLOWED = {
    "runtime/x86.h", "runtime/interp.cpp", "runtime/baseline.cpp", "runtime/seh.cpp", "runtime/imports.cpp",
    "runtime/cpu.cpp", "runtime/loader.cpp", "runtime/kernel32.cpp", "runtime/call_trace.cpp",
    "mods/hooks.cpp", "mods/native/replay.cpp",
}
PATTERN = re.compile(r"\beflags_(cf|zf|sf|of|pf|af|df)\b|\bcc_(op|a|b|res|carry|mask|size)\b"
                     r"|\bx86_cc_\w+|\bx86_set_eflags\b")


def main():
    """Print each flag-state access outside the allow-list and return 1 if there is any."""
    findings = 0
    for directory in DIRECTORIES:
        for path in sorted((ROOT / directory).rglob("*")):
            relative = path.relative_to(ROOT)
            if path.suffix not in SUFFIXES or not path.is_file() or relative.as_posix() in ALLOWED:
                continue
            if {"tests", "build"} & set(relative.parts):
                continue
            for number, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
                if PATTERN.search(line):
                    print(f"{relative.as_posix()}:{number}: flag state accessed outside the "
                          f"settle-first allow-list: {line.strip()}")
                    findings += 1
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
