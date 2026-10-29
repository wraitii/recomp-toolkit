#include "imports.h"
#include "game_config.h"
#include "../platform/os.h"
#include "gdi32_internal.h"
#include "user32_internal.h"
#include "kernel32_internal.h"
#include "seh.h"
#include "profile.h"
#include "mods_seam.h"

// Defined in kernel32.cpp with the scheduler.
void sched_checkpoint();
#include "memory.h"

#include <map>
#include <set>
#include <cctype>
#include <vector>
#include <string>
#include <algorithm>
#include <unordered_map>

// Registered unsupported APIs retain their arity for observers and diagnostics,
// but execution must stop until a real implementation is available.
void imports_unsupported(X86 *) {
    abort();
}

namespace {

constexpr uint8_t kTraceFiltered = 1;
constexpr uint8_t kTraceVerbose = 2;

struct ImportTracing {
    const char *filter = nullptr;
    uint8_t flags = 0;
};

ImportTracing g_import_tracing;

constexpr uint8_t kDispatchAtomic = 1;
constexpr uint8_t kDispatchSlow = 2;

struct TrampDispatch {
    void (*fn)(X86 *) = nullptr;
    uint32_t calls = 0;
    uint16_t pop = 4;
    uint8_t argc = 0;
    uint8_t flags = 0;

    void set(void (*handler)(X86 *), uint8_t args) {
        fn = handler;
        argc = args;
        pop = args >= ARGC_UNKNOWN ? 4 : 4 + 4u * args;
        flags = (flags & kDispatchAtomic) |
                ((!fn || fn == imports_unsupported || args == ARGC_UNKNOWN) ? kDispatchSlow : 0);
    }
};

TrampDispatch g_dispatch[TRAMP_MAX];
uint32_t g_tramp_count = 0;

void host_arm(uint32_t idx, const std::string &key);

struct Tramp {
    std::string dll;
    std::string name;
    std::string desc; // "DLL!name", stable storage for imports_describe
    TrampDispatch *dispatch = nullptr;
};

// Imports whose callers have just set up state the call reads back, with no
// import in between: a thread switch at their entry checkpoint would let
// another thread replace it. Delphi's VCL sets its creation-control global and
// calls CreateWindowEx at once; the window procedure reads it on the first
// message.
bool atomic_import(const std::string &key) {
    return key == "user32.dll!CreateWindowExA" || key == "user32.dll!CreateWindowExW";
}

// Deliberately leaked: an atexit hook (RECOMP_IMPORT_STATS) reads these after
// static destructors would otherwise have run.
std::vector<Tramp> &tramps() {
    static auto *v = [] {
        g_import_tracing.filter = recomp_env("TRACE_IMPORTS");
        g_import_tracing.flags =
            (g_import_tracing.filter ? kTraceFiltered : 0) | (log_level() >= 2 ? kTraceVerbose : 0);
        return new std::vector<Tramp>();
    }();
    return *v;
}
std::map<std::string, uint32_t> &index() {
    static auto *m = new std::map<std::string, uint32_t>();
    return *m;
}
std::map<std::string, ImportShim> &registry() {
    static auto *m = new std::map<std::string, ImportShim>();
    return *m;
}
std::set<std::string> &registered_dlls() {
    static auto *s = new std::set<std::string>();
    return *s;
}
std::set<std::string> &strict_dlls() {
    static auto *s = new std::set<std::string>();
    return *s;
}

struct DataImport {
    uint32_t size = 0;
    uint32_t addr = 0;
    std::string desc;
};
std::map<std::string, DataImport> &data_imports() {
    static auto *m = new std::map<std::string, DataImport>();
    return *m;
}

std::string lower_dll(const char *dll) {
    std::string k(dll ? dll : "");
    std::transform(k.begin(), k.end(), k.begin(),
                   [](unsigned char ch) { return (char)tolower(ch); });
    return k;
}

std::string key_of(const char *dll, const char *name) {
    std::string k = lower_dll(dll);
    k += '!';
    k += name ? name : "";
    return k;
}

// x86 stdcall exports may carry their stack byte count in _name@bytes.
// Only use this ABI evidence when no explicit signature was registered;
// fastcall (@name@bytes), C++ mangling and malformed suffixes remain unknown.
uint8_t decorated_stdcall_args(const char *name) {
    if (!name || name[0] != '_')
        return ARGC_UNKNOWN;
    const char *suffix = strrchr(name, '@');
    if (!suffix || suffix <= name + 1 || !suffix[1])
        return ARGC_UNKNOWN;
    unsigned bytes = 0;
    for (const char *p = suffix + 1; *p; ++p) {
        if (*p < '0' || *p > '9')
            return ARGC_UNKNOWN;
        bytes = bytes * 10 + unsigned(*p - '0');
        if (bytes > 4u * (ARGC_UNKNOWN - 1))
            return ARGC_UNKNOWN;
    }
    return bytes % 4 ? ARGC_UNKNOWN : uint8_t(bytes / 4);
}

} // namespace

