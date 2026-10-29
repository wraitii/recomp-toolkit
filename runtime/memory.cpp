#include "seh.h"
#define RECOMP_GUEST_MEMORY_OWNER 1 /* defines and maps g_mem */
#include "memory.h"
#include "win32.h"
#include "loader.h"

#include "../platform/os.h"
#include <algorithm>
#include <deque>
#include <map>
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <set>
#include <string>
#include <vector>

uint8_t *g_mem = nullptr;
static uint64_t g_heap_generation = 0;

uint64_t heap_generation() {
    return g_heap_generation;
}

// ---------------------------------------------------------------------------
// The guest-memory watchpoint declared in x86.h. A stray write is only ever
// seen through the damage it does, somewhere else, later; this reports the
// write itself. The host backtrace names the generated function, which is the
// guest routine doing the writing - the one thing the corrupted value cannot
// say. Armed from RECOMP_WATCH=<hex address>[:<length>], default length 4.
// ---------------------------------------------------------------------------
uint32_t recomp_frame_watch = 0;
uint8_t recomp_slow_routes = 0;

uint32_t g_watch_base = 0;
uint32_t g_watch_len = 0;
RecompDirty g_dirty[RECOMP_DIRTY_SLOTS];
uint32_t g_dirty_count = 0;
uint32_t g_store_hook = 0;

namespace {
struct WatchArm {
    WatchArm() {
        recomp_frame_watch = recomp_env("WATCH_FRAME") ? 1u : 0u;
        if (recomp_frame_watch) {
            memset(recomp_slow, 1, recomp_func_count);
            recomp_slow_routes = 1;
        }
        const char *s = recomp_env("WATCH");
        if (!s || !*s)
            return;
        char *end = nullptr;
        const unsigned long long base = strtoull(s, &end, 16);
        unsigned long long len = 4;
        if (end && *end == ':') {
            const unsigned long long given = strtoull(end + 1, nullptr, 16);
            if (given)
                len = given;
        }
        if (base >= GUEST_SIZE)
            return;
        if (!RECOMP_STORE_HOOKS) {
            fprintf(stderr, "[recomp] RECOMP_WATCH ignored: this build has no store hooks "
                            "([game] store_hooks = false)\n");
            return;
        }
        if (len == 0 || len > GUEST_SIZE - base)
            len = GUEST_SIZE - base;
        g_watch_base = (uint32_t)base;
        g_watch_len = (uint32_t)len;
        recomp_store_hook_update();
    }
};
WatchArm g_watch_arm;

// The host call stack names the generated function - fn_00xxxxxx - and that
// name IS the guest address of the routine doing the writing, which is the
// whole point. One line per write: a watched address in a reused stack slot is
// written thousands of times, and a report long enough to read is a report too
// slow to reach the moment worth reading.
const char *watch_writer() {
    static char out[128];
    out[0] = 0;
    os_backtrace(
        32,
        [](const char *frame, void *ctx) {
            char *dst = static_cast<char *>(ctx);
            if (dst[0])
                return;
            const char *fn = strstr(frame, "fn_00");
            if (!fn)
                return;
            size_t k = 0;
            while (k + 1 < sizeof out && fn[k] && fn[k] != ' ')
                ++k;
            snprintf(dst, sizeof out, "%.*s", (int)k, fn);
        },
        out);
    return out[0] ? out : "?";
}
} // namespace

extern "C" void recomp_frame_changed(X86 *c, uint32_t target, uint32_t before, uint32_t after) {
    // Rate-limited: a routine that does this does it on every call, and the
    // first few name it as well as thousands would. EIP is where the callee
    // left off: a routine that escapes through an indirect jump its own body
    // does not cover returns from there without running its epilogue, and that
    // address is the whole diagnosis.
    static uint32_t said = 0;
    if (said++ < 64)
        LOGW("frame: %08x returned with EBP %08x, was %08x (left at eip=%08x)", target, after,
             before, c ? c->eip : 0u);
}

extern "C" void recomp_saved_changed(X86 *c, uint32_t target, const RecompSaved *before) {
    // Same rate limit as recomp_frame_changed; the register names say which
    // part of the ABI the callee broke.
    // Each distinct callee once: the pairs that legitimately hand a frame to
    // their partner (enter in one call, leave in the next) would otherwise
    // fill any fixed cap before the routine that matters has been named.
    static std::set<uint32_t> seen;
    if (seen.size() < 512 && seen.insert(target).second)
        LOGW("frame: %08x returned with callee-saved registers changed: EBX %08x->%08x EBP "
             "%08x->%08x ESI %08x->%08x EDI %08x->%08x (left at eip=%08x)",
             target, before->ebx, c->r[R_EBX], before->ebp, c->r[R_EBP], before->esi, c->r[R_ESI],
             before->edi, c->r[R_EDI], c->eip);
}

extern "C" void recomp_watch_hit(uint32_t addr, uint32_t n, uint64_t value) {
    // Report every write, not just the first: what matters is which of them
    // was the last one before the damage was read back, and a legitimate
    // writer and a stray one look identical one at a time.
    static uint32_t hits = 0;
    LOGW("watch: %08x %u = %llx by %s (hit %u)", addr, n, (unsigned long long)value, watch_writer(),
         ++hits);
}

