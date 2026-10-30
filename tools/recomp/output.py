"""Writers for the generated tree: body chunks, entry thunks, funcs.h, table.c, symbols.json."""
import hashlib
import json
import os
import re
from collections import defaultdict
from pathlib import Path

from decoded import TranslateError
from ir.c_symbols import declaration, symbol, entry_signature
from ir.c_symbols import Declaration
from ir.body import BodyVariant, EntryForm, ReturnForm
from ir.emit_c import memory_helpers, TABLE_RETURN_HELPERS, FLOAT_HELPERS

# Stable address buckets prevent an insertion from repacking the entire image.
# Oversize functions stand alone; recursively split only an over-budget bucket.
# SSA bodies are larger per guest instruction than decoded ones; at 512 KiB the
# median bucket chunk compiled in ~0.2 s, so per-file overhead dominated.
CHUNK_ADDRESS_BITS = 16
CHUNK_SOURCE_BUDGET = 4 * 1024 * 1024
ENTRY_ADDRESS_BITS = 16
INDIRECT_CACHE = re.compile(r"recomp_icall\(c, [^;]*, &(\w+), [^;]*\);")

BODY_HEADER = """/* generated -- translated bodies do not depend on replacement policy */
#ifndef RECOMP_BODY_H
#define RECOMP_BODY_H
#include "x86.h"
#include "intrinsics.h"
#include "seh.h"
#define RECOMP_ENTRY_CAT_(a, b) a##b
#define RECOMP_ENTRY_CAT(a, b) RECOMP_ENTRY_CAT_(a, b)
#define CALL_FN(a) RECOMP_ENTRY_CAT(entry_, a)(c)
#define CALL_FN_CHECKED(a, expected) RECOMP_ENTRY_CAT(entry_checked_, a)(c, expected)
#endif
"""

ENTRY_STUB_INCLUDES = """#include "x86.h"
#ifdef RECOMP_OVERRIDE_HEADER
#include RECOMP_OVERRIDE_HEADER
#endif
"""

ENTRY_STUB_PRELUDE = ENTRY_STUB_INCLUDES + """void recomp_enter(X86 *c, uint32_t index);
#define RECOMP_SLOW_(i) RECOMP_UNLIKELY(__atomic_load_n(&recomp_slow[i], __ATOMIC_RELAXED))
#define RECOMP_ENTER(i, base) \\
    do { recomp_execution_checkpoint(c); if (RECOMP_SLOW_(i)) recomp_enter(c, i); else base; } while (0)
#define RECOMP_ENTER_CHECKED(i, body, base) \\
    do { recomp_execution_checkpoint(c); \\
         if (RECOMP_SLOW_(i)) recomp_enter_checked(c, i, expected, body); else base; } while (0)
"""

MODULE_ENTRY_STUB_PRELUDE = ENTRY_STUB_INCLUDES + """
extern uint8_t recomp_%(key)s_hooked[];
void recomp_enter_%(key)s(X86 *c, uint32_t index);
void recomp_hook_%(key)s(X86 *c, uint32_t index);
#define RECOMP_ENTER(i, base) \\
    do { \\
        if (RECOMP_UNLIKELY(recomp_profile_enabled || recomp_frame_watch)) \\
            recomp_enter_%(key)s(c, i); \\
        else { \\
            recomp_execution_checkpoint(c); \\
            if (RECOMP_UNLIKELY(__atomic_load_n(&recomp_%(key)s_hooked[i], __ATOMIC_ACQUIRE))) \\
                recomp_hook_%(key)s(c, i); \\
            else base; \\
        } \\
    } while (0)
"""



def partition_chunks(functions, bodies, budget=CHUNK_SOURCE_BUDGET):
    """Bound source size without moving functions outside their address bucket.

    The assignment is independent of input order and other buckets. A split can
    move neighbours within one 64 KiB guest-address bucket, never the whole image.
    Function bodies and their alternate-entry wrappers remain indivisible.
    """
    buckets = defaultdict(list)
    weights = {fn.addr: len("\n".join(bodies[fn.addr]).encode("utf-8")) + bool(bodies[fn.addr])
               for fn in functions}
    result = []
    for fn in sorted(functions, key=lambda fn: fn.addr):
        if weights[fn.addr] >= budget:
            result.append(("chunk_fn_%08x.c" % fn.addr, [fn]))
        else:
            buckets[fn.addr >> CHUNK_ADDRESS_BITS].append(fn)

    def split(group, base, bits):
        if sum(weights[fn.addr] for fn in group) <= budget or len(group) == 1:
            result.append(("chunk_%08x_%02d.c" % (base, bits), group))
            return
        half = base + (1 << (bits - 1))
        for children, start in (([fn for fn in group if fn.addr < half], base),
                                ([fn for fn in group if fn.addr >= half], half)):
            if children:
                split(children, start, bits - 1)

    for bucket, group in sorted(buckets.items()):
        split(group, bucket << CHUNK_ADDRESS_BITS, CHUNK_ADDRESS_BITS)
    return sorted(result)


