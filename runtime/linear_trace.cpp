// linear_trace.cpp - see linear_trace.h for the switches and file contract.
#include "linear_trace.h"

#include "x86.h"
#include "guest.h"
#include "win32.h"
#include "../platform/os.h"

#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

namespace {

constexpr uint32_t kRecordSize = 68;
constexpr uint32_t kHeaderSize = 36;
constexpr uint32_t kThreads = 128;
constexpr uint32_t kDepth = 2048;
constexpr uint32_t kBufferRecords = 256;
constexpr uint64_t kDefaultMax = 2000000;

struct __attribute__((packed)) TraceRecord {
    uint8_t kind;
    uint8_t flags;
    uint16_t reserved;
    uint32_t tid;
    uint32_t depth;
    uint32_t addr;
    uint32_t ret_addr;
    uint32_t esp;
    uint32_t ecx;
    uint32_t edx;
    uint32_t args[4];
    uint32_t eax;
    int32_t esp_delta;
    uint32_t x87_top;
    uint64_t st0_bits;
};

struct __attribute__((packed)) TraceHeader {
    char magic[8];
    uint32_t version;
    uint32_t record_size;
    uint32_t flags;
    uint32_t start_addr;
    uint32_t start_n;
    uint32_t range_lo;
    uint32_t range_hi;
};

static_assert(sizeof(TraceRecord) == kRecordSize, "trace record layout");
static_assert(sizeof(TraceHeader) == kHeaderSize, "trace header layout");

struct Frame {
    uint32_t addr;
    uint32_t esp;
    uint32_t id;
    uint8_t recorded;
    uint8_t pad[3];
};

struct ThreadState {
    uint32_t tid;
    uint32_t depth;
    Frame frames[kDepth];
    TraceRecord buf[kBufferRecords];
    uint32_t used;
};

int g_fd = -1;
char g_path[1024];
RecompHookFn *g_prev;
ThreadState g_threads[kThreads];
uint32_t g_thread_count;
uint64_t g_events;
uint64_t g_max = kDefaultMax;
uint64_t g_start_seen;
uint32_t g_start_addr;
uint32_t g_start_n = 1;
uint32_t g_range_lo;
uint32_t g_range_hi;
uint32_t g_call_seq;
uint8_t g_range;
uint8_t g_active;
uint8_t g_stopped;
uint8_t g_installed;
uint8_t g_finishing;
uint8_t g_reported;
uint8_t g_lock;

thread_local uint32_t t_cached_tid;
thread_local ThreadState *t_cached_state;

void lock_writer() {
    while (__atomic_exchange_n(&g_lock, 1, __ATOMIC_ACQUIRE))
        while (__atomic_load_n(&g_lock, __ATOMIC_RELAXED))
            ;
}

void unlock_writer() {
    __atomic_store_n(&g_lock, 0, __ATOMIC_RELEASE);
}

void write_bytes(const void *data, size_t size) {
    const uint8_t *bytes = (const uint8_t *)data;
    while (size) {
        int64_t written = os_fd_write(g_fd, bytes, size);
        if (written < 0 && errno == EINTR)
            continue;
        if (written <= 0) {
            __atomic_store_n(&g_stopped, 1, __ATOMIC_RELEASE);
            constexpr char message[] = "[trace-linear] cannot write trace\n";
            os_write_stderr_raw(message, sizeof message - 1);
            return;
        }
        bytes += written;
        size -= (size_t)written;
    }
}

void flush_thread_locked(ThreadState *st) {
    if (g_fd < 0 || !st || !st->used)
        return;
    write_bytes(st->buf, kRecordSize * st->used);
    st->used = 0;
}

void finish() {
    uint8_t expected = 0;
    if (!__atomic_compare_exchange_n(&g_finishing, &expected, 1, false, __ATOMIC_ACQ_REL,
                                     __ATOMIC_ACQUIRE))
        return;
    lock_writer();
    if (g_fd >= 0) {
        uint32_t count = __atomic_load_n(&g_thread_count, __ATOMIC_ACQUIRE);
        if (count > kThreads)
            count = kThreads;
        for (uint32_t i = 0; i < count; ++i)
            flush_thread_locked(&g_threads[i]);
        os_fd_close(g_fd);
        g_fd = -1;
    }
    unlock_writer();
    if (!g_reported) {
        g_reported = 1;
        fprintf(stderr, "[trace-linear] %llu records to %s%s\n", (unsigned long long)g_events,
                g_path, __atomic_load_n(&g_stopped, __ATOMIC_ACQUIRE) ? " (max reached)" : "");
    }
}

void flush_thread(ThreadState *st) {
    lock_writer();
    flush_thread_locked(st);
    unlock_writer();
}

void enqueue(ThreadState *st, const TraceRecord &record) {
    if (__atomic_load_n(&g_stopped, __ATOMIC_ACQUIRE) || g_fd < 0)
        return;
    if (g_max && g_events >= g_max) {
        __atomic_store_n(&g_stopped, 1, __ATOMIC_RELEASE);
        finish();
        return;
    }
    st->buf[st->used++] = record;
    ++g_events;
    if (st->used == kBufferRecords)
        flush_thread(st);
    if (g_max && g_events >= g_max) {
        __atomic_store_n(&g_stopped, 1, __ATOMIC_RELEASE);
        finish();
    }
}

ThreadState *thread_state(uint32_t tid) {
    if (t_cached_state && t_cached_tid == tid)
        return t_cached_state;
    uint32_t count = __atomic_load_n(&g_thread_count, __ATOMIC_ACQUIRE);
    if (count > kThreads)
        count = kThreads;
    for (uint32_t i = 0; i < count; ++i) {
        if (g_threads[i].tid == tid) {
            t_cached_tid = tid;
            t_cached_state = &g_threads[i];
            return t_cached_state;
        }
    }
    uint32_t slot = __atomic_fetch_add(&g_thread_count, 1, __ATOMIC_ACQ_REL);
    if (slot >= kThreads)
        return nullptr;
    ThreadState *st = &g_threads[slot];
    st->tid = tid;
    st->depth = 0;
    st->used = 0;
    t_cached_tid = tid;
    t_cached_state = st;
    return st;
}

uint32_t stack_word(uint32_t esp, uint32_t off) {
    uint32_t addr = esp + off;
    if (!g_mem || !gm_valid(addr, 4))
        return 0;
    return rd32(addr);
}

uint64_t st0_bits(const X86 *c, uint8_t *valid) {
    unsigned phys = c->fpu_top & 7u;
    *valid = ftag_of(c, phys) != FTAG_EMPTY;
    if (!*valid)
        return 0;
    uint64_t bits;
    memcpy(&bits, &c->st[phys], sizeof bits);
    return bits;
}

void call_previous(X86 *c, uint32_t index) {
    RecompHookFn previous = g_prev ? g_prev[index] : nullptr;
    if (previous)
        previous(c, index);
    else if (index < recomp_func_count)
        recomp_base_ptrs[index](c);
}

void linear_dispatch(X86 *c, uint32_t index) {
    if (!g_prev || index >= recomp_func_count || __atomic_load_n(&g_stopped, __ATOMIC_ACQUIRE)) {
        call_previous(c, index);
        return;
    }

    const uint32_t addr = recomp_func_addrs[index];
    const uint32_t tid = guest_current_thread_id();
    ThreadState *st = thread_state(tid);
    if (!st) {
        call_previous(c, index);
        return;
    }

    const uint32_t esp = c->r[R_ESP];
    while (st->depth && st->depth <= kDepth && st->frames[st->depth - 1].esp <= esp)
        --st->depth;

    const uint32_t id = __atomic_add_fetch(&g_call_seq, 1, __ATOMIC_RELAXED);
    const bool pushed = st->depth < kDepth;
    const uint32_t depth = pushed ? st->depth + 1 : kDepth;

    if (!__atomic_load_n(&g_active, __ATOMIC_ACQUIRE) && addr == g_start_addr) {
        uint64_t seen = __atomic_add_fetch(&g_start_seen, 1, __ATOMIC_RELAXED);
        if (seen >= g_start_n)
            __atomic_store_n(&g_active, 1, __ATOMIC_RELEASE);
    }

    const bool active = __atomic_load_n(&g_active, __ATOMIC_ACQUIRE);
    const bool in_range = !g_range || (addr >= g_range_lo && addr <= g_range_hi);
    const bool record = active && in_range && !__atomic_load_n(&g_stopped, __ATOMIC_ACQUIRE);

    if (record) {
        TraceRecord r{};
        r.kind = 1;
        r.tid = tid;
        r.depth = depth;
        r.addr = addr;
        r.ret_addr = stack_word(esp, 0);
        r.esp = esp;
        r.ecx = c->r[R_ECX];
        r.edx = c->r[R_EDX];
        for (uint32_t i = 0; i < 4; ++i)
            r.args[i] = stack_word(esp, 4 + 4 * i);
        enqueue(st, r);
    }

    if (pushed) {
        Frame &f = st->frames[st->depth];
        f.addr = addr;
        f.esp = esp;
        f.id = id;
        f.recorded = record;
        ++st->depth;
    }

    call_previous(c, index);

    if (!pushed)
        return;

    uint32_t found = st->depth;
    while (found) {
        --found;
        if (found < kDepth && st->frames[found].id == id)
            break;
    }
    if (found >= st->depth || found >= kDepth || st->frames[found].id != id)
        return;

    const Frame frame = st->frames[found];
    const uint32_t return_depth = found + 1;
    st->depth = found;

    if (!frame.recorded || __atomic_load_n(&g_stopped, __ATOMIC_ACQUIRE))
        return;

    TraceRecord r{};
    r.kind = 2;
    r.tid = tid;
    r.depth = return_depth;
    r.addr = frame.addr;
    r.esp = c->r[R_ESP];
    r.ret_addr = stack_word(frame.esp, 0);
    r.eax = c->r[R_EAX];
    r.edx = c->r[R_EDX];
    r.esp_delta = (int32_t)(c->r[R_ESP] - frame.esp);
    r.x87_top = c->fpu_top & 7u;
    uint8_t valid = 0;
    r.st0_bits = st0_bits(c, &valid);
    r.flags = valid;
    enqueue(st, r);
}

bool parse_u32(const char *text, char **end, uint32_t *out) {
    char *stop = nullptr;
    unsigned long value = strtoul(text, &stop, 16);
    if (stop == text || value > 0xfffffffful)
        return false;
    *out = (uint32_t)value;
    *end = stop;
    return true;
}

void parse_options() {
    if (const char *from = recomp_env("TRACE_LINEAR_FROM")) {
        char *end = nullptr;
        if (parse_u32(from, &end, &g_start_addr)) {
            g_start_n = 1;
            if (*end == ':')
                g_start_n = (uint32_t)strtoull(end + 1, nullptr, 0);
            if (!g_start_n)
                g_start_n = 1;
        } else {
            fprintf(stderr, "[trace-linear] bad FROM '%s'; recording from the start\n", from);
            g_active = 1;
        }
    } else {
        g_active = 1;
    }

    if (const char *max = recomp_env("TRACE_LINEAR_MAX")) {
        char *end = nullptr;
        unsigned long long value = strtoull(max, &end, 0);
        if (end != max)
            g_max = value;
    }

    if (const char *range = recomp_env("TRACE_LINEAR_RANGE")) {
        char *end = nullptr;
        if (parse_u32(range, &end, &g_range_lo)) {
            g_range_hi = 0xffffffffu;
            if (*end == ':') {
                char *stop = nullptr;
                unsigned long value = strtoul(end + 1, &stop, 16);
                if (stop != end + 1 && value <= 0xfffffffful)
                    g_range_hi = (uint32_t)value;
            }
            if (g_range_lo > g_range_hi) {
                uint32_t swap = g_range_lo;
                g_range_lo = g_range_hi;
                g_range_hi = swap;
            }
            g_range = 1;
        } else {
            fprintf(stderr, "[trace-linear] bad RANGE '%s'; no range filter\n", range);
        }
    }
}

bool install_hooks() {
    if (!recomp_func_count)
        return false;
    g_prev = (RecompHookFn *)calloc(recomp_func_count, sizeof(RecompHookFn));
    if (!g_prev)
        return false;

    __atomic_store_n(&recomp_hooks_ever, (uint8_t)1, __ATOMIC_RELEASE);
    __atomic_store_n(&recomp_slow_routes, (uint8_t)1, __ATOMIC_RELEASE);
    for (uint32_t i = 0; i < recomp_func_count; ++i) {
        g_prev[i] = __atomic_load_n(&recomp_hook_ptrs[i], __ATOMIC_ACQUIRE);
        __atomic_store_n(&recomp_hook_ptrs[i], (RecompHookFn)linear_dispatch, __ATOMIC_RELEASE);
        __atomic_store_n(&recomp_hooked[i], (uint8_t)1, __ATOMIC_RELEASE);
        recomp_slow_update(i);
    }
    return true;
}

void write_header() {
    TraceHeader h{};
    memcpy(h.magic, "RCTRACE1", 8);
    h.version = 1;
    h.record_size = kRecordSize;
    h.start_addr = g_start_addr;
    h.start_n = g_start_n;
    h.range_lo = g_range_lo;
    h.range_hi = g_range_hi;
    write_bytes(&h, sizeof h);
}

} // namespace

