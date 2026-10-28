#!/usr/bin/env python3
"""Run translated game functions and the original bytes under Unicorn on the same random inputs.

    run.py --game <id> --func 0x401000 [--func ...]
    run.py --game <id> --changed --baseline before-bodies.json
    run.py --game <id> --sample 2000
    run.py --game <id> --all
    run.py --game <id> --suite path.json --mode paired

Uses the archive from the last `tools/build.py` build, compiling the linked
harness through `tools/build.py --diff-harness`. Inputs whose original execution
faults, calls an import or runs too long are discarded; a function is reported
when a translation returns different registers, flags, x87 state or memory.

--mode selects what is linked into the run:
  specialized  production contract call sites (recomp_hooks_ever = 0), default
  ordinary     forced ordinary fallback (recomp_hooks_ever = 1)
  paired       both, per input, compared with each other and the original

Every run writes a fresh report directory. `--changed` reads only the explicit
`--baseline`; `--save-baseline` writes a new one and is never implicit.
"""

import argparse
import base64
import gzip
import ctypes as C
import hashlib
import json
import math
import os
import platform
import random
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

import numpy as np
import pefile
import capstone
from capstone import x86_const as CS_X86
from unicorn import (Uc, UcError, UC_ARCH_X86, UC_MODE_32, UC_HOOK_INTR, UC_HOOK_MEM_INVALID, UC_HOOK_CODE,
                     UC_PROT_READ, UC_PROT_WRITE, UC_PROT_EXEC, UC_PROT_ALL)
from unicorn.x86_const import (
    UC_X86_REG_EAX, UC_X86_REG_ECX, UC_X86_REG_EDX, UC_X86_REG_EBX, UC_X86_REG_ESP,
    UC_X86_REG_EBP, UC_X86_REG_ESI, UC_X86_REG_EDI, UC_X86_REG_EFLAGS, UC_X86_REG_EIP,
    UC_X86_REG_FS, UC_X86_REG_SS, UC_X86_REG_DS, UC_X86_REG_ES, UC_X86_REG_GDTR)
from unicorn.x86_const import UC_X86_REG_FPTAG, UC_X86_REG_FPSW

KIT = Path(__file__).resolve().parents[3]
sys.path[:0] = [str(KIT / "tools"), str(KIT / "tools/recomp")]
import buildlock  # noqa: E402
import game_config  # noqa: E402

SCRATCH, SCRATCH_SIZE = 0x0E000000, 0x00100000
STACK, STACK_SIZE = 0x0EF00000, 0x00100000
ESP_INIT = STACK + STACK_SIZE - 0x1000
ARG_BYTES = 0x100
CAVE = 0x0DEAC000
MAGIC_RET = CAVE
FNSAVE_CODE = CAVE + 0x20
FNSAVE_SLOT = CAVE + 0x200
FPU_RESET = CAVE + 0x40
FPU_CW_SLOT = CAVE + 0x300
IMPORT_TRAP = 0x0DF00000
TEB = 0x0DEB0000
GDT = 0x0DEB1000
ZERO_STACK = bytes(STACK_SIZE)

REGS = [UC_X86_REG_EAX, UC_X86_REG_ECX, UC_X86_REG_EDX, UC_X86_REG_EBX,
        UC_X86_REG_ESP, UC_X86_REG_EBP, UC_X86_REG_ESI, UC_X86_REG_EDI]
REG_NAMES = ["EAX", "ECX", "EDX", "EBX", "ESP", "EBP", "ESI", "EDI"]
FLAGS = {"CF": 0x001, "PF": 0x004, "ZF": 0x040, "SF": 0x080, "DF": 0x400, "OF": 0x800}
FLAG_MASK = sum(FLAGS.values())
FPU_CWS = (0x037F, 0x027F, 0x007F)
HOOKS_EVER = {"ordinary": 1, "specialized": 0}
MODES = ("ordinary", "specialized", "paired")


class State(C.Structure):
    _fields_ = [("r", C.c_uint32 * 8), ("eflags", C.c_uint32),
                ("fpu_cw", C.c_uint16), ("fpu_sw", C.c_uint16), ("fpu_tag", C.c_uint16),
                ("fpu_top", C.c_uint32), ("st0", C.c_double), ("escape_addr", C.c_uint32)]


def harness_name():
    return "harness.dylib" if platform.system() == "Darwin" else "harness.so"


def harness_path(build_root):
    return Path(build_root) / "recomp/diff" / harness_name()


def build_harness(build_root, out, game_dir, no_build=False):
    """Build the linked harness into this run's directory, or copy the prebuild."""
    if no_build:
        shared = harness_path(build_root)
        if not shared.exists():
            sys.exit("--no-build given but the prebuilt harness is missing: %s" % shared)
        with buildlock.BuildLock(Path(build_root).parent, "diff/run.py --no-build"):
            shutil.copy2(shared, out)
        return
    subprocess.check_call(
        [sys.executable, str(KIT / "tools/build.py"), "--game-dir", str(game_dir),
         "--build-root", str(build_root), "--diff-harness", "--harness-out", str(out)],
        cwd=str(KIT))