// ---------------------------------------------------------------------------
// Logging helpers (declared in guest.h; kept here so every translation unit in
// the runtime gets them without a separate object file).
// ---------------------------------------------------------------------------
int g_log_level = -1;

int log_level_init() {
    const char *e = recomp_env("LOG");
    g_log_level = e ? atoi(e) : 1;
    return g_log_level;
}

void log_msg(int level, const char *fmt, ...) {
    if (log_level() < level)
        return;
    va_list ap;
    va_start(ap, fmt);
    fputs("[recomp] ", stderr);
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
}

// A guest that generates code writes a new routine at a new address every
// time, and a diagnostic keyed by that address is a distinct key on every
// call. The set is therefore bounded: past the cap nothing new is remembered
// or reported, so a run that would have filled memory with keys for addresses
// nobody will ever look at stays flat instead. The cap is far above what a
// run that is behaving reports.
static const size_t kLogOnceKeys = 4096;

bool log_once(const char *key, const char *fmt, ...) {
    static std::set<std::string> seen;
    static bool capped = false;
    if (seen.size() >= kLogOnceKeys) {
        if (!capped) {
            capped = true;
            if (log_level() >= 1)
                fprintf(stderr,
                        "[recomp] log_once: %zu distinct diagnostics reported; further "
                        "first-occurrence messages are suppressed\n",
                        seen.size());
        }
        return false;
    }
    if (!seen.insert(key).second)
        return false;
    if (log_level() < 1)
        return true;
    va_list ap;
    va_start(ap, fmt);
    fputs("[recomp] ", stderr);
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    return true;
}

