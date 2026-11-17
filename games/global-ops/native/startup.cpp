#include "globalops_native.h"
#include "imports.h"
#include "memory.h"

extern "C" void fn_0040b880(X86 *c);

// The 4.0 update replaced a call at 0x0040ba69 with an indirect call through
// mss32.dll's own import table: the IAT slot 0x00518328 holds
// _AIL_digital_handle_release@4, and 0x3a5f0 bytes past it, in the shipped
// mss32.dll, is that DLL's import of KERNEL32!GetEnvironmentVariableA. The kit's
// Miles shim has no import table, so for the duration of the call the slot
// points at a cell holding the GetEnvironmentVariableA trampoline, preserving
// the original Windows behaviour.
extern "C" void globalops_startup(X86 *c) {
    const uint32_t slot = 0x00518328, offset = 0x3a5f0;
    static uint32_t cell = 0;
    if (!cell) {
        cell = heap_alloc(4, true);
        wr32(cell, imports_resolve("KERNEL32.dll", "GetEnvironmentVariableA"));
    }
    const uint32_t saved = rd32(slot);
    wr32(slot, cell - offset);
    fn_0040b880(c);
    wr32(slot, saved);
}
