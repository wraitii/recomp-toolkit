// The Visual C++ 9 (VS2008) C runtime, as the Spore executable imports it.
// The startup path is the reason this table exists: mainCRTStartup runs
// _initterm_e/_initterm over guest initializer tables, reads _acmdln and calls
// WinMain, and an unimplemented slot there would stop the run before any game
// code executes. The ordinary memory, string and character functions are
// covered alongside it; everything else stays a named failure (strict table)
// until its evidence is added here.
#include "imports.h"
#include "guest.h"
#include "game_config.h"
#include "intrinsics.h"
#include "memory.h"
#include "kernel32_internal.h"
#include "win32.h"
#include "../platform/os.h"

#include <algorithm>
#include <chrono>
#include <cctype>
#include <cerrno>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <map>
#include <string>
#include <vector>

namespace {

uint32_t g_app_type;
uint32_t g_user_math_err;
uint32_t g_thread_locale;
uint32_t g_mb_cp;
uint32_t g_rand_state = 1;
uint32_t g_fmode_addr;
uint32_t g_commode_addr;
uint32_t g_errno_addr;
uint32_t g_initenv_addr;
std::vector<uint32_t> g_onexit;
uint32_t g_signal_handlers[32];
std::map<std::string, uint32_t> g_env_cache;
thread_local uint32_t g_tm_slot;
thread_local uint32_t g_cvt_slot;

uint32_t guest_slot(uint32_t &slot, uint32_t size = 4) {
    if (!slot)
        slot = heap_alloc(size, true);
    return slot;
}

uint32_t dup_guest(const std::string &s) {
    uint32_t n = (uint32_t)s.size() + 1;
    uint32_t a = heap_alloc(n, true);
    if (a)
        memcpy(g_mem + a, s.c_str(), n);
    return a;
}

// Runs the registered guest exit handlers. A handler that registers another
// handler must have it run too, so the list is drained rather than copied.
void run_onexit(X86 *c) {
    while (!g_onexit.empty()) {
        uint32_t fn = g_onexit.back();
        g_onexit.pop_back();
        if (fn)
            guest_call(c, fn);
    }
}

// The CRT's public control word is not the x87 control word: PC lives in bits
// 16-17, RC in 8-9 and the exception masks are permuted (denormal at bit 19).
uint32_t cw_to_public(uint32_t x) {
    uint32_t p = 0;
    if (x & 0x0001)
        p |= 0x00000010; // invalid
    if (x & 0x0002)
        p |= 0x00080000; // denormal
    if (x & 0x0004)
        p |= 0x00000008; // zerodivide
    if (x & 0x0008)
        p |= 0x00000004; // overflow
    if (x & 0x0010)
        p |= 0x00000002; // underflow
    if (x & 0x0020)
        p |= 0x00000001;       // inexact
    p |= ((x >> 10) & 3) << 8; // RC
    switch ((x >> 8) & 3) {
    case 0:
        p |= 0x00020000;
        break; // 24-bit
    case 2:
        p |= 0x00010000;
        break; // 53-bit
    default:
        break; // 64-bit
    }
    return p;
}

uint32_t cw_from_public(uint32_t p) {
    uint32_t x = 0;
    if (p & 0x00000010)
        x |= 0x0001;
    if (p & 0x00080000)
        x |= 0x0002;
    if (p & 0x00000008)
        x |= 0x0004;
    if (p & 0x00000004)
        x |= 0x0008;
    if (p & 0x00000002)
        x |= 0x0010;
    if (p & 0x00000001)
        x |= 0x0020;
    x |= ((p >> 8) & 3) << 10;
    switch (p & 0x00030000) {
    case 0x00000000:
        x |= 0x0300;
        break; // 64-bit
    case 0x00010000:
        x |= 0x0200;
        break; // 53-bit
    default:
        break; // 24-bit
    }
    return x;
}

void crt_initterm_e(X86 *c) {
    uint32_t p = arg(c, 0), end = arg(c, 1);
    while (p < end) {
        if (!gm_valid(p, 4))
            break;
        uint32_t fn = rd32(p);
        p += 4;
        if (!fn)
            continue;
        if (guest_call(c, fn)) {
            set_eax(c, c->r[R_EAX]);
            return;
        }
    }
    set_eax(c, 0);
}

void crt_initterm(X86 *c) {
    uint32_t p = arg(c, 0), end = arg(c, 1);
    while (p < end) {
        if (!gm_valid(p, 4))
            break;
        uint32_t fn = rd32(p);
        p += 4;
        if (fn)
            guest_call(c, fn);
    }
    set_eax(c, 0);
}

void crt_ismbblead(X86 *c) {
    uint32_t ch = arg(c, 0) & 0xff;
    bool lead = false;
    switch (g_mb_cp) {
    case 932:
        lead = (ch >= 0x81 && ch <= 0x9f) || (ch >= 0xe0 && ch <= 0xfc);
        break;
    case 936:
    case 949:
    case 950:
        lead = ch >= 0x81 && ch <= 0xfe;
        break;
    default:
        break;
    }
    set_eax(c, lead ? 1u : 0u);
}

void crt_setmbcp(X86 *c) {
    g_mb_cp = arg(c, 0);
    set_eax(c, 0);
}

void crt_set_app_type(X86 *c) {
    g_app_type = arg(c, 0);
    set_eax(c, 0);
}

void crt_setusermatherr(X86 *c) {
    g_user_math_err = arg(c, 0);
    set_eax(c, 0);
}

void crt_adjust_fdiv(X86 *c) {
    set_eax(c, 0);
}

void crt_configthreadlocale(X86 *c) {
    uint32_t previous = g_thread_locale;
    g_thread_locale = arg(c, 0);
    set_eax(c, previous);
}

void crt_encode_pointer(X86 *c) {
    set_eax(c, arg(c, 0));
}

void crt_decode_pointer(X86 *c) {
    set_eax(c, arg(c, 0));
}

void crt_p_fmode(X86 *c) {
    set_eax(c, guest_slot(g_fmode_addr));
}

void crt_p_commode(X86 *c) {
    set_eax(c, guest_slot(g_commode_addr));
}

void crt_p_initenv(X86 *c) {
    set_eax(c, guest_slot(g_initenv_addr));
}

void crt_errno(X86 *c) {
    set_eax(c, guest_slot(g_errno_addr));
}

void crt_controlfp_s(X86 *c) {
    uint32_t out = arg(c, 0), value = arg(c, 1), mask = arg(c, 2);
    uint32_t pub = (cw_to_public(c->fpu_cw) & ~mask) | (value & mask);
    c->fpu_cw = (uint16_t)cw_from_public(pub);
    if (out && gm_valid(out, 4))
        wr32(out, cw_to_public(c->fpu_cw));
    set_eax(c, 0);
}

void crt_controlfp(X86 *c) {
    uint32_t value = arg(c, 0), mask = arg(c, 1);
    uint32_t previous = cw_to_public(c->fpu_cw);
    uint32_t pub = (previous & ~mask) | (value & mask);
    c->fpu_cw = (uint16_t)cw_from_public(pub);
    set_eax(c, previous);
}

void run_guest_exit(X86 *c, uint32_t code, bool run_handlers) {
    if (run_handlers)
        run_onexit(c);
    fflush(stdout);
    fflush(stderr);
    kernel32_exit_process(code);
}

void crt_exit(X86 *c) {
    run_guest_exit(c, arg(c, 0), true);
}

void crt_exit_fast(X86 *c) {
    run_guest_exit(c, arg(c, 0), false);
}

void crt_cexit(X86 *c) {
    run_onexit(c);
    fflush(stdout);
    fflush(stderr);
    set_eax(c, 0);
}

void crt_amsg_exit(X86 *c) {
    LOGW("runtime error %u (MSVCR90 _amsg_exit)", arg(c, 0));
    fflush(stdout);
    fflush(stderr);
    std::exit(255);
}

void crt_invoke_watson(X86 *c) {
    LOGW("MSVCR90 _invoke_watson (invalid parameter, expression/form/file/line %u)", arg(c, 3));
    fflush(stdout);
    fflush(stderr);
    abort();
}

void crt_debugger_hook(X86 *c) {
    set_eax(c, 0);
}

void crt_terminate(X86 *c) {
    LOGW("MSVCR90 terminate called");
    fflush(stdout);
    fflush(stderr);
    abort();
}

void crt_abort(X86 *c) {
    fflush(stdout);
    fflush(stderr);
    abort();
}

void crt_longjmp(X86 *c) {
    recomp_longjmp(c);
}

void crt_onexit(X86 *c) {
    uint32_t fn = arg(c, 0);
    if (fn && g_onexit.size() < 1024)
        g_onexit.push_back(fn);
    set_eax(c, fn);
}

void crt_dllonexit(X86 *c) {
    uint32_t fn = arg(c, 0);
    if (fn && g_onexit.size() < 1024)
        g_onexit.push_back(fn);
    set_eax(c, fn);
}

void crt_lock(X86 *c) {
    set_eax(c, 0);
}

void crt_unlock(X86 *c) {
    set_eax(c, 0);
}

// _beginthreadex(security, stack_size, start, arglist, initflag, thrdaddr) is
// cdecl, so the caller cleans its six arguments. The runtime's cooperative
// thread already supplies the TEB/TLS the worker needs; the CRT's own
// per-thread data block is not modelled.
void crt_beginthreadex(X86 *c) {
    uint32_t start = arg(c, 2), param = arg(c, 3), flags = arg(c, 4), ptid = arg(c, 5);
    uint32_t h = kernel32_create_guest_thread(c, start, param, flags, ptid);
    if (!h) {
        wr32(guest_slot(g_errno_addr), EAGAIN);
        LOGW("_beginthreadex(%08x): thread creation failed", start);
    }
    set_eax(c, h);
}

void crt_xcpt_filter(X86 *c) {
    // EXCEPTION_CONTINUE_SEARCH: the guest's own SEH records decide. The CRT's
    // /GS and math-error handling that would answer differently is not modelled.
    set_eax(c, 0);
}

void crt_signal(X86 *c) {
    uint32_t sig = arg(c, 0), handler = arg(c, 1);
    if (sig < 32) {
        uint32_t previous = g_signal_handlers[sig];
        g_signal_handlers[sig] = handler;
        set_eax(c, previous);
    } else {
        set_eax(c, 0xffffffffu);
    }
}

void crt_rand(X86 *c) {
    g_rand_state = g_rand_state * 214013u + 2531011u;
    set_eax(c, (g_rand_state >> 16) & 0x7fffu);
}

// ---------------------------------------------------------------------------
// __getmainargs. The WinMainCRTStartup path reads _acmdln directly, but the
// console startup variant calls this and a later entry may too.
// ---------------------------------------------------------------------------
void split_command_line(const std::string &line, std::vector<std::string> &out) {
    size_t i = 0;
    while (i < line.size()) {
        while (i < line.size() && (line[i] == ' ' || line[i] == '\t'))
            ++i;
        if (i >= line.size())
            break;
        std::string token;
        bool quoted = false;
        while (i < line.size()) {
            char ch = line[i];
            if (ch == '"') {
                quoted = !quoted;
                ++i;
                continue;
            }
            if (!quoted && (ch == ' ' || ch == '\t'))
                break;
            token += ch;
            ++i;
        }
        out.push_back(token);
    }
}

void crt_getmainargs(X86 *c) {
    uint32_t argc_out = arg(c, 0), argv_out = arg(c, 1), envp_out = arg(c, 2);
    std::vector<std::string> tokens;
    split_command_line(gm_str(win32_command_line()), tokens);
    if (tokens.empty())
        tokens.emplace_back(RECOMP_EXECUTABLE);

    uint32_t argv = heap_alloc((uint32_t)(tokens.size() + 1) * 4, true);
    if (!argv) {
        set_eax(c, -1);
        return;
    }
    for (size_t i = 0; i < tokens.size(); ++i)
        wr32(argv + (uint32_t)i * 4, dup_guest(tokens[i]));
    wr32(argv + (uint32_t)tokens.size() * 4, 0);

    uint32_t envp = heap_alloc(4, true);
    if (argc_out && gm_valid(argc_out, 4))
        wr32(argc_out, (uint32_t)tokens.size());
    if (argv_out && gm_valid(argv_out, 4))
        wr32(argv_out, argv);
    if (envp_out && gm_valid(envp_out, 4))
        wr32(envp_out, envp);
    set_eax(c, 0);
}

// ---------------------------------------------------------------------------
// Heap. Guest code dereferences these pointers, so they must come from the
// guest arena's heap, not the host's.
// ---------------------------------------------------------------------------
void crt_malloc(X86 *c) {
    set_eax(c, heap_alloc(arg(c, 0), false));
}

void crt_free(X86 *c) {
    uint32_t p = arg(c, 0);
    if (p)
        heap_free(p);
    set_eax(c, 0);
}

void crt_realloc(X86 *c) {
    uint32_t p = arg(c, 0), size = arg(c, 1);
    if (!p) {
        set_eax(c, heap_alloc(size, false));
        return;
    }
    set_eax(c, heap_realloc(p, size, false));
}

int current_drive_number() {
    std::string path = current_directory();
    if (path.size() >= 2 && path[1] == ':') {
        char ch = path[0];
        if (ch >= 'a' && ch <= 'z')
            ch = (char)(ch - 'a' + 'A');
        if (ch >= 'A' && ch <= 'Z')
            return ch - 'A' + 1;
    }
    return 3; // the guest root is on C:
}

void crt_getcwd(X86 *c, uint32_t drive, uint32_t out, int32_t maxlen) {
    if (maxlen <= 0 || drive > 26) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0);
        return;
    }
    if (drive != 0 && (int)drive != current_drive_number()) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0);
        return;
    }
    std::string path = current_directory();
    uint32_t need = 1;
    for (unsigned char ch : path)
        if ((ch & 0xc0) != 0x80)
            need += ch >= 0xf0 ? 2 : 1;
    if (need > (uint32_t)maxlen) {
        wr32(guest_slot(g_errno_addr), ERANGE);
        set_eax(c, 0);
        return;
    }
    if (!out) {
        uint64_t bytes = (uint64_t)(uint32_t)maxlen * 2;
        out = bytes > 0xffffffffu ? 0 : heap_alloc((uint32_t)bytes, true);
        if (!out) {
            wr32(guest_slot(g_errno_addr), ENOMEM);
            set_eax(c, 0);
            return;
        }
    }
    gm_put_wstr(out, path, (uint32_t)maxlen);
    set_eax(c, out);
}

void crt_wgetcwd(X86 *c) {
    crt_getcwd(c, 0, arg(c, 0), (int32_t)arg(c, 1));
}

