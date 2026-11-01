#pragma once
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int (*RpJitCompile)(const char *input, const char *directory);
void rp_jit_note(uint32_t target, int is_call, uint32_t base, const char *sha256,
                 RpJitCompile compile);
char *rp_jit_take(void);
void rp_jit_free_path(char *path);
void rp_jit_module_status(const char *directory, const char *status, const char *reason,
                          const uint32_t *addresses, const uint64_t *calls,
                          const uint64_t *handoffs, int resume_safe, uint32_t count);
void rp_jit_shutdown(void);

#ifdef __cplusplus
}
#endif
