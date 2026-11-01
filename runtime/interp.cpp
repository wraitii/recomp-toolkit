#include "interp.h"

#if RECOMP_PCODE_FALLBACK
#include "pcode.h"
#include "pcode_abi.h"
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
        if (mode && *mode && strcmp(mode, "pcode") != 0 && strcmp(mode, "baseline") != 0) {
            snprintf(g_error, sizeof g_error, "unsupported RECOMP_INTERP=%s", mode);
            fprintf(stderr, "[recomp] %s\n", g_error);
            abort();
        }
        return true;
    }();
    if (!enabled) {
        snprintf(g_error, sizeof g_error, "interpreter disabled by RECOMP_INTERP=off");
        fprintf(stderr, "[recomp] interp: %s (target %08x)\n", g_error, target);
        return 0;
    }
#if RECOMP_PCODE_FALLBACK
    static const bool baseline = [] {
        const char *mode = getenv("RECOMP_INTERP");
        return mode && strcmp(mode, "baseline") == 0;
    }();
    if ((baseline ? pcode_baseline_call(c, target) : pcode_interp_call(c, target)) == 1)
        return 1;
    snprintf(g_error, sizeof g_error, "%s", pcode_interp_last_error());
    fprintf(stderr, "[recomp] %s: routine %08x stopped: %s\n",
            baseline ? "baseline" : "pcode interp", target, g_error);
    fflush(stderr);
    rp_jit_shutdown();
    abort();
#else
    snprintf(g_error, sizeof g_error,
             "p-code interpreter is not linked; rebuild without --no-pcode");
    fprintf(stderr, "[recomp] interp: %s (target %08x)\n", g_error, target);
    return 0;
#endif
}

extern "C" const char *interp_last_error(void) {
    return g_error;
}
