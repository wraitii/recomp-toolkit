"""Stack bytes each runtime import shim pops, read from the shim tables the runtime registers."""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SHIM = re.compile(r'\{\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*(\d+|ARGC_CDECL|ARGC_UNKNOWN)\s*,')


def import_pops(runtime=ROOT / "runtime"):
    seen = {}
    for path in sorted(runtime.glob("*.cpp")):
        for dll, name, argc in SHIM.findall(path.read_text(errors="replace")):
            seen.setdefault("%s!%s" % (dll.lower(), name), set()).add(argc)
    pops = {}
    for key, argcs in seen.items():
        if len(argcs) != 1 or "ARGC_UNKNOWN" in argcs:
            continue
        argc = argcs.pop()
        pops[key] = 0 if argc == "ARGC_CDECL" else 4 * int(argc)
    return dict(sorted(pops.items()))
