#include "interp.h"

#if RECOMP_PCODE_FALLBACK
#include "baseline.h"
#include "jit_abi.h"
#endif

#include <cstdio>
#include <cstdlib>
#include <cstring>

namespace {
thread_local char g_error[512];
}

extern "C" int interp_call(X86 *c, uint32_t target) {
    static const bool enabled = [] {
        const char *mode = getenv("RECOMP_INTERP");
        if (mode && strcmp(mode, "off") == 0)
            return false;
        if (mode && *mode) {
            snprintf(g_error, sizeof g_error, "unsupported RECOMP_INTERP=%s", mode);
            fprintf(stderr, "[recomp] %s\n", g_error);
            abort();
        }
        return true;
    }();
    if (!enabled) {
        snprintf(g_error, sizeof g_error, "fallback disabled by RECOMP_INTERP=off");
        fprintf(stderr, "[recomp] interp: %s (target %08x)\n", g_error, target);
        return 0;
    }
#if RECOMP_PCODE_FALLBACK
    if (baseline_call(c, target) == 1)
        return 1;
    snprintf(g_error, sizeof g_error, "%s", baseline_last_error());
    fprintf(stderr, "[recomp] baseline: routine %08x stopped: %s\n", target, g_error);
    fflush(stderr);
    rp_jit_shutdown();
    abort();
#else
    snprintf(g_error, sizeof g_error,
             "baseline fallback is not linked; rebuild without --no-pcode");
    fprintf(stderr, "[recomp] interp: %s (target %08x)\n", g_error, target);
    return 0;
#endif
}

extern "C" const char *interp_last_error(void) {
    return g_error;
}