def function_bodies(gen):
    header = re.compile(r"^/\* FUN_([0-9a-f]{8})\s+\d+ insns\s+[0-9a-f]{8}\.\.[0-9a-f]{8}[^*]*\*/$", re.M)
    bodies = {}
    for chunk in sorted(gen.glob("chunk_*.c")):
        text = chunk.read_text()
        marks = list(header.finditer(text))
        for i, m in enumerate(marks):
            end = marks[i + 1].start() if i + 1 < len(marks) else len(text)
            bodies[int(m.group(1), 16)] = hashlib.sha1(text[m.start():end].encode()).hexdigest()
    return bodies


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def sha256_or_none(path):
    path = Path(path)
    return sha256_file(path) if path.is_file() else None


def tree_sha256(root):
    root = Path(root)
    if not root.is_dir():
        return None
    h = hashlib.sha256()
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        h.update(path.relative_to(root).as_posix().encode() + b"\0")
        h.update(path.read_bytes())
    return h.hexdigest()


def functions_sha256(addrs):
    return hashlib.sha256(",".join("%08x" % a for a in sorted(addrs)).encode()).hexdigest()


def write_exclusive(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    with os.fdopen(fd, "w") as fh:
        fh.write(text)


class Image:
    def __init__(self, exe):
        pe = pefile.PE(str(exe))
        self.base = pe.OPTIONAL_HEADER.ImageBase
        self.size = (pe.OPTIONAL_HEADER.SizeOfImage + 0xFFF) & ~0xFFF
        image = bytearray(pe.get_memory_mapped_image()[:self.size])
        image += bytes(self.size - len(image))
        slot = 0
        for entry in getattr(pe, "DIRECTORY_ENTRY_IMPORT", []):
            for imp in entry.imports:
                struct.pack_into("<I", image, imp.address - self.base, IMPORT_TRAP + 16 * slot)
                slot += 1
        self.bytes = bytes(image)
        self.sections = []
        self.writable = []
        self.bss = []
        for s in pe.sections:
            start = self.base + s.VirtualAddress
            size = (max(s.Misc_VirtualSize, s.SizeOfRawData) + 0xFFF) & ~0xFFF
            perms = UC_PROT_READ
            if s.Characteristics & 0x20000000:
                perms |= UC_PROT_EXEC
            if s.Characteristics & 0x80000000:
                perms |= UC_PROT_WRITE
                self.writable.append((start, size))
                if s.Misc_VirtualSize > s.SizeOfRawData:
                    self.bss.append((start + (s.SizeOfRawData + 3 & ~3),
                                     s.Misc_VirtualSize - s.SizeOfRawData & ~3))
            self.sections.append((start, size, perms))


class Native:
    def __init__(self, dylib, image):
        self.lib = C.CDLL(str(dylib), mode=C.RTLD_GLOBAL)
        self.lib.harness_mem.restype = C.c_void_p
        self.lib.diff_run.restype = C.c_char_p
        self.lib.diff_run.argtypes = [C.c_uint32, C.POINTER(State), C.c_uint32]
        self.lib.diff_set_hooks_ever.argtypes = [C.c_uint8]
        self.base = self.lib.harness_mem()
        self.write(image.base, image.bytes)

    def set_hooks_ever(self, value):
        self.lib.diff_set_hooks_ever(value)

    def write(self, addr, data):
        C.memmove(self.base + addr, data, len(data))

    def read(self, addr, n):
        return C.string_at(self.base + addr, n)


class Emu:
    def __init__(self, image):
        self.u = Uc(UC_ARCH_X86, UC_MODE_32)
        self.u.mem_map(image.base, 0x1000, UC_PROT_READ)
        self.u.mem_write(image.base, image.bytes[:0x1000])
        for start, size, perms in image.sections:
            self.u.mem_map(start, size, perms)
            self.u.mem_write(start, image.bytes[start - image.base:start - image.base + size])
        self.u.mem_map(SCRATCH, SCRATCH_SIZE, UC_PROT_READ | UC_PROT_WRITE)
        self.u.mem_map(STACK, STACK_SIZE, UC_PROT_READ | UC_PROT_WRITE)
        self.u.mem_map(CAVE, 0x1000, UC_PROT_ALL)
        self.u.mem_write(FNSAVE_CODE, b"\xdd\x35" + struct.pack("<I", FNSAVE_SLOT))
        self.u.mem_write(FPU_RESET, b"\xdb\xe3\xd9\x2d" + struct.pack("<I", FPU_CW_SLOT))
        self.u.mem_map(TEB, 0x1000, UC_PROT_READ | UC_PROT_WRITE)
        self.u.mem_map(GDT, 0x1000, UC_PROT_READ)
        self.u.mem_write(GDT + 8, segment(0, 0xFFFFF, flags=0xC) + segment(TEB, 0xFFF))
        self.u.reg_write(UC_X86_REG_GDTR, (0, GDT, 0x17, 0))
        for reg in (UC_X86_REG_SS, UC_X86_REG_DS, UC_X86_REG_ES):
            self.u.reg_write(reg, 1 << 3)
        self.u.reg_write(UC_X86_REG_FS, 2 << 3)
        self.u.hook_add(UC_HOOK_INTR, lambda uc, n, _: uc.emu_stop())
        self.fault = None
        self.u.hook_add(UC_HOOK_MEM_INVALID, self.on_fault)
        self.decoder = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_32)
        self.decoder.detail = True
        self.flag_effects = {}
        self.flag_mask = FLAG_MASK
        self.stack_fault = False
        self.u.hook_add(UC_HOOK_CODE, self.on_instruction)
        self.image = image

    def on_instruction(self, uc, addr, size, _):
        raw = bytes(uc.mem_read(addr, size))
        effect = self.flag_effects.get(raw)
        if effect is None:
            instruction = next(self.decoder.disasm(raw, addr))
            written = undefined = 0
            required = set()
            mnemonic = instruction.mnemonic
            fpu = instruction.group(CS_X86.X86_GRP_FPU)
            if not fpu or mnemonic in ("fcomi", "fcomip", "fucomi", "fucomip"):
                for name, bit in FLAGS.items():
                    if any(instruction.eflags & getattr(CS_X86, "X86_EFLAGS_" + action + "_" + name, 0)
                           for action in ("MODIFY", "SET", "RESET")):
                        written |= bit
                    if instruction.eflags & getattr(CS_X86, "X86_EFLAGS_UNDEFINED_" + name, 0):
                        undefined |= bit
            if fpu:
                if mnemonic.startswith(("fadd", "fsub", "fmul", "fdiv", "fcom", "fucom", "fst", "fist")) or mnemonic in (
                        "fabs", "fchs", "fsqrt", "frndint", "fsin", "fcos", "fsincos", "fptan", "fxch", "ftst", "fxtract"):
                    if mnemonic not in ("fstcw", "fstsw", "fstenv"):
                        required.add(0)
                if mnemonic in ("fcompp", "fucompp", "fprem", "fprem1", "fpatan", "fyl2x", "fyl2xp1", "fscale"):
                    required.update((0, 1))
                if mnemonic not in ("ffree",):
                    for operand in instruction.operands:
                        if operand.type == CS_X86.X86_OP_REG and CS_X86.X86_REG_ST0 <= operand.reg <= CS_X86.X86_REG_ST7:
                            required.add(operand.reg - CS_X86.X86_REG_ST0)
            if instruction.mnemonic in ("imul", "mul"):
                undefined |= FLAGS["PF"] | FLAGS["ZF"] | FLAGS["SF"]
            effect = written, undefined, tuple(sorted(required))
            self.flag_effects[raw] = effect
        written, undefined, required = effect
        self.flag_mask = (self.flag_mask | written) & ~undefined
        if required:
            tag = uc.reg_read(UC_X86_REG_FPTAG)
            top = (uc.reg_read(UC_X86_REG_FPSW) >> 11) & 7
            if any((tag >> (2 * ((top + slot) & 7))) & 3 == 3 for slot in required):
                self.stack_fault = True

    def on_fault(self, uc, access, addr, size, value, _):
        self.fault = addr
        return False

    def fault_kind(self):
        a = self.fault
        if a is None:
            return "exception"
        if a < 0x10000:
            return "null page"
        if a >= IMPORT_TRAP and a < IMPORT_TRAP + 0x100000:
            return "import"
        if self.image.base <= a < self.image.base + self.image.size:
            return "image"
        if TEB <= a < TEB + 0x1000 or SCRATCH <= a < SCRATCH + SCRATCH_SIZE or STACK <= a < STACK + STACK_SIZE:
            return "protection"
        return "wild"


