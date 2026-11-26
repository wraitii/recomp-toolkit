// dplayx.cpp - DirectPlay creation entry points (dplayx.dll).
//
// This build has no networking. The creation functions are stdcall, so they
// need real shims: a logging-only stub pops nothing and would leave the
// caller's arguments on the stack. Each reports DPERR_UNAVAILABLE and writes
// a null interface pointer, so a game takes its own "DirectPlay unavailable"
// path. Games commonly import these by ordinal, so both spellings are listed.
#include "dx.h"
#include "../runtime/guest.h"
#include "../runtime/imports.h"

#include <iterator>

namespace {

const uint32_t DPERR_UNAVAILABLE = 0x8877021cu;

void clear_out(X86 *c, int index) {
    uint32_t out = arg(c, index);
    if (out)
        wr32(out, 0);
}

// DirectPlayCreate(LPGUID, LPDIRECTPLAY *, IUnknown *)
void DirectPlayCreate(X86 *c) {
    clear_out(c, 1);
    imports_decline(c, "dplayx.dll!DirectPlayCreate", DPERR_UNAVAILABLE);
}

// DirectPlayLobbyCreate[AW](LPGUID, LPDIRECTPLAYLOBBY *, IUnknown *, LPVOID, DWORD)
void DirectPlayLobbyCreate(X86 *c) {
    clear_out(c, 1);
    imports_decline(c, "dplayx.dll!DirectPlayLobbyCreate", DPERR_UNAVAILABLE);
}

// dpnhpast.dll, the DirectPlay NAT helper, first shipped with DirectX 8.1.
// Games probe for DirectX 8.1 by loading it, so it must be loadable; creating
// a helper is declined like the rest of DirectPlay.
// DirectPlayNATHelpCreate(const GUID *, void **)
void DirectPlayNATHelpCreate(X86 *c) {
    clear_out(c, 1);
    imports_decline(c, "dpnhpast.dll!DirectPlayNATHelpCreate", DPERR_UNAVAILABLE);
}

// DirectPlayEnumerate[AW](LPDPENUMDPCALLBACK, LPVOID): enumerate nothing.
void DirectPlayEnumerate(X86 *c) {
    set_eax(c, 0);
}

const ImportShim g_dplayx_shims[] = {
    {"dplayx.dll", "DirectPlayCreate", 3, DirectPlayCreate},
    {"dplayx.dll", "ord1", 3, DirectPlayCreate},
    {"dplayx.dll", "DirectPlayEnumerateA", 2, DirectPlayEnumerate},
    {"dplayx.dll", "ord2", 2, DirectPlayEnumerate},
    {"dplayx.dll", "DirectPlayEnumerateW", 2, DirectPlayEnumerate},
    {"dplayx.dll", "ord3", 2, DirectPlayEnumerate},
    {"dplayx.dll", "DirectPlayLobbyCreateA", 5, DirectPlayLobbyCreate},
    {"dplayx.dll", "ord4", 5, DirectPlayLobbyCreate},
    {"dplayx.dll", "DirectPlayLobbyCreateW", 5, DirectPlayLobbyCreate},
    {"dplayx.dll", "ord5", 5, DirectPlayLobbyCreate},
    {"dplayx.dll", "DirectPlayEnumerate", 2, DirectPlayEnumerate},
    {"dplayx.dll", "ord9", 2, DirectPlayEnumerate},
    {"dpnhpast.dll", "DirectPlayNATHelpCreate", 2, DirectPlayNATHelpCreate},
};

} // namespace

void dplayx_register() {
    static bool done = false;
    if (done)
        return;
    done = true;
    imports_register(g_dplayx_shims, std::size(g_dplayx_shims));
}