void imports_register(const ImportShim *shims, size_t count) {
    for (size_t i = 0; i < count; ++i) {
        const ImportShim &s = shims[i];
        if (s.dll && *s.dll)
            registered_dlls().insert(lower_dll(s.dll));
        std::string k = key_of(s.dll, s.name);
        auto it = registry().find(k);
        if (it != registry().end() && it->second.fn && !s.fn)
            continue;
        registry()[k] = s;
        // If a trampoline already exists, upgrade it in place.
        auto ti = index().find(k);
        if (ti != index().end()) {
            TrampDispatch &d = g_dispatch[ti->second];
            d.set(s.fn ? s.fn : d.fn, s.argc_stdcall);
            host_arm(ti->second, k);
        }
    }
}

bool imports_has_dll(const char *dll_lower) {
    return registered_dlls().count(lower_dll(dll_lower)) != 0;
}

void imports_register_strict(const char *dll) {
    registered_dlls().insert(lower_dll(dll));
    strict_dlls().insert(lower_dll(dll));
}

bool imports_is_strict_dll(const char *dll_lower) {
    return strict_dlls().count(lower_dll(dll_lower)) != 0;
}

uint32_t imports_alloc_trampoline(const char *dll, const char *name, void (*fn)(X86 *),
                                  uint8_t argc_stdcall) {
    std::string k = key_of(dll, name);
    auto it = index().find(k);
    if (it != index().end()) {
        TrampDispatch &d = g_dispatch[it->second];
        if (fn) {
            // The loader can reinstall its unsupported handler on image reload.
            // An unknown replacement signature must not erase known ABI evidence.
            uint8_t argc = argc_stdcall == ARGC_UNKNOWN ? d.argc : argc_stdcall;
            if (argc == ARGC_UNKNOWN)
                argc = decorated_stdcall_args(name);
            d.set(fn, argc);
        }
        host_arm(it->second, k);
        return TRAMP_BASE + TRAMP_STRIDE * it->second;
    }

    Tramp t;
    t.dll = dll ? dll : "?";
    t.name = name ? name : "?";
    t.desc = t.dll + "!" + t.name;

    auto ri = registry().find(k);
    if (ri != registry().end() && !fn) {
        fn = ri->second.fn;
        argc_stdcall = ri->second.argc_stdcall;
    }
    if (argc_stdcall == ARGC_UNKNOWN)
        argc_stdcall = decorated_stdcall_args(name);

    uint32_t idx = (uint32_t)tramps().size();
    if (idx >= TRAMP_MAX) {
        LOGW("imports: trampoline space exhausted (%u entries) for %s", idx, t.desc.c_str());
        return 0;
    }
    t.dispatch = &g_dispatch[idx];
    t.dispatch->flags = atomic_import(k) ? kDispatchAtomic : 0;
    t.dispatch->set(fn, argc_stdcall);
    tramps().push_back(t);
    g_tramp_count = idx + 1;
    index()[k] = idx;
    host_arm(idx, k);
    return TRAMP_BASE + TRAMP_STRIDE * idx;
}

// The data imports D3DPopTB.exe has. weanetr.dll exports three GUIDs (16 bytes
// each) and options_to_parity_table (an int array; 4 KB of zeroed storage is
// what the plan specifies). Registering more is how a later task adds its own.
void imports_register_data(const char *dll, const char *name, uint32_t size) {
    std::string k = key_of(dll, name);
    DataImport &d = data_imports()[k];
    if (d.size < size)
        d.size = size;
    if (d.desc.empty())
        d.desc = std::string(dll) + "!" + name;
}

static void register_default_data_imports() {
    static bool done = false;
    if (done)
        return;
    done = true;
    imports_register_data("weanetr.dll", "?BFAID_INet@@3U_GUID@@A", 16);
    imports_register_data("weanetr.dll", "?BFAID_MODEM@@3U_GUID@@A", 16);
    imports_register_data("weanetr.dll", "?BFSPGUID_MODEM@@3U_GUID@@A", 16);
    imports_register_data("weanetr.dll", "?options_to_parity_table@@3PAHA", 4096);
}

