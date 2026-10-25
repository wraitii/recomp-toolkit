// cpu.cpp - the CPU-level call-outs x86.h declares as "provided by
// runtime/": import dispatch, unknown call targets, divide errors
// and the handful of privileged or environment-sensing instructions.
#include "imports.h"
#include "profile.h"
#include "mods_seam.h"
#include "intrinsics.h"
#include "game_config.h"
#include "discovery.h"
#include "interp.h"
#include "loader.h"
#include "win32.h"
#include "thunks.h"
#include "seh.h"
#include "linear_trace.h"
#include "../platform/os.h"

#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <map>
#include <vector>

// ---------------------------------------------------------------------------
// State for the _setjmp/_longjmp intrinsics, outside extern "C" because these
// helpers have C++ types.
// ---------------------------------------------------------------------------
namespace {
struct SetjmpRecord {
    uint32_t buf = 0;
    X86 saved{};
    jmp_buf env;
    bool armed = false;
    uint32_t profile_depth = 0;
    uint32_t callback_depth = 0;
};
// Keyed by the guest jmp_buf address. Heap allocated so a record outlives the
// frame that created it.
std::map<uint32_t, SetjmpRecord *> &setjmps() {
    static std::map<uint32_t, SetjmpRecord *> m;
    return m;
}
// Read after a longjmp, so it must not be a local of the returning frame.
X86 *g_setjmp_ctx = nullptr;
} // namespace

namespace {
std::vector<UnknownCall> &unknown_calls() {
    static std::vector<UnknownCall> v;
    return v;
}
} // namespace

const std::vector<UnknownCall> &recomp_unknown_calls() {
    return unknown_calls();
}

// ---------------------------------------------------------------------------
// Auxiliary module registry. Modules register from static constructors, so
// the vector is function-local to survive any construction order.
// ---------------------------------------------------------------------------
namespace {
std::vector<const RecompModule *> &modules() {
    static std::vector<const RecompModule *> v;
    return v;
}
int32_t module_index(const RecompModule *m, uint32_t target) {
    if (m->dispatch) {
        const RecompDispatchEntry *entry = recomp_dispatch_lookup(m->dispatch, target);
        return entry ? (int32_t)(entry->metadata & 0x7fffffffu) : -1;
    }
    uint32_t lo = 0, hi = m->func_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (m->func_addrs[mid] < target)
            lo = mid + 1;
        else
            hi = mid;
    }
    return lo < m->func_count && m->func_addrs[lo] == target ? (int32_t)lo : -1;
}
bool module_is_call_return(const RecompModule *m, uint32_t target) {
    uint32_t lo = 0, hi = m->call_return_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (m->call_returns[mid] < target)
            lo = mid + 1;
        else
            hi = mid;
    }
    return lo < m->call_return_count && m->call_returns[lo] == target;
}

const uint32_t kModuleCacheSize = 1u << 16;
const uint32_t kNoModule = 31u;
uint64_t g_module_return_cache[kModuleCacheSize];
uint32_t g_module_cache_slot(uint32_t target) {
    return (uint32_t)(target * 2654435761u) >> 16;
}
uint32_t module_position(const RecompModule *m) {
    const std::vector<const RecompModule *> &v = modules();
    for (uint32_t i = 0; i < v.size(); ++i)
        if (v[i] == m)
            return i;
    return kNoModule;
}
struct ModuleEntry {
    const RecompModule *module;
    int32_t index;
};
ModuleEntry module_entry(uint32_t target) {
    const RecompModule *m = recomp_module_containing(target);
    return {m, m ? module_index(m, target) : -1};
}
int module_region_enter(X86 *c, uint32_t target) {
    const RecompModule *m = recomp_module_containing(target);
    return m ? recomp_region_call(c, m->regions, m->region_count, target) : 0;
}
bool module_call_return(uint32_t target) {
    uint32_t slot = g_module_cache_slot(target);
    uint64_t e = __atomic_load_n(&g_module_return_cache[slot], __ATOMIC_ACQUIRE);
    if ((uint32_t)(e >> 32) == target && (uint32_t)e)
        return (uint32_t)e & 1u;
    const RecompModule *m = recomp_module_containing(target);
    bool ret = m && module_is_call_return(m, target);
    uint32_t pos = m ? module_position(m) : kNoModule;
    if (!m || pos < kNoModule) {
        uint32_t mod = m ? pos + 1 : kNoModule;
        __atomic_store_n(&g_module_return_cache[slot],
                         ((uint64_t)target << 32) | (mod << 1) | (ret ? 1u : 0u), __ATOMIC_RELEASE);
    }
    return ret;
}
const RecompModule *g_last_module = nullptr;
} // namespace

