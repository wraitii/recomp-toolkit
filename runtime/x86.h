/* x86.h - guest CPU state and instruction helpers for the static recompiler.
 *
 * Used by the code tools/recomp/driver.py generates.  The generated C in
 * build/recomp/gen/ includes this header, and so does the runtime in
 * runtime/.  Field names are fixed by the m1-recomp plan's
 * "Global constraints" section; do not rename them.
 *
 * Conventions
 *   - All guest pointers are uint32_t.  Guest address `a` lives at g_mem + a.
 *   - Flag fields hold 0 or 1.  The translator only stores a flag when a
 *     later instruction may read it (per-function liveness), so a flag field
 *     is only meaningful where the original x86 flag was live.
 *   - x87 registers are `double` (plan ruling; not 80-bit).
 */
#ifndef RECOMP_X86_H
#define RECOMP_X86_H

#include <stdint.h>
#include <stdbool.h>
#include <limits.h>
#include <string.h>
#include <math.h>
#include "observe.h"
#ifndef M_LN2 /* glibc and MSVC hide the M_ constants without feature macros */
#define M_LN2 0.693147180559945309417
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* The hot guest-memory and x87 helpers below are large enough that the
 * compiler stops inlining them on its own, which costs a call per guest
 * memory access, so these insist. */
#if defined(__GNUC__) || defined(__clang__)
#define RECOMP_HOT_INLINE static inline __attribute__((always_inline))
#define RECOMP_UNLIKELY(x) __builtin_expect(!!(x), 0)
#define RECOMP_COLD static __attribute__((noinline, cold))
#else
#define RECOMP_HOT_INLINE static inline
#define RECOMP_UNLIKELY(x) (x)
#define RECOMP_COLD static
#endif

/* ---------------------------------------------------------------- memory */

#ifndef GUEST_SIZE
#define GUEST_SIZE 0x10000000u /* 256 MB arena; RECOMP_GUEST_SIZE grows it for auxiliary modules */
#endif

#ifndef RECOMP_ARENA_ADDRESS
#define RECOMP_ARENA_ADDRESS 0
#endif
#if RECOMP_ARENA_ADDRESS
#define RECOMP_ARENA ((uint8_t *)(uintptr_t)RECOMP_ARENA_ADDRESS)
#else
#define RECOMP_ARENA g_mem
#endif
extern uint8_t *g_mem;

/* Guest memory layout (see plan Global constraints, as corrected):
 *   0x00400000 + SizeOfImage  image (preferred base, no relocation).  The end
 *                             is a PE header value, not a constant; ask the
 *                             loader (loader_image_limit()) for it.
 *   GUEST_HEAP_BASE..GUEST_HEAP_END  heap arena (configurable)
 *   0x0f000000                stack top, grows down (8 MB)
 *   0x0f000000..0x0fd00000    mod heap (mods/guest_mem.cpp)
 *   0x0fe00000                TEB (FS base)
 *   0x0ff00000 + 16*i         import shim trampoline for import i
 */
/* The build passes the game's image base; this default serves the test harness. */
#ifndef GUEST_IMAGE_BASE
#define GUEST_IMAGE_BASE 0x00400000u
#endif
/* The build passes the game's heap start ([game] heap_base); the default
 * serves the test harness and games whose image ends below 16 MB. */
#ifndef GUEST_HEAP_BASE
#define GUEST_HEAP_BASE 0x01000000u
#endif
/* Larger game heaps can sit above the fixed low runtime regions. */
#ifndef GUEST_HEAP_END
#define GUEST_HEAP_END 0x0e000000u
#endif
#if GUEST_HEAP_BASE >= GUEST_HEAP_END || GUEST_HEAP_END > GUEST_SIZE
#error "Guest heap must be a nonempty interval inside the arena"
#endif
#if GUEST_HEAP_BASE < 0x10000000u && GUEST_HEAP_END > 0x0e000000u
#error "Guest heap overlaps the mod heap, stack, TEB or import regions"
#endif
#define GUEST_STACK_TOP 0x0f000000u
#define GUEST_TEB_BASE 0x0fe00000u
#define GUEST_SHIM_BASE 0x0ff00000u
#define GUEST_SHIM_END 0x10000000u
#define GUEST_SHIM_STRIDE 16u
#define GUEST_RETURN_SENTINEL 0x0fdfff00u
/* Windows maps nothing in the first 64 KB, so a read or a write through a
 * null pointer faults and the program's own handlers see it. A flat arena
 * hands back a zero instead, which turns a null dereference into ordinary
 * data: Need for Speed Most Wanted walked an std::map whose node pointer was
 * null, read _Isnil from guest 0x15, got zero from the arena and looped on
 * the same node until the watchdog ended the run. recomp_unknown_call has
 * modelled this for calls all along; these are the other two ways to reach
 * the page. */
#define GUEST_NULL_LIMIT 0x10000u
/* Compiled in only when the build asks for it. The check is one compare
 * against a constant, but it sits in rd8/rd16/rd32 and their writers, which
 * are the hottest code in the system, and a guest that dereferences null is
 * broken whether or not we are looking. Leaving it in every build spends that
 * on every access of every game to catch a bug in one of them - and it
 * perturbs timing, which is how it was noticed: the game this found a spin in
 * has an intermittent race whose odds moved when the check went in.
 *
 * -DRECOMP_NULL_CHECKS=1 builds it; RECOMP_NULL_FAULTS=1 then arms it. Translate
 * such a build with fault_state = "exact" and msvc_x87_convention = false. */
#if defined(RECOMP_NULL_CHECKS) && RECOMP_NULL_CHECKS
#define RECOMP_NULL_GUARD(a, write)                                                                \
    do {                                                                                           \
        if (RECOMP_UNLIKELY((a) < GUEST_NULL_LIMIT))                                               \
            recomp_null_access((a), (write));                                                      \
    } while (0)
#else
#define RECOMP_NULL_GUARD(a, write) ((void)0)
#endif
/* Raises the access violation Windows would for an address in that page.
 * `write` picks the parameter the dispatcher reports. Declared here because
 * the accessors below are what call it. */
void recomp_null_access(uint32_t addr, int write);

/* Little-endian host (ARM64) matches the guest, so memcpy is a plain load. */
RECOMP_HOT_INLINE uint8_t rd8(uint32_t a) {
    RECOMP_NULL_GUARD(a, 0);
    return g_mem[a];
}
RECOMP_HOT_INLINE uint16_t rd16(uint32_t a) {
    RECOMP_NULL_GUARD(a, 0);
    uint16_t v;
    memcpy(&v, g_mem + a, 2);
    return v;
}
RECOMP_HOT_INLINE uint32_t rd32(uint32_t a) {
    RECOMP_NULL_GUARD(a, 0);
    uint32_t v;
    memcpy(&v, g_mem + a, 4);
    return v;
}
RECOMP_HOT_INLINE uint64_t rd64(uint32_t a) {
    uint64_t v;
    memcpy(&v, g_mem + a, 8);
    return v;
}
/* A guest-memory watchpoint.  RECOMP_WATCH=<hex address>[:<length>] reports
 * every write that touches those bytes together with the host backtrace, which
 * names the generated function and so the guest routine doing the writing.
 * This is the instrument for a stray write: the corrupted value says what the
 * damage is, never who did it, and the writer is usually nowhere near the
 * reader.  Unarmed - which is always, unless the switch is set - g_watch_len is
 * zero and a write costs one compare the branch predictor never takes. */
extern uint32_t g_watch_base;
extern uint32_t g_watch_len;
void recomp_watch_hit(uint32_t addr, uint32_t n, uint64_t value);

/* The spans written inside ranges the runtime is watching for itself: a
 * DirectDraw surface under an open Lock. Its Unlock has to find what the guest
 * changed through the pointer it was handed, and comparing the whole surface
 * to find a line of text is most of a frame; every store here widens the
 * written span of each range it lands in, so the Unlock compares only that.
 * No range is open almost always, and then this is one compare. */
#define RECOMP_DIRTY_SLOTS 4
typedef struct RecompDirty {
    uint32_t base, len; /* the range */
    uint32_t lo, hi;    /* what was written inside it; lo >= hi is nothing */
} RecompDirty;
extern RecompDirty g_dirty[RECOMP_DIRTY_SLOTS];
extern uint32_t g_dirty_count;
static inline void recomp_dirty(uint32_t a, uint32_t n) {
    for (uint32_t i = 0; i < g_dirty_count; ++i) {
        RecompDirty *d = &g_dirty[i];
        if (a < d->base + d->len && a + n > d->base) {
            if (a < d->lo)
                d->lo = a;
            if (a + n > d->hi)
                d->hi = a + n;
        }
    }
}

/* Store hooks: the watchpoint and DirectDraw dirty tracking. A game opts out
 * with [game] store_hooks = false, which builds RECOMP_STORE_HOOKS=0: no guest
 * store then tests anything, RECOMP_WATCH refuses to arm with a diagnostic and
 * a DirectDraw Unlock compares the whole locked surface. The test costs more
 * than its load and branch: every store may alias g_store_hook, so it is
 * reloaded after each one, and the stored value stays live for the call.
 *
 * g_store_hook is g_watch_len | g_dirty_count, kept by whoever changes either
 * (recomp_store_hook_update): one word is one load where the two counters
 * were two. */
#ifndef RECOMP_STORE_HOOKS
#define RECOMP_STORE_HOOKS 1
#endif
extern uint32_t g_store_hook;
static inline void recomp_store_hook_update(void) {
    g_store_hook = g_watch_len | g_dirty_count;
}
/* Keep the inactive gate in the store's caller: an outlined call for every
 * guest write costs more than the diagnostic check itself. */
RECOMP_HOT_INLINE void recomp_watch(uint32_t a, uint32_t n, uint64_t v) {
    // Both features are disarmed on the ordinary write (the watchpoint is a
    // diagnostic and a locked DirectDraw surface is rare), so one predictable
    // branch covers both. Arm and dirty state are set on this same guest
    // thread, so the value read here is the one the original two checks saw.
    if (RECOMP_STORE_HOOKS && RECOMP_UNLIKELY(g_store_hook != 0)) {
        if (g_watch_len != 0 && a < g_watch_base + g_watch_len && g_watch_base < a + n)
            recomp_watch_hit(a, n, v);
        if (g_dirty_count != 0)
            recomp_dirty(a, n);
    }
}

/* The bit copy behind float guest stores. __builtin_memcpy is spelled out
 * because the corpus and checks build with -fno-builtin, where plain memcpy
 * from a float local left a store/reload through the stack in the hot x87
 * store path. Both spellings copy the same 4 bytes. */
#if defined(__GNUC__) || defined(__clang__)
#define RECOMP_BITCOPY(d, s, n) __builtin_memcpy((d), (s), (n))
#else
#define RECOMP_BITCOPY(d, s, n) memcpy((d), (s), (n))
#endif
/* The store hook of a binary32 store: the watch and dirty checks see the
 * stored bit pattern, computed only when a hook is armed. */
static inline void recomp_watch_f32(uint32_t a, float v) {
    if (RECOMP_STORE_HOOKS && RECOMP_UNLIKELY(g_store_hook != 0)) {
        uint32_t bits;
        RECOMP_BITCOPY(&bits, &v, 4);
        recomp_watch(a, 4, bits);
    }
}

RECOMP_HOT_INLINE void wr8(uint32_t a, uint8_t v) {
    RECOMP_NULL_GUARD(a, 1);
    g_mem[a] = v;
    recomp_watch(a, 1, v);
}
RECOMP_HOT_INLINE void wr16(uint32_t a, uint16_t v) {
    RECOMP_NULL_GUARD(a, 1);
    memcpy(g_mem + a, &v, 2);
    recomp_watch(a, 2, v);
}
RECOMP_HOT_INLINE void wr32(uint32_t a, uint32_t v) {
    RECOMP_NULL_GUARD(a, 1);
    memcpy(g_mem + a, &v, 4);
    recomp_watch(a, 4, v);
}
RECOMP_HOT_INLINE void wr64(uint32_t a, uint64_t v) {
    memcpy(g_mem + a, &v, 8);
    recomp_watch(a, 8, v);
}
RECOMP_HOT_INLINE float rdf32(uint32_t a) {
    float v;
    memcpy(&v, g_mem + a, 4);
    return v;
}
RECOMP_HOT_INLINE double rdf64(uint32_t a) {
    double v;
    memcpy(&v, g_mem + a, 8);
    return v;
}
RECOMP_HOT_INLINE void wrf32(uint32_t a, float v) {
    RECOMP_NULL_GUARD(a, 1);
    RECOMP_BITCOPY(g_mem + a, &v, 4);
    recomp_watch_f32(a, v);
}
RECOMP_HOT_INLINE void wrf64(uint32_t a, double v) {
    uint64_t bits;
    memcpy(&bits, &v, 8);
    wr64(a, bits);
}

/* 80-bit x87 extended precision.  x87 registers are `double` here (plan
 * ruling), so a tbyte load rounds to 64-bit and a store re-expands. */
static inline double rdf80(uint32_t a) {
    uint64_t m = rd64(a);
    uint16_t se = rd16(a + 8);
    int exp = se & 0x7fffu;
    double v;
    if (exp == 0x7fff)
        v = (m << 1) ? (double)NAN : (double)INFINITY;
    else if (!exp && !m)
        v = 0.0;
    else
        v = ldexp((double)m, exp - 16383 - 63);
    return (se & 0x8000u) ? -v : v;
}

static inline void wrf80(uint32_t a, double v) {
    uint64_t m;
    uint16_t se;
    if (isnan(v)) {
        m = 0xc000000000000000ull;
        se = 0x7fffu;
    } else if (isinf(v)) {
        m = 0x8000000000000000ull;
        se = v < 0 ? 0xffffu : 0x7fffu;
    } else if (v == 0.0) {
        m = 0;
        se = signbit(v) ? 0x8000u : 0u;
    } else {
        int e, sign = signbit(v);
        double f = frexp(sign ? -v : v, &e); /* 0.5 <= f < 1 */
        m = (uint64_t)ldexp(f, 64);
        se = (uint16_t)((e - 1 + 16383) & 0x7fff);
        if (sign)
            se |= 0x8000u;
    }
    wr64(a, m);
    wr16(a + 8, se);
}

/* FBSTP: 18 packed BCD digits, little-endian, sign in bit 7 of byte 9. Rounds
 * to nearest even as the default control word does. Out-of-range values store
 * the BCD indefinite (0xffff c000 0000 0000 0000), as the hardware does. */
static inline void wrbcd80(uint32_t a, double v) {
    uint8_t out[10];
    memset(out, 0, sizeof out);
    double r = nearbyint(v);
    if (!(fabs(r) < 1e18)) {
        static const uint8_t indefinite[10] = {0, 0, 0, 0, 0, 0, 0, 0xc0, 0xff, 0xff};
        memcpy(g_mem + a, indefinite, 10);
        {
            uint64_t low;
            memcpy(&low, indefinite, 8);
            recomp_watch(a, 10, low);
        }
        return;
    }
    uint64_t m = (uint64_t)fabs(r);
    for (int i = 0; i < 9; ++i) {
        uint8_t lo = (uint8_t)(m % 10);
        m /= 10;
        uint8_t hi = (uint8_t)(m % 10);
        m /= 10;
        out[i] = (uint8_t)(hi << 4 | lo);
    }
    if (r < 0 || (r == 0 && signbit(v)))
        out[9] = 0x80;
    memcpy(g_mem + a, out, 10);
    {
        uint64_t low;
        memcpy(&low, out, 8);
        recomp_watch(a, 10, low);
    }
}

/* ------------------------------------------------------------- cpu state */

