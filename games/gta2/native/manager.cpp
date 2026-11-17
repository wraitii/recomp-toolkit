#include "gta2_native.h"
#include "guest.h"

// 0x004d0820 is the menu's Options: it minimizes the game, brings GTA2
// Manager forward or ShellExecutes "GTA2 Manager.EXE", and calls the fatal
// error handler when that launch fails. The Manager is a separate program the
// port does not run. DIVERGENCE(original): Options leaves the game in the menu.
extern "C" void gta2_open_manager(X86 *c) {
    log_once("gta2.manager", "gta2: Options opens GTA2 Manager, which is not available; "
                             "the game stays in the menu");
    c->eip = rd32(c->r[R_ESP]);
    c->r[R_ESP] += 4;
    recomp_return(c);
}
