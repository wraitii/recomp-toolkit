#include "native.h"
#include "memory.h"
#include "platform/os.h"

// SporeApp.exe 3.1.0.22, 0x00fce6c0..0x00fce729: table-based MMX pixel conversion.
// Ghidra bytes, caller 0x00fceb10 and texture upload 0x00fd9490 establish the path.
// Purpose, original switch and fault-state limitation: ../README.md, Video conversion.
extern "C" void fn_00fce6c0(X86 *c);

extern "C" void spore_video_convert_row(X86 *c) {
    static const bool original = [] {
        const char *value = os_getenv("RECOMP_SPORE_VIDEO_ORIGINAL");
        return value && value[0] == '1' && value[1] == '\0';
    }();
    if (original) {
        fn_00fce6c0(c);
        return;
    }

    x86_cc_settle(c);
    const uint32_t sp = c->r[R_ESP];
    wr32(sp - 4, c->r[R_EBX]);
    wr32(sp - 8, c->r[R_ESI]);
    wr32(sp - 12, c->r[R_EDI]);
    wr32(sp - 16, c->r[R_EBP]);
    uint32_t y = rd32(sp + 4), u = rd32(sp + 8), v = rd32(sp + 12);
    uint32_t dst = rd32(sp + 16);
    const uint32_t end = rd32(sp + 20);
    uint64_t mm0, mm1, mm2, mm3;
    do {
        const uint8_t ui = rd8(u++), vi = rd8(v++);
        mm2 = rd64(0x015b2018u + uint32_t(ui) * 8);
        mm3 = rd64(0x015b2818u + uint32_t(vi) * 8);
        const uint8_t y0 = rd8(y);
        mm2 = mmx_padd(mm2, mm3, 16);
        const uint8_t y1 = rd8(y + 1);
        y += 2;
        mm0 = rd64(0x015b1818u + uint32_t(y0) * 8);
        mm1 = rd64(0x015b1818u + uint32_t(y1) * 8);
        mm0 = mmx_padd(mm0, mm2, 16);
        mm1 = mmx_padd(mm1, mm2, 16);
        dst += 8;
        mm0 = mmx_packuswb(mm0, mm1);
        wr64(dst - 8, mm0);
    } while (dst != end);

    c->mm[0] = mm0;
    c->mm[1] = mm1;
    c->mm[2] = mm2;
    c->mm[3] = mm3;
    c->r[R_EAX] = 0;
    c->r[R_ECX] = y;
    c->r[R_EDX] = u;
    c->eflags_cf = c->eflags_of = c->eflags_af = c->eflags_sf = 0;
    c->eflags_zf = c->eflags_pf = 1;
    c->r[R_EBP] = rd32(sp - 16);
    c->r[R_EDI] = rd32(sp - 12);
    c->r[R_ESI] = rd32(sp - 8);
    c->r[R_EBX] = rd32(sp - 4);
    c->eip = rd32(sp);
    c->r[R_ESP] = sp + 4;
    recomp_return(c);
}
