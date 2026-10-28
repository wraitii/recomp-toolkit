"""C emission of finalized SSA, boundary operations and x87 recipes."""
from itertools import count
from .prepare import PreparedFunction, prepare
from .ssa import SSAError
from . import operations, c_symbols
from .body import BodyRequest, BodyVariant, EntryForm, ReturnForm, EmissionPolicy
from .contract_variants import PrivateLayout, output_mask as contract_output_mask
from .x87_cw import CW_FAST, Dynamic, Guarded, condition_of
from .c_boundaries import Printer as BoundaryPrinter, CC_OP_CONST
from .c_x87 import Printer as X87Printer
from .x87_transition import Store


RETURN_HELPERS = """
RECOMP_COLD void recomp_return_slow_(X86 *c) { recomp_return(c); }
static inline void recomp_return_fast_(X86 *c) {
    if (RECOMP_UNLIKELY(c->eip == GUEST_RETURN_SENTINEL) ||
        RECOMP_UNLIKELY(!recomp_is_call_return(c->eip)))
        recomp_return_slow_(c);
}
"""

TABLE_RETURN_HELPERS = """
extern const uint32_t recomp_call_return_min, recomp_call_return_span;
extern const uint32_t recomp_call_return_bits[];
RECOMP_COLD void recomp_return_slow_(X86 *c) { recomp_return(c); }
static inline void recomp_return_fast_(X86 *c) {
    uint32_t off = c->eip - recomp_call_return_min;
    if (RECOMP_UNLIKELY(off >= recomp_call_return_span) ||
        RECOMP_UNLIKELY(!((recomp_call_return_bits[off >> 5] >> (off & 31)) & 1u)))
        recomp_return_slow_(c);
}
"""

FLOAT_HELPERS = """
RECOMP_HOT_INLINE void recomp_fcom_(uint16_t *status, double a, double b, int quiet) {
    if (RECOMP_UNLIKELY(isnan(a) || isnan(b))) {
        fcom_status(status, a, b, quiet);
    } else {
        *status = (uint16_t)((*status & (uint16_t)~0x4700u) |
                            ((uint16_t)(a < b) << 8) | ((uint16_t)(a == b) << 14));
    }
}
"""


