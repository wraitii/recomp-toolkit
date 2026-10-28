#include "native.h"
#include "memory.h"
#include "platform/os.h"
#include <cstring>

// SporeApp.exe 3.1.0.22, 0x00d67cd0: update of creature action 0x02d852e6 (table 0x01583f30).
// BUGFIX(original): a null target mid-swing reaches 0x00d65d23 and faults; see ../README.md, Combat target.
extern "C" void fn_00d67cd0(X86 *c);

extern "C" void spore_combat_update(X86 *c) {
    static const bool original = [] {
        const char *value = os_getenv("RECOMP_SPORE_COMBAT_ORIGINAL");
        return value && value[0] == '1' && value[1] == '\0';
    }();
    const uint32_t sp = c->r[R_ESP];
    const uint32_t creature = rd32(sp + 4), state = rd32(sp + 24);
    const uint32_t swing = rd32(state + 0x30);
    if (original || rd32(creature + 0xe7c) || !swing) {
        fn_00d67cd0(c);
        return;
    }
    memset(c->xmm[0], 0, sizeof c->xmm[0]);
    wr32(state + 0x1c, 0);
    c->r[R_ECX] = creature;
    recomp_guest_call(c, 0x00c12560u, &swing, 1);
    x86_cc_settle(c);
    c->eflags_cf = c->eflags_of = c->eflags_af = c->eflags_sf = 0;
    c->eflags_zf = c->eflags_pf = 1;
    c->r[R_EAX] = 0;
    c->r[R_ECX] = rd32(sp + 20);
    c->eip = rd32(sp);
    c->r[R_ESP] = sp + 4;
    recomp_return(c);
}