def emit_body_chunks(out, functions, bodies, entries_by_fn, dependencies, known_modes=(), module=None):
    """Emit local call declarations: no global census, indices or overrides."""
    Path(out, "body.h").write_text(BODY_HEADER)
    paths = []
    for name, group in partition_chunks(functions, bodies):
        path = Path(out, name)
        text = {fn.addr: "\n".join(bodies[fn.addr]) for fn in group}
        caches = sorted(set(INDIRECT_CACHE.findall("\n".join(text.values()))))
        declarations = set()
        for fn in group:
            declarations |= dependencies[fn.addr]
        calls = sorted({d.target for d in declarations if d.kind == "entry"})
        checked = sorted({d.target for d in declarations if d.kind == "checked"})
        private = sorted({(d.target, d.mask, d.input_argument or "") for d in declarations
                          if d.kind == "private_contract"})
        prepare = sorted({d.target for d in declarations if d.kind == "private_prepare"})
        with path.open("w") as fh:
            fh.write('/* generated by tools/recomp/driver.py -- do not edit */\n')
            fh.write('#include "body.h"\n')
            if any('arena_' in body for body in text.values()):
                fh.write(memory_helpers())
            if any('recomp_return_fast_' in body for body in text.values()):
                prefix = "recomp_%s_" % module["key"] if module else "recomp_"
                fh.write(TABLE_RETURN_HELPERS.replace("recomp_call_return_", prefix + "call_return_"))
            if any('recomp_fcom_(' in body for body in text.values()):
                fh.write(FLOAT_HELPERS)
            for cache in caches:
                fh.write("static RecompICache %s;\n" % cache)
            for addr in calls:
                fh.write(declaration(Declaration("entry", addr)) + "\n")
            for addr in checked:
                fh.write(declaration(Declaration("checked", addr)) + "\n")
            for addr in prepare:
                fh.write(declaration(Declaration("private_prepare", addr)) + "\n")
                fh.write(emit_contract_prepare(addr, addr in known_modes))
            for addr, mask, input_argument in private:
                fh.write(declaration(Declaration("private_contract", addr, mask, input_argument or None)) + "\n")
            fh.write("\n")
            for fn in group:
                alt = sorted(entries_by_fn.get(fn.addr, ()))
                fh.write("/* %s  %d insns  %08x..%08x%s */\n"
                         % (fn.name, fn.count, fn.first, fn.last,
                            ("  entries: " + " ".join("%08x" % a for a in alt)) if alt else ""))
                fh.write(text[fn.addr] + "\n\n")
        paths.append(str(path))
    return paths