uint32_t imports_alloc_data(const char *dll, const char *name) {
    register_default_data_imports();
    auto it = data_imports().find(key_of(dll, name));
    if (it == data_imports().end())
        return 0;
    DataImport &d = it->second;
    // mem_init() invalidates the heap, so a stale address is re-allocated.
    if (!d.addr || !heap_owns(d.addr)) {
        d.addr = heap_alloc(d.size, true);
        if (!d.addr)
            LOGW("imports: cannot allocate %u bytes for data import %s", d.size, d.desc.c_str());
        else
            LOGV("data import %s -> %u zeroed bytes at %08x", d.desc.c_str(), d.size, d.addr);
    }
    return d.addr;
}

uint32_t imports_data_address(const char *dll, const char *name) {
    auto it = data_imports().find(key_of(dll, name));
    return it == data_imports().end() ? 0 : it->second.addr;
}

uint32_t imports_data_count() {
    return (uint32_t)data_imports().size();
}

uint32_t imports_trampoline_for(const char *dll, const char *name) {
    auto it = index().find(key_of(dll, name));
    if (it == index().end())
        return 0;
    return TRAMP_BASE + TRAMP_STRIDE * it->second;
}

uint32_t imports_resolve(const char *dll, const char *name) {
    uint32_t a = imports_trampoline_for(dll, name);
    if (a)
        return a;
    auto ri = registry().find(key_of(dll, name));
    // GetProcAddress spells an ordinal "#N" while IAT imports and the shim
    // tables register it as "ordN". They name the same export, so resolve both
    // spellings to the one registration; otherwise a game that resolves an
    // ordinal at run time (LHMultiplayerR asks ws2_32 for #115) misses a shim
    // its IAT import would have found.
    std::string ordinal;
    if (ri == registry().end() && name && name[0] == '#') {
        ordinal = "ord" + std::string(name + 1);
        ri = registry().find(key_of(dll, ordinal.c_str()));
    }
    if (ri == registry().end())
        return 0;
    return imports_alloc_trampoline(ri->second.dll, ri->second.name, ri->second.fn,
                                    ri->second.argc_stdcall);
}

bool imports_serves_module(const char *dll) {
    if (!dll || !*dll)
        return false;
    // key_of applies the same DLL normalization as registration/resolution.
    // Including '!' makes the prefix match the complete module name only.
    std::string prefix = key_of(dll, nullptr);
    auto it = registry().lower_bound(prefix);
    return it != registry().end() && it->first.compare(0, prefix.size(), prefix) == 0;
}

const char *imports_describe(uint32_t target) {
    if (!imports_is_trampoline(target))
        return nullptr;
    uint32_t idx = (target - TRAMP_BASE) / TRAMP_STRIDE;
    if (idx >= tramps().size())
        return nullptr;
    return tramps()[idx].desc.c_str();
}

uint8_t imports_argc(uint32_t trampoline) {
    if (!imports_is_trampoline(trampoline))
        return 0;
    uint32_t idx = (trampoline - TRAMP_BASE) / TRAMP_STRIDE;
    return idx < tramps().size() ? g_dispatch[idx].argc : 0;
}

uint32_t imports_count() {
    return (uint32_t)tramps().size();
}

uint32_t imports_call_count(const char *dll, const char *name) {
    auto it = index().find(key_of(dll, name));
    if (it == index().end())
        return 0;
    return g_dispatch[it->second].calls;
}

void imports_dump_stats(FILE *out) {
    std::vector<const Tramp *> v;
    for (const Tramp &t : tramps())
        if (t.dispatch->calls)
            v.push_back(&t);
    std::sort(v.begin(), v.end(), [](const Tramp *a, const Tramp *b) {
        return a->dispatch->calls > b->dispatch->calls;
    });
    fprintf(out, "import calls (%zu of %zu entries used):\n", v.size(), tramps().size());
    for (const Tramp *t : v)
        fprintf(out, "  %8u  %-40s %s\n", t->dispatch->calls, t->desc.c_str(),
                t->dispatch->fn ? "" : "(logging only)");
}

void imports_coverage(uint32_t *implemented, uint32_t *logging_only, uint32_t *unknown_argc) {
    uint32_t impl = 0, log_only = 0, unknown = 0;
    for (const Tramp &t : tramps()) {
        if (t.dispatch->fn)
            ++impl;
        else
            ++log_only;
        if (t.dispatch->argc == ARGC_UNKNOWN)
            ++unknown;
    }
    if (implemented)
        *implemented = impl;
    if (logging_only)
        *logging_only = log_only;
    if (unknown_argc)
        *unknown_argc = unknown;
}

