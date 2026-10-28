#pragma once
#include "x86.h"
void bw_display_start(X86 *c);
void bw_display_init(X86 *c);
void bw_display_layout(X86 *c);
#define FN_00515690 bw_display_layout
void bw_display_callback(X86 *c);
#define FN_00642d80 bw_display_start
#define FN_00514f50 bw_display_init
#define FN_005140c0 bw_display_callback
