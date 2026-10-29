// imports.h - import shim table, trampolines and dispatch.
//
// Every IAT slot of the loaded PE is patched to a unique trampoline address in
// the range TRAMP_BASE .. TRAMP_LIMIT. An indirect call to such an address is
// routed here by recomp_call (generated code) via imports_dispatch().
//
// Shim contract (see README.md):
//   On entry ESP points at the guest return address; arguments start at ESP+4
//   and are read with arg(c, i). The shim writes its result to EAX (and EDX
//   for 64-bit results) and returns. imports_dispatch pops the return address
//   plus 4*argc_stdcall bytes and sets EIP to the return address, so a shim
//   never touches ESP itself.
#pragma once
#include "guest.h"
#include <stdio.h>

// argc_stdcall sentinels.
static const uint8_t ARGC_CDECL = 0xff;   // caller cleans the stack: pop only the return address
static const uint8_t ARGC_UNKNOWN = 0xfe; // signature unknown: pop nothing and log loudly

enum class ImportMemory : uint8_t { Unknown, CallerStackPreserved, NoGuestWrites };

struct ImportShim {
    const char *dll;
    const char *name;
    uint8_t argc_stdcall;
    void (*fn)(X86 *); // nullptr => logging-only stub returning EAX = 0
    ImportMemory memory = ImportMemory::Unknown;
};

// Explicit unsupported handler: dispatch logs the import name and aborts.
// Use it when only the ABI is known; no guest result is fabricated.
void imports_unsupported(X86 *c);

// Registers a shim table. Later registrations override earlier ones for the
// same (dll, name); a null fn never overrides a non-null one.
void imports_register(const ImportShim *shims, size_t count);
// Declares a DLL whose unimplemented imports must stop the run under their
// own name instead of returning 0 from a logging-only stub. A table that is
// still being filled in uses this so every gap stays a named failure.
void imports_register_strict(const char *dll);
bool imports_is_strict_dll(const char *dll_lower);
// True if a shim table registered an entry for this DLL (case-insensitive).
bool imports_has_dll(const char *dll_lower);

// ---------------------------------------------------------------------------
// Return observer for import capture and diagnostics. Runs after the shim with
// its description, EAX and guest return address, without changing guest state.
// ---------------------------------------------------------------------------
typedef void (*ImportReturnObserver)(const char *desc, uint32_t eax, uint32_t caller);
void imports_set_return_observer(ImportReturnObserver fn);

// ---------------------------------------------------------------------------
// Call observer. The return observer above sees only a name and a result,
// which is enough to report a failure and not enough to reproduce a call. This
// one is given the arguments as they were on entry, read before the shim runs
// and therefore before it has moved ESP, which is the only moment they can be
// read at all.
//
// Its `allow` return decides whether the call may proceed: a capture that
// cannot reproduce a call must stop the run rather than record something it
// could not replay. Returning false makes the dispatcher skip the shim and
// leave EAX as the observer set it.
//
// One observer, installed by the capture harness under POPM_TESTING. Like the
// return observer it must not touch the guest context beyond what it is given.
// ---------------------------------------------------------------------------
typedef bool (*ImportCallObserver)(const char *desc, const uint32_t *args, uint32_t argc,
                                   uint32_t *result);
void imports_set_call_observer(ImportCallObserver fn);
// Reads back the installed observers so a temporary one can chain to, or
// restore, whatever was there rather than silence it.
ImportReturnObserver imports_set_return_observer_get(void);
ImportCallObserver imports_set_call_observer_get(void);

// Registers the KERNEL32/USER32/GDI32/ADVAPI32/SHELL32/ole32/IMM32/WSOCK32/
// WINMM tables owned by this runtime. Idempotent; called by loader_load().
void imports_init();
void oleaut32_register();
void oleaut32_reset();
void oleaut32_thread_exit(X86 *c);
void misc_dlls_register();
void msvcr90_register();
void msvcr71_register();
void steam_api_register();
// Fills the CRT's data imports once the image is mapped and the IAT is bound.
void msvcr90_init();
void comctl32_register();
void media_foundation_register();