/* Register indices into X86.r */
enum { R_EAX = 0, R_ECX, R_EDX, R_EBX, R_ESP, R_EBP, R_ESI, R_EDI };

/* Lazy arithmetic flags: the operands of the last flag-producing integer
 * instruction, kept in X86 instead of materialising all six flag fields at a
 * seam.  x86_cc_settle writes the fields and clears the descriptor.  NONE
 * means the fields are current. ADD/ADC/SUB/SBB/CMP/LOGIC/INC/DEC are encoded;
 * everything else writes the fields eagerly and stores NONE. */
enum X86CcOp {
    X86_CC_NONE = 0,
    X86_CC_ADD,
    X86_CC_SUB,
    X86_CC_LOGIC,
    X86_CC_INC,
    X86_CC_DEC,
    X86_CC_ADC,
    X86_CC_SBB,
};

/* Which flag fields a pending descriptor defines and must write.  Flags not in
 * the mask keep their current field value, matching the eager publication. */
enum {
    X86_CCF_CF = 1u << 0,
    X86_CCF_PF = 1u << 1,
    X86_CCF_AF = 1u << 2,
    X86_CCF_ZF = 1u << 3,
    X86_CCF_SF = 1u << 4,
    X86_CCF_OF = 1u << 5,
};

struct X86 {
    uint32_t r[8]; /* EAX ECX EDX EBX ESP EBP ESI EDI */
    uint32_t eip;
    uint32_t eflags_cf, eflags_zf, eflags_sf, eflags_of, eflags_pf, eflags_af, eflags_df;
    /* Every EFLAGS bit other than CF PF AF ZF SF DF OF, so PUSHFD/POPFD
     * round-trip (the CPUID probe toggles the ID bit).  Initialise to 0x202. */
    uint32_t eflags_misc;
    /* Pending arithmetic flags (see enum X86CcOp).  cc_size is 1/2/4 bytes;
     * cc_mask names the fields to write; cc_a/cc_b are the operands and
     * cc_res the wrapped result; cc_carry is the incoming ADC/SBB carry. */
    uint8_t cc_op, cc_size, cc_mask, cc_carry;
    uint32_t cc_a, cc_b, cc_res;
    double st[8]; /* x87 stack, physical slots */
    /* FILD integers retain all 64 mantissa bits until arithmetic replaces
     * them. The double alone cannot preserve qword copies above 2^53. */
    uint64_t st_bits[8];
    uint8_t st_exact[8];
    uint32_t fpu_top; /* ST(i) == st[(fpu_top + i) & 7] */
    uint16_t fpu_cw, fpu_sw;
    /* x87 tag word, 2 bits per PHYSICAL register st[i]: 3 = empty, else in
     * use.  Initialise to 0xffff (the post-FINIT all-empty state). */
    uint16_t fpu_tag;
    uint32_t fs_base;
    /* MXCSR. Initialise to 0x1f80 (round to nearest, every exception masked). */
    uint32_t mxcsr;
    /* Held as dwords: SSE arithmetic and moves address the register in
     * dword lanes, and on a little-endian host the byte order of a copy then
     * takes care of itself. Lane 0 is the low four bytes. */
    uint32_t xmm[8][4];
    /* The eight MMX registers. Kept apart from the x87 stack: a program that
     * leaves MMX without EMMS and then uses the FPU sees its old values, not
     * the MMX ones, which no game this runtime serves relies on. */
    uint64_t mm[8];
    /* Translated calls until this guest thread's next scheduler checkpoint. */
    uint32_t checkpoint_countdown;
};
typedef struct X86 X86;

/* ----------------------------------------------------------------- SSE */

/* An XMM register holds dwords (see struct X86), so a scalar double or float
 * in its low lanes is read and written through these. Scalar SSE arithmetic
 * leaves the lanes above its result alone, which is why nothing here clears
 * them. */
static inline double xmm_f64(const X86 *c, int reg) {
    uint64_t bits = (uint64_t)c->xmm[reg][0] | ((uint64_t)c->xmm[reg][1] << 32);
    double v;
    memcpy(&v, &bits, 8);
    return v;
}
static inline void xmm_set_f64(X86 *c, int reg, double v) {
    uint64_t bits;
    memcpy(&bits, &v, 8);
    c->xmm[reg][0] = (uint32_t)bits;
    c->xmm[reg][1] = (uint32_t)(bits >> 32);
}
static inline float xmm_f32(const X86 *c, int reg) {
    float v;
    memcpy(&v, &c->xmm[reg][0], 4);
    return v;
}
static inline void xmm_set_f32(X86 *c, int reg, float v) {
    memcpy(&c->xmm[reg][0], &v, 4);
}

/* SQRTSD and SQRTSS of a negative operand return the "indefinite" quiet NaN,
 * which has its sign bit set; the host's sqrt returns a positive NaN and sets
 * errno. Everything else is the same operation in the same rounding mode. */
/* The sign is tested as a bit rather than by comparing, because a compiler
 * that may assume a NaN's payload is unspecified folds the comparison away
 * and leaves the host's own sqrt to produce its own NaN - which on ARM is the
 * positive one. */
static inline double recomp_sse_sqrt(double v) {
    uint64_t bits;
    memcpy(&bits, &v, 8);
    if ((bits >> 63) && (bits << 1) != 0) { /* negative, and not -0.0 */
        uint64_t indefinite = 0xfff8000000000000ull;
        double r;
        memcpy(&r, &indefinite, 8);
        return r;
    }
    return sqrt(v);
}
static inline int recomp_sse_nan_bits(uint32_t bits) {
    return (bits & 0x7fffffffu) > 0x7f800000u;
}
static inline float recomp_sse_sqrtf(float v) {
    float r = sqrtf(v);
    if (RECOMP_UNLIKELY(isnan(r))) {
        uint32_t bits;
        memcpy(&bits, &v, 4);
        bits = recomp_sse_nan_bits(bits) ? (bits | 0x00400000u) : 0xffc00000u;
        memcpy(&r, &bits, 4);
    }
    return r;
}

/* CVTSS2SI/CVTSD2SI round according to MXCSR, which recomp_ldmxcsr holds at
 * round-to-nearest-even. Values that round outside int32 produce the
 * indefinite result. */
static inline int32_t recomp_sse_cvtsd2si(double v) {
    double rounded = nearbyint(v);
    if (!(rounded >= -2147483648.0 && rounded <= 2147483647.0))
        return INT32_MIN;
    return (int32_t)rounded;
}
static inline int32_t recomp_sse_cvtss2si(float v) {
    return recomp_sse_cvtsd2si((double)v);
}
static inline int32_t recomp_sse_cvttsd2si(double v) {
    if (!(v >= -2147483648.0 && v < 2147483648.0))
        return INT32_MIN;
    return (int32_t)v;
}
static inline int32_t recomp_sse_cvttss2si(float v) {
    return recomp_sse_cvttsd2si((double)v);
}

static inline float recomp_sse_f32(uint32_t bits) {
    float v;
    memcpy(&v, &bits, 4);
    return v;
}
static inline uint32_t recomp_sse_f32_bits(float v) {
    uint32_t bits;
    memcpy(&bits, &v, 4);
    return bits;
}
/* MXCSR.FTZ replaces a tiny result with a zero of the same sign. x86 detects
 * tininess after rounding to an unbounded exponent, so a result the host
 * rounds up to the smallest normal can still flush; the exact value, which
 * double precision holds for these operations, scaled out of the denormal
 * range, decides it. */
static inline uint32_t recomp_sse_result(const X86 *c, uint32_t a, uint32_t b, float r,
                                         double exact) {
    uint32_t bits = recomp_sse_f32_bits(r), magnitude = bits & 0x7fffffffu;
    if (RECOMP_UNLIKELY(magnitude - 1u < 0x00800000u)) {
        if ((c->mxcsr & 0x8000u) && fabsf((float)(exact * 0x1p64)) < 0x1p-62f)
            return bits & 0x80000000u;
        return bits;
    }
    if (!RECOMP_UNLIKELY(magnitude > 0x7f800000u))
        return bits;
    if (recomp_sse_nan_bits(a))
        return a | 0x00400000u;
    if (recomp_sse_nan_bits(b))
        return b | 0x00400000u;
    return 0xffc00000u;
}
static inline uint32_t recomp_sse_addps(const X86 *c, uint32_t a, uint32_t b) {
    float x = recomp_sse_f32(a), y = recomp_sse_f32(b);
    return recomp_sse_result(c, a, b, x + y, (double)x + (double)y);
}
static inline uint32_t recomp_sse_subps(const X86 *c, uint32_t a, uint32_t b) {
    float x = recomp_sse_f32(a), y = recomp_sse_f32(b);
    return recomp_sse_result(c, a, b, x - y, (double)x - (double)y);
}
static inline uint32_t recomp_sse_mulps(const X86 *c, uint32_t a, uint32_t b) {
    float x = recomp_sse_f32(a), y = recomp_sse_f32(b);
    return recomp_sse_result(c, a, b, x * y, (double)x * (double)y);
}
static inline uint32_t recomp_sse_divps(const X86 *c, uint32_t a, uint32_t b) {
    float x = recomp_sse_f32(a), y = recomp_sse_f32(b);
    return recomp_sse_result(c, a, b, x / y, (double)x / (double)y);
}
static inline uint32_t recomp_sse_minps(uint32_t a, uint32_t b) {
    float x = recomp_sse_f32(a), y = recomp_sse_f32(b);
    return x < y ? a : b;
}
static inline uint32_t recomp_sse_maxps(uint32_t a, uint32_t b) {
    float x = recomp_sse_f32(a), y = recomp_sse_f32(b);
    return x > y ? a : b;
}
static inline uint32_t recomp_sse_sqrtps(uint32_t a) {
    return recomp_sse_f32_bits(recomp_sse_sqrtf(recomp_sse_f32(a)));
}
/* The original RCPPS/RSQRTPS are approximate. These return the IEEE result,
 * which is also what the Unicorn oracle produces. DIVERGENCE(original): the
 * low bits may differ from a real x86 approximation instruction. RCPPS
 * flushes a tiny result to zero whatever MXCSR says. */
static inline uint32_t recomp_sse_rcpps(uint32_t a) {
    float r = 1.0f / recomp_sse_f32(a);
    uint32_t bits = recomp_sse_f32_bits(r);
    if (RECOMP_UNLIKELY(isnan(r)))
        return recomp_sse_nan_bits(a) ? (a | 0x00400000u) : 0xffc00000u;
    if (RECOMP_UNLIKELY((bits & 0x7fffffffu) < 0x00800000u))
        return bits & 0x80000000u;
    return bits;
}
static inline uint32_t recomp_sse_rsqrtps(uint32_t a) {
    float r = 1.0f / recomp_sse_sqrtf(recomp_sse_f32(a));
    if (RECOMP_UNLIKELY(isnan(r)))
        return recomp_sse_nan_bits(a) ? (a | 0x00400000u) : 0xffc00000u;
    return recomp_sse_f32_bits(r);
}
static inline float recomp_sse_rcpss(float v) {
    return recomp_sse_f32(recomp_sse_rcpps(recomp_sse_f32_bits(v)));
}
static inline float recomp_sse_rsqrtss(float v) {
    return recomp_sse_f32(recomp_sse_rsqrtps(recomp_sse_f32_bits(v)));
}
static inline uint32_t recomp_sse_cmpps(unsigned pred, uint32_t a, uint32_t b) {
    float x = recomp_sse_f32(a), y = recomp_sse_f32(b);
    int r;
    switch (pred & 7u) {
    case 0:
        r = x == y;
        break;
    case 1:
        r = x < y;
        break;
    case 2:
        r = x <= y;
        break;
    case 3:
        r = isnan(x) || isnan(y);
        break;
    case 4:
        r = !(x == y);
        break;
    case 5:
        r = !(x < y);
        break;
    case 6:
        r = !(x <= y);
        break;
    default:
        r = !(isnan(x) || isnan(y));
        break;
    }
    return r ? 0xffffffffu : 0u;
}

/* COMISD/UCOMISD and their single-precision forms: the one place SSE writes
 * the guest's flags. Unordered - either operand a NaN - sets ZF, PF and CF
 * together, and the rest of EFLAGS is cleared either way. The difference
 * between the two forms is only which NaNs raise an exception, and this kit
 * raises none. */
static inline void recomp_comis(X86 *c, double a, double b) {
    c->cc_op = X86_CC_NONE;
    c->eflags_of = c->eflags_af = c->eflags_sf = 0;
    if (a != a || b != b) {
        c->eflags_zf = c->eflags_pf = c->eflags_cf = 1;
    } else {
        c->eflags_pf = 0;
        c->eflags_zf = a == b;
        c->eflags_cf = a < b;
    }
}

/* ------------------------------------------------- runtime call-outs
 * Implemented in generated table.c (recomp_call) or by runtime/.
 */

/* Indirect CALL: dispatch `target` to a translated function, an import shim,
 * or recomp_unknown_call.  Generated into build/recomp/gen/table.c. */
void recomp_call(X86 *c, uint32_t target);
void recomp_run(X86 *c, uint32_t target);
/* Translated call boundaries also schedule guest-only polling loops. Does not
 * alter this thread's guest state; the scheduler keeps guest execution
 * serialized. The countdown lives in the guest thread's CPU rather than a
 * thread-local, which costs a TLV lookup per call on Darwin. */
void recomp_execution_checkpoint_due(X86 *c);
static inline void recomp_execution_checkpoint(X86 *c) {
    if (__builtin_expect(c->checkpoint_countdown-- == 0u, 0))
        recomp_execution_checkpoint_due(c);
}
static inline void recomp_execution_checkpoint_charge(X86 *c, uint32_t cost) {
    if (c->checkpoint_countdown < cost)
        recomp_execution_checkpoint_due(c);
    else
        c->checkpoint_countdown -= cost;
}

/* RECOMP_WATCH_FRAME=1 reports a guest call that returns with EBP changed.
 * A routine that loses the frame pointer corrupts nothing and crashes nowhere:
 * its caller simply reads its locals from somewhere else afterwards, and the
 * damage surfaces as a wrong value far away. Off, this costs one register copy
 * and one compare per call, both of which the optimiser folds away. */
extern uint32_t recomp_frame_watch;
void recomp_frame_changed(X86 *c, uint32_t target, uint32_t before, uint32_t after);
/* The callee-saved registers of the MSVC ABI. RECOMP_WATCH_FRAME also reports a
 * call that returns with EBX, ESI or EDI changed: a loop that keeps its cursor
 * in ESI, or a constant in EBX, then reads a wrong value far from the callee
 * that broke it. */
typedef struct {
    uint32_t ebx, ebp, esi, edi;
} RecompSaved;
static inline void recomp_save(const X86 *c, RecompSaved *s) {
    if (recomp_frame_watch) {
        s->ebx = c->r[3];
        s->ebp = c->r[5];
        s->esi = c->r[6];
        s->edi = c->r[7];
    }
}
void recomp_saved_changed(X86 *c, uint32_t target, const RecompSaved *before);
static inline void recomp_check_saved(X86 *c, uint32_t target, const RecompSaved *before) {
    if (c->r[3] != before->ebx || c->r[5] != before->ebp || c->r[6] != before->esi ||
        c->r[7] != before->edi)
        recomp_saved_changed(c, target, before);
}
extern const int recomp_profile_enabled;
void recomp_profile_push(uint32_t index);
void recomp_profile_pop(void);

/* Indirect JMP or tail call: dispatch `target` through the generated
 * block-entry table (every function start plus every decoded jump-table
 * target), then the import shim range.  Anything else aborts. */
