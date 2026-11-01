#define RECOMP_GUEST_MEMORY_OWNER 1 /* defines and maps g_mem */
#include "../platform/os.h"
// interp_tests.cpp - the missing-code fallback (runtime/interp.cpp, runtime/baseline.cpp).
//
//   interp_tests               the checks below
//   interp_tests --run HEX     runs HEX as a routine from a fixed state and
//                              prints the final state
#include "interp.h"
#if RECOMP_PCODE_FALLBACK
#include "baseline.h"
#endif

#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>
#ifndef _WIN32
#include <sys/wait.h>
#include <unistd.h>
#ifdef __APPLE__
#include <mach-o/dyld.h>
#endif
#endif

uint8_t *g_mem;
// This test links the fallback alone, so it owns the definitions the
// runtime would otherwise supply: guest memory, and the dirty-region list
// x86.h's stores record.
RecompDirty g_dirty[RECOMP_DIRTY_SLOTS];
uint32_t g_dirty_count, g_store_hook;
uint32_t g_watch_base, g_watch_len; // the memory watch, never armed here
extern "C" void recomp_watch_hit(uint32_t, uint32_t, uint64_t) {}

// The fallback tests link neither cpu.cpp nor a guest with handlers, so a
// null dereference has nowhere to raise to: report it and stop, which is what
// the real one does when the guest has no handler either.
extern "C" void recomp_null_access(uint32_t addr, int write) {
    fprintf(stderr, "null %s of %08x\n", write ? "write" : "read", addr);
    abort();
}

static int g_checks = 0, g_failures = 0;
#define CHECK(cond)                                                                                \
    do {                                                                                           \
        ++g_checks;                                                                                \
        if (!(cond)) {                                                                             \
            ++g_failures;                                                                          \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);                        \
        }                                                                                          \
    } while (0)

const uint32_t CODE = 0x01000000, HELPER = 0x01001000, DATA = 0x02000000, STACK = 0x0e000000;
const uint32_t NATIVE = 0x00401000, RETURN = 0x00400ff0;
static uint32_t g_native_arg = 0, g_native_calls = 0;

// The generated table's stand-in: NATIVE is a "translated" cdecl function
// that returns its argument plus one; heap addresses go to the baseline.
extern "C" void recomp_call(X86 *c, uint32_t target) {
    if (target == NATIVE) {
        ++g_native_calls;
        g_native_arg = rd32(c->r[R_ESP] + 4);
        c->r[R_EAX] = g_native_arg + 1;
        c->eip = rd32(c->r[R_ESP]);
        c->r[R_ESP] += 4;
        return;
    }
    if (!interp_call(c, target)) {
        fprintf(stderr, "recomp_call %08x: %s\n", target, interp_last_error());
        c->eip = rd32(c->r[R_ESP]);
        c->r[R_ESP] += 4;
        c->r[R_EAX] = 0;
    }
}

// NATIVE is the one "translated" entry the table knows, so a JMP to it is a
// tail call the baseline hands back rather than decoding into.
extern "C" int recomp_is_call_return(uint32_t target) {
    return target == RETURN;
}
extern "C" int recomp_module_is_call_return(uint32_t) {
    return 0;
}
extern "C" void recomp_rdtsc(X86 *) {
    fprintf(stderr, "interp fixture: RDTSC helper unavailable\n");
    abort();
}
extern "C" void recomp_cpuid(X86 *) {
    fprintf(stderr, "interp fixture: CPUID helper unavailable\n");
    abort();
}

extern "C" int recomp_thunk_target_kind(uint32_t target) {
    return target == NATIVE ? 1 : 0;
}

extern "C" void recomp_jump(X86 *c, uint32_t target) {
    recomp_call(c, target);
}

extern "C" void recomp_execution_checkpoint_due(X86 *) {}
extern "C" int discovery_enabled(void) {
    return 0;
}
extern "C" void discovery_note(const char *, uint32_t, uint32_t) {}

static void put(uint32_t at, const std::vector<uint8_t> &b) {
    memcpy(g_mem + at, b.data(), b.size());
}
static void put_call(uint32_t at, uint32_t target) {
    g_mem[at] = 0xe8;
    wr32(at + 1, target - (at + 5));
}

static X86 fresh() {
    X86 c{};
    c.eflags_misc = 0x202;
    c.r[R_ESP] = STACK - 0x100;
    return c;
}