def emit_entry_chunks(out, entry_names, module=None, checked_returns=(), private_masks=None,
                      private_input_arguments=None):
    """Tiny address-named entry thunks own the changing dense dispatch indices.

    A main-image thunk takes the execution checkpoint and jumps straight to its
    base, the FN_<addr> override or the translation, unless recomp_slow sends it
    through recomp_enter. Editing an override header recompiles these thunks
    and table.c, never the translated bodies.
    """
    groups = defaultdict(list)
    for index, addr in enumerate(entry_names):
        groups[addr >> ENTRY_ADDRESS_BITS].append((index, addr))
    paths = []
    for bucket, entries in sorted(groups.items()):
        path = Path(out, "chunk_entries_%08x.c" % (bucket << ENTRY_ADDRESS_BITS))
        with path.open("w") as fh:
            fh.write('/* generated stable entry thunks -- do not edit */\n')
            if module:
                fh.write(MODULE_ENTRY_STUB_PRELUDE % module)
            else:
                fh.write(ENTRY_STUB_PRELUDE)
            for index, addr in entries:
                fh.write("#ifndef FN_%08x\n#define FN_%08x fn_%08x\n#define RECOMP_RAW_%08x\n#endif\n"
                         "void FN_%08x(X86 *c);\n"
                         "void entry_%08x(X86 *c) { RECOMP_ENTER(%du, FN_%08x(c)); }\n"
                         % (addr, addr, addr, addr, addr, addr, index, addr))
            private = [(i, a) for i, a in entries if private_masks and a in private_masks]
            if private:
                for index, addr in private:
                    for mask in sorted(private_masks[addr]):
                        variant = BodyVariant(EntryForm.PRIVATE, ReturnForm.PRIVATE)
                        body = symbol("fn_contract", addr, mask)
                        entry = symbol("entry_contract", addr, mask)
                        input_argument = (private_input_arguments or {}).get(addr)
                        fh.write(entry_signature(variant, body, input_argument) + ";\n")
                        arguments = "c, %s_, expected" % input_argument.lower() if input_argument else "c, expected"
                        fh.write("%s { %s(%s); }\n" %
                                 (entry_signature(variant, entry, input_argument), body, arguments))
            checked = [(i, a) for i, a in entries if a in checked_returns]
            if checked:
                fh.write("void recomp_enter_checked(X86 *c, uint32_t index, uint32_t expected, "
                         "void (*body)(X86 *, uint32_t));\n")
                for index, addr in checked:
                    variant = BodyVariant(EntryForm.CHECKED, ReturnForm.CHECKED)
                    body, entry = symbol("fn_checked", addr), symbol("entry_checked", addr)
                    fh.write(entry_signature(variant, body) + ";\n")
                    if module:
                        fh.write("%s { recomp_enter_checked(c, %du, expected, %s); }\n"
                                 % (entry_signature(variant, entry), index, body))
                    else:
                        fh.write("%s {\n#ifdef RECOMP_RAW_%08x\n"
                                 "    RECOMP_ENTER_CHECKED(%du, %s, %s(c, expected));\n#else\n"
                                 "    RECOMP_ENTER_CHECKED(%du, %s, FN_%08x(c));\n#endif\n}\n"
                                 % (entry_signature(variant, entry), addr, index, body, body,
                                    index, body, addr))
        paths.append(str(path))
    return paths


def _entry_dispatch(signature, base_call):
    return """
%s
{
    RecompSaved saved_; recomp_save(c, &saved_);
    if (recomp_profile_enabled) { recomp_call(c, recomp_func_addrs[i]); return; }
#ifndef RECOMP_NO_HOOKS
    if (__builtin_expect(__atomic_load_n(&recomp_hooked[i], __ATOMIC_ACQUIRE) != 0u, 0)) {
        /* A hook callback observes the full register file, so a pending lazy
         * descriptor must be materialised before it runs.  The callee's own
         * entry settle may have been elided. */
        x86_cc_settle(c);
        recomp_hook_ptrs[i](c, i);
    } else
#endif
%s
#ifndef RECOMP_NO_HOOKS
    if (recomp_frame_watch)
        recomp_check_saved(c, recomp_func_addrs[i], &saved_);
#endif
}
""" % (signature, base_call)


def emit_entry_dispatch():
    """The slow half of the call-site policy, entered only when recomp_slow[i] is set.

    The entry stub has already taken the execution checkpoint. Profiling still
    enters recomp_call (including module lookup and its frame watch). Raw/base tables bypass this seam for call-original and comparisons.
    A guest return address is already on the guest stack before entering here.
    The acquire flag load observes the installer's published callback pointer.
    """
    ordinary = _entry_dispatch("void recomp_enter(X86 *c, uint32_t i)",
                               "        recomp_base_ptrs[i](c);")
    checked = _entry_dispatch(
        "void recomp_enter_checked(X86 *c, uint32_t i, uint32_t expected, "
        "void (*body)(X86 *, uint32_t))",
        "        if (recomp_base_ptrs[i] == recomp_raw_ptrs[i]) body(c, expected);\n"
        "        else recomp_base_ptrs[i](c);")
    return ordinary + checked


