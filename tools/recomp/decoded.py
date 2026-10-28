"""The decoded emitter: Capstone-decoded instructions lowered to plain C.

Used for the bodies the SSA emitter does not take and for the entry wrappers of
bodies whose extra entries are all switch cases. Structural analysis is inherited
from x86_frontend; decoding and normalized IR are shared frontend modules.
Every instruction is emitted eagerly: no flag liveness and
no register or x87 locals.
"""

import re

from frontend import function_ir, external_exits, table_jumps
from ir.lift import LiftError
from ir import x87_transition as x87_transition
from ir.c_x87 import Printer as X87Printer
from ir.ssa import SSAError
from program import TranslateError
from decode import Image, Insn
from x86_frontend import (ControlFlow, Function, Op as SemanticOperand,
                          REG32, REG16, REG8L, REG8H,
                          R_EAX, R_ECX, R_EDX, R_EBX, R_ESP, R_EBP, R_ESI, R_EDI,
                          parse_reg, parse_imm, parse_mem_expr, parse_operand,
                          operand_size, names_an_xmm, is_vector_insn,
                          VECTOR_REG_RE, AVX_OPERAND_RE, VECTOR_MNEM,
                          MEM_RE, IMM_RE, PTR_SIZE, SEGMENT_SELECTOR, ST_RE, MM_RE,
                          XMM_RE, TERMINATORS, STRING_MNEM,
                          seh_chain_operand, seh_zero_base, seh_restore_sites, seh_frame_sites)

#: The import-shim trampoline range from x86.h; a literal dispatch into it is
#: an import, not a missing function.
GUEST_SHIM_BASE = 0x0FF00000
GUEST_SHIM_END = 0x10000000


# --------------------------------------------------------------- registers --


#: MMX on the MMn registers and their XMM SSE2 counterparts are emitted by
#: decoded.py; unsupported vector forms still become traps at their own address.
MMX_BINARY = {
    "PADDB": ("mmx_padd", 8), "PADDW": ("mmx_padd", 16), "PADDD": ("mmx_padd", 32),
    "PADDQ": ("mmx_padd", 64),
    "PSUBB": ("mmx_psub", 8), "PSUBW": ("mmx_psub", 16), "PSUBD": ("mmx_psub", 32),
    "PSUBQ": ("mmx_psub", 64),
    "PADDSB": ("mmx_padds", 8), "PADDSW": ("mmx_padds", 16),
    "PSUBSB": ("mmx_psubs", 8), "PSUBSW": ("mmx_psubs", 16),
    "PADDUSB": ("mmx_paddus", 8), "PADDUSW": ("mmx_paddus", 16),
    "PSUBUSB": ("mmx_psubus", 8), "PSUBUSW": ("mmx_psubus", 16),
    "PMULLW": ("mmx_pmullw", 0), "PMULHW": ("mmx_pmulhw", 0), "PMADDWD": ("mmx_pmaddwd", 0),
    "PCMPEQB": ("mmx_pcmpeq", 8), "PCMPEQW": ("mmx_pcmpeq", 16), "PCMPEQD": ("mmx_pcmpeq", 32),
    "PCMPGTB": ("mmx_pcmpgt", 8), "PCMPGTW": ("mmx_pcmpgt", 16), "PCMPGTD": ("mmx_pcmpgt", 32),
    "PACKSSWB": ("mmx_packsswb", 0), "PACKSSDW": ("mmx_packssdw", 0),
    "PACKUSWB": ("mmx_packuswb", 0),
    "PMINUB": ("mmx_pminub", 0), "PMAXUB": ("mmx_pmaxub", 0),
    "PUNPCKLBW": ("mmx_punpckl", 8), "PUNPCKLWD": ("mmx_punpckl", 16),
    "PUNPCKLDQ": ("mmx_punpckl", 32),
    "PUNPCKHBW": ("mmx_punpckh", 8), "PUNPCKHWD": ("mmx_punpckh", 16),
    "PUNPCKHDQ": ("mmx_punpckh", 32),
    "PAND": ("mmx_pand", 0), "PANDN": ("mmx_pandn", 0), "POR": ("mmx_por", 0),
    "PXOR": ("mmx_pxor", 0),
}
MMX_SHIFT = {
    "PSRLW": ("mmx_psrl", 16), "PSRLD": ("mmx_psrl", 32), "PSRLQ": ("mmx_psrl", 64),
    "PSRAW": ("mmx_psra", 16), "PSRAD": ("mmx_psra", 32),
    "PSLLW": ("mmx_psll", 16), "PSLLD": ("mmx_psll", 32), "PSLLQ": ("mmx_psll", 64),
}
MMN_RE = re.compile(r"\bMM[0-7]\b")


def is_mmx_insn(mnem, ops):
    """An instruction the translator emits as MMX: an MMX mnemonic whose
    operands name an MMn register and no XMM one."""
    if mnem not in MMX_BINARY and mnem not in MMX_SHIFT and mnem not in (
            "MOVQ", "MOVD", "MOVNTQ", "PSHUFW"):
        return False
    return (any(MMN_RE.search(o) for o in ops) and not any(XMM_RE.search(o) for o in ops))


def hexlit(v):
    return "0x%xu" % (v & 0xFFFFFFFF)


def mask_of(size):
    return {8: 0xFF, 16: 0xFFFF, 32: 0xFFFFFFFF}[size]


def utype(size):
    return {8: "uint8_t", 16: "uint16_t", 32: "uint32_t"}[size]


def stype(size):
    return {8: "int8_t", 16: "int16_t", 32: "int32_t"}[size]


# ---------------------------------------------------------------- operands --

class Op(SemanticOperand):
    __slots__ = ("addr_c",)

    def __init__(self, kind, **kw):
        super().__init__(kind, **kw)
        self.addr_c = kw.get("addr_c")


# ------------------------------------------------------------ C generation --

def addr_expr(op):
    if isinstance(op, Op) and op.addr_c is not None:
        return op.addr_c
    terms = []
    if op.seg == "FS":
        terms.append("c->fs_base")
    if op.base is not None:
        terms.append("c->r[%d]" % op.base)
    if op.index is not None:
        if op.scale == 1:
            terms.append("c->r[%d]" % op.index)
        else:
            terms.append("c->r[%d] * %du" % (op.index, op.scale))
    if op.disp or not terms:
        terms.append(hexlit(op.disp))
    if len(terms) == 1:
        return terms[0]
    return "(" + " + ".join(terms) + ")"


def reg_read(idx, size, part):
    if size == 32:
        return "c->r[%d]" % idx
    if size == 16:
        return "(uint16_t)c->r[%d]" % idx
    if part == "h":
        return "(uint8_t)(c->r[%d] >> 8)" % idx
    return "(uint8_t)c->r[%d]" % idx


def reg_write(idx, size, part, value):
    if size == 32:
        return "c->r[%d] = %s;" % (idx, value)
    if size == 16:
        return "c->r[%d] = (c->r[%d] & 0xffff0000u) | ((%s) & 0xffffu);" % (idx, idx, value)
    if part == "h":
        return ("c->r[%d] = (c->r[%d] & 0xffff00ffu) | (((%s) & 0xffu) << 8);"
                % (idx, idx, value))
    return "c->r[%d] = (c->r[%d] & 0xffffff00u) | ((%s) & 0xffu);" % (idx, idx, value)


def read_op(op, size):
    if op.kind == "reg":
        return reg_read(op.reg, op.size, op.part)
    if op.kind == "imm":
        return hexlit(op.imm) if size == 32 else "0x%xu" % (op.imm & mask_of(size))
    if op.kind == "mem":
        return "rd%d(%s)" % (size, addr_expr(op))
    if op.kind == "sreg":
        return "0x%xu" % op.imm
    raise TranslateError("cannot read operand %r" % op.kind)


def write_op(op, size, value):
    if op.kind == "reg":
        return reg_write(op.reg, op.size, op.part, value)
    if op.kind == "mem":
        return "wr%d(%s, (%s)(%s));" % (size, addr_expr(op), utype(size), value)
    raise TranslateError("cannot write operand %r" % op.kind)


# ----------------------------------------------------------------- parsing --


# ------------------------------------------------------------------- flags --