void recomp_jump(X86 *c, uint32_t target);

/* Indirect JMP whose target is neither a block entry nor a shim.  Aborts with
 * the target and the jumping instruction's address. */
void recomp_unknown_jump(X86 *c, uint32_t target);

/* Provided by runtime/ ------------------------------------- */

bool imports_dispatch(X86 *c, uint32_t target);
#if defined(__GNUC__)
__attribute__((noreturn, cold, noinline))
#endif
void recomp_shim_call(X86 *c, uint32_t target);
/* target is neither a translated function nor a shim.  Logs; may abort. */
void recomp_unknown_call(X86 *c, uint32_t target);
/* A checked return adjusted the target to an address that is not resumable. */
void recomp_unexpected_return(X86 *c, uint32_t site, uint32_t expected, uint32_t target);
/* CALL continuations inside code executed by the interpreter. */
unsigned recomp_dynamic_return_push(uint32_t target);
void recomp_dynamic_return_pop(unsigned depth);
int recomp_dynamic_return_accept(uint32_t target);
void recomp_region_missing(X86 *c, uint32_t target);
/* A popped callback sentinel can bypass translated callers. The innermost
 * guest_call owns the live host return checkpoint and its guest stack range. */
void recomp_callback_return(X86 *c);
uint32_t recomp_callback_depth(void);
void recomp_callback_truncate(uint32_t depth);
void recomp_callback_reset(X86 *c);
/* DIV/IDIV with a zero divisor or a quotient that does not fit. */
void recomp_div_error(X86 *c, uint32_t addr);

void recomp_rdtsc(X86 *c); /* -> EDX:EAX */
void recomp_cpuid(X86 *c); /* EAX in -> EAX EBX ECX EDX */
void recomp_ldmxcsr(X86 *c, uint32_t addr, uint32_t a);
uint32_t recomp_in(X86 *c, uint32_t port, int size);
void recomp_out(X86 *c, uint32_t port, uint32_t val, int size);
void recomp_cli(X86 *c);
void recomp_sti(X86 *c);
void recomp_hlt(X86 *c);
void recomp_int(X86 *c, uint32_t vec);
/* An instruction the translator could not model, reached at run time. The
 * translation carries a trap at that address instead of refusing the whole
 * image, because a listing routinely decodes the data past a function's last
 * instruction as code. Reaching one is fatal and says where. */
void recomp_unmodelled(X86 *c, uint32_t addr);
void recomp_unmodelled_named(X86 *c, uint32_t addr, const char *reason);
void recomp_x87_contract_violation(X86 *c, uint32_t addr, uint16_t mask, uint16_t value);
void recomp_breakpoint(X86 *c, uint32_t addr);

/* ------------------------------------------------------ hook dispatch -- */

/* Three tables, generated into build/recomp/gen/table.c:
 *   recomp_func_addrs   sorted address index, immutable
 *   recomp_base_ptrs    the original for each entry, fixed at build time
 *   recomp_hook_ptrs    mutable dispatch, initially "run the base"
 * plus recomp_hooked, the per-function flag the slow path reads with an
 * ACQUIRE load. The installer publishes the pointer with a release store and
 * then the flag with a release store, so a set flag always implies a visible
 * pointer. The index is passed to the dispatcher because one C function serves
 * every hooked entry. */
typedef void (*RecompHookFn)(X86 *c, uint32_t index);

/* Sorted table of every translated entry point (function starts, alternate
 * entries and jump-table block entries), from the generated table.c. */
extern const uint32_t recomp_func_addrs[];
extern const uint32_t recomp_func_count;
extern void (*const recomp_base_ptrs[])(X86 *c);
/* The untouched translation of each entry. recomp_base_ptrs holds this build's
 * FN_<addr> override where one is defined, so the two tables differ exactly
 * over the override set - which is how a run record states what was replaced,
 * instead of trusting an environment variable read after the build. */
extern void (*const recomp_raw_ptrs[])(X86 *c);
extern RecompHookFn recomp_hook_ptrs[];
extern uint8_t recomp_hooked[];
/* What each entry stub reads, unordered, before jumping straight to its body:
 * recomp_hooked or a process-wide observer (profiling, frame watch). The slow
 * path rereads recomp_hooked with acquire, so writers update this after it. */
extern uint8_t recomp_slow[];
static inline void recomp_slow_update(uint32_t i) {
    uint8_t slow = __atomic_load_n(&recomp_hooked[i], __ATOMIC_RELAXED) |
                   (uint8_t)(recomp_profile_enabled || recomp_frame_watch);
    __atomic_store_n(&recomp_slow[i], slow, __ATOMIC_RELEASE);
}
/* Set once any mod hook is installed and never cleared.  A call whose
 * contract dropped CPU fields publishes them anyway while this is set,
 * because a hook callback observes the full register file. */
extern uint8_t recomp_hooks_ever;

typedef void (*RecompEntryFn)(X86 *c);
typedef struct RecompDispatchEntry {
    uint32_t address, metadata;
    RecompEntryFn fn;
} RecompDispatchEntry;
#ifdef RECOMP_DISPATCH_ENTRIES
extern RecompDispatchEntry RECOMP_DISPATCH_ENTRIES[];
#endif
typedef struct RecompDispatchTable {
    const RecompDispatchEntry *entries;
    uint32_t base;
    uint64_t span;
    uint32_t shift, max_probe, count;
} RecompDispatchTable;
extern const RecompDispatchTable recomp_dispatch;
void recomp_dispatch_init(RecompDispatchEntry *entries, const RecompDispatchTable *table,
                          const RecompDispatchEntry *seeds, uint32_t seed_count);
const RecompDispatchEntry *recomp_dispatch_lookup(const RecompDispatchTable *table,
                                                  uint32_t target);
RecompEntryFn recomp_icall_resolve(uint32_t target);
void recomp_icall_slow(X86 *c, uint32_t target, uint32_t site);

/* A host implementation of a stdcall guest function that guest_call runs in
 * place of the translation while the function is not hooked. args holds the
 * nargs stack arguments; the result is EAX. The implementation must leave the
 * guest's callee-saved registers and ESP as the original would. */
typedef uint32_t (*RecompHostCall)(X86 *c, const uint32_t *args);
void recomp_register_host_call(uint32_t fn, uint32_t nargs, RecompHostCall call);
RecompHostCall recomp_host_call(uint32_t fn, uint32_t nargs);
uint32_t recomp_guest_call(X86 *c, uint32_t fn, const uint32_t *args, int nargs);

uint32_t recomp_override_count(void);
uint64_t recomp_override_hash(void);

/* Index into the three tables, or -1 when addr is not an entry. Being an entry
 * is NOT the same as being hookable: eligibility comes from symbols.json. */
int32_t recomp_index_of(uint32_t addr);
int recomp_is_call_return(uint32_t target);

typedef struct RecompCallFact {
    uint32_t ret, first, count;
} RecompCallFact;
extern const RecompCallFact recomp_call_facts[];
extern const uint32_t recomp_call_fact_count;
extern const uint32_t recomp_call_fact_targets[];
typedef struct RecompCwEffect {
    uint32_t ret;
    uint16_t preserved, known, value;
} RecompCwEffect;
extern const RecompCwEffect recomp_cw_effects[];
extern const uint32_t recomp_cw_effect_count;
#ifdef RECOMP_CHECK_CALL_FACTS
void recomp_call_fact_violation(uint32_t ret, uint32_t target);
void recomp_import_cw_violation(const char *name, uint16_t before, uint16_t after);
#endif

/* ------------------------------------------------ auxiliary modules -- */

/* Additional translated guest code (RECOMP_EXTRA_CODE libraries). Its table.c
 * registers the module from a constructor; the main image's recomp_call/recomp_jump
 * fall back to the registry after their own table misses. The runtime
 * loader maps the module's sections at [base, end), and LoadLibrary /
 * GetProcAddress answer from the module's export directory. */
typedef struct RecompRegion {
    uint32_t start, end, index;
    void (*fn)(X86 *, uint32_t);
    const uint8_t *boundaries;
} RecompRegion;
int recomp_region_call(X86 *c, const RecompRegion *regions, uint32_t count, uint32_t target);
int recomp_region_has_target(const RecompRegion *regions, uint32_t count, uint32_t target);
int recomp_module_region_has_target(uint32_t target);

typedef struct RecompModule {
    const char *name;
    uint32_t base, end;
    const uint32_t *func_addrs;
    uint32_t func_count;
    void (*const *base_ptrs)(X86 *c);
    RecompHookFn *hook_ptrs;
    uint8_t *hooked;
    const uint32_t *call_returns;
    uint32_t call_return_count;
    const char *const *profile_names;
    const RecompRegion *regions;
    uint32_t region_count;
    const RecompDispatchTable *dispatch;
} RecompModule;

void recomp_module_register(const RecompModule *m);
uint32_t recomp_module_count(void);
const RecompModule *recomp_module_at(uint32_t i);
const RecompModule *recomp_module_containing(uint32_t addr);
/* Index into the owning module's tables, or -1 when no module has addr as an entry. */
int32_t recomp_module_lookup(uint32_t target);
int recomp_module_is_call_return(uint32_t target);
/* Dispatches target through the owning module's tables; 0 when no module owns it. */
int recomp_module_call(X86 *c, uint32_t target);
void recomp_module_invoke(X86 *c, const RecompModule *m, int32_t i);
void recomp_module_jump(X86 *c, uint32_t target);

/* RET has already popped EIP and applied any immediate stack adjustment.
 * A CALL continuation belongs to the pending host caller, even when it is
 * also an alternate entry. Other entries are tail calls, as in interface
 * adapters that exchange a vtable method onto the guest stack before RET.
 * Delay-load adapters also RET into resolved import shims. An unrecognised
 * non-continuation target is a named runtime failure. */
static inline void recomp_return(X86 *c) {
    if (c->eip == GUEST_RETURN_SENTINEL) {
        recomp_callback_return(c);
        return;
    }
    if (recomp_is_call_return(c->eip) || recomp_module_is_call_return(c->eip))
        return;
    if (recomp_dynamic_return_accept(c->eip))
        return;
    if ((c->eip >= GUEST_SHIM_BASE && c->eip < GUEST_SHIM_END) || recomp_index_of(c->eip) >= 0 ||
        recomp_module_lookup(c->eip) >= 0)
        recomp_call(c, c->eip);
    else
        recomp_unexpected_return(c, c->eip, 0, c->eip);
}

static inline void recomp_return_resume(X86 *c, uint32_t site, uint32_t expected, uint32_t target) {
    if (target == expected)
        return;
    if ((target >= GUEST_SHIM_BASE && target < GUEST_SHIM_END) || recomp_index_of(target) >= 0 ||
        recomp_module_lookup(target) >= 0) {
        recomp_jump(c, target);
        return;
    }
    recomp_unexpected_return(c, site, expected, target);
}

/* --------------------------------------------------------------- flags */

RECOMP_HOT_INLINE uint32_t parity8(uint32_t v) {
    v &= 0xffu;
    v ^= v >> 4;
    v ^= v >> 2;
    v ^= v >> 1;
    return (~v) & 1u;
}

/* Materialise pending arithmetic flags into the fields and clear the
 * descriptor; callers go through x86_cc_settle.  Recipes match the
 * interpreter's and the decoded emitter's: logic clears AF, INC/DEC leave CF,
 * and MUL/IMUL are not encoded here.
 * DIVERGENCE(original): AND/OR/XOR/TEST choose zero for architecturally
 * undefined AF, consistently with the interpreter and both emitters. */
RECOMP_COLD void x86_cc_materialize(X86 *c) {
    const uint32_t op = c->cc_op;
    const uint32_t bits = (uint32_t)c->cc_size * 8u;
    const uint32_t shift = bits - 1u;
    const uint32_t sign = 1u << shift;
    const uint32_t mask = bits == 32u ? 0xffffffffu : ((1u << bits) - 1u);
    const uint32_t fields = c->cc_mask;
    const uint32_t a = c->cc_a & mask, b = c->cc_b & mask, r = c->cc_res & mask;
    const uint32_t carry = c->cc_carry & 1u;
    c->cc_op = X86_CC_NONE;
    c->cc_mask = 0;
    c->cc_size = 0;
    c->cc_carry = 0;
    c->cc_a = c->cc_b = c->cc_res = 0;
    switch (op) {
    case X86_CC_ADD:
    case X86_CC_ADC:
        if (fields & X86_CCF_CF)
            c->eflags_cf = (r < a) || (op == X86_CC_ADC && carry && r == a);
        if (fields & X86_CCF_OF)
            c->eflags_of = (((a ^ r) & (b ^ r)) >> shift) & 1u;
        if (fields & X86_CCF_AF)
            c->eflags_af = ((a ^ b ^ r) >> 4) & 1u;
        if (fields & X86_CCF_ZF)
            c->eflags_zf = (r == 0);
        if (fields & X86_CCF_SF)
            c->eflags_sf = (r >> shift) & 1u;
        if (fields & X86_CCF_PF)
            c->eflags_pf = parity8(r);
        break;
    case X86_CC_SUB:
    case X86_CC_SBB:
        if (fields & X86_CCF_CF)
            c->eflags_cf = (a < b) || (op == X86_CC_SBB && carry && a == b);
        if (fields & X86_CCF_OF)
            c->eflags_of = (((a ^ b) & (a ^ r)) >> shift) & 1u;
        if (fields & X86_CCF_AF)
            c->eflags_af = ((a ^ b ^ r) >> 4) & 1u;
        if (fields & X86_CCF_ZF)
            c->eflags_zf = (r == 0);
        if (fields & X86_CCF_SF)
            c->eflags_sf = (r >> shift) & 1u;
        if (fields & X86_CCF_PF)
            c->eflags_pf = parity8(r);
        break;
    case X86_CC_LOGIC:
        if (fields & X86_CCF_CF)
            c->eflags_cf = 0;
        if (fields & X86_CCF_OF)
            c->eflags_of = 0;
        if (fields & X86_CCF_AF)
            c->eflags_af = 0;
        if (fields & X86_CCF_ZF)
            c->eflags_zf = (r == 0);
        if (fields & X86_CCF_SF)
            c->eflags_sf = (r >> shift) & 1u;
        if (fields & X86_CCF_PF)
            c->eflags_pf = parity8(r);
        break;
    case X86_CC_INC:
        if (fields & X86_CCF_OF)
            c->eflags_of = (r == sign);
        if (fields & X86_CCF_AF)
            c->eflags_af = ((a ^ r) >> 4) & 1u;
        if (fields & X86_CCF_ZF)
            c->eflags_zf = (r == 0);
        if (fields & X86_CCF_SF)
            c->eflags_sf = (r >> shift) & 1u;
        if (fields & X86_CCF_PF)
            c->eflags_pf = parity8(r);
        break; /* CF is preserved. */
    case X86_CC_DEC:
        if (fields & X86_CCF_OF)
            c->eflags_of = (r == (sign - 1u));
        if (fields & X86_CCF_AF)
            c->eflags_af = ((a ^ r) >> 4) & 1u;
        if (fields & X86_CCF_ZF)
            c->eflags_zf = (r == 0);
        if (fields & X86_CCF_SF)
            c->eflags_sf = (r >> shift) & 1u;
        if (fields & X86_CCF_PF)
            c->eflags_pf = parity8(r);
        break; /* CF is preserved. */
    default:
        break;
    }
}

/* Settle sits at every decoded entry and call return, so only the NONE test
 * is inlined; the materialising switch stays out of line. */
RECOMP_HOT_INLINE void x86_cc_settle(X86 *c) {
    if (RECOMP_UNLIKELY(c->cc_op != X86_CC_NONE))
        x86_cc_materialize(c);
}