void imports_dump_coverage(FILE *out) {
    std::map<std::string, std::pair<uint32_t, uint32_t>> per_dll; // implemented, logging-only
    std::vector<const Tramp *> log_only, unknown;
    for (const Tramp &t : tramps()) {
        auto &e = per_dll[t.dll];
        if (t.dispatch->fn)
            ++e.first;
        else {
            ++e.second;
            log_only.push_back(&t);
        }
        if (t.dispatch->argc == ARGC_UNKNOWN)
            unknown.push_back(&t);
    }
    fprintf(out, "import coverage by DLL (implemented / logging-only):\n");
    for (const auto &kv : per_dll)
        fprintf(out, "  %-16s %3u / %3u\n", kv.first.c_str(), kv.second.first, kv.second.second);
    fprintf(out, "logging-only imports (%zu):\n", log_only.size());
    for (const Tramp *t : log_only)
        fprintf(out, "  %s\n", t->desc.c_str());
    fprintf(out, "imports with an unknown stdcall argument count (%zu):\n", unknown.size());
    for (const Tramp *t : unknown)
        fprintf(out, "  %s\n", t->desc.c_str());
    fprintf(out, "data imports backed by guest storage (%zu):\n", data_imports().size());
    for (const auto &kv : data_imports())
        fprintf(out, "  %-44s %5u bytes at %08x\n", kv.second.desc.c_str(), kv.second.size,
                kv.second.addr);
}

void imports_dump_report(FILE *out) {
    std::vector<const Tramp *> called, not_reached, log_only;
    for (const Tramp &t : tramps()) {
        if (!t.dispatch->fn)
            log_only.push_back(&t);
        else if (t.dispatch->calls)
            called.push_back(&t);
        else
            not_reached.push_back(&t);
    }
    auto by_name = [](const Tramp *a, const Tramp *b) { return a->desc < b->desc; };
    std::sort(called.begin(), called.end(), by_name);
    std::sort(not_reached.begin(), not_reached.end(), by_name);
    std::sort(log_only.begin(), log_only.end(), by_name);

    fprintf(out, "implemented and called (%zu):\n", called.size());
    for (const Tramp *t : called)
        fprintf(out, "  %-44s %u calls\n", t->desc.c_str(), t->dispatch->calls);
    fprintf(out, "implemented but not reached (%zu):\n", not_reached.size());
    for (const Tramp *t : not_reached)
        fprintf(out, "  %s\n", t->desc.c_str());
    fprintf(out, "logging-only (%zu), of which called: ", log_only.size());
    uint32_t hit = 0;
    for (const Tramp *t : log_only)
        if (t->dispatch->calls)
            ++hit;
    fprintf(out, "%u\n", hit);
    for (const Tramp *t : log_only)
        fprintf(out, "  %-44s %s\n", t->desc.c_str(), t->dispatch->calls ? "CALLED" : "");
}

namespace {
ImportReturnObserver g_return_observer = nullptr;
ImportCallObserver g_call_observer = nullptr;

std::unordered_map<std::string, uint32_t> &host_index() {
    static auto *index = [] {
        auto *m = new std::unordered_map<std::string, uint32_t>();
        for (uint32_t i = 0; i < recomp_host_method_count; ++i)
            (*m)[recomp_host_methods[i].identity] = i;
        return m;
    }();
    return *index;
}

std::vector<int32_t> &host_tramps() {
    static auto *v = new std::vector<int32_t>(recomp_host_method_count, -1);
    return *v;
}

void host_arm_slot(uint32_t slot, uint32_t idx) {
    const RecompHostMethod &m = recomp_host_methods[slot];
    TrampDispatch &d = g_dispatch[idx];
    host_tramps()[slot] = (int32_t)idx;
    if (d.fn == m.fn && d.pop != m.pop) {
        LOGW("imports: %s pops %u bytes but its method fact pops %u", m.identity, d.pop, m.pop);
        fflush(stderr);
        abort();
    }
    const bool direct = d.fn == m.fn && !(d.flags & (kDispatchSlow | kDispatchAtomic)) &&
                        !recomp_observe_on && !recomp_frame_watch && !g_import_tracing.flags &&
                        !g_call_observer && !g_return_observer;
    recomp_host_slots[slot].calls = &d.calls;
    recomp_host_slots[slot].tramp = direct ? TRAMP_BASE + TRAMP_STRIDE * idx : 1u;
}

void host_arm(uint32_t idx, const std::string &key) {
    auto it = host_index().find(key);
    if (it != host_index().end())
        host_arm_slot(it->second, idx);
}

void host_refresh() {
    for (uint32_t slot = 0; slot < recomp_host_method_count; ++slot)
        if (host_tramps()[slot] >= 0)
            host_arm_slot(slot, (uint32_t)host_tramps()[slot]);
}
} // namespace

void imports_set_return_observer(ImportReturnObserver fn) {
    g_return_observer = fn;
    host_refresh();
}

