/* Port tab + live FXAA toggle for Ghost Recon.
 *
 * DIVERGENCE(original): adds a Port options tab and an FXAA checkbox, and
 * filters the finished 3D scene when the checkbox is on. The installed
 * Data/Shell/IKE.RES is never modified: the loader hook reads it, rewrites the
 * bytes in memory and hands the original loader a synthetic guest path aliased
 * to a GENERATED copy on the host temp directory. The toggle drives
 * gr_port_fxaa_mode(), which scene_post feeds to d3d8_scene_boundary.
 * RECOMP_FXAA still seeds the mode and its unsupported-value diagnostic is
 * unchanged. Set RECOMP_PORT_TAB=0 to disable the rewrite and use the original
 * resource. See README.md in this directory for addresses and evidence.
 */
#include "port_tab.h"
#include "native_seam.h"
#include "port_tab_logic.h"
#include "port_tab_rewrite.h"
#include "x86.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Retained mechanical originals (declared by the generated funcs.h only after
 * the override header, so declare them here). */
void fn_00650490(X86 *c);
void fn_006a7fe0(X86 *c);
void fn_006a7990(X86 *c);
void fn_006ac670(X86 *c); /* options page ACCEPT (vtable+0x10c) */
void fn_006ac5f0(X86 *c); /* options page CANCEL (vtable+0x110) */

/* ------------------------------------------------------------------------- */
/* Live FXAA state and persistence.                                           */

/* Plain words, not atomics: the options callbacks and the scene boundary both
 * run under the guest scheduler baton, so a host thread can never observe a
 * torn value. Do not read g_fxaa_mode from a native thread. */
static uint32_t g_fxaa_mode;
static uint32_t g_fxaa_snapshot;
static int g_fxaa_ready;
static int g_options_active;

#define GR_SETTINGS_ENV "RECOMP_PORT_SETTINGS"
#define GR_SETTINGS_NAME ".ghostrecon_recomp_port_tab.cfg"

/* Resolve the settings file. RECOMP_PORT_SETTINGS (used by the tests) wins;
 * otherwise the per-user home from the environment. Returns 0, with a named
 * diagnostic, when the path is unset or does not fit. */
static int config_path(char *out, size_t n) {
    const char *override = getenv(GR_SETTINGS_ENV);
    if (override && override[0]) {
        int w = snprintf(out, n, "%s", override);
        if (w < 0 || (size_t)w >= n) {
            fprintf(stderr, "port_tab: %s is too long; choices will not persist\n",
                    GR_SETTINGS_ENV);
            return 0;
        }
        return 1;
    }
    const char *home = getenv("HOME");
    if (!home || !home[0])
        home = getenv("USERPROFILE");
    if (!home || !home[0])
        return 0;
    int w = snprintf(out, n, "%s/%s", home, GR_SETTINGS_NAME);
    if (w < 0 || (size_t)w >= n) {
        fprintf(stderr, "port_tab: home path is too long; choices will not persist\n");
        return 0;
    }
    return 1;
}

static void fxaa_init(void) {
    if (g_fxaa_ready)
        return;
    g_fxaa_ready = 1;
    uint32_t mode = 0;
    const char *env = getenv("RECOMP_FXAA");
    if (env && env[0]) {
        mode = (uint32_t)strtoul(env, NULL, 10);
    } else {
        char path[1024];
        if (config_path(path, sizeof path)) {
            FILE *f = fopen(path, "rb");
            if (f) {
                unsigned v = 0;
                if (fscanf(f, "fxaa=%u", &v) == 1) {
                    if (v <= 1)
                        mode = v;
                    else
                        fprintf(stderr, "port_tab: ignoring unsupported persisted mode %u in %s\n",
                                v, path);
                }
                fclose(f);
            }
        }
    }
    g_fxaa_mode = mode;
    g_fxaa_snapshot = mode;
}

/* Hide PORT through the container's original +0xd8 method. Preserve the
 * complete guest context across this extra __thiscall (one stack argument). */
static void hide_panel(X86 *c, uint32_t panel) {
    X86 saved = *c;
    const uint32_t hidden = 0;
    c->r[R_ECX] = panel;
    recomp_guest_call(c, rd32(rd32(panel) + 0xd8), &hidden, 1);
    *c = saved;
}

/* MIPMAP's type-0x0f button uses state 2 for checked and 0 for off.
 * Original OptionsPage helper 006aa3a0 sends event 0xba with (3, bool);
 * 0068a2f0 (receiver widget+8) writes precisely widget+0x14, with no
 * other effects on this branch. The +0xd4 setter's flag 4 controls hover
 * state 1 and refuses to clear state 2, so it cannot synchronize a toggle.
 * This native equivalent retains the original button's input/render methods. */