/* Drop a pending descriptor without materialising it: a direct field writer.
 * A producer that ran with a pending descriptor must call this first (the
 * decoded/interpreter paths settle at entry, so this is defence in depth).
 * Only cc_op is cleared: the payload is dead once cc_op is NONE, and the hot
 * path keeps the single store.  Test/diagnostic comparisons canonicalise the
 * payload explicitly through x86_cc_canonicalize. */
static inline void x86_cc_drop(X86 *c) {
    c->cc_op = X86_CC_NONE;
}

/* Materialise a descriptor that defines a flag in `need`; otherwise leave it
 * pending, since the `need` fields are already current. */
RECOMP_HOT_INLINE void x86_cc_settle_mask(X86 *c, uint32_t need) {
    if (RECOMP_UNLIKELY(c->cc_op != X86_CC_NONE) && (c->cc_mask & need))
        x86_cc_materialize(c);
}

/* As x86_cc_settle_mask, for a site whose region overwrites every flag
 * outside `need` before it is observed: a descriptor that defines none of
 * `need` is dropped. Only needed fields are computed before dropping the
 * descriptor; every other pending field is overwritten by the region. */
RECOMP_HOT_INLINE void x86_cc_settle_or_drop(X86 *c, uint32_t need) {
    if (RECOMP_UNLIKELY(c->cc_op != X86_CC_NONE)) {
        if (c->cc_mask & need) {
            c->cc_mask &= (uint8_t)need;
            x86_cc_materialize(c);
        } else
            c->cc_op = X86_CC_NONE;
    }
}

static inline void recomp_icall(X86 *c, uint32_t target, uint32_t site) {
#ifndef RECOMP_CHECK_CALL_FACTS
    if (!recomp_observe_on) {
        if (target - GUEST_SHIM_BASE < GUEST_SHIM_END - GUEST_SHIM_BASE) {
            if (!recomp_frame_watch) {
                if (RECOMP_UNLIKELY(!imports_dispatch(c, target)))
                    recomp_shim_call(c, target);
                return;
            }
        } else {
#ifdef RECOMP_DISPATCH_ENTRIES
            const RecompDispatchEntry *entries = RECOMP_DISPATCH_ENTRIES;
            uint32_t base = RECOMP_DISPATCH_BASE, shift = RECOMP_DISPATCH_SHIFT;
            uint64_t span = RECOMP_DISPATCH_SPAN;
#else
            const RecompDispatchEntry *entries = recomp_dispatch.entries;
            uint32_t base = recomp_dispatch.base, shift = recomp_dispatch.shift;
            uint64_t span = recomp_dispatch.span;
#endif
            RecompEntryFn fn;
            uint32_t offset = target - base;
            if (offset < span) {
                const RecompDispatchEntry *entry = entries + (offset >> shift);
                if (entry->address == target)
                    fn = entry->fn;
                else
                    fn = recomp_icall_resolve(target);
            } else
                fn = recomp_icall_resolve(target);
            if (fn) {
                fn(c);
                return;
            }
        }
    }
#endif
    recomp_icall_slow(c, target, site);
}

static inline void recomp_icall_host(X86 *c, uint32_t target, uint32_t site) {
#ifndef RECOMP_CHECK_CALL_FACTS
    if (!recomp_observe_on && !recomp_frame_watch) {
        if (RECOMP_UNLIKELY(!imports_dispatch(c, target)))
            recomp_shim_call(c, target);
        return;
    }
#endif
    recomp_icall_slow(c, target, site);
}

static inline void recomp_icall_guest(X86 *c, uint32_t target, uint32_t site) {
#ifndef RECOMP_CHECK_CALL_FACTS
    if (!recomp_observe_on) {
#ifdef RECOMP_DISPATCH_ENTRIES
        const RecompDispatchEntry *entries = RECOMP_DISPATCH_ENTRIES;
        uint32_t base = RECOMP_DISPATCH_BASE, shift = RECOMP_DISPATCH_SHIFT;
        uint64_t span = RECOMP_DISPATCH_SPAN;
#else
        const RecompDispatchEntry *entries = recomp_dispatch.entries;
        uint32_t base = recomp_dispatch.base, shift = recomp_dispatch.shift;
        uint64_t span = recomp_dispatch.span;
#endif
        RecompEntryFn fn;
        uint32_t offset = target - base;
        if (offset < span) {
            const RecompDispatchEntry *entry = entries + (offset >> shift);
            if (entry->address == target)
                fn = entry->fn;
            else
                fn = recomp_icall_resolve(target);
        } else
            fn = recomp_icall_resolve(target);
        if (fn) {
            fn(c);
            return;
        }
    }
#endif
    recomp_icall_slow(c, target, site);
}

/* Clear the dead descriptor payload so a full-struct comparison does not see
 * bytes left over from an earlier descriptor.  Production never reads them
 * while cc_op is NONE, so this is a test/diagnostic helper. */
static inline void x86_cc_canonicalize(X86 *c) {
    if (c->cc_op == X86_CC_NONE) {
        c->cc_size = c->cc_mask = 0;
        c->cc_carry = 0;
        c->cc_a = c->cc_b = c->cc_res = 0;
    }
}

/* Bits x86_get_eflags/x86_set_eflags build from the individual fields; every
 * other bit lives in eflags_misc so PUSHFD/POPFD is lossless. */
#define X86_EFLAGS_SPLIT 0x00000cd5u /* CF PF AF ZF SF DF OF */

static inline uint32_t x86_get_eflags(X86 *c) {
    x86_cc_settle(c);
    return (c->eflags_misc & ~X86_EFLAGS_SPLIT) | 0x00000002u /* reserved bit 1 reads as 1 */
           | (c->eflags_cf << 0) | (c->eflags_pf << 2) | (c->eflags_af << 4) | (c->eflags_zf << 6) |
           (c->eflags_sf << 7) | (c->eflags_df << 10) | (c->eflags_of << 11);
}

static inline void x86_set_eflags(X86 *c, uint32_t v) {
    x86_cc_drop(c);
    c->eflags_misc = (v & ~X86_EFLAGS_SPLIT) | 0x00000002u;
    c->eflags_cf = (v >> 0) & 1u;
    c->eflags_pf = (v >> 2) & 1u;
    c->eflags_af = (v >> 4) & 1u;
    c->eflags_zf = (v >> 6) & 1u;
    c->eflags_sf = (v >> 7) & 1u;
    c->eflags_df = (v >> 10) & 1u;
    c->eflags_of = (v >> 11) & 1u;
}

/* SAHF: AH -> SF ZF AF PF CF */
static inline void x86_sahf(X86 *c) {
    uint32_t ah = (c->r[R_EAX] >> 8) & 0xffu;
    x86_cc_drop(c);
    c->eflags_cf = (ah >> 0) & 1u;
    c->eflags_pf = (ah >> 2) & 1u;
    c->eflags_af = (ah >> 4) & 1u;
    c->eflags_zf = (ah >> 6) & 1u;
    c->eflags_sf = (ah >> 7) & 1u;
}

/* ------------------------------------------------------ shifts / rotates
 * The `_f` variants also write the flags the instruction defines.  A shift
 * or rotate with a zero count leaves every flag untouched, which is why
 * these are functions rather than inline expressions.
 */
/* Every shift is done on uint32_t: a uint8_t or uint16_t operand would
 * otherwise promote to signed int and `(uint16_t)0xffff << 16` is UB even
 * though the count is masked.  SAR shifts the sign-extended value, and a
 * count >= BITS saturates to BITS-1 so the arithmetic shift stays defined. */
#define SHIFT_OPS(BITS, UT, ST)                                                                    \
    static inline UT shl##BITS(UT v, uint32_t n) {                                                 \
        return (UT)((uint32_t)v << (n & 31u));                                                     \
    }                                                                                              \
    static inline UT shr##BITS(UT v, uint32_t n) {                                                 \
        return (UT)((uint32_t)v >> (n & 31u));                                                     \
    }                                                                                              \
    static inline UT sar##BITS(UT v, uint32_t n) {                                                 \
        uint32_t k = n & 31u;                                                                      \
        if (k >= BITS)                                                                             \
            k = BITS - 1;                                                                          \
        return (UT)((int32_t)(ST)v >> k);                                                          \
    }                                                                                              \
    static inline UT shl##BITS##_f(X86 *c, UT v, uint32_t n) {                                     \
        uint32_t cnt = n & 31u;                                                                    \
        UT r;                                                                                      \
        if (!cnt)                                                                                  \
            return v;                                                                              \
        if (cnt <= BITS)                                                                           \
            c->eflags_cf = (uint32_t)(((uint32_t)v >> (BITS - cnt)) & 1u);                         \
        else                                                                                       \
            c->eflags_cf = 0;                                                                      \
        r = (UT)(cnt >= BITS ? 0u : ((uint32_t)v << cnt));                                         \
        c->eflags_of = ((uint32_t)(r >> (BITS - 1)) & 1u) ^ c->eflags_cf;                          \
        c->eflags_zf = (r == 0);                                                                   \
        c->eflags_sf = (uint32_t)(r >> (BITS - 1)) & 1u;                                           \
        c->eflags_pf = parity8((uint32_t)r);                                                       \
        return r;                                                                                  \
    }                                                                                              \
    static inline UT shr##BITS##_f(X86 *c, UT v, uint32_t n) {                                     \
        uint32_t cnt = n & 31u;                                                                    \
        UT r;                                                                                      \
        if (!cnt)                                                                                  \
            return v;                                                                              \
        c->eflags_cf = cnt <= BITS ? (uint32_t)(((uint32_t)v >> (cnt - 1)) & 1u) : 0u;             \
        r = (UT)(cnt >= BITS ? 0u : ((uint32_t)v >> cnt));                                         \
        c->eflags_of = (uint32_t)(v >> (BITS - 1)) & 1u;                                           \
        c->eflags_zf = (r == 0);                                                                   \
        c->eflags_sf = (uint32_t)(r >> (BITS - 1)) & 1u;                                           \
        c->eflags_pf = parity8((uint32_t)r);                                                       \
        return r;                                                                                  \
    }                                                                                              \
    static inline UT sar##BITS##_f(X86 *c, UT v, uint32_t n) {                                     \
        uint32_t cnt = n & 31u;                                                                    \
        UT r;                                                                                      \
        if (!cnt)                                                                                  \
            return v;                                                                              \
        if (cnt >= BITS)                                                                           \
            cnt = BITS;                                                                            \
        c->eflags_cf = (uint32_t)(((int32_t)(ST)v >> (cnt - 1)) & 1);                              \
        r = (UT)((int32_t)(ST)v >> (cnt == BITS ? BITS - 1 : cnt));                                \
        c->eflags_of = 0;                                                                          \
        c->eflags_zf = (r == 0);                                                                   \
        c->eflags_sf = (uint32_t)(r >> (BITS - 1)) & 1u;                                           \
        c->eflags_pf = parity8((uint32_t)r);                                                       \
        return r;                                                                                  \
    }                                                                                              \
    static inline UT rol##BITS(UT v, uint32_t n) {                                                 \
        uint32_t cnt = (n & 31u) % BITS, w = (uint32_t)v;                                          \
        return cnt ? (UT)((w << cnt) | (w >> (BITS - cnt))) : v;                                   \
    }                                                                                              \
    static inline UT ror##BITS(UT v, uint32_t n) {                                                 \
        uint32_t cnt = (n & 31u) % BITS, w = (uint32_t)v;                                          \
        return cnt ? (UT)((w >> cnt) | (w << (BITS - cnt))) : v;                                   \
    }                                                                                              \
    static inline UT rol##BITS##_f(X86 *c, UT v, uint32_t n) {                                     \
        UT r;                                                                                      \
        if (!(n & 31u))                                                                            \
            return v;                                                                              \
        r = rol##BITS(v, n);                                                                       \
        c->eflags_cf = (uint32_t)r & 1u;                                                           \
        c->eflags_of = ((uint32_t)(r >> (BITS - 1)) & 1u) ^ c->eflags_cf;                          \
        return r;                                                                                  \
    }                                                                                              \
    static inline UT ror##BITS##_f(X86 *c, UT v, uint32_t n) {                                     \
        UT r;                                                                                      \
        if (!(n & 31u))                                                                            \
            return v;                                                                              \
        r = ror##BITS(v, n);                                                                       \
        c->eflags_cf = (uint32_t)(r >> (BITS - 1)) & 1u;                                           \
        c->eflags_of = c->eflags_cf ^ ((uint32_t)(r >> (BITS - 2)) & 1u);                          \
        return r;                                                                                  \
    }                                                                                              \
    static inline UT rcl##BITS##_f(X86 *c, UT v, uint32_t n) {                                     \
        uint32_t cnt = (n & 31u) % (BITS + 1), i;                                                  \
        for (i = 0; i < cnt; i++) {                                                                \
            uint32_t hi = (uint32_t)(v >> (BITS - 1)) & 1u;                                        \
            v = (UT)((v << 1) | c->eflags_cf);                                                     \
            c->eflags_cf = hi;                                                                     \
        }                                                                                          \
        if (cnt)                                                                                   \
            c->eflags_of = ((uint32_t)(v >> (BITS - 1)) & 1u) ^ c->eflags_cf;                      \
        return v;                                                                                  \
    }                                                                                              \
    static inline UT rcr##BITS##_f(X86 *c, UT v, uint32_t n) {                                     \
        uint32_t cnt = (n & 31u) % (BITS + 1), i;                                                  \
        if (cnt)                                                                                   \
            c->eflags_of = ((uint32_t)(v >> (BITS - 1)) & 1u) ^ c->eflags_cf;                      \
        for (i = 0; i < cnt; i++) {                                                                \
            uint32_t lo = (uint32_t)v & 1u;                                                        \
            v = (UT)(((uint32_t)v >> 1) | (c->eflags_cf << (BITS - 1)));                           \
            c->eflags_cf = lo;                                                                     \
        }                                                                                          \
        return v;                                                                                  \
    }
SHIFT_OPS(8, uint8_t, int8_t)
SHIFT_OPS(16, uint16_t, int16_t)
SHIFT_OPS(32, uint32_t, int32_t)
#undef SHIFT_OPS

/* SHLD/SHRD (32-bit forms only; the corpus has no 16-bit double shift). */
static inline uint32_t shld32_f(X86 *c, uint32_t d, uint32_t s, uint32_t n) {
    uint32_t cnt = n & 31u, r;
    if (!cnt)
        return d;
    r = (d << cnt) | (s >> (32 - cnt));
    c->eflags_cf = (d >> (32 - cnt)) & 1u;
    c->eflags_of = ((r >> 31) & 1u) ^ c->eflags_cf;
    c->eflags_zf = (r == 0);
    c->eflags_sf = (r >> 31) & 1u;
    c->eflags_pf = parity8(r);
    return r;
}
static inline uint32_t shrd32_f(X86 *c, uint32_t d, uint32_t s, uint32_t n) {
    uint32_t cnt = n & 31u, r;
    if (!cnt)
        return d;
    r = (d >> cnt) | (s << (32 - cnt));
    c->eflags_cf = (d >> (cnt - 1)) & 1u;
    c->eflags_of = ((r >> 31) & 1u) ^ ((d >> 31) & 1u);
    c->eflags_zf = (r == 0);
    c->eflags_sf = (r >> 31) & 1u;
    c->eflags_pf = parity8(r);
    return r;
}

/* ------------------------------------------------------ multiply / divide */