void crt_wgetdcwd(X86 *c) {
    crt_getcwd(c, arg(c, 0), arg(c, 1), (int32_t)arg(c, 2));
}

void crt_getdrive(X86 *c) {
    set_eax(c, (uint32_t)current_drive_number());
}

void crt_getenv(X86 *c) {
    std::string name = gm_str(arg(c, 0));
    auto it = g_env_cache.find(name);
    if (it != g_env_cache.end()) {
        set_eax(c, it->second);
        return;
    }
    std::string value;
    if (!kernel32_guest_environment_lookup(name, value)) {
        set_eax(c, 0);
        return;
    }
    uint32_t text = dup_guest(value);
    g_env_cache[name] = text;
    set_eax(c, text);
}

void crt_aligned_malloc(X86 *c) {
    set_eax(c, heap_alloc(arg(c, 0), false, arg(c, 1)));
}

void crt_aligned_free(X86 *c) {
    uint32_t p = arg(c, 0);
    if (p)
        heap_free(p);
    set_eax(c, 0);
}

void crt_aligned_realloc(X86 *c) {
    uint32_t p = arg(c, 0), size = arg(c, 1), align = arg(c, 2);
    if (!p) {
        set_eax(c, heap_alloc(size, false, align));
        return;
    }
    uint32_t fresh = heap_alloc(size, false, align);
    if (!fresh) {
        set_eax(c, 0);
        return;
    }
    uint32_t old = heap_size(p);
    if (old != 0xffffffffu)
        memcpy(g_mem + fresh, g_mem + p, std::min(old, size));
    heap_free(p);
    set_eax(c, fresh);
}

void crt_memcpy(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1), size = arg(c, 2);
    if (size && gm_valid(dst, size) && gm_valid(src, size))
        memcpy(g_mem + dst, g_mem + src, size);
    set_eax(c, dst);
}

void crt_memmove(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1), size = arg(c, 2);
    if (size && gm_valid(dst, size) && gm_valid(src, size))
        memmove(g_mem + dst, g_mem + src, size);
    set_eax(c, dst);
}

void crt_memset(X86 *c) {
    uint32_t dst = arg(c, 0), size = arg(c, 2);
    if (size && gm_valid(dst, size))
        memset(g_mem + dst, (int)(arg(c, 1) & 0xff), size);
    set_eax(c, dst);
}

void crt_memchr(X86 *c) {
    uint32_t p = arg(c, 0), size = arg(c, 2);
    uint8_t ch = (uint8_t)arg(c, 1);
    if (gm_valid(p, size)) {
        for (uint32_t i = 0; i < size; ++i)
            if (g_mem[p + i] == ch) {
                set_eax(c, p + i);
                return;
            }
    }
    set_eax(c, 0);
}

void qsort_swap(uint32_t a, uint32_t b, uint32_t width) {
    if (a == b || width == 0)
        return;
    for (uint32_t i = 0; i < width; ++i) {
        uint8_t left = rd8(a + i), right = rd8(b + i);
        wr8(a + i, right);
        wr8(b + i, left);
    }
}

int32_t qsort_compare(X86 *c, uint32_t compare, uint32_t a, uint32_t b) {
    return (int32_t)guest_call(c, compare, a, b);
}

// The selection pass MSVCR90's qsort uses for runs of at most eight elements.
void qsort_select(X86 *c, uint32_t low, uint32_t high, uint32_t width, uint32_t compare) {
    if (high <= low)
        return;
    const uint32_t first = low + width;
    do {
        uint32_t best = low;
        for (uint32_t at = first; at <= high; at += width)
            if (qsort_compare(c, compare, at, best) > 0)
                best = at;
        qsort_swap(best, high, width);
        high -= width;
    } while (high > low);
}

// MSVCR90 qsort, ported from the shipped msvcr90.dll (base 0x78590000,
// qsort 0x78594640). Its explicit stack and selection cutoff decide the order
// of elements a comparator reports equal, which matters for the game's float
// comparator that never returns zero.
void crt_qsort(X86 *c) {
    const uint32_t base = arg(c, 0), count = arg(c, 1);
    const uint32_t width = arg(c, 2), compare = arg(c, 3);
    if ((!base && count) || !width) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0);
        return;
    }
    if (count < 2) {
        set_eax(c, 0);
        return;
    }

    uint32_t stack_low[30], stack_high[30];
    int stack_count = 0;
    uint32_t low_bound = base, high_bound = base + (count - 1) * width;
    uint32_t low = low_bound, high = high_bound;
    for (;;) {
        const uint32_t length = (high - low) / width + 1;
        uint32_t pivot;
        if (length <= 8) {
            qsort_select(c, low, high, width, compare);
            goto pop;
        }
        pivot = low + (length / 2) * width;
        if (qsort_compare(c, compare, low, pivot) > 0)
            qsort_swap(low, pivot, width);
        if (qsort_compare(c, compare, low, high) > 0)
            qsort_swap(low, high, width);
        if (qsort_compare(c, compare, pivot, high) > 0)
            qsort_swap(pivot, high, width);

        for (;;) {
            if (pivot > low) {
                bool reached_pivot = false;
                do {
                    low += width;
                    if (low >= pivot) {
                        reached_pivot = true;
                        break;
                    }
                } while (qsort_compare(c, compare, low, pivot) <= 0);
                if (!reached_pivot)
                    goto scan_low;
            }
            do {
                low += width;
                if (low > high_bound)
                    break;
            } while (qsort_compare(c, compare, low, pivot) <= 0);
        scan_low:
            high -= width;
            if (high > pivot)
                while (qsort_compare(c, compare, high, pivot) > 0) {
                    high -= width;
                    if (high <= pivot)
                        break;
                }
            if (low > high)
                break;
            qsort_swap(low, high, width);
            if (pivot == high)
                pivot = low;
        }

        high += width;
        if (pivot < high) {
            for (;;) {
                high -= width;
                if (high <= pivot)
                    goto trim_low;
                const int32_t r = qsort_compare(c, compare, high, pivot);
                if (r != 0) {
                    if (pivot < high)
                        goto split;
                    goto trim_low;
                }
            }
        }
    trim_low:
        for (;;) {
            high -= width;
            if (high <= low_bound)
                goto split;
            if (qsort_compare(c, compare, high, pivot) != 0)
                goto split;
        }

    split:
        {
            const int32_t left = (int32_t)(high - low_bound);
            const int32_t right = (int32_t)(high_bound - low);
            if (left < right) {
                if (low < high_bound) {
                    stack_low[stack_count] = low;
                    stack_high[stack_count] = high_bound;
                    ++stack_count;
                }
                if (low_bound >= high)
                    goto pop;
                low = low_bound;
                high_bound = high;
            } else {
                if (low_bound < high) {
                    stack_low[stack_count] = low_bound;
                    stack_high[stack_count] = high;
                    ++stack_count;
                }
                if (low >= high_bound)
                    goto pop;
                low_bound = low;
                high = high_bound;
            }
            continue;
        }

    pop:
        if (--stack_count < 0) {
            set_eax(c, 0);
            return;
        }
        low_bound = low = stack_low[stack_count];
        high_bound = high = stack_high[stack_count];
    }
}

// ---------------------------------------------------------------------------
// Strings. Guest pointers are bounded before they are dereferenced.
// ---------------------------------------------------------------------------
const char *guest_cstr(uint32_t p) {
    return gm_valid(p, 1) ? (const char *)g_mem + p : "";
}

void crt_strcmp(X86 *c) {
    const unsigned char *a = (const unsigned char *)guest_cstr(arg(c, 0));
    const unsigned char *b = (const unsigned char *)guest_cstr(arg(c, 1));
    while (*a && *a == *b) {
        ++a;
        ++b;
    }
    set_eax(c, *a == *b ? 0u : (*a < *b ? 0xffffffffu : 1u));
}

void crt_strncmp(X86 *c) {
    const unsigned char *a = (const unsigned char *)guest_cstr(arg(c, 0));
    const unsigned char *b = (const unsigned char *)guest_cstr(arg(c, 1));
    uint32_t n = arg(c, 2);
    while (n--) {
        if (*a != *b || !*a)
            break;
        ++a;
        ++b;
    }
    set_eax(c, *a == *b ? 0u : (*a < *b ? 0xffffffffu : 1u));
}

int compare_ci(const unsigned char *a, const unsigned char *b, uint32_t n, bool bounded) {
    for (;;) {
        if (bounded && n == 0)
            return 0;
        unsigned char ca = *a, cb = *b;
        if (ca >= 'A' && ca <= 'Z')
            ca += 32;
        if (cb >= 'A' && cb <= 'Z')
            cb += 32;
        if (ca != cb)
            return ca < cb ? -1 : 1;
        if (!ca)
            return 0;
        ++a;
        ++b;
        if (bounded)
            --n;
    }
}

void crt_stricmp(X86 *c) {
    int r = compare_ci((const unsigned char *)guest_cstr(arg(c, 0)),
                       (const unsigned char *)guest_cstr(arg(c, 1)), 0, false);
    set_eax(c, (uint32_t)r);
}

void crt_strnicmp(X86 *c) {
    int r = compare_ci((const unsigned char *)guest_cstr(arg(c, 0)),
                       (const unsigned char *)guest_cstr(arg(c, 1)), arg(c, 2), true);
    set_eax(c, (uint32_t)r);
}

void crt_strncpy(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1), n = arg(c, 2);
    if (gm_valid(dst, n) && gm_valid(src, 1)) {
        uint32_t i = 0;
        for (; i < n && g_mem[src + i]; ++i)
            g_mem[dst + i] = g_mem[src + i];
        for (; i < n; ++i)
            g_mem[dst + i] = 0;
    }
    set_eax(c, dst);
}

void crt_strcpy(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1);
    if (gm_valid(dst, 1) && gm_valid(src, 1)) {
        uint32_t i = 0;
        for (; gm_valid(src + i, 1) && gm_valid(dst + i, 1); ++i) {
            g_mem[dst + i] = g_mem[src + i];
            if (!g_mem[src + i])
                break;
        }
    }
    set_eax(c, dst);
}

void crt_strcat(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1);
    if (gm_valid(dst, 1) && gm_valid(src, 1)) {
        uint32_t at = 0;
        while (gm_valid(dst + at, 1) && g_mem[dst + at])
            ++at;
        for (uint32_t i = 0; gm_valid(src + i, 1) && gm_valid(dst + at + i, 1); ++i) {
            g_mem[dst + at + i] = g_mem[src + i];
            if (!g_mem[src + i])
                break;
        }
    }
    set_eax(c, dst);
}

void crt_strncat(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1), n = arg(c, 2);
    if (gm_valid(dst, 1) && gm_valid(src, 1)) {
        uint32_t at = 0;
        while (gm_valid(dst + at, 1) && g_mem[dst + at])
            ++at;
        uint32_t i = 0;
        for (; i < n && gm_valid(src + i, 1) && g_mem[src + i]; ++i)
            g_mem[dst + at + i] = g_mem[src + i];
        if (gm_valid(dst + at + i, 1))
            g_mem[dst + at + i] = 0;
    }
    set_eax(c, dst);
}

void crt_strchr(X86 *c) {
    uint32_t p = arg(c, 0);
    uint8_t ch = (uint8_t)arg(c, 1);
    if (gm_valid(p, 1)) {
        for (uint32_t i = 0; gm_valid(p + i, 1); ++i) {
            if (g_mem[p + i] == ch) {
                set_eax(c, p + i);
                return;
            }
            if (!g_mem[p + i])
                break;
        }
    }
    set_eax(c, 0);
}

void crt_strrchr(X86 *c) {
    uint32_t p = arg(c, 0), hit = 0;
    uint8_t ch = (uint8_t)arg(c, 1);
    if (gm_valid(p, 1)) {
        for (uint32_t i = 0; gm_valid(p + i, 1); ++i) {
            if (g_mem[p + i] == ch)
                hit = p + i;
            if (!g_mem[p + i])
                break;
        }
    }
    set_eax(c, hit);
}

void crt_strstr(X86 *c) {
    uint32_t h = arg(c, 0), n = arg(c, 1);
    if (!gm_valid(h, 1) || !gm_valid(n, 1)) {
        set_eax(c, 0);
        return;
    }
    if (!g_mem[n]) {
        set_eax(c, h);
        return;
    }
    for (uint32_t i = 0; gm_valid(h + i, 1) && g_mem[h + i]; ++i) {
        uint32_t j = 0;
        while (g_mem[n + j] && gm_valid(h + i + j, 1) && g_mem[h + i + j] == g_mem[n + j])
            ++j;
        if (!g_mem[n + j]) {
            set_eax(c, h + i);
            return;
        }
    }
    set_eax(c, 0);
}

void crt_strpbrk(X86 *c) {
    uint32_t s = arg(c, 0), accept = arg(c, 1);
    if (gm_valid(s, 1) && gm_valid(accept, 1)) {
        for (uint32_t i = 0; gm_valid(s + i, 1) && g_mem[s + i]; ++i)
            for (uint32_t j = 0; gm_valid(accept + j, 1) && g_mem[accept + j]; ++j)
                if (g_mem[s + i] == g_mem[accept + j]) {
                    set_eax(c, s + i);
                    return;
                }
    }
    set_eax(c, 0);
}

void crt_strspn(X86 *c) {
    uint32_t s = arg(c, 0), accept = arg(c, 1), n = 0;
    if (gm_valid(s, 1) && gm_valid(accept, 1)) {
        for (; gm_valid(s + n, 1) && g_mem[s + n]; ++n) {
            bool found = false;
            for (uint32_t j = 0; gm_valid(accept + j, 1) && g_mem[accept + j]; ++j)
                found = found || g_mem[s + n] == g_mem[accept + j];
            if (!found)
                break;
        }
    }
    set_eax(c, n);
}

void crt_strcspn(X86 *c) {
    uint32_t s = arg(c, 0), reject = arg(c, 1), n = 0;
    if (gm_valid(s, 1) && gm_valid(reject, 1)) {
        for (; gm_valid(s + n, 1) && g_mem[s + n]; ++n) {
            for (uint32_t j = 0; gm_valid(reject + j, 1) && g_mem[reject + j]; ++j)
                if (g_mem[s + n] == g_mem[reject + j]) {
                    set_eax(c, n);
                    return;
                }
        }
    }
    set_eax(c, n);
}