def emit_contract_prepare(addr, known_mode=False):
    mode_guard = "        (!mode_proven && (c->fpu_cw & 0x0f00u) != 0u) ||\n" if known_mode else ""
    unused_mode = "" if known_mode else "    (void)mode_proven;\n"
    return """
static inline int entry_contract_prepare_%08x(X86 *c, int mode_proven)
{
%s    if (RECOMP_UNLIKELY(recomp_hooks_ever || recomp_profile_enabled || recomp_frame_watch ||
%s        c->checkpoint_countdown == 0u || entry_contract_base_%08x != fn_%08x))
        return 0;
    --c->checkpoint_countdown;
    return 1;
}
""" % (addr, unused_mode, mode_guard, addr, addr)


def emit_contract_base(addr):
    return "void (*const entry_contract_base_%08x)(X86 *) = FN(%08x);\n" % (addr, addr)


def write_funcs_header(out, entry_names):
    with open(os.path.join(out, "funcs.h"), "w") as fh:
        fh.write("/* generated by tools/recomp/driver.py -- do not edit */\n")
        fh.write("#ifndef RECOMP_FUNCS_H\n#define RECOMP_FUNCS_H\n")
        fh.write('#include "x86.h"\n#include "intrinsics.h"\n#include "seh.h"\n')
        fh.write("/* Define FN_<addr> in RECOMP_OVERRIDE_HEADER to select a native\n"
                 " * base in table.c. Stable entries route direct and computed calls\n"
                 " * through that selection without recompiling their callers. */\n")
        fh.write("#ifdef RECOMP_OVERRIDE_HEADER\n#include RECOMP_OVERRIDE_HEADER\n#endif\n")
        fh.write("#define FN_CAT_(a, b) a##b\n#define FN_CAT(a, b) FN_CAT_(a, b)\n")
        fh.write("#define FN(a) FN_CAT(FN_, a)\n")
        fh.write("#define FIDX(a) FN_CAT(FIDX_, a)\n")
        fh.write('#include "body.h"\n')
        for i, a in enumerate(entry_names):
            fh.write("#define FIDX_%08x %du\n" % (a, i))
        # Both spellings are declared: fn_ADDR because the chunk defines it,
        # and FN(ADDR) so an override header only has to #define FN_<addr> and
        # its replacement is declared here.  In the default case the two are
        # the same declaration, which C allows to repeat.
        for a in entry_names:
            fh.write("#ifndef FN_%08x\n#define FN_%08x fn_%08x\n#endif\n" % (a, a, a))
        for a in entry_names:
            fh.write("void fn_%08x(X86 *c);\nvoid FN(%08x)(X86 *c);\nvoid entry_%08x(X86 *c);\n" % (a, a, a))
        fh.write("#endif\n")


def write_call_return_bitmap(fh, call_returns, prefix="recomp_"):
    if call_returns:
        first = call_returns[0]
        words = [0] * (((call_returns[-1] - first) >> 5) + 1)
        for addr in call_returns:
            offset = addr - first
            words[offset >> 5] |= 1 << (offset & 31)
    else:
        first, words = 0, [0]
    fh.write("const uint32_t %scall_return_min = 0x%08xu;\n" % (prefix, first))
    fh.write("const uint32_t %scall_return_span = %du;\n" % (prefix, len(words) * 32))
    fh.write("const uint32_t %scall_return_bits[] = {\n" % prefix)
    for word in words:
        fh.write("    0x%08xu,\n" % word)
    fh.write("};\n")
    return first, len(words) * 32


