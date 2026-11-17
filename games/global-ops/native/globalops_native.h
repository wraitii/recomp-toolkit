#ifndef GLOBALOPS_NATIVE_H
#define GLOBALOPS_NATIVE_H
#include "x86.h"
#ifdef __cplusplus
extern "C" {
#endif
void globalops_startup(X86 *c);
void globalops_pick_display_mode(X86 *c);
void globalops_alloca_probe(X86 *c);
void globalops_voice_codec_init(X86 *c);
void globalops_client_voice_codec_init(X86 *c);
#ifdef __cplusplus
}
#endif
#define FN_10098a20 globalops_client_voice_codec_init
#define FN_004b4a50 globalops_alloca_probe
#define FN_10259f30 globalops_voice_codec_init
#define FN_0040b880 globalops_startup
#define FN_004c33b0 globalops_pick_display_mode
#endif