// ---------------------------------------------------------------------------
// Arena
// ---------------------------------------------------------------------------
namespace {

struct Blk {
    uint32_t addr; // block start
    uint32_t size; // rounded, multiple of 16
    uint32_t req;  // bytes the guest asked for
    bool used;
    bool listed;      // in the free index
    Blk *prev, *next; // address order
    Blk *free_left, *free_right;
    uint64_t free_priority;
    uint32_t free_max_size;
};

Blk *g_block_head = nullptr, *g_block_tail = nullptr;
std::vector<Blk *> g_block_slots;
size_t g_block_live = 0, g_block_tombs = 0;
Blk *g_node_pool = nullptr;
const Blk *kHashTomb = (const Blk *)1;
Blk *g_free_root = nullptr;
uint64_t g_total_allocs = 0, g_total_frees = 0;

uint32_t free_max_size(const Blk *n) {
    return n ? n->free_max_size : 0;
}
void free_update(Blk *n) {
    n->free_max_size =
        std::max(n->size, std::max(free_max_size(n->free_left), free_max_size(n->free_right)));
}
uint64_t free_priority(uint32_t addr) {
    uint64_t x = addr + 0x9e3779b97f4a7c15ull;
    x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9ull;
    x = (x ^ (x >> 27)) * 0x94d049bb133111ebull;
    return x ^ (x >> 31);
}
bool free_higher(const Blk *a, const Blk *b) {
    return a->free_priority > b->free_priority ||
           (a->free_priority == b->free_priority && a->addr < b->addr);
}
Blk *free_rotate_right(Blk *root) {
    Blk *top = root->free_left;
    root->free_left = top->free_right;
    top->free_right = root;
    free_update(root);
    free_update(top);
    return top;
}
Blk *free_rotate_left(Blk *root) {
    Blk *top = root->free_right;
    root->free_right = top->free_left;
    top->free_left = root;
    free_update(root);
    free_update(top);
    return top;
}
Blk *free_insert(Blk *root, Blk *n) {
    if (!root)
        return n;
    if (n->addr < root->addr) {
        root->free_left = free_insert(root->free_left, n);
        if (free_higher(root->free_left, root))
            root = free_rotate_right(root);
    } else {
        root->free_right = free_insert(root->free_right, n);
        if (free_higher(root->free_right, root))
            root = free_rotate_left(root);
    }
    free_update(root);
    return root;
}
Blk *free_merge(Blk *left, Blk *right) {
    if (!left)
        return right;
    if (!right)
        return left;
    if (free_higher(left, right)) {
        left->free_right = free_merge(left->free_right, right);
        free_update(left);
        return left;
    }
    right->free_left = free_merge(left, right->free_left);
    free_update(right);
    return right;
}
Blk *free_erase(Blk *root, uint32_t addr) {
    if (!root)
        return nullptr;
    if (addr < root->addr) {
        root->free_left = free_erase(root->free_left, addr);
    } else if (addr > root->addr) {
        root->free_right = free_erase(root->free_right, addr);
    } else {
        return free_merge(root->free_left, root->free_right);
    }
    free_update(root);
    return root;
}
void free_index_add(Blk *n) {
    n->free_left = n->free_right = nullptr;
    n->free_priority = free_priority(n->addr);
    n->free_max_size = n->size;
    g_free_root = free_insert(g_free_root, n);
}
void free_index_remove(Blk *n) {
    g_free_root = free_erase(g_free_root, n->addr);
    n->free_left = n->free_right = nullptr;
    n->free_max_size = 0;
}
inline uint32_t align_up(uint32_t v, uint32_t a);
Blk *find_block(uint32_t addr);
Blk *first_fit(Blk *root, uint32_t need, uint32_t align) {
    if (!root || root->free_max_size < need)
        return nullptr;
    if (root->free_left && root->free_left->free_max_size >= need) {
        if (Blk *found = first_fit(root->free_left, need, align))
            return found;
    }
    uint32_t user = align_up(root->addr, align);
    if (user >= root->addr) {
        uint32_t pad = user - root->addr;
        if (pad <= root->size && need <= root->size - pad)
            return root;
    }
    return first_fit(root->free_right, need, align);
}
bool free_index_valid(const Blk *n, uint64_t lower, uint64_t upper, size_t &count,
                      uint32_t &max_size) {
    if (!n) {
        max_size = 0;
        return true;
    }
    if (n->addr <= lower || n->addr >= upper || n->used || !n->listed || find_block(n->addr) != n ||
        (n->free_left && free_higher(n->free_left, n)) ||
        (n->free_right && free_higher(n->free_right, n)))
        return false;
    uint32_t left_max, right_max;
    if (!free_index_valid(n->free_left, lower, n->addr, count, left_max) ||
        !free_index_valid(n->free_right, n->addr, upper, count, right_max))
        return false;
    ++count;
    max_size = std::max(n->size, std::max(left_max, right_max));
    return n->free_max_size == max_size;
}

uint32_t block_bucket(uint32_t addr, size_t cap) {
    return (uint32_t)((addr >> 4) * 2654435761u) & (uint32_t)(cap - 1);
}
Blk *find_block(uint32_t addr) {
    size_t cap = g_block_slots.size();
    if (!cap)
        return nullptr;
    for (uint32_t i = block_bucket(addr, cap);;) {
        Blk *n = g_block_slots[i];
        if (!n)
            return nullptr;
        if (n != kHashTomb && n->addr == addr)
            return n;
        i = (i + 1) & (uint32_t)(cap - 1);
    }
}
void block_rehash(size_t cap) {
    g_block_slots.assign(cap, nullptr);
    g_block_live = g_block_tombs = 0;
    for (Blk *n = g_block_head; n; n = n->next) {
        uint32_t i = block_bucket(n->addr, cap);
        while (g_block_slots[i])
            i = (i + 1) & (uint32_t)(cap - 1);
        g_block_slots[i] = n;
        ++g_block_live;
    }
}
void block_hash_put(Blk *n) {
    size_t cap = g_block_slots.size();
    uint32_t i = block_bucket(n->addr, cap);
    while (g_block_slots[i] && g_block_slots[i] != kHashTomb)
        i = (i + 1) & (uint32_t)(cap - 1);
    if (g_block_slots[i] == kHashTomb)
        --g_block_tombs;
    g_block_slots[i] = n;
    ++g_block_live;
}
void block_hash_del(Blk *n) {
    size_t cap = g_block_slots.size();
    uint32_t i = block_bucket(n->addr, cap);
    while (g_block_slots[i] != n)
        i = (i + 1) & (uint32_t)(cap - 1);
    g_block_slots[i] = (Blk *)kHashTomb;
    --g_block_live;
    ++g_block_tombs;
}
Blk *block_new(uint32_t addr, uint32_t size, uint32_t req, bool used) {
    Blk *n = g_node_pool;
    if (n)
        g_node_pool = n->next;
    else
        n = new Blk();
    n->addr = addr;
    n->size = size;
    n->req = req;
    n->used = used;
    n->listed = !used;
    n->prev = n->next = nullptr;
    n->free_left = n->free_right = nullptr;
    n->free_priority = 0;
    n->free_max_size = 0;
    return n;
}
Blk *block_add(Blk *at, uint32_t addr, uint32_t size, uint32_t req, bool used) {
    Blk *n = block_new(addr, size, req, used);
    size_t cap = g_block_slots.size();
    if (!cap || (g_block_live + g_block_tombs + 1) * 4 > cap * 3)
        block_rehash(cap ? cap * 2 : 256);
    n->prev = at;
    n->next = at ? at->next : g_block_head;
    if (n->prev)
        n->prev->next = n;
    else
        g_block_head = n;
    if (n->next)
        n->next->prev = n;
    else
        g_block_tail = n;
    block_hash_put(n);
    if (n->listed)
        free_index_add(n);
    return n;
}
void block_remove(Blk *n) {
    if (n->listed)
        free_index_remove(n);
    if (n->prev)
        n->prev->next = n->next;
    else
        g_block_head = n->next;
    if (n->next)
        n->next->prev = n->prev;
    else
        g_block_tail = n->prev;
    block_hash_del(n);
    n->next = g_node_pool;
    g_node_pool = n;
}

// Optional per-allocation tracing (RECOMP_HEAP_TRACE=1): remembers the guest
// site and requested size of every live block so the out-of-memory dump can
// say who is holding the heap. Off by default; the map is untouched then.
struct LiveAlloc {
    uint32_t site;
    uint32_t size;
};
std::map<uint32_t, LiveAlloc> *g_live = nullptr;
bool g_heap_trace = false;
uint32_t heap_site() {
    if (!g_heap_trace)
        return 0;
    const X86 *c = guest_current_context();
    return c ? c->eip : 0;
}
void live_set(uint32_t addr, uint32_t site, uint32_t size) {
    if (g_heap_trace)
        (*g_live)[addr] = LiveAlloc{site, size};
}
void live_erase(uint32_t addr) {
    if (g_heap_trace)
        g_live->erase(addr);
}

// On out-of-memory, print who is holding the heap: totals per guest call site,
// the largest non-surface allocations, and a size histogram. Without it the
// only fact is that the arena is full, not which subsystem filled it.
void heap_trace_report() {
    std::map<uint32_t, std::pair<uint32_t, uint64_t>> by_site; // site -> (count, bytes)
    uint64_t buckets[5] = {0, 0, 0, 0, 0};                     // <1K, <16K, <64K, <256K, >=256K
    for (auto &kv : *g_live) {
        auto &e = by_site[kv.second.site];
        ++e.first;
        e.second += kv.second.size;
        uint32_t s = kv.second.size;
        int b = s < 1024 ? 0 : s < 16384 ? 1 : s < 65536 ? 2 : s < 262144 ? 3 : 4;
        buckets[b] += s;
    }
    std::vector<std::pair<uint32_t, std::pair<uint32_t, uint64_t>>> sorted(by_site.begin(),
                                                                           by_site.end());
    std::sort(sorted.begin(), sorted.end(),
              [](const auto &a, const auto &b) { return a.second.second > b.second.second; });
    std::map<std::pair<uint32_t, uint32_t>, std::pair<uint32_t, uint64_t>> by_size;
    for (auto &kv : *g_live) {
        auto &e = by_size[{kv.second.site, kv.second.size}];
        ++e.first;
        e.second += kv.second.size;
    }
    std::vector<std::pair<std::pair<uint32_t, uint32_t>, std::pair<uint32_t, uint64_t>>> sizes(
        by_size.begin(), by_size.end());
    std::sort(sizes.begin(), sizes.end(),
              [](const auto &a, const auto &b) { return a.second.second > b.second.second; });
    for (size_t i = 0; i < sizes.size() && i < 8; ++i)
        LOGW("heap live size: site %08x size %u: %u blocks, %llu bytes", sizes[i].first.first,
             sizes[i].first.second, sizes[i].second.first,
             (unsigned long long)sizes[i].second.second);
    for (size_t i = 0; i < sorted.size() && i < 16; ++i)
        LOGW("heap live: site %08x: %u blocks, %llu bytes", sorted[i].first, sorted[i].second.first,
             (unsigned long long)sorted[i].second.second);
    LOGW("heap live buckets: <1K=%llu <16K=%llu <64K=%llu <256K=%llu >=256K=%llu",
         (unsigned long long)buckets[0], (unsigned long long)buckets[1],
         (unsigned long long)buckets[2], (unsigned long long)buckets[3],
         (unsigned long long)buckets[4]);
    // RECOMP_HEAP_PEEK=0xADDR,0xADDR lets a run print guest globals that
    // explain a budget without the runtime naming game addresses.
    if (const char *peek = recomp_env("HEAP_PEEK")) {
        const char *p = peek;
        while (*p) {
            char *end = nullptr;
            unsigned long a = strtoul(p, &end, 0);
            if (end == p)
                break;
            if (a < GUEST_SIZE - 4) {
                const uint8_t *m = g_mem + a;
                uint32_t v = (uint32_t)m[0] | ((uint32_t)m[1] << 8) | ((uint32_t)m[2] << 16) |
                             ((uint32_t)m[3] << 24);
                LOGW("heap peek: [%08lx] = %08x", a, (unsigned)v);
            }
            p = (*end == ',') ? end + 1 : end;
        }
    }
}

inline uint32_t align_up(uint32_t v, uint32_t a) {
    return (v + a - 1) & ~(a - 1);
}

// Largest request the arena could ever satisfy. Anything at or above this
// would overflow the rounding in align_up (0xffffffff rounds to 0), so it is
// rejected before any arithmetic on it.
const uint32_t HEAP_ARENA_BYTES = HEAP_LIMIT - HEAP_BASE;
inline bool size_is_sane(uint32_t size) {
    return size <= HEAP_ARENA_BYTES;
}

// Optional redzones (RECOMP_HEAP_CANARY=1, =2 aborts on the first hit). Every
// block is over-allocated by at least 16 bytes and the bytes between the
// requested size and the end of the block are filled with a pattern. A guest
// overrun (or a helper that writes past the size it was given) damages the
// pattern; it is checked at free/realloc and by a periodic sweep of every
// live block (RECOMP_HEAP_CANARY_PERIOD allocator calls, default 4096), and a
// hit names the block, its allocation site and the current guest context.
// The block layout changes while it is on, so it is a diagnostic only.
bool g_canary = false;
int g_canary_mode = 0;
uint32_t g_canary_period = 1024, g_canary_ticks = 0;
const uint32_t CANARY_PAD = 16;
const uint8_t CANARY_BYTE = 0xfd;
std::set<uint32_t> g_canary_reported;

// Quarantine state (RECOMP_HEAP_QUARANTINE, implied by the canary): freed
// blocks held back from reuse, oldest first. With the canary on they are also
// filled with a poison byte, so a write into freed memory is detectable.
const uint8_t POISON_BYTE = 0xdd;
std::deque<uint32_t> g_held;
uint64_t g_held_bytes = 0;
std::map<uint32_t, uint32_t> g_free_site; // held block -> guest eip when it was freed

inline uint32_t canary_pad() {
    return g_canary ? CANARY_PAD : 0;
}
void canary_fill(uint32_t addr, const Blk &b) {
    if (g_canary && b.size > b.req)
        memset(g_mem + addr + b.req, CANARY_BYTE, b.size - b.req);
}
// Reports the first damaged redzone byte of a used block; once per block.
bool canary_check(uint32_t addr, const Blk &b, const char *when) {
    if (!g_canary || !b.used || b.size <= b.req)
        return true;
    const uint8_t *z = g_mem + addr + b.req;
    uint32_t n = b.size - b.req, bad = 0;
    while (bad < n && z[bad] == CANARY_BYTE)
        ++bad;
    if (bad == n)
        return true;
    if (!g_canary_reported.insert(addr).second)
        return false;
    char dump[160];
    int len = 0;
    for (uint32_t i = bad; i < n && i < bad + 24; ++i)
        len += snprintf(dump + len, sizeof dump - len, "%02x ", z[i]);
    auto live = g_live->find(addr);
    LOGW("heap canary: block %08x req %u size %u damaged at +%u past the request (%s): %s", addr,
         b.req, b.size, bad, when, dump);
    if (live != g_live->end())
        LOGW("heap canary:   allocated near guest eip %08x", live->second.site);
    if (const X86 *c = guest_current_context()) {
        LOGW("heap canary:   now eip=%08x esp=%08x ebp=%08x", c->eip, c->r[R_ESP], c->r[R_EBP]);
        auto chain = win32_return_chain(c->r[R_EBP], 8);
        for (size_t i = 0; i < chain.size(); ++i)
            LOGW("heap canary:   frame %zu returns to %08x", i, chain[i]);
    }
    if (g_canary_mode >= 2)
        abort();
    return false;
}
// A held (freed, quarantined) block whose poison was disturbed was written
// after free: by the guest through a stale pointer, or by a helper.
void poison_check(uint32_t addr, const Blk &b, const char *when) {
    if (!g_canary || b.used)
        return;
    const uint8_t *z = g_mem + addr;
    uint32_t bad = 0;
    while (bad < b.size && z[bad] == POISON_BYTE)
        ++bad;
    if (bad == b.size || !g_canary_reported.insert(addr).second)
        return;
    char dump[160];
    int len = 0;
    for (uint32_t i = bad; i < b.size && i < bad + 24; ++i)
        len += snprintf(dump + len, sizeof dump - len, "%02x ", z[i]);
    auto fs = g_free_site.find(addr);
    LOGW("heap canary: freed block %08x size %u written after free at +%u (%s): %s", addr, b.size,
         bad, when, dump);
    LOGW("heap canary:   freed near guest eip %08x", fs != g_free_site.end() ? fs->second : 0);
    if (const X86 *c = guest_current_context()) {
        LOGW("heap canary:   now eip=%08x esp=%08x ebp=%08x", c->eip, c->r[R_ESP], c->r[R_EBP]);
        auto chain = win32_return_chain(c->r[R_EBP], 8);
        for (size_t i = 0; i < chain.size(); ++i)
            LOGW("heap canary:   frame %zu returns to %08x", i, chain[i]);
    }
    if (g_canary_mode >= 2)
        abort();
}
void canary_sweep(const char *when) {
    for (Blk *b = g_block_head; b; b = b->next)
        if (b->used)
            canary_check(b->addr, *b, when);
    for (uint32_t a : g_held) {
        Blk *b = find_block(a);
        if (b)
            poison_check(a, *b, when);
    }
}
void canary_tick() {
    if (g_canary && ++g_canary_ticks >= g_canary_period) {
        g_canary_ticks = 0;
        canary_sweep("sweep");
    }
}

void heap_reset() {
    if (!g_live)
        g_live = new std::map<uint32_t, LiveAlloc>();
    g_heap_trace = recomp_env("HEAP_TRACE") != nullptr;
    if (const char *e = recomp_env("HEAP_CANARY")) {
        g_canary_mode = atoi(e);
        g_canary = g_canary_mode > 0;
        g_heap_trace = g_heap_trace || g_canary; // the live map supplies allocation sites
        if (const char *per = recomp_env("HEAP_CANARY_PERIOD"))
            g_canary_period = (uint32_t)strtoul(per, nullptr, 0);
        if (!g_canary_period)
            g_canary_period = 1;
        g_canary_ticks = 0;
        g_canary_reported.clear();
    }
    g_held.clear();
    g_held_bytes = 0;
    g_free_site.clear();
    g_live->clear();
    g_free_root = nullptr;
    g_block_slots.clear();
    g_block_live = g_block_tombs = 0;
    while (g_block_head) {
        Blk *n = g_block_head;
        g_block_head = n->next;
        n->next = g_node_pool;
        g_node_pool = n;
    }
    g_block_tail = nullptr;
    block_add(nullptr, HEAP_BASE, HEAP_LIMIT - HEAP_BASE, 0, false);
    g_total_allocs = g_total_frees = 0;
}

uint8_t *reserve_arena() {
    return (uint8_t *)(RECOMP_ARENA_ADDRESS
                           ? os_vm_reserve_at((void *)(uintptr_t)RECOMP_ARENA_ADDRESS, GUEST_SIZE)
                           : os_vm_reserve(GUEST_SIZE));
}

} // namespace

