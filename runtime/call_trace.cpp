// call_trace.cpp - see call_trace.h for the switch and the contract.
//
// Design notes:
//
// * The hook is published exactly like a mod hook (mods/hooks.cpp): the
//   function pointer with a release store, then the per-entry flag with a
//   release store, so a call site that observes the flag is guaranteed to see
//   the pointer. The previous pointer is captured first and chained to, so the
//   original (or a mod's dispatch) still runs underneath.
//
// * An address may live in the main image or in a registered auxiliary module
//   (`Module!addr`). The generated dispatch passes only a table-local index to
//   the hook, so a shared dispatcher could not tell a main-image index from a
//   module index. Each armed address therefore gets its own entry stub, and the
//   stub records which trace it belongs to; installation goes into whichever
//   table owns the address.
//
// * The hook runs the callee through the previous pointer and only reads the
//   X86 struct and guest memory around it. It never writes guest registers,
//   flags or memory, so a traced run is byte-for-byte the run it would have
//   been.
//
// * Nothing here allocates or holds a C++ owner across the guest call: a guest
//   longjmp can cross these frames, and the state is process-lifetime POD.
#include "call_trace.h"

#include "x86.h"
#include "guest.h"
#include "../platform/os.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

// The generated table owns these strongly. The weak fallbacks let a build with
// no translation (the runtime unit tests, a stub host that links boot.cpp)
// link: the tracer then finds no entries and arms nothing. The generated
// table's strong definitions override them at link time; the entry-fixture test
// gives its arrays initialisers so it overrides them too.
//
// The prior declarations in x86.h are extern "C", so these definitions keep C
// linkage.
extern "C" {
__attribute__((weak)) const uint32_t recomp_func_addrs[] = {0};
__attribute__((weak)) const uint32_t recomp_func_count = 0;
__attribute__((weak)) const RecompDispatchTable recomp_dispatch = {nullptr, 0, 0, 0, 0, 0};
__attribute__((weak)) void (*const recomp_base_ptrs[])(X86 *c) = {nullptr};
__attribute__((weak)) void (*const recomp_raw_ptrs[])(X86 *c) = {nullptr};
__attribute__((weak)) RecompHookFn recomp_hook_ptrs[] = {nullptr};
__attribute__((weak)) uint8_t recomp_hooked[] = {0};
__attribute__((weak)) uint8_t recomp_slow[] = {0};
__attribute__((weak)) int recomp_region_enter(X86 *, uint32_t) {
    return 0;
}
}

