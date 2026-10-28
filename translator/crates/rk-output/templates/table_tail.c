#include "reloc.h"

int recomp_is_call_return(uint32_t target)
{
    uint32_t off = target - recomp_call_return_min;
    if (off >= recomp_call_return_span)
        return 0;
    return (recomp_call_return_bits[off >> 5] >> (off & 31)) & 1u;
}

int recomp_region_enter(X86 *c, uint32_t target)
{
    return recomp_region_call(c, recomp_regions, recomp_region_count, target);
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

static int32_t recomp_lookup(uint32_t target)
{
    const RecompDispatchEntry *entry = recomp_dispatch_lookup(&recomp_dispatch, target);
    return entry ? (int32_t)(entry->metadata & 0x7fffffffu) : -1;
}

int32_t recomp_index_of(uint32_t addr) { return recomp_lookup(addr); }

int recomp_thunk_target_kind(uint32_t target)
{
    if (recomp_lookup(target) >= 0 || recomp_module_lookup(target) >= 0 ||
        recomp_region_has_target(recomp_regions, recomp_region_count, target) ||
        recomp_module_region_has_target(target)) return 1;
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
    if (recomp_module_call(c, target)) return;
    if (target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) {
        if (RECOMP_UNLIKELY(!imports_dispatch(c, target))) recomp_shim_call(c, target);
        return;
    }
    if (recomp_region_enter(c, target)) return;
    if (recomp_is_call_return(target) || recomp_module_is_call_return(target)) { c->eip = target; return; }
    recomp_unknown_jump(c, target);
}

void recomp_unknown_jump(X86 *c, uint32_t target)
{
    if (recomp_run_thunk(c, target)) return;
    if (recomp_region_enter(c, target)) return;
    if (recomp_reloc_call(c, target)) return;
    /* The address is code the map does not carry: record it (RECOMP_DISCOVERY)
     * so it can be added in Ghidra, then let the interpreter carry the run
     * past the gap so one run reports every gap it reaches. */
    discovery_note("jump", target, c->eip);
    if (interp_call(c, target)) return;
    discovery_write();
    observe_write();
    fprintf(stderr, "recomp: no block entry for indirect jump to 0x%08x from 0x%08x\n",
            target, c->eip);
    abort();
}