void crt_strlwr(X86 *c) {
    uint32_t p = arg(c, 0);
    if (gm_valid(p, 1))
        for (uint32_t i = 0; gm_valid(p + i, 1) && g_mem[p + i]; ++i)
            if (g_mem[p + i] >= 'A' && g_mem[p + i] <= 'Z')
                g_mem[p + i] = (uint8_t)(g_mem[p + i] + 32);
    set_eax(c, p);
}

void crt_wcslwr(X86 *c) {
    uint32_t p = arg(c, 0);
    if (gm_valid(p, 2))
        for (uint32_t i = 0; gm_valid(p + i * 2, 2) && rd16(p + i * 2); ++i) {
            uint16_t ch = rd16(p + i * 2);
            if (ch >= 'A' && ch <= 'Z')
                wr16(p + i * 2, (uint16_t)(ch + 32));
        }
    set_eax(c, p);
}

uint32_t wcs_len(uint32_t p) {
    uint32_t n = 0;
    while (gm_valid(p + n * 2, 2) && rd16(p + n * 2))
        ++n;
    return n;
}

bool wcs_contains(uint32_t p, uint16_t ch) {
    for (uint32_t i = 0; gm_valid(p + i * 2, 2); ++i) {
        uint16_t unit = rd16(p + i * 2);
        if (unit == ch)
            return true;
        if (!unit)
            break;
    }
    return false;
}

void crt_wcsncpy(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1);
    uint32_t n = dst < GUEST_SIZE ? std::min(arg(c, 2), (GUEST_SIZE - dst) / 2) : 0;
    uint32_t i = 0;
    for (; i < n; ++i) {
        uint16_t ch = gm_valid(src + i * 2, 2) ? rd16(src + i * 2) : 0;
        if (!ch)
            break;
        wr16(dst + i * 2, ch);
    }
    for (; i < n; ++i)
        wr16(dst + i * 2, 0);
    set_eax(c, dst);
}

void crt_wcsncat(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1);
    uint32_t at = wcs_len(dst);
    uint32_t cap = dst < GUEST_SIZE ? (GUEST_SIZE - dst) / 2 : 0;
    if (at < cap) {
        uint32_t n = std::min(arg(c, 2), cap - at - 1), i = 0;
        for (; i < n; ++i) {
            uint16_t ch = gm_valid(src + i * 2, 2) ? rd16(src + i * 2) : 0;
            if (!ch)
                break;
            wr16(dst + (at + i) * 2, ch);
        }
        wr16(dst + (at + i) * 2, 0);
    }
    set_eax(c, dst);
}

void crt_wcscspn(X86 *c) {
    uint32_t s = arg(c, 0), reject = arg(c, 1), n = 0;
    while (gm_valid(s + n * 2, 2)) {
        uint16_t ch = rd16(s + n * 2);
        if (!ch || wcs_contains(reject, ch))
            break;
        ++n;
    }
    set_eax(c, n);
}

void crt_wcschr(X86 *c) {
    uint32_t s = arg(c, 0);
    uint16_t ch = (uint16_t)arg(c, 1);
    for (uint32_t i = 0; gm_valid(s + i * 2, 2); ++i) {
        uint16_t unit = rd16(s + i * 2);
        if (unit == ch) {
            set_eax(c, s + i * 2);
            return;
        }
        if (!unit)
            break;
    }
    set_eax(c, 0);
}

void crt_wcsrchr(X86 *c) {
    uint32_t s = arg(c, 0), hit = 0;
    uint16_t ch = (uint16_t)arg(c, 1);
    for (uint32_t i = 0; gm_valid(s + i * 2, 2); ++i) {
        uint16_t unit = rd16(s + i * 2);
        if (unit == ch)
            hit = s + i * 2;
        if (!unit)
            break;
    }
    set_eax(c, hit);
}

void crt_wcsstr(X86 *c) {
    uint32_t haystack = arg(c, 0), needle = arg(c, 1);
    if (!gm_valid(haystack, 2) || !gm_valid(needle, 2)) {
        set_eax(c, 0);
        return;
    }
    if (!rd16(needle)) {
        set_eax(c, haystack);
        return;
    }
    for (uint32_t i = 0; gm_valid(haystack + i * 2, 2) && rd16(haystack + i * 2); ++i) {
        uint32_t j = 0;
        while (gm_valid(needle + j * 2, 2) && rd16(needle + j * 2) &&
               gm_valid(haystack + (i + j) * 2, 2) &&
               rd16(haystack + (i + j) * 2) == rd16(needle + j * 2))
            ++j;
        if (gm_valid(needle + j * 2, 2) && !rd16(needle + j * 2)) {
            set_eax(c, haystack + i * 2);
            return;
        }
    }
    set_eax(c, 0);
}

void crt_wcsncmp(X86 *c) {
    uint32_t a = arg(c, 0), b = arg(c, 1), n = arg(c, 2);
    for (uint32_t i = 0; i < n; ++i) {
        uint16_t ca = gm_valid(a + i * 2, 2) ? rd16(a + i * 2) : 0;
        uint16_t cb = gm_valid(b + i * 2, 2) ? rd16(b + i * 2) : 0;
        if (ca != cb) {
            set_eax(c, ca < cb ? 0xffffffffu : 1u);
            return;
        }
        if (!ca)
            break;
    }
    set_eax(c, 0);
}

int compare_wci(uint32_t a, uint32_t b, uint32_t n, bool bounded) {
    for (uint32_t i = 0; !bounded || i < n; ++i) {
        uint16_t ca = gm_valid(a + i * 2, 2) ? rd16(a + i * 2) : 0;
        uint16_t cb = gm_valid(b + i * 2, 2) ? rd16(b + i * 2) : 0;
        if (ca >= 'A' && ca <= 'Z')
            ca += 32;
        if (cb >= 'A' && cb <= 'Z')
            cb += 32;
        if (ca != cb)
            return ca < cb ? -1 : 1;
        if (!ca)
            return 0;
    }
    return 0;
}

void crt_wcsicmp(X86 *c) {
    set_eax(c, (uint32_t)compare_wci(arg(c, 0), arg(c, 1), 0, false));
}

void crt_wcsnicmp(X86 *c) {
    set_eax(c, (uint32_t)compare_wci(arg(c, 0), arg(c, 1), arg(c, 2), true));
}

void crt_strtol(X86 *c) {
    uint32_t p = arg(c, 0), endp = arg(c, 1);
    int base = (int)arg(c, 2);
    const char *s = guest_cstr(p);
    const char *start = s;
    while (*s == ' ' || *s == '\t' || *s == '\r' || *s == '\n')
        ++s;
    bool negative = false;
    if (*s == '-' || *s == '+')
        negative = *s++ == '-';
    if (base == 0) {
        if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) {
            base = 16;
            s += 2;
        } else if (s[0] == '0') {
            base = 8;
            ++s;
        } else {
            base = 10;
        }
    } else if (base == 16 && s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) {
        s += 2;
    }
    if (base < 2 || base > 36) {
        if (endp && gm_valid(endp, 4))
            wr32(endp, p + (uint32_t)(start - guest_cstr(p)));
        set_eax(c, 0);
        return;
    }
    uint32_t value = 0;
    const char *digits = s;
    for (;; ++s) {
        int d = -1;
        if (*s >= '0' && *s <= '9')
            d = *s - '0';
        else if (*s >= 'a' && *s <= 'z')
            d = *s - 'a' + 10;
        else if (*s >= 'A' && *s <= 'Z')
            d = *s - 'A' + 10;
        if (d < 0 || d >= base)
            break;
        value = value * (uint32_t)base + (uint32_t)d;
    }
    if (s == digits)
        s = start;
    if (endp && gm_valid(endp, 4))
        wr32(endp, p + (uint32_t)(s - guest_cstr(p)));
    set_eax(c, negative ? (uint32_t)(-(int32_t)value) : value);
}

int wide_digit(uint16_t ch) {
    if (ch >= '0' && ch <= '9')
        return ch - '0';
    if (ch >= 'a' && ch <= 'z')
        return ch - 'a' + 10;
    if (ch >= 'A' && ch <= 'Z')
        return ch - 'A' + 10;
    return -1;
}

uint64_t wide_parse_uint(uint32_t p, int base, bool &negative, uint32_t &end) {
    auto unit = [p](uint32_t i) { return gm_valid(p + i * 2, 2) ? rd16(p + i * 2) : (uint16_t)0; };
    uint32_t i = 0;
    while (unit(i) == ' ' || unit(i) == '\t' || unit(i) == '\r' || unit(i) == '\n' ||
           unit(i) == '\f' || unit(i) == '\v')
        ++i;
    negative = false;
    if (unit(i) == '-' || unit(i) == '+') {
        negative = unit(i) == '-';
        ++i;
    }
    if (base == 0) {
        if (unit(i) == '0' && (unit(i + 1) == 'x' || unit(i + 1) == 'X')) {
            base = 16;
            i += 2;
        } else if (unit(i) == '0') {
            base = 8;
        } else {
            base = 10;
        }
    } else if (base == 16 && unit(i) == '0' && (unit(i + 1) == 'x' || unit(i + 1) == 'X')) {
        i += 2;
    }
    if (base < 2 || base > 36) {
        end = 0;
        return 0;
    }
    uint32_t digits = i;
    uint64_t value = 0;
    bool overflow = false;
    for (;; ++i) {
        int d = wide_digit(unit(i));
        if (d < 0 || d >= base)
            break;
        if (value > (UINT64_MAX - (uint32_t)d) / (uint32_t)base)
            overflow = true;
        else
            value = value * (uint32_t)base + (uint32_t)d;
    }
    if (i == digits) {
        end = 0;
        return 0;
    }
    end = i;
    return overflow ? UINT64_MAX : value;
}

void crt_wcstol(X86 *c) {
    uint32_t p = arg(c, 0), endp = arg(c, 1);
    bool negative = false;
    uint32_t end = 0;
    uint64_t value = wide_parse_uint(p, (int)arg(c, 2), negative, end);
    if (endp && gm_valid(endp, 4))
        wr32(endp, p + end * 2);
    if (negative) {
        if (value > 0x80000000ull)
            value = 0x80000000ull;
        set_eax(c, (uint32_t)(-(int64_t)value));
    } else {
        set_eax(c, (uint32_t)std::min<uint64_t>(value, 0x7fffffffull));
    }
}

void crt_wcstoul(X86 *c) {
    uint32_t p = arg(c, 0), endp = arg(c, 1);
    bool negative = false;
    uint32_t end = 0;
    uint64_t value = wide_parse_uint(p, (int)arg(c, 2), negative, end);
    if (endp && gm_valid(endp, 4))
        wr32(endp, p + end * 2);
    uint32_t clamped = (uint32_t)std::min<uint64_t>(value, 0xffffffffull);
    set_eax(c, negative ? 0u - clamped : clamped);
}

void crt_wcstod(X86 *c) {
    uint32_t p = arg(c, 0), endp = arg(c, 1);
    std::string narrow;
    uint32_t i = 0;
    for (; i < 0x8000; ++i) {
        uint16_t ch = gm_valid(p + i * 2, 2) ? rd16(p + i * 2) : 0;
        if (!ch)
            break;
        narrow += ch < 0x80 ? (char)ch : '?';
    }
    char *end = nullptr;
    double value = std::strtod(narrow.c_str(), &end);
    if (endp && gm_valid(endp, 4))
        wr32(endp, p + (uint32_t)(end - narrow.c_str()) * 2);
    fpush(c, value);
}

void crt_wtol(X86 *c) {
    uint32_t p = arg(c, 0);
    bool negative = false;
    uint32_t end = 0;
    uint64_t value = wide_parse_uint(p, 10, negative, end);
    if (negative) {
        if (value > 0x80000000ull)
            value = 0x80000000ull;
        set_eax(c, (uint32_t)(-(int64_t)value));
    } else {
        set_eax(c, (uint32_t)std::min<uint64_t>(value, 0x7fffffffull));
    }
}

void write_uint_wstr(X86 *c, uint32_t value, uint32_t dst, int radix, bool signed_value) {
    if (radix < 2 || radix > 36)
        radix = 10;
    bool negative = signed_value && (int32_t)value < 0 && radix == 10;
    uint32_t v = negative ? (uint32_t)(-(int64_t)(int32_t)value) : value;
    char digits[34];
    uint32_t n = 0;
    do {
        uint32_t d = v % (uint32_t)radix;
        digits[n++] = (char)(d < 10 ? '0' + d : 'a' + d - 10);
        v /= (uint32_t)radix;
    } while (v);
    if (negative)
        digits[n++] = '-';
    if (gm_valid(dst, (n + 1) * 2)) {
        for (uint32_t i = 0; i < n; ++i)
            wr16(dst + i * 2, (uint16_t)digits[n - 1 - i]);
        wr16(dst + n * 2, 0);
    }
    set_eax(c, dst);
}

void crt_ltow(X86 *c) {
    write_uint_wstr(c, arg(c, 0), arg(c, 1), (int)arg(c, 2), true);
}

void crt_ultow(X86 *c) {
    write_uint_wstr(c, arg(c, 0), arg(c, 1), (int)arg(c, 2), false);
}

void write_uint_str(X86 *c, uint32_t value, uint32_t dst, int radix, bool signed_value) {
    if (radix < 2 || radix > 36)
        radix = 10;
    bool negative = signed_value && (int32_t)value < 0 && radix == 10;
    uint32_t v = negative ? (uint32_t)(-(int64_t)(int32_t)value) : value;
    char digits[34];
    uint32_t n = 0;
    do {
        uint32_t d = v % (uint32_t)radix;
        digits[n++] = (char)(d < 10 ? '0' + d : 'a' + d - 10);
        v /= (uint32_t)radix;
    } while (v);
    if (negative)
        digits[n++] = '-';
    if (gm_valid(dst, n + 1))
        for (uint32_t i = 0; i < n; ++i)
            g_mem[dst + i] = (uint8_t)digits[n - 1 - i];
    if (gm_valid(dst, n + 1))
        g_mem[dst + n] = 0;
    set_eax(c, dst);
}

void crt_ltoa(X86 *c) {
    write_uint_str(c, arg(c, 0), arg(c, 1), (int)arg(c, 2), true);
}

void crt_ultoa(X86 *c) {
    write_uint_str(c, arg(c, 0), arg(c, 1), (int)arg(c, 2), false);
}

void crt_atof(X86 *c) {
    fpush(c, std::strtod(guest_cstr(arg(c, 0)), nullptr));
}

void crt_strtod(X86 *c) {
    uint32_t p = arg(c, 0), endp = arg(c, 1);
    const char *s = guest_cstr(p);
    char *end = nullptr;
    double value = std::strtod(s, &end);
    if (endp && gm_valid(endp, 4))
        wr32(endp, p + (uint32_t)(end - s));
    fpush(c, value);
}