def write_table(out, entry_names, functions, call_returns, private_masks=None, known_modes=(), regions=()):
    with open(os.path.join(out, "table.c"), "w") as fh:
        fh.write("/* generated by tools/recomp/driver.py -- do not edit */\n")
        fh.write('#include <stdio.h>\n#include <stdlib.h>\n#include "funcs.h"\n#include "thunks.h"\n'
                 '#include "discovery.h"\n#include "interp.h"\n\n')
        fh.write("const uint32_t recomp_func_addrs[] = {\n")
        for a in entry_names:
            fh.write("    0x%08xu,\n" % a)
        fh.write("};\n")
        fh.write("const uint32_t recomp_func_count = %d;\n\n" % len(entry_names))
        fh.write("static const uint32_t recomp_call_returns[] = {\n")
        for a in call_returns or [0]:  # valid C storage even for a call-free image
            fh.write("    0x%08xu,\n" % a)
        fh.write("};\nstatic const uint32_t recomp_call_return_count = %d;\n\n" % len(call_returns))
        rows = sorted(regions or (), key=lambda row: row[0])
        for symbol in sorted({symbol for _start, _end, symbol in rows}):
            fh.write("void %s(X86 *c, uint32_t target);\n" % symbol)
        fh.write("typedef struct {\n    uint32_t start;\n    uint32_t end;\n    void (*fn)(X86 *, uint32_t);\n} RecompRegion;\n")
        fh.write("static const RecompRegion recomp_regions[] = {\n")
        for start, end, symbol in rows or [(0, 0, "0")]:
            fh.write("    {0x%08xu, 0x%08xu, %s},\n" % (start, end, symbol))
        fh.write("};\nstatic const uint32_t recomp_region_count = %d;\n\n" % len(rows))
        fh.write("static const char *const profile_names[] = {\n")
        for symbol in functions:
            fh.write("    %s,\n" % json.dumps(symbol["name"]))
        fh.write("};\n")
        fh.write("void (*const recomp_base_ptrs[])(X86 *) = {\n")
        for a in entry_names:
            fh.write("    FN(%08x),\n" % a)
        fh.write("};\n\n")
        fh.write("static void recomp_run_base(X86 *c, uint32_t i)\n"
                 "{\n    recomp_base_ptrs[i](c);\n}\n\n")
        # The untouched translations, beside the possibly-overridden ones: the
        # two differ exactly where this build defined an FN_<addr> override, so
        # the override set becomes a fact the binary states about itself.
        fh.write("void (*const recomp_raw_ptrs[])(X86 *) = {\n")
        for a in entry_names:
            fh.write("    fn_%08x,\n" % a)
        fh.write("};\n\n")
        fh.write("RecompHookFn recomp_hook_ptrs[] = {\n")
        for _a in entry_names:
            fh.write("    recomp_run_base,\n")
        fh.write("};\n")
        fh.write("uint8_t recomp_hooked[%d] = {0};\n" % len(entry_names))
        fh.write("uint8_t recomp_slow[%d] = {0};\n\n" % len(entry_names))
        fh.write(emit_entry_dispatch())
        known_modes = set(known_modes or ())
        for addr in sorted(private_masks or ()):
            fh.write(emit_contract_base(addr))
        # O(1) call-return lookup: a bitset over the addresses' span, built
        # once at translation time. The binary search this replaces ran on
        # every guest RET (7.4% self in a windowed profile); the bitset is
        # one array read. recomp_call_returns stays emitted as the source of
        # truth the translator's differential test compares the bitset to.
        write_call_return_bitmap(fh, call_returns)
        fh.write("""int recomp_is_call_return(uint32_t target)
{
    uint32_t off = target - recomp_call_return_min;
    if (off >= recomp_call_return_span)
        return 0;
    return (recomp_call_return_bits[off >> 5] >> (off & 31)) & 1u;
}

int recomp_region_enter(X86 *c, uint32_t target)
{
    uint32_t lo = 0, hi = recomp_region_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (recomp_regions[mid].start <= target) lo = mid + 1; else hi = mid;
    }
    if (lo == 0 || target >= recomp_regions[lo - 1].end) return 0;
    discovery_note("region", target, c->eip);
    recomp_regions[lo - 1].fn(c, target);
    return 1;
}

const char *recomp_profile_name(uint32_t i) { return i < recomp_func_count ? profile_names[i] : 0; }

uint32_t recomp_override_count(void)
{
    uint32_t i, n = 0;
    for (i = 0; i < recomp_func_count; ++i)
        if (recomp_base_ptrs[i] != recomp_raw_ptrs[i]) ++n;
    return n;
}

uint64_t recomp_override_hash(void)
{
    uint64_t h = 1469598103934665603ull;
    uint32_t i, j;
    for (i = 0; i < recomp_func_count; ++i) {
        if (recomp_base_ptrs[i] == recomp_raw_ptrs[i]) continue;
        for (j = 0; j < 4; ++j) {
            h ^= (recomp_func_addrs[i] >> (8 * j)) & 0xffu;
            h *= 1099511628211ull;
        }
    }
    return h;
}

/* Indirect calls repeat the same few targets, so a direct-mapped cache sits
 * in front of the binary search. An entry is (target << 32) | (index + 2),
 * written as one word, so a racing reader sees an old or a new entry. */
static uint64_t recomp_lookup_cache[1u << 14];

static int32_t recomp_lookup_slow(uint32_t target);

static int32_t recomp_lookup(uint32_t target)
{
    uint32_t slot = (uint32_t)(target * 2654435761u) >> 18;
    uint64_t e = __atomic_load_n(&recomp_lookup_cache[slot], __ATOMIC_ACQUIRE);
    if ((uint32_t)(e >> 32) == target && (uint32_t)e)
        return (int32_t)((uint32_t)e - 2u); /* 1 caches "not a function" (a shim) */
    int32_t i = recomp_lookup_slow(target);
    __atomic_store_n(&recomp_lookup_cache[slot],
                     ((uint64_t)target << 32) | (uint64_t)(uint32_t)(i + 2), __ATOMIC_RELEASE);
    return i;
}

static int32_t recomp_lookup_slow(uint32_t target)
{
    uint32_t lo = 0, hi = recomp_func_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (recomp_func_addrs[mid] < target) lo = mid + 1; else hi = mid;
    }
    return (lo < recomp_func_count && recomp_func_addrs[lo] == target)
           ? (int32_t)lo : -1;
}

int32_t recomp_index_of(uint32_t addr) { return recomp_lookup(addr); }

int recomp_thunk_target_kind(uint32_t target)
{
    if (recomp_lookup(target) >= 0 || recomp_module_lookup(target) >= 0) return 1;
    return (recomp_is_call_return(target) || recomp_module_is_call_return(target)) ? 2 : 0;
}

#ifdef RECOMP_CHECK_CALL_FACTS
static void recomp_check_call_fact(uint32_t ret, uint32_t target)
{
    uint32_t lo = 0, hi = recomp_call_fact_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (recomp_call_facts[mid].ret < ret) lo = mid + 1; else hi = mid;
    }
    if (lo == recomp_call_fact_count || recomp_call_facts[lo].ret != ret) return;
    const uint32_t *t = recomp_call_fact_targets + recomp_call_facts[lo].first;
    for (uint32_t i = 0; i < recomp_call_facts[lo].count; ++i)
        if (t[i] == target) return;
    recomp_call_fact_violation(ret, target);
}
#endif
static void recomp_call_inner(X86 *c, uint32_t target);
void recomp_call(X86 *c, uint32_t target)
{
#ifdef RECOMP_CHECK_CALL_FACTS
    recomp_check_call_fact(rd32(c->r[4]), target);
#endif
    RecompSaved saved_; recomp_save(c, &saved_);
    recomp_call_inner(c, target);
    if (recomp_frame_watch)
        recomp_check_saved(c, target, &saved_);
}
static void recomp_call_inner(X86 *c, uint32_t target)
{
    uint32_t site = observe_take();
    if (target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) {
        observe_indirect(site, target, "shim");
        if (RECOMP_UNLIKELY(!imports_dispatch(c, target))) recomp_shim_call(c, target);
        return;
    }
    int32_t i = recomp_lookup(target);
    if (i >= 0) {
        observe_indirect(site, target, "known");
        recomp_execution_checkpoint(c);
        if (recomp_profile_enabled) recomp_profile_push((uint32_t)i);
        /* A translated body settles or drops a pending descriptor at its own
         * entry; an override or a hook reads the flag fields as published. */
        if (recomp_base_ptrs[i] != recomp_raw_ptrs[i]) x86_cc_settle(c);
#ifdef RECOMP_NO_HOOKS
        recomp_base_ptrs[i](c);
#else
        if (__atomic_load_n(&recomp_hooked[i], __ATOMIC_ACQUIRE)) {
            x86_cc_settle(c);
            recomp_hook_ptrs[i](c, (uint32_t)i);
        } else
            recomp_base_ptrs[i](c);
#endif
        if (recomp_profile_enabled) recomp_profile_pop();
        return;
    }
    if (recomp_module_call(c, target)) {
        observe_indirect(site, target, "module");
        return;
    }
    if (target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) {
        if (RECOMP_UNLIKELY(!imports_dispatch(c, target))) recomp_shim_call(c, target);
        return;
    }
    x86_cc_settle(c);
    observe_pend(site);
    recomp_unknown_call(c, target);
}

/* Block-entry dispatch: the table holds every function start, every alternate
 * entry and every decoded jump-table target. */
void recomp_jump(X86 *c, uint32_t target)
{
    if (recomp_seh_pending_target()) recomp_seh_intercept(c, target);
    int32_t i = recomp_lookup(target);
    if (i >= 0) {
        recomp_execution_checkpoint(c);
        if (recomp_profile_enabled) recomp_profile_push((uint32_t)i);
#ifdef RECOMP_NO_HOOKS
        recomp_base_ptrs[i](c);
#else
        if (__atomic_load_n(&recomp_hooked[i], __ATOMIC_ACQUIRE)) {
            /* A jump into a hooked entry must publish a pending descriptor
             * before the callback reads the CPU. */
            x86_cc_settle(c);
            recomp_hook_ptrs[i](c, (uint32_t)i);
        } else {
            recomp_base_ptrs[i](c);
        }
#endif
        if (recomp_profile_enabled) recomp_profile_pop();
        return;
    }
    if (recomp_module_containing(target)) {
        /* Preserve the void tail jump; a status-returning call grows the host stack. */
        recomp_module_jump(c, target);
        return;
    }
    if (target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) {
        if (RECOMP_UNLIKELY(!imports_dispatch(c, target))) recomp_shim_call(c, target);
        return;
    }
    const RecompRegion *region = recomp_region_find(recomp_regions, recomp_region_count, target);
    if (region) {
        /* Keep this tail-callable: guest loops can cross region boundaries. */
        recomp_region_invoke(c, region, target);
        return;
    }
    if (recomp_is_call_return(target) || recomp_module_is_call_return(target)) { c->eip = target; return; }
    recomp_unknown_jump(c, target);
}

void recomp_unknown_jump(X86 *c, uint32_t target)
{
    if (recomp_run_thunk(c, target)) return;
    if (recomp_region_enter(c, target)) return;
    /* The address is code the map does not carry: record it (RECOMP_DISCOVERY)
     * so it can be added in Ghidra, then let the interpreter carry the run
     * past the gap so one run reports every gap it reaches. */
    discovery_note("jump", target, c->eip);
    if (interp_call(c, target)) return;
    discovery_write();
    observe_write();
    fprintf(stderr, "recomp: no block entry for indirect jump to 0x%08x from 0x%08x\\n",
            target, c->eip);
    abort();
}
""")