// ---- lazy arithmetic flag descriptor (x86.h) --------------------------
//
// x86_cc_settle materialises a pending ADD/SUB/LOGIC/INC/DEC descriptor. Its
// recipes must match the eager ones the decoded emitter and the baseline
// use, for every operation and operand width. Compare them over random
// operands rather than the fixed register table the baseline fixtures use.
struct EagerFlags {
    uint32_t cf, pf, af, zf, sf, of;
};

static uint32_t width_mask(int bits) {
    return bits == 32 ? 0xffffffffu : (1u << bits) - 1u;
}

// CMP and TEST share the SUB and LOGIC descriptor kinds, so this covers all
// six integer flag producers. Flags an operation does not define keep the
// incoming field, matching the eager publication.
static EagerFlags eager_flags(int op, int bits, uint32_t a, uint32_t b, const EagerFlags &before) {
    const uint32_t m = width_mask(bits), sign = 1u << (bits - 1);
    EagerFlags f = before;
    a &= m;
    b &= m;
    uint32_t r = 0;
    switch (op) {
    case X86_CC_ADD:
        r = (a + b) & m;
        f.cf = r < a;
        f.of = (((a ^ r) & (b ^ r)) >> (bits - 1)) & 1u;
        f.af = ((a ^ b ^ r) >> 4) & 1u;
        f.zf = r == 0;
        f.sf = (r >> (bits - 1)) & 1u;
        f.pf = parity8(r);
        break;
    case X86_CC_SUB:
        r = (a - b) & m;
        f.cf = a < b;
        f.of = (((a ^ b) & (a ^ r)) >> (bits - 1)) & 1u;
        f.af = ((a ^ b ^ r) >> 4) & 1u;
        f.zf = r == 0;
        f.sf = (r >> (bits - 1)) & 1u;
        f.pf = parity8(r);
        break;
    case X86_CC_LOGIC:
        r = a & b;
        f.cf = 0;
        f.of = 0;
        f.af = 0;
        f.zf = r == 0;
        f.sf = (r >> (bits - 1)) & 1u;
        f.pf = parity8(r);
        break;
    case X86_CC_INC:
        r = (a + 1u) & m;
        f.of = r == sign;
        f.af = ((a ^ r) >> 4) & 1u; // CF is preserved.
        f.zf = r == 0;
        f.sf = (r >> (bits - 1)) & 1u;
        f.pf = parity8(r);
        break;
    case X86_CC_DEC:
        r = (a - 1u) & m;
        f.of = r == (sign - 1u);
        f.af = ((a ^ r) >> 4) & 1u; // CF is preserved.
        f.zf = r == 0;
        f.sf = (r >> (bits - 1)) & 1u;
        f.pf = parity8(r);
        break;
    default:
        break;
    }
    return f;
}

static uint32_t g_flag_seed = 0x9e3779b9u;
static uint32_t flag_random(void) {
    g_flag_seed ^= g_flag_seed << 13;
    g_flag_seed ^= g_flag_seed >> 17;
    g_flag_seed ^= g_flag_seed << 5;
    return g_flag_seed;
}