void crt_strtoul(X86 *c) {
    uint32_t p = arg(c, 0), endp = arg(c, 1);
    const char *s = guest_cstr(p);
    char *end = nullptr;
    uint64_t value = std::strtoull(s, &end, (int)arg(c, 2));
    if (endp && gm_valid(endp, 4))
        wr32(endp, p + (uint32_t)(end - s));
    if (value > 0xffffffffull) {
        value = 0xffffffffull;
        wr32(guest_slot(g_errno_addr), ERANGE);
    }
    set_eax(c, (uint32_t)value);
}

void crt_strtoui64(X86 *c) {
    uint32_t p = arg(c, 0), endp = arg(c, 1);
    const char *s = guest_cstr(p);
    char *end = nullptr;
    uint64_t value = std::strtoull(s, &end, (int)arg(c, 2));
    if (endp && gm_valid(endp, 4))
        wr32(endp, p + (uint32_t)(end - s));
    set_eax64(c, value);
}

uint32_t tm_slot() {
    if (!g_tm_slot)
        g_tm_slot = heap_alloc(36, true);
    return g_tm_slot;
}

void write_tm(uint32_t dst, const std::tm &tm) {
    int fields[9] = {tm.tm_sec,  tm.tm_min,  tm.tm_hour, tm.tm_mday, tm.tm_mon,
                     tm.tm_year, tm.tm_wday, tm.tm_yday, tm.tm_isdst};
    for (uint32_t i = 0; i < 9; ++i)
        wr32(dst + i * 4, (uint32_t)fields[i]);
}

bool read_tm(uint32_t src, std::tm &tm) {
    if (!src || !gm_valid(src, 36))
        return false;
    tm = {};
    tm.tm_sec = (int)rd32(src + 0);
    tm.tm_min = (int)rd32(src + 4);
    tm.tm_hour = (int)rd32(src + 8);
    tm.tm_mday = (int)rd32(src + 12);
    tm.tm_mon = (int)rd32(src + 16);
    tm.tm_year = (int)rd32(src + 20);
    tm.tm_wday = (int)rd32(src + 24);
    tm.tm_yday = (int)rd32(src + 28);
    tm.tm_isdst = (int)rd32(src + 32);
    return true;
}

template <bool Wide> void crt_stat64i32(X86 *c) {
    uint32_t name = arg(c, 0), dst = arg(c, 1);
    if (!name || !dst || !gm_valid(dst, 48)) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0xffffffffu);
        return;
    }
    std::string host =
        win32_host_path_op(Wide ? gm_wstr(name) : gm_str(name, 4096), WIN32_FILE_READ);
    OsStat st{};
    if (host.empty() || os_stat(host.c_str(), &st) != 0) {
        wr32(guest_slot(g_errno_addr), ENOENT);
        set_eax(c, 0xffffffffu);
        return;
    }
    uint16_t mode = st.is_dir ? 0x4000 : 0x8000; // _S_IFDIR / _S_IFREG
    mode |= 0x0100;                              // _S_IREAD
    if (!st.is_readonly)
        mode |= 0x0080; // _S_IWRITE
    wr32(dst + 0, 0);
    wr16(dst + 4, (uint16_t)st.ino);
    wr16(dst + 6, mode);
    wr16(dst + 8, st.is_dir ? 2 : 1);
    wr16(dst + 10, 0);
    wr16(dst + 12, 0);
    wr32(dst + 16, 0);
    wr32(dst + 20, st.is_dir ? 0 : (uint32_t)st.size);
    wr64(dst + 24, (uint64_t)st.atime);
    wr64(dst + 32, (uint64_t)st.mtime);
    wr64(dst + 40, (uint64_t)st.ctime);
    set_eax(c, 0);
}

void crt_time64(X86 *c) {
    uint64_t now = (uint64_t)std::time(nullptr);
    uint32_t dst = arg(c, 0);
    if (dst && gm_valid(dst, 8)) {
        wr32(dst, (uint32_t)now);
        wr32(dst + 4, (uint32_t)(now >> 32));
    }
    set_eax64(c, now);
}

void crt_time32(X86 *c) {
    uint32_t now = (uint32_t)std::time(nullptr);
    uint32_t dst = arg(c, 0);
    if (dst && gm_valid(dst, 4))
        wr32(dst, now);
    set_eax(c, now);
}

void crt_localtime64(X86 *c) {
    uint32_t p = arg(c, 0);
    if (!p || !gm_valid(p, 8)) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0);
        return;
    }
    uint64_t raw = (uint64_t)rd32(p) | ((uint64_t)rd32(p + 4) << 32);
    std::time_t value = (std::time_t)(int64_t)raw;
    std::tm *tm = std::localtime(&value);
    if (!tm) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0);
        return;
    }
    uint32_t out = tm_slot();
    write_tm(out, *tm);
    set_eax(c, out);
}

void crt_gmtime64(X86 *c) {
    uint32_t p = arg(c, 0);
    if (!p || !gm_valid(p, 8)) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0);
        return;
    }
    uint64_t raw = (uint64_t)rd32(p) | ((uint64_t)rd32(p + 4) << 32);
    std::time_t value = (std::time_t)(int64_t)raw;
    std::tm *tm = std::gmtime(&value);
    if (!tm) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax(c, 0);
        return;
    }
    uint32_t out = tm_slot();
    write_tm(out, *tm);
    set_eax(c, out);
}

void crt_mktime64(X86 *c) {
    uint32_t p = arg(c, 0);
    std::tm tm;
    if (!read_tm(p, tm)) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax64(c, (uint64_t)-1);
        return;
    }
    std::time_t value = std::mktime(&tm);
    if (value == (std::time_t)-1) {
        wr32(guest_slot(g_errno_addr), EINVAL);
        set_eax64(c, (uint64_t)-1);
        return;
    }
    write_tm(p, tm);
    set_eax64(c, (uint64_t)value);
}

void crt_strftime(X86 *c) {
    uint32_t out = arg(c, 0), cap = arg(c, 1);
    std::tm tm;
    if (!out || !cap || cap > 0x1000000 || !read_tm(arg(c, 3), tm)) {
        set_eax(c, 0);
        return;
    }
    std::string format = gm_str(arg(c, 2));
    std::vector<char> text(cap);
    size_t n = std::strftime(text.data(), cap, format.c_str(), &tm);
    if (!n) {
        set_eax(c, 0);
        return;
    }
    set_eax(c, gm_put_str(out, text.data(), cap));
}

void crt_purecall(X86 *c) {
    LOGW("_purecall: pure virtual function call");
    fflush(stdout);
    fflush(stderr);
    abort();
}

void crt_atoi(X86 *c) {
    uint32_t p = arg(c, 0);
    const char *s = guest_cstr(p);
    while (*s == ' ' || *s == '\t' || *s == '\r' || *s == '\n')
        ++s;
    bool negative = false;
    if (*s == '-' || *s == '+')
        negative = *s++ == '-';
    int32_t value = 0;
    while (*s >= '0' && *s <= '9')
        value = value * 10 + (*s++ - '0');
    set_eax(c, (uint32_t)(negative ? -value : value));
}

int ascii_class(int ch) {
    if (ch < 0 || ch > 0xff)
        return 0;
    return ch;
}

enum class FmtLen { None, H, HH, L, LL, LDouble, I64, I32, W, Z, T };

struct FmtArgs {
    uint32_t p = 0;
    uint32_t u32() {
        uint32_t v = gm_valid(p, 4) ? rd32(p) : 0;
        p += 4;
        return v;
    }
    uint64_t u64() {
        uint64_t lo = u32();
        uint64_t hi = u32();
        return lo | (hi << 32);
    }
    double f64() {
        uint64_t bits = u64();
        double d = 0;
        memcpy(&d, &bits, sizeof d);
        return d;
    }
};

std::string format_host(const std::string &fmt, FmtArgs &args) {
    std::string out;
    size_t i = 0;
    while (i < fmt.size()) {
        if (fmt[i] != '%') {
            out += fmt[i++];
            continue;
        }
        if (i + 1 < fmt.size() && fmt[i + 1] == '%') {
            out += '%';
            i += 2;
            continue;
        }
        size_t start = i++;
        std::string flags;
        while (i < fmt.size() && strchr("-+ 0#", fmt[i]) && fmt[i])
            flags += fmt[i++];
        int width = -1;
        if (i < fmt.size() && fmt[i] == '*') {
            width = (int32_t)args.u32();
            if (width < 0) {
                width = -width;
                flags += '-';
            }
            ++i;
        } else {
            while (i < fmt.size() && fmt[i] >= '0' && fmt[i] <= '9')
                width = (width < 0 ? 0 : width) * 10 + (fmt[i++] - '0');
        }
        int precision = -1;
        if (i < fmt.size() && fmt[i] == '.') {
            ++i;
            precision = 0;
            if (i < fmt.size() && fmt[i] == '*') {
                precision = (int32_t)args.u32();
                if (precision < 0)
                    precision = -1;
                ++i;
            } else {
                while (i < fmt.size() && fmt[i] >= '0' && fmt[i] <= '9')
                    precision = precision * 10 + (fmt[i++] - '0');
            }
        }
        FmtLen len = FmtLen::None;
        bool wide_prefix = false;
        if (i < fmt.size()) {
            char m = fmt[i];
            if (m == 'h') {
                ++i;
                if (i < fmt.size() && fmt[i] == 'h') {
                    len = FmtLen::HH;
                    ++i;
                } else {
                    len = FmtLen::H;
                }
            } else if (m == 'l') {
                ++i;
                if (i < fmt.size() && fmt[i] == 'l') {
                    len = FmtLen::LL;
                    ++i;
                } else {
                    len = FmtLen::L;
                }
            } else if (m == 'L') {
                len = FmtLen::LDouble;
                ++i;
            } else if (m == 'I') {
                ++i;
                if (i + 1 < fmt.size() && fmt[i] == '6' && fmt[i + 1] == '4') {
                    len = FmtLen::I64;
                    i += 2;
                } else if (i + 1 < fmt.size() && fmt[i] == '3' && fmt[i + 1] == '2') {
                    len = FmtLen::I32;
                    i += 2;
                } else {
                    len = FmtLen::I32;
                }
            } else if (m == 'w') {
                wide_prefix = true;
                ++i;
            } else if (m == 'z') {
                len = FmtLen::Z;
                ++i;
            } else if (m == 't') {
                len = FmtLen::T;
                ++i;
            } else if (m == 'j') {
                len = FmtLen::LL;
                ++i;
            }
        }
        if (i >= fmt.size())
            break;
        char conv = fmt[i++];

        char host[96];
        size_t h = 0;
        host[h++] = '%';
        for (char f : flags)
            if (h + 2 < sizeof host)
                host[h++] = f;
        if (width >= 0)
            h += (size_t)snprintf(host + h, sizeof host - h, "%d", width);
        if (precision >= 0)
            h += (size_t)snprintf(host + h, sizeof host - h, ".%d", precision);

        bool is64 = len == FmtLen::LL || len == FmtLen::I64 || len == FmtLen::LDouble;
        switch (conv) {
        case 'd':
        case 'i':
        case 'u':
        case 'o':
        case 'x':
        case 'X': {
            bool sign = conv == 'd' || conv == 'i';
            if (is64) {
                uint64_t v = args.u64();
                host[h++] = 'l';
                host[h++] = 'l';
                host[h++] = sign ? 'd' : conv;
                host[h] = 0;
                int n = snprintf(nullptr, 0, host,
                                 sign ? (long long)(int64_t)v : (unsigned long long)v);
                if (n < 0)
                    break;
                std::vector<char> b((size_t)n + 1);
                snprintf(b.data(), b.size(), host,
                         sign ? (long long)(int64_t)v : (unsigned long long)v);
                out.append(b.data(), (size_t)n);
            } else {
                uint32_t v = args.u32();
                if (len == FmtLen::H) {
                    v = sign ? (uint32_t)(int32_t)(int16_t)v : (uint32_t)(uint16_t)v;
                } else if (len == FmtLen::HH) {
                    v = sign ? (uint32_t)(int32_t)(int8_t)v : (uint32_t)(uint8_t)v;
                }
                host[h++] = conv;
                host[h] = 0;
                int n = snprintf(nullptr, 0, host, sign ? (int)(int32_t)v : (unsigned)v);
                if (n < 0)
                    break;
                std::vector<char> b((size_t)n + 1);
                snprintf(b.data(), b.size(), host, sign ? (int)(int32_t)v : (unsigned)v);
                out.append(b.data(), (size_t)n);
            }
            break;
        }
        case 'c':
        case 'C': {
            bool wide = wide_prefix || len == FmtLen::L || conv == 'C';
            if (len == FmtLen::H)
                wide = false;
            uint32_t v = args.u32();
            std::string s;
            if (wide) {
                if (v < 0x80) {
                    s += (char)v;
                } else if (v < 0x800) {
                    s += (char)(0xc0 | (v >> 6));
                    s += (char)(0x80 | (v & 0x3f));
                } else {
                    s += (char)(0xe0 | (v >> 12));
                    s += (char)(0x80 | ((v >> 6) & 0x3f));
                    s += (char)(0x80 | (v & 0x3f));
                }
            } else {
                s += (char)v;
            }
            host[h++] = 's';
            host[h] = 0;
            int n = snprintf(nullptr, 0, host, s.c_str());
            if (n >= 0) {
                std::vector<char> b((size_t)n + 1);
                snprintf(b.data(), b.size(), host, s.c_str());
                out.append(b.data(), (size_t)n);
            }
            break;
        }
        case 's':
        case 'S': {
            bool wide = wide_prefix || len == FmtLen::L || conv == 'S';
            if (len == FmtLen::H)
                wide = false;
            uint32_t p = args.u32();
            std::string s = wide ? gm_wstr(p) : std::string(guest_cstr(p));
            if (p == 0)
                s = "(null)";
            host[h++] = 's';
            host[h] = 0;
            int n = snprintf(nullptr, 0, host, s.c_str());
            if (n >= 0) {
                std::vector<char> b((size_t)n + 1);
                snprintf(b.data(), b.size(), host, s.c_str());
                out.append(b.data(), (size_t)n);
            }
            break;
        }
        case 'p': {
            uint32_t v = args.u32();
            int n = snprintf(nullptr, 0, "%08X", v);
            std::vector<char> b((size_t)(n < 0 ? 0 : n) + 1);
            if (n > 0)
                snprintf(b.data(), b.size(), "%08X", v);
            out.append(b.data(), (size_t)(n < 0 ? 0 : n));
            break;
        }
        case 'f':
        case 'F':
        case 'e':
        case 'E':
        case 'g':
        case 'G':
        case 'a':
        case 'A': {
            double v = args.f64();
            host[h++] = (conv == 'F') ? 'f' : conv;
            host[h] = 0;
            int n = snprintf(nullptr, 0, host, v);
            if (n >= 0) {
                std::vector<char> b((size_t)n + 1);
                snprintf(b.data(), b.size(), host, v);
                out.append(b.data(), (size_t)n);
            }
            break;
        }
        case 'n': {
            uint32_t p = args.u32();
            int count = (int)out.size();
            if (gm_valid(p, 1)) {
                if (len == FmtLen::HH)
                    wr8(p, (uint8_t)count);
                else if (len == FmtLen::H)
                    wr16(p, (uint16_t)count);
                else
                    wr32(p, (uint32_t)count);
            }
            break;
        }
        default:
            out.append(fmt, start, i - start);
            break;
        }
    }
    return out;
}

