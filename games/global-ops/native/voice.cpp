#include "globalops_native.h"

// object.lto 0x10259f30 (thiscall, bit rate) opens the Miles "ASI codec" that
// turns .RAW voice into the shipped Mssv24.asi format, then calls the returned
// ASI_stream_open without checking that a provider was found; its caller
// 0x1025a769 ignores the result. The kit's Miles has no ASI providers and no
// audio input. DIVERGENCE(original): record the bit rate as the original does at
// 0x10259f6f and report success without opening codec streams, so voice chat is
// unavailable.
// cshell.dll 0x10098a20 (thiscall, bit rate) is the client's copy, called from
// 0x10012a16, which ignores the result. Its teardown 0x10098c50 closes the codec
// streams whenever 0x10128671, set when audio input opened, is non-zero.
// DIVERGENCE(original): clear that flag and report success, so the client treats
// voice as never initialised.
extern "C" void globalops_client_voice_codec_init(X86 *c) {
    const uint32_t esp = c->r[R_ESP];
    wr8(0x10128671, 0);
    c->r[R_EAX] = 1;
    c->eip = rd32(esp);
    c->r[R_ESP] = esp + 8;
    recomp_return(c);
}

extern "C" void globalops_voice_codec_init(X86 *c) {
    const uint32_t esp = c->r[R_ESP];
    wr32(0x100c08d0, rd32(esp + 4));
    c->r[R_EAX] = 1;
    c->eip = rd32(esp);
    c->r[R_ESP] = esp + 8;
    recomp_return(c);
}
