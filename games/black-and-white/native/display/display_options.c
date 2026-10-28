#include "display_options.h"
#include "native_seam.h"
#include "display_seam.h"
#include <stdio.h>
#include <stdlib.h>

/* DIVERGENCE(original): 0x642d80 seeds missing display settings; 0x514f50 and
 * 0x5140c0 add a next-launch selector. Evidence and gaps: README.md. */
void fn_00642d80(X86 *c);
void fn_00514f50(X86 *c);
void fn_005140c0(X86 *c);
void fn_00515690(X86 *c);
static const char key[] =
    "HKEY_CURRENT_USER\\Software\\Lionhead Studios Ltd\\Black & White\\BWSetup";
static uint32_t label, scratch, selected_w, selected_h;
static int changed;

static int original(void) {
    const char *v = getenv("RECOMP_DISPLAY_ORIGINAL");
    return v && v[0] == '1' && !v[1];
}

static uint32_t invoke(X86 *c, uint32_t addr, uint32_t self, const uint32_t *args, unsigned n) {
    X86 saved = *c;
    for (unsigned i = n; i; --i) {
        c->r[R_ESP] -= 4;
        wr32(c->r[R_ESP], args[i - 1]);
    }
    c->r[R_ESP] -= 4;
    wr32(c->r[R_ESP], 0x005152d7);
    c->r[R_ECX] = self;
    recomp_call(c, addr);
    uint32_t result = c->r[R_EAX];
    *c = saved;
    return result;
}

static void text(uint32_t dest, const char *s) {
    unsigned i = 0;
    for (; s[i] && i < 255; ++i)
        wr16(dest + i * 2, (unsigned char)s[i]);
    wr16(dest + i * 2, 0);
}

static uint32_t control(X86 *c, unsigned id, unsigned x, unsigned y, unsigned w,
                        const char *caption, unsigned arrow) {
    uint32_t alloc[] = {arrow ? 0x25c : 0x244};
    uint32_t p = invoke(c, 0x007c64ee, 0, alloc, 1);
    if (!p) {
        fprintf(stderr, "display options: control allocation failed\n");
        abort();
    }
    text(scratch, arrow ? " " : caption);
    uint32_t args[] = {id, x, y, w, 30, scratch, 0};
    if (arrow) {
        uint32_t a[] = {id, x, y, scratch, 32, 0, arrow};
        invoke(c, 0x0040d260, p, a, 7);
    } else {
        invoke(c, 0x004098b0, p, args, 7);
    }
    wr32(p + 0x20, invoke(c, 0x00407a00, 0, NULL, 0) & 255);
    return p;
}

static void refresh(void) {
    char s[128];
    snprintf(s, sizeof s, "Resolution: %ux%u", selected_w, selected_h);
    text(label + 0x24, s);
}

static void offer_host_modes(void) {
    const char *override = getenv("RECOMP_DDRAW_MODES");
    if (override && *override)
        return;
    int w = 0, h = 0;
    if (host_display_screen_size(&w, &h))
        ddraw_add_mode(w & ~1, h & ~1, 32);
    int32_t pw = 0, ph = 0;
    if (host_display_screen(&pw, &ph)) {
        ddraw_add_mode(pw & ~1, ph & ~1, 32);
        ddraw_add_mode((pw / 2) & ~1, (ph / 2) & ~1, 32);
    }
}

static void move_y(uint32_t p, uint32_t y) {
    uint32_t height = rd32(p + 0x14) - rd32(p + 0xc);
    wr32(p + 0xc, y);
    wr32(p + 0x14, y + height);
}

void bw_display_layout(X86 *c) {
    uint32_t self = c->r[R_ECX];
    fn_00515690(c);
    if (original())
        return;
    uint32_t detail = rd32(self + 0x44);
    uint32_t y = rd32(detail + 0xc);
    if (y == 350) {
        y = 320;
        move_y(rd32(self + 0x2c), 250);
        move_y(detail, y);
        move_y(rd32(self + 0x18), y - 1);
        move_y(rd32(self + 0x1c), y - 1);
    }
    uint32_t owner = rd32(detail + 0x234);
    for (uint32_t p = rd32(owner + 0x6c); p; p = rd32(p + 0x230)) {
        uint32_t id = rd32(p + 0x18);
        if (id >= 0x1800 && id <= 0x1803)
            move_y(p, y + (id == 0x1803 ? 88 : id == 0x1800 ? 50 : 49));
    }
}