def segment(base, limit, access=0x93, flags=0x4):
    return struct.pack("<Q", (limit & 0xFFFF) | (base & 0xFFFFFF) << 16 | access << 40 |
                       (limit >> 16 & 0xF) << 48 | flags << 52 | (base >> 24 & 0xFF) << 56)


def teb():
    blob = bytearray(0x1000)
    struct.pack_into("<III", blob, 0, 0xFFFFFFFF, STACK + STACK_SIZE, STACK)
    struct.pack_into("<I", blob, 0x18, TEB)
    return bytes(blob)


class Inputs:
    def __init__(self, seed, image, cws):
        self.cws = cws
        self.rng = random.Random(seed)
        self.np = np.random.default_rng(seed)
        self.image = image
        self.pool = b""

    def pointer(self):
        return SCRATCH + (self.rng.randrange(SCRATCH_SIZE * 3 // 4) & ~3)

    def word(self):
        k = self.rng.random()
        if k < 0.5:
            return self.pointer()
        if k < 0.75:
            return self.rng.randrange(256)
        if k < 0.9:
            return struct.unpack("<I", struct.pack("<f", self.rng.uniform(-1000, 1000)))[0]
        return self.rng.getrandbits(32)

    def words(self, size):
        if len(self.pool) < 2 * size:
            self.pool = self.fresh(2 * size)
        at = self.rng.randrange(size // 4 + 1) * 4
        return self.pool[at:at + size]

    def fresh(self, size):
        n = size // 4
        kind = self.np.random(n)
        pointers = SCRATCH + self.np.integers(0, SCRATCH_SIZE * 3 // 16, n) * 4
        small = self.np.integers(0, 256, n)
        floats = self.np.uniform(-1000, 1000, n).astype(np.float32).view(np.uint32)
        noise = self.np.integers(0, 1 << 32, n, dtype=np.uint64)
        words = np.select([kind < 0.55, kind < 0.7, kind < 0.9], [pointers, small, floats], noise)
        return words.astype("<u4").tobytes()

    def make(self):
        regs = [self.word() for _ in range(8)]
        regs[4] = ESP_INIT
        stack = struct.pack("<I", MAGIC_RET) + b"".join(
            struct.pack("<I", self.word()) for _ in range(ARG_BYTES // 4 - 1))
        eflags = 0x202 | (self.rng.getrandbits(12) & (FLAG_MASK & ~FLAGS["DF"]))
        return (regs, eflags, self.rng.choice(self.cws), stack,
                [(SCRATCH, self.words(SCRATCH_SIZE)), (TEB, teb())] + [(a, self.words(n)) for a, n in self.image.bss])

    def structured(self, index):
        pointer = SCRATCH + 0x1000
        regs = [pointer if index == 1 else 0] * 8
        regs[1], regs[4] = pointer, ESP_INIT
        stack = struct.pack("<I", MAGIC_RET) + struct.pack(
            "<I", pointer if index == 2 else 0) * (ARG_BYTES // 4 - 1)
        fill = (0, 1, 255, SCRATCH + 0x2000, 0)[index]
        scratch = bytearray(struct.pack("<I", fill) * (SCRATCH_SIZE // 4))
        struct.pack_into("<I", scratch, 0x1000, 0)
        return (regs, 0x202, self.cws[index % len(self.cws)], stack,
                [(SCRATCH, bytes(scratch)), (TEB, teb())] + [(a, bytes(n)) for a, n in self.image.bss])


def ext80(b):
    mant, se = struct.unpack("<QH", b)
    sign = -1.0 if se & 0x8000 else 1.0
    exp = se & 0x7FFF
    if exp == 0x7FFF:
        return float("nan") if mant << 1 & ((1 << 64) - 1) else sign * float("inf")
    if exp == 0 and mant == 0:
        return sign * 0.0
    try:
        return sign * math.ldexp(float(mant), exp - 16383 - 63)
    except OverflowError:
        return sign * float("inf")


def close(a, b, tol):
    if a == b or (a != a and b != b):
        return True
    return abs(a - b) <= tol * max(abs(a), abs(b))


def memory_diff(want, got, base):
    hard, fp = [], []
    w = np.frombuffer(want, dtype="<u4")
    g = np.frombuffer(got, dtype="<u4")
    for i in np.nonzero(w != g)[0][:64]:
        a, b = int(w[i]), int(g[i])
        fa, fb = (struct.unpack("<f", struct.pack("<I", v))[0] for v in (a, b))
        entry = "%08x: %08x != %08x" % (base + 4 * int(i), a, b)
        (fp if close(fa, fb, 1e-6) or ((i & 1) and close_double(w, g, i)) else hard).append(entry)
    return hard, fp


def close_double(w, g, i):
    a = struct.unpack("<d", struct.pack("<II", int(w[i - 1]), int(w[i])))[0]
    b = struct.unpack("<d", struct.pack("<II", int(g[i - 1]), int(g[i])))[0]
    return close(a, b, 1e-12)


def regions(image):
    return list(image.writable) + [(SCRATCH, SCRATCH_SIZE), (ESP_INIT, ARG_BYTES), (TEB, 0x1000)]


def make_state(regs, eflags, cw):
    st = State()
    st.r[:] = regs
    st.eflags = eflags
    st.fpu_cw = cw
    return st


def load_memory(sink, data, fills, stack):
    for start, blob in data:
        sink(start, blob)
    for start, blob in fills:
        sink(start, blob)
    sink(STACK, ZERO_STACK)
    sink(ESP_INIT, stack)


def oracle_snapshot(u, fnsave, region_list):
    """x87 ST0 is only compared when the oracle tag word marks it valid."""
    want_tag = struct.unpack_from("<H", fnsave, 8)[0]
    want_top = (struct.unpack_from("<H", fnsave, 4)[0] >> 11) & 7
    st0 = None
    if (want_tag >> (2 * want_top)) & 3 != 3:
        st0 = ext80(fnsave[28:38])
    return {"regs": [u.reg_read(r) for r in REGS],
            "eflags": u.reg_read(UC_X86_REG_EFLAGS),
            "fpu_top": want_top, "st0": st0,
            "mem": [bytes(u.mem_read(s, n)) for s, n in region_list]}


def native_snapshot(native, st, region_list):
    active = (st.fpu_tag >> (2 * st.fpu_top)) & 3 != 3
    return {"regs": [st.r[i] for i in range(8)],
            "eflags": st.eflags,
            "fpu_top": st.fpu_top, "st0": st.st0 if active else None,
            "mem": [native.read(s, n) for s, n in region_list]}


def compare_states(want, got, region_list):
    """Differences of one state against another, in a deterministic order."""
    problems = []
    for name, a, b in zip(REG_NAMES, want["regs"], got["regs"]):
        if a != b:
            problems.append("%s %08x != %08x" % (name, a, b))
    flags = [n for n, bit in FLAGS.items()
             if (want["eflags"] ^ got["eflags"]) & bit & want.get("flag_mask", FLAG_MASK)]
    if want["fpu_top"] != got["fpu_top"]:
        problems.append("x87 TOP %d != %d" % (want["fpu_top"], got["fpu_top"]))
    else:
        a, b = want["st0"], got["st0"]
        if (a is None) != (b is None):
            problems.append("x87 ST0 %s != %s" % ("empty" if a is None else "active",
                                                  "empty" if b is None else "active"))
        elif a is not None and not close(a, b, 1e-9):
            problems.append("ST0 %r != %r" % (a, b))
    fp = []
    for (start, n), w, g in zip(region_list, want["mem"], got["mem"]):
        hard, soft = memory_diff(w, g, start)
        problems += hard
        fp += soft
    if flags and not problems:
        problems.append("flags " + " ".join(flags))
    if fp and not problems:
        problems.append("float only: " + "; ".join(fp[:4]))
    return problems


def compare_pair(specialized, ordinary, region_list):
    """Specialized against ordinary, including escape shapes, as a pair result."""
    if specialized[0] == "escape" and ordinary[0] == "escape":
        if specialized[1] == ordinary[1] and specialized[2] == ordinary[2]:
            return []
        if specialized[1] == ordinary[1]:
            return ["pair escape %s @%08x != @%08x" % (specialized[1], specialized[2], ordinary[2])]
        return ["pair escape %s != %s" % (specialized[1], ordinary[1])]
    if specialized[0] == "escape":
        return ["pair specialized escaped (%s @%08x) but ordinary returned"
                % (specialized[1], specialized[2])]
    if ordinary[0] == "escape":
        return ["pair ordinary escaped (%s @%08x) but specialized returned"
                % (ordinary[1], ordinary[2])]
    return ["pair " + p for p in compare_states(specialized[1], ordinary[1], region_list)]


def findings_add(store, problems):
    for problem in problems:
        store[problem] = store.get(problem, 0) + 1


def findings_list(store):
    return sorted(({"problem": p, "count": c} for p, c in store.items()),
                  key=lambda f: (-f["count"], f["problem"]))


def oracle_problems(oracle, variant, region_list):
    if variant[0] == "escape":
        return ["translation escaped (%s %08x) where the original returned" % (variant[1], variant[2])]
    return compare_states(oracle, variant[1], region_list)


def emulate_original(u, emu, addr, data, region_list, args, regs, eflags, cw, stack, fills):
    """Run the original bytes and return (skip_reason, oracle_snapshot|None)."""
    load_memory(u.mem_write, data, fills, stack)
    for r, v in zip(REGS, regs):
        u.reg_write(r, v)
    u.mem_write(FPU_CW_SLOT, struct.pack("<H", cw))
    u.emu_start(FPU_RESET, FPU_RESET + 8)
    u.reg_write(UC_X86_REG_EFLAGS, eflags)
    emu.fault = None
    emu.flag_mask = FLAG_MASK
    emu.stack_fault = False
    try:
        u.emu_start(addr, MAGIC_RET, count=args.max_insns)
    except UcError:
        return emu.fault_kind(), None
    if u.reg_read(UC_X86_REG_EIP) != MAGIC_RET:
        return "original did not return", None
    saved_fpu = fnsave(u)
    if emu.stack_fault or struct.unpack_from("<H", saved_fpu, 4)[0] & 0x40:
        return "original x87 stack fault", None
    oracle = oracle_snapshot(u, saved_fpu, region_list)
    oracle["flag_mask"] = emu.flag_mask
    return None, oracle


def run_variant(native, mode, addr, data, fills, stack, region_list, regs, eflags, cw):
    load_memory(native.write, data, fills, stack)
    native.set_hooks_ever(HOOKS_EVER[mode])
    st = make_state(regs, eflags, cw)
    escape = native.lib.diff_run(addr, C.byref(st), TEB)
    if escape:
        return ("escape", escape.decode(), st.escape_addr)
    return ("state", native_snapshot(native, st, region_list))


def apply_fixed(values, spec):
    regs, eflags, cw, stack, fills = values
    regs = list(regs)
    stack = bytearray(stack)
    for name, value in spec.get("regs", {}).items():
        regs[REG_NAMES.index(name)] = value & 0xffffffff
    if spec.get("eflags") is not None:
        eflags = spec["eflags"] & 0xffffffff
    for index, value in spec.get("args", {}).items():
        struct.pack_into("<I", stack, 4 + 4 * index, value & 0xffffffff)
    return regs, eflags, cw, bytes(stack), fills


class RecordedInputs:
    def __init__(self, addr, image, args):
        self.addr, self.args, self.iteration = addr, args, 0
        self.source = Inputs(args.seed ^ addr, image, args.fpu_cw)
        self.fixed = getattr(args, "fixed_inputs", {}).get(addr, [])

    def make(self):
        args = self.args
        index = self.iteration
        filename = "%08x-%06d.json.gz" % (self.addr, index)
        self.iteration += 1
        if args.replay_inputs:
            with gzip.open(args.replay_inputs / filename, "rt") as stream:
                record = json.load(stream)
            return (record["regs"], record["eflags"], record["cw"],
                    base64.b64decode(record["stack"], validate=True),
                    [(addr, base64.b64decode(data, validate=True)) for addr, data in record["fills"]])
        values = (self.source.make() if index < args.iterations
                  else self.source.structured(index - args.iterations))
        if index < len(self.fixed):
            values = apply_fixed(values, self.fixed[index])
        if args.record_inputs:
            regs, eflags, cw, stack, fills = values
            record = {"regs": regs, "eflags": eflags, "cw": cw,
                      "stack": base64.b64encode(stack).decode(),
                      "fills": [(addr, base64.b64encode(data).decode()) for addr, data in fills]}
            with gzip.open(args.record_inputs / filename, "xt") as stream:
                json.dump(record, stream, separators=(",", ":"))
        return values


def run_single(addr, native, emu, image, args):
    """One mode, the original bytes and the translation on shared inputs."""
    mode = args.mode
    inputs = RecordedInputs(addr, image, args)
    u = emu.u
    data = [(s, image.bytes[s - image.base:s - image.base + n]) for s, n in image.writable]
    region_list = regions(image)
    skipped = Counter()
    compared = 0
    for it in range(args.iterations + args.structured_inputs):
        regs, eflags, cw, stack, fills = inputs.make()
        reason, oracle = emulate_original(u, emu, addr, data, region_list, args,
                                          regs, eflags, cw, stack, fills)
        if reason is not None:
            skipped[reason] += 1
            continue
        compared += 1
        variant = run_variant(native, mode, addr, data, fills, stack, region_list,
                              regs, eflags, cw)
        problems = oracle_problems(oracle, variant, region_list)
        if problems:
            store = {}
            findings_add(store, problems)
            findings = findings_list(store)
            return {"addr": addr, "status": "diverged", "iteration": it, "compared": compared,
                    "mode": mode, "oracle": {mode: findings}, "pair": [],
                    "input": input_record(regs, eflags, cw, stack),
                    "problems": [f["problem"] for f in findings][:24]}
    return {"addr": addr, "status": "ok" if compared else "unreached", "compared": compared,
            "mode": mode, "oracle": {mode: []}, "pair": [], "skipped": dict(skipped)}


def run_paired(addr, native, emu, image, args):
    """Both modes on every returning input; oracle and pair results aggregate."""
    inputs = RecordedInputs(addr, image, args)
    u = emu.u
    data = [(s, image.bytes[s - image.base:s - image.base + n]) for s, n in image.writable]
    region_list = regions(image)
    skipped = Counter()
    compared = 0
    oracle_store = {"specialized": {}, "ordinary": {}}
    pair_store = {}
    first = {}
    for it in range(args.iterations + args.structured_inputs):
        regs, eflags, cw, stack, fills = inputs.make()
        reason, oracle = emulate_original(u, emu, addr, data, region_list, args,
                                          regs, eflags, cw, stack, fills)
        if reason is not None:
            skipped[reason] += 1
            continue
        compared += 1
        variants = {mode: run_variant(native, mode, addr, data, fills, stack, region_list,
                                      regs, eflags, cw)
                    for mode in ("specialized", "ordinary")}
        for mode in ("specialized", "ordinary"):
            problems = oracle_problems(oracle, variants[mode], region_list)
            if problems and mode not in first:
                first[mode] = {"iteration": it, "input": input_record(regs, eflags, cw, stack),
                               "problems": problems[:24]}
            findings_add(oracle_store[mode], problems)
        pair = compare_pair(variants["specialized"], variants["ordinary"], region_list)
        if pair and "pair" not in first:
            first["pair"] = {"iteration": it, "input": input_record(regs, eflags, cw, stack),
                             "problems": pair[:24]}
        findings_add(pair_store, pair)
    findings = findings_list(pair_store)
    diverged = bool(findings) or any(oracle_store[m] for m in oracle_store)
    return {"addr": addr, "status": "diverged" if diverged else ("ok" if compared else "unreached"),
            "compared": compared, "mode": "paired",
            "oracle": {m: findings_list(oracle_store[m]) for m in ("specialized", "ordinary")},
            "pair": findings, "first": first, "skipped": dict(skipped)}


def fnsave(u):
    """Run FNSAVE in the Unicorn cave and return the 108-byte image."""
    u.mem_write(FNSAVE_SLOT, bytes(108))
    u.emu_start(FNSAVE_CODE, FNSAVE_CODE + 6)
    return bytes(u.mem_read(FNSAVE_SLOT, 108))


def input_record(regs, eflags, cw, stack):
    return {"regs": dict(zip(REG_NAMES, ("%08x" % v for v in regs))),
            "eflags": "%08x" % eflags, "fpu_cw": "%04x" % cw,
            "args": [("%08x" % v) for v in struct.unpack("<8I", stack[4:36])]}


def run_function(addr, native, emu, image, args):
    if args.mode == "paired":
        return run_paired(addr, native, emu, image, args)
    return run_single(addr, native, emu, image, args)


def run_all(addrs, args, dylib, image):
    native = Native(dylib, image)
    emu = Emu(image)
    pending = list(addrs)
    running = {}
    results = []
    started = time.time()
    while pending or running:
        while pending and len(running) < args.jobs:
            addr = pending.pop(0)
            r, w = os.pipe()
            pid = os.fork()
            if pid == 0:
                os.close(r)
                try:
                    result = run_function(addr, native, emu, image, args)
                except Exception as e:  # noqa: BLE001
                    result = {"addr": addr, "status": "error", "problems": [repr(e)]}
                os.write(w, json.dumps(result).encode())
                os._exit(0)
            os.close(w)
            running[r] = (pid, addr, time.time(), bytearray())
        ready, _, _ = select.select(list(running), [], [], 0.5)
        for fd in ready:
            chunk = os.read(fd, 1 << 16)
            if chunk:
                running[fd][3].extend(chunk)
                continue
            pid, addr, begun, out = running.pop(fd)
            os.close(fd)
            _, status = os.waitpid(pid, 0)
            if out:
                results.append(json.loads(out))
            else:
                results.append({"addr": addr, "status": "crashed",
                                "problems": ["translation crashed (signal %d)" % os.WTERMSIG(status)
                                             if os.WIFSIGNALED(status) else "worker exited"]})
            results[-1]["seconds"] = round(time.time() - begun, 3)
            report(results[-1], len(results), len(addrs))
        now = time.time()
        for fd, (pid, addr, begun, _) in list(running.items()):
            if now - begun > args.timeout:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
                os.close(fd)
                del running[fd]
                results.append({"addr": addr, "status": "hung",
                                "problems": ["no result after %ds" % args.timeout]})
                report(results[-1], len(results), len(addrs))
    print("%d functions in %.0fs" % (len(results), time.time() - started))
    return results


def report(result, done, total):
    if result["status"] in ("ok", "unreached"):
        if sys.stdout.isatty():
            print("\r%d/%d" % (done, total), end="", flush=True)
        return
    print("\r%08x %s" % (result["addr"], result["status"]))
    if "input" in result:
        print("    input %s" % json.dumps(result["input"]))
    for mode in sorted(result.get("oracle", ())):
        for f in result["oracle"][mode][:4]:
            print("    %s oracle x%d: %s" % (mode, f["count"], f["problem"]))
    for f in result.get("pair", [])[:4]:
        print("    pair x%d: %s" % (f["count"], f["problem"]))
    for category in sorted(result.get("first", {})):
        entry = result["first"][category]
        print("    first %s iteration %d: %s"
              % (category, entry["iteration"], json.dumps(entry["input"])))
    for p in result.get("problems", []):
        print("    " + p)


def failure(result, fail_on):
    if result["status"] in ("error", "crashed", "hung"):
        return True
    if fail_on == "none":
        return False
    if fail_on == "pair":
        return bool(result.get("pair"))
    return bool(result.get("pair")) or any(result.get("oracle", {}).values())


def default_run_dir(build_root, mode):
    base = Path(build_root) / "recomp/diff"
    base.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix="%s-" % mode, dir=base))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    game_config.add_game_args(parser)
    pick = parser.add_mutually_exclusive_group(required=True)
    pick.add_argument("--func", type=lambda v: int(v, 16), action="append")
    pick.add_argument("--changed", action="store_true")
    pick.add_argument("--sample", type=int)
    pick.add_argument("--all", action="store_true")
    pick.add_argument("--suite", type=Path, help="run a hash-pinned original-byte regression suite")
    parser.add_argument("--mode", choices=MODES, default="specialized",
                        help="specialized is production; ordinary forces fallback; paired runs both")
    parser.add_argument("--baseline", type=Path, default=None,
                        help="generated-body hashes read by --changed (there is no implicit baseline)")
    parser.add_argument("--save-baseline", type=Path, default=None,
                        help="write the current generated-body hashes here; never implicit")
    parser.add_argument("--out", type=Path, default=None,
                        help="report directory; refused if it already exists (default: a fresh directory)")
    parser.add_argument("--fail-on", choices=("any", "pair", "none"), default="any",
                        help="any fails on any oracle or pair discrepancy; pair only on pair")
    parser.add_argument("--no-build", action="store_true",
                        help="copy the prebuilt harness instead of invoking tools/build.py")
    recording = parser.add_mutually_exclusive_group()
    recording.add_argument("--record-inputs", type=Path)
    recording.add_argument("--replay-inputs", type=Path)
    parser.add_argument("--iterations", type=int, default=25,
                        help="random inputs per function (default: 25), followed by five structured seeds")
    parser.add_argument("--no-structured-inputs", action="store_true",
                        help="omit the five structured seeds; suites always use their original input schedule")
    parser.add_argument("--max-insns", type=int, default=200000)
    parser.add_argument("--timeout", type=int, default=60)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--fpu-cw", type=lambda v: int(v, 16), action="append",
                        help="initial x87 control words, default %s" % " ".join("%04x" % w for w in FPU_CWS))
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    args = game_config.resolve_game_args(parser.parse_args(), default=None)
    args.fpu_cw = args.fpu_cw or list(FPU_CWS)

    cfg = game_config.load(args.game_dir, args.build_root)
    suite = None
    if args.suite:
        suite = json.loads(args.suite.read_text())
        if (suite.get("format") != "recomp-diff-suite-v1"
                or suite.get("executable_sha256") != cfg["game"]["sha256"]):
            parser.error("regression suite schema or executable identity mismatch")
        args.seed, args.iterations = suite["seed"], suite["iterations"]
        args.fpu_cw = [int(cw, 16) for cw in suite["fpu_cws"]]

    args.structured_inputs = 0 if suite or args.no_structured_inputs else 5

    for name, value in (("--iterations", args.iterations), ("--jobs", args.jobs),
                        ("--max-insns", args.max_insns), ("--timeout", args.timeout)):
        if value < 1:
            parser.error("%s must be at least 1" % name)
    if args.sample is not None and args.sample < 1:
        parser.error("--sample must be at least 1")
    if args.fail_on == "pair" and args.mode != "paired":
        parser.error("--fail-on pair needs --mode paired")

    exe = cfg["developer_exe_path"]
    if not exe.is_file():
        parser.error("developer executable not found: %s (pass --build-root)" % exe)
    digest = sha256_file(exe)
    if digest != cfg["game"]["sha256"]:
        parser.error("executable identity mismatch: %s has %s, game.toml pins %s"
                     % (exe, digest, cfg["game"]["sha256"]))

    if (args.baseline is not None and args.save_baseline is not None
            and args.baseline.resolve() == args.save_baseline.resolve()):
        parser.error("--baseline and --save-baseline must be different paths")
    if args.save_baseline is not None and args.save_baseline.exists():
        parser.error("--save-baseline already exists; refusing to overwrite %s" % args.save_baseline)
    if args.out is not None:
        out = args.out.resolve()
        if out.exists():
            parser.error("--out already exists; refusing to reuse %s" % out)
        out.mkdir(parents=True)
    else:
        out = default_run_dir(args.build_root, args.mode)

    build_root = args.build_root
    gen = build_root / "recomp/gen"
    symbols = json.loads((gen / "symbols.json").read_text())
    entries = sorted(int(f["addr"], 16) for f in symbols["functions"] if f["kind"] == "entry")
    bodies = function_bodies(gen)
    if suite:
        selection = "suite"
        addrs = [int(addr, 16) for addr in suite["functions"]]
        if set(addrs) - set(entries):
            parser.error("regression suite contains unmapped entries")
    elif args.func:
        selection = "func"
        addrs = args.func
        if set(addrs) - set(entries):
            parser.error("--func contains unmapped entries")
    elif args.changed:
        selection = "changed"
        if args.baseline is None:
            parser.error("--changed needs --baseline PATH; there is no implicit baseline")
        if not args.baseline.is_file():
            sys.exit("baseline does not exist: %s" % args.baseline)
        before = {int(k, 16): v for k, v in json.loads(args.baseline.read_text()).items()}
        addrs = [a for a in entries if bodies.get(a) != before.get(a)]
        print("%d functions changed" % len(addrs))
    elif args.sample:
        selection = "sample"
        addrs = sorted(random.Random(args.seed).sample(entries, min(args.sample, len(entries))))
    else:
        selection = "all"
        addrs = entries

    if args.save_baseline is not None:
        write_exclusive(args.save_baseline,
                        json.dumps({"%08x" % a: h for a, h in bodies.items()}, sort_keys=True) + "\n")
        print("saved baseline: %s" % args.save_baseline)

    input_manifest = {"format": "recomp-diff-inputs-v1", "executable_sha256": digest,
                      "functions": ["%08x" % addr for addr in addrs],
                      "iterations": args.iterations + args.structured_inputs,
                      "seed": args.seed, "fpu_cws": args.fpu_cw}
    if args.structured_inputs:
        input_manifest["structured_inputs"] = args.structured_inputs
    if args.record_inputs:
        args.record_inputs.mkdir(parents=True, exist_ok=False)
        write_exclusive(args.record_inputs / "manifest.json", json.dumps(input_manifest, sort_keys=True))
    if args.replay_inputs:
        recorded = json.loads((args.replay_inputs / "manifest.json").read_text())
        if recorded != input_manifest:
            parser.error("replay manifest differs from selected executable, functions or input settings")

    harness = out / harness_name()
    build_harness(build_root, harness, args.game_dir, args.no_build)
    harness_digest = sha256_file(harness)

    results = run_all(addrs, args, harness, Image(exe))
    results.sort(key=lambda r: r["addr"])
    translate = cfg.get("translate", {})
    facts, boundaries = translate.get("x87_facts"), translate.get("x87_mode_metadata")
    indirect = translate.get("indirect_targets")
    analysis = translate.get("analysis_facts")
    stamp = build_root / "recomp/translate.stamp"
    run_meta = {
        "format": "recomp-diff-run-v1",
        "mode": args.mode,
        "game": cfg["game"]["id"],
        "selection": selection,
        "function_count": len(addrs),
        "functions_sha256": functions_sha256(addrs),
        "executable": str(exe),
        "executable_sha256": digest,
        "expected_executable_sha256": cfg["game"]["sha256"],
        "harness": str(harness),
        "harness_sha256": harness_digest,
        "record_inputs": str(args.record_inputs) if args.record_inputs else None,
        "replay_inputs": str(args.replay_inputs) if args.replay_inputs else None,
        "translate_stamp": stamp.read_text().strip() if stamp.is_file() else None,
        "game_toml_sha256": sha256_or_none(args.game_dir / "game.toml"),
        "code_map_sha256": tree_sha256(cfg["code_map_path"]) if cfg.get("code_map_path") else None,
        "x87_facts_sha256": sha256_or_none(args.game_dir / facts) if facts else None,
        "x87_boundaries_sha256": sha256_or_none(args.game_dir / boundaries) if boundaries else None,
        "indirect_targets_sha256": sha256_or_none(args.game_dir / indirect) if indirect else None,
        "analysis_facts_sha256": sha256_or_none(args.game_dir / analysis) if analysis else None,
        "seed": args.seed,
        "iterations": args.iterations + args.structured_inputs,
        "random_iterations": args.iterations,
        "structured_inputs": args.structured_inputs,
        "fpu_cws": ["%04x" % cw for cw in args.fpu_cw],
        "max_insns": args.max_insns,
        "timeout": args.timeout,
        "jobs": args.jobs,
        "fail_on": args.fail_on,
        "baseline": str(args.baseline) if args.baseline else None,
        "save_baseline": str(args.save_baseline) if args.save_baseline else None,
        "input_reset": {
            "stack_base": "%08x" % STACK,
            "stack_bytes": STACK_SIZE,
            "zero_stack": True,
            "reloaded": ["image writable sections", "bss", "scratch", "teb", "stack"],
        },
    }
    if len(addrs) <= 64:
        run_meta["functions"] = ["%08x" % a for a in sorted(addrs)]
    write_exclusive(out / "report.json", json.dumps(results, indent=1, sort_keys=True) + "\n")
    write_exclusive(out / "run.json", json.dumps(run_meta, indent=1, sort_keys=True) + "\n")
    counts = Counter(r["status"] for r in results)
    failed = sum(failure(r, args.fail_on) for r in results)
    print(", ".join("%d %s" % (n, s) for s, n in sorted(counts.items())))
    print("%d functions failed under --fail-on %s" % (failed, args.fail_on))
    print("report: %s" % (out / "report.json"))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