static void set_toggle(uint32_t box, uint32_t mode) {
    if (box)
        wr32(box + 0x14, mode ? 2u : 0u);
}

/* The Port panel is present only when the resource rewrite actually replaced
 * the loader's stream. Checking the live page, rather than a session flag,
 * keeps every menu-side change off the original resource and off a page built
 * before the rewrite existed. */
static uint32_t page_port(uint32_t page) {
    return page ? gr_child_by_name(g_mem, page, "PORT") : 0;
}

static void fxaa_apply(uint32_t page, uint32_t mode) {
    g_fxaa_mode = mode;
    uint32_t port = page_port(page);
    set_toggle(gr_child_by_name(g_mem, port, "PORT_FXAA"), mode);
}

/* Persist a 0/1 preference. An unsupported RECOMP_FXAA mode is a renderer
 * diagnostic, not something the menu can set, so it is never written and a
 * previously saved choice is left alone. The write goes to an exclusively
 * created sibling temp and is renamed into place, so a symlink at the temp path
 * cannot be followed. Every failure is named and keeps the previous file.
 * On Windows rename cannot replace an existing destination; a later save then
 * fails with the diagnostic rather than deleting the previous choice. */
static void fxaa_persist(void) {
    uint32_t value = g_fxaa_mode;
    if (value > 1)
        return;
    char path[1024];
    if (!config_path(path, sizeof path))
        return;
    char tmp[1088];
    int w = snprintf(tmp, sizeof tmp, "%s.tmp", path);
    if (w < 0 || (size_t)w >= sizeof tmp) {
        fprintf(stderr, "port_tab: settings path is too long; choice not saved\n");
        return;
    }
    FILE *f = fopen(tmp, "wbx");
    if (!f) {
        fprintf(stderr, "port_tab: cannot create %s; choice not saved\n", tmp);
        return;
    }
    int failed = 0;
    if (fprintf(f, "fxaa=%u\n", value) < 0)
        failed = 1;
    if (fflush(f) != 0)
        failed = 1;
    if (fclose(f) != 0)
        failed = 1;
    if (failed) {
        fprintf(stderr, "port_tab: cannot write %s; choice not saved\n", tmp);
        remove(tmp);
        return;
    }
    if (rename(tmp, path) != 0) {
        fprintf(stderr, "port_tab: cannot replace %s; keeping the previous choice\n", path);
        remove(tmp);
    }
}

uint32_t gr_port_fxaa_mode(void) {
    fxaa_init();
    return g_fxaa_mode;
}

#ifdef PORT_TAB_TEST_BUILD
/* Test-only seam: the hook test harness runs several scenarios in one process
 * and must re-run lazy initialization between them. Never defined for the game
 * build. */
void gr_port_tab_test_reset(void) {
    g_fxaa_mode = 0;
    g_fxaa_snapshot = 0;
    g_fxaa_ready = 0;
    g_options_active = 0;
}
#endif

/* ------------------------------------------------------------------------- */
/* Resource loader hook.                                                      */

static uint8_t *read_host_file(const char *path, size_t *out_len) {
    FILE *f = fopen(path, "rb");
    if (!f)
        return NULL;
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (sz <= 0) {
        fclose(f);
        return NULL;
    }
    uint8_t *p = (uint8_t *)malloc((size_t)sz);
    if (!p || fread(p, 1, (size_t)sz, f) != (size_t)sz) {
        free(p);
        fclose(f);
        return NULL;
    }
    fclose(f);
    *out_len = (size_t)sz;
    return p;
}