static inline void mul8(X86 *c, uint8_t s) {
    uint32_t p = (uint32_t)(uint8_t)c->r[R_EAX] * s;
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | (p & 0xffffu);
    c->eflags_cf = c->eflags_of = ((p >> 8) != 0);
}
static inline void mul16(X86 *c, uint16_t s) {
    uint32_t p = (uint32_t)(uint16_t)c->r[R_EAX] * s;
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | (p & 0xffffu);
    c->r[R_EDX] = (c->r[R_EDX] & 0xffff0000u) | ((p >> 16) & 0xffffu);
    c->eflags_cf = c->eflags_of = ((p >> 16) != 0);
}
static inline void mul32(X86 *c, uint32_t s) {
    uint64_t p = (uint64_t)c->r[R_EAX] * s;
    c->r[R_EAX] = (uint32_t)p;
    c->r[R_EDX] = (uint32_t)(p >> 32);
    c->eflags_cf = c->eflags_of = (c->r[R_EDX] != 0);
}

static inline void imul8(X86 *c, uint8_t s) {
    int32_t p = (int32_t)(int8_t)c->r[R_EAX] * (int8_t)s;
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | ((uint32_t)p & 0xffffu);
    c->eflags_cf = c->eflags_of = (p != (int8_t)p);
}
static inline void imul16(X86 *c, uint16_t s) {
    int32_t p = (int32_t)(int16_t)c->r[R_EAX] * (int16_t)s;
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | ((uint32_t)p & 0xffffu);
    c->r[R_EDX] = (c->r[R_EDX] & 0xffff0000u) | (((uint32_t)p >> 16) & 0xffffu);
    c->eflags_cf = c->eflags_of = (p != (int16_t)p);
}
static inline void imul32(X86 *c, uint32_t s) {
    int64_t p = (int64_t)(int32_t)c->r[R_EAX] * (int32_t)s;
    c->r[R_EAX] = (uint32_t)p;
    c->r[R_EDX] = (uint32_t)((uint64_t)p >> 32);
    c->eflags_cf = c->eflags_of = (p != (int32_t)p);
}

/* Two/three-operand IMUL: value is the truncated product; CF/OF on overflow. */
/* The low half of a signed product is the low half of the unsigned product;
 * computing it unsigned keeps the wraparound defined. */
static inline uint32_t imul2_32(uint32_t a, uint32_t b) {
    return a * b;
}
static inline uint32_t imul2_32_f(X86 *c, uint32_t a, uint32_t b) {
    int64_t p = (int64_t)(int32_t)a * (int32_t)b;
    c->eflags_cf = c->eflags_of = (p != (int32_t)p);
    return (uint32_t)p;
}
static inline uint16_t imul2_16(uint16_t a, uint16_t b) {
    return (uint16_t)((uint32_t)a * b);
}
static inline uint16_t imul2_16_f(X86 *c, uint16_t a, uint16_t b) {
    int32_t p = (int32_t)(int16_t)a * (int16_t)b;
    c->eflags_cf = c->eflags_of = (p != (int16_t)p);
    return (uint16_t)p;
}

static inline void div8(X86 *c, uint8_t s, uint32_t at) {
    uint32_t n = (uint16_t)c->r[R_EAX], q;
    if (!s) {
        recomp_div_error(c, at);
        return;
    }
    q = n / s;
    if (q > 0xffu) {
        recomp_div_error(c, at);
        return;
    }
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | (q & 0xffu) | ((n % s) << 8);
}
static inline void div16(X86 *c, uint16_t s, uint32_t at) {
    uint32_t n = ((uint32_t)(uint16_t)c->r[R_EDX] << 16) | (uint16_t)c->r[R_EAX], q;
    if (!s) {
        recomp_div_error(c, at);
        return;
    }
    q = n / s;
    if (q > 0xffffu) {
        recomp_div_error(c, at);
        return;
    }
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | q;
    c->r[R_EDX] = (c->r[R_EDX] & 0xffff0000u) | (n % s);
}
static inline void div32(X86 *c, uint32_t s, uint32_t at) {
    uint64_t n = ((uint64_t)c->r[R_EDX] << 32) | c->r[R_EAX], q;
    if (!s) {
        recomp_div_error(c, at);
        return;
    }
    q = n / s;
    if (q > 0xffffffffu) {
        recomp_div_error(c, at);
        return;
    }
    c->r[R_EAX] = (uint32_t)q;
    c->r[R_EDX] = (uint32_t)(n % s);
}

static inline void idiv8(X86 *c, uint8_t s, uint32_t at) {
    int32_t n = (int16_t)c->r[R_EAX], q;
    if (!s) {
        recomp_div_error(c, at);
        return;
    }
    if ((int8_t)s == -1 && n == INT32_MIN) {
        recomp_div_error(c, at);
        return;
    }
    q = n / (int8_t)s;
    if (q != (int8_t)q) {
        recomp_div_error(c, at);
        return;
    }
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | ((uint32_t)q & 0xffu) |
                  (((uint32_t)(n % (int8_t)s) & 0xffu) << 8);
}
static inline void idiv16(X86 *c, uint16_t s, uint32_t at) {
    int32_t n = (int32_t)(((uint32_t)(uint16_t)c->r[R_EDX] << 16) | (uint16_t)c->r[R_EAX]), q;
    if (!s) {
        recomp_div_error(c, at);
        return;
    }
    if ((int16_t)s == -1 && n == INT32_MIN) {
        recomp_div_error(c, at);
        return;
    }
    q = n / (int16_t)s;
    if (q != (int16_t)q) {
        recomp_div_error(c, at);
        return;
    }
    c->r[R_EAX] = (c->r[R_EAX] & 0xffff0000u) | ((uint32_t)q & 0xffffu);
    c->r[R_EDX] = (c->r[R_EDX] & 0xffff0000u) | ((uint32_t)(n % (int16_t)s) & 0xffffu);
}
static inline void idiv32(X86 *c, uint32_t s, uint32_t at) {
    int64_t n = (int64_t)(((uint64_t)c->r[R_EDX] << 32) | c->r[R_EAX]), q;
    if (!s) {
        recomp_div_error(c, at);
        return;
    }
    if ((int32_t)s == -1 && n == INT64_MIN) {
        recomp_div_error(c, at);
        return;
    }
    q = n / (int32_t)s;
    if (q != (int32_t)q) {
        recomp_div_error(c, at);
        return;
    }
    c->r[R_EAX] = (uint32_t)q;
    c->r[R_EDX] = (uint32_t)(n % (int32_t)s);
}

/* -------------------------------------------------------------- bit scan */

/* With a zero source the destination is architecturally undefined; unicorn,
 * like AMD, leaves it unchanged, so the old value is passed in. */
static inline uint32_t bsr32_f(X86 *c, uint32_t old, uint32_t v) {
    c->eflags_zf = (v == 0);
    return v ? 31u - (uint32_t)__builtin_clz(v) : old;
}
static inline uint32_t bsf32_f(X86 *c, uint32_t old, uint32_t v) {
    c->eflags_zf = (v == 0);
    return v ? (uint32_t)__builtin_ctz(v) : old;
}

/* ---------------------------------------------------------- push and pop */

static inline void x86_pushad(X86 *c) {
    uint32_t sp = c->r[R_ESP];
    c->r[R_ESP] -= 32;
    wr32(sp - 4, c->r[R_EAX]);
    wr32(sp - 8, c->r[R_ECX]);
    wr32(sp - 12, c->r[R_EDX]);
    wr32(sp - 16, c->r[R_EBX]);
    wr32(sp - 20, sp);
    wr32(sp - 24, c->r[R_EBP]);
    wr32(sp - 28, c->r[R_ESI]);
    wr32(sp - 32, c->r[R_EDI]);
}
static inline void x86_popad(X86 *c) {
    uint32_t sp = c->r[R_ESP];
    c->r[R_EDI] = rd32(sp + 0);
    c->r[R_ESI] = rd32(sp + 4);
    c->r[R_EBP] = rd32(sp + 8); /* skip saved ESP at sp+12 */
    c->r[R_EBX] = rd32(sp + 16);
    c->r[R_EDX] = rd32(sp + 20);
    c->r[R_ECX] = rd32(sp + 24);
    c->r[R_EAX] = rd32(sp + 28);
    c->r[R_ESP] = sp + 32;
}

static inline void recomp_pushfd(X86 *c) {
    c->r[R_ESP] -= 4;
    wr32(c->r[R_ESP], x86_get_eflags(c));
}
static inline void recomp_popfd(X86 *c) {
    x86_set_eflags(c, rd32(c->r[R_ESP]));
    c->r[R_ESP] += 4;
}
static inline void recomp_pushf(X86 *c) {
    c->r[R_ESP] -= 2;
    wr16(c->r[R_ESP], (uint16_t)x86_get_eflags(c));
}
static inline void recomp_popf(X86 *c) {
    x86_set_eflags(c, (x86_get_eflags(c) & 0xffff0000u) | rd16(c->r[R_ESP]));
    c->r[R_ESP] += 2;
}
static inline void recomp_insn_in(X86 *c, uint32_t port, int size, uint32_t *dst, unsigned shift,
                                  uint32_t mask) {
    uint32_t v = recomp_in(c, port, size);
    *dst = (*dst & ~(mask << shift)) | ((v & mask) << shift);
}
static inline void recomp_insn_out(X86 *c, uint32_t port, const uint32_t *src, unsigned shift,
                                   uint32_t mask, int size) {
    recomp_out(c, port, (*src >> shift) & mask, size);
}

/* ---------------------------------------------------------- string ops
 * DF selects the step direction.  The REP forms use ECX as the counter.
 */
/* A forward REP MOVS as one memmove: both spans inside the arena above the
 * null page, no store hook armed, and no destination starting inside the
 * source, where the element loop would replicate a pattern instead. */
static inline int rep_movs_bulk(X86 *c, uint32_t size) {
    const uint64_t bytes = (uint64_t)c->r[R_ECX] * size;
    const uint32_t d = c->r[R_EDI], s = c->r[R_ESI];
    if (c->eflags_df || (RECOMP_STORE_HOOKS && g_store_hook != 0) || d < GUEST_NULL_LIMIT ||
        s < GUEST_NULL_LIMIT || d + bytes > GUEST_SIZE || s + bytes > GUEST_SIZE ||
        (d > s && d < s + bytes))
        return 0;
    memmove(g_mem + d, g_mem + s, (size_t)bytes);
    c->r[R_EDI] = d + (uint32_t)bytes;
    c->r[R_ESI] = s + (uint32_t)bytes;
    c->r[R_ECX] = 0;
    return 1;
}
#define STRING_OPS(SUF, BITS, UT, RD, WR)                                                          \
    static inline void stos##SUF(X86 *c) {                                                         \
        WR(c->r[R_EDI], (UT)c->r[R_EAX]);                                                          \
        c->r[R_EDI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
    }                                                                                              \
    static inline void rep_stos##SUF(X86 *c) {                                                     \
        while (c->r[R_ECX]) {                                                                      \
            stos##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
        }                                                                                          \
    }                                                                                              \
    static inline void movs##SUF(X86 *c) {                                                         \
        WR(c->r[R_EDI], RD(c->r[R_ESI]));                                                          \
        c->r[R_EDI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
        c->r[R_ESI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
    }                                                                                              \
    static inline void rep_movs##SUF(X86 *c) {                                                     \
        if (rep_movs_bulk(c, BITS / 8))                                                            \
            return;                                                                                \
        while (c->r[R_ECX]) {                                                                      \
            movs##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
        }                                                                                          \
    }                                                                                              \
    /* The port string forms. A user-mode guest that reaches one has been      \
     * misdecoded - Windows would fault - so they exist to be translatable      \
     * rather than useful: the port shims answer, and the pointer and count     \
     * advance exactly as the other string forms do. */                   \
    static inline void ins##SUF(X86 *c) {                                                          \
        WR(c->r[R_EDI], (UT)recomp_in(c, c->r[R_EDX] & 0xffffu, BITS / 8));                        \
        c->r[R_EDI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
    }                                                                                              \
    static inline void rep_ins##SUF(X86 *c) {                                                      \
        while (c->r[R_ECX]) {                                                                      \
            ins##SUF(c);                                                                           \
            c->r[R_ECX]--;                                                                         \
        }                                                                                          \
    }                                                                                              \
    static inline void outs##SUF(X86 *c) {                                                         \
        recomp_out(c, c->r[R_EDX] & 0xffffu, RD(c->r[R_ESI]), BITS / 8);                           \
        c->r[R_ESI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
    }                                                                                              \
    static inline void rep_outs##SUF(X86 *c) {                                                     \
        while (c->r[R_ECX]) {                                                                      \
            outs##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
        }                                                                                          \
    }                                                                                              \
    static inline void lods##SUF(X86 *c) {                                                         \
        UT v = RD(c->r[R_ESI]);                                                                    \
        if (BITS == 32)                                                                            \
            c->r[R_EAX] = (uint32_t)v;                                                             \
        else                                                                                       \
            c->r[R_EAX] = (c->r[R_EAX] & ~(uint32_t)((1ull << BITS) - 1)) |                        \
                          ((uint32_t)v & (uint32_t)((1ull << BITS) - 1));                          \
        c->r[R_ESI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
    }                                                                                              \
    static inline void rep_lods##SUF(X86 *c) {                                                     \
        while (c->r[R_ECX]) {                                                                      \
            lods##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
        }                                                                                          \
    }
STRING_OPS(b, 8, uint8_t, rd8, wr8)
STRING_OPS(w, 16, uint16_t, rd16, wr16)
STRING_OPS(d, 32, uint32_t, rd32, wr32)
#undef STRING_OPS

/* SCAS/CMPS set the flags of a SUB.  Defined out of line below the generic
 * SUB flag helper. */
static inline void x86_sub_flags8(X86 *c, uint32_t a, uint32_t b);
static inline void x86_sub_flags16(X86 *c, uint32_t a, uint32_t b);
static inline void x86_sub_flags32(X86 *c, uint32_t a, uint32_t b);

static inline void x86_sub_flags8(X86 *c, uint32_t a, uint32_t b) {
    uint32_t r = (a - b) & 0xffu;
    x86_cc_drop(c);
    c->eflags_cf = ((a & 0xffu) < (b & 0xffu));
    c->eflags_of = (((a ^ b) & (a ^ r)) >> 7) & 1u;
    c->eflags_af = ((a ^ b ^ r) >> 4) & 1u;
    c->eflags_zf = (r == 0);
    c->eflags_sf = (r >> 7) & 1u;
    c->eflags_pf = parity8(r);
}
static inline void x86_sub_flags16(X86 *c, uint32_t a, uint32_t b) {
    uint32_t r = (a - b) & 0xffffu;
    x86_cc_drop(c);
    c->eflags_cf = ((a & 0xffffu) < (b & 0xffffu));
    c->eflags_of = (((a ^ b) & (a ^ r)) >> 15) & 1u;
    c->eflags_af = ((a ^ b ^ r) >> 4) & 1u;
    c->eflags_zf = (r == 0);
    c->eflags_sf = (r >> 15) & 1u;
    c->eflags_pf = parity8(r);
}
static inline void x86_sub_flags32(X86 *c, uint32_t a, uint32_t b) {
    uint32_t r = a - b;
    x86_cc_drop(c);
    c->eflags_cf = (a < b);
    c->eflags_of = (((a ^ b) & (a ^ r)) >> 31) & 1u;
    c->eflags_af = ((a ^ b ^ r) >> 4) & 1u;
    c->eflags_zf = (r == 0);
    c->eflags_sf = (r >> 31) & 1u;
    c->eflags_pf = parity8(r);
}

