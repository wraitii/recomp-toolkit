#pragma once
#include "x86.h"

void nfs_carbon_motion_blur(X86 *c);
void nfs_carbon_render_motion_blur(X86 *c);

#define FN_007133c0 nfs_carbon_motion_blur
#define FN_00726430 nfs_carbon_render_motion_blur