void mem_init() {
    ++g_heap_generation;
    recomp_seh_reset(nullptr);
    if (g_mem) {
        os_vm_release(g_mem, GUEST_SIZE);
        g_mem = nullptr;
    }
    g_mem = reserve_arena();
    if (!g_mem) {
        fprintf(
            stderr,
            "[recomp] fatal: cannot reserve %u bytes of guest memory (requested host base %#llx)\n",
            GUEST_SIZE, (unsigned long long)RECOMP_ARENA_ADDRESS);
        abort();
    }
    heap_reset();
}

void recomp_arena_swap(uint8_t *other, size_t size) {
    if (!g_mem) {
        g_mem = reserve_arena();
        if (!g_mem) {
            fprintf(stderr,
                    "[recomp] fatal: cannot reserve guest memory (requested host base %#llx)\n",
                    (unsigned long long)RECOMP_ARENA_ADDRESS);
            abort();
        }
    }
    if (size > GUEST_SIZE) {
        fprintf(stderr, "[recomp] fatal: arena swap of %zu bytes exceeds the %u-byte arena\n", size,
                GUEST_SIZE);
        abort();
    }
    uint8_t chunk[4096];
    for (size_t at = 0; at < size; at += sizeof chunk) {
        const size_t n = std::min(sizeof chunk, size - at);
        memcpy(chunk, other + at, n);
        memcpy(other + at, g_mem + at, n);
        memcpy(g_mem + at, chunk, n);
    }
}