static void test_lazy_flags() {
    static const uint8_t ops[] = {X86_CC_ADD, X86_CC_SUB, X86_CC_LOGIC, X86_CC_INC, X86_CC_DEC};
    static const uint8_t masks[] = {0x3fu, 0x3fu, 0x3fu, 0x3eu, 0x3eu};
    for (int bits = 8; bits <= 32; bits *= 2) {
        for (unsigned oi = 0; oi < sizeof ops / sizeof *ops; ++oi) {
            for (int t = 0; t < 4096; ++t) {
                EagerFlags before = {flag_random() & 1u, flag_random() & 1u, flag_random() & 1u,
                                     flag_random() & 1u, flag_random() & 1u, flag_random() & 1u};
                X86 c;
                memset(&c, 0, sizeof c);
                c.eflags_cf = before.cf;
                c.eflags_pf = before.pf;
                c.eflags_af = before.af;
                c.eflags_zf = before.zf;
                c.eflags_sf = before.sf;
                c.eflags_of = before.of;
                const uint32_t a = flag_random();
                const uint32_t b = flag_random();
                const uint32_t m = width_mask(bits);
                const uint32_t xa = a & m;
                const uint32_t xb = (ops[oi] == X86_CC_INC || ops[oi] == X86_CC_DEC) ? 1u : (b & m);
                c.cc_op = ops[oi];
                c.cc_size = (uint8_t)(bits / 8);
                c.cc_mask = masks[oi];
                c.cc_a = a;
                c.cc_b = (ops[oi] == X86_CC_INC || ops[oi] == X86_CC_DEC) ? 1u : b;
                if (ops[oi] == X86_CC_ADD)
                    c.cc_res = (xa + xb) & m;
                else if (ops[oi] == X86_CC_SUB)
                    c.cc_res = (xa - xb) & m;
                else if (ops[oi] == X86_CC_LOGIC)
                    c.cc_res = xa & xb;
                else if (ops[oi] == X86_CC_INC)
                    c.cc_res = (xa + 1u) & m;
                else
                    c.cc_res = (xa - 1u) & m;
                // Capture the expected result before x86_cc_settle clears the
                // descriptor fields it was built from.
                const EagerFlags want = eager_flags(ops[oi], bits, a, c.cc_b, before);
                x86_cc_settle(&c);
                CHECK(c.cc_op == X86_CC_NONE);
                CHECK(c.eflags_cf == want.cf && c.eflags_pf == want.pf && c.eflags_af == want.af &&
                      c.eflags_zf == want.zf && c.eflags_sf == want.sf && c.eflags_of == want.of);
            }
        }
    }
    // A partial mask writes only the named fields and leaves the rest.
    X86 c;
    memset(&c, 0, sizeof c);
    c.eflags_cf = 1;
    c.eflags_af = 1;
    c.eflags_of = 1;
    c.eflags_sf = 1;
    c.cc_op = X86_CC_SUB;
    c.cc_size = 4;
    c.cc_mask = X86_CCF_ZF;
    c.cc_a = 5;
    c.cc_b = 5;
    c.cc_res = 0;
    x86_cc_settle(&c);
    CHECK(c.eflags_zf == 1 && c.eflags_cf == 1 && c.eflags_af == 1 && c.eflags_of == 1 &&
          c.eflags_sf == 1 && c.eflags_pf == 0);
    // x86_get_eflags settles first, so PUSHFD cannot read a stale field while a
    // descriptor is pending.
    memset(&c, 0, sizeof c);
    c.eflags_misc = 0x202;
    c.cc_op = X86_CC_SUB;
    c.cc_size = 4;
    c.cc_mask = 0x3fu;
    c.cc_a = 5;
    c.cc_b = 7;
    c.cc_res = 5u - 7u;
    const EagerFlags want = eager_flags(X86_CC_SUB, 32, c.cc_a, c.cc_b, EagerFlags{});
    const uint32_t eflags = x86_get_eflags(&c);
    CHECK(c.cc_op == X86_CC_NONE);
    CHECK(((eflags >> 0) & 1u) == want.cf);
    CHECK(((eflags >> 2) & 1u) == want.pf);
    CHECK(((eflags >> 4) & 1u) == want.af);
    CHECK(((eflags >> 6) & 1u) == want.zf);
    CHECK(((eflags >> 7) & 1u) == want.sf);
    CHECK(((eflags >> 11) & 1u) == want.of);
    memset(&c, 0, sizeof c);
    c.eflags_of = c.eflags_sf = 1;
    c.cc_op = X86_CC_ADD;
    c.cc_size = 4;
    c.cc_mask = 0x3fu;
    c.cc_a = 0xffffffffu;
    c.cc_b = 1;
    c.cc_res = 0;
    x86_cc_settle_or_drop(&c, X86_CCF_CF);
    CHECK(c.cc_op == X86_CC_NONE && c.eflags_cf == 1);
    CHECK(c.eflags_of == 1 && c.eflags_sf == 1 && c.eflags_pf == 0 && c.eflags_zf == 0 &&
          c.eflags_af == 0);
    c.cc_op = X86_CC_INC;
    c.cc_size = 4;
    c.cc_mask = 0x3eu;
    c.cc_a = 0xffffffffu;
    c.cc_b = 1;
    c.cc_res = 0;
    x86_cc_settle_or_drop(&c, X86_CCF_CF);
    CHECK(c.cc_op == X86_CC_NONE && c.eflags_cf == 1);
    CHECK(c.eflags_of == 1 && c.eflags_sf == 1 && c.eflags_pf == 0 && c.eflags_zf == 0 &&
          c.eflags_af == 0);
}