COND = {
    "Z":  ("c->eflags_zf", ("zf",)),
    "NZ": ("!c->eflags_zf", ("zf",)),
    "C":  ("c->eflags_cf", ("cf",)),
    "B":  ("c->eflags_cf", ("cf",)),
    "NC": ("!c->eflags_cf", ("cf",)),
    "AE": ("!c->eflags_cf", ("cf",)),
    "A":  ("(!c->eflags_cf && !c->eflags_zf)", ("cf", "zf")),
    "BE": ("(c->eflags_cf || c->eflags_zf)", ("cf", "zf")),
    "S":  ("c->eflags_sf", ("sf",)),
    "NS": ("!c->eflags_sf", ("sf",)),
    "O":  ("c->eflags_of", ("of",)),
    "NO": ("!c->eflags_of", ("of",)),
    "P":  ("c->eflags_pf", ("pf",)),
    "NP": ("!c->eflags_pf", ("pf",)),
    "G":  ("(!c->eflags_zf && c->eflags_sf == c->eflags_of)", ("zf", "sf", "of")),
    "GE": ("c->eflags_sf == c->eflags_of", ("sf", "of")),
    "L":  ("c->eflags_sf != c->eflags_of", ("sf", "of")),
    "LE": ("(c->eflags_zf || c->eflags_sf != c->eflags_of)", ("zf", "sf", "of")),
}
# capstone spells several conditions differently from Ghidra
for _dst, _src in (("E", "Z"), ("NE", "NZ"), ("NAE", "C"), ("NB", "NC"),
                   ("NBE", "A"), ("NA", "BE"), ("NG", "LE"), ("NGE", "L"),
                   ("NL", "GE"), ("NLE", "G"), ("PE", "P"), ("PO", "NP")):
    COND[_dst] = COND[_src]

JCC = {"J" + k: v for k, v in COND.items()}
JCC["JECXZ"] = ("c->r[1] == 0", ())
JCC["JCXZ"] = ("(c->r[1] & 0xffffu) == 0", ())
# LOOP decrements ECX and branches while it is not zero; the decrement is
# the condition, so it happens whichever way the branch goes.
JCC["LOOP"] = ("(c->r[1] = c->r[1] - 1u) != 0u", ())
SETCC = {"SET" + k: v for k, v in COND.items()}
CMOVCC = {"CMOV" + k: v for k, v in COND.items()}
# FCMOVcc tests the same EFLAGS bits as the integer forms but names parity U/NU.
FCMOVCC = {"FCMOV" + k: COND[v] for k, v in (
    ("B", "B"), ("E", "E"), ("BE", "BE"), ("U", "P"),
    ("NB", "NB"), ("NE", "NE"), ("NBE", "NBE"), ("NU", "NP"))}


# -------------------------------------------------------------- translator --

NORETURN_IMPORTS = frozenset(("RaiseException", "ExitProcess", "TerminateProcess",
                              "ExitThread", "FatalAppExitA", "FatalExit"))