std::string guest_format(uint32_t fmtp, uint32_t argp) {
    FmtArgs args;
    args.p = argp;
    return format_host(std::string(guest_cstr(fmtp)), args);
}

bool scan_space(char ch) {
    return std::isspace((unsigned char)ch) != 0;
}

void scan_write_int(uint32_t p, FmtLen len, bool sign, uint64_t value) {
    if (len == FmtLen::LL || len == FmtLen::I64) {
        if (!gm_valid(p, 8))
            return;
        wr32(p, (uint32_t)value);
        wr32(p + 4, (uint32_t)(value >> 32));
    } else if (len == FmtLen::H) {
        if (!gm_valid(p, 2))
            return;
        wr16(p, sign ? (uint16_t)(int16_t)value : (uint16_t)value);
    } else if (len == FmtLen::HH) {
        if (!gm_valid(p, 1))
            return;
        wr8(p, sign ? (uint8_t)(int8_t)value : (uint8_t)value);
    } else if (gm_valid(p, 4)) {
        wr32(p, sign ? (uint32_t)(int32_t)value : (uint32_t)value);
    }
}

int scan_guest(const std::string &input, const std::string &fmt, uint32_t argp) {
    size_t i = 0, s = 0;
    int assigned = 0;
    bool converted = false;
    while (i < fmt.size()) {
        if (scan_space(fmt[i])) {
            while (i < fmt.size() && scan_space(fmt[i]))
                ++i;
            while (s < input.size() && scan_space(input[s]))
                ++s;
            continue;
        }
        if (fmt[i] != '%') {
            if (s >= input.size())
                return converted ? assigned : -1;
            if (input[s] != fmt[i])
                break;
            ++s;
            ++i;
            continue;
        }
        ++i;
        if (i < fmt.size() && fmt[i] == '%') {
            if (s >= input.size())
                return converted ? assigned : -1;
            if (input[s] != '%')
                break;
            ++s;
            ++i;
            continue;
        }
        bool suppress = false;
        if (i < fmt.size() && fmt[i] == '*') {
            suppress = true;
            ++i;
        }
        int width = -1;
        while (i < fmt.size() && fmt[i] >= '0' && fmt[i] <= '9')
            width = (width < 0 ? 0 : width) * 10 + (fmt[i++] - '0');
        FmtLen len = FmtLen::None;
        if (i < fmt.size()) {
            char m = fmt[i];
            if (m == 'h') {
                ++i;
                if (i < fmt.size() && fmt[i] == 'h') {
                    len = FmtLen::HH;
                    ++i;
                } else {
                    len = FmtLen::H;
                }
            } else if (m == 'l') {
                ++i;
                if (i < fmt.size() && fmt[i] == 'l') {
                    len = FmtLen::LL;
                    ++i;
                } else {
                    len = FmtLen::L;
                }
            } else if (m == 'L') {
                len = FmtLen::LDouble;
                ++i;
            } else if (m == 'I') {
                ++i;
                if (i + 1 < fmt.size() && fmt[i] == '6' && fmt[i + 1] == '4') {
                    len = FmtLen::I64;
                    i += 2;
                } else if (i + 1 < fmt.size() && fmt[i] == '3' && fmt[i + 1] == '2') {
                    len = FmtLen::I32;
                    i += 2;
                } else {
                    len = FmtLen::I32;
                }
            } else if (m == 'w') {
                ++i;
            } else if (m == 'z') {
                len = FmtLen::Z;
                ++i;
            } else if (m == 't') {
                len = FmtLen::T;
                ++i;
            } else if (m == 'j') {
                len = FmtLen::LL;
                ++i;
            }
        }
        if (i >= fmt.size())
            break;
        char conv = fmt[i++];
        uint32_t dst = suppress ? 0 : (gm_valid(argp, 4) ? rd32(argp) : 0);
        if (!suppress)
            argp += 4;
        if (conv != 'c' && conv != '[' && conv != 'n')
            while (s < input.size() && scan_space(input[s]))
                ++s;

        if (conv == 'n') {
            if (!suppress)
                scan_write_int(dst, len, true, s);
            converted = true;
            continue;
        }
        if (s >= input.size())
            return converted ? assigned : -1;
        if (conv == 'c') {
            size_t n = width < 0 ? 1 : (size_t)width;
            if (s + n > input.size() || (!suppress && !gm_valid(dst, (uint32_t)n)))
                break;
            if (!suppress) {
                for (size_t k = 0; k < n; ++k)
                    wr8(dst + (uint32_t)k, (uint8_t)input[s + k]);
                ++assigned;
            }
            s += n;
            converted = true;
            continue;
        }
        if (conv == 's') {
            size_t begin = s;
            size_t limit = width < 0 ? input.size() - s : (size_t)width;
            while (s < input.size() && s - begin < limit && !scan_space(input[s]))
                ++s;
            size_t n = s - begin;
            if (!n)
                break;
            converted = true;
            if (!suppress) {
                if (!gm_valid(dst, (uint32_t)n + 1))
                    break;
                for (size_t k = 0; k < n; ++k)
                    wr8(dst + (uint32_t)k, (uint8_t)input[begin + k]);
                wr8(dst + (uint32_t)n, 0);
                ++assigned;
            }
            continue;
        }
        size_t token_len = width < 0 ? input.size() - s : (size_t)width;
        std::string token = input.substr(s, token_len);
        char *end = nullptr;
        errno = 0;
        if (conv == 'd' || conv == 'i' || conv == 'u' || conv == 'o' || conv == 'x' ||
            conv == 'X' || conv == 'p') {
            bool sign = conv == 'd' || conv == 'i';
            int base = conv == 'o' ? 8 : (conv == 'x' || conv == 'X' || conv == 'p' ? 16 : 10);
            if (conv == 'i')
                base = 0;
            uint64_t value = sign ? (uint64_t)std::strtoll(token.c_str(), &end, base)
                                  : std::strtoull(token.c_str(), &end, base);
            if (end == token.c_str())
                break;
            s += (size_t)(end - token.c_str());
            converted = true;
            if (!suppress) {
                scan_write_int(dst, conv == 'p' ? FmtLen::I32 : len, sign, value);
                ++assigned;
            }
            continue;
        }
        if (conv == 'f' || conv == 'F' || conv == 'e' || conv == 'E' || conv == 'g' ||
            conv == 'G' || conv == 'a' || conv == 'A') {
            double value = std::strtod(token.c_str(), &end);
            if (end == token.c_str())
                break;
            s += (size_t)(end - token.c_str());
            converted = true;
            if (!suppress) {
                bool as_double = len == FmtLen::L || len == FmtLen::LDouble;
                if (as_double && gm_valid(dst, 8)) {
                    uint64_t bits;
                    memcpy(&bits, &value, sizeof bits);
                    wr32(dst, (uint32_t)bits);
                    wr32(dst + 4, (uint32_t)(bits >> 32));
                } else if (!as_double && gm_valid(dst, 4)) {
                    float f = (float)value;
                    uint32_t bits;
                    memcpy(&bits, &f, sizeof bits);
                    wr32(dst, bits);
                }
                ++assigned;
            }
            continue;
        }
        break;
    }
    return assigned;
}

uint32_t put_guest_text(uint32_t dst, const std::string &s, uint32_t cap) {
    if (!dst)
        return 0;
    if (dst >= GUEST_SIZE)
        return 0;
    uint32_t limit = GUEST_SIZE - dst;
    if (cap && cap < limit)
        limit = cap;
    uint32_t n = 0;
    while (n + 1 < limit && n < s.size()) {
        g_mem[dst + n] = (uint8_t)s[n];
        ++n;
    }
    if (limit)
        g_mem[dst + n] = 0;
    return n;
}

void crt_sscanf(X86 *c) {
    int n = scan_guest(gm_str(arg(c, 0), 1 << 20), gm_str(arg(c, 1), 4096), c->r[R_ESP] + 12);
    set_eax(c, (uint32_t)n);
}

void crt_sprintf(X86 *c) {
    uint32_t dst = arg(c, 0);
    std::string s = guest_format(arg(c, 1), c->r[R_ESP] + 12);
    put_guest_text(dst, s, GUEST_SIZE);
    set_eax(c, (uint32_t)s.size());
}

void crt_vsprintf(X86 *c) {
    uint32_t dst = arg(c, 0);
    std::string s = guest_format(arg(c, 1), arg(c, 2));
    put_guest_text(dst, s, GUEST_SIZE);
    set_eax(c, (uint32_t)s.size());
}

void crt_snprintf(X86 *c) {
    uint32_t dst = arg(c, 0);
    uint32_t count = arg(c, 1);
    std::string s = guest_format(arg(c, 2), c->r[R_ESP] + 16);
    put_guest_text(dst, s, count);
    set_eax(c, s.size() < count ? (uint32_t)s.size() : 0xffffffffu);
}

void crt_vsnprintf(X86 *c) {
    uint32_t dst = arg(c, 0);
    uint32_t count = arg(c, 1);
    std::string s = guest_format(arg(c, 2), arg(c, 3));
    put_guest_text(dst, s, count);
    set_eax(c, s.size() < count ? (uint32_t)s.size() : 0xffffffffu);
}

double stack_double(X86 *c, int slot) {
    uint32_t p = c->r[R_ESP] + 4 + 4 * (uint32_t)slot;
    uint64_t lo = gm_valid(p, 4) ? rd32(p) : 0;
    uint64_t hi = gm_valid(p + 4, 4) ? rd32(p + 4) : 0;
    uint64_t bits = lo | (hi << 32);
    double d = 0;
    memcpy(&d, &bits, sizeof d);
    return d;
}

std::string cvt_digits(double value, int count, int &decpt, int &sign, bool significant) {
    sign = std::signbit(value) ? 1 : 0;
    decpt = 0;
    std::string mantissa = "0";
    if (std::isfinite(value) && value != 0) {
        char text[832];
        snprintf(text, sizeof text, "%.800e", std::fabs(value));
        char *exp_start = strchr(text, 'e');
        decpt = atoi(exp_start + 1) + 1;
        mantissa.clear();
        for (char *p = text; p != exp_start; ++p)
            if (*p >= '0' && *p <= '9')
                mantissa += *p;
        mantissa.resize(18, '0');
        if (mantissa[17] >= '5') {
            int i = 16;
            while (i >= 0 && mantissa[i] == '9')
                mantissa[i--] = '0';
            if (i >= 0) {
                ++mantissa[i];
            } else {
                mantissa.insert(mantissa.begin(), '1');
                ++decpt;
            }
        }
        mantissa.resize(17);
    }
    int digits = significant ? count : decpt + count;
    digits = std::min(digits, 347);
    if (digits < 0)
        return {};

    std::string out(1, '0');
    for (int i = 0; i < digits; ++i)
        out += i < (int)mantissa.size() ? mantissa[i] : '0';
    char round_digit = digits < (int)mantissa.size() ? mantissa[digits] : '0';
    if (round_digit >= '5') {
        int i = (int)out.size() - 1;
        while (i >= 0 && out[i] == '9') {
            out[i] = '0';
            --i;
        }
        if (i >= 0)
            ++out[i];
    }
    if (out[0] == '1') {
        ++decpt;
    } else {
        out.erase(0, 1);
    }
    if (significant && (int)out.size() > digits)
        out.resize((size_t)digits);
    return out;
}

uint32_t cvt_slot() {
    if (!g_cvt_slot)
        g_cvt_slot = heap_alloc(350, true);
    return g_cvt_slot;
}

void crt_cvt(X86 *c, bool significant) {
    int decpt = 0, sign = 0;
    std::string digits =
        cvt_digits(stack_double(c, 0), (int32_t)arg(c, 2), decpt, sign, significant);
    uint32_t decpt_p = arg(c, 3), sign_p = arg(c, 4);
    if (decpt_p && gm_valid(decpt_p, 4))
        wr32(decpt_p, (uint32_t)decpt);
    if (sign_p && gm_valid(sign_p, 4))
        wr32(sign_p, (uint32_t)sign);
    uint32_t out = cvt_slot();
    put_guest_text(out, digits, GUEST_SIZE);
    set_eax(c, out);
}

void crt_fcvt(X86 *c) {
    crt_cvt(c, false);
}
void crt_ecvt(X86 *c) {
    crt_cvt(c, true);
}

// The compiler intrinsics take and return a double in ST(0), not on the stack.
void crt_ci_log(X86 *c) {
    fset(c, 0, std::log(ST(c, 0)));
}
void crt_ci_cos(X86 *c) {
    fset(c, 0, std::cos(ST(c, 0)));
}
void crt_ci_sin(X86 *c) {
    fset(c, 0, std::sin(ST(c, 0)));
}
void crt_ci_sqrt(X86 *c) {
    fset(c, 0, std::sqrt(ST(c, 0)));
}
void crt_ci_asin(X86 *c) {
    fset(c, 0, std::asin(ST(c, 0)));
}
void crt_ci_acos(X86 *c) {
    fset(c, 0, std::acos(ST(c, 0)));
}
void crt_ci_cosh(X86 *c) {
    fset(c, 0, std::cosh(ST(c, 0)));
}
void crt_ci_sinh(X86 *c) {
    fset(c, 0, std::sinh(ST(c, 0)));
}
void crt_ci_tanh(X86 *c) {
    fset(c, 0, std::tanh(ST(c, 0)));
}

void crt_ci_fmod(X86 *c) {
    double divisor = ST(c, 0), dividend = ST(c, 1);
    fpop(c);
    fset(c, 0, std::fmod(dividend, divisor));
}