// The state test_interp_unicorn.py mirrors: registers from a fixed table,
// ESI and EBX pointing into a data page filled with a pattern.
static int run_hex(const char *hex) {
    std::vector<uint8_t> code;
    for (const char *p = hex; p[0] && p[1]; p += 2)
        code.push_back((uint8_t)strtoul(std::string(p, 2).c_str(), nullptr, 16));
    put(CODE, code);
    for (uint32_t i = 0; i < 0x1000; ++i)
        g_mem[DATA + i] = (uint8_t)(i * 37 + 11);
    X86 c = fresh();
    const uint32_t regs[8] = {0x12345678, 0x80000000, 0x7fffffff,   DATA + 0x800,
                              0,          0xfffffffe, DATA + 0x400, 3};
    for (int i = 0; i < 8; ++i)
        if (i != R_ESP)
            c.r[i] = regs[i];
    c.r[R_ESP] = STACK - 0x100;
    wr32(c.r[R_ESP], RETURN);
    if (!interp_call(&c, CODE)) {
        printf("error %s\n", interp_last_error());
        return 1;
    }
    printf("eax=%08x ecx=%08x edx=%08x ebx=%08x esp=%08x ebp=%08x esi=%08x edi=%08x eip=%08x\n",
           c.r[0], c.r[1], c.r[2], c.r[3], c.r[4], c.r[5], c.r[6], c.r[7], c.eip);
    printf("cf=%u zf=%u sf=%u of=%u\n", c.eflags_cf, c.eflags_zf, c.eflags_sf, c.eflags_of);
    printf("data=");
    for (uint32_t i = 0; i < 0x1000; ++i)
        printf("%02x", g_mem[DATA + i]);
    printf("\n");
    return 0;
}

static X86 run_code(const std::vector<uint8_t> &code) {
    put(CODE, code);
    memset(g_mem + DATA, 0, 0x100);
    X86 c = fresh();
    wr32(c.r[R_ESP] - 4, RETURN);
    c.r[R_ESP] -= 4;
    CHECK(interp_call(&c, CODE) == 1);
    return c;
}