def write_module_table(out, entry_names, functions, call_returns, module):
    key = module["key"]
    prefix = "recomp_%s_" % key
    with open(os.path.join(out, "table.c"), "w") as fh:
        fh.write('#include "funcs.h"\n#include "thunks.h"\n\n')
        fh.write("static const uint32_t %saddrs[] = {\n" % prefix)
        for addr in entry_names:
            fh.write("    0x%08xu,\n" % addr)
        fh.write("};\n")
        return_min, return_span = write_call_return_bitmap(fh, call_returns, prefix)
        fh.write("static const char *const %snames[] = {\n" % prefix)
        for symbol in functions:
            fh.write("    %s,\n" % json.dumps(symbol["name"]))
        fh.write("};\nstatic void (*const %sbase[])(X86 *) = {\n" % prefix)
        for addr in entry_names:
            fh.write("    FN(%08x),\n" % addr)
        fh.write("};\n")
        fh.write("static void %srun(X86 *c, uint32_t i) { %sbase[i](c); }\n" % (prefix, prefix))
        fh.write("static RecompHookFn %shooks[] = {\n" % prefix)
        for _ in entry_names:
            fh.write("    %srun,\n" % prefix)
        fh.write("};\nuint8_t %shooked[%d];\n" % (prefix, len(entry_names)))
        fh.write("void recomp_hook_%s(X86 *c, uint32_t i)\n{\n" % key)
        fh.write("    x86_cc_settle(c); %shooks[i](c, i);\n}\n" % prefix)
        fh.write("void recomp_enter_%s(X86 *c, uint32_t i)\n{\n" % key)
        fh.write("    if (!recomp_profile_enabled && !recomp_frame_watch) {\n")
        fh.write("        recomp_execution_checkpoint(c);\n")
        fh.write("        if (__atomic_load_n(&%shooked[i], __ATOMIC_ACQUIRE)) recomp_hook_%s(c, i);\n" % (prefix, key))
        fh.write("        else %sbase[i](c);\n        return;\n    }\n" % prefix)
        fh.write("    RecompSaved saved_; recomp_save(c, &saved_);\n")
        fh.write("    recomp_execution_checkpoint(c);\n")
        fh.write("    if (recomp_profile_enabled) recomp_profile_push(i);\n")
        fh.write("    if (__atomic_load_n(&%shooked[i], __ATOMIC_ACQUIRE)) recomp_hook_%s(c, i);\n" % (prefix, key))
        fh.write("    else %sbase[i](c);\n" % prefix)
        fh.write("    if (recomp_profile_enabled) recomp_profile_pop();\n")
        fh.write("    if (recomp_frame_watch) recomp_check_saved(c, %saddrs[i], &saved_);\n}\n" % prefix)
        fh.write("static const RecompModule %smodule = {\n" % prefix)
        fh.write("    %s, 0x%08xu, 0x%08xu,\n" %
                 (json.dumps(module["name"]), module["base"], module["base"] + module["size"]))
        fh.write("    %saddrs, %d, %sbase, %shooks, %shooked,\n" %
                 (prefix, len(entry_names), prefix, prefix, prefix))
        fh.write("    %scall_return_bits, 0x%08xu, %du, %snames\n};\n" % (prefix, return_min, return_span, prefix))
        fh.write("__attribute__((constructor)) static void %sregister(void)\n" % prefix)
        fh.write("{\n    recomp_module_register(&%smodule);\n}\n" % prefix)