void crt_ci_pow(X86 *c) {
    double exponent = ST(c, 0), base = ST(c, 1);
    fpop(c);
    fset(c, 0, std::pow(base, exponent));
}

void crt_floor(X86 *c) {
    fpush(c, std::floor(stack_double(c, 0)));
}
void crt_ceil(X86 *c) {
    fpush(c, std::ceil(stack_double(c, 0)));
}

void crt_ldexp(X86 *c) {
    fpush(c, std::ldexp(stack_double(c, 0), (int32_t)arg(c, 2)));
}

void crt_modf(X86 *c) {
    double integral = 0;
    double frac = std::modf(stack_double(c, 0), &integral);
    uint32_t out = arg(c, 2);
    if (gm_valid(out, 8)) {
        uint64_t bits;
        memcpy(&bits, &integral, sizeof bits);
        wr32(out, (uint32_t)bits);
        wr32(out + 4, (uint32_t)(bits >> 32));
    }
    fpush(c, frac);
}

void crt_isnan(X86 *c) {
    set_eax(c, std::isnan(stack_double(c, 0)) ? 1 : 0);
}
void crt_finite(X86 *c) {
    set_eax(c, std::isfinite(stack_double(c, 0)) ? 1 : 0);
}

void crt_isupper(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, ch >= 'A' && ch <= 'Z');
}
void crt_islower(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, ch >= 'a' && ch <= 'z');
}
void crt_isdigit(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, ch >= '0' && ch <= '9');
}
void crt_isspace(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' || ch == '\f' || ch == '\v');
}
void crt_isalpha(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, (ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z'));
}
void crt_isalnum(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, (ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z') || (ch >= '0' && ch <= '9'));
}
void crt_isprint(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, ch >= 0x20 && ch <= 0x7e);
}
void crt_isxdigit(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, (ch >= '0' && ch <= '9') || (ch >= 'A' && ch <= 'F') || (ch >= 'a' && ch <= 'f'));
}
void crt_tolower(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, ch >= 'A' && ch <= 'Z' ? (uint32_t)(ch + 32) : (uint32_t)ch);
}
void crt_toupper(X86 *c) {
    int ch = ascii_class((int)arg(c, 0));
    set_eax(c, ch >= 'a' && ch <= 'z' ? (uint32_t)(ch - 32) : (uint32_t)ch);
}
void crt_towlower(X86 *c) {
    uint32_t ch = arg(c, 0);
    set_eax(c, ch >= 'A' && ch <= 'Z' ? ch + 32 : ch);
}
void crt_towupper(X86 *c) {
    uint32_t ch = arg(c, 0);
    set_eax(c, ch >= 'a' && ch <= 'z' ? ch - 32 : ch);
}
// MSVCRT's _isctype/_iswctype class bits, for the C locale. The wide routine
// still only classifies what the narrow one does; the table's high half is
// empty outside a thread locale, which this CRT does not switch.
uint32_t crt_wctype_class(uint32_t ch) {
    if (ch > 0xff)
        return 0;
    uint32_t bits = 0;
    if (ch >= 'A' && ch <= 'Z')
        bits |= 0x001 | 0x100; // _UPPER | _ALPHA
    if (ch >= 'a' && ch <= 'z')
        bits |= 0x002 | 0x100; // _LOWER | _ALPHA
    if (ch >= '0' && ch <= '9')
        bits |= 0x004; // _DIGIT
    if (ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' || ch == '\f' || ch == '\v')
        bits |= 0x008; // _SPACE
    if (ch == ' ' || ch == '\t')
        bits |= 0x040; // _BLANK
    if (ch < 0x20 || ch == 0x7f)
        bits |= 0x020; // _CONTROL
    if (ch > 0x20 && ch < 0x7f && !(bits & (0x004 | 0x100)))
        bits |= 0x010; // _PUNCT
    if ((ch >= '0' && ch <= '9') || (ch >= 'A' && ch <= 'F') || (ch >= 'a' && ch <= 'f'))
        bits |= 0x080; // _HEX
    return bits;
}
void crt_iswctype(X86 *c) {
    set_eax(c, crt_wctype_class(arg(c, 0)) & arg(c, 1));
}

uint32_t g_iob_addr;
constexpr uint32_t kCrtFileSize = 0x20;
void crt_bind_iob(uint32_t base);

void crt_strlen(X86 *c) {
    set_eax(c, (uint32_t)strlen(guest_cstr(arg(c, 0))));
}

void crt_setlocale(X86 *c) {
    static uint32_t cached;
    if (!cached)
        cached = dup_guest("C");
    set_eax(c, cached);
}

void crt_iob_func(X86 *c) {
    if (!g_iob_addr)
        g_iob_addr = heap_alloc(3 * kCrtFileSize, true);
    crt_bind_iob(g_iob_addr);
    set_eax(c, g_iob_addr);
}

void crt_pctype_func(X86 *c) {
    static uint32_t table;
    if (!table) {
        table = heap_alloc(257 * 2, true) + 2;
        for (uint32_t ch = 0; ch < 256; ++ch)
            wr16(table + ch * 2, (uint16_t)crt_wctype_class(ch));
    }
    set_eax(c, table);
}

void crt_return_zero(X86 *c) {
    set_eax(c, 0);
}

// ---------------------------------------------------------------------------
// CRT file handles. Guest FILE* values are opaque guest addresses; the host
// stdio object lives here and never crosses the guest boundary.
// ---------------------------------------------------------------------------
struct CrtFile {
    FILE *fp;
    bool owned;
};
std::map<uint32_t, CrtFile> g_crt_files;
thread_local uint32_t g_strtok_next;

FILE *crt_file(uint32_t handle) {
    auto it = g_crt_files.find(handle);
    return it == g_crt_files.end() ? nullptr : it->second.fp;
}

uint32_t crt_add_file(FILE *fp, bool owned) {
    if (!fp)
        return 0;
    uint32_t handle = heap_alloc(kCrtFileSize, true);
    if (!handle)
        return 0;
    g_crt_files[handle] = {fp, owned};
    return handle;
}

void crt_bind_iob(uint32_t base) {
    if (!base)
        return;
    g_crt_files[base] = {stdin, false};
    g_crt_files[base + kCrtFileSize] = {stdout, false};
    g_crt_files[base + 2 * kCrtFileSize] = {stderr, false};
}

bool crt_guest_file(uint32_t guest, bool write, std::string &host) {
    host = win32_host_path_op(gm_str(guest, 4096), write ? WIN32_FILE_WRITE : WIN32_FILE_READ);
    return !host.empty();
}

void crt_set_errno(int value) {
    wr32(guest_slot(g_errno_addr), (uint32_t)value);
}

void crt_fopen(X86 *c) {
    uint32_t path = arg(c, 0), mode = arg(c, 1);
    std::string name = gm_str(path, 4096), flags = gm_str(mode, 32);
    bool write = flags.find_first_of("wa+") != std::string::npos;
    std::string host;
    if (name.empty() || !crt_guest_file(path, write, host)) {
        crt_set_errno(ENOENT);
        set_eax(c, 0);
        return;
    }
    int oflags = OS_O_RDONLY;
    if (flags.find('+') != std::string::npos)
        oflags = OS_O_RDWR;
    else if (write)
        oflags = OS_O_WRONLY;
    if (flags.find('w') != std::string::npos)
        oflags |= OS_O_CREAT | OS_O_TRUNC;
    if (flags.find('a') != std::string::npos)
        oflags |= OS_O_CREAT;
    int fd = os_fd_open(host.c_str(), oflags);
    if (fd < 0) {
        crt_set_errno(ENOENT);
        set_eax(c, 0);
        return;
    }
    std::string host_mode(1, flags[0]);
    if (flags.find('+') != std::string::npos)
        host_mode += '+';
    FILE *fp = (FILE *)os_fdopen(fd, (host_mode + 'b').c_str());
    if (!fp) {
        os_fd_close(fd);
        crt_set_errno(EINVAL);
        set_eax(c, 0);
        return;
    }
    set_eax(c, crt_add_file(fp, true));
}

void crt_fclose(X86 *c) {
    uint32_t handle = arg(c, 0);
    auto it = g_crt_files.find(handle);
    if (it == g_crt_files.end()) {
        crt_set_errno(EBADF);
        set_eax(c, 0xffffffffu);
        return;
    }
    int result = it->second.owned ? fclose(it->second.fp) : 0;
    g_crt_files.erase(it);
    set_eax(c, result == 0 ? 0u : 0xffffffffu);
}

void crt_fread(X86 *c) {
    uint32_t dst = arg(c, 0), size = arg(c, 1), count = arg(c, 2);
    FILE *fp = crt_file(arg(c, 3));
    const uint64_t bytes = (uint64_t)size * count;
    if (!fp || !size || !count || bytes > 0xffffffffu || !gm_valid(dst, (uint32_t)bytes)) {
        set_eax(c, 0);
        return;
    }
    set_eax(c, (uint32_t)fread(g_mem + dst, size, count, fp));
}

void crt_fwrite(X86 *c) {
    uint32_t src = arg(c, 0), size = arg(c, 1), count = arg(c, 2);
    FILE *fp = crt_file(arg(c, 3));
    const uint64_t bytes = (uint64_t)size * count;
    if (!fp || !size || !count || bytes > 0xffffffffu || !gm_valid(src, (uint32_t)bytes)) {
        set_eax(c, 0);
        return;
    }
    set_eax(c, (uint32_t)fwrite(g_mem + src, size, count, fp));
}

void crt_fseek(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    set_eax(c, fp && fseek(fp, (long)(int32_t)arg(c, 1), (int)arg(c, 2)) == 0 ? 0u : 0xffffffffu);
}

void crt_ftell(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    set_eax(c, fp ? (uint32_t)ftell(fp) : 0xffffffffu);
}

void crt_fflush(X86 *c) {
    uint32_t handle = arg(c, 0);
    if (!handle) {
        set_eax(c, (uint32_t)fflush(nullptr));
        return;
    }
    FILE *fp = crt_file(handle);
    set_eax(c, fp && fflush(fp) == 0 ? 0u : 0xffffffffu);
}

void crt_fgetc(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    set_eax(c, fp ? (uint32_t)fgetc(fp) : 0xffffffffu);
}

void crt_fputc(X86 *c) {
    FILE *fp = crt_file(arg(c, 1));
    set_eax(c, fp ? (uint32_t)fputc((int)(arg(c, 0) & 0xff), fp) : 0xffffffffu);
}

void crt_ungetc(X86 *c) {
    FILE *fp = crt_file(arg(c, 1));
    set_eax(c, fp ? (uint32_t)ungetc((int)(arg(c, 0) & 0xff), fp) : 0xffffffffu);
}

void crt_feof(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    set_eax(c, fp ? (uint32_t)feof(fp) : 1u);
}

void crt_ferror(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    set_eax(c, fp ? (uint32_t)ferror(fp) : 1u);
}

void crt_clearerr(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    if (fp)
        clearerr(fp);
    set_eax(c, 0);
}

void crt_printf(X86 *c) {
    std::string text = guest_format(arg(c, 0), c->r[R_ESP] + 8);
    fwrite(text.data(), 1, text.size(), stdout);
    set_eax(c, (uint32_t)text.size());
}

void crt_fprintf(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    std::string text = guest_format(arg(c, 1), c->r[R_ESP] + 12);
    if (fp)
        fwrite(text.data(), 1, text.size(), fp);
    set_eax(c, (uint32_t)text.size());
}

void crt_vfprintf(X86 *c) {
    FILE *fp = crt_file(arg(c, 0));
    std::string text = guest_format(arg(c, 1), arg(c, 2));
    if (fp)
        fwrite(text.data(), 1, text.size(), fp);
    set_eax(c, (uint32_t)text.size());
}

void crt_ftime(X86 *c) {
    uint32_t out = arg(c, 0);
    if (!out || !gm_valid(out, 12))
        return;
    auto since_epoch = std::chrono::system_clock::now().time_since_epoch();
    auto millis = std::chrono::duration_cast<std::chrono::milliseconds>(since_epoch).count();
    std::time_t now = (std::time_t)(millis / 1000);
    std::tm local = *std::localtime(&now);
    std::tm utc = *std::gmtime(&now);
    long diff = (long)(std::mktime(&local) - std::mktime(&utc)) / 60;
    wr32(out, (uint32_t)now);
    wr16(out + 4, (uint16_t)(millis % 1000));
    wr16(out + 6, (uint16_t)(-diff));
    wr16(out + 8, (uint16_t)(local.tm_isdst > 0 ? 1 : 0));
    wr16(out + 10, 0);
}

void crt_fullpath(X86 *c) {
    uint32_t out = arg(c, 0), rel = arg(c, 1), cap = arg(c, 2);
    std::string full = full_path_named(gm_str(rel, 4096));
    if (!out) {
        out = heap_alloc(cap ? cap : 260, true);
        cap = cap ? cap : 260;
    }
    if (!out || !cap) {
        set_eax(c, 0);
        return;
    }
    put_guest_text(out, full, cap);
    set_eax(c, out);
}

void crt_findfirst(X86 *c) {
    uint32_t h = win32_crt_findfirst(gm_str(arg(c, 0), 4096), arg(c, 1));
    if (h == 0xffffffffu)
        crt_set_errno(ENOENT);
    set_eax(c, h);
}

void crt_findnext(X86 *c) {
    uint32_t handle = arg(c, 0), data = arg(c, 1);
    if (!win32_crt_findnext(handle, data)) {
        crt_set_errno(ENOENT);
        set_eax(c, 0xffffffffu);
        return;
    }
    set_eax(c, 0);
}

void crt_findclose(X86 *c) {
    set_eax(c, win32_crt_findclose(arg(c, 0)) ? 0u : 0xffffffffu);
}

void crt_open(X86 *c) {
    std::string host;
    uint32_t flags = arg(c, 1);
    if (!crt_guest_file(arg(c, 0), (flags & 3) != 0, host)) {
        crt_set_errno(ENOENT);
        set_eax(c, 0xffffffffu);
        return;
    }
    int oflags = OS_O_RDONLY;
    if ((flags & 3) == 1)
        oflags = OS_O_WRONLY;
    else if ((flags & 3) == 2)
        oflags = OS_O_RDWR;
    if (flags & 0x0100)
        oflags |= OS_O_CREAT;
    if (flags & 0x0200)
        oflags |= OS_O_TRUNC;
    if (flags & 0x0400)
        oflags |= OS_O_EXCL;
    int fd = os_fd_open(host.c_str(), oflags);
    if (fd < 0)
        crt_set_errno(ENOENT);
    set_eax(c, fd < 0 ? 0xffffffffu : (uint32_t)fd);
}

void crt_read(X86 *c) {
    int fd = (int)arg(c, 0);
    uint32_t dst = arg(c, 1), count = arg(c, 2);
    int64_t n = !count ? 0 : gm_valid(dst, count) ? os_fd_read(fd, g_mem + dst, count) : -1;
    set_eax(c, n < 0 ? 0xffffffffu : (uint32_t)n);
}

void crt_write(X86 *c) {
    int fd = (int)arg(c, 0);
    uint32_t src = arg(c, 1), count = arg(c, 2);
    int64_t n = !count ? 0 : gm_valid(src, count) ? os_fd_write(fd, g_mem + src, count) : -1;
    set_eax(c, n < 0 ? 0xffffffffu : (uint32_t)n);
}

void crt_lseek(X86 *c) {
    int64_t off = os_fd_seek((int)arg(c, 0), (int32_t)arg(c, 1), (int)arg(c, 2));
    set_eax(c, off < 0 ? 0xffffffffu : (uint32_t)off);
}

void crt_close(X86 *c) {
    set_eax(c, os_fd_close((int)arg(c, 0)) == 0 ? 0u : 0xffffffffu);
}

void crt_filelength(X86 *c) {
    OsStat st{};
    int fd = (int)arg(c, 0);
    if (fd < 0 || os_fd_stat(fd, &st) != 0) {
        crt_set_errno(EBADF);
        set_eax(c, 0xffffffffu);
        return;
    }
    set_eax(c, (uint32_t)st.size);
}

void crt_remove(X86 *c) {
    std::string host;
    if (!crt_guest_file(arg(c, 0), true, host) || os_unlink(host.c_str()) != 0) {
        crt_set_errno(ENOENT);
        set_eax(c, 0xffffffffu);
        return;
    }
    set_eax(c, 0);
}

void crt_mkdir(X86 *c) {
    std::string host;
    if (!crt_guest_file(arg(c, 0), true, host) || os_mkdir(host.c_str()) != 0) {
        crt_set_errno(EEXIST);
        set_eax(c, 0xffffffffu);
        return;
    }
    set_eax(c, 0);
}

void crt_rmdir(X86 *c) {
    std::string host;
    if (!crt_guest_file(arg(c, 0), true, host) || os_rmdir(host.c_str()) != 0) {
        crt_set_errno(ENOENT);
        set_eax(c, 0xffffffffu);
        return;
    }
    set_eax(c, 0);
}

void crt_getdiskfree(X86 *c) {
    uint32_t out = arg(c, 1);
    uint64_t free_bytes = 0;
    std::string host = win32_host_path_op(".", WIN32_FILE_LIST);
    if (!host.empty())
        os_free_space(host.c_str(), &free_bytes);
    if (out && gm_valid(out, 16)) {
        wr32(out + 0, (uint32_t)(free_bytes / (8 * 512)));
        wr32(out + 4, (uint32_t)(free_bytes / (8 * 512)));
        wr32(out + 8, 8);
        wr32(out + 12, 512);
    }
    set_eax(c, 0);
}

void crt_getcwd_a(X86 *c) {
    uint32_t out = arg(c, 0), cap = arg(c, 1);
    std::string path = current_directory();
    if (!out || !cap || path.size() + 1 > cap) {
        crt_set_errno(ERANGE);
        set_eax(c, 0);
        return;
    }
    put_guest_text(out, path, cap);
    set_eax(c, out);
}

void crt_localtime32(X86 *c) {
    uint32_t p = arg(c, 0);
    if (!p || !gm_valid(p, 4)) {
        set_eax(c, 0);
        return;
    }
    std::time_t value = (std::time_t)(int32_t)rd32(p);
    std::tm *tm = std::localtime(&value);
    if (!tm) {
        set_eax(c, 0);
        return;
    }
    uint32_t out = tm_slot();
    write_tm(out, *tm);
    set_eax(c, out);
}

void crt_gmtime32(X86 *c) {
    uint32_t p = arg(c, 0);
    if (!p || !gm_valid(p, 4)) {
        set_eax(c, 0);
        return;
    }
    std::time_t value = (std::time_t)(int32_t)rd32(p);
    std::tm *tm = std::gmtime(&value);
    if (!tm) {
        set_eax(c, 0);
        return;
    }
    uint32_t out = tm_slot();
    write_tm(out, *tm);
    set_eax(c, out);
}

void crt_mktime32(X86 *c) {
    uint32_t p = arg(c, 0);
    std::tm tm;
    if (!read_tm(p, tm)) {
        set_eax(c, 0xffffffffu);
        return;
    }
    std::time_t value = std::mktime(&tm);
    if (value == (std::time_t)-1) {
        set_eax(c, 0xffffffffu);
        return;
    }
    write_tm(p, tm);
    set_eax(c, (uint32_t)value);
}

void crt_clock(X86 *c) {
    set_eax(c, host_millis());
}

void crt_srand(X86 *c) {
    g_rand_state = arg(c, 0);
    set_eax(c, 0);
}

void crt_calloc(X86 *c) {
    uint32_t count = arg(c, 0), size = arg(c, 1);
    uint64_t total = (uint64_t)count * size;
    uint32_t p = total && total <= 0xffffffffu ? heap_alloc((uint32_t)total, true) : 0;
    set_eax(c, p);
}

void crt_memcmp(X86 *c) {
    uint32_t a = arg(c, 0), b = arg(c, 1), n = arg(c, 2);
    int r = 0;
    if (n && gm_valid(a, n) && gm_valid(b, n))
        r = memcmp(g_mem + a, g_mem + b, n);
    set_eax(c, (uint32_t)r);
}

void crt_strcoll(X86 *c) {
    set_eax(c, (uint32_t)strcmp(gm_str(arg(c, 0), 1 << 20).c_str(),
                                gm_str(arg(c, 1), 1 << 20).c_str()));
}

void crt_strerror(X86 *c) {
    static thread_local uint32_t slot;
    if (!slot)
        slot = heap_alloc(128, true);
    put_guest_text(slot, std::strerror((int)arg(c, 0)), 128);
    set_eax(c, slot);
}

void crt_strtok(X86 *c) {
    uint32_t p = arg(c, 0) ? arg(c, 0) : g_strtok_next;
    std::string delim = gm_str(arg(c, 1), 256);
    if (!p || !gm_valid(p, 1)) {
        g_strtok_next = 0;
        set_eax(c, 0);
        return;
    }
    while (gm_valid(p, 1) && g_mem[p] && delim.find((char)g_mem[p]) != std::string::npos)
        ++p;
    if (!gm_valid(p, 1) || !g_mem[p]) {
        g_strtok_next = 0;
        set_eax(c, 0);
        return;
    }
    uint32_t start = p;
    while (gm_valid(p, 1) && g_mem[p] && delim.find((char)g_mem[p]) == std::string::npos)
        ++p;
    if (gm_valid(p, 1) && g_mem[p]) {
        g_mem[p] = 0;
        g_strtok_next = p + 1;
    } else {
        g_strtok_next = 0;
    }
    set_eax(c, start);
}

void crt_bsearch(X86 *c) {
    uint32_t key = arg(c, 0), base = arg(c, 1), count = arg(c, 2), width = arg(c, 3),
             compare = arg(c, 4);
    uint32_t lo = 0, hi = count;
    while (lo < hi && width) {
        uint32_t mid = lo + (hi - lo) / 2;
        uint32_t item = base + mid * width;
        int32_t order = (int32_t)guest_call(c, compare, key, item);
        if (!order) {
            set_eax(c, item);
            return;
        }
        if (order > 0)
            lo = mid + 1;
        else
            hi = mid;
    }
    set_eax(c, 0);
}

void crt_wcslen(X86 *c) {
    uint32_t p = arg(c, 0), n = 0;
    while (gm_valid(p + n * 2, 2) && rd16(p + n * 2))
        ++n;
    set_eax(c, n);
}

void crt_wcscpy(X86 *c) {
    uint32_t dst = arg(c, 0), src = arg(c, 1), i = 0;
    while (gm_valid(src + i * 2, 2) && gm_valid(dst + i * 2, 2)) {
        uint16_t ch = rd16(src + i * 2);
        wr16(dst + i * 2, ch);
        ++i;
        if (!ch)
            break;
    }
    set_eax(c, dst);
}

void crt_math_sqrt(X86 *c) {
    fpush(c, std::sqrt(stack_double(c, 0)));
}

void crt_math_cos(X86 *c) {
    fpush(c, std::cos(stack_double(c, 0)));
}

void crt_abs(X86 *c) {
    int32_t v = (int32_t)arg(c, 0);
    set_eax(c, (uint32_t)(v < 0 ? -v : v));
}

void crt_raise(X86 *c) {
    uint32_t sig = arg(c, 0);
    if (sig < 32 && g_signal_handlers[sig]) {
        uint32_t handler = g_signal_handlers[sig];
        if (handler != 1)
            guest_call(c, handler, sig);
    }
    set_eax(c, 0);
}

const ImportShim g_msvcr90_shims[] = {
    {"MSVCR90.dll", "strlen", ARGC_CDECL, crt_strlen},
    {"MSVCR90.dll", "setlocale", ARGC_CDECL, crt_setlocale},
    {"MSVCR90.dll", "__iob_func", ARGC_CDECL, crt_iob_func},
    {"MSVCR90.dll", "__pctype_func", ARGC_CDECL, crt_pctype_func},
    {"MSVCR90.dll", "___lc_handle_func", ARGC_CDECL, crt_return_zero},
    {"MSVCR90.dll", "___lc_codepage_func", ARGC_CDECL, crt_return_zero},
    {"MSVCR90.dll", "___setlc_active_func", ARGC_CDECL, crt_return_zero},
    {"MSVCR90.dll", "___unguarded_readlc_active_add_func", ARGC_CDECL, crt_return_zero},
    {"MSVCR90.dll", "_initterm_e", ARGC_CDECL, crt_initterm_e},
    {"MSVCR90.dll", "_initterm", ARGC_CDECL, crt_initterm},
    {"MSVCR90.dll", "_ismbblead", ARGC_CDECL, crt_ismbblead},
    {"MSVCR90.dll", "_setmbcp", ARGC_CDECL, crt_setmbcp},
    {"MSVCR90.dll", "__set_app_type", ARGC_CDECL, crt_set_app_type},
    {"MSVCR90.dll", "__setusermatherr", ARGC_CDECL, crt_setusermatherr},
    {"MSVCR90.dll", "_adjust_fdiv", ARGC_CDECL, crt_adjust_fdiv},
    {"MSVCR90.dll", "_configthreadlocale", ARGC_CDECL, crt_configthreadlocale},
    {"MSVCR90.dll", "_encode_pointer", ARGC_CDECL, crt_encode_pointer},
    {"MSVCR90.dll", "_decode_pointer", ARGC_CDECL, crt_decode_pointer},
    {"MSVCR90.dll", "__p__fmode", ARGC_CDECL, crt_p_fmode},
    {"MSVCR90.dll", "__p__commode", ARGC_CDECL, crt_p_commode},
    {"MSVCR90.dll", "__p___initenv", ARGC_CDECL, crt_p_initenv},
    {"MSVCR90.dll", "_errno", ARGC_CDECL, crt_errno},
    {"MSVCR90.dll", "_controlfp_s", ARGC_CDECL, crt_controlfp_s},
    {"MSVCR90.dll", "_controlfp", ARGC_CDECL, crt_controlfp},
    {"MSVCR90.dll", "_control87", ARGC_CDECL, crt_controlfp},
    {"MSVCR90.dll", "exit", ARGC_CDECL, crt_exit},
    {"MSVCR90.dll", "_exit", ARGC_CDECL, crt_exit_fast},
    {"MSVCR90.dll", "_cexit", ARGC_CDECL, crt_cexit},
    {"MSVCR90.dll", "_amsg_exit", ARGC_CDECL, crt_amsg_exit},
    {"MSVCR90.dll", "_invoke_watson", ARGC_CDECL, crt_invoke_watson},
    {"MSVCR90.dll", "_crt_debugger_hook", ARGC_CDECL, crt_debugger_hook},
    {"MSVCR90.dll", "?terminate@@YAXXZ", ARGC_CDECL, crt_terminate},
    {"MSVCR90.dll", "abort", ARGC_CDECL, crt_abort},
    {"MSVCR90.dll", "longjmp", ARGC_CDECL, crt_longjmp},
    {"MSVCR90.dll", "_purecall", ARGC_CDECL, crt_purecall},
    {"MSVCR90.dll", "_onexit", ARGC_CDECL, crt_onexit},
    {"MSVCR90.dll", "__dllonexit", ARGC_CDECL, crt_dllonexit},
    {"MSVCR90.dll", "_lock", ARGC_CDECL, crt_lock},
    {"MSVCR90.dll", "_unlock", ARGC_CDECL, crt_unlock},
    {"MSVCR90.dll", "_beginthreadex", ARGC_CDECL, crt_beginthreadex},
    {"MSVCR90.dll", "_XcptFilter", ARGC_CDECL, crt_xcpt_filter},
    {"MSVCR90.dll", "signal", ARGC_CDECL, crt_signal},
    {"MSVCR90.dll", "rand", ARGC_CDECL, crt_rand},
    {"MSVCR90.dll", "_stat64i32", ARGC_CDECL, crt_stat64i32<false>},
    {"MSVCR90.dll", "_wstat64i32", ARGC_CDECL, crt_stat64i32<true>},
    {"MSVCR90.dll", "_time32", ARGC_CDECL, crt_time32},
    {"MSVCR90.dll", "_time64", ARGC_CDECL, crt_time64},
    {"MSVCR90.dll", "_localtime64", ARGC_CDECL, crt_localtime64},
    {"MSVCR90.dll", "_gmtime64", ARGC_CDECL, crt_gmtime64},
    {"MSVCR90.dll", "_mktime64", ARGC_CDECL, crt_mktime64},
    {"MSVCR90.dll", "strftime", ARGC_CDECL, crt_strftime},
    {"MSVCR90.dll", "__getmainargs", ARGC_CDECL, crt_getmainargs},
    {"MSVCR90.dll", "malloc", ARGC_CDECL, crt_malloc},
    {"MSVCR90.dll", "free", ARGC_CDECL, crt_free},
    {"MSVCR90.dll", "realloc", ARGC_CDECL, crt_realloc},
    {"MSVCR90.dll", "_wgetcwd", ARGC_CDECL, crt_wgetcwd},
    {"MSVCR90.dll", "wgetcwd", ARGC_CDECL, crt_wgetcwd},
    {"MSVCR90.dll", "_wgetdcwd", ARGC_CDECL, crt_wgetdcwd},
    {"MSVCR90.dll", "_getdrive", ARGC_CDECL, crt_getdrive},
    {"MSVCR90.dll", "getenv", ARGC_CDECL, crt_getenv},
    {"MSVCR90.dll", "_aligned_malloc", ARGC_CDECL, crt_aligned_malloc},
    {"MSVCR90.dll", "_aligned_free", ARGC_CDECL, crt_aligned_free},
    {"MSVCR90.dll", "_aligned_realloc", ARGC_CDECL, crt_aligned_realloc},
    {"MSVCR90.dll", "memcpy", ARGC_CDECL, crt_memcpy},
    {"MSVCR90.dll", "memmove", ARGC_CDECL, crt_memmove},
    {"MSVCR90.dll", "memset", ARGC_CDECL, crt_memset},
    {"MSVCR90.dll", "memchr", ARGC_CDECL, crt_memchr},
    {"MSVCR90.dll", "qsort", ARGC_CDECL, crt_qsort},
    {"MSVCR90.dll", "strcmp", ARGC_CDECL, crt_strcmp},
    {"MSVCR90.dll", "strncmp", ARGC_CDECL, crt_strncmp},
    {"MSVCR90.dll", "_stricmp", ARGC_CDECL, crt_stricmp},
    {"MSVCR90.dll", "_strcmpi", ARGC_CDECL, crt_stricmp},
    {"MSVCR90.dll", "_strnicmp", ARGC_CDECL, crt_strnicmp},
    {"MSVCR90.dll", "strncpy", ARGC_CDECL, crt_strncpy},
    {"MSVCR90.dll", "strcpy", ARGC_CDECL, crt_strcpy},
    {"MSVCR90.dll", "_mbscpy", ARGC_CDECL, crt_strcpy},
    {"MSVCR90.dll", "strcat", ARGC_CDECL, crt_strcat},
    {"MSVCR90.dll", "_mbscat", ARGC_CDECL, crt_strcat},
    {"MSVCR90.dll", "strncat", ARGC_CDECL, crt_strncat},
    {"MSVCR90.dll", "strchr", ARGC_CDECL, crt_strchr},
    {"MSVCR90.dll", "strrchr", ARGC_CDECL, crt_strrchr},
    {"MSVCR90.dll", "strstr", ARGC_CDECL, crt_strstr},
    {"MSVCR90.dll", "strpbrk", ARGC_CDECL, crt_strpbrk},
    {"MSVCR90.dll", "strspn", ARGC_CDECL, crt_strspn},
    {"MSVCR90.dll", "strcspn", ARGC_CDECL, crt_strcspn},
    {"MSVCR90.dll", "_strlwr", ARGC_CDECL, crt_strlwr},
    {"MSVCR90.dll", "_wcslwr", ARGC_CDECL, crt_wcslwr},
    {"MSVCR90.dll", "wcsncpy", ARGC_CDECL, crt_wcsncpy},
    {"MSVCR90.dll", "wcsncat", ARGC_CDECL, crt_wcsncat},
    {"MSVCR90.dll", "wcscspn", ARGC_CDECL, crt_wcscspn},
    {"MSVCR90.dll", "wcschr", ARGC_CDECL, crt_wcschr},
    {"MSVCR90.dll", "wcsrchr", ARGC_CDECL, crt_wcsrchr},
    {"MSVCR90.dll", "wcsstr", ARGC_CDECL, crt_wcsstr},
    {"MSVCR90.dll", "wcsncmp", ARGC_CDECL, crt_wcsncmp},
    {"MSVCR90.dll", "_wcsicmp", ARGC_CDECL, crt_wcsicmp},
    {"MSVCR90.dll", "_wcsnicmp", ARGC_CDECL, crt_wcsnicmp},
    {"MSVCR90.dll", "wcstol", ARGC_CDECL, crt_wcstol},
    {"MSVCR90.dll", "wcstoul", ARGC_CDECL, crt_wcstoul},
    {"MSVCR90.dll", "wcstod", ARGC_CDECL, crt_wcstod},
    {"MSVCR90.dll", "_wtol", ARGC_CDECL, crt_wtol},
    {"MSVCR90.dll", "_ltow", ARGC_CDECL, crt_ltow},
    {"MSVCR90.dll", "_ultow", ARGC_CDECL, crt_ultow},
    {"MSVCR90.dll", "strtol", ARGC_CDECL, crt_strtol},
    {"MSVCR90.dll", "strtoul", ARGC_CDECL, crt_strtoul},
    {"MSVCR90.dll", "_strtoui64", ARGC_CDECL, crt_strtoui64},
    {"MSVCR90.dll", "atof", ARGC_CDECL, crt_atof},
    {"MSVCR90.dll", "strtod", ARGC_CDECL, crt_strtod},
    {"MSVCR90.dll", "_fcvt", ARGC_CDECL, crt_fcvt},
    {"MSVCR90.dll", "_ecvt", ARGC_CDECL, crt_ecvt},
    {"MSVCR90.dll", "_ltoa", ARGC_CDECL, crt_ltoa},
    {"MSVCR90.dll", "_ultoa", ARGC_CDECL, crt_ultoa},
    {"MSVCR90.dll", "atoi", ARGC_CDECL, crt_atoi},
    {"MSVCR90.dll", "atol", ARGC_CDECL, crt_atoi},
    {"MSVCR90.dll", "sscanf", ARGC_CDECL, crt_sscanf},
    {"MSVCR90.dll", "sprintf", ARGC_CDECL, crt_sprintf},
    {"MSVCR90.dll", "vsprintf", ARGC_CDECL, crt_vsprintf},
    {"MSVCR90.dll", "_snprintf", ARGC_CDECL, crt_snprintf},
    {"MSVCR90.dll", "_vsnprintf", ARGC_CDECL, crt_vsnprintf},
    {"MSVCR90.dll", "printf", ARGC_CDECL, crt_printf},
    {"MSVCR90.dll", "fprintf", ARGC_CDECL, crt_fprintf},
    {"MSVCR90.dll", "vfprintf", ARGC_CDECL, crt_vfprintf},
    {"MSVCR90.dll", "fopen", ARGC_CDECL, crt_fopen},
    {"MSVCR90.dll", "fclose", ARGC_CDECL, crt_fclose},
    {"MSVCR90.dll", "fread", ARGC_CDECL, crt_fread},
    {"MSVCR90.dll", "fwrite", ARGC_CDECL, crt_fwrite},
    {"MSVCR90.dll", "fseek", ARGC_CDECL, crt_fseek},
    {"MSVCR90.dll", "ftell", ARGC_CDECL, crt_ftell},
    {"MSVCR90.dll", "fflush", ARGC_CDECL, crt_fflush},
    {"MSVCR90.dll", "fgetc", ARGC_CDECL, crt_fgetc},
    {"MSVCR90.dll", "fputc", ARGC_CDECL, crt_fputc},
    {"MSVCR90.dll", "ungetc", ARGC_CDECL, crt_ungetc},
    {"MSVCR90.dll", "feof", ARGC_CDECL, crt_feof},
    {"MSVCR90.dll", "ferror", ARGC_CDECL, crt_ferror},
    {"MSVCR90.dll", "clearerr", ARGC_CDECL, crt_clearerr},
    {"MSVCR90.dll", "_ftime", ARGC_CDECL, crt_ftime},
    {"MSVCR90.dll", "_fullpath", ARGC_CDECL, crt_fullpath},
    {"MSVCR90.dll", "_findfirst", ARGC_CDECL, crt_findfirst},
    {"MSVCR90.dll", "_findnext", ARGC_CDECL, crt_findnext},
    {"MSVCR90.dll", "_findclose", ARGC_CDECL, crt_findclose},
    {"MSVCR90.dll", "_open", ARGC_CDECL, crt_open},
    {"MSVCR90.dll", "_read", ARGC_CDECL, crt_read},
    {"MSVCR90.dll", "_write", ARGC_CDECL, crt_write},
    {"MSVCR90.dll", "_lseek", ARGC_CDECL, crt_lseek},
    {"MSVCR90.dll", "_close", ARGC_CDECL, crt_close},
    {"MSVCR90.dll", "_filelength", ARGC_CDECL, crt_filelength},
    {"MSVCR90.dll", "remove", ARGC_CDECL, crt_remove},
    {"MSVCR90.dll", "_mkdir", ARGC_CDECL, crt_mkdir},
    {"MSVCR90.dll", "_rmdir", ARGC_CDECL, crt_rmdir},
    {"MSVCR90.dll", "_getdiskfree", ARGC_CDECL, crt_getdiskfree},
    {"MSVCR90.dll", "_getcwd", ARGC_CDECL, crt_getcwd_a},
    {"MSVCR90.dll", "time", ARGC_CDECL, crt_time32},
    {"MSVCR90.dll", "localtime", ARGC_CDECL, crt_localtime32},
    {"MSVCR90.dll", "gmtime", ARGC_CDECL, crt_gmtime32},
    {"MSVCR90.dll", "mktime", ARGC_CDECL, crt_mktime32},
    {"MSVCR90.dll", "clock", ARGC_CDECL, crt_clock},
    {"MSVCR90.dll", "srand", ARGC_CDECL, crt_srand},
    {"MSVCR90.dll", "calloc", ARGC_CDECL, crt_calloc},
    {"MSVCR90.dll", "memcmp", ARGC_CDECL, crt_memcmp},
    {"MSVCR90.dll", "strcoll", ARGC_CDECL, crt_strcoll},
    {"MSVCR90.dll", "strerror", ARGC_CDECL, crt_strerror},
    {"MSVCR90.dll", "strtok", ARGC_CDECL, crt_strtok},
    {"MSVCR90.dll", "bsearch", ARGC_CDECL, crt_bsearch},
    {"MSVCR90.dll", "wcslen", ARGC_CDECL, crt_wcslen},
    {"MSVCR90.dll", "wcscpy", ARGC_CDECL, crt_wcscpy},
    {"MSVCR90.dll", "sqrt", ARGC_CDECL, crt_math_sqrt},
    {"MSVCR90.dll", "cos", ARGC_CDECL, crt_math_cos},
    {"MSVCR90.dll", "abs", ARGC_CDECL, crt_abs},
    {"MSVCR90.dll", "raise", ARGC_CDECL, crt_raise},
    {"MSVCR90.dll", "_endthreadex", ARGC_CDECL, kernel32_exit_thread},
    {"MSVCR90.dll", "_CIcosh", ARGC_CDECL, crt_ci_cosh},
    {"MSVCR90.dll", "_CIsinh", ARGC_CDECL, crt_ci_sinh},
    {"MSVCR90.dll", "_CItanh", ARGC_CDECL, crt_ci_tanh},
    {"MSVCR90.dll", "_CIlog", ARGC_CDECL, crt_ci_log},
    {"MSVCR90.dll", "_CIcos", ARGC_CDECL, crt_ci_cos},
    {"MSVCR90.dll", "_CIsin", ARGC_CDECL, crt_ci_sin},
    {"MSVCR90.dll", "_CIsqrt", ARGC_CDECL, crt_ci_sqrt},
    {"MSVCR90.dll", "_CIasin", ARGC_CDECL, crt_ci_asin},
    {"MSVCR90.dll", "_CIacos", ARGC_CDECL, crt_ci_acos},
    {"MSVCR90.dll", "_CIfmod", ARGC_CDECL, crt_ci_fmod},
    {"MSVCR90.dll", "_CIpow", ARGC_CDECL, crt_ci_pow},
    {"MSVCR90.dll", "floor", ARGC_CDECL, crt_floor},
    {"MSVCR90.dll", "ceil", ARGC_CDECL, crt_ceil},
    {"MSVCR90.dll", "ldexp", ARGC_CDECL, crt_ldexp},
    {"MSVCR90.dll", "modf", ARGC_CDECL, crt_modf},
    {"MSVCR90.dll", "_isnan", ARGC_CDECL, crt_isnan},
    {"MSVCR90.dll", "_finite", ARGC_CDECL, crt_finite},
    {"MSVCR90.dll", "isupper", ARGC_CDECL, crt_isupper},
    {"MSVCR90.dll", "islower", ARGC_CDECL, crt_islower},
    {"MSVCR90.dll", "isdigit", ARGC_CDECL, crt_isdigit},
    {"MSVCR90.dll", "isspace", ARGC_CDECL, crt_isspace},
    {"MSVCR90.dll", "isalpha", ARGC_CDECL, crt_isalpha},
    {"MSVCR90.dll", "isalnum", ARGC_CDECL, crt_isalnum},
    {"MSVCR90.dll", "isprint", ARGC_CDECL, crt_isprint},
    {"MSVCR90.dll", "isxdigit", ARGC_CDECL, crt_isxdigit},
    {"MSVCR90.dll", "tolower", ARGC_CDECL, crt_tolower},
    {"MSVCR90.dll", "toupper", ARGC_CDECL, crt_toupper},
    {"MSVCR90.dll", "towlower", ARGC_CDECL, crt_towlower},
    {"MSVCR90.dll", "towupper", ARGC_CDECL, crt_towupper},
    {"MSVCR90.dll", "iswctype", ARGC_CDECL, crt_iswctype},
};

} // namespace

