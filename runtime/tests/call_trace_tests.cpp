// call_trace_tests.cpp - the guest call tracer against the game-free fixture.
//
// The fixture (tools/recomp/tests/gen_entry_fixture.py) emits a real function
// table and real entry dispatch, so the test exercises the same hook table the
// generated game uses. call_trace lives in recomp_runtime with weak table
// fallbacks; the fixture's strong tables override them and provide real entry
// dispatch to hook.
//
// The module cases register a synthetic RecompModule through the real runtime
// registry, so they exercise the per-module install path and the per-trace hook
// stubs that keep a main-image index and a module-local index apart.
//
// Run from the repository root:
//   .venv/bin/python tools/test.py --compile-only && build/recomp/call_trace_tests
#include "call_trace.h"
#include "x86.h"
#include "../platform/os.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern "C" void entry_0d001000(X86 *c);
extern "C" void entry_0d001010(X86 *c);
extern "C" void entry_0d001020(X86 *c);
extern "C" void entry_0d001030(X86 *c);

// A base for index 0 (a native replacement in the fixture). The rest of the
// runtime symbols the fixture references come from recomp_runtime and
// stub_recomp_call.
extern "C" void fixture_native(X86 *c) {}

// A synthetic auxiliary module. Its tables live here, not in the fixture, so
// the module install path and the module-local index are independent of the
// main image's.
static int g_mod_calls;
extern "C" void mod_base(X86 *c) {
    ++g_mod_calls;
    (void)c;
}
static const uint32_t g_mod_addrs[] = {0x20000000u, 0x20000010u};
static void (*const g_mod_base_ptrs[])(X86 *) = {mod_base, mod_base};
static RecompHookFn g_mod_hook_ptrs[2] = {nullptr, nullptr};
static uint8_t g_mod_hooked[2] = {0, 0};
static const uint32_t g_mod_call_returns[] = {0};
static const char *const g_mod_profile_names[] = {"modfn", "modfn2"};
static const RecompModule g_test_module = {
    "TestMod.dll",   0x20000000u,  0x20001000u,        g_mod_addrs, 2, g_mod_base_ptrs,
    g_mod_hook_ptrs, g_mod_hooked, g_mod_call_returns, 0,           0, g_mod_profile_names,
};

static int g_checks = 0, g_failures = 0;

static void check(bool ok, const char *what) {
    ++g_checks;
    if (!ok)
        ++g_failures;
    printf("  [%s] %s\n", ok ? "ok" : "FAIL", what);
}

// A raw store keeps the test independent of the generated store-hook path.
static void put32(uint32_t addr, uint32_t value) {
    memcpy(g_mem + addr, &value, 4);
}

