#include <cstdint>
#include <cstdio>

void dx_report_failure(const char *desc, uint32_t hr, uint32_t caller) {
    if (!(hr & 0x80000000u))
        return;
    fprintf(stderr, "COM failure: %s (0x%08x) at 0x%08x\n", desc ? desc : "unknown", hr, caller);
}