static inline void scasb(X86 *c) {
    x86_sub_flags8(c, c->r[R_EAX] & 0xffu, rd8(c->r[R_EDI]));
    c->r[R_EDI] += c->eflags_df ? (uint32_t)-1 : 1u;
}
static inline void repne_scasb(X86 *c) {
    while (c->r[R_ECX]) {
        scasb(c);
        c->r[R_ECX]--;
        if (c->eflags_zf)
            break;
    }
}
static inline void repe_scasb(X86 *c) {
    while (c->r[R_ECX]) {
        scasb(c);
        c->r[R_ECX]--;
        if (!c->eflags_zf)
            break;
    }
}
static inline void cmpsb(X86 *c) {
    x86_sub_flags8(c, rd8(c->r[R_ESI]), rd8(c->r[R_EDI]));
    c->r[R_EDI] += c->eflags_df ? (uint32_t)-1 : 1u;
    c->r[R_ESI] += c->eflags_df ? (uint32_t)-1 : 1u;
}
static inline void repe_cmpsb(X86 *c) {
    while (c->r[R_ECX]) {
        cmpsb(c);
        c->r[R_ECX]--;
        if (!c->eflags_zf)
            break;
    }
}
static inline void repne_cmpsb(X86 *c) {
    while (c->r[R_ECX]) {
        cmpsb(c);
        c->r[R_ECX]--;
        if (c->eflags_zf)
            break;
    }
}

/* The same at word and dword width.  A REP-prefixed compare with ECX = 0
 * runs nothing and leaves every flag as it found it, like the byte forms. */
#define X86_CMPS_SCAS(SUF, BITS, MASK)                                                             \
    static inline void scas##SUF(X86 *c) {                                                         \
        x86_sub_flags##BITS(c, c->r[R_EAX] & (MASK), rd##BITS(c->r[R_EDI]));                       \
        c->r[R_EDI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
    }                                                                                              \
    static inline void cmps##SUF(X86 *c) {                                                         \
        x86_sub_flags##BITS(c, rd##BITS(c->r[R_ESI]), rd##BITS(c->r[R_EDI]));                      \
        c->r[R_EDI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
        c->r[R_ESI] += c->eflags_df ? (uint32_t)-(BITS / 8) : (uint32_t)(BITS / 8);                \
    }                                                                                              \
    static inline void repe_scas##SUF(X86 *c) {                                                    \
        while (c->r[R_ECX]) {                                                                      \
            scas##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
            if (!c->eflags_zf)                                                                     \
                break;                                                                             \
        }                                                                                          \
    }                                                                                              \
    static inline void repne_scas##SUF(X86 *c) {                                                   \
        while (c->r[R_ECX]) {                                                                      \
            scas##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
            if (c->eflags_zf)                                                                      \
                break;                                                                             \
        }                                                                                          \
    }                                                                                              \
    static inline void repe_cmps##SUF(X86 *c) {                                                    \
        while (c->r[R_ECX]) {                                                                      \
            cmps##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
            if (!c->eflags_zf)                                                                     \
                break;                                                                             \
        }                                                                                          \
    }                                                                                              \
    static inline void repne_cmps##SUF(X86 *c) {                                                   \
        while (c->r[R_ECX]) {                                                                      \
            cmps##SUF(c);                                                                          \
            c->r[R_ECX]--;                                                                         \
            if (c->eflags_zf)                                                                      \
                break;                                                                             \
        }                                                                                          \
    }
X86_CMPS_SCAS(w, 16, 0xffffu)
X86_CMPS_SCAS(d, 32, 0xffffffffu)
#undef X86_CMPS_SCAS

/* -------------------------------------------------------------- x87 -----
 * st[] holds doubles.  ST(i) is st[(fpu_top + i) & 7]; a push predecrements
 * fpu_top.  Status-word bits: C0=8 C1=9 C2=10 TOP=11..13 C3=14.
 */
#define ST(c, i) ((c)->st[((c)->fpu_top + (unsigned)(i)) & 7u])

#define FTAG_VALID 0u
#define FTAG_ZERO 1u
#define FTAG_SPECIAL 2u /* NaN, infinity, denormal, unsupported */
#define FTAG_EMPTY 3u

/* The tag describes the register as x87 sees it, which is the 80-bit value.
 * A subnormal double is a NORMAL extended - the extended exponent range
 * reaches 2^-16382, far below 2^-1074 - so it tags valid, not special.  An
 * 80-bit subnormal cannot occur at all while the registers are doubles. */
RECOMP_HOT_INLINE unsigned ftag_classify(double v) {
    /* The runtime already represents x87 registers as IEEE binary64. Read
     * the representation without aliasing or floating-point operations:
     * tagging every FPU write must not call libm or quiet a signaling NaN. */
    uint64_t bits;
    memcpy(&bits, &v, sizeof bits);
    bits &= UINT64_C(0x7fffffffffffffff);
    if (!bits)
        return FTAG_ZERO;
    if (bits >= UINT64_C(0x7ff0000000000000))
        return FTAG_SPECIAL;
    return FTAG_VALID; /* NORMAL and SUBNORMAL */
}

RECOMP_HOT_INLINE unsigned ftag_of(const X86 *c, unsigned phys) {
    return (c->fpu_tag >> (2u * (phys & 7u))) & 3u;
}
RECOMP_HOT_INLINE void ftag_put(X86 *c, unsigned phys, unsigned tag) {
    unsigned sh = 2u * (phys & 7u);
    c->fpu_tag = (uint16_t)((c->fpu_tag & ~(3u << sh)) | (tag << sh));
}
/* ST(i) is empty (nothing has been pushed into it since the last FINIT). */
RECOMP_HOT_INLINE unsigned fempty(const X86 *c, unsigned i) {
    return ftag_of(c, (c->fpu_top + i) & 7u) == FTAG_EMPTY;
}

RECOMP_HOT_INLINE void fpush(X86 *c, double v) {
    c->fpu_top = (c->fpu_top - 1u) & 7u;
    c->st[c->fpu_top] = v;
    c->st_bits[c->fpu_top] = 0;
    c->st_exact[c->fpu_top] = 0;
    ftag_put(c, c->fpu_top, ftag_classify(v));
}
static inline void fpush_int(X86 *c, int64_t v) {
    fpush(c, (double)v);
    c->st_bits[c->fpu_top] = (uint64_t)v;
    c->st_exact[c->fpu_top] = 1;
}
/* FLD ST(i) captures the source before pushing, including the wraparound
 * case where the destination is the same physical slot. */
static inline void fpush_st(X86 *c, unsigned i) {
    unsigned src = (c->fpu_top + i) & 7u;
    uint64_t bits = c->st_bits[src];
    uint8_t exact = c->st_exact[src];
    unsigned tag = ftag_of(c, src);
    fpush(c, c->st[src]);
    c->st_bits[c->fpu_top] = bits;
    c->st_exact[c->fpu_top] = exact;
    ftag_put(c, c->fpu_top, tag);
}
RECOMP_HOT_INLINE double fpop(X86 *c) {
    double v = c->st[c->fpu_top];
    c->st_exact[c->fpu_top] = 0;
    ftag_put(c, c->fpu_top, FTAG_EMPTY);
    c->fpu_top = (c->fpu_top + 1u) & 7u;
    return v;
}
RECOMP_HOT_INLINE void fdrop(X86 *c) {
    c->st_exact[c->fpu_top] = 0;
    ftag_put(c, c->fpu_top, FTAG_EMPTY);
    c->fpu_top = (c->fpu_top + 1u) & 7u;
}

/* Assign ST(i) and retag it.  Every write to an x87 register goes through
 * this, so an arithmetic result that turns out to be zero, a NaN, an infinity
 * or a denormal leaves the tag word describing what the register now holds
 * rather than what it held before. */
RECOMP_HOT_INLINE void fset(X86 *c, unsigned i, double v) {
    unsigned phys = (c->fpu_top + i) & 7u;
    c->st[phys] = v;
    c->st_bits[phys] = 0;
    c->st_exact[phys] = 0;
    ftag_put(c, phys, ftag_classify(v));
}

/* Register moves preserve the integer representation as well as the tag. */
static inline void fcopy(X86 *c, unsigned dst, unsigned src) {
    unsigned a = (c->fpu_top + dst) & 7u, b = (c->fpu_top + src) & 7u;
    c->st[a] = c->st[b];
    c->st_bits[a] = c->st_bits[b];
    c->st_exact[a] = c->st_exact[b];
    ftag_put(c, a, ftag_of(c, b));
}

/* FXCH: the tags and exact integers travel with the values. */
RECOMP_HOT_INLINE void fxch(X86 *c, unsigned i) {
    unsigned a = c->fpu_top & 7u, b = (c->fpu_top + i) & 7u;
    double v = c->st[a];
    unsigned t = ftag_of(c, a);
    uint64_t bits = c->st_bits[a];
    uint8_t exact = c->st_exact[a];
    c->st[a] = c->st[b];
    c->st_bits[a] = c->st_bits[b];
    c->st_exact[a] = c->st_exact[b];
    ftag_put(c, a, ftag_of(c, b));
    c->st[b] = v;
    c->st_bits[b] = bits;
    c->st_exact[b] = exact;
    ftag_put(c, b, t);
}

/* Game code compares stored float bits, so the NaN a masked invalid x87
 * operation produces has to be one fixed pattern rather than whatever the
 * host happened to carry in.  x87 silicon yields the negative QNaN
 * "indefinite" (0xfff8000000000000, or 0xffc00000 stored as a float) and
 * ARM64 yields a positive one, so normalise to the x86 pattern.  Unicorn's
 * softfloat is not self-consistent about this sign, which is why two of the
 * sweep functions are reported as known NaN-payload divergences. */
static inline double x87_indefinite(void) {
    uint64_t bits = 0xfff8000000000000ull;
    double d;
    memcpy(&d, &bits, 8);
    return d;
}
/* An x87 result that is not affected by precision control: the invalid
 * -operation NaN is still canonicalised, and the invalid flag is raised, but
 * the value keeps the register's full precision.  FRNDINT, the transcendental
 * instructions and FXCH all land here; per the SDM, PC applies only to
 * FADD/FSUB/FMUL/FDIV (and their integer and popping forms) and FSQRT. */
/* x87 raises #Z when a finite non-zero dividend meets a zero divisor.  This
 * is the one exception besides IE that the parity oracle also reports, so
 * modelling it keeps the exemption down to IE alone. */
/* The `_sw` forms take the status word as a scalar and the control word by
 * value. Scalar x87 SSA bodies keep CW/SW in two function locals; passing a
 * whole X86 environment by address made it escape into helpers clang did not
 * always inline, pinning both words to stack slots reloaded around every
 * operation. Every `_sw` helper is always inlined, so the locals stay SSA. */
RECOMP_HOT_INLINE double fdivz_sw(uint16_t *sw, double a, double b) {
    if (b == 0.0 && a == a && !isinf(a) && a != 0.0)
        *sw |= 0x0004u;
    return a / b;
}
RECOMP_HOT_INLINE double fdivz(X86 *c, double a, double b) {
    return fdivz_sw(&c->fpu_sw, a, b);
}

RECOMP_HOT_INLINE double fx87_exact_sw(uint16_t *sw, double r) {
    if (r == r)
        return r;
    *sw |= 0x0001u; /* IE: invalid operation */
    return x87_indefinite();
}
RECOMP_HOT_INLINE double fx87_exact(X86 *c, double r) {
    return fx87_exact_sw(&c->fpu_sw, r);
}

/* An x87 result from one of the basic arithmetic instructions, which do
 * observe the precision-control field.  PC=00 rounds to single; PC=10
 * (53-bit) and PC=11 (64-bit) both land on the double we store, 64-bit
 * approximated by 53 per the plan ruling. */
RECOMP_HOT_INLINE double fx87_sw(uint16_t *sw, uint16_t cw, double r) {
    if (r != r) {
        *sw |= 0x0001u;
        return x87_indefinite();
    }
    if (((cw >> 8) & 3u) == 0u)
        return (double)(float)r;
    return r;
}
RECOMP_HOT_INLINE double fx87(X86 *c, double r) {
    return fx87_sw(&c->fpu_sw, c->fpu_cw, r);
}

/* A signalling NaN has the quiet bit (mantissa MSB) clear. */
RECOMP_HOT_INLINE int is_snan(double v) {
    uint64_t b;
    memcpy(&b, &v, 8);
    return ((b >> 52) & 0x7ffu) == 0x7ffu && (b & 0x000fffffffffffffull) != 0 &&
           ((b >> 51) & 1u) == 0;
}

/* FCOM/FUCOM: C3 C2 C0 = ZF PF CF of the comparison.  They differ only in
 * which NaNs raise the invalid-operation exception: FCOM raises on any NaN,
 * FUCOM only on a signalling one. */
RECOMP_HOT_INLINE void fcom_status(uint16_t *status, double a, double b, int quiet) {
    uint16_t sw = (uint16_t)(*status & (uint16_t)~0x4700u);
    if (isnan(a) || isnan(b)) {
        sw |= 0x4500u; /* unordered: C3 C2 C0 */
        if (!quiet || is_snan(a) || is_snan(b))
            sw |= 0x0001u; /* IE */
    } else if (a < b)
        sw |= 0x0100u; /* C0 */
    else if (a == b)
        sw |= 0x4000u; /* C3 */
    *status = sw;
}
RECOMP_HOT_INLINE void fcom_sw(uint16_t *status, double a, double b) {
    fcom_status(status, a, b, 0);
}
RECOMP_HOT_INLINE void fucom_sw(uint16_t *status, double a, double b) {
    fcom_status(status, a, b, 1);
}
RECOMP_HOT_INLINE void fcom_common(X86 *c, double a, double b, int quiet) {
    fcom_status(&c->fpu_sw, a, b, quiet);
}
RECOMP_HOT_INLINE void fcom(X86 *c, double a, double b) {
    fcom_common(c, a, b, 0);
}
RECOMP_HOT_INLINE void fucom(X86 *c, double a, double b) {
    fcom_common(c, a, b, 1);
}
RECOMP_HOT_INLINE uint16_t fcomi_flags_sw(uint16_t *status, double a, double b, int quiet) {
    uint8_t flags;
    if (isnan(a) || isnan(b)) {
        flags = 0x45u;
        if (!quiet || is_snan(a) || is_snan(b))
            *status |= 0x0001u;
    } else {
        flags = (uint8_t)((a < b) | ((a == b) << 6));
    }
    return flags;
}
/* FCOMI/FUCOMI report into EFLAGS instead of the status word, with the same
 * split over which NaNs are quiet. */
static inline void fcomi_common(X86 *c, double a, double b, int quiet) {
    uint16_t flags = fcomi_flags_sw(&c->fpu_sw, a, b, quiet);
    c->eflags_of = c->eflags_sf = c->eflags_af = 0;
    c->eflags_cf = flags & 1u;
    c->eflags_pf = (flags >> 2) & 1u;
    c->eflags_zf = (flags >> 6) & 1u;
}
static inline void fcomi(X86 *c, double a, double b) {
    fcomi_common(c, a, b, 0);
}
static inline void fucomi(X86 *c, double a, double b) {
    fcomi_common(c, a, b, 1);
}

/* FNSTSW/FSTSW: the architectural status word, with TOP injected. */
static inline uint16_t fstsw(const X86 *c) {
    return (uint16_t)((c->fpu_sw & (uint16_t)~0x3800u) | (uint16_t)(c->fpu_top << 11));
}
/* FXAM.  C1 is the sign bit (of a NaN too), and C3 C2 C0 encode the class:
 *   NaN 001, infinity 011, normal 010, zero 100, empty 101, denormal 110. */
