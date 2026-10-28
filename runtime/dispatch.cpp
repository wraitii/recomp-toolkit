#include "x86.h"
#include "profile.h"

#include <stdio.h>
#include <stdlib.h>

RECOMP_COLD void recomp_dispatch_overflow(uint32_t target, uint32_t max_probe) {
    fprintf(stderr, "recomp: dispatch placement for %08x exceeds %u extra probes\n", target,
            max_probe);
    abort();
}

void recomp_dispatch_init(RecompDispatchEntry *entries, const RecompDispatchTable *table,
                          const RecompDispatchEntry *seeds, uint32_t seed_count) {
    for (uint32_t i = 0; i < seed_count; ++i) {
        const RecompDispatchEntry seed = seeds[i];
        uint32_t offset = seed.address - table->base;
        if (offset >= table->span || !seed.fn)
            recomp_dispatch_overflow(seed.address, table->max_probe);
        uint32_t home = offset >> table->shift;
        uint32_t slot = home;
        while (entries[slot].fn) {
            if (slot - home >= table->max_probe)
                recomp_dispatch_overflow(seed.address, table->max_probe);
            ++slot;
        }
        entries[slot] = seed;
        if (slot != home)
            entries[home].metadata |= 0x80000000u;
    }
}

const RecompDispatchEntry *recomp_dispatch_lookup(const RecompDispatchTable *table,
                                                  uint32_t target) {
    if (!table->span) {
        uint32_t lo = 0, hi = table->count;
        while (lo < hi) {
            uint32_t mid = lo + (hi - lo) / 2;
            if (table->entries[mid].address < target)
                lo = mid + 1;
            else
                hi = mid;
        }
        return lo < table->count && table->entries[lo].address == target && table->entries[lo].fn
                   ? &table->entries[lo]
                   : nullptr;
    }
    uint32_t offset = target - table->base;
    if (offset >= table->span)
        return nullptr;
    uint32_t slot = offset >> table->shift;
    const RecompDispatchEntry *entry = &table->entries[slot];
    if (entry->address == target)
        return entry;
    if (!(entry->metadata & 0x80000000u))
        return nullptr;
    for (uint32_t probe = 1; probe <= table->max_probe; ++probe) {
        entry = &table->entries[slot + probe];
        if (entry->address == target)
            return entry;
        if (!entry->fn)
            break;
    }
    return nullptr;
}
const RecompRegion *recomp_region_find(const RecompRegion *regions, uint32_t count,
                                       uint32_t target) {
    uint32_t lo = 0, hi = count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (regions[mid].start <= target)
            lo = mid + 1;
        else
            hi = mid;
    }
    return lo && target < regions[lo - 1].end ? &regions[lo - 1] : nullptr;
}

int recomp_region_has_target(const RecompRegion *regions, uint32_t count, uint32_t target) {
    const RecompRegion *r = recomp_region_find(regions, count, target);
    if (!r)
        return 0;
    uint32_t offset = target - r->start;
    return (r->boundaries[offset >> 3] >> (offset & 7u)) & 1u;
}

void recomp_region_invoke(X86 *c, const RecompRegion *r, uint32_t target) {
    if (!recomp_profile_enabled && !recomp_frame_watch) {
        recomp_execution_checkpoint(c);
        // No post-call work: guest loops may re-enter through this tail jump.
        r->fn(c, target);
        return;
    }
    RecompSaved saved;
    recomp_save(c, &saved);
    recomp_execution_checkpoint(c);
    if (recomp_profile_enabled)
        recomp_profile_push(r->index);
    r->fn(c, target);
    if (recomp_profile_enabled)
        recomp_profile_pop();
    if (recomp_frame_watch)
        recomp_check_saved(c, target, &saved);
}

int recomp_region_call(X86 *c, const RecompRegion *regions, uint32_t count, uint32_t target) {
    const RecompRegion *r = recomp_region_find(regions, count, target);
    if (!r)
        return 0;
    recomp_region_invoke(c, r, target);
    return 1;
}