namespace {

constexpr uint32_t kMaxTraces = 64;
constexpr uint32_t kLabelCap = 64;
constexpr uint32_t kArgs = 4;
constexpr uint32_t kMaxDumps = 16;
constexpr uint32_t kDumpTextCap = 32;

enum DumpBaseKind {
    DUMP_ARG = 0, // base register value read from the guest stack
    DUMP_REG = 1, // base register value read from X86
};

// One logged word: `arg0+0x18` or `arg0+0x14c.b` or `eax+0x4`.
struct DumpSpec {
    uint8_t base_kind;
    uint8_t base_index; // stack argument index or X86 register index
    uint8_t size;       // 1 (byte) or 4 (dword)
    uint32_t off;
    char text[kDumpTextCap];
};

struct TraceState {
    uint32_t addr;
    uint32_t index; // index into the owning table
    const RecompModule *module;
    char label[kLabelCap];
    uint64_t calls;
    uint64_t logged;
    uint64_t suppressed;
    RecompHookFn next; // the hook that was published when we installed
    uint32_t last_args[kArgs];
    uint32_t last_ret;
    uint32_t last_dumps[kMaxDumps];
    uint8_t last_valid[kMaxDumps];
    uint8_t have_last;
    uint32_t dump_count;
    DumpSpec dumps[kMaxDumps];
};

TraceState g_traces[kMaxTraces];
uint32_t g_trace_count;
uint32_t g_full_limit = 20;
uint64_t g_period_ns = 5ull * 1000ull * 1000ull * 1000ull;
uint64_t g_last_summary_ns;
uint8_t g_installed;
uint8_t g_summary_at_exit;

void trace_dispatch(X86 *c, uint32_t slot);

// A distinct stub per trace slot. The generated tables store hooks of type
// RecompHookFn (X86*, index); a single dispatcher would only see the index,
// which is table-local, and so could not resolve a main-image index from a
// module index. The stub carries the slot instead.
// clang-format off
#define RECOMP_TRACE_SLOT_SEQ(X) \
    X(0)  X(1)  X(2)  X(3)  X(4)  X(5)  X(6)  X(7) \
    X(8)  X(9)  X(10) X(11) X(12) X(13) X(14) X(15) \
    X(16) X(17) X(18) X(19) X(20) X(21) X(22) X(23) \
    X(24) X(25) X(26) X(27) X(28) X(29) X(30) X(31) \
    X(32) X(33) X(34) X(35) X(36) X(37) X(38) X(39) \
    X(40) X(41) X(42) X(43) X(44) X(45) X(46) X(47) \
    X(48) X(49) X(50) X(51) X(52) X(53) X(54) X(55) \
    X(56) X(57) X(58) X(59) X(60) X(61) X(62) X(63)

#define RECOMP_TRACE_SLOT_DECL(n) extern "C" void recomp_trace_slot_##n(X86 *c, uint32_t index);
RECOMP_TRACE_SLOT_SEQ(RECOMP_TRACE_SLOT_DECL)
#undef RECOMP_TRACE_SLOT_DECL

static RecompHookFn const kSlotFns[] = {
#define RECOMP_TRACE_SLOT_ENTRY(n) recomp_trace_slot_##n,
    RECOMP_TRACE_SLOT_SEQ(RECOMP_TRACE_SLOT_ENTRY)
#undef RECOMP_TRACE_SLOT_ENTRY
};
static_assert(sizeof kSlotFns / sizeof kSlotFns[0] >= kMaxTraces, "not enough trace slots");

#define RECOMP_TRACE_SLOT_DEF(n) \
    extern "C" void recomp_trace_slot_##n(X86 *c, uint32_t index) { \
        (void)index; \
        trace_dispatch(c, n); \
    }
RECOMP_TRACE_SLOT_SEQ(RECOMP_TRACE_SLOT_DEF)
#undef RECOMP_TRACE_SLOT_DEF
// clang-format on

TraceState *find_by_index(const RecompModule *module, uint32_t index) {
    for (uint32_t i = 0; i < g_trace_count; ++i)
        if (g_traces[i].module == module && g_traces[i].index == index)
            return &g_traces[i];
    return nullptr;
}

// The generated table is sorted; the tracer needs its own lookup only because
// a no-translation test host has no recomp_index_of.
int32_t main_lookup(uint32_t addr) {
    uint32_t lo = 0, hi = recomp_func_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (recomp_func_addrs[mid] < addr)
            lo = mid + 1;
        else
            hi = mid;
    }
    return (lo < recomp_func_count && recomp_func_addrs[lo] == addr) ? (int32_t)lo : -1;
}

int32_t module_lookup(const RecompModule *m, uint32_t addr) {
    uint32_t lo = 0, hi = m->func_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (m->func_addrs[mid] < addr)
            lo = mid + 1;
        else
            hi = mid;
    }
    return (lo < m->func_count && m->func_addrs[lo] == addr) ? (int32_t)lo : -1;
}

// Case-insensitive match of `name[0..len)` against the module's name, allowing
// the key form without the extension (`ScriptLibraryR` for
// `ScriptLibraryR.dll`).
bool module_prefix_matches(const char *full, const char *name, size_t len) {
    for (size_t i = 0; i < len; ++i) {
        if (!full[i])
            return false;
        char a = full[i], b = name[i];
        if (a >= 'A' && a <= 'Z')
            a = (char)(a + 32);
        if (b >= 'A' && b <= 'Z')
            b = (char)(b + 32);
        if (a != b)
            return false;
    }
    return full[len] == '\0' || full[len] == '.';
}

const RecompModule *find_module(const char *name, size_t len) {
    if (!len)
        return nullptr;
    for (uint32_t i = 0; i < recomp_module_count(); ++i) {
        const RecompModule *m = recomp_module_at(i);
        if (module_prefix_matches(m->name, name, len))
            return m;
    }
    return nullptr;
}

uint32_t read_stack(uint32_t esp, uint32_t off) {
    if (!g_mem || !gm_valid(esp + off, 4))
        return 0;
    return rd32(esp + off);
}

