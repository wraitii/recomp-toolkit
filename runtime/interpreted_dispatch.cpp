#include "imports.h"
#include "interp.h"
#include "loader.h"
#include "seh.h"
#include "thunks.h"
#include <cstdio>
#include <cstdlib>

namespace {
[[maybe_unused]] const bool g_registered = (loader_register_interpreted_dispatch(), true);
}

extern "C" {
const RecompHostMethod recomp_host_methods[1] = {};
const uint32_t recomp_host_method_count = 0;
RecompHostSlot recomp_host_slots[1] = {};
uint32_t recomp_override_count() {
    return 0;
}
uint64_t recomp_override_hash() {
    return 0;
}
int32_t recomp_index_of(uint32_t) {
    return -1;
}
int recomp_is_call_return(uint32_t) {
    return 0;
}

void recomp_call(X86 *c, uint32_t target) {
    if (target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) {
        if (!imports_dispatch(c, target))
            recomp_shim_call(c, target);
        return;
    }
    if (target < GUEST_NULL_LIMIT || target == GUEST_RETURN_SENTINEL) {
        recomp_unknown_call(c, target);
        return;
    }
    if (recomp_run_thunk(c, target))
        return;
    if (interp_call(c, target))
        return;
    fprintf(stderr, "recomp-run: cannot execute %08x: %s\n", target, interp_last_error());
    abort();
}
void recomp_unknown_jump(X86 *c, uint32_t target) {
    recomp_call(c, target);
}
void recomp_jump(X86 *c, uint32_t target) {
    if (recomp_seh_pending_target())
        recomp_seh_intercept(c, target);
    recomp_call(c, target);
}
}
