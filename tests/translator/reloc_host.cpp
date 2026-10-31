#include "runtime/guest.h"
#include "runtime/loader.h"
#include "runtime/reloc.h"

bool loader_in_image(uint32_t addr) {
    return addr >= 0x00400000u && addr < 0x00600000u;
}

void log_msg(int, const char *, ...) {}

extern "C" int reloc_test_call(uint32_t target, uint32_t eax, uint32_t ecx, int cached,
                               uint32_t *result) {
    X86 cpu = {};
    cpu.r[R_EAX] = eax;
    cpu.r[R_ECX] = ecx;
    cpu.r[R_ESP] = 0x0efff000u;
    cpu.checkpoint_countdown = 1023;
    x87_finit(&cpu);
    wr32(cpu.r[R_ESP], 0x0deac000u);
    int matched = cached ? recomp_reloc_cached(&cpu, target) : recomp_reloc_call(&cpu, target);
    result[0] = cpu.r[R_EAX];
    result[1] = cpu.r[R_ESP];
    return matched;
}