void gr_ui_load_resource(X86 *c) {
    uint32_t esp = c->r[R_ESP];
    uint32_t name_ptr = rd32(esp + 4);
    const char *name = name_ptr ? (const char *)(g_mem + name_ptr) : NULL;
    const char *disable = getenv("RECOMP_PORT_TAB");
    if (!name || (disable && disable[0] == '0') || !gr_path_is_ike_res(name)) {
        fn_00650490(c);
        return;
    }
    fxaa_init();

    char host[1024];
    if (!recomp_readable_path(name, host, sizeof host)) {
        fprintf(stderr, "port_tab: cannot resolve %s (using original)\n", name);
        fn_00650490(c);
        return;
    }
    size_t in_len = 0;
    uint8_t *in = read_host_file(host, &in_len);
    if (!in) {
        fprintf(stderr, "port_tab: cannot read %s (using original)\n", host);
        fn_00650490(c);
        return;
    }
    uint8_t *aug = NULL;
    size_t aug_len = 0;
    char err[128] = {0};
    int ok = gr_port_tab_rewrite(in, in_len, &aug, &aug_len, err, sizeof err) == 0;
    free(in);
    if (!ok) {
        fprintf(stderr, "port_tab: %s (using original IKE.RES)\n", err);
        fn_00650490(c);
        return;
    }

    char alias_host[1024] = {0};
    if (!recomp_temp_file(".res", aug, aug_len, alias_host, sizeof alias_host)) {
        fprintf(stderr, "port_tab: cannot create a temp resource (using original)\n");
        free(aug);
        fn_00650490(c);
        return;
    }
    free(aug);

    static const char synthetic[] = "C:\\Windows\\Temp\\recomp_port_tab.res";
    if (!recomp_file_alias_add(synthetic, alias_host)) {
        fprintf(stderr, "port_tab: cannot alias %s (using original)\n", alias_host);
        remove(alias_host);
        fn_00650490(c);
        return;
    }
    uint32_t guest = recomp_guest_alloc((uint32_t)sizeof synthetic);
    if (!guest) {
        recomp_file_alias_remove(synthetic);
        remove(alias_host);
        fn_00650490(c);
        return;
    }
    memcpy(g_mem + guest, synthetic, sizeof synthetic);

    uint32_t saved = rd32(esp + 4);
    wr32(esp + 4, guest);
    fn_00650490(c);
    wr32(esp + 4, saved);

    /* The loader reads the stream synchronously and closes it before returning,
   * so the alias and temp file can go now. */
    recomp_file_alias_remove(synthetic);
    recomp_guest_free(guest);
    remove(alias_host);
}

/* ------------------------------------------------------------------------- */
/* Options menu hooks.                                                        */

/* Event source widget: `**(int**)(event + 0x24)`, verified at 0x006a832d. */
static uint32_t event_source(uint32_t event) {
    uint32_t p = rd32(event + 0x24);
    return p ? rd32(p) : 0;
}

void gr_options_dispatch(X86 *c) {
    uint32_t event = rd32(c->r[R_ESP] + 4);
    uint16_t id = rd16(event + 8);
    if (id == 0xb6) {
        uint32_t page = c->r[R_ECX] - 8; /* receiver is page+8 */
        if (page_port(page)) {
            uint32_t src = event_source(event);
            const uint16_t *nm = gr_widget_name(g_mem, src);
            if (gr_wide_eq(nm, "PORT_FXAA")) {
                fxaa_apply(page, g_fxaa_mode ? 0u : 1u);
            } else if (gr_wide_eq(nm, "RESET")) {
                /* The original RESET repopulates only its own controls, so the Port
         * checkbox must be reset here. Accept and Cancel are deliberately not
         * handled: their confirmed callbacks own the transaction. */
                fxaa_apply(page, 0);
            }
        }
    }
    fn_006a7fe0(c);
}

void gr_options_set_visible(X86 *c) {
    uint32_t page = c->r[R_ECX];
    uint32_t show = rd8(c->r[R_ESP] + 4);
    fxaa_init();
    if (show && !g_options_active)
        g_fxaa_snapshot = g_fxaa_mode; /* only a real closed -> open transition */
    g_options_active = show != 0;
    fn_006a7990(c);
    if (show && page_port(page)) {
        /* The original hides its six panels; PORT is ours, so hide it too until
     * the generic TAB_PORT -> PORT selection shows it. */
        uint32_t port = page_port(page);
        hide_panel(c, port);
        set_toggle(gr_child_by_name(g_mem, port, "PORT_FXAA"), g_fxaa_mode);
    }
}

static int subpanel_back(uint32_t page) {
    uint32_t sub = rd32(page + 0x13c);
    return sub && (rd8(sub + 0x10) & 1);
}

void gr_options_accept(X86 *c) {
    uint32_t page = c->r[R_ECX];
    /* 006ac670 copies the pending page values into the settings arrays, then
   * closes when DAT_008ddbec+100 is set (or runs 006ac7e0 otherwise). No
   * validation-abort path was found, so persisting the live mode here is the
   * outcome the confirmed Accept produces. */
    if (page_port(page))
        fxaa_persist();
    fn_006ac670(c);
}

void gr_options_cancel(X86 *c) {
    uint32_t page = c->r[R_ECX];
    /* 006ac5f0 backs out of the Input sub-panel when page[0x13c]+0x10 bit 0 is
   * set and does not close Options; in that case the pending choice must stay,
   * not revert. */
    if (page_port(page) && !subpanel_back(page))
        fxaa_apply(page, g_fxaa_snapshot);
    fn_006ac5f0(c);
}