// The base is the guest address named by the dump spec: an argument read from
// the stack, or a register. An unmapped address yields valid = 0 and the r8/r32
// accessors are never reached.
uint32_t read_dump(const DumpSpec &d, const X86 *c, const uint32_t *args, uint8_t *valid) {
    uint32_t base = (d.base_kind == DUMP_ARG) ? args[d.base_index] : c->r[d.base_index];
    uint32_t addr = base + d.off;
    if (!g_mem || !gm_valid(addr, d.size)) {
        *valid = 0;
        return 0;
    }
    *valid = 1;
    return d.size == 1 ? rd8(addr) : rd32(addr);
}

void read_dumps(const TraceState *st, const X86 *c, const uint32_t *args, uint32_t *vals,
                uint8_t *valid) {
    for (uint32_t i = 0; i < st->dump_count; ++i)
        vals[i] = read_dump(st->dumps[i], c, args, &valid[i]);
}

// Bounded append; returns the new length, clamped at cap.
size_t app(char *buf, size_t cap, size_t n, const char *fmt, ...) {
    if (n >= cap)
        return cap;
    va_list ap;
    va_start(ap, fmt);
    int w = vsnprintf(buf + n, cap - n, fmt, ap);
    va_end(ap);
    if (w < 0)
        return n;
    if ((size_t)w >= cap - n)
        return cap;
    return n + (size_t)w;
}

void format_dumps(const TraceState *st, const uint32_t *vals, const uint8_t *valid, char *buf,
                  size_t cap) {
    size_t n = app(buf, cap, 0, "[");
    for (uint32_t i = 0; i < st->dump_count; ++i) {
        if (i)
            n = app(buf, cap, n, " ");
        if (valid[i])
            n = app(buf, cap, n, "%s=0x%0*x", st->dumps[i].text, st->dumps[i].size * 2, vals[i]);
        else
            n = app(buf, cap, n, "%s=?", st->dumps[i].text);
    }
    app(buf, cap, n, "]");
}

const char *x87_string(const X86 *c, char *out, size_t cap) {
    out[0] = 0;
    unsigned phys = c->fpu_top & 7u;
    if (ftag_of(c, phys) != FTAG_EMPTY)
        snprintf(out, cap, " st0=%.17g", ST(c, 0));
    return out;
}

void log_entry(const TraceState *st, const X86 *c, uint32_t caller, const uint32_t *args,
               const uint32_t *dumps, const uint8_t *valid) {
    char d[640];
    format_dumps(st, dumps, valid, d, sizeof d);
    fprintf(stderr,
            "[trace-call] %s 0x%08x call#%llu entry ret=0x%08x ecx=0x%08x edx=0x%08x "
            "args=[0x%08x 0x%08x 0x%08x 0x%08x] dumps=%s\n",
            st->label, st->addr, (unsigned long long)st->calls, caller, c->r[R_ECX], c->r[R_EDX],
            args[0], args[1], args[2], args[3], d);
}

void log_return(const TraceState *st, const X86 *c, const uint32_t *dumps, const uint8_t *valid) {
    char d[640], x87[48];
    format_dumps(st, dumps, valid, d, sizeof d);
    fprintf(stderr, "[trace-call] %s 0x%08x call#%llu return eax=0x%08x%s dumps=%s\n", st->label,
            st->addr, (unsigned long long)st->calls, c->r[R_EAX], x87_string(c, x87, sizeof x87),
            d);
}

void log_changed(const TraceState *st, const X86 *c, uint32_t caller, const uint32_t *args,
                 const uint32_t *dumps, const uint8_t *valid) {
    char d[640], x87[48];
    format_dumps(st, dumps, valid, d, sizeof d);
    fprintf(stderr,
            "[trace-call] %s 0x%08x call#%llu ret=0x%08x ecx=0x%08x edx=0x%08x "
            "args=[0x%08x 0x%08x 0x%08x 0x%08x] -> eax=0x%08x%s dumps=%s (suppressed %llu)\n",
            st->label, st->addr, (unsigned long long)st->calls, caller, c->r[R_ECX], c->r[R_EDX],
            args[0], args[1], args[2], args[3], c->r[R_EAX], x87_string(c, x87, sizeof x87), d,
            (unsigned long long)st->suppressed);
}

