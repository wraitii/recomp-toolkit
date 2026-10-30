#!/usr/bin/env python3
"""Assemble translator cases, translate them, and compare generated C with Unicorn.

Each cases/*.s file is one self-contained fixture. The runner prepends
TEXT_BASE, DATA_BASE, ESP_INIT, data_start and case_start definitions, assembles
with `clang -target i386-unknown-linux-gnu`, and rejects any relocation. `.text`
holds STT_FUNC spans; `.data` holds initialized tables and globals. Global
functions are run; local functions are translated as helpers. Per-case lines:

  # input: eax=0x1 arg0=5
  # table: site=label targets=label,label
  # method: site=label identity=dll!Interface::Method argc=N
  # translate: key = value
  # env: RK_NO_DIRECT=1
  # xfail: reason

`argN` writes the guest stack slot at [esp+4+4N]. Each input line is used for
one iteration before the remaining iterations use random input. A memory operand
cannot add two symbols, so give each data label an absolute alias:

  .set global_va, DATA_BASE + (global - data_start)

Then use [global_va]. Put jump tables in .data and list their indirect JMP site
with # table. A # method directive gives an indirect CALL site a COM method fact
(argc includes `this`); the group then translates with an analysis fact file.
Harness host entries abort, so the site must not run. A # translate directive merges one TOML value into the fixture's
[translate] table and repeats; # env sets one translator environment switch.
Cases with the same directives share an image, translation and harness; distinct
directives get separate groups. An # xfail case documents a known translator bug:
it must diverge, and starts failing the run once it passes.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib

KIT = Path(__file__).resolve().parents[2]
CASES = Path(__file__).resolve().parent / "cases"
IMAGE_BASE = 0x00400000
TEXT_RVA = 0x1000
DATA_RVA = 0x100000
CASE_STRIDE = 0x1000
ESP_INIT = 0x0EFFF000
DEFAULT_TRANSLATE = {"direct_variants": True,
                     "dispatch": {"shift": 3, "max_probe": 31}}

sys.path[:0] = [str(KIT / "tools"), str(KIT / "tools/recomp")]
import game_config  # noqa: E402
from diff import run as diff  # noqa: E402

spec = importlib.util.spec_from_file_location("build_py", KIT / "tools/build.py")
build_py = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build_py)


def u16(data, at):
    return struct.unpack_from("<H", data, at)[0]


def u32(data, at):
    return struct.unpack_from("<I", data, at)[0]


class Object:
    def __init__(self, path):
        data = Path(path).read_bytes()
        if data[:4] != b"\x7fELF" or data[4] != 1 or data[5] != 1:
            raise ValueError("%s: expected a little-endian ELF32 object" % path)
        shoff, shentsize, shnum, shstrndx = u32(data, 32), u16(data, 46), u16(data, 48), u16(data, 50)
        self.sections = []
        for i in range(shnum):
            at = shoff + i * shentsize
            self.sections.append({"name": u32(data, at), "type": u32(data, at + 4),
                                  "flags": u32(data, at + 8), "offset": u32(data, at + 16),
                                  "size": u32(data, at + 20), "link": u32(data, at + 24),
                                  "entsize": u32(data, at + 36)})
        names = self.sections[shstrndx]
        names_blob = data[names["offset"]:names["offset"] + names["size"]]
        for section in self.sections:
            end = names_blob.find(b"\0", section["name"])
            section["name"] = names_blob[section["name"]:end].decode()
        relocs = [s["name"] for s in self.sections if s["type"] in (4, 9) and s["size"]]
        if relocs:
            raise ValueError("%s: relocations are not allowed (%s)" % (path, ", ".join(relocs)))
        alloc = [s["name"] for s in self.sections
                 if s["flags"] & 2 and s["size"] and s["name"] not in (".text", ".data")]
        if alloc:
            raise ValueError("%s: only .text and .data may be allocated, found %s" % (path, ", ".join(alloc)))
        self.data = {s["name"]: data[s["offset"]:s["offset"] + s["size"]]
                     for s in self.sections if s["name"] in (".text", ".data")}
        self.data.setdefault(".text", b"")
        self.data.setdefault(".data", b"")
        self.indices = {s["name"]: i for i, s in enumerate(self.sections)}
        self.symbols = []
        symtab = next((s for s in self.sections if s["type"] == 2), None)
        if symtab is None:
            raise ValueError("%s: no symbol table" % path)
        strtab = self.sections[symtab["link"]]
        strings = data[strtab["offset"]:strtab["offset"] + strtab["size"]]
        for at in range(symtab["offset"], symtab["offset"] + symtab["size"], 16):
            name_at, value, size, info, _other, shndx = struct.unpack_from("<IIIBBH", data, at)
            end = strings.find(b"\0", name_at)
            name = strings[name_at:end].decode() if name_at else ""
            self.symbols.append({"name": name, "value": value, "size": size,
                                 "type": info & 0xf, "bind": info >> 4, "section": shndx})

    def at(self, name):
        index = self.indices[name]
        return [s for s in self.symbols if s["section"] == index]


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, list):
        return "[%s]" % ", ".join(toml_value(item) for item in value)
    if isinstance(value, dict):
        return "{%s}" % ", ".join("%s = %s" % (json.dumps(key), toml_value(item))
                                  for key, item in value.items())
    raise ValueError("unsupported TOML value %r" % (value,))


def case_config(text):
    translate, env = {}, {}
    for line in text.splitlines():
        found = re.match(r"\s*#\s*translate:\s*(.*)$", line)
        if found:
            parsed = tomllib.loads(found.group(1))
            for key, value in parsed.items():
                if key in translate:
                    raise ValueError("duplicate translate directive %r" % key)
                translate[key] = value
            continue
        found = re.match(r"\s*#\s*env:\s*([A-Za-z_][A-Za-z0-9_]*)(?:=(.*))?$", line)
        if found:
            name, value = found.group(1), found.group(2)
            if name in env:
                raise ValueError("duplicate env directive %r" % name)
            env[name] = "1" if value is None else value
    return translate, env


def case_directives(text):
    inputs, tables, methods = [], [], []
    for line in text.splitlines():
        found = re.match(r"\s*#\s*input:\s*(.*)$", line)
        if found:
            regs, args = {}, {}
            for field in found.group(1).replace(",", " ").split():
                name, _, value = field.partition("=")
                if not _:
                    raise ValueError("input directive needs NAME=VALUE: %s" % line)
                value = int(value, 0)
                upper = name.upper()
                if upper in diff.REG_NAMES:
                    regs[upper] = value
                elif upper == "EFLAGS":
                    args["eflags"] = value
                elif re.fullmatch(r"arg[0-9]+", name, re.I):
                    args.setdefault("args", {})[int(name[3:])] = value
                else:
                    raise ValueError("unknown input name %r" % name)
            inputs.append({"regs": regs, "args": args.get("args", {}), "eflags": args.get("eflags")})
        found = re.match(r"\s*#\s*table:\s*(.*)$", line)
        if found:
            parts = re.split(r"[,\s]+(?=[A-Za-z_]\w*=)", found.group(1))
            fields = dict(part.split("=", 1) for part in parts)
            if set(fields) != {"site", "targets"}:
                raise ValueError("table directive needs site= and targets=: %s" % line)
            tables.append((fields["site"], fields["targets"].split(",")))
        found = re.match(r"\s*#\s*method:\s*(.*)$", line)
        if found:
            fields = dict(part.split("=", 1) for part in found.group(1).split())
            if set(fields) != {"site", "identity", "argc"}:
                raise ValueError("method directive needs site=, identity= and argc=: %s" % line)
            methods.append((fields["site"], fields["identity"], int(fields["argc"], 0)))
    return inputs, tables, methods


def fixture(case_files, out):
    cases = []
    symbols = {}
    for index, path in enumerate(case_files):
        text_base = IMAGE_BASE + TEXT_RVA + index * CASE_STRIDE
        data_base = IMAGE_BASE + DATA_RVA + index * CASE_STRIDE
        prelude = (".intel_syntax noprefix\n.equ TEXT_BASE, 0x%08x\n.equ DATA_BASE, 0x%08x\n"
                   ".equ ESP_INIT, 0x%08x\n.section .data\ndata_start:\n.text\ncase_start:\n"
                   % (text_base, data_base, ESP_INIT))
        source = out / ("%s.s" % path.stem)
        obj = out / ("%s.o" % path.stem)
        text = prelude + path.read_text()
        source.write_text(text)
        subprocess.run(["clang", "-target", "i386-unknown-linux-gnu", "-c", str(source), "-o", str(obj)],
                       check=True)
        parsed = Object(obj)
        funcs = [s for s in parsed.at(".text") if s["type"] == 2 and s["size"]]
        if not funcs:
            raise ValueError("%s: no sized STT_FUNC symbols in .text" % path)
        text_blob, data_blob = parsed.data[".text"], parsed.data[".data"]
        if len(text_blob) > CASE_STRIDE or len(data_blob) > CASE_STRIDE:
            raise ValueError("%s: case exceeds %d bytes" % (path, CASE_STRIDE))
        inputs, tables, methods = case_directives(path.read_text())
        entries = [s for s in funcs if s["bind"] in (1, 2)]
        if not entries:
            raise ValueError("%s: no global function to run" % path)
        spans = []
        md = diff.capstone.Cs(diff.capstone.CS_ARCH_X86, diff.capstone.CS_MODE_32)
        for fn in sorted(funcs, key=lambda s: s["value"]):
            start, size = fn["value"], fn["size"]
            if start + size > len(text_blob):
                raise ValueError("%s: function %s exceeds .text" % (path, fn["name"]))
            lengths, at = [], start
            for insn in md.disasm(text_blob[start:start + size], text_base + start):
                lengths.append("%x" % insn.size)
                at += insn.size
            if at != start + size or not lengths:
                raise ValueError("%s: capstone does not tile %s" % (path, fn["name"]))
            spans.append((text_base + start, size, "".join(lengths)))
            symbols["%s:%s" % (path.stem, fn["name"])] = text_base + start
        for sym in parsed.at(".text"):
            if sym["name"]:
                symbols.setdefault("%s:%s" % (path.stem, sym["name"]), text_base + sym["value"])
        table_rows = []
        for site, targets in tables:
            site_addr = resolve(symbols, path.stem, site)
            target_addrs = [resolve(symbols, path.stem, target) for target in targets]
            owner = next((span[0] for span in spans if span[0] <= site_addr < span[0] + span[1]), None)
            if owner is None:
                raise ValueError("%s: table site %s is outside a function" % (path, site))
            table_rows.append((owner, site_addr, target_addrs))
        cases.append({"name": path.stem, "path": path, "text": text_blob, "data": data_blob,
                      "text_base": text_base, "data_base": data_base, "spans": spans,
                      "entries": [text_base + s["value"] for s in entries],
                      "inputs": inputs, "tables": table_rows,
                      "methods": [(resolve(symbols, path.stem, site), identity, argc)
                                  for site, identity, argc in methods],
                      "xfail": re.search(r"^\s*#\s*xfail:\s*(.+)$", path.read_text(), re.M)})
    return cases


def method_facts(cases, exe):
    methods, sites = [], []
    for case in cases:
        for site, identity, argc in case["methods"]:
            sites.append({"s": "%08x" % site, "k": "call", "e": 0, "ev": [0], "m": len(methods)})
            methods.append({"id": len(methods), "identity": identity, "argc": argc, "evidence": [0]})
    return {"format": "recomp-analysis-facts-v2",
            "executable_sha256": hashlib.sha256(exe.read_bytes()).hexdigest(),
            "evidence": [{"id": 0, "text": "translator case # method directive"}],
            "effects": [{"id": 0, "regs": "unknown", "x87": "unknown"}],
            "methods": methods, "sites": sites, "inputs": {}}


def resolve(symbols, case, name):
    key = "%s:%s" % (case, name)
    if key not in symbols:
        raise ValueError("unknown symbol %r in %s" % (name, case))
    return symbols[key]


def pe_image(cases, path):
    text = b"".join(c["text"].ljust(CASE_STRIDE, b"\0") for c in cases)
    data = b"".join(c["data"].ljust(CASE_STRIDE, b"\0") for c in cases)
    text_raw = max(0x200, (len(text) + 0x1ff) & ~0x1ff)
    data_raw = max(0x200, (len(data) + 0x1ff) & ~0x1ff)
    headers = 0x400
    text_at, data_at = headers, headers + text_raw
    size_image = (DATA_RVA + len(data) + 0xfff) & ~0xfff
    file_size = data_at + data_raw
    dos = bytearray(headers)
    dos[:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3c, 0x80)
    dos[0x80:0x84] = b"PE\0\0"
    coff = 0x84
    struct.pack_into("<HHIIIHH", dos, coff, 0x14c, 2, 0, 0, 0, 0xe0, 0x0102)
    opt = coff + 20
    struct.pack_into("<H", dos, opt, 0x10b)
    struct.pack_into("<I", dos, opt + 16, TEXT_RVA)
    struct.pack_into("<I", dos, opt + 20, TEXT_RVA)
    struct.pack_into("<I", dos, opt + 24, DATA_RVA)
    struct.pack_into("<I", dos, opt + 28, IMAGE_BASE)
    struct.pack_into("<I", dos, opt + 32, 0x1000)
    struct.pack_into("<I", dos, opt + 36, 0x200)
    struct.pack_into("<HH", dos, opt + 40, 4, 0)
    struct.pack_into("<HH", dos, opt + 48, 4, 0)
    struct.pack_into("<I", dos, opt + 56, size_image)
    struct.pack_into("<I", dos, opt + 60, headers)
    struct.pack_into("<H", dos, opt + 68, 3)
    struct.pack_into("<I", dos, opt + 92, 16)
    section = opt + 0xe0
    struct.pack_into("<8sIIIIIIHHI", dos, section, b".text", len(text), TEXT_RVA, text_raw,
                     text_at, 0, 0, 0, 0, 0x60000020)
    struct.pack_into("<8sIIIIIIHHI", dos, section + 40, b".data", len(data), DATA_RVA, data_raw,
                     data_at, 0, 0, 0, 0, 0xc0000040)
    image = bytes(dos).ljust(headers, b"\0") + text.ljust(text_raw, b"\0") + data.ljust(data_raw, b"\0")
    path.write_bytes(image[:file_size])


def write_map(cases, exe, root):
    root.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256(exe.read_bytes()).hexdigest()
    (root / "metadata.txt").write_text(
        "format=recomp-code-map-v3\nstatus=complete\nexecutable_sha256=%s\nimage_base=%08x\nfunctions=%d\n"
        % (digest, IMAGE_BASE, sum(len(c["spans"]) for c in cases)))
    rows = ["function\tstart\tbytes\tlengths"]
    for case in cases:
        for start, size, lengths in case["spans"]:
            rows.append("\t%08x\t%d\t%s" % (start, size, lengths))
    (root / "spans.tsv").write_text("\n".join(rows) + "\n")
    table_rows = ["function\tsite\ttargets"]
    for case in cases:
        for owner, site, targets in case["tables"]:
            table_rows.append("%08x\t%08x\t%s" % (owner, site, ",".join("%08x" % t for t in targets)))
    (root / "tables.tsv").write_text("\n".join(table_rows) + "\n")
    (root / "entries.tsv").write_text("address\towner\tkind\n")
    (root / "noreturn.tsv").write_text("address\tkind\n")


def game_dir(out, exe, map_dir, translate):
    game = out / "game"
    game.mkdir()
    (game / "fixture.exe").write_bytes(exe.read_bytes())
    shutil.copytree(map_dir, game / "map")
    directives = "".join("%s = %s\n" % (key, toml_value(value))
                         for key, value in translate.items())
    (game / "game.toml").write_text("""[game]