static void test_baseline_boundaries() {
    X86 c = run_code({0xa1, 0x00, 0x00, 0x00, 0x02, // mov eax,[02000000]
                      0x83, 0xc0, 0x05,             // add eax,5
                      0xa3, 0x04, 0x00, 0x00, 0x02, // mov [02000004],eax
                      0xc3});
    CHECK(c.eip == RETURN);
    CHECK(rd32(DATA + 4) == 5);
    c = run_code({0xb0, 0x12, 0xb4, 0x34, 0x66, 0x05, 0x01, 0x01,
                  0xc3}); // mov al,12h; mov ah,34h; add ax,0101h
    CHECK((c.r[R_EAX] & 0xffff) == 0x3513);
    c = run_code({0x85, 0xc0, 0x74, 0x06, 0xb8, 0x01, 0, 0, 0, 0xc3, 0xb8, 0x02, 0, 0, 0,
                  0xc3}); // test eax,eax; je +6; mov eax,1; ret; mov eax,2; ret
    CHECK(c.r[R_EAX] == 2);
    c = run_code({0x68, 0x34, 0x12, 0, 0, 0x58, 0xc3}); // push 1234h; pop eax
    CHECK(c.r[R_EAX] == 0x1234);

    c = run_code({0xb8, 0xff, 0xff, 0xff, 0xff, 0x83, 0xc0, 0x01, 0xc3}); // mov eax,-1; add eax,1
    CHECK(c.r[R_EAX] == 0 && c.eflags_cf == 1 && c.eflags_zf == 1 && c.eflags_of == 0);
    c = run_code(
        {0xb8, 0xff, 0xff, 0xff, 0x7f, 0x83, 0xc0, 0x01, 0xc3}); // mov eax,7fffffffh; add eax,1
    CHECK(c.r[R_EAX] == 0x80000000u && c.eflags_of == 1 && c.eflags_sf == 1 && c.eflags_cf == 0);
    c = run_code({0xb8, 0, 0, 0, 0, 0x83, 0xe8, 0x01, 0xc3}); // mov eax,0; sub eax,1
    CHECK(c.r[R_EAX] == 0xffffffffu && c.eflags_cf == 1 && c.eflags_sf == 1 && c.eflags_of == 0);
    c = run_code({0xb8, 0, 0, 0, 0x80, 0x83, 0xe8, 0x01, 0xc3}); // mov eax,80000000h; sub eax,1
    CHECK(c.r[R_EAX] == 0x7fffffffu && c.eflags_of == 1 && c.eflags_cf == 0);

    c = run_code({0xb8, 0xff, 0x56, 0x34, 0x12, 0x04, 0x01, 0xc3}); // mov eax,123456ffh; add al,1
    CHECK((c.r[R_EAX] & 0xffff) == 0x5600 && c.eflags_cf == 1 && c.eflags_zf == 1);
    c = run_code({0xb8, 0xff, 0xff, 0x34, 0x12, 0x66, 0x05, 0x01, 0x00,
                  0xc3}); // mov eax,1234ffffh; add ax,1
    CHECK((c.r[R_EAX] & 0xffffffffu) == 0x12340000u && c.eflags_cf == 1);
    c = run_code({0xb8, 0x01, 0, 0, 0x80, 0xd1, 0xe0, 0xc3}); // mov eax,80000001h; shl eax,1
    CHECK(c.r[R_EAX] == 2 && c.eflags_cf == 1);
    c = run_code({0xb8, 0x01, 0, 0, 0x80, 0xd1, 0xf8, 0xc3}); // mov eax,80000001h; sar eax,1
    CHECK(c.r[R_EAX] == 0xc0000000u && c.eflags_cf == 1);

    const uint32_t target = CODE + 0x20;
    std::vector<uint8_t> jump = {0xb8, 0,    0,    0,
                                 0,    0xff, 0xe0, 0xc3}; // mov eax,target; jmp eax; ret
    put(CODE, jump);
    wr32(CODE + 1, target);
    put(target, {0xbb, 0x55, 0, 0, 0, 0xc3}); // mov ebx,55h; ret
    c = fresh();
    wr32(c.r[R_ESP] - 4, RETURN);
    c.r[R_ESP] -= 4;
    CHECK(interp_call(&c, CODE) == 1);
    CHECK(c.r[R_EBX] == 0x55 && c.eip == RETURN);

    put(HELPER, {0xb8, 0x07, 0, 0, 0, 0xc3}); // mov eax,7; ret
    put(CODE, {0xe8, 0, 0, 0, 0, 0xc3});      // call HELPER; ret
    wr32(CODE + 1, HELPER - (CODE + 5));
    c = run_code(
        {g_mem[CODE], g_mem[CODE + 1], g_mem[CODE + 2], g_mem[CODE + 3], g_mem[CODE + 4], 0xc3});
    CHECK(c.r[R_EAX] == 7 && c.eip == RETURN);
    g_mem[HELPER + 1] = 9;
    c = fresh();
    wr32(c.r[R_ESP] - 4, RETURN);
    c.r[R_ESP] -= 4;
    CHECK(interp_call(&c, CODE) == 1 && c.r[R_EAX] == 9);

    put(CODE, {0x68, 0x34, 0x12, 0, 0, 0xe8, 0, 0, 0, 0, 0x83, 0xc4, 0x04,
               0xc3}); // push arg; call NATIVE; add esp,4; ret
    wr32(CODE + 6, NATIVE - (CODE + 10));
    c = fresh();
    wr32(c.r[R_ESP] - 4, RETURN);
    c.r[R_ESP] -= 4;
    CHECK(interp_call(&c, CODE) == 1 && c.r[R_EAX] == 0x1235 && c.eip == RETURN);

    put(CODE, {0xe9, 0, 0, 0, 0}); // jmp NATIVE
    wr32(CODE + 1, NATIVE - (CODE + 5));
    c = fresh();
    wr32(c.r[R_ESP], RETURN);
    wr32(c.r[R_ESP] + 4, 0x1234);
    CHECK(interp_call(&c, CODE) == 1 && c.r[R_EAX] == 0x1235 && c.eip == RETURN);
}

#if RECOMP_PCODE_FALLBACK && !defined(_WIN32)
static const char *baseline_stop_needle(const char *name) {
    if (!strcmp(name, "x87"))
        return "null-page";
    if (!strcmp(name, "short"))
        return "lift failed";
    if (!strcmp(name, "fault"))
        return "outside arena";
    if (!strcmp(name, "recursion"))
        return "recursion limit";
    return "";
}

