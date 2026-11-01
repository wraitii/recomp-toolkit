#pragma once
#include "x86.h"

#ifdef __cplusplus
extern "C" {
#endif

int baseline_call(X86 *c, uint32_t target);
const char *baseline_last_error(void);

#ifdef __cplusplus
}
#endif