def write_call_facts(out, facts, cw_effects):
    rows, targets = [], []
    for ret, found in sorted(facts.items()):
        rows.append("    {0x%08xu, %du, %du},\n" % (ret, len(targets), len(found)))
        targets.extend(found)
    with open(os.path.join(out, "call_facts.c"), "w") as fh:
        fh.write("/* generated by tools/recomp/driver.py -- do not edit */\n")
        fh.write('#include "x86.h"\n\n')
        fh.write("const RecompCallFact recomp_call_facts[] = {\n")
        fh.write("".join(rows) or "    {0u, 0u, 0u},\n")
        fh.write("};\nconst uint32_t recomp_call_fact_count = %d;\n\n" % len(rows))
        fh.write("const uint32_t recomp_call_fact_targets[] = {\n")
        for t in targets or [0]:
            fh.write("    0x%08xu,\n" % t)
        fh.write("};\n\nconst RecompCwEffect recomp_cw_effects[] = {\n")
        for ret, (preserved, known, value) in sorted(cw_effects.items()) or [(0, (0, 0, 0))]:
            fh.write("    {0x%08xu, 0x%04xu, 0x%04xu, 0x%04xu},\n" % (ret, preserved, known, value))
        fh.write("};\nconst uint32_t recomp_cw_effect_count = %d;\n" % len(cw_effects))