static int baseline_stop(const char *name) {
    const char *needle = baseline_stop_needle(name);
    bool fault_progress = false;
    if (!strcmp(name, "x87"))
        put(CODE, {0xd9, 0x46, 0x04, 0xc3});
    else if (!strcmp(name, "div-zero"))
        put(CODE, {0xb8, 0x64, 0, 0, 0, 0xb9, 0, 0, 0, 0, 0x31, 0xd2, 0xf7, 0xf1, 0xc3});
    else if (!strcmp(name, "idiv-overflow"))
        put(CODE, {0xb8, 0, 0, 0, 0x80, 0xb9, 0xff, 0xff, 0xff, 0xff, 0x99, 0xf7, 0xf9, 0xc3});
    else if (!strcmp(name, "shrd"))
        put(CODE, {0x0f, 0xad, 0xc8, 0xc3});
    else if (!strcmp(name, "short"))
        put(CODE, std::vector<uint8_t>(15, 0x0f));
    else if (!strcmp(name, "fault")) {
        put(CODE, {0xc7, 0x05, 0x00, 0x00, 0x00, 0x02, 0x55, 0, 0, 0, 0xa1, 0xff, 0xff, 0xff, 0xff,
                   0xc3});
        fault_progress = true;
    } else if (!strcmp(name, "recursion"))
        put(CODE, {0xe8, 0xfb, 0xff, 0xff, 0xff});
    else
        return 2;
    X86 c = fresh();
    wr32(c.r[R_ESP], RETURN);
    bool nested = !strcmp(name, "recursion");
    int result = nested ? interp_call(&c, CODE) : baseline_call(&c, CODE);
    if (nested)
        return 1;
    const char *error = baseline_last_error();
    if (error)
        fprintf(stderr, "%s\n", error);
    if (result != -1 || !error || !strstr(error, needle))
        return 1;
    if (fault_progress && rd32(DATA) != 0x55)
        return 1;
    return 0;
}

static void test_baseline_stops() {
    struct Case {
        const char *name, *needle;
    } cases[] = {
        {"x87", "null-page"},
        {"div-zero", "division"},
        {"idiv-overflow", "division"},
        {"shrd", "SHR"},
        {"short", "lift failed"},
        {"fault", "outside arena"},
        {"recursion", "recursion limit"},
    };
    char self[4096];
#ifdef __APPLE__
    uint32_t size = sizeof self - 1;
    ssize_t n = _NSGetExecutablePath(self, &size) ? -1 : (ssize_t)strlen(self);
#else
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
#endif
    if (n <= 0)
        return;
    self[n] = 0;
    for (const Case &item : cases) {
        int pipefd[2];
        if (pipe(pipefd) != 0)
            return;
        pid_t pid = fork();
        if (pid == 0) {
            close(pipefd[0]);
            dup2(pipefd[1], 2);
            close(pipefd[1]);
            execl(self, self, "--baseline-stop", item.name, (char *)nullptr);
            _exit(127);
        }
        close(pipefd[1]);
        char output[512] = {};
        ssize_t used = 0;
        while (used < (ssize_t)sizeof output - 1) {
            ssize_t n = read(pipefd[0], output + used, sizeof output - 1 - used);
            if (n <= 0)
                break;
            used += n;
        }
        close(pipefd[0]);
        int status = 0;
        bool nested = !strcmp(item.name, "recursion");
        bool ok = pid > 0 && waitpid(pid, &status, 0) == pid &&
                  (nested ? WIFSIGNALED(status) && WTERMSIG(status) == SIGABRT
                          : WIFEXITED(status) && WEXITSTATUS(status) == 0) &&
                  strstr(output, item.needle) != nullptr;
        ++g_checks;
        if (!ok) {
            ++g_failures;
            fprintf(stderr, "FAIL baseline stop case %s (status %d): %s\n", item.name, status,
                    output);
        }
    }
}
#endif

#ifndef _WIN32
extern "C" void watchdog(int) {
    const char msg[] = "FAIL interp_tests timed out\n";
    ssize_t ignored = write(2, msg, sizeof msg - 1);
    (void)ignored;
    _exit(1);
}
#endif

int main(int argc, char **argv) {
    g_mem = (uint8_t *)os_vm_reserve(GUEST_SIZE);
    if (!g_mem)
        return 1;
    if (argc == 3 && !strcmp(argv[1], "--run"))
        return run_hex(argv[2]);
#ifndef _WIN32
    signal(SIGALRM, watchdog);
    alarm(30);
#endif
#if RECOMP_PCODE_FALLBACK && !defined(_WIN32)
    if (argc == 3 && !strcmp(argv[1], "--baseline-stop"))
        return baseline_stop(argv[2]);
#endif
#if RECOMP_PCODE_FALLBACK
    test_baseline_boundaries();
#ifndef _WIN32
    test_baseline_stops();
#endif
#endif
    test_lazy_flags();
    printf("interp: %d checks, %d failures\n", g_checks, g_failures);
    return g_failures ? 1 : 0;
}
