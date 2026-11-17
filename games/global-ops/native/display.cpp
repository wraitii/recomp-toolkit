#include "globalops_native.h"

extern "C" void fn_004c33b0(X86 *c);

// 0x004c33b0 picks the display mode matching the configured resolution and a
// requested bit depth. The shipped default.cfg asks for 16 bits, the kit's
// Direct3D 8 adapter offers only X8R8G8B8 modes, and the renderer's Init
// (0x004b4b90) reports success on every failure path through its shared exit at
// 0x004b505e, leaving a null device. DIVERGENCE(original): a 16-bit request
// selects the 32-bit mode.
extern "C" void globalops_pick_display_mode(X86 *c) {
    const uint32_t depth = c->r[R_ESP] + 8;
    if (rd32(depth) == 16)
        wr32(depth, 32);
    fn_004c33b0(c);
}
