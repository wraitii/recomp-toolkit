#ifndef GTA2_NATIVE_H
#define GTA2_NATIVE_H
#include "x86.h"
#ifdef __cplusplus
extern "C" {
#endif
void gta2_probe_directmusic(X86 *c);
void gta2_open_manager(X86 *c);
#ifdef __cplusplus
}
#endif
#define FN_0044db50 gta2_probe_directmusic
#define FN_004d0820 gta2_open_manager
#endif