def write_symbols(out, executable, image_base, functions, curated):
    sha = hashlib.sha256(open(executable, "rb").read()).hexdigest()
    globals_out = []
    for section, body in sorted(curated.items()):
        if not section.startswith("globals."):
            continue
        e = {"name": section.split(".", 1)[1], "addr": "%08x" % body["addr"]}
        for k in ("size", "stride", "count"):
            if k in body:
                e[k] = body[k]
        globals_out.append(e)
    by_addr = {int(f["addr"], 16): f for f in functions}
    events = {k: "%08x" % v for k, v in sorted(curated.get("events", {}).items())}
    for name, addr in events.items():
        f = by_addr.get(int(addr, 16))
        if not f or not f["hookable"]:
            raise TranslateError("event %s (%s) is not a hookable entry symbol" % (name, addr))
    with open(os.path.join(out, "symbols.json"), "w") as fh:
        json.dump({"exe_sha256": sha, "image_base": "%08x" % image_base,
                   "functions": functions, "globals": globals_out,
                   "events": events}, fh, indent=1)
    return len(globals_out), len(events)


HOOK_EVIDENCE = ("reloc", "curated")


def hook_kind(addr, listed, alt_owner, provenance, evidence):
    """The symbol kind for `addr`, and whether a mod may hook it.

    Every entry can be dispatched to; only a place the program names has a
    meaning stable enough to hook:

      entry         a function start or a curated entry.
      block         an interior address a jump table names, however else it
                    is reached.
      alternate     an interior entry a base relocation or curation names.
      continuation  any other interior entry.
    """
    if addr in listed or "curated_entry" in evidence.get(addr, ()):
        return "entry", True
    if addr not in alt_owner or provenance.get(addr) == "table":
        return "block", False
    if any(e in HOOK_EVIDENCE for e in evidence.get(addr, ())):
        return "alternate", True
    return "continuation", False