int main(void) {
    // A small arena is enough: the test sets ESP into it and reads only there.
    g_mem = (uint8_t *)os_vm_reserve(65536);
    if (!g_mem)
        return 1;

    printf("\n== call trace\n");

    // The module must be registered before the tracer resolves `TestMod!...`.
    recomp_module_register(&g_test_module);

    // 1. Unset is a no-op.
    os_unsetenv("RECOMP_TRACE_CALLS");
    recomp_trace_calls_init();
    check(recomp_hooked[1] == 0 && recomp_hooked[2] == 0, "unset leaves hooks unarmed");
    check(recomp_trace_calls_total(0x0d001010u) == 0, "unset records no calls");

    // 2. Install two main-image addresses and one module address (key without
    //    the `.dll`, and with a dump list).
    os_setenv("RECOMP_TRACE_CALLS",
              "0d001010:caller,0d001020:leaf,TestMod!0x20000000:modfn{arg0+0x8,arg0+0x9.b}");
    recomp_trace_calls_init();
    check(recomp_hooked[1] == 1 && recomp_hooked[2] == 1, "both main addresses are hooked");
    check(recomp_hook_ptrs[1] != nullptr && recomp_hook_ptrs[2] != nullptr,
          "both main hook pointers are published");
    check(g_mod_hooked[0] == 1, "module key without extension resolves and hooks");
    check(g_mod_hook_ptrs[0] != nullptr, "module hook pointer is published");

    // 3. The rate limiter: 20 full calls, then the same signature is
    //    suppressed, and a changed argument logs again.
    X86 c = {};
    c.r[R_ESP] = 0x1000;
    c.r[R_EAX] = 0;
    put32(0x1000, 0xdeadbeefu); // caller return address
    for (uint32_t i = 0; i < 4; ++i)
        put32(0x1004 + 4 * i, 0x11110000u + i); // constant arguments
    for (int i = 0; i < 25; ++i) {
        c.r[R_EAX] = 0;
        entry_0d001010(&c);
    }
    check(recomp_trace_calls_total(0x0d001010u) == 25, "25 calls observed");
    check(recomp_trace_calls_logged(0x0d001010u) == 20, "first 20 calls logged");
    check(recomp_trace_calls_suppressed(0x0d001010u) == 5, "5 identical calls suppressed");

    put32(0x1004, 0x22220000u); // change an argument
    c.r[R_EAX] = 7;             // and the return value
    entry_0d001010(&c);
    check(recomp_trace_calls_total(0x0d001010u) == 26, "changed call observed");
    check(recomp_trace_calls_logged(0x0d001010u) == 21, "changed signature logs again");
    check(recomp_trace_calls_suppressed(0x0d001010u) == 0, "suppression resets on change");

    // 3b. A second traced address keeps its own counters and the nested
    //     untraced call under it does not disturb them.
    entry_0d001020(&c);
    check(recomp_trace_calls_total(0x0d001020u) == 1, "second address counted");
    check(recomp_trace_calls_total(0x0d001010u) == 26, "first address counters unchanged");

    // 4. The base still ran and guest state is what the original produces.
    check(c.r[1] == 26, "base ran for every call (EAX-adjacent state unchanged)");

    // 5. Module tracing: the module base runs through the module hook, and the
    //    main-image counters are untouched.
    g_mod_calls = 0;
    put32(0x1004, 0x3000u); // arg0 -> inside the arena
    put32(0x3008, 0xaaaaaaaau);
    memcpy(g_mem + 0x3009, "\xbb", 1);
    c.r[R_EAX] = 0;
    for (int i = 0; i < 20; ++i)
        recomp_module_call(&c, 0x20000000u);
    check(g_mod_calls == 20, "module base ran for every call");
    check(recomp_trace_calls_total(0x20000000u) == 20, "module calls counted");
    check(recomp_trace_calls_logged(0x20000000u) == 20, "first 20 module calls logged");
    check(recomp_trace_calls_total(0x0d001010u) == 26, "main counters unchanged by module");

    // 5b. A call identical in arguments and EAX but with a changed dumped word
    //     logs again: the change signature includes the dumped words.
    recomp_module_call(&c, 0x20000000u);
    check(recomp_trace_calls_total(0x20000000u) == 21, "identical module call observed");
    check(recomp_trace_calls_logged(0x20000000u) == 20, "identical module call suppressed");
    check(recomp_trace_calls_suppressed(0x20000000u) == 1, "module suppression counted");
    put32(0x3008, 0xbbbbbbbbu); // change only the dumped word
    recomp_module_call(&c, 0x20000000u);
    check(recomp_trace_calls_total(0x20000000u) == 22, "changed dump observed");
    check(recomp_trace_calls_logged(0x20000000u) == 21, "changed dump logs again");
    check(recomp_trace_calls_suppressed(0x20000000u) == 0, "module suppression resets");

    // 5c. An unmapped dump base is skipped rather than read.
    put32(0x1004, 0xfffffff0u); // base + 8 wraps past GUEST_SIZE
    recomp_module_call(&c, 0x20000000u);
    check(recomp_trace_calls_total(0x20000000u) == 23, "unmapped dump call observed without fault");

    recomp_trace_calls_flush();
    printf("\n%d checks, %d failures\n", g_checks, g_failures);
    return g_failures ? 1 : 0;
}