void imports_set_call_observer(ImportCallObserver fn) {
    g_call_observer = fn;
    host_refresh();
}

ImportReturnObserver imports_set_return_observer_get(void) {
    return g_return_observer;
}

ImportCallObserver imports_set_call_observer_get(void) {
    return g_call_observer;
}

// Dump raw ABI words without dereferencing pointer-valued arguments. Use wide
// address arithmetic so a corrupt stack cannot wrap the preview into low memory.
// The arena is mapped except for its null guard; bypass faulting guest accessors
// so diagnostics neither raise guest exceptions nor mutate guest state.
static void diagnostic_word(const X86 *c, uint32_t offset, const char *label, unsigned index) {
    const uint64_t at = uint64_t(c->r[R_ESP]) + offset;
    fprintf(stderr, "  %s[%u] @0x%08llx = ", label, index, (unsigned long long)at);
    if (!g_mem || at < GUEST_NULL_LIMIT || at + 4 > GUEST_SIZE) {
        fprintf(stderr, "<unreadable>\n");
        return;
    }
    uint32_t value;
    memcpy(&value, g_mem + at, sizeof value);
    fprintf(stderr, "0x%08x\n", value);
}

static void diagnose_unsupported_import(const X86 *c, const char *desc, uint8_t argc) {
    fprintf(stderr, "unsupported import %s\n", desc);
    fprintf(stderr,
            "  ESP=0x%08x EAX=0x%08x EBX=0x%08x ECX=0x%08x EDX=0x%08x\n"
            "  ESI=0x%08x EDI=0x%08x EBP=0x%08x EIP=0x%08x\n",
            c->r[R_ESP], c->r[R_EAX], c->r[R_EBX], c->r[R_ECX], c->r[R_EDX], c->r[R_ESI],
            c->r[R_EDI], c->r[R_EBP], c->eip);
    diagnostic_word(c, 0, "return_address", 0);
    const bool known = argc != ARGC_UNKNOWN && argc != ARGC_CDECL;
    if (known)
        fprintf(stderr, "  ABI: stdcall, %u stack argument words (raw 32-bit values)\n",
                unsigned(argc));
    else
        fprintf(stderr,
                "  ABI: %s; argument count unknown; raw stack preview (not identified arguments)\n",
                argc == ARGC_CDECL ? "cdecl" : "unknown");
    for (unsigned i = 0; i < (known ? unsigned(argc) : 8u); ++i)
        diagnostic_word(c, 4 + i * 4, known ? "arg" : "stack", i);
    fflush(stderr);
}

// Dispatch a guest import trampoline at a scheduler checkpoint.
// Copy dispatch metadata before calling a shim, then restore EIP/ESP according to its calling convention.
uint64_t recomp_import_calls = 0;
namespace {
#ifdef RECOMP_CHECK_CALL_FACTS
thread_local uint64_t g_guest_entries = 0;

bool cw_effect_allows(uint32_t ret, uint16_t before, uint16_t after) {
    uint32_t lo = 0, hi = recomp_cw_effect_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (recomp_cw_effects[mid].ret < ret)
            lo = mid + 1;
        else
            hi = mid;
    }
    if (lo == recomp_cw_effect_count || recomp_cw_effects[lo].ret != ret)
        return before == after;
    const RecompCwEffect &e = recomp_cw_effects[lo];
    return !((before ^ after) & e.preserved) && (after & e.known) == e.value;
}
#endif
} // namespace
uint64_t imports_call_count() {
    return recomp_import_calls;
}
void imports_call_leaves_surfaces() {
    --recomp_import_calls;
}

[[gnu::noinline]] static void trace_import_enter(X86 *c, const char *desc, uint8_t argc,
                                                 uint32_t ret_addr) {
    if (g_import_tracing.flags & kTraceVerbose)
        log_msg(2, "-> %s (esp=%08x ret=%08x)", desc, c->r[R_ESP], ret_addr);
    const char *trace_filter = g_import_tracing.filter;
    auto trace_match = [&]() {
        if (!trace_filter)
            return false;
        const char *p = trace_filter;
        while (*p) {
            const char *e = strchr(p, ',');
            size_t n = e ? (size_t)(e - p) : strlen(p);
            if (n && n < 128) {
                char part[128];
                memcpy(part, p, n);
                part[n] = 0;
                if (strstr(desc, part))
                    return true;
            }
            if (!e)
                break;
            p = e + 1;
        }
        return false;
    };
    if (trace_filter != nullptr && trace_match()) {
        char line[512];
        int n = snprintf(line, sizeof line, "import: %s ret=%08x", desc, ret_addr);
        uint32_t na = (argc == ARGC_CDECL || argc == ARGC_UNKNOWN) ? 0u : (uint32_t)argc;
        for (uint32_t i = 1; i < na && i < 8 && n > 0 && n < (int)sizeof line - 40; ++i) {
            uint32_t v = rd32(c->r[R_ESP] + 4 + 4 * i);
            float f;
            memcpy(&f, &v, 4);
            n += snprintf(line + n, sizeof line - (size_t)n, " %08x(%g)", v, (double)f);
        }
        LOGW("%s", line);
    }
    if (g_import_tracing.flags & kTraceVerbose)
        recomp_seh_validate_chain_body(c, "import enter", desc);
}