void print_summary(const TraceState &st) {
    char d[640];
    format_dumps(&st, st.last_dumps, st.last_valid, d, sizeof d);
    fprintf(stderr,
            "[trace-call] summary %s 0x%08x calls=%llu logged=%llu suppressed=%llu last=%s\n",
            st.label, st.addr, (unsigned long long)st.calls, (unsigned long long)st.logged,
            (unsigned long long)st.suppressed, d);
}

void maybe_summary() {
    if (!g_trace_count)
        return;
    uint64_t now = os_monotonic_ns();
    if (!g_last_summary_ns) {
        g_last_summary_ns = now;
        return;
    }
    if (now - g_last_summary_ns < g_period_ns)
        return;
    g_last_summary_ns = now;
    for (uint32_t i = 0; i < g_trace_count; ++i)
        print_summary(g_traces[i]);
}

void trace_atexit() {
    if (g_summary_at_exit)
        recomp_trace_calls_flush();
}

// The hook. It must be C linkage: it is stored in the table the generated C
// reads.
void trace_dispatch(X86 *c, uint32_t slot) {
    if (slot >= g_trace_count)
        return;
    TraceState *st = &g_traces[slot];

    const uint32_t esp = c->r[R_ESP];
    const uint32_t caller = read_stack(esp, 0);
    uint32_t args[kArgs];
    for (uint32_t i = 0; i < kArgs; ++i)
        args[i] = read_stack(esp, 4 + 4 * i);

    ++st->calls;
    const bool verbose = st->calls <= g_full_limit;

    uint32_t entry_dumps[kMaxDumps] = {};
    uint8_t entry_valid[kMaxDumps] = {};
    if (verbose && st->dump_count)
        read_dumps(st, c, args, entry_dumps, entry_valid);
    if (verbose)
        log_entry(st, c, caller, args, entry_dumps, entry_valid);

    if (st->next)
        st->next(c, st->index);
    else if (st->module)
        st->module->base_ptrs[st->index](c);
    else
        recomp_base_ptrs[st->index](c);

    // The post-call values are the ones that show whether the traced state is
    // advancing between calls, so the change signature is built from them.
    uint32_t dumps[kMaxDumps] = {};
    uint8_t valid[kMaxDumps] = {};
    if (st->dump_count)
        read_dumps(st, c, args, dumps, valid);

    bool changed = !st->have_last || st->last_ret != c->r[R_EAX] ||
                   memcmp(st->last_args, args, sizeof args) != 0;
    if (st->dump_count) {
        if (memcmp(st->last_dumps, dumps, st->dump_count * sizeof dumps[0]) != 0 ||
            memcmp(st->last_valid, valid, st->dump_count) != 0)
            changed = true;
    }
    memcpy(st->last_args, args, sizeof args);
    st->last_ret = c->r[R_EAX];
    if (st->dump_count) {
        memcpy(st->last_dumps, dumps, st->dump_count * sizeof dumps[0]);
        memcpy(st->last_valid, valid, st->dump_count);
    }
    st->have_last = 1;

    if (verbose) {
        ++st->logged;
        log_return(st, c, dumps, valid);
    } else if (changed) {
        log_changed(st, c, caller, args, dumps, valid);
        ++st->logged;
        st->suppressed = 0;
    } else {
        ++st->suppressed;
    }

    maybe_summary();
}

