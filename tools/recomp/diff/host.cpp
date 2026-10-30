#include <stdint.h>

#include <string>

#include "runtime/x86.h"

extern "C" void harness_escape(const char *reason, uint32_t addr);

void guest_sleep_ms(uint32_t ms) {
    harness_escape("sleep", ms);
}

bool guest_key_names_system() {
    return false;
}

std::string guest_key_name(uint32_t dik) {
    (void)dik;
    return std::string();
}

extern "C" uint8_t recomp_observe_on = 0;
extern "C" uint32_t recomp_observe_site = 0;
extern "C" void observe_record(uint32_t site, uint32_t target, const char *kind) {
    (void)site;
    (void)target;
    (void)kind;
}
extern "C" void observe_write(void) {}
extern "C" RecompEntryFn recomp_icall_resolve(uint32_t target) {
    const RecompDispatchEntry *entry = recomp_dispatch_lookup(&recomp_dispatch, target);
    return entry ? entry->fn : nullptr;
}
extern "C" void recomp_icall_slow(X86 *c, uint32_t target, uint32_t site) {
    observe_pend(site);
    recomp_call(c, target);
}
extern "C" void recomp_icall_miss(X86 *c, uint32_t target, uint32_t site) {
    if (target - GUEST_SHIM_BASE < GUEST_SHIM_END - GUEST_SHIM_BASE) {
        if (!imports_dispatch(c, target))
            recomp_shim_call(c, target);
    } else if (RecompEntryFn fn = recomp_icall_resolve(target))
        fn(c);
    else
        recomp_icall_slow(c, target, site);
}
extern "C" void recomp_module_invoke(X86 *c, const RecompModule *m, int32_t i) {
    recomp_unknown_call(c, m->func_addrs[i]);
}

extern "C" bool imports_dispatch(X86 *c, uint32_t target) {
    (void)c;
    harness_escape("import", target);
    return false;
}
extern "C" int recomp_module_region_has_target(uint32_t target) {
    (void)target;
    return 0;
}