void bw_display_start(X86 *c) {
    if (!original()) {
        const char *names[] = {"ScreenW", "ScreenH", "ScreenD"};
        uint32_t defaults[] = {1024, 768, 32}, value;
        for (unsigned i = 0; i < 3; ++i)
            if (!recomp_registry_dword(key, names[i], &value))
                recomp_registry_set_dword(key, names[i], defaults[i]);
    }
    if (!original())
        offer_host_modes();
    fn_00642d80(c);
}

void bw_display_init(X86 *c) {
    fn_00514f50(c);
    if (original())
        return;
    offer_host_modes();
    changed = 0;
    selected_w = 1024;
    selected_h = 768;
    recomp_registry_dword(key, "ScreenW", &selected_w);
    recomp_registry_dword(key, "ScreenH", &selected_h);
    if (!scratch)
        scratch = recomp_guest_alloc(512);
    if (!scratch)
        abort();
    label = control(c, 0x1800, 250, 370, 300, "", 0);
    control(c, 0x1801, 210, 369, 32, "", 2);
    control(c, 0x1802, 558, 369, 32, "", 3);
    uint32_t notice = control(c, 0x1803, 250, 408, 300, "Requires restart", 0);
    wr32(notice + 0x20, 16);
    wr32(notice, 0x008ab5c0);
    wr32(notice + 0x23c, 4);
    wr8(notice + 0x22a, 0);
    refresh();
}

void bw_display_callback(X86 *c) {
    uint32_t sp = c->r[R_ESP];
    uint32_t event = rd32(sp + 4), owner = rd32(sp + 8), p = rd32(sp + 12);
    uint32_t current_label = 0;
    if (!original() && owner)
        for (uint32_t item = rd32(owner + 0x6c); item; item = rd32(item + 0x230))
            if (rd32(item + 0x18) == 0x1800) {
                current_label = item;
                break;
            }
    if (current_label)
        label = current_label;
    if (!original() && current_label && p && rd32(p + 0x234) == owner && rd32(p + 0x18) >= 0x1800 &&
        rd32(p + 0x18) <= 0x1803) {
        uint32_t keycode = rd32(sp + 16);
        int keyboard = event == 2 && (keycode == 0x1c || keycode == 0x9c || keycode == 0x39 ||
                                      keycode == 0xcb || keycode == 0xcd);
        if ((event == 1 || keyboard) && rd32(p + 0x18) != 0x1803) {
            uint32_t widths[256], heights[256], n = 0, w, h, bpp;
            for (uint32_t i = 0; ddraw_enum_display_mode(i, &w, &h, &bpp); ++i) {
                if (bpp != 32 || w < 1024 || h < 768)
                    continue;
                if (n == 256) {
                    fprintf(stderr, "display options: too many modes\n");
                    abort();
                }
                widths[n] = w;
                heights[n++] = h;
            }
            for (uint32_t a = 1; a < n; ++a)
                for (uint32_t b = a;
                     b && (widths[b] < widths[b - 1] ||
                           (widths[b] == widths[b - 1] && heights[b] < heights[b - 1]));
                     --b) {
                    uint32_t t = widths[b];
                    widths[b] = widths[b - 1];
                    widths[b - 1] = t;
                    t = heights[b];
                    heights[b] = heights[b - 1];
                    heights[b - 1] = t;
                }
            if (n) {
                uint32_t i = 0;
                while (i < n && (widths[i] != selected_w || heights[i] != selected_h))
                    ++i;
                int backwards = keyboard && (keycode == 0xcb || keycode == 0xcd)
                                    ? keycode == 0xcb
                                    : rd32(p + 0x18) == 0x1801;
                i = i == n ? (backwards ? n - 1 : 0) : (i + n + (backwards ? -1 : 1)) % n;
                selected_w = widths[i];
                selected_h = heights[i];
                changed = 1;
                refresh();
            }
        }
        if (event == 2 && !keyboard) {
            fn_005140c0(c);
            return;
        }
        c->eip = rd32(sp);
        c->r[R_ESP] = sp + 24;
        return;
    }
    if (!original() && event == 7 && changed && current_label) {
        recomp_registry_set_dword(key, "ScreenW", selected_w);
        recomp_registry_set_dword(key, "ScreenH", selected_h);
        recomp_registry_set_dword(key, "ScreenD", 32);
        changed = 0;
    }
    fn_005140c0(c);
}