def memory_helpers():
    lines = []
    for floating, widths in ((False, (8, 16, 32, 64)), (True, (32, 64))):
        for bits in widths:
            ctype = ('float' if bits == 32 else 'double') if floating else 'uint%d_t' % bits
            suffix = '%s%d' % ('f' if floating else '', bits)
            for write in (False, True):
                name = ('wr' if write else 'rd') + suffix
                arguments = 'uint8_t *arena, uint32_t a' + (', %s v' % ctype if write else '')
                lines.extend(['RECOMP_HOT_INLINE %s recomp_%s_(%s) {' % ('void' if write else ctype, name, arguments),
                              '#if RECOMP_STORE_HOOKS || (defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS)',
                              '    (void)arena;',
                              '    %s%s(a%s);' % ('' if write else 'return ', name, ', v' if write else ''),
                              '#else'])
                if not write:
                    lines.append('    %s v;' % ctype)
                lines.append('    RECOMP_BITCOPY(%s, %s, %d);' % (
                    'arena + a' if write else '&v', '&v' if write else 'arena + a', bits // 8))
                if not write:
                    lines.append('    return v;')
                lines.extend(['#endif', '}'])
    return '\n'.join(lines) + '\n'


class ScalarPrinter(X87Printer):
    def expr(self, value):
        if value.kind == 'helper' and value.data in ('fcom', 'fucom'):
            status, a, b = [self.expr(arg) for arg in value.args]
            return 'recomp_fcom_(&%s, %s, %s, %du)' % (status, a, b, value.data == 'fucom')
        if value.kind == 'nan_fold':
            source = self.expr(value.args[0])
            return '(RECOMP_UNLIKELY(%s != %s) ? x87_indefinite() : %s)' % (source, source, source)
        if value.kind == 'float_store':
            cw, source = [self.expr(arg) for arg in value.args]
            return '(RECOMP_UNLIKELY((%s & 0x300u) != 0u || %s != %s) ? fto_float_cw(%s, %s) : (float)(%s))' % (
                cw, source, source, cw, source, source)
        if value.kind == 'load' and value.data[0] <= 64:
            bits, integer = value.data
            address = self.expr(value.args[0])
            call = 'recomp_rd%s%d_(arena_, (uint32_t)%s)' % ('' if integer else 'f', bits, address)
            return '(int%d_t)%s' % (bits, call) if integer else call
        if value.kind == 'load_word':
            return 'recomp_rd16_(arena_, (uint32_t)%s)' % self.expr(value.args[0])
        return super().expr(value)

    def operations(self, operations):
        lines, pending = [], []
        for operation in operations:
            if isinstance(operation, Store) and operation.bits <= 64 and not operation.bcd:
                lines.extend(super().operations(pending))
                pending = []
                lines.append('recomp_wr%s%d_(arena_, (uint32_t)%s, %s);' % (
                    'f' if operation.floating else '', operation.bits,
                    self.expr(operation.address), self.expr(operation.value)))
            else:
                pending.append(operation)
        return lines + super().operations(pending)


def declaration_lines(declarations):
    groups = {}
    for ctype, name in declarations:
        groups.setdefault(ctype, []).append(name)
    return ["%s %s;" % (ctype, ", ".join(names)) for ctype, names in groups.items()]


def seh_lines(kind, addr):
    eip = "c->eip = 0x%xu;" % addr
    if kind == "enter":
        return [eip, "{ jmp_buf *b_ = recomp_seh_frame_enter(c); "
                     "if (RECOMP_SETJMP(*b_)) { recomp_seh_land(c); return; } }"]
    if kind == "adopt":
        return [eip, "{ jmp_buf *b_ = recomp_seh_frame_adopt(c); "
                     "if (b_) { if (RECOMP_SETJMP(*b_)) { recomp_seh_land(c); return; } } }"]
    if kind == "leave":
        return [eip, "recomp_seh_frame_leave(c);"]
    if kind == "landing":
        return ["if (recomp_seh_pending_target()) recomp_seh_intercept(c, 0x%xu);" % addr]
    return ["recomp_seh_frame_orphan(c, seh_mark_);"]




def emit_prepared(prepared, external_entry=False):
    if prepared.final is None or prepared.transport is None:
        raise SSAError("C emission requires finalized boundary operations")
    options = prepared.options
    symbol = options.symbol
    x87_mode_sites, x87_entry_verify = options.mode_sites, options.request.entry_verify
    fir, s, live = prepared.ir, prepared.ssa, prepared.live
    mapping = {key: (c_symbols.cpu_field(field), lane) for key, (field, lane) in prepared.mapping.items()}
    seh_mark = prepared.seh_mark
    arithmetic_plan = prepared.arithmetic
    def ref(v):
        v = s.resolve(v)
        if v.opc == "CONST":
            return "0x%xu" % v.data if v.size <= 4 else "0x%xull" % v.data
        if v.opc == "TARGET":
            return "0x%xull" % v.data
        if v.opc == "CC_KIND":
            return CC_OP_CONST[v.data]
        return "v%d" % v.id

    def mask(size):
        return "0x%xull" % ((1 << (size * 8)) - 1)

    def assignment(v, value):
        if v.size not in (4, 8):
            value = "(%s) & %s" % (value, mask(v.size))
        return "v%d = %s;" % (v.id, value)

    def expression(v):
        wide = v.size > 4 or v.opc == "SUBPIECE"
        a = [ref(arg) for arg in v.args]
        opc = v.opc
        if opc in operations.FLOAT_COMPARISONS:
            values = ["recomp_sse_f32((uint32_t)(%s))" % arg for arg in a]
            if opc == "FLOAT_NAN":
                return "!!isnan(%s)" % values[0]
            return "%s %s %s" % (values[0], "==" if opc == "FLOAT_EQUAL" else "<", values[1])
        if opc in operations.FLOAT_ARITHMETIC:
            helper = {"FLOAT_ADD": "add", "FLOAT_SUB": "sub", "FLOAT_MULT": "mul",
                      "FLOAT_DIV": "div", "FLOAT_MIN": "min", "FLOAT_MAX": "max"}[opc]
            return "recomp_sse_%sps((uint32_t)(%s), (uint32_t)(%s))" % (helper, a[0], a[1])
        if opc == "CVTSS2SI":
            return "(uint32_t)recomp_sse_cvtss2si(c, recomp_sse_f32((uint32_t)(%s)))" % a[0]
        if opc == "CVTTSS2SI":
            return "(uint32_t)recomp_sse_cvttss2si(recomp_sse_f32((uint32_t)(%s)))" % a[0]
        if opc == "CVTSI2SS":
            return "recomp_sse_f32_bits((float)(int32_t)(uint32_t)(%s))" % a[0]
        if wide:
            a = ["(uint64_t)(%s)" % arg for arg in a]
        zero = "0ull" if wide else "0u"
        one = "1ull" if wide else "1u"
        suffix = "ull" if wide else "u"
        if opc == "PACK":
            return " | ".join("(%s << %d)" % (arg, n * 8) for n, arg in enumerate(a))
        if opc == "BYTE":
            return "%s >> %d" % (a[0], v.data * 8)
        if opc in ("COPY", "INT_ZEXT"):
            return a[0]
        if opc == "INT_SEXT":
            sign = "0x%x%s" % (1 << (v.args[0].size * 8 - 1), suffix)
            return "(%s ^ %s) - %s" % (a[0], sign, sign)
        binary = {"INT_ADD": "+", "INT_SUB": "-", "INT_MULT": "*", "INT_AND": "&", "INT_OR": "|",
                  "INT_XOR": "^", "INT_EQUAL": "==", "INT_NOTEQUAL": "!=",
                  "INT_LESS": "<", "INT_LESSEQUAL": "<=", "BOOL_AND": "&&",
                  "BOOL_OR": "||", "BOOL_XOR": "!=", "PIECE": None}
        if opc in binary and binary[opc]:
            return "%s %s %s" % (a[0], binary[opc], a[1])
        bits = v.args[0].size * 8 if v.args else 0
        if opc in ("INT_SLESS", "INT_SLESSEQUAL"):
            # Sign-bit bias compares two's-complement values without a signed
            # overflow or an implementation-defined unsigned-to-signed cast.
            sign = "0x%x%s" % (1 << (bits - 1), suffix)
            return "(%s ^ %s) %s (%s ^ %s)" % (a[0], sign,
                    "<" if opc == "INT_SLESS" else "<=", a[1], sign)
        if opc in ("INT_LEFT", "INT_RIGHT"):
            return "%s >= %d ? %s : (%s %s %s)" % (a[1], bits, zero, a[0],
                    "<<" if opc == "INT_LEFT" else ">>", a[1])
        if opc == "INT_SRIGHT":
            # P-code counts saturate, unlike the x86 helper's modulo-32 count.
            # Biasing the sign bit expresses sign extension with unsigned C.
            sign = "0x%x%s" % (1 << (bits - 1), suffix)
            return "%s >= %d ? (%s - (%s >> %d)) : (((%s ^ %s) >> %s) - (%s >> %s))" % (
                a[1], bits, zero, a[0], bits - 1, a[0], sign, a[1], sign, a[1])
        if opc == "INT_CARRY":
            operand = "0x%x%s" % ((1 << (v.args[0].size * 8)) - 1, suffix)
            return "%s > (%s - %s)" % (a[0], operand, a[1])
        if opc in ("INT_SCARRY", "INT_SBORROW"):
            result = "(%s %s %s)" % (a[0], "+" if opc == "INT_SCARRY" else "-", a[1])
            return "((%s(%s ^ %s) & (%s ^ %s)) >> %d) & %s" % (
                "~" if opc == "INT_SCARRY" else "", a[0], a[1], a[0], result, bits - 1, one)
        if opc == "BOOL_NEGATE":
            return "!%s" % a[0]
        if opc == "INT_NEGATE":
            return "~%s" % a[0]
        if opc == "INT_2COMP":
            return "%s - %s" % (zero, a[0])
        if opc == "POPCOUNT":
            return "%s(%s)" % ("__builtin_popcountll" if v.args[0].size > 4 else "__builtin_popcount", a[0])
        if opc == "SUBPIECE":
            return "%s >= 8 ? 0ull : (%s >> (%s * 8))" % (a[1], a[0], a[1])
        if opc == "PIECE":
            return "(%s << %d) | %s" % (a[0], v.args[1].size * 8, a[1])
        raise SSAError("unsupported integer operation %s" % opc)

    caches = count()
    boundary = BoundaryPrinter(prepared.transport,
                               {name: mapping[keys[0]][0] for name, keys in prepared.state_groups},
                               ref, mask,
                               lambda: "ic_%s_%d" % (prepared.options.symbol, next(caches)))

    def publish(event):
        return boundary.publication(event)

    def edge(source, target, fallthrough=None):
        return boundary.edge(source, target, block_label, fallthrough)

    x87_printer = ScalarPrinter(ref)
    block_label = "F" if arithmetic_plan.programs[0].clone == "fast" else "B"

    lines = [c_symbols.signature(options.request.variant, symbol, options.request.layout)]
    lines.append("uint8_t *arena_ = g_mem;")
    lines.extend(declaration_lines(
        ("uint%d_t" % (32 if v.size <= 4 else 64), "v%d" % v.id)
        for v in s.values
        if v.id in live and v.size and v.opc not in ("CONST", "TARGET", "CC_KIND") and s.resolve(v) is v))
    scalar_declarations = len(lines)
    lines.extend(x87_printer.declaration(op) for op in arithmetic_plan.environment_declarations())
    if not external_entry:
        lines.extend(boundary.operations(boundary.plan.entry))
    if seh_mark:
        lines.append("uint64_t seh_mark_ = recomp_seh_frame_mark(c);")
    for v in s.values:
        if v.opc != "INPUT" or not v.size or v.id not in live or s.resolve(v) is not v:
            continue
        field, lane = mapping[v.data]
        if (options.request.layout is not None and options.request.layout.input_argument
                and field == c_symbols.cpu_field(options.request.layout.input_argument)):
            field = options.request.layout.input_argument.lower() + "_"
        lines.append(assignment(v, "%s >> %d" % (field, lane * 8)))
    for v in s.entry_ops:
        if v.id in live and s.resolve(v) is v:
            lines.append(assignment(v, expression(v)))
    if x87_entry_verify:
        condition = condition_of(getattr(x87_entry_verify, "condition", x87_entry_verify))
        lines.append('if (RECOMP_UNLIKELY(!recomp_hooks_ever && (c->fpu_cw & 0x%xu) != 0x%xu)) recomp_x87_contract_violation(c, 0x%08xu, 0x%xu, 0x%xu);' % (condition.mask, condition.value, fir.addr, condition.mask, condition.value))
    if fir.entries:
        lines.append("switch (entry_) {")
        for e, block in zip(fir.entries, s.alternates):
            lines.append("case 0x%xu:" % e)
            lines.extend(edge(-1, block))
        lines.append("default:;")
        lines.extend(edge(-1, s.entry, next(iter(s.blocks))))
        lines.append("}")
    else:
        lines.extend(edge(-1, s.entry, next(iter(s.blocks))))
    indices = arithmetic_plan.indices
    lines[scalar_declarations:scalar_declarations] = [x87_printer.declaration(op) for op in arithmetic_plan.declarations()]

    for program in arithmetic_plan.programs:
        block_label = 'F' if program.clone == 'fast' else 'B'
        emitted = set()
        block_order = list(s.blocks)
        for position, i in enumerate(block_order):
            b = s.blocks[i]
            following = block_order[position + 1] if position + 1 < len(block_order) else None
            lines.append('%s%d:;' % (block_label, i))
            lines.append('// %08x: %s' % (b.insn.addr, b.insn.text))
            lines.extend(x87_printer.operations(program.blocks[i]))
            for v in b.ops:
                emitted.add(v.id)
                if v.opc not in ("X87_REG", "X87_MEM"):
                    lines.extend(x87_printer.operations(program.events[v.id]))
                if v.opc == "MEMORY":
                    continue
                if v.opc in ("X87_REG", "X87_MEM"):
                    if v.opc == "X87_MEM":
                        lines.extend(publish(v))
                    lines.append("{")
                    lines.extend(x87_printer.operations(program.events[v.id]))
                    lines.append("}")
                elif v.opc == "CALL":
                    lines.extend(boundary.event(v))
                elif v.opc in ("CALL_RELOAD", "CALL_KEEP"):
                    lines.extend(boundary.event(v))
                elif v.opc == "CALLIND":
                    # Publish the joined contract's pre-call state, including the
                    # target, then dispatch through the runtime. The return address
                    # was already stored by the preceding lifted CALL sequence, and
                    # the SSA builder keeps or reloads each lane/flag afterward.
                    lines.extend(boundary.event(v))
                elif v.opc == "STRINGOP":
                    # Byte-audited string instruction. Publication before the helper
                    # and reloads afterward keep guest memory observers and fault
                    # snapshots in access-then-advance order; the helper owns
                    # EDI/ESI/ECX/DF exactly as the eager emitter's string calls do.
                    lines.extend(publish(v))
                    lines.append("%s(c);" % v.data["helper"])
                elif v.opc in ("LOAD", "STORE", "DIV32", "IDIV32"):
                    # Guest accesses do not observe x87 state (see x87_scalar.py);
                    # only strict mode publishes before them. The division seam
                    # always sees the complete CPU.
                    lines.extend(publish(v))
                    if v.opc in ("DIV32", "IDIV32"):
                        helper = "div32" if v.opc == "DIV32" else "idiv32"
                        lines.append("%s(c, (uint32_t)%s, (uint32_t)%s);"
                                     % (helper, ref(v.args[2]), ref(v.args[3])))
                        lines.append("v%d = c->r[R_EAX] | ((uint64_t)c->r[R_EDX] << 32);" % v.id)
                    elif v.opc == "LOAD":
                        lines.append("v%d = recomp_rd%d_(arena_, (uint32_t)%s);" % (v.id, v.size * 8, ref(v.args[1])))
                    else:
                        lines.append("recomp_wr%d_(arena_, (uint32_t)%s, (uint%d_t)%s);" % (
                            v.args[2].size * 8, ref(v.args[1]), v.args[2].size * 8, ref(v.args[2])))
                elif v.opc == "RETURN":
                    lines.extend(boundary.event(v))
                elif v.opc == "SEH":
                    lines.extend(publish(v))
                    lines.extend(seh_lines(v.data["kind"], v.data["eip"]))
                elif v.opc == "TRAP":
                    lines.append("recomp_unknown_call(c, 0x%xu); return;" % v.data)
                elif v.opc == "BREAKPOINT":
                    lines.extend(publish(v))
                    lines.append("recomp_breakpoint(c, 0x%xu); return;" % v.data)
                elif v.opc == "TAIL":
                    lines.extend(boundary.event(v))
                elif v.opc == "BRANCHIND":
                    if i in fir.import_tails:
                        lines.extend(x87_printer.operations(program.defaults.get(v.id, ())))
                        lines.extend(publish(v))
                        lines.append('c->eip = 0x%xu;' % b.insn.addr)
                        target = ref(v.args[0])
                        first = min(ins.addr for ins in fir.insns if ins.length)
                        end = max(ins.addr + ins.length for ins in fir.insns if ins.length)
                        lines.append('if ((uint32_t)%s >= 0x%xu && (uint32_t)%s < 0x%xu)' % (target, first, target, end))
                        lines.append('%s(c, (uint32_t)%s);' % (c_symbols.symbol('body_import_fallback', fir.addr), target))
                        lines.append('else recomp_jump(c, (uint32_t)%s);' % target)
                        lines.append('return;')
                        continue
                    # A decoded jump table: the target address selects a case, any
                    # other value is a runtime jump exactly like the decoded
                    # emitter's default arm (publish everything, set EIP to the
                    # jump, `recomp_jump`). Case edges are ordinary internal edges.
                    grouped = {}
                    for j in sorted(set(fir.succ[i])):
                        grouped.setdefault(j, fir.insns[j].addr)
                    lines.append("switch ((uint32_t)%s) {" % ref(v.args[0]))
                    for j, addr in grouped.items():
                        lines.append("case 0x%xu:" % addr)
                        lines.extend(edge(i, j))
                    lines.append("default:")
                    lines.extend(x87_printer.operations(program.defaults.get(v.id, ())))
                    lines.extend(publish(v))
                    lines.append("c->eip = 0x%xu; recomp_jump(c, (uint32_t)%s); return;" % (
                        b.insn.addr, ref(v.args[0])))
                    lines.append("}")
                elif v.opc == "BRANCH":
                    lines.extend(edge(i, indices[v.args[0].data], following))
                elif v.opc == "CBRANCH":
                    target = indices[v.args[0].data]
                    lines.append("if (%s)" % ref(v.args[1]))
                    lines.extend(edge(i, target, following) or ['{}'])
                    if target == following:
                        lines.append('else')
                        lines.extend(edge(i, indices[b.insn.addr + b.insn.length], following) or ['{}'])
                        continue
                    lines.extend(edge(i, indices[b.insn.addr + b.insn.length], following))
                else:
                    lines.append(assignment(v, expression(v)))
            if not any(v.opc in operations.TERMINATORS for v in b.ops):
                lines.extend(x87_printer.operations(program.ends[i]))
                lines.extend(edge(i, fir.succ[i][0], following))
        if emitted != set(program.events):
            raise SSAError('x87 emission differs from its planned event coverage')
    lines.append("}")
    referenced = {line[5:-1] for line in lines if line.startswith('goto ')}
    lines = [line for line in lines if not (line.startswith(('B', 'F'))
             and line.endswith(':;') and line[:-2] not in referenced)]
    lines[scalar_declarations:scalar_declarations] = [x87_printer.declaration(op) for op in arithmetic_plan.temporaries]
    return "\n".join(lines)


def emit(fir, symbol, **options):
    requests, decisions = options.pop("variant_requests", None), options.pop("call_decisions", None)
    lifter = options.pop("lifter", None)
    masks = options.pop("contract_masks", None)
    fields = options.pop("return_fields", None)
    checked = options.pop("checked_return", False)
    known_mode = options.pop("x87_known_mode", False)
    if fields is not None and not checked:
        raise SSAError("private state entry requires a checked leaf without opaque observers")
    entry = (EntryForm.PRIVATE if fields is not None else EntryForm.CHECKED if checked
             else EntryForm.MULTIPLE if fir.entries else EntryForm.ORDINARY)
    returns = ReturnForm.PRIVATE if fields is not None else ReturnForm.CHECKED if checked else ReturnForm.ORDINARY
    variant = BodyVariant(entry, returns, Guarded(CW_FAST, "entry") if known_mode else Dynamic())
    policy = EmissionPolicy(**{name: options.pop(name) for name in EmissionPolicy.__dataclass_fields__ if name in options})
    options["mode_sites"] = options.pop("x87_mode_sites", None)
    options["entry_verify"] = options.pop("x87_entry_verify", None)
    request = BodyRequest(symbol, variant, policy, layout=PrivateLayout.from_mask(contract_output_mask(fields))
                          if fields is not None else None, report_calls=decisions is not None, **options)
    prepared = prepare(fir, request, lifter=lifter)
    wanted = {}
    for target, mask in prepared.requests:
        wanted[target] = wanted.get(target, 0) | mask
    layouts = {target: PrivateLayout.from_mask(mask) for target, mask in wanted.items()}
    if masks:
        layouts.update({target: PrivateLayout.from_mask(mask) for target, mask in masks.items()})
    prepared.finalize(layouts)
    source = emit_prepared(prepared)
    if requests is not None:
        requests.update(prepared.bound_requests)
    if decisions is not None:
        decisions.extend(prepared.decision_rows())
    return ("#ifndef RECOMP_SSA_SUPPORT\n#define RECOMP_SSA_SUPPORT\n" + memory_helpers()
            + RETURN_HELPERS + FLOAT_HELPERS + "#endif\n" + source)