void msvcr90_register() {
    imports_register_data("MSVCR90.dll", "_acmdln", 4);
    imports_register(g_msvcr90_shims, sizeof g_msvcr90_shims / sizeof g_msvcr90_shims[0]);
    imports_register_strict("MSVCR90.dll");
}

void msvcr71_register() {
    static std::vector<ImportShim> shims;
    if (shims.empty()) {
        const size_t count = sizeof g_msvcr90_shims / sizeof g_msvcr90_shims[0];
        shims.assign(g_msvcr90_shims, g_msvcr90_shims + count);
        for (ImportShim &shim : shims)
            shim.dll = "MSVCR71.dll";
    }
    imports_register(shims.data(), shims.size());
    imports_register_strict("MSVCR71.dll");
    imports_register_data("MSVCR71.dll", "_acmdln", 4);
    imports_register_data("MSVCR71.dll", "_iob", 3 * kCrtFileSize);
}

void msvcr90_init() {
    for (const char *dll : {"MSVCR90.dll", "MSVCR71.dll"}) {
        uint32_t slot = imports_data_address(dll, "_acmdln");
        if (slot)
            wr32(slot, win32_command_line());
    }
    g_iob_addr = imports_data_address("MSVCR71.dll", "_iob");
    crt_bind_iob(g_iob_addr);
}
