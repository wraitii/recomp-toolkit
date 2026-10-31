#pragma once
#include "x86.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RecompJitBytes {
    uint32_t address, size;
    const uint8_t *bytes;
} RecompJitBytes;

#define RECOMP_JIT_RESUME_SAFE 1u

typedef struct RecompJitModule {
    uint32_t version, cpu_size, image_base;
    const char *image_sha256;
    uint32_t entry_count, range_count, return_count;
    const RecompDispatchEntry *entries;
    const RecompJitBytes *ranges;
    const uint32_t *returns;
} RecompJitModule;

int recomp_jit_compile(const char *input, const char *directory);
int recomp_jit_call(X86 *c, uint32_t target);
int recomp_jit_resume(X86 *c, uint32_t target, uint32_t continuation, uint32_t *next);
void recomp_jit_jump(X86 *c, uint32_t target);

#ifdef __cplusplus
}
#endif