void arm(const RecompModule *module, uint32_t addr, const char *label, const DumpSpec *dumps,
         uint32_t dump_count) {
    int32_t index = module ? module_lookup(module, addr) : main_lookup(addr);
    if (index < 0) {
        fprintf(stderr, "[trace-call] 0x%08x%s is not a translated entry; not traced\n", addr,
                module ? " in that module" : "");
        return;
    }
    if (find_by_index(module, (uint32_t)index)) {
        fprintf(stderr, "[trace-call] 0x%08x already traced\n", addr);
        return;
    }
    if (g_trace_count >= kMaxTraces) {
        fprintf(stderr, "[trace-call] too many addresses (max %u); 0x%08x ignored\n", kMaxTraces,
                addr);
        return;
    }

    TraceState &st = g_traces[g_trace_count];
    st = TraceState{};
    st.addr = addr;
    st.index = (uint32_t)index;
    st.module = module;
    if (label && *label) {
        snprintf(st.label, sizeof st.label, "%s", label);
    } else if (module) {
        snprintf(st.label, sizeof st.label, "%s!0x%08x", module->name, addr);
    } else {
        snprintf(st.label, sizeof st.label, "fn_%08x", addr);
    }
    st.dump_count = dump_count;
    for (uint32_t i = 0; i < dump_count; ++i)
        st.dumps[i] = dumps[i];

    // Chain to whatever hook was already published (the default base runner or
    // a mod's dispatch), then publish ours: pointer first, flag second, both
    // release.
    RecompHookFn *slot_ptr;
    uint8_t *slot_flag;
    if (module) {
        slot_ptr = &module->hook_ptrs[index];
        slot_flag = &module->hooked[index];
    } else {
        slot_ptr = &recomp_hook_ptrs[index];
        slot_flag = &recomp_hooked[index];
    }
    st.next = __atomic_load_n(slot_ptr, __ATOMIC_ACQUIRE);
    __atomic_store_n(&recomp_hooks_ever, (uint8_t)1, __ATOMIC_RELEASE);
    __atomic_store_n(&recomp_slow_routes, (uint8_t)1, __ATOMIC_RELEASE);
    __atomic_store_n(slot_ptr, kSlotFns[g_trace_count], __ATOMIC_RELEASE);
    __atomic_store_n(slot_flag, (uint8_t)1, __ATOMIC_RELEASE);
    if (!module)
        recomp_slow_update((uint32_t)index);
    ++g_trace_count;
}

bool is_mod_char(char c) {
    return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_' ||
           c == '.' || c == '-';
}

int reg_index(const char *p) {
    static const char *const names[] = {"eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi"};
    for (int i = 0; i < 8; ++i) {
        const char *n = names[i];
        if ((p[0] == n[0] || p[0] == (char)(n[0] - 32)) &&
            (p[1] == n[1] || p[1] == (char)(n[1] - 32)) &&
            (p[2] == n[2] || p[2] == (char)(n[2] - 32)))
            return i;
    }
    return -1;
}

// Parse one `{...}` dump list starting after the opening brace. Returns the
// pointer past the closing brace (or at the end).
const char *parse_dumps(const char *p, DumpSpec *dumps, uint32_t *count) {
    *count = 0;
    while (*p && *p != '}') {
        while (*p == ' ' || *p == '\t')
            ++p;
        if (*p == ',') {
            ++p;
            continue;
        }
        const char *start = p;
        DumpSpec d{};
        if (p[0] == 'a' && p[1] == 'r' && p[2] == 'g' && p[3] >= '0' && p[3] <= '9') {
            d.base_kind = DUMP_ARG;
            d.base_index = (uint8_t)(p[3] - '0');
            p += 4;
        } else {
            int reg = reg_index(p);
            if (reg < 0) {
                // Not a dump we understand: skip to the next separator.
                while (*p && *p != ',' && *p != '}')
                    ++p;
                continue;
            }
            d.base_kind = DUMP_REG;
            d.base_index = (uint8_t)reg;
            p += 3;
        }
        d.off = 0;
        if (*p == '+') {
            ++p;
            char *end = nullptr;
            d.off = (uint32_t)strtoul(p, &end, 16);
            p = end;
        }
        d.size = 4;
        if (p[0] == '.' && (p[1] == 'b' || p[1] == 'B')) {
            d.size = 1;
            p += 2;
        }
        size_t n = (size_t)(p - start);
        if (n >= sizeof d.text)
            n = sizeof d.text - 1;
        memcpy(d.text, start, n);
        d.text[n] = 0;
        while (n && (d.text[n - 1] == ' ' || d.text[n - 1] == '\t'))
            d.text[--n] = 0;
        if (*count < kMaxDumps)
            dumps[(*count)++] = d;
        while (*p && *p != ',' && *p != '}')
            ++p;
    }
    if (*p == '}')
        ++p;
    return p;
}