extern "C" void recomp_linear_trace_on_fault(void) {
    uint8_t expected = 0;
    if (!__atomic_compare_exchange_n(&g_lock, &expected, 1, false, __ATOMIC_ACQUIRE,
                                     __ATOMIC_RELAXED))
        return;
    if (g_fd >= 0) {
        uint32_t count = __atomic_load_n(&g_thread_count, __ATOMIC_ACQUIRE);
        if (count > kThreads)
            count = kThreads;
        for (uint32_t i = 0; i < count; ++i)
            flush_thread_locked(&g_threads[i]);
    }
    unlock_writer();
}

extern "C" void recomp_linear_trace_init(void) {
    if (g_installed)
        return;
    const char *path = recomp_env("TRACE_LINEAR");
    if (!path || !*path)
        return;
    if (!RECOMP_CALL_HOOKS) {
        fprintf(stderr, "[recomp] RECOMP_TRACE_LINEAR ignored: this build has no call hooks "
                        "([game] call_hooks = false)\n");
        return;
    }

    g_installed = 1;
    snprintf(g_path, sizeof g_path, "%s", path);
    parse_options();

    g_fd = os_fd_open(path, OS_O_WRONLY | OS_O_CREAT | OS_O_TRUNC);
    if (g_fd < 0) {
        fprintf(stderr, "[trace-linear] cannot open '%s'\n", path);
        return;
    }
    write_header();

    if (!install_hooks()) {
        fprintf(stderr, "[trace-linear] no translated main-image entries to hook\n");
        os_fd_close(g_fd);
        g_fd = -1;
        return;
    }

    atexit(finish);
    fprintf(stderr,
            "[trace-linear] %s: %u entries, start=%s0x%08x#%u, max=%llu, range=%s0x%08x:0x%08x\n",
            g_path, recomp_func_count,
            __atomic_load_n(&g_active, __ATOMIC_ACQUIRE) ? "now " : "at ", g_start_addr, g_start_n,
            (unsigned long long)g_max, g_range ? "" : "none ", g_range_lo, g_range_hi);
}