void mem_shutdown() {
    ++g_heap_generation;
    recomp_seh_reset(nullptr);
    if (g_mem) {
        os_vm_release(g_mem, GUEST_SIZE);
        g_mem = nullptr;
    }
    while (g_block_head) {
        Blk *n = g_block_head;
        g_block_head = n->next;
        delete n;
    }
    g_block_tail = nullptr;
    while (g_node_pool) {
        Blk *n = g_node_pool;
        g_node_pool = n->next;
        delete n;
    }
    g_block_slots.clear();
    g_block_live = g_block_tombs = 0;
    g_free_root = nullptr;
    if (g_live) {
        delete g_live;
        g_live = nullptr;
    }
}

uint32_t heap_alloc(uint32_t size, bool zero, uint32_t align) {
    if (!g_block_head)
        return 0;
    if (!size_is_sane(size)) {
        LOGW("heap_alloc: refusing a %u byte request, the arena is %u bytes", size,
             HEAP_ARENA_BYTES);
        // Oversized requests often originate in a corrupted guest length.
        // Use this thread's register file, including worker-thread callers.
        if (const X86 *c = guest_current_context()) {
            LOGW("heap_alloc: EIP=%08x EAX=%08x ECX=%08x EDX=%08x EBX=%08x "
                 "ESP=%08x EBP=%08x ESI=%08x EDI=%08x",
                 c->eip, c->r[R_EAX], c->r[R_ECX], c->r[R_EDX], c->r[R_EBX], c->r[R_ESP],
                 c->r[R_EBP], c->r[R_ESI], c->r[R_EDI]);
            auto chain = win32_return_chain(c->r[R_EBP], 12);
            for (size_t i = 0; i < chain.size(); ++i)
                LOGW("  frame %zu returns to %08x", i, chain[i]);
            // Frameless RTL helpers do not appear in the EBP chain. Include
            // stack words preceded by a CALL, as the exception diagnostic does.
            auto candidates = win32_stack_return_candidates(c->r[R_ESP], 0x400, loader_image_base(),
                                                            loader_image_limit(), 24);
            std::string line;
            for (uint32_t ret : candidates) {
                char word[16];
                snprintf(word, sizeof word, " %08x", ret);
                line += word;
            }
            if (!line.empty())
                LOGW("  return addresses on the stack, newest first:%s", line.c_str());
        }
        return 0;
    }
    if (align < 16)
        align = 16;
    uint32_t need = align_up((size ? size : 1) + canary_pad(), 16);

    if (Blk *b = first_fit(g_free_root, need, align)) {
        uint32_t start = b->addr, len = b->size;
        uint32_t user = align_up(start, align);
        uint32_t pad = user - start;
        uint32_t tail = len - pad - need;
        Blk *at = b->prev;
        block_remove(b);
        if (pad)
            at = block_add(at, start, pad, 0, false);
        Blk *used_block = block_add(at, user, need, size, true);
        if (tail)
            block_add(used_block, user + need, tail, 0, false);
        if (zero)
            memset(g_mem + user, 0, need);
        ++g_total_allocs;
        live_set(user, heap_site(), size);
        canary_fill(user, *used_block);
        canary_tick();
        return user;
    }
    LOGW("heap_alloc: out of guest heap (%u bytes requested)", size);
    {
        HeapStats hs = heap_stats();
        LOGW("heap: %llu used (%llu requested) in %llu blocks, %llu free in %llu blocks, "
             "largest free %llu; %llu allocs, %llu frees",
             (unsigned long long)hs.used_bytes, (unsigned long long)hs.req_bytes,
             (unsigned long long)hs.used_blocks, (unsigned long long)hs.free_bytes,
             (unsigned long long)hs.free_blocks, (unsigned long long)hs.largest_free,
             (unsigned long long)hs.total_allocs, (unsigned long long)hs.total_frees);
        if (g_heap_trace)
            heap_trace_report();
    }
    return 0;
}