// Parse a comma-separated list of items:
//   [Module!]<hex>[:<label>][{<dump>,...}]
// The label runs to the next `{` or comma and may contain spaces. Dump commas
// are protected by the braces.
void parse(const char *spec) {
    const char *p = spec;
    while (*p && g_trace_count < kMaxTraces) {
        while (*p == ' ' || *p == '\t' || *p == '\n')
            ++p;
        if (*p == ',') {
            ++p;
            continue;
        }
        if (!*p)
            break;

        const RecompModule *module = nullptr;
        const char *name_start = p;
        while (is_mod_char(*p))
            ++p;
        size_t name_len = (size_t)(p - name_start);
        if (*p == '!') {
            ++p;
            module = find_module(name_start, name_len);
            if (!module) {
                fprintf(stderr, "[trace-call] unknown module '%.*s' in trace spec; skipped\n",
                        (int)name_len, name_start);
                while (*p && *p != ',')
                    ++p;
                continue;
            }
        } else {
            p = name_start;
        }

        char *end = nullptr;
        unsigned long value = strtoul(p, &end, 16);
        if (end == p) {
            while (*p && *p != ',')
                ++p;
            continue;
        }
        p = end;

        char label[kLabelCap] = "";
        if (*p == ':') {
            ++p;
            size_t n = 0;
            while (*p && *p != '{' && *p != ',' && n < sizeof label - 1)
                label[n++] = *p++;
            label[n] = 0;
            while (n && (label[n - 1] == ' ' || label[n - 1] == '\t'))
                label[--n] = 0;
        }

        DumpSpec dumps[kMaxDumps];
        uint32_t dump_count = 0;
        if (*p == '{')
            p = parse_dumps(p + 1, dumps, &dump_count);

        while (*p && *p != ',')
            ++p;

        arm(module, (uint32_t)value, label, dumps, dump_count);
    }
}

} // namespace

extern "C" void recomp_trace_calls_init(void) {
    if (g_installed)
        return;
    const char *spec = recomp_env("TRACE_CALLS");
    if (!spec || !*spec)
        return;
    if (!RECOMP_CALL_HOOKS) {
        fprintf(stderr, "[recomp] RECOMP_TRACE_CALLS ignored: this build has no call hooks "
                        "([game] call_hooks = false)\n");
        return;
    }

    if (const char *full = recomp_env("TRACE_CALLS_FULL"))
        g_full_limit = (uint32_t)strtoul(full, nullptr, 10);
    if (const char *period = recomp_env("TRACE_CALLS_PERIOD")) {
        double seconds = strtod(period, nullptr);
        if (seconds > 0)
            g_period_ns = (uint64_t)(seconds * 1e9);
    }

    parse(spec);
    if (!g_trace_count)
        return;

    g_installed = 1;
    g_last_summary_ns = os_monotonic_ns();
    g_summary_at_exit = 1;
    atexit(trace_atexit);

    fprintf(stderr, "[trace-call] tracing %u address%s (full=%u period=%.1fs)\n", g_trace_count,
            g_trace_count == 1 ? "" : "es", g_full_limit, (double)g_period_ns / 1e9);
    for (uint32_t i = 0; i < g_trace_count; ++i) {
        const TraceState &st = g_traces[i];
        fprintf(stderr, "[trace-call]   %s 0x%08x", st.label, st.addr);
        if (st.dump_count)
            fprintf(stderr, " (%u dumped word%s)", st.dump_count, st.dump_count == 1 ? "" : "s");
        fprintf(stderr, "\n");
    }
}

extern "C" void recomp_trace_calls_flush(void) {
    if (!g_trace_count)
        return;
    for (uint32_t i = 0; i < g_trace_count; ++i)
        print_summary(g_traces[i]);
}

extern "C" uint64_t recomp_trace_calls_total(uint32_t addr) {
    for (uint32_t i = 0; i < g_trace_count; ++i)
        if (g_traces[i].addr == addr)
            return g_traces[i].calls;
    return 0;
}

extern "C" uint64_t recomp_trace_calls_logged(uint32_t addr) {
    for (uint32_t i = 0; i < g_trace_count; ++i)
        if (g_traces[i].addr == addr)
            return g_traces[i].logged;
    return 0;
}

extern "C" uint64_t recomp_trace_calls_suppressed(uint32_t addr) {
    for (uint32_t i = 0; i < g_trace_count; ++i)
        if (g_traces[i].addr == addr)
            return g_traces[i].suppressed;
    return 0;
}