static inline void fxam(X86 *c) {
    uint16_t sw = (uint16_t)(c->fpu_sw & (uint16_t)~0x4700u);
    double v = ST(c, 0);
    if (signbit(v))
        sw |= 0x0200u; /* C1 = sign */
    if (fempty(c, 0))
        sw |= 0x4100u; /* C3 C0 */
    /* Classified as the 80-bit register would be: a subnormal double is a
     * normal extended, so the denormal encoding C3 C2 = 1 1 is unreachable
     * while the registers are doubles. */
    else
        switch (fpclassify(v)) {
        case FP_NAN:
            sw |= 0x0100u;
            break; /* C0 */
        case FP_INFINITE:
            sw |= 0x0500u;
            break; /* C2 C0 */
        case FP_ZERO:
            sw |= 0x4000u;
            break; /* C3 */
        default:
            sw |= 0x0400u;
            break; /* C2, normal */
        }
    c->fpu_sw = sw;
}

/* Round per the control word's RC field (bits 10..11).
 *
 * Every mode is computed explicitly, so the result depends only on fpu_cw and
 * never on the host's ambient rounding mode.  That matters because the guest
 * control word can be restored by paths this header does not control (a
 * _longjmp reinstating a saved X86, for one), which would silently leave an
 * fesetround-based implementation rounding the wrong way.
 *
 * Nearest is roundeven, which ignores the host mode (frintn on ARM64), rather
 * than `floor(v + 0.5)`: that addition itself rounds, and turns the exactly
 * representable 4503599627370497 into ...498.
 */
RECOMP_HOT_INLINE double fround_rc(uint16_t cw, double v) {
    switch ((cw >> 10) & 3u) {
    case 1:
        return floor(v);
    case 2:
        return ceil(v);
    case 3:
        return trunc(v);
    default:
        return __builtin_roundeven(v);
    }
}
static inline double fround_cw(const X86 *c, double v) {
    return fround_rc(c->fpu_cw, v);
}

/* FLDCW.  Nothing outside fpu_cw has to change: rounding reads the field
 * directly.  Arithmetic and float stores are the one place the host's own
 * nearest-even rounding is used for the significand, which is exact whenever
 * RC = 00; this binary only ever leaves RC = 00 across the CRT's ftol, whose
 * window between the two FLDCWs contains a single FISTP and no arithmetic. */
static inline void x87_set_cw(X86 *c, uint16_t cw) {
    c->fpu_cw = cw;
}

/* FINIT/FNINIT: the post-reset state - control word 0x037f (round to nearest,
 * extended precision, every exception masked), status clear, TOP = 0, every
 * register tagged empty.  The register values themselves are left alone, as
 * on hardware; the tags make them unreadable. */
static inline void x87_finit(X86 *c) {
    x87_set_cw(c, 0x037fu);
    c->fpu_sw = 0;
    c->fpu_top = 0;
    c->fpu_tag = 0xffffu;
    memset(c->st_exact, 0, sizeof c->st_exact);
}

/* FNSAVE m108: the 32-bit protected-mode layout.  Control, status and tag
 * words each in their own dword, then the four exception pointers (FIP, FCS
 * with the opcode, FDP, FDS), which this model does not track and writes as
 * zero, then ST(0) through ST(7) as 80-bit values in stack order.  FNSAVE
 * then reinitialises the FPU, which is why the CRT pairs it with FRSTOR. */
/* FNSTENV m28: the control, status and tag words, then the exception
 * pointers this model does not track, written as zero. Every exception is
 * masked afterwards, as on hardware; the register stack is untouched. */
static inline void x87_fnstenv(X86 *c, uint32_t a) {
    wr32(a, c->fpu_cw);
    wr32(a + 4, fstsw(c));
    wr32(a + 8, c->fpu_tag);
    wr32(a + 12, 0);
    wr32(a + 16, 0);
    wr32(a + 20, 0);
    wr32(a + 24, 0);
    x87_set_cw(c, (uint16_t)(c->fpu_cw | 0x3fu));
}

/* FLDENV m28: control word, status word with TOP, tag word. The registers
 * stay in their physical slots, so ST(i) follows the restored TOP. */
static inline void x87_fldenv(X86 *c, uint32_t a) {
    uint16_t sw = rd16(a + 4);
    x87_set_cw(c, rd16(a));
    c->fpu_top = (sw >> 11) & 7u;
    c->fpu_sw = (uint16_t)(sw & (uint16_t)~0x3800u);
    c->fpu_tag = rd16(a + 8);
}

static inline void x87_fnsave(X86 *c, uint32_t a) {
    unsigned i;
    wr32(a, c->fpu_cw);
    wr32(a + 4, fstsw(c));
    wr32(a + 8, c->fpu_tag);
    wr32(a + 12, 0);
    wr32(a + 16, 0);
    wr32(a + 20, 0);
    wr32(a + 24, 0);
    for (i = 0; i < 8; i++)
        wrf80(a + 28 + 10 * i, ST(c, i));
    x87_finit(c);
}

/* FRSTOR m108: the inverse.  TOP comes out of the saved status word and the
 * eight registers go back into the physical slots that TOP implies, so a
 * later ST(i) reads what FNSAVE wrote as ST(i). */
static inline void x87_frstor(X86 *c, uint32_t a) {
    unsigned i;
    uint16_t sw = rd16(a + 4);
    x87_set_cw(c, rd16(a));
    c->fpu_top = (sw >> 11) & 7u;
    c->fpu_sw = (uint16_t)(sw & (uint16_t)~0x3800u);
    for (i = 0; i < 8; i++)
        fset(c, i, rdf80(a + 28 + 10 * i));
    c->fpu_tag = rd16(a + 8);
}

/* FXSAVE m512: control and status words, the abridged tag byte (one bit per
 * physical register, set when in use), the exception pointers this model
 * does not track, MXCSR and MXCSR_MASK, then ST(0) through ST(7) in 16-byte
 * slots and XMM0-7. The mask leaves out DAZ: the modelled CPU, a Pentium III,
 * has none, and recomp_ldmxcsr refuses it. */
static inline void recomp_fxsave(const X86 *c, uint32_t a) {
    unsigned i, tags = 0;
    for (uint32_t offset = 0; offset < 512; offset += 4)
        wr32(a + offset, 0u);
    for (i = 0; i < 8; i++)
        if (((c->fpu_tag >> (2 * i)) & 3u) != 3u)
            tags |= 1u << i;
    wr16(a, c->fpu_cw);
    wr16(a + 2, fstsw(c));
    wr8(a + 4, (uint8_t)tags);
    wr32(a + 0x18, c->mxcsr);
    wr32(a + 0x1c, 0xffbfu);
    for (i = 0; i < 8; i++)
        wrf80(a + 0x20 + 16 * i, ST(c, i));
    for (i = 0; i < 8; i++)
        for (unsigned lane = 0; lane < 4; lane++)
            wr32(a + 0xa0 + 16 * i + 4 * lane, c->xmm[i][lane]);
}

static inline void recomp_stmxcsr(const X86 *c, uint32_t a) {
    wr32(a, c->mxcsr);
}

/* One binary32 ULP toward +infinity (up) or -infinity, bit for bit what
 * nextafterf gives for the inputs the directed-rounding step reaches: finite
 * nonzero values, infinities (stepping FLT_MAX out of an infinity) and zero
 * (stepping to the smallest subnormal of the requested sign). Integer work
 * only: a call here kept every value live across the store in a callee-saved
 * register, costing a 96-byte frame in 0081ae30, while the rare RC != 0
 * correction needs no call at all. */
static inline float fto_float_step(float f, int up) {
    uint32_t b, neg, mag;
    RECOMP_BITCOPY(&b, &f, 4);
    neg = b >> 31;
    mag = b & 0x7fffffffu;
    if (mag == 0) {
        b = up ? 0x00000001u : 0x80000001u;
    } else {
        /* Away from zero is +1 on a positive value stepping up or a negative
         * value stepping down; otherwise toward zero. */
        mag = (up == (neg == 0)) ? mag + 1 : mag - 1;
        b = (neg << 31) | mag;
    }
    RECOMP_BITCOPY(&f, &b, 4);
    return f;
}

/* FST/FSTP to a float, rounded per RC.  The host conversion rounds to
 * nearest; for the directed modes, step one ULP if it went the wrong way. */
RECOMP_HOT_INLINE float fto_float_cw(uint16_t cw, double v) {
    unsigned rc = (cw >> 10) & 3u;
    float f = (float)v;
    /* A float load followed by double widening and float narrowing must quiet
     * an sNaN as the eager host conversion does. Optimizers can otherwise
     * cancel the casts when the double is local, retaining a signaling payload
     * in guest FST memory even though the widened x87 slot is quiet. */
    if (v != v) {
        uint32_t bits;
        memcpy(&bits, &f, sizeof bits);
        bits |= 0x00400000u;
        memcpy(&f, &bits, sizeof f);
        return f;
    }
    if (!RECOMP_UNLIKELY(rc != 0) || isinf(v) || (double)f == v)
        return f;
    /* The host conversion rounded to nearest; step one ULP toward the mode's
     * direction when it went the wrong way. Kept inline: with an out-of-line
     * cold helper (1d72468) as its only new callee, 0081ae30's frame grew
     * 0x70 -> 0xd0 (saving d8/d9) and its in-game self time ~12% over two
     * paired captures, as with the earlier nextafterf call above. */
    {
        double back = (double)f;
        if (rc == 1 && back > v)
            return fto_float_step(f, 0); /* down */
        if (rc == 2 && back < v)
            return fto_float_step(f, 1); /* up */
        if (rc == 3 && ((v > 0 && back > v) || (v < 0 && back < v)))
            return fto_float_step(f, f < 0.0f); /* truncate toward zero */
    }
    return f;
}
static inline float fto_float(const X86 *c, double v) {
    return fto_float_cw(c->fpu_cw, v);
}

/* FIST/FISTP: round per the control word, and store the "integer indefinite"
 * when the value does not fit (NaN, infinity, out of range) exactly as x87
 * does with the invalid-operation exception masked, raising IE. Integer
 * stores use FILD's exact signed value until an arithmetic write. */
#define X86_FIST(bits, lo, in_range)                                                               \
    RECOMP_HOT_INLINE int##bits##_t fto_i##bits##_sw(uint16_t *sw, uint16_t cw, double v) {        \
        double r = fround_rc(cw, v);                                                               \
        if (r >= lo && in_range)                                                                   \
            return (int##bits##_t)r;                                                               \
        *sw |= 0x0001u;                                                                            \
        return INT##bits##_MIN;                                                                    \
    }                                                                                              \
    RECOMP_HOT_INLINE int##bits##_t fist_i##bits##_sw(uint16_t *sw, uint16_t cw, double v,         \
                                                      uint8_t exact, uint64_t raw) {               \
        int64_t e = (int64_t)raw;                                                                  \
        if (exact && e >= INT##bits##_MIN && e <= INT##bits##_MAX)                                 \
            return (int##bits##_t)e;                                                               \
        return fto_i##bits##_sw(sw, cw, v);                                                        \
    }                                                                                              \
    static inline int##bits##_t fto_i##bits(X86 *c, double v) {                                    \
        return fto_i##bits##_sw(&c->fpu_sw, c->fpu_cw, v);                                         \
    }                                                                                              \
    static inline int##bits##_t fist_i##bits(X86 *c) {                                             \
        return fist_i##bits##_sw(&c->fpu_sw, c->fpu_cw, ST(c, 0), c->st_exact[c->fpu_top],         \
                                 c->st_bits[c->fpu_top]);                                          \
    }
X86_FIST(16, -32768.0, r <= 32767.0)
X86_FIST(32, -2147483648.0, r <= 2147483647.0)
X86_FIST(64, -9223372036854775808.0, r < 9223372036854775808.0)
#undef X86_FIST

/* FPREM / FPREM1.
 *
 * Two things the obvious implementation gets wrong.
 *
 * When the operands' exponents differ by 64 or more, x87 does not finish: it
 * performs a PARTIAL reduction, sets C2, and expects the guest to execute the
 * instruction again.  Clearing C2 unconditionally turns that loop into a
 * single pass with a different answer.
 *
 * The quotient bits C0, C3 and C1 are bits 2, 1 and 0 of |Q|, and Q does not
 * fit anywhere: for a = 2^56, b = 3 it is 24019198012642645, and forming it
 * as a double rounds it, so the low bits are lost before they can be read.
 * They are recovered instead from two remainders - `fmod(A, B)` and
 * `fmod(A, 8B)` differ by exactly (Q mod 8) * B - which never builds a large
 * quotient at all.
 */
static inline unsigned fprem_low_bits(double A, double B) {
    double r = fmod(A, B);
    double r8 = fmod(A, 8.0 * B);
    double q = (r8 - r) / B;
    long n = lround(q);
    return (unsigned)(n & 7);
}

static inline void fprem_flags(uint16_t *status, unsigned bits, int incomplete) {
    uint16_t sw = (uint16_t)(*status & (uint16_t)~0x4700u);
    if (incomplete) {
        sw |= 0x0400u; /* C2: reduction not finished */
    } else {
        if (bits & 4)
            sw |= 0x0100u; /* C0 = quotient bit 2 */
        if (bits & 2)
            sw |= 0x4000u; /* C3 = quotient bit 1 */
        if (bits & 1)
            sw |= 0x0200u; /* C1 = quotient bit 0 */
    }
    *status = sw;
}

static inline double fprem_sw(uint16_t *sw, double a, double b, int ieee) {
    double A, B, r;
    int ea, eb;
    if (b == 0.0 || isnan(a) || isnan(b) || isinf(a)) {
        *sw = (uint16_t)((*sw & (uint16_t)~0x4700u) | 0x0001u);
        return x87_indefinite();
    }
    if (a == 0.0 || isinf(b)) { /* already reduced */
        fprem_flags(sw, 0, 0);
        return a;
    }
    A = fabs(a);
    B = fabs(b);
    ea = ilogb(A);
    eb = ilogb(B);
    if (ea - eb >= 64) {
        /* Partial reduction: take out the top of the quotient only, leaving
         * the exponent difference smaller, and tell the guest to come back. */
        double scale = scalbn(B, ea - eb - 32);
        double part = trunc(A / scale);
        r = A - part * scale;
        fprem_flags(sw, 0, 1);
        return a < 0 ? -r : r;
    }
    r = fmod(A, B);
    if (ieee) {
        /* Round the quotient to nearest instead of truncating: shift the
         * remainder into (-B/2, B/2]. */
        /* Compare 2r against B rather than r against B/2.  Halving is not
         * exact at the bottom of the range: for B = 2**-1074 it underflows to
         * zero, every r then reads as a tie, and FPREM1(x, x) came back as -x
         * instead of 0.  Doubling r is safe: r < B, so 2r only overflows when
         * r already exceeds B/2, which is the case that adjusts anyway. */
        double r2 = scalbn(r, 1);
        unsigned bits = fprem_low_bits(A, B);
        if (r2 > B || (r2 == B && (bits & 1))) {
            r -= B;
            bits = (bits + 1) & 7;
        }
        fprem_flags(sw, bits, 0);
        return a < 0 ? -r : r;
    }
    fprem_flags(sw, fprem_low_bits(A, B), 0);
    return a < 0 ? -r : r;
}
static inline double fprem_common(X86 *c, double a, double b, int ieee) {
    return fprem_sw(&c->fpu_sw, a, b, ieee);
}

/* FSCALE: ST(0) *= 2 ** trunc(ST(1)) */
static inline double fscale(double a, double b) {
    return ldexp(a, (int)trunc(b));
}

