#include "globalops_native.h"

// 0x004b4a50 is MSVC 6's _alloca_probe: it lowers the caller's ESP by EAX and
// leaves through JMP EAX to the return address rather than RET. Translated, that
// jump dispatches the caller's continuation as a separate entry, and the caller
// then resumes its own body a second time on the unwound stack. This performs
// the probe's register effects and returns to the call's continuation.
extern "C" void globalops_alloca_probe(X86 *c) {
    const uint32_t esp = c->r[R_ESP], ret = rd32(esp);
    c->r[R_ESP] = esp + 4 - c->r[R_EAX];
    c->r[R_EAX] = ret;
    c->eip = ret;
    recomp_return(c);
}