bool heap_owns(uint32_t addr) {
    Blk *b = find_block(addr);
    return b && b->used;
}

uint32_t heap_size(uint32_t addr) {
    Blk *b = find_block(addr);
    if (!b || !b->used)
        return 0xffffffffu;
    return b->req;
}

// Returns a freed block to the free list and merges it with free neighbours.
static void release_free_block(Blk *b) {
    b->used = false;
    b->listed = false;
    // Merge only with blocks on the free list: a quarantined block is free but held.
    Blk *next = b->next;
    if (next && !next->used && next->listed && next->addr == b->addr + b->size) {
        b->size += next->size;
        block_remove(next);
    }
    Blk *prev = b->prev;
    if (prev && !prev->used && prev->listed && prev->addr + prev->size == b->addr) {
        free_index_remove(prev);
        prev->listed = false;
        prev->size += b->size;
        block_remove(b);
        prev->listed = true;
        free_index_add(prev);
    } else {
        b->listed = true;
        free_index_add(b);
    }
}

bool heap_free(uint32_t addr) {
    Blk *b = addr ? find_block(addr) : nullptr;
    if (!b || !b->used) {
        LOGW("heap_free: %08x is not a live allocation", addr);
        if (g_canary) {
            auto fs = g_free_site.find(addr);
            if (fs != g_free_site.end())
                LOGW("heap canary: double free; first freed near guest eip %08x", fs->second);
            if (const X86 *c = guest_current_context()) {
                LOGW("heap canary:   now eip=%08x esp=%08x ebp=%08x", c->eip, c->r[R_ESP],
                     c->r[R_EBP]);
                auto chain = win32_return_chain(c->r[R_EBP], 8);
                for (size_t i = 0; i < chain.size(); ++i)
                    LOGW("heap canary:   frame %zu returns to %08x", i, chain[i]);
            }
            if (g_canary_mode >= 2)
                abort();
        }
        return false;
    }
    canary_check(addr, *b, "free");
    g_canary_reported.erase(addr);
    b->used = false;
    b->req = 0;
    ++g_total_frees;
    live_erase(addr);
    canary_tick();

    // RECOMP_HEAP_QUARANTINE=1 holds a freed block back instead of returning
    // it: its address is not handed out again, and no neighbour absorbs it. A
    // guest that goes on using memory it has already freed then reads its own
    // dead object rather than whatever was allocated over the top of it, which
    // is the difference between a fault that names the culprit and one that
    // does not. The hold is a FIFO bounded by RECOMP_HEAP_QUARANTINE_MB (default
    // 48): past that the oldest block is released for reuse, so the heap lasts
    // and a stale use is caught for as long as the window covers. Off by
    // default; a diagnostic rather than a way to play.
    static const bool quarantine = recomp_env("HEAP_QUARANTINE") != nullptr || g_canary;
    if (quarantine) {
        static const uint64_t budget = [] {
            const char *e = recomp_env("HEAP_QUARANTINE_MB");
            return (uint64_t)(e ? strtoul(e, nullptr, 0) : 48ul) << 20;
        }();
        if (g_canary) {
            memset(g_mem + addr, POISON_BYTE, b->size);
            const X86 *c = guest_current_context();
            g_free_site[addr] = c ? c->eip : 0;
        }
        g_held.push_back(addr);
        g_held_bytes += b->size;
        while (g_held_bytes > budget && !g_held.empty()) {
            uint32_t old = g_held.front();
            g_held.pop_front();
            Blk *oit = find_block(old);
            if (!oit || oit->used)
                continue;
            poison_check(old, *oit, "release");
            g_free_site.erase(old);
            g_canary_reported.erase(old);
            g_held_bytes -= oit->size;
            release_free_block(oit);
        }
        return true;
    }
    release_free_block(b);
    return true;
}