[[gnu::noinline]] static bool imports_dispatch_slow(X86 *c, uint32_t idx, void (*fn)(X86 *),
                                                    uint8_t argc, uint8_t tracing) {
    std::vector<Tramp> &table = tramps();
    const uint16_t pop = g_dispatch[idx].pop;
    // A guest exception can longjmp across fn(c). Keep no C++ owner live
    // across that call, and do not retain a pointer into the movable vector.
    // The copy exists because the shim can reallocate `tramps()`; only take it
    // when a consumer is active. Unknown arity and null/unsupported shims also
    // need the description for their diagnostics.
    // RECOMP_TRACE_IMPORTS=<substring>[,<substring>...] logs every import whose "dll!name"
    // contains the substring, with its stdcall arguments (hex and as float)
    // and the guest return address.
    const bool need_desc = tracing || g_call_observer != nullptr || fn == nullptr ||
                           fn == imports_unsupported || argc == ARGC_UNKNOWN;
    char desc[512];
    desc[0] = 0;
    if (need_desc) {
        const std::string &d = table[idx].desc;
        size_t n = std::min(d.size(), sizeof desc - 1);
        memcpy(desc, d.data(), n);
        desc[n] = 0;
    }
    ++g_dispatch[idx].calls;
    ++recomp_import_calls;

    uint32_t ret_addr = rd32(c->r[R_ESP]);
    // The guest EIP is only advanced at call boundaries, so without this a
    // shim that stops (an abort) reports the PREVIOUS import's return address.
    // Nothing reads EIP across a running shim except diagnostics and SEH, and
    // the generated code restores it from the stack on RET, so publishing the
    // current call's return address here is the honest value.
    c->eip = ret_addr;
    if (tracing)
        trace_import_enter(c, desc, argc, ret_addr);

    // The arguments as they are NOW, before the shim runs: a stdcall shim pops
    // them, so after the call they are gone and this is the only point they
    // can be read. Only meaningful for a known stdcall count; a cdecl or
    // unknown shim reports none rather than inventing them.
#ifdef RECOMP_CHECK_CALL_FACTS
    const uint16_t cw_before = c->fpu_cw;
    const uint64_t entries_before = g_guest_entries;
#endif
    bool skip = false;
    if (g_call_observer) {
        uint32_t args[8];
        uint32_t n = (argc == ARGC_CDECL || argc == ARGC_UNKNOWN) ? 0u : (uint32_t)argc;
        if (n > 8)
            n = 8;
        for (uint32_t i = 0; i < n; ++i)
            args[i] = rd32(c->r[R_ESP] + 4 + 4 * i);
        uint32_t result = 0;
        if (!g_call_observer(desc, args, n, &result)) {
            set_eax(c, result);
            skip = true;
        }
    }

    if (skip) {
        // The observer refused the call and has set the result itself.
    } else if (fn) {
        if (fn == imports_unsupported) {
            x86_cc_settle(c);
            diagnose_unsupported_import(c, desc, argc);
        }
        fn(c);
    } else if (RECOMP_STRICT_IMPORTS && argc == ARGC_UNKNOWN) {
        // A game profile can require that an import with no known stdcall
        // arity stop the run instead of returning 0 with its arguments left
        // on the stack. Both the diagnostic and the abort name dll!function.
        x86_cc_settle(c);
        diagnose_unsupported_import(c, desc, argc);
        imports_unsupported(c);
    } else {
        x86_cc_settle(c);
        if (log_once(desc, "unimplemented import %s: returning 0", desc))
            diagnose_unsupported_import(c, desc, argc);
        set_eax(c, 0);
    }
#ifdef RECOMP_CHECK_CALL_FACTS
    if (g_guest_entries == entries_before && !cw_effect_allows(ret_addr, cw_before, c->fpu_cw))
        recomp_import_cw_violation(table[idx].desc.c_str(), cw_before, c->fpu_cw);
#endif
    // Read after the shim ran, so the pointer is into the table as it is now.
    if (g_return_observer)
        g_return_observer(table[idx].desc.c_str(), c->r[R_EAX], ret_addr);
    if (tracing & kTraceVerbose)
        log_msg(2, "<- %s (eax=%08x)", desc, c->r[R_EAX]);

    if (argc == ARGC_UNKNOWN) {
        char key[sizeof desc + 6];
        snprintf(key, sizeof key, "%s#argc", desc);
        log_once(key,
                 "%s has an unknown stdcall argument count: not adjusting ESP, "
                 "the guest stack will drift if it is really stdcall",
                 desc);
    }
    c->r[R_ESP] += pop;
    c->eip = ret_addr;
    if (tracing & kTraceVerbose)
        recomp_seh_validate_chain_body(c, "import leave", desc);
    return true;
}

