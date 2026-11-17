#include "gta2_native.h"

// 0x0044db50 is the last step of the DirectX SDK's GetDXVersion: it loads
// DMUSIC.DLL and creates CLSID_DirectMusic, and records 0x601 at 0x005e96fc when
// that succeeds. Its caller (0x00403575) refuses to start below DirectX 6.1, and
// the game uses DirectMusic nowhere else. The kit has no DirectMusic.
// DIVERGENCE(original): record DirectX 6.1 without the probe.
extern "C" void gta2_probe_directmusic(X86 *c) {
    wr16(0x005e96fc, 0x601);
    c->eip = rd32(c->r[R_ESP]);
    c->r[R_ESP] += 4;
    recomp_return(c);
}