extern "C" {

// See x86.h: set by the mod hook installer, read by contract call sites.
uint8_t recomp_hooks_ever = 0;

void recomp_run(X86 *c, uint32_t target) {
    c->eip = target;
    recomp_call(c, target);
}

int recomp_module_region_has_target(uint32_t target) {
    const RecompModule *m = recomp_module_containing(target);
    return m && recomp_region_has_target(m->regions, m->region_count, target);
}

RecompEntryFn recomp_icall_resolve(uint32_t target) {
    const RecompDispatchEntry *entry = recomp_dispatch_lookup(&recomp_dispatch, target);
    if (!entry) {
        const RecompModule *m = recomp_module_containing(target);
        if (m && m->dispatch)
            entry = recomp_dispatch_lookup(m->dispatch, target);
    }
    return entry ? entry->fn : nullptr;
}

void recomp_icall_slow(X86 *c, uint32_t target, uint32_t site) {
#ifndef RECOMP_CHECK_CALL_FACTS
    if (target - GUEST_SHIM_BASE < GUEST_SHIM_END - GUEST_SHIM_BASE && !recomp_frame_watch &&
        !recomp_observe_on) {
        if (RECOMP_UNLIKELY(!imports_dispatch(c, target)))
            recomp_shim_call(c, target);
        return;
    }
#endif
    observe_pend(site);
    recomp_call(c, target);
}

void recomp_module_register(const RecompModule *m) {
    modules().push_back(m);
    for (uint64_t &entry : g_module_return_cache)
        __atomic_store_n(&entry, 0, __ATOMIC_RELEASE);
}
uint32_t recomp_module_count(void) {
    return (uint32_t)modules().size();
}
const RecompModule *recomp_module_at(uint32_t i) {
    return i < modules().size() ? modules()[i] : nullptr;
}
const RecompModule *recomp_module_containing(uint32_t addr) {
    const RecompModule *m = __atomic_load_n(&g_last_module, __ATOMIC_RELAXED);
    if (m && addr >= m->base && addr < m->end)
        return m;
    for (const RecompModule *c : modules())
        if (addr >= c->base && addr < c->end) {
            __atomic_store_n(&g_last_module, c, __ATOMIC_RELAXED);
            return c;
        }
    return nullptr;
}
int32_t recomp_module_lookup(uint32_t target) {
    return module_entry(target).index;
}
int recomp_module_is_call_return(uint32_t target) {
    return module_call_return(target) ? 1 : 0;
}
int recomp_module_call(X86 *c, uint32_t target) {
    ModuleEntry e = module_entry(target);
    if (!e.module || e.index < 0)
        return module_region_enter(c, target);
    recomp_module_invoke(c, e.module, e.index);
    return 1;
}
void recomp_module_invoke(X86 *c, const RecompModule *m, int32_t i) {
    recomp_execution_checkpoint(c);
#ifdef RECOMP_NO_HOOKS
    m->base_ptrs[i](c);
#else
    if (__atomic_load_n(&m->hooked[i], __ATOMIC_ACQUIRE)) {
        x86_cc_settle(c);
        m->hook_ptrs[i](c, (uint32_t)i);
    } else
        m->base_ptrs[i](c);
#endif
}

void recomp_module_jump(X86 *c, uint32_t target) {
    ModuleEntry e = module_entry(target);
    if (!e.module || e.index < 0) {
        if (module_region_enter(c, target))
            return;
        recomp_unknown_jump(c, target);
        return;
    }
    recomp_execution_checkpoint(c);
#ifdef RECOMP_NO_HOOKS
    e.module->base_ptrs[e.index](c);
#else
    if (__atomic_load_n(&e.module->hooked[e.index], __ATOMIC_ACQUIRE)) {
        x86_cc_settle(c);
        e.module->hook_ptrs[e.index](c, (uint32_t)e.index);
    } else
        e.module->base_ptrs[e.index](c);
#endif
}

// Consumes the return address the caller pushed and continues after the call,
// which is what the callee's RET would have done. Without this a call that the
// runtime cannot deliver leaves a dword on the guest stack and every later
// Where an exception record's parameters go: just below the guest stack
// pointer, which the dispatcher copies into its own record before any handler
// runs. A fault can arrive with a stack pointer that is null, tiny or wild -
// that is often WHY it faulted - and ESP - 16 then wraps to the top of the
// address space, so writing there would take the host out instead of the
// guest. Returns false when there is nowhere safe to put them.
static bool exception_info_block(const X86 *c, uint32_t *info) {
    if (!c || c->r[R_ESP] < GUEST_NULL_LIMIT + 16)
        return false;
    const uint32_t at = c->r[R_ESP] - 16;
    if (at + 8 > GUEST_SIZE)
        return false;
    *info = at;
    return true;
}

// frame is displaced by four bytes.
static void return_as_if_ret(X86 *c) {
    c->eip = rd32(c->r[R_ESP]);
    c->r[R_ESP] += 4;
    c->r[R_EAX] = 0;
}

void recomp_shim_call(X86 *c, uint32_t target) {
    LOGW("recomp_shim_call: %08x is not an allocated trampoline (ESP=%08x, return=%08x)", target,
         c->r[R_ESP], rd32(c->r[R_ESP]));
    fflush(stdout);
    fflush(stderr);
    abort();
}

namespace {
constexpr unsigned kDynamicReturns = 64;
thread_local uint32_t dynamic_returns[kDynamicReturns];
thread_local unsigned dynamic_return_count;
} // namespace

extern "C" unsigned recomp_dynamic_return_push(uint32_t target) {
    const unsigned depth = dynamic_return_count;
    if (depth < kDynamicReturns)
        dynamic_returns[dynamic_return_count++] = target;
    return depth;
}

extern "C" void recomp_dynamic_return_pop(unsigned depth) {
    if (dynamic_return_count > depth)
        dynamic_return_count = depth;
}

extern "C" int recomp_dynamic_return_accept(uint32_t target) {
    for (unsigned i = dynamic_return_count; i; --i) {
        if (dynamic_returns[i - 1] != target)
            continue;
        dynamic_return_count = i - 1;
        return 1;
    }
    return 0;
}

#if defined(__GNUC__)
__attribute__((cold, noinline))
#endif
void recomp_unexpected_return(X86 *c, uint32_t site, uint32_t expected, uint32_t target) {
    recomp_linear_trace_on_fault();
    fprintf(
        stderr,
        "recomp: unexpected guest return to 0x%08x from site 0x%08x (expected 0x%08x, ESP=%08x)\n",
        target, site, expected, c ? c->r[R_ESP] : 0);
    os_backtrace(
        48, [](const char *frame, void *) { fprintf(stderr, "  host frame: %s\n", frame); },
        nullptr);
    if (c) {
        for (int i = -2; i < 8; ++i) {
            const uint32_t at = c->r[R_ESP] + (uint32_t)(i * 4);
            fprintf(stderr, "  stack[%+d] @%08x = %08x\n", i, at, rd32(at));
        }
    }
    fflush(stdout);
    fflush(stderr);
    abort();
}

void recomp_region_missing(X86 *c, uint32_t target) {
    LOGW("recomp_region_missing: %08x is not an instruction of its decoded region (from=%08x)",
         target, c->eip);
    fflush(stdout);
    fflush(stderr);
    abort();
}

void recomp_unknown_call(X86 *c, uint32_t target) {
    uint32_t site = observe_take();
    if (recomp_run_thunk(c, target)) {
        observe_indirect(site, target, "thunk");
        return;
    }
    if (target >= GUEST_NULL_LIMIT && recomp_region_enter(c, target)) {
        observe_indirect(site, target, "region");
        return;
    }
    // Code no translation covers runs in the interpreter when every
    // instruction in it decodes: code the guest built or copied into its heap
    // at run time, and code inside the image that discovery missed. The
    // second kind is a translator gap, so the run keeps going and reports it
    // (see discovery.h) instead of returning a zero into whatever asked.
    // Not for a target in the first 64 KB: Windows maps nothing there, so a
    // call through a null pointer faults at the call, and there are no
    // instructions at it to interpret. Reading them would be the very
    // dereference this page exists to refuse.
    discovery_note("call", target, site);
    if (target >= GUEST_NULL_LIMIT && interp_call(c, target)) {
        observe_indirect(site, target, "interp");
        return;
    }
    observe_indirect(site, target, "unknown");
    if (target == GUEST_RETURN_SENTINEL) {
        recomp_callback_return(c);
        // With no matching live callback this is an ordinary undeliverable
        // CALL, whose pushed return still needs to be consumed.
        return_as_if_ret(c);
        return;
    }
    uint32_t ret = rd32(c->r[R_ESP]);
    // Windows maps nothing in the first 64 KB, so a call there - through a nil
    // interface, whose vtable reads back as zero - faults, and the program's
    // own handlers see an access violation. A Delphi program turns that into
    // EAccessViolation, and a try/except around the call carries on: the
    // original drew speech with a surface that could be nil and relied on
    // exactly that. Returning 0 instead ran on with the garbage and aborted at
    // the next jump through it.
    if (target < 0x10000) {
        // EXCEPTION_ACCESS_VIOLATION's two parameters: an execute fault (8) at
        // the target. They sit below the stack pointer, which the dispatcher
        // copies into its own record before any handler runs.
        uint32_t info = 0;
        if (exception_info_block(c, &info)) {
            wr32(info, 8);
            wr32(info + 4, target);
            std::string chain;
            for (uint32_t frame : win32_return_chain(c->r[R_EBP], 8)) {
                char word[16];
                snprintf(word, sizeof word, " %08x", frame);
                chain += word;
            }
            uint32_t self = c->r[R_ESI], obj = c->r[R_EAX];
            uint32_t self_vt = gm_valid(self, 4) ? rd32(self) : 0;
            uint32_t obj_vt = gm_valid(obj, 4) ? rd32(obj) : 0;
            log_once("null-call",
                     "call to %08x (return=%08x EAX=%08x ECX=%08x EDX=%08x ESI=%08x EDI=%08x "
                     "EBP=%08x chain:%s self_vt=%08x obj_vt=%08x self_flags=%08x "
                     "self_20=%08x FS:[0]=%08x): raising an access violation, as Windows "
                     "would, for the guest's handlers",
                     target, ret, c->r[R_EAX], c->r[R_ECX], c->r[R_EDX], c->r[R_ESI], c->r[R_EDI],
                     c->r[R_EBP], chain.c_str(), self_vt, obj_vt,
                     gm_valid(self + 8, 4) ? rd32(self + 8) : 0u,
                     gm_valid(self + 0x20, 4) ? rd32(self + 0x20) : 0u,
                     c->fs_base && gm_valid(c->fs_base, 4) ? rd32(c->fs_base) : 0u);
            recomp_seh_raise(c, 0xc0000005u, 0, 2, info);
        }
    }
    char key[64];
    snprintf(key, sizeof key, "unknown-call:%08x", target);
    // Bounded for the same reason log_once is: a guest that generates code
    // presents a new target on every call, and a run report wants a sample of
    // them, not all of them.
    // The registers say what the call was made ON. A virtual dispatch holds the
    // object in a register and reached this target through the object's vtable,
    // so a null target beside a plausible object means the vtable slot is
    // empty, while a wild object means the pointer never was one.
    if (log_once(key,
                 "call to unknown target %08x (ESP=%08x, return=%08x, EAX=%08x EBX=%08x "
                 "ECX=%08x EDX=%08x ESI=%08x EDI=%08x): aborting",
                 target, c->r[R_ESP], ret, c->r[R_EAX], c->r[R_EBX], c->r[R_ECX], c->r[R_EDX],
                 c->r[R_ESI], c->r[R_EDI]) &&
        unknown_calls().size() < 256)
        unknown_calls().push_back(UnknownCall{target, ret});
    observe_write();
    fflush(stdout);
    fflush(stderr);
    abort();
}

#ifdef RECOMP_CHECK_CALL_FACTS
void recomp_call_fact_violation(uint32_t ret, uint32_t target) {
    fprintf(stderr, "indirect call returning to %08x reached undeclared target %08x\n", ret,
            target);
    fflush(stdout);
    abort();
}

void recomp_import_cw_violation(const char *name, uint16_t before, uint16_t after) {
    fprintf(stderr, "import %s changed x87 control word %04x -> %04x without guest callbacks\n",
            name, before, after);
    fflush(stdout);
    abort();
}
#endif

// DIV/IDIV with a zero divisor or an out-of-range quotient. On real hardware
// this raises #DE; deliver it to the guest's own handlers as Windows would,
// with the record and CONTEXT naming the faulting instruction. The registers
// are left untouched and the caller continues on the old value; with no
// handler Windows ends the process.
void recomp_div_error(X86 *c, uint32_t addr) {
    LOGW("divide error at %08x (EAX=%08x EDX=%08x): raising STATUS_INTEGER_DIVIDE_BY_ZERO", addr,
         c->r[R_EAX], c->r[R_EDX]);
    recomp_seh_raise_fault(c, 0xc0000094u, addr);
}

// A read or a write through a pointer in the first 64 KB. Windows maps
// nothing there, so the program's own handlers see an access violation and a
// try/except around the dereference carries on; a flat arena hands back a
// zero instead and lets the guest run on with it. NFS Most Wanted walked an
// std::map whose node was null, read _Isnil out of guest 0x15, got that zero
// and looped on the same node with no call into the runtime at all, until the
// watchdog ended the run three minutes later.
void recomp_null_access(uint32_t addr, int write) {
    // Reached only in a build made with -DRECOMP_NULL_CHECKS=1; see x86.h for
    // why the check is not in every build.
    //
    // Off by default even then, and deliberately. Windows would fault here and the
    // guest's own handlers would see it, but this port reaches these reads
    // with pointers Windows would have filled in: the nulls are ours, from
    // a shim that answered 0 or an object nothing built, and raising on them
    // ends the run at the first one instead of at the one that matters.
    // RECOMP_NULL_FAULTS=1 turns the page back into the hole it is on
    // Windows, which is how the std::map spin in NFS Most Wanted was found -
    // a null node whose _Isnil read back as zero, walked forever.
    static const bool fault = recomp_env("NULL_FAULTS") != nullptr;
    if (!fault)
        return;
    X86 *c = guest_current_context();
    uint32_t info = 0;
    // A fault raised at an instruction we are already raising for means the
    // handler returned us onto it to fault again. Tracking the EIP rather
    // than a flag keeps this honest across the raise's longjmp: an unrelated
    // fault elsewhere still gets its exception, and only a genuine loop on
    // one instruction is refused.
    static thread_local uint32_t raising_at = 0;
    if (c && c->eip != raising_at && exception_info_block(c, &info)) {
        // EXCEPTION_ACCESS_VIOLATION's two parameters: the kind of access,
        // 0 for a read and 1 for a write, then the address.
        raising_at = c->eip;
        wr32(info, write ? 1u : 0u);
        wr32(info + 4, addr);
        char key[64];
        snprintf(key, sizeof key, "null-%s:%08x", write ? "write" : "read", addr);
        // The EIP is the last one the translated code stored, which is a
        // call or a branch rather than this instruction: generated bodies do
        // not sync it per access. It names the neighbourhood, not the fault.
        log_once(key,
                 "%s of %08x (near EIP=%08x): raising an access violation, as Windows "
                 "would, for the guest's handlers",
                 write ? "write" : "read", addr, c->eip);
        recomp_seh_raise(c, 0xc0000005u, 0, 2, info);
        raising_at = 0; // it returned, so nothing handled it
    }
    // Returning would put the guest straight back on the same instruction to
    // fault again, which is the spin this exists to end.
    LOGW("unhandled null %s of %08x%s", write ? "write" : "read", addr,
         c ? "" : " with no guest context");
    abort();
}

void recomp_x87_contract_violation(X86 *c, uint32_t addr, uint16_t mask, uint16_t value) {
    LOGW("x87 entry contract violated at %08x: CW=%04x, expected CW & %04x == %04x", addr,
         c->fpu_cw, mask, value);
    abort();
}

// An instruction the translation left as a trap: one the translator could not
// model, or a branch it could not resolve. Continuing would run guessed code,
// so this is fatal.
void recomp_unmodelled(X86 *c, uint32_t addr) {
    LOGW("untranslated instruction at %08x (ESP=%08x, return=%08x): the translation left a trap "
         "here",
         addr, c->r[R_ESP], rd32(c->r[R_ESP]));
    abort();
}

void recomp_unmodelled_named(X86 *c, uint32_t addr, const char *reason) {
    LOGW("untranslated %s at %08x (ESP=%08x, return=%08x): the translation left a trap here",
         reason, addr, c->r[R_ESP], rd32(c->r[R_ESP]));
    abort();
}

// INT3: a breakpoint no debugger will handle. Windows hands STATUS_BREAKPOINT
// to the program's own handlers and ends the process when none takes it.
void recomp_breakpoint(X86 *c, uint32_t addr) {
    LOGW("INT3 at %08x (ESP=%08x, return=%08x): raising STATUS_BREAKPOINT", addr, c->r[R_ESP],
         rd32(c->r[R_ESP]));
    recomp_seh_raise_fault(c, 0x80000003u, addr);
    LOGW("INT3 at %08x: unhandled breakpoint, stopping", addr);
    fflush(stdout);
    fflush(stderr);
    abort();
}

// A monotonically increasing cycle counter derived from the millisecond clock,
// so repeated runs see the same ordering.
void recomp_rdtsc(X86 *c) {
    static uint64_t tsc = 0;
    uint64_t from_ms = (uint64_t)host_millis() * 1000000ull;
    if (from_ms > tsc)
        tsc = from_ms;
    tsc += 1000;
    c->r[R_EAX] = (uint32_t)tsc;
    c->r[R_EDX] = (uint32_t)(tsc >> 32);
}

// LDMXCSR. The translated SSE arithmetic honours FTZ only, so another rounding
// mode, DAZ (absent from the modelled CPU) or an unmasked exception stops here
// rather than computing under the wrong mode. DIVERGENCE(original): the sticky
// exception flags in bits 0-5 keep what the guest wrote and are never raised.
void recomp_ldmxcsr(X86 *c, uint32_t addr, uint32_t a) {
    uint32_t value = rd32(a);
    if ((value & ~0xffbfu) || (value & 0x7f80u) != 0x1f80u) {
        LOGW("LDMXCSR %08x at %08x: only round-to-nearest with every exception masked and "
             "no DAZ is modelled",
             value, addr);
        abort();
    }
    c->mxcsr = value;
}

// GenuineIntel family 6, model 7 (Pentium III), with FPU, TSC, CMOV, MMX, FXSR
// and SSE.
void recomp_cpuid(X86 *c) {
    switch (c->r[R_EAX]) {
    case 0:
        c->r[R_EAX] = 1;
        memcpy(&c->r[R_EBX], "Genu", 4);
        memcpy(&c->r[R_EDX], "ineI", 4);
        memcpy(&c->r[R_ECX], "ntel", 4);
        break;
    case 1:
        c->r[R_EAX] = 0x00000672; // family 6, model 7, stepping 2
        c->r[R_EBX] = 0;
        c->r[R_ECX] = 0;
        c->r[R_EDX] = 0x03808011; // FPU | TSC | CMOV | MMX | FXSR | SSE
        break;
    default:
        c->r[R_EAX] = c->r[R_EBX] = c->r[R_ECX] = c->r[R_EDX] = 0;
        break;
    }
}

uint32_t recomp_in(X86 *c, uint32_t port, int size) {
    (void)c;
    char key[48];
    snprintf(key, sizeof key, "in:%04x", port);
    log_once(key, "IN from port %04x (%d bytes): returning 0", port, size);
    return 0;
}

void recomp_out(X86 *c, uint32_t port, uint32_t val, int size) {
    (void)c;
    char key[48];
    snprintf(key, sizeof key, "out:%04x", port);
    log_once(key, "OUT to port %04x value %08x (%d bytes): ignored", port, val, size);
}

void recomp_cli(X86 *c) {
    (void)c;
    log_once("cli", "CLI ignored");
}
void recomp_sti(X86 *c) {
    (void)c;
    log_once("sti", "STI ignored");
}
void recomp_hlt(X86 *c) {
    (void)c;
    log_once("hlt", "HLT ignored");
}

void recomp_int(X86 *c, uint32_t vec) {
    LOGW("INT %02x at EIP %08x: no interrupt handling, continuing", vec, c->eip);
}

// ---------------------------------------------------------------------------
// _setjmp / _longjmp intrinsics. See intrinsics.h for the two substitution
// forms and why the two-call one is correct.
// ---------------------------------------------------------------------------
jmp_buf *recomp_setjmp_prepare(X86 *c) {
    uint32_t buf = rd32(c->r[R_ESP] + 4); // cdecl argument 0
    SetjmpRecord *&rec = setjmps()[buf];
    if (!rec)
        rec = new SetjmpRecord();
    rec->buf = buf;
    rec->saved = *c;
    rec->profile_depth = recomp_profile_depth();
    rec->callback_depth = recomp_callback_depth();
    rec->armed = true;
    g_setjmp_ctx = c;
    // Leave a marker in the guest jmp_buf so a stale buffer is recognisable.
    if (buf) {
        wr32(buf, 0x4d504f50u /* 'POPM' */);
        wr32(buf + 4, buf);
    }
    LOGV("_setjmp(%08x): saved guest state, ESP=%08x", buf, c->r[R_ESP]);
    return &rec->env;
}

void recomp_setjmp_return(X86 *c, int value) {
    uint32_t ret = rd32(c->r[R_ESP]);
    c->r[R_ESP] += 4; // cdecl: emulate RET, the caller pops the argument
    c->eip = ret;
    c->r[R_EAX] = (uint32_t)value;
}

// Deliberately fatal. Taking the host setjmp here would save a frame that has
// returned by the time _longjmp fires, so the only safe substitution is the
// two-call form the translator now emits at the call site.
void recomp_setjmp(X86 *c) {
    LOGW("_setjmp reached the single-call intrinsic at ESP=%08x. The call site must emit "
         "{ jmp_buf *b = recomp_setjmp_prepare(c); recomp_setjmp_return(c, setjmp(*b)); } "
         "so the host setjmp belongs to a frame that is still live when _longjmp runs.",
         c->r[R_ESP]);
    abort();
}

void recomp_longjmp(X86 *c) {
    uint32_t buf = rd32(c->r[R_ESP] + 4);
    int value = (int)rd32(c->r[R_ESP] + 8);
    auto it = setjmps().find(buf);
    if (it == setjmps().end() || !it->second->armed) {
        LOGW("_longjmp(%08x, %d): no matching _setjmp, cannot unwind", buf, value);
        abort();
    }
    if (value == 0)
        value = 1; // longjmp(buf, 0) makes setjmp return 1
    SetjmpRecord *rec = it->second;
    // A guest longjmp jumps over every host frame between here and the
    // setjmp, including any mod hook invocation sitting on them. Tell the mod
    // layer so those invocations are abandoned rather than left to run their
    // after hooks against a stack that no longer exists. Before the registers
    // are restored, because the ESP being unwound to is the saved one.
    mods_hooks_unwind_to_esp(rec->saved.r[R_ESP]);
    recomp_profile_truncate(rec->profile_depth);
    recomp_seh_callback_leave(c, rec->callback_depth + 1);
    recomp_callback_truncate(rec->callback_depth);
    *c = rec->saved; // guest registers as they were at the _setjmp
    g_setjmp_ctx = c;
    LOGV("_longjmp(%08x, %d): restoring ESP=%08x", buf, value, c->r[R_ESP]);
    longjmp(rec->env, value);
}

} // extern "C"