// Resize a guest heap allocation in place when possible, otherwise copy into a new block.
// Reject arena-sized overflow requests and leave the original allocation live if growth fails.
uint32_t heap_realloc(uint32_t addr, uint32_t new_size, bool zero) {
    if (!addr)
        return heap_alloc(new_size, zero);
    if (!g_block_head)
        return 0;
    if (!size_is_sane(new_size)) {
        LOGW("heap_realloc: refusing a %u byte request, the arena is %u bytes", new_size,
             HEAP_ARENA_BYTES);
        return 0;
    }
    Blk *b = find_block(addr);
    if (!b || !b->used) {
        LOGW("heap_realloc: %08x is not a live allocation", addr);
        return 0;
    }
    canary_check(addr, *b, "realloc");
    g_canary_reported.erase(addr);
    uint32_t old_req = b->req;
    uint32_t need = align_up((new_size ? new_size : 1) + canary_pad(), 16);
    uint32_t have = b->size;

    if (need <= have) {
        uint32_t tail = have - need;
        if (tail >= 16) {
            b->size = need;
            Blk *next = b->next;
            if (next && !next->used && next->listed && next->addr == addr + have) {
                uint32_t merged = tail + next->size;
                block_remove(next);
                block_add(b, addr + need, merged, 0, false);
            } else {
                block_add(b, addr + need, tail, 0, false);
            }
        }
        b->req = new_size;
        live_set(addr, heap_site(), new_size);
        if (zero && new_size > old_req)
            memset(g_mem + addr + old_req, 0, new_size - old_req);
        canary_fill(addr, *b);
        return addr;
    }

    // Try to grow into a free neighbour.
    Blk *next = b->next;
    if (next && !next->used && next->listed && next->addr == addr + have &&
        have + next->size >= need) {
        uint32_t total = have + next->size;
        block_remove(next);
        uint32_t tail = total - need;
        b->size = need;
        b->req = new_size;
        live_set(addr, heap_site(), new_size);
        if (tail)
            block_add(b, addr + need, tail, 0, false);
        if (zero)
            memset(g_mem + addr + old_req, 0, new_size - old_req);
        canary_fill(addr, *b);
        return addr;
    }

    uint32_t fresh = heap_alloc(new_size, false);
    if (!fresh)
        return 0;
    uint32_t copy = old_req < new_size ? old_req : new_size;
    memmove(g_mem + fresh, g_mem + addr, copy);
    if (zero && new_size > copy)
        memset(g_mem + fresh + copy, 0, new_size - copy);
    heap_free(addr);
    return fresh;
}