id = "translator-tests"
name = "Translator Tests"
app_name = "TranslatorTests"
bundle_id = "dev.recompkit.translator-tests"
executable = "fixture.exe"
developer_exe = "fixture.exe"
sha256 = "%s"
image_base = 0x00400000
entry_point = 0x00401000
guest_root = 'C:\\TranslatorTests'
windows_version = "4.10"
store_hooks = false

[translate]
engine = "rust"
code_map = "map"
discover = false
entry_points = []
%s

[input]
relative_mouse_capture = false

[controls]
default_layout = "hidden"
mobile_control = "off"

[bundle]
exclude = []
""" % (hashlib.sha256(exe.read_bytes()).hexdigest(), directives))
    return game


def translate(game, out, env=None):
    cfg = game_config.load(game, KIT / "build")
    cfg["translate_engine"] = "rust"
    source = out / "translate-input.json"
    source.write_text(json.dumps(build_py.translation_input(cfg), indent=1, sort_keys=True) + "\n")
    gen = out / "gen"
    report = out / "translate-report.json"
    binary = build_py.rust_translator(KIT / "build")
    subprocess.run([str(binary), "--input", str(source), "--out", str(gen), "--report", str(report)],
                   cwd=KIT, check=True, env={**os.environ, **(env or {})})
    return gen, report


def assert_translation(report_path, case_cases):
    report = json.loads(Path(report_path).read_text())
    wanted = {addr for case in case_cases for addr in case["entries"]}
    problems = []
    if report.get("diagnostics"):
        problems.append("diagnostics: %s" % report["diagnostics"][:3])
    if report.get("discovery_traps"):
        problems.append("discovery traps: %s" % report["discovery_traps"])
    if report["bodies"].get("trapping"):
        problems.append("%d trapping bodies" % report["bodies"]["trapping"])
    for reason, sites in (report["fallbacks"].get("reasons") or {}).items():
        hit = wanted.intersection(int(site, 16) for site in sites["sites"])
        if hit:
            problems.append("fallback %s at %s" % (reason, ", ".join("%08x" % a for a in sorted(hit))))
    for reason, addrs in (report["bodies"].get("detail_addrs") or {}).items():
        hit = wanted.intersection(int(addr, 16) if isinstance(addr, str) else addr for addr in addrs)
        if hit:
            problems.append("body detail %s at %s" % (reason, ", ".join("%08x" % a for a in sorted(hit))))
    return problems


def run_cases(cases, gen, harness, out, args):
    symbols = json.loads((gen / "symbols.json").read_text())
    emitted = {int(row["addr"], 16) for row in symbols["functions"]}
    missing = [addr for case in cases for addr in case["entries"] if addr not in emitted]
    if missing:
        raise RuntimeError("translated entries missing: %s" % ", ".join("%08x" % a for a in missing))
    image = diff.Image(out / "game/fixture.exe")
    addrs = [addr for case in cases for addr in case["entries"]]
    fixed = {addr: case["inputs"] for case in cases for addr in case["entries"]}
    names = {addr: case["name"] for case in cases for addr in case["entries"]}
    run_args = SimpleNamespace(mode="specialized", iterations=args.iterations, max_insns=20000,
                               timeout=args.timeout, jobs=args.jobs, seed=args.seed,
                               fpu_cw=list(diff.FPU_CWS), replay_inputs=None, record_inputs=None,
                               fail_on="any", fixed_inputs=fixed, structured_inputs=0)
    results = diff.run_all(addrs, run_args, harness, image)
    xfail = {addr: case["xfail"].group(1) for case in cases if case["xfail"] for addr in case["entries"]}
    failures = []
    for result in sorted(results, key=lambda r: r["addr"]):
        failed = result["status"] != "ok" or result.get("compared") != args.iterations or result.get("skipped")
        if result["addr"] in xfail:
            print("%s %s %08x: %s" % ("xfail" if failed else "XPASS", names[result["addr"]],
                                      result["addr"], xfail[result["addr"]]))
            if not failed:
                failures.append({**result, "status": "unexpectedly passed"})
        elif failed:
            failures.append(result)
    case_seconds = {}
    for result in results:
        name = names.get(result["addr"], "?")
        case_seconds[name] = case_seconds.get(name, 0.0) + result.get("seconds", 0.0)
    if failures:
        print("translator case failures:", file=sys.stderr)
        for result in failures:
            addr = result["addr"]
            print("  %s %08x %s (compared %s/%d)" % (
                names.get(addr, "?"), addr, result["status"],
                result.get("compared", 0), args.iterations), file=sys.stderr)
            if result.get("input"):
                print("    input %s" % json.dumps(result["input"]), file=sys.stderr)
            if result.get("skipped"):
                print("    skipped %s" % json.dumps(result["skipped"]), file=sys.stderr)
            if result.get("problems"):
                for problem in result["problems"]:
                    print("    %s" % problem, file=sys.stderr)
    return failures, case_seconds


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--case", action="append", default=[], help="run only this case (repeatable)")
    parser.add_argument("--iterations", type=int, default=200)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    parser.add_argument("--timeout", type=int, default=60)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    if args.iterations < 1 or args.jobs < 1 or args.timeout < 1:
        parser.error("iterations, jobs and timeout must be positive")
    files = sorted(CASES.glob("*.s"))
    if args.case:
        selected = set(args.case)
        files = [path for path in files if path.stem in selected]
        missing = selected - {path.stem for path in files}
        if missing:
            parser.error("unknown case(s): %s" % ", ".join(sorted(missing)))
    if not files:
        parser.error("no cases selected")
    if args.out is not None:
        out = args.out.resolve()
        if out.exists():
            parser.error("--out already exists; refusing to reuse %s" % out)
        out.mkdir(parents=True)
    else:
        base = KIT / "build/translator-tests"
        base.mkdir(parents=True, exist_ok=True)
        out = Path(tempfile.mkdtemp(prefix="run-", dir=base))
    started = time.time()
    timings = {}
    groups = {}
    for path in files:
        translate_config, env = case_config(path.read_text())
        key = (json.dumps(translate_config, sort_keys=True), tuple(sorted(env.items())))
        groups.setdefault(key, []).append(path)
    failures = []
    case_seconds = {}
    total_cases = 0
    for index, (key, group_files) in enumerate(groups.items()):
        translate_config, env = case_config(group_files[0].read_text())
        group_translate = {**DEFAULT_TRANSLATE, **translate_config}
        root = out / ("group-%d" % index)
        root.mkdir()
        phase = time.time()
        cases = fixture(group_files, root)
        exe = root / "fixture.exe"
        pe_image(cases, exe)
        map_dir = root / "map"
        write_map(cases, exe, map_dir)
        facts = any(case["methods"] for case in cases)
        if facts:
            group_translate["analysis_facts"] = "analysis-facts.json"
        game = game_dir(root, exe, map_dir, group_translate)
        if facts:
            (game / "analysis-facts.json").write_text(json.dumps(method_facts(cases, exe), indent=1))
        timings["group%d.assemble" % index] = time.time() - phase
        phase = time.time()
        gen, report = translate(game, root, env)
        timings["group%d.translate" % index] = time.time() - phase
        problems = assert_translation(report, cases)
        if problems:
            print("translation check failed:", file=sys.stderr)
            for problem in problems:
                print("  " + problem, file=sys.stderr)
            return 1
        harness = root / ("harness.dylib" if platform.system() == "Darwin" else "harness.so")
        phase = time.time()
        subprocess.run([sys.executable, str(KIT / "tools/build.py"), "--translator-harness",
                        "--translator-gen", str(gen), "--harness-out", str(harness)], cwd=KIT, check=True,
                       env={**os.environ, **env})
        timings["group%d.native" % index] = time.time() - phase
        phase = time.time()
        group_failures, group_seconds = run_cases(cases, gen, harness, root, args)
        timings["group%d.execute" % index] = time.time() - phase
        failures.extend(group_failures)
        total_cases += len(cases)
        for name, seconds in group_seconds.items():
            case_seconds[name] = case_seconds.get(name, 0.0) + seconds
    for name, seconds in case_seconds.items():
        timings["case:" + name] = seconds
    timings["total"] = time.time() - started
    print("%d cases in %d groups, %d iterations each, %.1fs: %s" % (
        total_cases, len(groups), args.iterations, timings["total"],
        "PASS" if not failures else "%d FAILED" % len(failures)))
    print("timings: " + ", ".join("%s %.2fs" % item for item in timings.items()))
    print("run: %s" % out)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
