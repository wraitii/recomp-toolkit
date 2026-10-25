// reloc.cpp - binds heap-loaded bodies to translated templates and dispatches them.

#include "reloc.h"
#include "guest.h"
#include "loader.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vector>

namespace {

constexpr uint32_t kBuckets = 4096;
constexpr uint32_t kCacheSize = 1u << 14;

struct Run {
    uint32_t start, length;
};

struct Key {
    uint32_t template_index;
    uint32_t entry;
    uint32_t key;
};

struct Index {
    std::vector<const RecompRelocTemplate *> templates;
    std::vector<std::vector<Run>> runs;
    std::vector<Key> candidates;
    std::vector<uint32_t> heads;
    std::vector<uint32_t> next;
    std::vector<uint32_t> short_entries;
    std::vector<uint8_t> logged;
};

Index *g_index = nullptr;
uint64_t g_cache[kCacheSize];

std::vector<const RecompRelocTemplate *> &templates() {
    static std::vector<const RecompRelocTemplate *> v;
    return v;
}

bool reloc_enabled() {
    static int enabled = -1;
    int value = __atomic_load_n(&enabled, __ATOMIC_RELAXED);
    if (value < 0) {
        const char *setting = getenv("RECOMP_RELOC");
        value = !(setting && (!strcmp(setting, "off") || !strcmp(setting, "0")));
        __atomic_store_n(&enabled, value, __ATOMIC_RELAXED);
    }
    return value != 0;
}

uint32_t bucket_of(uint32_t key) {
    return (key * 2654435761u) >> (32 - 12);
}

uint32_t cache_slot(uint32_t target) {
    return (target * 2654435761u) >> (32 - 14);
}

bool masked_at(const RecompRelocTemplate *t, uint32_t offset) {
    uint32_t lo = 0, hi = t->masked_count;
    while (lo < hi) {
        uint32_t mid = lo + (hi - lo) / 2;
        if (t->masked[mid] < offset)
            lo = mid + 1;
        else
            hi = mid;
    }
    return lo < t->masked_count && t->masked[lo] == offset;
}

std::vector<Run> unmasked_runs(const RecompRelocTemplate *t) {
    std::vector<Run> runs;
    uint32_t start = 0;
    for (uint32_t i = 0; i < t->masked_count; ++i) {
        uint32_t mask = t->masked[i];
        if (mask > start)
            runs.push_back({start, mask - start});
        start = mask + 1;
    }
    if (start < t->code_len)
        runs.push_back({start, t->code_len - start});
    return runs;
}

bool verify_body(const Index *index, uint32_t template_index, uint32_t base) {
    const RecompRelocTemplate *t = index->templates[template_index];
    if (base < HEAP_BASE || base > HEAP_LIMIT || t->code_len > HEAP_LIMIT - base)
        return false;
    if (loader_in_image(base) || loader_in_image(base + t->code_len - 1))
        return false;
    const uint8_t *guest = g_mem + base;
    for (const Run &run : index->runs[template_index])
        if (memcmp(guest + run.start, t->code + run.start, run.length) != 0)
            return false;
    for (uint32_t i = 0; i < t->import_count; ++i) {
        const RecompRelocImport &import = t->imports[i];
        if (import.slot > t->code_len || t->code_len - import.slot < 4)
            return false;
        uint32_t got;
        memcpy(&got, guest + import.slot, 4);
        if (got != import.target - (base + import.slot) - 4)
            return false;
    }
    return true;
}

uint32_t candidate_key(const RecompRelocTemplate *t, uint32_t offset) {
    uint32_t key = 0;
    for (uint32_t i = 0; i < 4; ++i) {
        uint8_t byte = masked_at(t, offset + i) ? 0 : t->code[offset + i];
        key |= (uint32_t)byte << (8 * i);
    }
    return key;
}

void add_candidate(Index &index, uint32_t template_index, uint32_t entry) {
    const RecompRelocTemplate *t = index.templates[template_index];
    uint32_t offset = t->entries[entry].offset;
    if (offset >= t->code_len || t->code_len - offset < 4) {
        index.short_entries.push_back((uint32_t)index.candidates.size());
        index.candidates.push_back({template_index, entry, 0});
        index.next.push_back(0);
        return;
    }
    uint32_t key = candidate_key(t, offset);
    uint32_t bucket = bucket_of(key);
    index.candidates.push_back({template_index, entry, key});
    index.next.push_back(index.heads[bucket]);
    index.heads[bucket] = (uint32_t)index.candidates.size();
}

Index *build_index() {
    Index *index = new Index;
    index->templates = templates();
    index->heads.assign(kBuckets, 0);
    index->logged.assign(index->templates.size(), 0);
    for (uint32_t template_index = 0; template_index < index->templates.size(); ++template_index) {
        const RecompRelocTemplate *t = index->templates[template_index];
        index->runs.push_back(unmasked_runs(t));
        for (uint32_t entry = 0; entry < t->entry_count; ++entry)
            add_candidate(*index, template_index, entry);
    }
    return index;
}

bool lookup(const Index *index, uint32_t target, uint32_t &template_index, uint32_t &entry_index) {
    if (target < HEAP_BASE + 4 || target > HEAP_LIMIT - 4)
        return false;
    for (uint32_t zeroed = 0; zeroed < 16; ++zeroed) {
        uint32_t key = 0;
        for (uint32_t i = 0; i < 4; ++i) {
            uint8_t byte = zeroed & (1u << i) ? 0 : rd8(target + i);
            key |= (uint32_t)byte << (8 * i);
        }
        for (uint32_t slot = index->heads[bucket_of(key)]; slot; slot = index->next[slot - 1]) {
            const Key &candidate = index->candidates[slot - 1];
            if (candidate.key != key)
                continue;
            const RecompRelocTemplate *t = index->templates[candidate.template_index];
            uint32_t offset = t->entries[candidate.entry].offset;
            if (offset > target)
                continue;
            if (verify_body(index, candidate.template_index, target - offset)) {
                template_index = candidate.template_index;
                entry_index = candidate.entry;
                return true;
            }
        }
    }
    for (uint32_t item : index->short_entries) {
        const Key &candidate = index->candidates[item];
        const RecompRelocTemplate *t = index->templates[candidate.template_index];
        uint32_t offset = t->entries[candidate.entry].offset;
        if (offset > target)
            continue;
        if (verify_body(index, candidate.template_index, target - offset)) {
            template_index = candidate.template_index;
            entry_index = candidate.entry;
            return true;
        }
    }
    return false;
}

bool cached(Index *index, uint32_t target, uint32_t &template_index, uint32_t &entry_index) {
    uint64_t entry = __atomic_load_n(&g_cache[cache_slot(target)], __ATOMIC_ACQUIRE);
    uint32_t value = (uint32_t)entry;
    if (!value || (uint32_t)(entry >> 32) != target)
        return false;
    uint32_t encoded = value - 1;
    template_index = encoded >> 16;
    entry_index = encoded & 0xffff;
    if (template_index >= index->templates.size() ||
        entry_index >= index->templates[template_index]->entry_count)
        return false;
    uint32_t offset = index->templates[template_index]->entries[entry_index].offset;
    if (offset > target || !verify_body(index, template_index, target - offset)) {
        __atomic_store_n(&g_cache[cache_slot(target)], 0, __ATOMIC_RELEASE);
        return false;
    }
    return true;
}

bool bind(Index *index, uint32_t target, uint32_t &template_index, uint32_t &entry_index) {
    if (!lookup(index, target, template_index, entry_index))
        return false;
    uint64_t value = (uint64_t)((template_index << 16) | entry_index) + 1;
    __atomic_store_n(&g_cache[cache_slot(target)], ((uint64_t)target << 32) | value,
                     __ATOMIC_RELEASE);
    const RecompRelocTemplate *t = index->templates[template_index];
    if (template_index < index->logged.size() &&
        !__atomic_exchange_n(&index->logged[template_index], 1, __ATOMIC_RELAXED))
        log_msg(1, "reloc: bound %s at %08x (entry %08x)", t->name,
                target - t->entries[entry_index].offset, target);
    return true;
}

} // namespace

extern "C" void recomp_reloc_register(const RecompRelocTemplate *table, uint32_t count) {
    std::vector<const RecompRelocTemplate *> &all = templates();
    for (uint32_t i = 0; i < count; ++i)
        if (table[i].code && table[i].code_len && table[i].entry_count)
            all.push_back(&table[i]);
    for (uint64_t &entry : g_cache)
        __atomic_store_n(&entry, 0, __ATOMIC_RELEASE);
    Index *index = build_index();
    __atomic_store_n(&g_index, index, __ATOMIC_RELEASE);
}

extern "C" int recomp_reloc_call(X86 *c, uint32_t target) {
    if (!reloc_enabled())
        return 0;
    Index *index = __atomic_load_n(&g_index, __ATOMIC_ACQUIRE);
    if (!index)
        return 0;
    uint32_t template_index, entry_index;
    if (!cached(index, target, template_index, entry_index) &&
        !bind(index, target, template_index, entry_index))
        return 0;
    const RecompRelocTemplate *t = index->templates[template_index];
    uint32_t base = target - t->entries[entry_index].offset;
    recomp_execution_checkpoint(c);
    t->entries[entry_index].fn(c, base);
    return 1;
}