HeapStats heap_stats() {
    HeapStats s{};
    s.total_allocs = g_total_allocs;
    s.total_frees = g_total_frees;
    if (!g_block_head)
        return s;
    for (Blk *b = g_block_head; b; b = b->next) {
        if (b->used) {
            ++s.used_blocks;
            s.used_bytes += b->size;
            s.req_bytes += b->req;
        } else {
            ++s.free_blocks;
            s.free_bytes += b->size;
            if (b->size > s.largest_free)
                s.largest_free = b->size;
        }
    }
    return s;
}

// Check that the heap block map covers the arena without gaps, overlap or adjacent free blocks.
// Return the first invariant failure, or an empty string when the allocation structure is consistent.
std::string heap_check() {
    if (!g_block_head)
        return "heap not initialised";
    uint32_t cursor = HEAP_BASE;
    bool prev_free = false;
    char buf[160];
    for (Blk *b = g_block_head; b; b = b->next) {
        if (b->addr != cursor) {
            snprintf(buf, sizeof buf, "gap or overlap at %08x (expected %08x)", b->addr, cursor);
            return buf;
        }
        if (b->size == 0 || (b->size & 15)) {
            snprintf(buf, sizeof buf, "block %08x has bad size %u", b->addr, b->size);
            return buf;
        }
        if (!b->used && prev_free) {
            snprintf(buf, sizeof buf, "adjacent free blocks at %08x", b->addr);
            return buf;
        }
        prev_free = !b->used;
        cursor += b->size;
    }
    if (cursor != HEAP_LIMIT) {
        snprintf(buf, sizeof buf, "heap ends at %08x, expected %08x", cursor, HEAP_LIMIT);
        return buf;
    }
    size_t free_count = 0;
    for (Blk *b = g_block_head; b; b = b->next)
        if (!b->used && b->listed)
            ++free_count;
    size_t indexed_count = 0;
    uint32_t indexed_max;
    if (!free_index_valid(g_free_root, 0, 1ull << 32, indexed_count, indexed_max))
        return "free index tree invariant failed";
    if (free_count != indexed_count) {
        snprintf(buf, sizeof buf, "free index has %zu entries, the heap has %zu listed free blocks",
                 indexed_count, free_count);
        return buf;
    }
    return std::string();
}