class Translator(ControlFlow):
    # -- emission ----------------------------------------------------------

    def translate(self, fn, entries=(), body_symbol=None):
        """Emit the body, dispatched by target address, plus one thin fn_ wrapper per entry.

        Without body_symbol the body is `void region_ADDR(X86 *, uint32_t target)`,
        with a case for every live instruction, and fn_ADDR and each entry call it.
        With body_symbol the body takes entry_ over its entries and the fallback."""
        self.call_targets = set()
        entries = sorted(set(entries))
        dead = {i for i, ins in enumerate(fn.insns) if ins.addr in fn.dead_addrs}
        live = [i for i in range(len(fn.insns)) if i not in dead]
        labels = set(fn.pushed_continuations) | set(entries)
        if any(ins.mnem == "JMP" and self.branch_target(ins) is None
               and not self.jumptables.get((fn.addr, ins.addr)) for ins in fn.insns):
            labels.update(fn.insns[i].addr for i in live)
        for i in live:
            ins = fn.insns[i]
            t = self.push_ret_target(fn, i)
            if t is not None and t in fn.index:
                labels.add(t)
            if ins.mnem in JCC or ins.mnem == "JMP":
                t = self.branch_target(ins)
                if t is not None and t in fn.index:
                    labels.add(t)
            labels.update(t for t in self.jumptables.get((fn.addr, ins.addr), []) if t in fn.index)
            if ins.mnem not in TERMINATORS and not fn.contiguous[i] and fn.fallthrough[i] in fn.index:
                labels.add(fn.fallthrough[i])
        last = fn.insns[live[-1]]
        if last.mnem not in TERMINATORS and (fn.fallthrough[live[-1]] or fn.end) in fn.index:
            labels.add(fn.fallthrough[live[-1]] or fn.end)

        region = body_symbol is None
        if region:
            labels.update(fn.insns[i].addr for i in live)
        head = fn.insns[0].addr != fn.addr
        if head:
            labels.add(fn.addr)
        out = []
        if region:
            out.append("void region_%08x(X86 *c, uint32_t target) {" % fn.addr)
            out.append("    x86_cc_settle(c);")
            if fn.seh_escapes:
                out.append("    uint64_t seh_mark_ = recomp_seh_frame_mark(c);")
            if head:
                out.append("    if (target == %s) goto L_%08x;" % (hexlit(fn.addr), fn.addr))
            out.append("    static const uint32_t addrs_[] = {")
            out.extend("        %s," % hexlit(fn.insns[i].addr) for i in live)
            out.append("    };")
            out.append("    static void *const labels_[] = {")
            out.extend("        &&L_%08x," % fn.insns[i].addr for i in live)
            out.append("    };")
            out.append("    uint32_t lo_ = 0, hi_ = %d;" % len(live))
            out.append("    while (lo_ < hi_) {")
            out.append("        uint32_t mid_ = lo_ + (hi_ - lo_) / 2;")
            out.append("        if (addrs_[mid_] < target) lo_ = mid_ + 1; else hi_ = mid_;")
            out.append("    }")
            out.append("    if (lo_ == %d || addrs_[lo_] != target) recomp_region_missing(c, target);" % len(live))
            out.append("    goto *labels_[lo_];")
        elif entries:
            out.append("RECOMP_COLD void %s(X86 *c, uint32_t entry_) {" % body_symbol)
            out.append("    x86_cc_settle(c);")
            if fn.seh_escapes:
                out.append("    uint64_t seh_mark_ = recomp_seh_frame_mark(c);")
            out.append("    switch (entry_) {")
            for e in entries:
                out.append("    case %s: goto L_%08x;" % (hexlit(e), e))
            out.append("    default: recomp_jump(c, entry_); return;")
            out.append("    }")
        else:
            out.append("void fn_%08x(X86 *c) {" % fn.addr)
            out.append("    x86_cc_settle(c);")
            if fn.seh_escapes:
                out.append("    uint64_t seh_mark_ = recomp_seh_frame_mark(c);")
            if head:
                out.append("    goto L_%08x;" % fn.addr)
        for position, i in enumerate(live):
            ins = fn.insns[i]
            if ins.addr in labels:
                out.append("L_%08x: ;" % ins.addr)
            out.append('    // %08x: %s' % (ins.addr, ' '.join(filter(None, (ins.rep, ins.mnem, ', '.join(ins.ops))))))
            emitted = self.emit(fn, i)
            if (not fn.contiguous[i] and ins.mnem not in TERMINATORS
                    and ins.mnem != "INT3" and not self.never_returns(ins)):
                t = fn.fallthrough[i]
                emitted.extend(self.goto_target(fn, t, ins))
            if position + 1 < len(live) and emitted and emitted[-1] == 'goto L_%08x;' % fn.insns[live[position + 1]].addr:
                emitted.pop()
            out.extend("    " + line for line in emitted)
        if (last.mnem not in TERMINATORS and last.mnem != "INT3"
                and not self.never_returns(last)):
            out.append("    " + " ".join(self.goto_target(fn, fn.fallthrough[live[-1]] or fn.end, last)))
        out.append("}")
        referenced = set(re.findall(r'(?:\bgoto |&&)(L_[0-9a-f]+)\b', '\n'.join(out)))
        out = [line for line in out if not (line.startswith('L_')
               and line.endswith(': ;') and line[:-3] not in referenced)]
        if region:
            for e in sorted({fn.addr, *entries}):
                out.append("void fn_%08x(X86 *c) { region_%08x(c, %s); }" % (e, fn.addr, hexlit(e)))
        return out

    # ---- helpers used by emit -------------------------------------------

    def flags_arith(self, kind, size, carry=True, a="a_", b="b_", r="r_", rf="rf_"):
        """Flag stores for ADD/ADC/SUB/SBB/CMP/INC/DEC."""
        lines = []
        if carry and kind in ("add", "sub"):
            lines.append("c->eflags_cf = (uint32_t)((%s >> %d) & 1u);" % (rf, size))
        if kind == "add":
            lines.append("c->eflags_of = (uint32_t)((~(%s ^ %s) & (%s ^ %s)) >> %d) & 1u;"
                         % (a, b, a, r, size - 1))
        else:
            lines.append("c->eflags_of = (uint32_t)(((%s ^ %s) & (%s ^ %s)) >> %d) & 1u;"
                         % (a, b, a, r, size - 1))
        lines.append("c->eflags_af = ((%s ^ %s ^ %s) >> 4) & 1u;" % (a, b, r))
        lines.append("c->eflags_zf = (%s == 0);" % r)
        lines.append("c->eflags_sf = (%s >> %d) & 1u;" % (r, size - 1))
        lines.append("c->eflags_pf = parity8(%s);" % r)
        return lines

    def flags_logic(self, size, r="r_"):
        return ["c->eflags_cf = 0;", "c->eflags_of = 0;", "c->eflags_af = 0;",
                "c->eflags_zf = (%s == 0);" % r,
                "c->eflags_sf = (%s >> %d) & 1u;" % (r, size - 1),
                "c->eflags_pf = parity8(%s);" % r]

    def rmw(self, op, size, lines):
        """For a memory destination, hoist the address into ad_ and return a
        substitute operand that reuses it."""
        if op.kind != "mem":
            return op
        lines.append("uint32_t ad_ = %s;" % addr_expr(op))
        return Op("mem", size=op.size, addr_c="ad_")

    # ---- the instruction dispatcher --------------------------------------

    def emit(self, fn, i):
        ins = fn.insns[i]
        m = ins.mnem
        # A recovered block can end on a CALL, with its last-byte estimate
        # four bytes short of the real rel32 continuation. Listing gaps can
        # also skip past it. The return address belongs to the instruction.
        nxt = fn.fallthrough[i] or (fn.insns[i + 1].addr if i + 1 < len(fn.insns) else fn.end)
        try:
            body = self._emit(fn, i, ins, m, nxt)
        except TranslateError as e:
            self.unmodelled.append((ins.addr, "%s: %s" % (ins.raw.split("  ", 1)[1], e)))
            body = ["recomp_unmodelled(c, 0x%08xu);" % ins.addr]
        declares = any(body[0].startswith(t) for t in
                       ("uint8_t ", "uint16_t ", "uint32_t ", "uint64_t ", "double "))
        if len(body) == 1 and not declares:
            return ["%s  /* %08x %s */" % (body[0], ins.addr, ins.raw.split("  ", 1)[1])]
        return (["{   /* %08x %s */" % (ins.addr, ins.raw.split("  ", 1)[1])]
                + ["    " + b for b in body] + ["}"])


    def _emit(self, fn, i, ins, m, nxt):
        if any(AVX_OPERAND_RE.search(o) for o in ins.ops or ()):
            # Never modelled, and its operands do not parse: trap it here,
            # before parsing refuses the whole function over a path CPUID
            # keeps the guest from taking.
            return ["recomp_unmodelled(c, %s); return;" % hexlit(ins.addr)]
        if m in self.STRING_MNEM and not names_an_xmm(ins):
            # Ghidra prints the implicit ES:EDI / ESI operands; they carry no
            # information the mnemonic does not already imply.
            return self.emit_string(ins, m)
        ops = [parse_operand(o) for o in ins.ops] if ins.ops else []
        L = []

        # ---------------------------------------------------------- data --
        if m == "MOV":
            size = operand_size(ops)
            dst, src = ops
            if dst.kind == "sreg":
                # A segment load means nothing in the flat model the runtime
                # provides; a load of CS is an invalid opcode on the CPU too.
                # These come from data Ghidra decoded as code.
                return ["recomp_int(c, 6u);"] if dst.imm == SEGMENT_SELECTOR["CS"] else [";"]
            if self.seh_except_entry(ins):
                L.append("if (recomp_seh_pending_target()) recomp_seh_intercept(c, %s);"
                         % hexlit(ins.addr))
            L.append(write_op(dst, size, read_op(src, size)))
            unlinks = fn.seh_sites or self.image.msvc_seh_epilog(fn.addr)
            if (unlinks and (seh_chain_operand(dst) or i in fn.seh_sites)
                    and src.kind == "reg" and src.size == 32):
                L.append("c->eip = %s;" % hexlit(ins.addr))
                if i in fn.seh_sites:
                    L.append("{ jmp_buf *b_ = recomp_seh_frame_enter(c); "
                             "if (RECOMP_SETJMP(*b_)) { recomp_seh_land(c); return; } }")
                else:
                    L.append("recomp_seh_frame_leave(c);")
            return L

        if m == "LEA":
            dst, src = ops
            if src.kind != "mem":
                raise TranslateError("LEA without memory operand")
            L.append(write_op(dst, 32, addr_expr(src)))
            return L

        if m in ("LDS", "LES"):
            dst, src = ops
            if src.kind != "mem" or dst.size != 32:
                raise TranslateError("%s without a 32-bit register and memory operand" % m)
            L.append("uint32_t a_ = %s; uint32_t v_ = rd32(a_); (void)rd16(a_ + 4u);" % addr_expr(src))
            L.append(write_op(dst, 32, "v_"))
            return L

        if m in ("MOVSX", "MOVZX"):
            dst, src = ops
            ssize = src.size if src.kind == "mem" and src.size else (
                src.size if src.kind == "reg" else None)
            if ssize is None:
                raise TranslateError("%s: unknown source size" % m)
            if m == "MOVSX":
                val = "(uint32_t)(int32_t)(%s)(%s)" % (stype(ssize), read_op(src, ssize))
            else:
                val = "(uint32_t)(%s)" % read_op(src, ssize)
            L.append(write_op(dst, dst.size, val))
            return L

        if m == "XCHG":
            size = operand_size(ops)
            a, b = ops
            a = self.rmw(a, size, L)
            L.append("uint32_t t_ = %s;" % read_op(a, size))
            L.append(write_op(a, size, read_op(b, size)))
            L.append(write_op(b, size, "t_"))
            return L

        if m == "CMPXCHG":
            # The guest is single-threaded, so LOCK needs no host atomic.
            # Flags come from accumulator - destination on either path.
            size = operand_size(ops)
            dst, src = ops
            dst = self.rmw(dst, size, L)
            L.append("uint32_t a_ = c->r[0] & %s, b_ = %s;"
                     % (hexlit(mask_of(size)), read_op(dst, size)))
            L.append("uint64_t rf_ = (uint64_t)a_ - b_;")
            L.append("uint32_t r_ = (uint32_t)rf_ & %s;" % hexlit(mask_of(size)))
            L.append("if (a_ == b_) { %s }" % write_op(dst, size, read_op(src, size)))
            L.append("else { %s }" % write_op(Op("reg", reg=0, size=size, part=None), size, "b_"))
            L.extend(self.flags_arith("sub", size))
            return L

        if m == "CMPXCHG8B":
            # LOCK needs no host atomic under the cooperative guest scheduler.
            # Capture the guest address before the failure path changes EDX:EAX.
            L.append("uint32_t ad_ = %s;" % addr_expr(ops[0]))
            L.append("uint64_t dst_ = rd64(ad_);")
            L.append("uint64_t expected_ = ((uint64_t)c->r[2] << 32) | c->r[0];")
            L.append("c->eflags_zf = (expected_ == dst_);")
            L.append("if (c->eflags_zf) { wr64(ad_, ((uint64_t)c->r[1] << 32) | c->r[3]); }")
            L.append("else { c->r[0] = (uint32_t)dst_; c->r[2] = (uint32_t)(dst_ >> 32); }")
            return L

        if m == "XADD":
            size = operand_size(ops)
            dst, src = ops
            dst = self.rmw(dst, size, L)
            L.append("uint32_t a_ = %s, b_ = %s;" % (read_op(dst, size), read_op(src, size)))
            L.append("uint64_t rf_ = (uint64_t)a_ + b_;")
            L.append("uint32_t r_ = (uint32_t)rf_ & %s;" % hexlit(mask_of(size)))
            L.append(write_op(src, size, "a_"))
            L.append(write_op(dst, size, "r_"))
            L.extend(self.flags_arith("add", size))
            return L

        if m == "PUSH":
            size = 32 if ops[0].kind == "sreg" else operand_size(ops, hint=32)
            if size == 16:
                L.append("uint32_t v_ = %s;" % read_op(ops[0], 16))
                L.append("c->r[4] -= 2; wr16(c->r[4], (uint16_t)v_);")
                return L
            if size != 32:
                raise TranslateError("non-32-bit PUSH")
            L.append("uint32_t v_ = %s;" % read_op(ops[0], 32))
            L.append("c->r[4] -= 4; wr32(c->r[4], v_);")
            t = self.push_ret_target(fn, i)
            if t is not None and t not in fn.pushed_continuations:
                # Preserve the guest stack write even though the pair has no
                # net stack effect, then take the RET's guest continuation.
                L.append("c->eip = v_; c->r[4] += 4;")
                L.extend(self.goto_target(fn, t, ins))
            return L

        if m == "POP":
            if ops[0].kind == "sreg":
                return ["c->r[4] += 4;"]
            size = operand_size(ops, hint=32)
            if size == 16:
                L.append("uint32_t v_ = rd16(c->r[4]); c->r[4] += 2;")
                L.append(write_op(ops[0], 16, "v_"))
                return L
            if size != 32:
                raise TranslateError("non-32-bit POP")
            L.append("uint32_t v_ = rd32(c->r[4]); c->r[4] += 4;")
            L.append(write_op(ops[0], 32, "v_"))
            if i in fn.seh_restores:
                L.append("c->eip = %s; recomp_seh_frame_leave(c);" % hexlit(ins.addr))
            return L

        if m == "PUSHAD":
            return ["x86_pushad(c);"]
        if m == "POPAD":
            return ["x86_popad(c);"]
        if m == "PUSHFD":
            return ["c->r[4] -= 4; wr32(c->r[4], x86_get_eflags(c));"]
        if m == "POPFD":
            return ["x86_set_eflags(c, rd32(c->r[4])); c->r[4] += 4;"]
        if m == "PUSHF":
            return ["c->r[4] -= 2; wr16(c->r[4], (uint16_t)x86_get_eflags(c));"]
        if m == "POPF":
            return ["x86_set_eflags(c, (x86_get_eflags(c) & 0xffff0000u) | rd16(c->r[4]));",
                    "c->r[4] += 2;"]
        if m == "LEAVE":
            return ["c->r[4] = c->r[5]; c->r[5] = rd32(c->r[4]); c->r[4] += 4;"]
        if m == "SAHF":
            return ["x86_sahf(c);"]
        if m == "LAHF":
            # AH = SF:ZF:0:AF:0:PF:1:CF
            return ["c->r[0] = (c->r[0] & 0xffff00ffu) | "
                    "(((x86_get_eflags(c) & 0xd5u) | 0x02u) << 8);"]
        if m == "XLAT":
            return ["xlat(c);"]
        if m == "BSWAP":
            return [write_op(ops[0], 32, "bswap32(%s)" % read_op(ops[0], 32))]

        if m == "CDQ":
            return ["c->r[2] = (uint32_t)((int32_t)c->r[0] >> 31);"]
        if m == "CWD":
            return ["c->r[2] = (c->r[2] & 0xffff0000u) | "
                    "((uint32_t)((int16_t)c->r[0] >> 15) & 0xffffu);"]
        if m == "CBW":
            return ["c->r[0] = (c->r[0] & 0xffff0000u) | "
                    "((uint32_t)(int32_t)(int8_t)c->r[0] & 0xffffu);"]
        if m == "CWDE":
            return ["c->r[0] = (uint32_t)(int32_t)(int16_t)c->r[0];"]
        if m == "STMXCSR":
            return ["recomp_stmxcsr(c, %s);" % addr_expr(ops[0])]
        if m == "LDMXCSR":
            return ["recomp_ldmxcsr(c, %s, %s);" % (hexlit(ins.addr), addr_expr(ops[0]))]

        # ------------------------------------------------------- arith ----
        if m in ("ADD", "ADC", "SUB", "SBB", "CMP"):
            size = operand_size(ops)
            dst, src = ops
            dst = self.rmw(dst, size, L)
            L.append("uint32_t a_ = %s, b_ = %s;" % (read_op(dst, size), read_op(src, size)))
            if m in ("ADD", "ADC"):
                carry = " + c->eflags_cf" if m == "ADC" else ""
                L.append("uint64_t rf_ = (uint64_t)a_ + b_%s;" % carry)
                kind = "add"
            else:
                carry = " - c->eflags_cf" if m == "SBB" else ""
                L.append("uint64_t rf_ = (uint64_t)a_ - b_%s;" % carry)
                kind = "sub"
            L.append("uint32_t r_ = (uint32_t)rf_ & %s;" % hexlit(mask_of(size)))
            if m != "CMP":
                L.append(write_op(dst, size, "r_"))
            L += self.flags_arith(kind, size)
            return L

        if m in ("INC", "DEC"):
            size = operand_size(ops)
            dst = self.rmw(ops[0], size, L)
            L.append("uint32_t a_ = %s, b_ = 1u;" % read_op(dst, size))
            L.append("uint64_t rf_ = (uint64_t)a_ %s b_;" % ("+" if m == "INC" else "-"))
            L.append("uint32_t r_ = (uint32_t)rf_ & %s;" % hexlit(mask_of(size)))
            L.append(write_op(dst, size, "r_"))
            L += self.flags_arith("add" if m == "INC" else "sub", size, carry=False)
            return L

        if m == "NEG":
            size = operand_size(ops)
            dst = self.rmw(ops[0], size, L)
            L.append("uint32_t b_ = %s, a_ = 0u;" % read_op(dst, size))
            L.append("uint64_t rf_ = (uint64_t)a_ - b_;")
            L.append("uint32_t r_ = (uint32_t)rf_ & %s;" % hexlit(mask_of(size)))
            L.append(write_op(dst, size, "r_"))
            L += self.flags_arith("sub", size)
            return L

        if m == "NOT":
            size = operand_size(ops)
            dst = self.rmw(ops[0], size, L)
            L.append(write_op(dst, size, "~%s" % read_op(dst, size)))
            return L

        if m in ("AND", "OR", "XOR", "TEST"):
            size = operand_size(ops)
            dst, src = ops
            cop = {"AND": "&", "OR": "|", "XOR": "^", "TEST": "&"}[m]
            dst2 = self.rmw(dst, size, L) if m != "TEST" else dst
            L.append("uint32_t r_ = (%s %s %s) & %s;"
                     % (read_op(dst2, size), cop, read_op(src, size), hexlit(mask_of(size))))
            if m != "TEST":
                L.append(write_op(dst2, size, "r_"))
            L += self.flags_logic(size)
            return L

        # ------------------------------------------------- shift / rotate --
        if m in ("SHL", "SHR", "SAR", "ROL", "ROR", "RCL", "RCR"):
            size = ops[0].size or operand_size(ops, hint=32)
            dst = self.rmw(ops[0], size, L)
            cnt = read_op(ops[1], 32) if len(ops) > 1 else "1u"
            if len(ops) > 1 and ops[1].kind == "reg":
                cnt = "(uint32_t)%s" % read_op(ops[1], ops[1].size)
            fn_base = m.lower() + str(size)
            call = "%s_f(c, %s, %s)" % (fn_base, read_op(dst, size), cnt)
            L.append(write_op(dst, size, call))
            return L

        if m in ("SHLD", "SHRD"):
            size = operand_size(ops[:2])
            if size != 32:
                raise TranslateError("%s at width %d" % (m, size))
            dst = self.rmw(ops[0], 32, L)
            cnt = ("(uint32_t)%s" % read_op(ops[2], ops[2].size)
                   if ops[2].kind == "reg" else read_op(ops[2], 32))
            L.append(write_op(dst, 32, "%s32_f(c, %s, %s, %s)"
                              % (m.lower(), read_op(dst, 32), read_op(ops[1], 32), cnt)))
            return L

        # -------------------------------------------------- mul  /  div ---
        if m == "IMUL" and len(ops) >= 2:
            size = operand_size(ops[:2])
            if len(ops) == 2:
                a, b = read_op(ops[0], size), read_op(ops[1], size)
            else:
                a, b = read_op(ops[1], size), read_op(ops[2], size)
            L.append(write_op(ops[0], size, "imul2_%d_f(c, %s, %s)" % (size, a, b)))
            return L

        if m in ("MUL", "IMUL", "DIV", "IDIV"):
            size = operand_size(ops)
            src = read_op(ops[0], size)
            base = m.lower() + str(size)
            if m in ("DIV", "IDIV"):
                L.append("%s(c, (%s)(%s), %s);" % (base, utype(size), src, hexlit(ins.addr)))
            else:
                L.append("%s(c, (%s)(%s));" % (base, utype(size), src))
            return L

        # ------------------------------------------------------- bit ops --
        if m in ("BT", "BTS", "BTR", "BTC"):
            size = operand_size(ops, hint=32)
            dst, src = ops
            idx = read_op(src, size)
            if dst.kind == "mem":
                L.append("uint32_t bi_ = %s;" % idx)
                L.append("uint32_t ad_ = %s + 4u * (bi_ >> 5);" % addr_expr(dst))
                dst = Op("mem", size=32, addr_c="ad_")
                bit = "(bi_ & 31u)"
            else:
                L.append("uint32_t bi_ = (%s) & %du;" % (idx, size - 1))
                bit = "bi_"
            L.append("uint32_t v_ = %s;" % read_op(dst, size))
            L.append("c->eflags_cf = (v_ >> %s) & 1u;" % bit)
            if m == "BTS":
                L.append(write_op(dst, size, "v_ | (1u << %s)" % bit))
            elif m == "BTR":
                L.append(write_op(dst, size, "v_ & ~(1u << %s)" % bit))
            elif m == "BTC":
                L.append(write_op(dst, size, "v_ ^ (1u << %s)" % bit))
            return L

        if m in ("BSR", "BSF"):
            # With a zero source the destination is architecturally undefined;
            # unicorn (and AMD) leave it unchanged, so pass the old value in.
            L.append(write_op(ops[0], 32, "%s32_f(c, %s, %s)"
                              % (m.lower(), read_op(ops[0], 32), read_op(ops[1], 32))))
            return L

        if m in SETCC:
            cond = SETCC[m][0]
            L.append(write_op(ops[0], 8, "(%s) ? 1u : 0u" % cond))
            return L

        if m in CMOVCC:
            size = operand_size(ops)
            cond = CMOVCC[m][0]
            L.append("if (%s) { %s }" % (cond, write_op(ops[0], size,
                                                        read_op(ops[1], size))))
            return L

        if m == "CLD":
            return ["c->eflags_df = 0;"]
        if m == "STD":
            return ["c->eflags_df = 1;"]
        if m == "CLC":
            return ["c->eflags_cf = 0;"]
        if m == "STC":
            return ["c->eflags_cf = 1;"]
        if m == "CMC":
            return ["c->eflags_cf = !c->eflags_cf;"]

        # ---------------------------------------------------- control flow --
        if m in JCC:
            cond = JCC[m][0]
            t = self.branch_target(ins)
            if t is None:
                raise TranslateError("indirect conditional jump")
            return ["if (%s) { %s }" % (cond, " ".join(self.goto_target(fn, t, ins)))]

        if m == "JMP":
            t = self.branch_target(ins)
            if t is not None:
                return self.goto_target(fn, t, ins)
            return self.emit_indirect_jump(fn, i, ins, ops[0])

        if m == "CALL":
            if ins.ops and ins.ops[0].startswith("0x"):
                t = int(ins.ops[0], 16)
                L.append("c->r[4] -= 4; wr32(c->r[4], %s);" % hexlit(nxt))
                if t in self.func_addrs:
                    if (self.image.import_thunk_name(t) or "").lstrip("_") == "setjmp3":
                        L.append("RECOMP_SETJMP3(c);")
                    else:
                        self.call_targets.add(t)
                        L.append("CALL_FN(%08x);" % t)
                else:
                    self.reject_offimage_call(t)
                    L.append("recomp_call(c, %s);" % hexlit(t))
                # An SSA callee may return with a pending flags descriptor; the
                # decoded caller reads the guest's fields directly.
                L.append("x86_cc_settle(c);")
                if t in self.seh_helpers:
                    L.append("c->eip = %s;" % hexlit(ins.addr))
                    L.append("{ jmp_buf *b_ = recomp_seh_frame_adopt(c); "
                             "if (b_) { if (RECOMP_SETJMP(*b_)) { recomp_seh_land(c); return; } } }")
                if t in self.noreturn_callees:
                    # The callee throws or exits; what follows is padding and
                    # tables, never code.  Reaching this line means it came
                    # back after all, which the runtime then reports by address.
                    L.append("recomp_unknown_call(c, %s); return;" % hexlit(nxt))
                return L
            L.append("uint32_t t_ = %s;" % read_op(ops[0], 32))
            L.append("c->r[4] -= 4; wr32(c->r[4], %s);" % hexlit(nxt))
            L.append("recomp_icall(c, t_, &ic_%08x, %s);" % (ins.addr, hexlit(ins.addr)))
            L.append("x86_cc_settle(c);")
            return L

        if m == "RET":
            n = parse_imm(ins.ops[0]) if ins.ops else 0
            orphan = "recomp_seh_frame_orphan(c, seh_mark_); " if i in fn.seh_escapes else ""
            if fn.pushed_continuations:
                # Normal finally cleanup stays in the establishing C frame.
                # An alternate entry called by the exception dispatcher has
                # its own return address and takes the default arm instead.
                L = ["uint32_t r_ = rd32(c->r[4]); c->r[4] += %du;" % (4 + n),
                     "switch (r_) {"]
                L.extend("case %s: goto L_%08x;" % (hexlit(t), t)
                         for t in sorted(fn.pushed_continuations))
                L.extend(["default: c->eip = r_; " + orphan + "recomp_return(c); return;", "}"])
                return L
            return ["c->eip = rd32(c->r[4]); c->r[4] += %du; " % (4 + n) +
                    orphan + "recomp_return(c); return;"]

        # ------------------------------------------------------------ SSE --
        # Data movement only, in dword lanes. A Delphi runtime's FillChar and
        # Move reach for these unconditionally - SSE2 predates every CPU the
        # compiler supports, so there is no feature test to fail - while the
        # AVX forms beside them are gated on a CPUID bit this kit does not
        # set, and stay traps nobody reaches.
        # MMX first: is_mmx_insn only matches an MMn operand with no XMM one,
        # so the SSE2 path below still takes every xmm form of MOVQ/MOVD.
        if is_mmx_insn(m, ins.ops):
            return self.emit_mmx(ins, m)
        # Lane-wise integer, logical and unpack forms. Every one of them has
        # the same shape: both operands are read in full before either lane of
        # the destination is written, because a destination is commonly also
        # the source and these are not sequences of independent moves.
        SSE_LANE_OPS = {"PXOR": "^", "XORPD": "^", "XORPS": "^", "PAND": "&", "ANDPD": "&",
                        "ANDPS": "&", "POR": "|", "ORPD": "|", "ORPS": "|"}
        SSE_LANE_FORMS = ("PANDN", "ANDNPD", "ANDNPS", "PCMPEQD", "PUNPCKLDQ", "PUNPCKHDQ",
                          "PUNPCKLQDQ", "PUNPCKHQDQ", "UNPCKLPD", "UNPCKHPD", "UNPCKLPS",
                          "UNPCKHPS", "MOVDDUP", "MOVLPD", "MOVLPS", "MOVHPD", "MOVHPS",
                          "MOVLHPS", "MOVHLPS", "SHUFPS", "SHUFPD", "MOVAPD", "MOVUPD")
        if m in SSE_LANE_OPS or m in SSE_LANE_FORMS:
            dst, src = ops[0], ops[1]

            def lane(op, i):
                if op.kind == "xmm":
                    return "c->xmm[%d][%d]" % (op.reg, i)
                return "rd32(%s + %du)" % (addr_expr(op), 4 * i)

            def put(op, i, value):
                if op.kind == "xmm":
                    return "c->xmm[%d][%d] = %s;" % (op.reg, i, value)
                return "wr32(%s + %du, %s);" % (addr_expr(op), 4 * i, value)

            # d0_..d3_ and s0_..s3_ hold the operands as they were on entry.
            # A half-width form reads only the half it uses.
            halves = 2 if m in ("MOVLPD", "MOVLPS", "MOVHPD", "MOVHPS", "MOVLHPS") else 4
            L.extend("uint32_t d%d_ = %s;" % (i, lane(dst, i)) for i in range(halves))
            L.extend("uint32_t s%d_ = %s;" % (i, lane(src, i)) for i in range(halves))
            d = lambda i: "d%d_" % i
            sv = lambda i: "s%d_" % i

            if m in SSE_LANE_OPS:
                L.extend(put(dst, i, "%s %s %s" % (d(i), SSE_LANE_OPS[m], sv(i)))
                         for i in range(4))
            elif m in ("PANDN", "ANDNPD", "ANDNPS"):
                L.extend(put(dst, i, "(~%s) & %s" % (d(i), sv(i))) for i in range(4))
            elif m == "PCMPEQD":
                L.extend(put(dst, i, "%s == %s ? 0xffffffffu : 0u" % (d(i), sv(i)))
                         for i in range(4))
            elif m in ("MOVAPD", "MOVUPD"):   # MOVAPS and MOVUPS under another name
                L.extend(put(dst, i, sv(i)) for i in range(4))
            elif m in ("PUNPCKLDQ", "UNPCKLPS"):   # d0 s0 d1 s1
                L.extend([put(dst, 0, d(0)), put(dst, 1, sv(0)),
                          put(dst, 2, d(1)), put(dst, 3, sv(1))])
            elif m in ("PUNPCKHDQ", "UNPCKHPS"):   # d2 s2 d3 s3
                L.extend([put(dst, 0, d(2)), put(dst, 1, sv(2)),
                          put(dst, 2, d(3)), put(dst, 3, sv(3))])
            elif m in ("PUNPCKLQDQ", "UNPCKLPD"):   # d0 d1 s0 s1
                L.extend([put(dst, 2, sv(0)), put(dst, 3, sv(1))])
            elif m in ("UNPCKHPD", "PUNPCKHQDQ"):   # d2 d3 s2 s3
                L.extend([put(dst, 0, d(2)), put(dst, 1, d(3)),
                          put(dst, 2, sv(2)), put(dst, 3, sv(3))])
            elif m == "MOVDDUP":                    # s0 s1 s0 s1
                L.extend([put(dst, 0, sv(0)), put(dst, 1, sv(1)),
                          put(dst, 2, sv(0)), put(dst, 3, sv(1))])
            elif m == "MOVLHPS":
                # Raw low source lanes replace the high destination lanes.
                # Snapshot the source before stores, including dst == src.
                L.extend(put(dst, 2 + i, sv(i)) for i in range(2))
            elif m == "MOVHLPS":
                L.extend(put(dst, i, sv(2 + i)) for i in range(2))
            elif m in ("MOVLPD", "MOVLPS", "MOVHPD", "MOVHPS"):
                # One 64-bit half moves; the other half keeps what it had.
                half = 0 if m in ("MOVLPD", "MOVLPS") else 2
                if dst.kind == "xmm":
                    L.extend(put(dst, half + i, sv(i)) for i in range(2))
                else:
                    L.extend(put(dst, i, "c->xmm[%d][%d]" % (src.reg, half + i))
                             for i in range(2))
            elif m == "SHUFPS":
                # Lanes 0 and 1 come from the destination, 2 and 3 from the source.
                sel = parse_imm(ins.ops[2])
                L.extend([put(dst, 0, d(sel & 3)), put(dst, 1, d((sel >> 2) & 3)),
                          put(dst, 2, sv((sel >> 4) & 3)), put(dst, 3, sv((sel >> 6) & 3))])
            else:  # SHUFPD: one double from each operand
                sel = parse_imm(ins.ops[2])
                lo, hi = 2 * (sel & 1), 2 * ((sel >> 1) & 1)
                L.extend([put(dst, 0, d(lo)), put(dst, 1, d(lo + 1)),
                          put(dst, 2, sv(hi)), put(dst, 3, sv(hi + 1))])
            return L

        SSE_PACKED_FLOAT = ("ADDPS", "SUBPS", "MULPS", "DIVPS", "MINPS", "MAXPS")
        SSE_PACKED_UNARY = ("SQRTPS", "RCPPS", "RSQRTPS")
        SSE_CMP_PS = {"CMPEQPS": 0, "CMPLTPS": 1, "CMPLEPS": 2, "CMPUNORDPS": 3,
                      "CMPNEQPS": 4, "CMPNLTPS": 5, "CMPNLEPS": 6, "CMPORDPS": 7}
        if m in SSE_PACKED_FLOAT or m in SSE_PACKED_UNARY or m in SSE_CMP_PS or m == "MOVMSKPS":
            def lane(op, i):
                if op.kind == "xmm":
                    return "c->xmm[%d][%d]" % (op.reg, i)
                return "rd32(%s + %du)" % (addr_expr(op), 4 * i)

            dst, src = ops[0], ops[1]
            if m in SSE_PACKED_FLOAT:
                helper = "recomp_sse_%s(%s" % (m.lower(), "" if m in ("MINPS", "MAXPS") else "c, ")
                L.extend("c->xmm[%d][%d] = %sc->xmm[%d][%d], %s);"
                         % (dst.reg, i, helper, dst.reg, i, lane(src, i)) for i in range(4))
            elif m in SSE_PACKED_UNARY:
                helper = "recomp_sse_%s" % m.lower()
                L.extend("c->xmm[%d][%d] = %s(%s);"
                         % (dst.reg, i, helper, lane(src, i)) for i in range(4))
            elif m in SSE_CMP_PS:
                pred = SSE_CMP_PS[m]
                L.extend("c->xmm[%d][%d] = recomp_sse_cmpps(%du, c->xmm[%d][%d], %s);"
                         % (dst.reg, i, pred, dst.reg, i, lane(src, i)) for i in range(4))
            else:
                L.append(write_op(dst, 32,
                                  " | ".join("(((c->xmm[%d][%d] >> 31) & 1u) << %d)"
                                             % (src.reg, i, i) for i in range(4))))
            return L

        # Scalar double and single forms. The host's double and float are the
        # same IEEE formats in the same rounding mode, and scalar SSE leaves
        # the lanes above its result alone, which is why nothing here clears
        # them except a load from memory, where the hardware does.
        SSE_SCALAR_MATH = {"ADDSD": "+", "SUBSD": "-", "MULSD": "*", "DIVSD": "/",
                           "ADDSS": "+", "SUBSS": "-", "MULSS": "*", "DIVSS": "/"}
        SSE_SCALAR_FORMS = ("MOVSD", "MOVSS", "SQRTSD", "SQRTSS", "MINSD", "MAXSD",
                            "MINSS", "MAXSS", "CVTSI2SD", "CVTSI2SS", "CVTTSD2SI",
                            "CVTTSS2SI", "CVTSS2SI", "CVTSD2SI", "CVTSD2SS", "CVTSS2SD", "COMISD", "UCOMISD",
                            "COMISS", "UCOMISS", "RCPSS", "RSQRTSS")
        # Only MOVSD and MOVSS are shared with another instruction; the rest
        # of these mnemonics are SSE whatever their operands are, and a
        # conversion's source or destination is often memory or a register.
        if (m in SSE_SCALAR_MATH
                or (m in SSE_SCALAR_FORMS
                    and (m not in ("MOVSD", "MOVSS") or names_an_xmm(ins)))):
            dst, src = ops[0], ops[1]
            # Which host type this form works in. The conversions name both,
            # so they spell their own operands out instead.
            single = ((m.endswith("SS") and m != "CVTSD2SS")
                      or m in ("CVTTSS2SI", "CVTSS2SI"))

            def scalar(op, as_single):
                if op.kind == "xmm":
                    return "xmm_f32(c, %d)" % op.reg if as_single else "xmm_f64(c, %d)" % op.reg
                return "rdf32(%s)" % addr_expr(op) if as_single else "rdf64(%s)" % addr_expr(op)

            def store_scalar(op, value, as_single):
                if op.kind == "xmm":
                    return ("xmm_set_f32(c, %d, %s);" if as_single else "xmm_set_f64(c, %d, %s);") \
                        % (op.reg, value)
                return ("wrf32(%s, %s);" if as_single else "wrf64(%s, %s);") \
                    % (addr_expr(op), value)

            if m in SSE_SCALAR_MATH:
                L.append(store_scalar(dst, "%s %s %s" % (scalar(dst, single), SSE_SCALAR_MATH[m],
                                                         scalar(src, single)), single))
            elif m in ("SQRTSD", "SQRTSS"):
                L.append(store_scalar(dst, "%s(%s)" % ("recomp_sse_sqrtf" if single
                                                       else "recomp_sse_sqrt",
                                                       scalar(src, single)), single))
            elif m in ("RCPSS", "RSQRTSS"):
                L.append(store_scalar(dst, "recomp_sse_%s(%s)" % (m.lower(), scalar(src, True)),
                                      True))
            elif m in ("MINSD", "MAXSD", "MINSS", "MAXSS"):
                # The hardware returns its second operand when either is a NaN
                # or both are zero, which is what this spelling does.
                t = "float" if single else "double"
                keep = "<" if m in ("MINSD", "MINSS") else ">"
                L.append("{ %s a_ = %s, b_ = %s; %s }"
                         % (t, scalar(dst, single), scalar(src, single),
                            store_scalar(dst, "a_ %s b_ ? a_ : b_" % keep, single)))
            elif m in ("COMISD", "UCOMISD", "COMISS", "UCOMISS"):
                L.append("recomp_comis(c, %s, %s);" % (scalar(dst, single), scalar(src, single)))
            elif m in ("CVTSI2SD", "CVTSI2SS"):
                L.append(store_scalar(dst, "(%s)(int32_t)%s" % ("float" if single else "double",
                                                                read_op(src, 32)), single))
            elif m in ("CVTTSD2SI", "CVTTSS2SI"):
                value = ("recomp_sse_cvttss2si(%s)" % scalar(src, True) if single
                         else "(int32_t)%s" % scalar(src, False))
                L.append(write_op(dst, 32, "(uint32_t)%s" % value))
            elif m in ("CVTSS2SI", "CVTSD2SI"):
                helper = "recomp_sse_cvtss2si" if single else "recomp_sse_cvtsd2si"
                L.append(write_op(dst, 32, "(uint32_t)%s(c, %s)" % (helper, scalar(src, single))))
            elif m == "CVTSD2SS":
                L.append(store_scalar(dst, "(float)%s" % scalar(src, False), True))
            elif m == "CVTSS2SD":
                L.append(store_scalar(dst, "(double)%s" % scalar(src, True), False))
            else:  # MOVSD, MOVSS between registers or memory
                L.append(store_scalar(dst, scalar(src, single), single))
                if dst.kind == "xmm" and src.kind != "xmm":
                    # A load clears what is above the value; a move between
                    # registers keeps it.
                    L.extend("c->xmm[%d][%d] = 0u;" % (dst.reg, i)
                             for i in range(1 if single else 2, 4))
            return L

        if m == "CVTPI2PS":
            dst, src = ops
            if dst.kind != "xmm":
                raise TranslateError("CVTPI2PS destination is not an XMM register")
            if src.kind == "mm":
                L.append("uint64_t s_ = c->mm[%d];" % src.reg)
            elif src.kind == "mem":
                L.append("uint64_t s_ = rd64(%s);" % addr_expr(src))
            else:
                raise TranslateError("CVTPI2PS source is not MMX or memory")
            L.append("c->xmm[%d][0] = recomp_sse_f32_bits((float)(int32_t)(uint32_t)s_);"
                     % dst.reg)
            L.append("c->xmm[%d][1] = recomp_sse_f32_bits((float)(int32_t)(uint32_t)(s_ >> 32));"
                     % dst.reg)
            return L

        if m == "CVTPS2PI":
            dst, src = ops
            if dst.kind != "mm":
                raise TranslateError("CVTPS2PI destination is not an MMX register")
            if src.kind == "xmm":
                lo, hi = "c->xmm[%d][0]" % src.reg, "c->xmm[%d][1]" % src.reg
            elif src.kind == "mem":
                lo, hi = "rd32(%s)" % addr_expr(src), "rd32(%s + 4u)" % addr_expr(src)
            else:
                raise TranslateError("CVTPS2PI source is not XMM or memory")
            L.append("c->mm[%d] = (uint64_t)(uint32_t)recomp_sse_cvtss2si(c, recomp_sse_f32(%s))"
                     " | ((uint64_t)(uint32_t)recomp_sse_cvtss2si(c, recomp_sse_f32(%s)) << 32);"
                     % (dst.reg, lo, hi))
            return L

        if m == "CVTTPS2DQ":
            dst, src = ops
            if dst.kind != "xmm":
                raise TranslateError("CVTTPS2DQ destination is not an XMM register")

            def lane(op, i):
                if op.kind == "xmm":
                    return "c->xmm[%d][%d]" % (op.reg, i)
                return "rd32(%s + %du)" % (addr_expr(op), 4 * i)

            L.extend("c->xmm[%d][%d] = (uint32_t)recomp_sse_cvttss2si(recomp_sse_f32(%s));"
                     % (dst.reg, i, lane(src, i)) for i in range(4))
            return L

        if m == "PEXTRW":
            dst, src = ops[0], ops[1]
            if src.kind != "xmm":
                raise TranslateError("PEXTRW source is not an XMM register")
            sel = parse_imm(ins.ops[2]) & 7
            value = "((c->xmm[%d][%d] >> %du) & 0xffffu)" % (src.reg, sel >> 1, (sel & 1) * 16)
            if dst.kind == "mem":
                L.append(write_op(dst, 16, value))
            else:
                L.append(write_op(dst, 32, "(uint32_t)%s" % value))
            return L

        if m == "MOVDQ2Q":
            dst, src = ops
            if dst.kind != "mm" or src.kind != "xmm":
                raise TranslateError("MOVDQ2Q form")
            L.append("c->mm[%d] = (uint64_t)c->xmm[%d][0] | ((uint64_t)c->xmm[%d][1] << 32);"
                     % (dst.reg, src.reg, src.reg))
            return L

        if m == "PSRLDQ":
            dst, src = ops
            if dst.kind != "xmm" or src.kind != "imm":
                raise TranslateError("PSRLDQ form")
            L.append("xmm_psrldq(c->xmm[%d], %du);" % (dst.reg, parse_imm(ins.ops[1]) & 0xff))
            return L

        if m in MMX_BINARY or m in MMX_SHIFT:
            return self.emit_sse_packed(ins, m, ops)

        if m in ("MOVUPS", "MOVAPS", "MOVDQU", "MOVDQA", "MOVNTPS", "MOVQ", "MOVD", "PSHUFD"):
            def lane(op, i):
                if op.kind == "xmm":
                    return "c->xmm[%d][%d]" % (op.reg, i)
                return "rd32(%s + %du)" % (addr_expr(op), 4 * i)

            def put(op, i, value):
                if op.kind == "xmm":
                    return "c->xmm[%d][%d] = %s;" % (op.reg, i, value)
                return "wr32(%s + %du, %s);" % (addr_expr(op), 4 * i, value)

            dst, src = ops[0], ops[1]
            if m == "PSHUFD":
                sel = parse_imm(ins.ops[2])
                # Read every lane before writing one: the destination is
                # commonly the source, and a shuffle is not a sequence of
                # independent moves.
                L.extend("uint32_t s%d_ = %s;" % (i, lane(src, i)) for i in range(4))
                L.extend(put(dst, i, "s%d_" % ((sel >> (2 * i)) & 3)) for i in range(4))
                return L
            if m == "MOVD":
                # A dword between an XMM lane and a general register or memory,
                # zeroing what is above it when the XMM side is written.
                if dst.kind == "xmm":
                    L.append(put(dst, 0, read_op(src, 32)))
                    L.extend(put(dst, i, "0u") for i in range(1, 4))
                    return L
                L.append(write_op(dst, 32, lane(src, 0)))
                return L
            lanes = 2 if m == "MOVQ" else 4
            L.extend("uint32_t s%d_ = %s;" % (i, lane(src, i)) for i in range(lanes))
            L.extend(put(dst, i, "s%d_" % i) for i in range(lanes))
            # MOVQ into a register clears the upper half; into memory it
            # writes eight bytes and stops.
            if m == "MOVQ" and dst.kind == "xmm":
                L.extend(put(dst, i, "0u") for i in range(2, 4))
            return L
        if m in ("SFENCE", "LFENCE", "MFENCE", "VZEROUPPER", "VZEROALL", "PREFETCHNTA",
                 "PREFETCHT0", "PREFETCHT1", "PREFETCHT2"):
            # Ordering and cache hints on a machine with one guest thread of
            # execution at a time, and no AVX state to clear.
            return [";"]

        # --------------------------------------------------------- system --
        if m == "RDTSC":
            return ["recomp_rdtsc(c);"]
        if m == "CPUID":
            return ["recomp_cpuid(c);"]
        if m == "CLI":
            return ["recomp_cli(c);"]
        if m == "STI":
            return ["recomp_sti(c);"]
        if m == "HLT":
            return ["recomp_hlt(c);"]
        if m == "INT":
            return ["recomp_int(c, %s);" % hexlit(parse_imm(ins.ops[0]))]
        if m == "INT3":
            return ["recomp_breakpoint(c, %s); return;" % hexlit(ins.addr)]
        if m == "IN":
            size = operand_size(ops[:1], hint=32)
            port = read_op(ops[1], 32) if len(ops) > 1 else "0u"
            L.append(write_op(ops[0], size, "recomp_in(c, %s, %d)" % (port, size // 8)))
            return L
        if m == "OUT":
            size = operand_size(ops[1:], hint=8)
            port = read_op(ops[0], 32) if ops[0].kind == "reg" else read_op(ops[0], 32)
            L.append("recomp_out(c, %s, %s, %d);" % (port, read_op(ops[1], size), size // 8))
            return L
        if m in ("NOP", "WAIT", "PAUSE"):
            return [";"]
        if m in ('EMMS', 'FEMMS'):
            return self.emit_x87(fn, ins, m, ops)
        if m == "FXSAVE":
            return ["recomp_fxsave(c, %s);" % addr_expr(ops[0])]

        # ------------------------------------------------------------ x87 --
        if m.startswith("F"):
            return self.emit_x87(fn, ins, m, ops)

        if is_vector_insn(m, ins.ops or ()):
            # Not one of the forms modelled above: a trap says so if a guest
            # that skipped the CPUID check ever reaches it.
            return ["recomp_unmodelled(c, %s); return;" % hexlit(ins.addr)]
        raise TranslateError("unhandled mnemonic %s" % m)

    # ---- control-flow helpers -------------------------------------------

    def reject_offimage_call(self, t):
        """Trap an off-image direct call at its own instruction address."""
        if self.image.end <= self.image.base:
            return  # a translator built without bytes, as the unit tests are
        if self.image.base <= t < self.image.end:
            return
        if GUEST_SHIM_BASE <= t < GUEST_SHIM_END:
            return
        raise TranslateError("call to %08x, which is outside the image" % t)

    def goto_target(self, fn, t, ins):
        if t in fn.index:
            return ["goto L_%08x;" % t]
        if t in self.func_addrs:
            self.call_targets.add(t)
            return ["CALL_FN(%08x); return;" % t]
        mapped = getattr(self, "mapped_containing", lambda t: None)(t) is not None
        if fn.addr in getattr(self, "provisional", ()) and not mapped and not GUEST_SHIM_BASE <= t < GUEST_SHIM_END:
            self.unmodelled.append((ins.addr, "transfer to unmapped instruction %08x" % t))
            return ["recomp_unmodelled(c, %s); return;" % hexlit(ins.addr)]
        return ["c->eip = %s; recomp_jump(c, %s); return;" % (hexlit(ins.addr), hexlit(t))]

    def emit_indirect_jump(self, fn, i, ins, op):
        if i in fn.return_jumps:
            orphan = ["recomp_seh_frame_orphan(c, seh_mark_);"] if i in fn.seh_escapes else []
            # A JMP through the entry stack slot is a RET written out longhand,
            # so it ends like one. Returning on the strength of the proof alone
            # assumes the slot held this host frame's return address, and the
            # proof only ever established where the value came from, not what
            # it is: a block recovered as its own function starts at delta zero
            # holding whatever its real caller pushed, and for Delphi's finally
            # idiom - PUSH resume; CALL cleanup; POP EAX; JMP EAX - that is a
            # continuation INTO the establishing body. Setting EIP and
            # returning drops it, and the establishing body's epilogue never
            # runs, so it never restores EBP; its caller then reads its own
            # locals through a frame pointer that moved, and the damage shows
            # up as a wrong value somewhere else entirely. recomp_return keeps
            # a genuine return as cheap as it was and dispatches the rest.
            return orphan + ["c->eip = %s; recomp_return(c); return;" % read_op(op, 32)]
        targets = self.jumptables.get((fn.addr, ins.addr))
        L = ["uint32_t t_ = %s;" % read_op(op, 32)]
        if not targets:
            # A table-less jump may enter any instruction of this body. Keep
            # that transfer in the current host frame; nonlocal targets and
            # holes in the listing retain the existing runtime dispatch.
            L.append("if (t_ >= %s && t_ < %s) {" % (hexlit(fn.insns[0].addr), hexlit(fn.end)))
            L.append("switch (t_) {")
            for t in sorted(fn.addrs - fn.dead_addrs):
                L.append("case %s: goto L_%08x;" % (hexlit(t), t))
            L.extend(["default: break;", "}"])
            L.append("}")
            L.append("c->eip = %s; recomp_jump(c, t_); return;" % hexlit(ins.addr))
            return L
        L.append("switch (t_) {")
        for t in sorted(set(targets)):
            if t in fn.index:
                L.append("case %s: goto L_%08x;" % (hexlit(t), t))
            else:
                self.call_targets.add(t)
                L.append("case %s: CALL_FN(%08x); return;" % (hexlit(t), t))
        L.append("default: c->eip = %s; recomp_jump(c, t_); return;" % hexlit(ins.addr))
        L.append("}")
        return L

    def emit_string(self, ins, m):
        suf = {"B": "b", "W": "w", "D": "d"}[m[-1]]
        kind = m[:-1].lower()
        if kind in ("scas", "cmps"):
            if ins.rep == "REPE" or ins.rep == "REP":
                return ["repe_%s(c);" % m.lower()]
            if ins.rep == "REPNE":
                return ["repne_%s(c);" % m.lower()]
            if ins.rep:
                raise TranslateError("prefix %s on %s" % (ins.rep, m))
            return ["%s(c);" % m.lower()]
        if ins.rep in ("REP", "REPE", "REPNE"):
            return ["rep_%s%s(c);" % (kind, suf)]
        if ins.rep:
            raise TranslateError("prefix %s on %s" % (ins.rep, m))
        return ["%s%s(c);" % (kind, suf)]


    # ---- x87 -------------------------------------------------------------

    def emit_sse_packed(self, ins, m, ops):
        dst = ops[0]
        if dst.kind != "xmm":
            raise TranslateError("%s destination is not an XMM register" % m)

        def source(op):
            if op.kind == "xmm":
                return "c->xmm[%d]" % op.reg, []
            if op.kind == "mem":
                return "s_", ["uint32_t ad_ = %s;" % addr_expr(op),
                              "uint32_t s_[4] = { rd32(ad_), rd32(ad_ + 4u), "
                              "rd32(ad_ + 8u), rd32(ad_ + 12u) };"]
            raise TranslateError("%s source is not XMM or memory" % m)

        if m in MMX_SHIFT:
            fn, bits = MMX_SHIFT[m]
            count = ops[1]
            if count.kind == "imm":
                expr, lines = hexlit(count.imm), []
            elif count.kind == "xmm":
                expr = "((uint64_t)c->xmm[%d][0] | ((uint64_t)c->xmm[%d][1] << 32))" \
                    % (count.reg, count.reg)
                lines = []
            elif count.kind == "mem":
                expr, lines = "rd64(%s)" % addr_expr(count), []
            else:
                raise TranslateError("%s shift count form" % m)
            wrapped = fn.replace("mmx_", "xmm_", 1)
            return lines + ["%s(c->xmm[%d], (uint64_t)%s, %du);" % (wrapped, dst.reg, expr, bits)]

        fn, bits = MMX_BINARY[m]
        src, lines = source(ops[1])
        if fn == "mmx_punpckl":
            lines.append("xmm_punpck(c->xmm[%d], %s, %du, 0);" % (dst.reg, src, bits))
        elif fn == "mmx_punpckh":
            lines.append("xmm_punpck(c->xmm[%d], %s, %du, 1);" % (dst.reg, src, bits))
        elif fn in ("mmx_packsswb", "mmx_packssdw", "mmx_packuswb"):
            from_bits = 16 if fn in ("mmx_packsswb", "mmx_packuswb") else 32
            signed = 0 if fn == "mmx_packuswb" else 1
            lines.append("xmm_pack(c->xmm[%d], %s, %du, %d);"
                         % (dst.reg, src, from_bits, signed))
        elif fn in ("mmx_pcmpeq", "mmx_pcmpgt"):
            lines.append("%s(c->xmm[%d], %s, %du);" % (fn.replace("mmx_", "xmm_", 1),
                                                       dst.reg, src, bits))
        else:
            wrapped = fn.replace("mmx_", "xmm_", 1)
            if bits:
                lines.append("%s(c->xmm[%d], %s, %du);" % (wrapped, dst.reg, src, bits))
            else:
                lines.append("%s(c->xmm[%d], %s);" % (wrapped, dst.reg, src))
        return lines

    def emit_mmx(self, ins, m):
        """MMX on the eight MMn registers, through runtime/x86.h's helpers."""
        ops = [parse_operand(o) for o in ins.ops]

        def src(op, width=64):
            if op.kind == "mm":
                return "c->mm[%d]" % op.reg
            if op.kind == "mem":
                return "rd64(%s)" % addr_expr(op) if width == 64 else "(uint64_t)rd32(%s)" % addr_expr(op)
            if op.kind == "imm":
                return "(uint64_t)0x%xu" % (op.imm & 0xff)
            if op.kind == "reg" and op.size == 32:
                return "(uint64_t)c->r[%d]" % op.reg
            raise TranslateError("MMX operand %r" % op.kind)

        if m == "MOVQ":
            dst, s = ops
            if dst.kind == "mm":
                return ["c->mm[%d] = %s;" % (dst.reg, src(s))]
            if dst.kind == "mem" and s.kind == "mm":
                return ["wr64(%s, c->mm[%d]);" % (addr_expr(dst), s.reg)]
            raise TranslateError("MOVQ form")
        if m == "MOVD":
            dst, s = ops
            if dst.kind == "mm":
                return ["c->mm[%d] = %s;" % (dst.reg, src(s, 32))]
            if s.kind == "mm":
                return [write_op(dst, 32, "(uint32_t)c->mm[%d]" % s.reg)]
            raise TranslateError("MOVD form")
        if m == "MOVNTQ":
            dst, s = ops
            if dst.kind == "mem" and s.kind == "mm":
                return ["wr64(%s, c->mm[%d]);" % (addr_expr(dst), s.reg)]
            raise TranslateError("MOVNTQ form")
        dst = ops[0]
        if dst.kind != "mm":
            raise TranslateError("%s destination is not an MMX register" % m)
        if m == "PSHUFW":
            return ["c->mm[%d] = mmx_pshufw(%s, %du);" % (dst.reg, src(ops[1]), ops[2].imm & 0xff)]
        if m in MMX_SHIFT:
            fn, bits = MMX_SHIFT[m]
            return ["c->mm[%d] = %s(c->mm[%d], %s, %du);" % (dst.reg, fn, dst.reg, src(ops[1]), bits)]
        fn, bits = MMX_BINARY[m]
        if bits:
            return ["c->mm[%d] = %s(c->mm[%d], %s, %du);" % (dst.reg, fn, dst.reg, src(ops[1]), bits)]
        return ["c->mm[%d] = %s(c->mm[%d], %s);" % (dst.reg, fn, dst.reg, src(ops[1]))]

    def emit_x87(self, fn, ins, m, ops):
        t = x87_transition
        operands = tuple(('st', op.sti) if op.kind == 'st' else
                         ('mem', (op.size or 0) // 8) if op.kind == 'mem' else
                         ('ax', 0) if op.kind == 'reg' else ('unsupported', 0) for op in ops)
        m = {'FSTSW': 'FNSTSW', 'FSTCW': 'FNSTCW'}.get(m, m)
        if m in t.ARITH and len(operands) == 1 and operands[0][0] == 'st':
            at = ins.addr
            while self.image.rd8(at) in (0x26, 0x2e, 0x36, 0x3e, 0x64, 0x65, 0x66, 0x67, 0x9b):
                at += 1
            if self.image.rd8(at) == 0xdc:
                operands = (operands[0], ('st', 0))
        memory = next((op for op in ops if op.kind == 'mem'), None)
        try:
            recipe = t.recipe({'mnem': m, 'operands': operands}, extended=True)
            operations = t.eager(recipe, t.external(memory) if memory is not None else None, decoded=True)
            return X87Printer(addr_expr, decoded=True, conditions=FCMOVCC).operations(operations)
        except SSAError as error:
            raise TranslateError(str(error)) from error