// Allocates (or returns the existing) trampoline for dll!name. If a shim was
// registered under that name its fn/argc win unless a non-null fn is passed
// here. Used by the loader for IAT patching and by the DirectX task to expose
// COM vtable slots inside the same trampoline range.
uint32_t imports_alloc_trampoline(const char *dll, const char *name, void (*fn)(X86 *),
                                  uint8_t argc_stdcall);
// Whether a trampoline takes the slow dispatch path (no handler, unsupported, or
// unknown arity). A COM slot's handler may be a dx/com.h host entry wrapping the
// real one, so compare handler identity only through this, never by pointer.
bool imports_trampoline_slow(uint32_t target);

// ---------------------------------------------------------------------------
// Data imports. A few imports are variables, not functions (weanetr exports
// three GUIDs and a parity table). Their IAT slot must hold the address of
// guest storage, not a trampoline, or the guest reads a code address where it
// expects data. imports_alloc_data returns that address, allocating zeroed
// guest memory the first time, and 0 when dll!name is not a known data import.
// ---------------------------------------------------------------------------
void imports_register_data(const char *dll, const char *name, uint32_t size);
uint32_t imports_alloc_data(const char *dll, const char *name);
uint32_t imports_data_address(const char *dll, const char *name);
uint32_t imports_data_count();

// Import calls made so far, for a caller that needs to know whether guest
// memory may have been written behind the translated code's back: every shim
// counts, except one that says, as its first act, that it writes no
// DirectDraw surface (imports_call_leaves_surfaces).
uint64_t imports_call_count();
void imports_call_leaves_surfaces();

// Trampoline for an already-known import, or 0. Does not allocate.
uint32_t imports_trampoline_for(const char *dll, const char *name);
// Like imports_trampoline_for but allocates on demand when the name is one of
// the registered shims. Backs GetProcAddress.
uint32_t imports_resolve(const char *dll, const char *name);
// Whether at least one shim is registered for this DLL (case-insensitive).
// Backs LoadLibrary; unimplemented IAT trampolines alone do not serve a DLL.
bool imports_serves_module(const char *dll);

static inline bool imports_is_trampoline(uint32_t a) {
    return a >= TRAMP_BASE && a < TRAMP_LIMIT && ((a - TRAMP_BASE) % TRAMP_STRIDE) == 0;
}
// "DLL!name" for a trampoline address, or nullptr.
const char *imports_describe(uint32_t target);
// Stdcall argument count for an allocated trampoline, or 0 otherwise.
uint8_t imports_argc(uint32_t trampoline);

// Argument i of the current shim call: dword at ESP+4+4*i.
static inline uint32_t arg(X86 *c, int i) {
    return rd32(c->r[R_ESP] + 4 + 4 * (uint32_t)i);
}
static inline void set_eax(X86 *c, uint32_t v) {
    c->r[R_EAX] = v;
}
static inline void set_eax64(X86 *c, uint64_t v) {
    c->r[R_EAX] = (uint32_t)v;
    c->r[R_EDX] = (uint32_t)(v >> 32);
}

// Shim tables, one per runtime module. Declared here so both the defining
// translation unit and imports_init() see external linkage.
extern const ImportShim g_kernel32_shims[];
extern const size_t g_kernel32_shim_count;
extern const ImportShim g_user32_shims[];
extern const size_t g_user32_shim_count;
extern const ImportShim g_misc_shims[];
extern const size_t g_misc_shim_count;
extern const ImportShim g_gdi32_shims[];
extern const size_t g_gdi32_shim_count;

// Per-import call counts, for the runtime tests and end-of-run diagnostics.
void imports_dump_stats(FILE *out);
// Per-DLL implemented / logging-only / unknown-argc breakdown of every
// allocated trampoline, and the names in each of the latter two groups.
void imports_dump_coverage(FILE *out);
// Classifies every import as implemented-and-called, implemented-but-not-
// reached, or logging-only. Meaningful after a run of the real guest code;
// RECOMP_IMPORT_STATS=1 prints it to stderr when the process exits.
void imports_dump_report(FILE *out);
// Counts across all trampolines. Any of the out pointers may be null.
void imports_coverage(uint32_t *implemented, uint32_t *logging_only, uint32_t *unknown_argc);
uint32_t imports_call_count(const char *dll, const char *name);
uint32_t imports_count();
