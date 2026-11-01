#pragma once
#include "pcode_abi.h"
#include "x86.h"

#ifdef __cplusplus
extern "C" {
#endif

int pcode_interp_call(X86 *c, uint32_t target);
int pcode_baseline_call(X86 *c, uint32_t target);
const char *pcode_interp_last_error(void);

#ifdef __cplusplus
}
#endif