bool imports_dispatch(X86 *c, uint32_t target) {
    if (RECOMP_UNLIKELY(g_call_observer || g_return_observer))
        x86_cc_settle(c);
    if (!imports_is_trampoline(target))
        return false;
    uint32_t idx = (target - TRAMP_BASE) / TRAMP_STRIDE;
    TrampDispatch &d = g_dispatch[idx];
    if (idx >= g_tramp_count || !(d.flags & kDispatchAtomic))
        recomp_execution_checkpoint_charge(c, RECOMP_IMPORT_CHECKPOINT_COST);
    if (RECOMP_UNLIKELY(idx >= g_tramp_count)) {
        LOGW("imports: call to unallocated trampoline %08x (ESP=%08x, return=%08x)", target,
             c->r[R_ESP], rd32(c->r[R_ESP]));
        fflush(stderr);
        abort();
    }
    void (*fn)(X86 *) = d.fn;
    uint8_t argc = d.argc;
    const uint8_t tracing = g_import_tracing.flags;
#ifdef RECOMP_CHECK_CALL_FACTS
    return imports_dispatch_slow(c, idx, fn, argc, tracing);
#else
    if (RECOMP_UNLIKELY(tracing || g_call_observer || g_return_observer ||
                        (d.flags & kDispatchSlow)))
        return imports_dispatch_slow(c, idx, fn, argc, tracing);
    ++g_dispatch[idx].calls;
    ++recomp_import_calls;
    const uint32_t ret_addr = rd32(c->r[R_ESP]);
    c->eip = ret_addr;
    const uint16_t pop = d.pop;
    fn(c);
    if (RECOMP_UNLIKELY(g_return_observer))
        g_return_observer(tramps()[idx].desc.c_str(), c->r[R_EAX], ret_addr);
    c->r[R_ESP] += pop;
    c->eip = ret_addr;
    return true;
#endif
}

// ---------------------------------------------------------------------------
// Calling guest code from a shim.
// ---------------------------------------------------------------------------
namespace {
struct Callback {
    Callback *next;
    X86 *cpu;
    jmp_buf env;
    uint32_t saved_esp, saved_eip, return_sp, profile_depth;
};
struct Callbacks {
    std::vector<Callback *> stack;
    Callback *free = nullptr;

    Callback *acquire() {
        if (!free)
            return new Callback{};
        Callback *call = free;
        free = call->next;
        return call;
    }

    void recycle(Callback *call) {
        call->next = free;
        free = call;
    }

    ~Callbacks() {
        for (Callback *call : stack)
            delete call;
        while (free) {
            Callback *call = free;
            free = call->next;
            delete call;
        }
    }
};
Callbacks &callback_state() {
    static thread_local Callbacks *cached = nullptr;
    if (!cached) {
        static thread_local Callbacks owned;
        cached = &owned;
    }
    return *cached;
}
} // namespace

uint32_t recomp_callback_depth(void) {
    return (uint32_t)callback_state().stack.size();
}

void recomp_callback_truncate(uint32_t depth) {
    Callbacks &local = callback_state();
    while (local.stack.size() > depth) {
        local.recycle(local.stack.back());
        local.stack.pop_back();
    }
}

void recomp_callback_reset(X86 *c) {
    Callbacks &local = callback_state();
    for (size_t i = local.stack.size(); i-- > 0;)
        if (!c || local.stack[i]->cpu == c) {
            local.recycle(local.stack[i]);
            local.stack.erase(local.stack.begin() + i);
        }
}

void recomp_callback_return(X86 *c) {
    Callbacks &local = callback_state();
    if (local.stack.empty())
        return;
    Callback *call = local.stack.back();
    // Every translated return here is deeper on the host stack than this
    // live guest_call. Match the guest return slot as well: a completing SEH
    // landing may RET to an older callback's identical sentinel, in which
    // case intercept must finish that function through its SEH checkpoint.
    if (call->cpu != c || c->r[R_ESP] < call->return_sp + 4 || c->r[R_ESP] > call->saved_esp)
        return;
    mods_hooks_unwind_to_esp(call->return_sp);
    recomp_profile_truncate(call->profile_depth);
    RECOMP_LONGJMP(call->env, 1);
}