/* ----------------------------------------------------------------- MMX ---
 * The MMn registers, as decoded.py's MMX_BINARY/MMX_SHIFT emit them. Each
 * helper takes and returns a packed 64-bit register; the lane width is the
 * caller's, so one helper serves every element size of an operation. */
static inline uint64_t mmx_mask(unsigned bits) {
    return bits >= 64 ? ~(uint64_t)0 : (((uint64_t)1 << bits) - 1);
}
static inline uint64_t mmx_lane(uint64_t v, unsigned bits, unsigned i) {
    return (v >> (bits * i)) & mmx_mask(bits);
}
static inline int64_t mmx_slane(uint64_t v, unsigned bits, unsigned i) {
    uint64_t x = mmx_lane(v, bits, i);
    if (bits < 64 && (x >> (bits - 1)) & 1)
        x |= ~mmx_mask(bits);
    return (int64_t)x;
}
static inline uint64_t mmx_put(uint64_t acc, unsigned bits, unsigned i, uint64_t x) {
    uint64_t m = mmx_mask(bits);
    return (acc & ~(m << (bits * i))) | ((x & m) << (bits * i));
}
static inline int64_t mmx_sat_s(int64_t x, unsigned bits) {
    int64_t hi = ((int64_t)1 << (bits - 1)) - 1, lo = -hi - 1;
    return x > hi ? hi : x < lo ? lo : x;
}
static inline int64_t mmx_sat_u(int64_t x, unsigned bits) {
    int64_t hi = ((int64_t)1 << bits) - 1;
    return x > hi ? hi : x < 0 ? 0 : x;
}
#define MMX_LANES(bits) (64u / (bits))
static inline uint64_t mmx_padd(uint64_t a, uint64_t b, unsigned bits) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 9)
    if (bits == 16) {
        typedef uint16_t lanes4 __attribute__((vector_size(8)));
        lanes4 x, y;
        RECOMP_BITCOPY(&x, &a, 8);
        RECOMP_BITCOPY(&y, &b, 8);
        x += y;
        RECOMP_BITCOPY(&a, &x, 8);
        return a;
    }
#endif
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i, mmx_lane(a, bits, i) + mmx_lane(b, bits, i));
    return r;
}
static inline uint64_t mmx_psub(uint64_t a, uint64_t b, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i, mmx_lane(a, bits, i) - mmx_lane(b, bits, i));
    return r;
}
static inline uint64_t mmx_padds(uint64_t a, uint64_t b, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i,
                    (uint64_t)mmx_sat_s(mmx_slane(a, bits, i) + mmx_slane(b, bits, i), bits));
    return r;
}
static inline uint64_t mmx_psubs(uint64_t a, uint64_t b, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i,
                    (uint64_t)mmx_sat_s(mmx_slane(a, bits, i) - mmx_slane(b, bits, i), bits));
    return r;
}
static inline uint64_t mmx_paddus(uint64_t a, uint64_t b, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i,
                    (uint64_t)mmx_sat_u(
                        (int64_t)mmx_lane(a, bits, i) + (int64_t)mmx_lane(b, bits, i), bits));
    return r;
}
static inline uint64_t mmx_psubus(uint64_t a, uint64_t b, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i,
                    (uint64_t)mmx_sat_u(
                        (int64_t)mmx_lane(a, bits, i) - (int64_t)mmx_lane(b, bits, i), bits));
    return r;
}
static inline uint64_t mmx_pmullw(uint64_t a, uint64_t b) {
    uint64_t r = 0;
    for (unsigned i = 0; i < 4; i++)
        r = mmx_put(r, 16, i, (uint64_t)(mmx_slane(a, 16, i) * mmx_slane(b, 16, i)));
    return r;
}
static inline uint64_t mmx_pmulhw(uint64_t a, uint64_t b) {
    uint64_t r = 0;
    for (unsigned i = 0; i < 4; i++)
        r = mmx_put(r, 16, i, (uint64_t)((mmx_slane(a, 16, i) * mmx_slane(b, 16, i)) >> 16));
    return r;
}
static inline uint64_t mmx_pmaddwd(uint64_t a, uint64_t b) {
    uint64_t r = 0;
    for (unsigned i = 0; i < 2; i++) {
        int64_t s = mmx_slane(a, 16, 2 * i) * mmx_slane(b, 16, 2 * i) +
                    mmx_slane(a, 16, 2 * i + 1) * mmx_slane(b, 16, 2 * i + 1);
        r = mmx_put(r, 32, i, (uint64_t)s);
    }
    return r;
}
static inline uint64_t mmx_pcmpeq(uint64_t a, uint64_t b, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i, mmx_lane(a, bits, i) == mmx_lane(b, bits, i) ? ~(uint64_t)0 : 0);
    return r;
}
static inline uint64_t mmx_pcmpgt(uint64_t a, uint64_t b, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i, mmx_slane(a, bits, i) > mmx_slane(b, bits, i) ? ~(uint64_t)0 : 0);
    return r;
}
/* Packing: the destination's lanes narrow into the low half, the source's
 * into the high half. */
static inline uint64_t mmx_pack(uint64_t a, uint64_t b, unsigned from, int is_signed) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 9)
    if (from == 16 && !is_signed) {
        typedef int16_t lanes8 __attribute__((vector_size(16)));
        typedef uint8_t bytes8 __attribute__((vector_size(8)));
        uint64_t packed[2] = {a, b};
        lanes8 x;
        RECOMP_BITCOPY(&x, packed, 16);
        x &= ~(x < 0);
        lanes8 high = x > 255;
        x = (x & ~high) | (high & 255);
        bytes8 narrowed = __builtin_convertvector(x, bytes8);
        RECOMP_BITCOPY(&a, &narrowed, 8);
        return a;
    }
#endif
    unsigned to = from / 2, n = 64 / from;
    uint64_t r = 0;
    for (unsigned i = 0; i < n; i++) {
        int64_t x = mmx_slane(a, from, i), y = mmx_slane(b, from, i);
        r = mmx_put(r, to, i, (uint64_t)(is_signed ? mmx_sat_s(x, to) : mmx_sat_u(x, to)));
        r = mmx_put(r, to, n + i, (uint64_t)(is_signed ? mmx_sat_s(y, to) : mmx_sat_u(y, to)));
    }
    return r;
}
static inline uint64_t mmx_packsswb(uint64_t a, uint64_t b) {
    return mmx_pack(a, b, 16, 1);
}
static inline uint64_t mmx_packssdw(uint64_t a, uint64_t b) {
    return mmx_pack(a, b, 32, 1);
}
static inline uint64_t mmx_packuswb(uint64_t a, uint64_t b) {
    return mmx_pack(a, b, 16, 0);
}
/* Unpacking interleaves the low (or high) half of each operand, destination
 * lane first. */
static inline uint64_t mmx_punpck(uint64_t a, uint64_t b, unsigned bits, unsigned half) {
    unsigned n = 64 / bits / 2;
    uint64_t r = 0;
    for (unsigned i = 0; i < n; i++) {
        r = mmx_put(r, bits, 2 * i, mmx_lane(a, bits, half + i));
        r = mmx_put(r, bits, 2 * i + 1, mmx_lane(b, bits, half + i));
    }
    return r;
}
static inline uint64_t mmx_punpckl(uint64_t a, uint64_t b, unsigned bits) {
    return mmx_punpck(a, b, bits, 0);
}
static inline uint64_t mmx_punpckh(uint64_t a, uint64_t b, unsigned bits) {
    return mmx_punpck(a, b, bits, 64 / bits / 2);
}
static inline uint64_t mmx_pshufw(uint64_t a, unsigned order) {
    uint64_t r = 0;
    for (unsigned i = 0; i < 4; i++)
        r = mmx_put(r, 16, i, mmx_lane(a, 16, (order >> (2 * i)) & 3));
    return r;
}
static inline uint64_t mmx_pand(uint64_t a, uint64_t b) {
    return a & b;
}
static inline uint64_t mmx_pandn(uint64_t a, uint64_t b) {
    return ~a & b;
}
static inline uint64_t mmx_por(uint64_t a, uint64_t b) {
    return a | b;
}
static inline uint64_t mmx_pxor(uint64_t a, uint64_t b) {
    return a ^ b;
}
/* Shifts take their count from all 64 bits of the source; a count past the
 * lane width clears the lane, or fills it with the sign for PSRA. */
static inline uint64_t mmx_psll(uint64_t a, uint64_t count, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i, count >= bits ? 0 : mmx_lane(a, bits, i) << count);
    return r;
}
static inline uint64_t mmx_psrl(uint64_t a, uint64_t count, unsigned bits) {
    uint64_t r = 0;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i, count >= bits ? 0 : mmx_lane(a, bits, i) >> count);
    return r;
}
static inline uint64_t mmx_psra(uint64_t a, uint64_t count, unsigned bits) {
    uint64_t r = 0;
    unsigned c = count >= bits ? bits - 1 : (unsigned)count;
    for (unsigned i = 0; i < MMX_LANES(bits); i++)
        r = mmx_put(r, bits, i, (uint64_t)(mmx_slane(a, bits, i) >> c));
    return r;
}
static inline uint64_t mmx_pminub(uint64_t a, uint64_t b) {
    uint64_t r = 0;
    for (unsigned i = 0; i < 8; i++) {
        uint64_t x = mmx_lane(a, 8, i), y = mmx_lane(b, 8, i);
        r = mmx_put(r, 8, i, x < y ? x : y);
    }
    return r;
}
static inline uint64_t mmx_pmaxub(uint64_t a, uint64_t b) {
    uint64_t r = 0;
    for (unsigned i = 0; i < 8; i++) {
        uint64_t x = mmx_lane(a, 8, i), y = mmx_lane(b, 8, i);
        r = mmx_put(r, 8, i, x > y ? x : y);
    }
    return r;
}

static inline uint64_t xmm_low64(const uint32_t a[4]) {
    return (uint64_t)a[0] | ((uint64_t)a[1] << 32);
}
static inline uint64_t xmm_high64(const uint32_t a[4]) {
    return (uint64_t)a[2] | ((uint64_t)a[3] << 32);
}
static inline void xmm_put64(uint32_t a[4], unsigned half, uint64_t v) {
    a[2 * half] = (uint32_t)v;
    a[2 * half + 1] = (uint32_t)(v >> 32);
}
#define RECOMP_XMM_BIN(name, fn)                                                                   \
    static inline void name(uint32_t d[4], const uint32_t s[4], unsigned bits) {                   \
        uint64_t dl = xmm_low64(d), dh = xmm_high64(d);                                            \
        uint64_t sl = xmm_low64(s), sh = xmm_high64(s);                                            \
        xmm_put64(d, 0, fn(dl, sl, bits));                                                         \
        xmm_put64(d, 1, fn(dh, sh, bits));                                                         \
    }
#define RECOMP_XMM_BIN64(name, fn)                                                                 \
    static inline void name(uint32_t d[4], const uint32_t s[4]) {                                  \
        uint64_t dl = xmm_low64(d), dh = xmm_high64(d);                                            \
        uint64_t sl = xmm_low64(s), sh = xmm_high64(s);                                            \
        xmm_put64(d, 0, fn(dl, sl));                                                               \
        xmm_put64(d, 1, fn(dh, sh));                                                               \
    }
RECOMP_XMM_BIN(xmm_padd, mmx_padd)
RECOMP_XMM_BIN(xmm_psub, mmx_psub)
RECOMP_XMM_BIN(xmm_padds, mmx_padds)
RECOMP_XMM_BIN(xmm_psubs, mmx_psubs)
RECOMP_XMM_BIN(xmm_paddus, mmx_paddus)
RECOMP_XMM_BIN(xmm_psubus, mmx_psubus)
RECOMP_XMM_BIN(xmm_pcmpeq, mmx_pcmpeq)
RECOMP_XMM_BIN(xmm_pcmpgt, mmx_pcmpgt)
RECOMP_XMM_BIN64(xmm_pmullw, mmx_pmullw)
RECOMP_XMM_BIN64(xmm_pmulhw, mmx_pmulhw)
RECOMP_XMM_BIN64(xmm_pmaddwd, mmx_pmaddwd)
RECOMP_XMM_BIN64(xmm_pminub, mmx_pminub)
RECOMP_XMM_BIN64(xmm_pmaxub, mmx_pmaxub)
RECOMP_XMM_BIN64(xmm_pand, mmx_pand)
RECOMP_XMM_BIN64(xmm_pandn, mmx_pandn)
RECOMP_XMM_BIN64(xmm_por, mmx_por)
RECOMP_XMM_BIN64(xmm_pxor, mmx_pxor)
#undef RECOMP_XMM_BIN
#undef RECOMP_XMM_BIN64
#define RECOMP_XMM_SHIFT(name, fn)                                                                 \
    static inline void name(uint32_t d[4], uint64_t count, unsigned bits) {                        \
        uint64_t dl = xmm_low64(d), dh = xmm_high64(d);                                            \
        xmm_put64(d, 0, fn(dl, count, bits));                                                      \
        xmm_put64(d, 1, fn(dh, count, bits));                                                      \
    }
RECOMP_XMM_SHIFT(xmm_psll, mmx_psll)
RECOMP_XMM_SHIFT(xmm_psrl, mmx_psrl)
RECOMP_XMM_SHIFT(xmm_psra, mmx_psra)
#undef RECOMP_XMM_SHIFT
static inline void xmm_punpck(uint32_t d[4], const uint32_t s[4], unsigned bits, int high) {
    uint64_t dl = xmm_low64(d), dh = xmm_high64(d);
    uint64_t sl = xmm_low64(s), sh = xmm_high64(s);
    uint64_t lo, hi;
    if (bits == 64) {
        lo = high ? dh : dl;
        hi = high ? sh : sl;
    } else if (high) {
        lo = mmx_punpckl(dh, sh, bits);
        hi = mmx_punpckh(dh, sh, bits);
    } else {
        lo = mmx_punpckl(dl, sl, bits);
        hi = mmx_punpckh(dl, sl, bits);
    }
    xmm_put64(d, 0, lo);
    xmm_put64(d, 1, hi);
}
static inline void xmm_pack(uint32_t d[4], const uint32_t s[4], unsigned from, int is_signed) {
    uint64_t dl = xmm_low64(d), dh = xmm_high64(d);
    uint64_t sl = xmm_low64(s), sh = xmm_high64(s);
    xmm_put64(d, 0, mmx_pack(dl, dh, from, is_signed));
    xmm_put64(d, 1, mmx_pack(sl, sh, from, is_signed));
}
static inline void xmm_psrldq(uint32_t d[4], unsigned count) {
    uint8_t in[16];
    memcpy(in, d, 16);
    memset(d, 0, 16);
    if (count < 16)
        for (unsigned i = 0; i < 16 - count; i++)
            ((uint8_t *)d)[i] = in[i + count];
}

/* ----------------------------------------------------------------- misc */

/* FXTRACT: ST(0) becomes its exponent, and its significand is pushed. */
static inline double fxtract_exponent(double v) {
    if (v == 0.0 || isnan(v) || isinf(v))
        return v == 0.0 ? -(double)INFINITY : v;
    return (double)ilogb(v);
}
static inline double fxtract_significand(double v) {
    if (v == 0.0 || isnan(v) || isinf(v))
        return v;
    return scalbn(v, -ilogb(v)); /* in [1, 2) */
}

static inline void xlat(X86 *c) {
    c->r[R_EAX] = (c->r[R_EAX] & 0xffffff00u) | rd8(c->r[R_EBX] + (c->r[R_EAX] & 0xffu));
}
static inline uint32_t bswap32(uint32_t v) {
    return __builtin_bswap32(v);
}

#ifdef __cplusplus
}
#endif
#endif /* RECOMP_X86_H */