namespace {
struct HostCall {
    uint32_t fn, nargs;
    RecompHostCall call;
};
HostCall g_host_calls[32];
uint32_t g_host_call_count = 0;

RecompHostCall host_call_for(uint32_t fn, int nargs) {
    for (uint32_t i = 0; i < g_host_call_count; ++i) {
        const HostCall &entry = g_host_calls[i];
        if (entry.fn != fn || entry.nargs != (uint32_t)nargs)
            continue;
        if (recomp_hooks_ever) {
            int32_t index = recomp_index_of(fn);
            if (index >= 0 && __atomic_load_n(&recomp_hooked[index], __ATOMIC_ACQUIRE))
                return nullptr;
        }
        return entry.call;
    }
    return nullptr;
}
} // namespace

extern "C" void recomp_register_host_call(uint32_t fn, uint32_t nargs, RecompHostCall call) {
    if (g_host_call_count == std::size(g_host_calls)) {
        fprintf(stderr, "recomp_register_host_call: no room for %08x\n", fn);
        abort();
    }
    g_host_calls[g_host_call_count++] = {fn, nargs, call};
}

extern "C" RecompHostCall recomp_host_call(uint32_t fn, uint32_t nargs) {
    return g_host_call_count ? host_call_for(fn, (int)nargs) : nullptr;
}

extern "C" uint32_t recomp_guest_call(X86 *c, uint32_t fn, const uint32_t *args, int nargs) {
    return guest_call(c, fn, args, nargs);
}

uint32_t guest_call(X86 *c, uint32_t fn, const uint32_t *args, int nargs) {
#ifdef RECOMP_CHECK_CALL_FACTS
    ++g_guest_entries;
#endif
    if (g_host_call_count)
        if (RecompHostCall host = host_call_for(fn, nargs))
            return host(c, args);
    if (imports_is_trampoline(fn)) {
        const uint32_t saved_esp = c->r[R_ESP], saved_eip = c->eip;
        uint32_t esp = saved_esp;
        for (int i = nargs - 1; i >= 0; --i) {
            esp -= 4;
            wr32(esp, args[i]);
        }
        esp -= 4;
        wr32(esp, GUEST_RETURN_SENTINEL);
        c->r[R_ESP] = esp;
        imports_dispatch(c, fn);
        c->r[R_ESP] = saved_esp;
        c->eip = saved_eip;
        return c->r[R_EAX];
    }
    Callbacks &local = callback_state();
    Callback *call = local.acquire();
    call->cpu = c;
    call->saved_esp = c->r[R_ESP];
    call->saved_eip = c->eip;
    call->profile_depth = recomp_profile_depth();
    uint32_t esp = call->saved_esp;
    for (int i = nargs - 1; i >= 0; --i) {
        esp -= 4;
        wr32(esp, args[i]);
    }
    esp -= 4;
    wr32(esp, GUEST_RETURN_SENTINEL);
    c->r[R_ESP] = esp;
    call->return_sp = esp;
    local.stack.push_back(call);
    if (!RECOMP_SETJMP(call->env))
        recomp_run(c, fn);
    recomp_seh_callback_leave(c, static_cast<uint32_t>(local.stack.size()));
    uint32_t eax = c->r[R_EAX];
    c->r[R_ESP] = call->saved_esp;
    c->eip = call->saved_eip;
    local.stack.pop_back();
    local.recycle(call);
    return eax;
}

// ---------------------------------------------------------------------------
// Shim tables owned by this runtime.
// ---------------------------------------------------------------------------
void imports_init() {
    static bool done = false;
    if (done)
        return;
    done = true;
    imports_register(g_kernel32_shims, g_kernel32_shim_count);
    kernel32_wide_register();
    oleaut32_register();
    misc_dlls_register();
    msvcr90_register();
    msvcr71_register();
    steam_api_register();
    comctl32_register();
    media_foundation_register();
    imports_register(g_user32_shims, g_user32_shim_count);
    user32_wide_register();
    user32_vcl_register();
    imports_register(g_misc_shims, g_misc_shim_count);
    imports_register(g_gdi32_shims, g_gdi32_shim_count);
    extern void gdi_model_register();
    gdi_model_register();
    extern void gdi_bitmaps_register();
    gdi_bitmaps_register();
    gdi::register_draw();
    gdi::register_text();
    extern void msimg32_register();
    msimg32_register();
    extern void imagehlp_register();
    imagehlp_register();
}
