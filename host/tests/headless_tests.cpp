// Exercise the production file-writing presenter without booting a game.
// Renaming its entry point keeps the capture/report code identical to headless.
#define main headless_guest_main
#include "../headless_main.cpp"
#undef main
#include "../../runtime/imports.h"
#include "../../runtime/memory.h"
#include <fstream>
#include "../../dx/com.h"
#include "../../dx/dx.h"
#include "../../dx/dxtypes.h"

static int checks = 0, failures = 0;
static void check(bool ok, const char *what) {
    ++checks;
    if (!ok) {
        ++failures;
        fprintf(stderr, "FAIL: %s\n", what);
    }
}
static uint32_t invoke(X86 &c, uint32_t target, const std::vector<uint32_t> &values) {
    uint32_t sp = c.r[R_ESP];
    if (!target) {
        check(false, "shim exists");
        return 0;
    }
    for (auto i = values.rbegin(); i != values.rend(); ++i) {
        c.r[R_ESP] -= 4;
        wr32(c.r[R_ESP], *i);
    }
    c.r[R_ESP] -= 4;
    wr32(c.r[R_ESP], 0);
    imports_dispatch(&c, target);
    check(c.r[R_ESP] == sp, "stdcall stack balance");
    return c.r[R_EAX];
}
static uint32_t call(X86 &c, const char *dll, const char *name,
                     std::initializer_list<uint32_t> args) {
    return invoke(c, imports_resolve(dll, name), args);
}
static uint32_t method(X86 &c, uint32_t object, uint32_t slot,
                       std::initializer_list<uint32_t> args = {}) {
    std::vector<uint32_t> values{object};
    values.insert(values.end(), args);
    return invoke(c, rd32(rd32(object) + slot * 4), values);
}
static uint32_t frame_pixel(int x, int y) {
    if (g_frames.empty() || g_frames.back().path.empty())
        return 0xffffffff;
    std::ifstream f(g_frames.back().path, std::ios::binary);
    std::string magic;
    int w = 0, h = 0, max = 0;
    f >> magic >> w >> h >> max;
    f.get();
    if (magic != "P6" || max != 255 || x < 0 || y < 0 || x >= w || y >= h)
        return 0xffffffff;
    f.seekg((size_t(y) * w + x) * 3, std::ios::cur);
    unsigned char p[3] = {};
    f.read(reinterpret_cast<char *>(p), 3);
    return f ? uint32_t(p[0]) << 16 | uint32_t(p[1]) << 8 | p[2] : 0xffffffff;
}
int main(int argc, char **argv) {
    if (argc != 2)
        return 2;
    g_frames_dir = argv[1];
    g_frame_every = 1;
    mkdir_p(g_frames_dir);
    mem_init();
    imports_init();
    dx_register_shims();
    X86 c;
    loader_init_context(&c);
    uint32_t s = 0x00300000;
    gm_put_wstr(s + 0x100, "CaptureWindow", 32);
    wr32(s + 4, imports_resolve("USER32.dll", "DefWindowProcW"));
    wr32(s + 36, s + 0x100);
    call(c, "USER32.dll", "RegisterClassW", {s});
    auto window = [&](uint32_t ex, uint32_t style, int x, int y, uint32_t parent = 0) {
        return call(
            c, "USER32.dll", "CreateWindowExW",
            {ex, s + 0x100, 0, style, uint32_t(x), uint32_t(y), 16, 16, parent, 0, IMAGE_BASE, 0});
    };
    uint32_t hwnd = window(0, 0x10000000, 0, 0);
    uint32_t blue = call(c, "GDI32.dll", "CreateSolidBrush", {0xff0000});
    uint32_t red = call(c, "GDI32.dll", "CreateSolidBrush", {0xff});
    uint32_t rect = s + 0x200, ps = s + 0x300, msg = s + 0x400;
    wr32(rect, 2);
    wr32(rect + 4, 2);
    wr32(rect + 8, 12);
    wr32(rect + 12, 12);
    uint32_t dc = call(c, "USER32.dll", "BeginPaint", {hwnd, ps});
    call(c, "USER32.dll", "FillRect", {dc, rect, blue});
    call(c, "USER32.dll", "EndPaint", {hwnd, ps});
    check(g_present_count == 1 && g_frames_written == 1, "EndPaint writes one captured frame");
    check(frame_pixel(4, 4) == 0x0000ff, "FillRect blue survives ARGB to PPM conversion");

    dc = call(c, "USER32.dll", "GetDC", {hwnd});
    call(c, "USER32.dll", "FillRect", {dc, rect, red});
    call(c, "USER32.dll", "PeekMessageW", {msg, 0, 0x9000, 0x9000, 1});
    check(g_present_count == 2 && frame_pixel(4, 4) == 0xff0000,
          "message pump presents dirty pixels while the DC is retained");
    call(c, "USER32.dll", "PeekMessageW", {msg, 0, 0x9000, 0x9000, 1});
    call(c, "USER32.dll", "ReleaseDC", {hwnd, dc});
    check(g_present_count == 2, "clean pump and DC release do not invent frames");

    // A static form gets no more WM_PAINT, but the display still refreshes.
    // Pinning changes guest time once per present, never once per paint/poll.
    host_set_time_source_pinned(100, 50);
    uint32_t refresh_start = g_present_count;
    for (unsigned n = 0; n < 4; ++n) {
        os_sleep_us(20000);
        headless_tick();
        check(g_present_count == refresh_start + n + 1,
              "painted-once window presents once on each display tick");
        check(host_pinned_clock_value() == 100 + (n + 1) * 50,
              "each window refresh advances the pinned clock once");
    }
    check(frame_pixel(4, 4) == 0xff0000, "refresh captures the unchanged painted pixels");
    host_set_time_source(nullptr);

    uint32_t top = window(8, 0x10000000, 0, 0);
    dc = call(c, "USER32.dll", "GetDC", {top});
    call(c, "USER32.dll", "FillRect", {dc, rect, blue});
    call(c, "USER32.dll", "ReleaseDC", {top, dc});
    uint32_t later = window(0, 0x10000000, 0, 0);
    dc = call(c, "USER32.dll", "GetDC", {later});
    call(c, "USER32.dll", "FillRect", {dc, rect, red});
    call(c, "USER32.dll", "ReleaseDC", {later, dc});
    check(frame_pixel(4, 4) == 0x0000ff, "topmost surface composites after later ordinary windows");

    uint32_t hidden = window(0, 0, 0, 0), before = g_present_count;
    dc = call(c, "USER32.dll", "GetDC", {hidden});
    call(c, "USER32.dll", "FillRect", {dc, rect, red});
    call(c, "USER32.dll", "ReleaseDC", {hidden, dc});
    call(c, "USER32.dll", "PeekMessageW", {msg, 0, 0x9000, 0x9000, 1});
    check(g_present_count == before, "hidden dirty windows do not present");
    call(c, "DDRAW.dll", "DirectDrawCreate", {0, s + 0x500, 0});
    uint32_t dd = rd32(s + 0x500), desc = s + 0x600;
    method(c, dd, 21, {640, 480, 16}); // SetDisplayMode
    wr32(desc, DDSD_SIZE);
    wr32(desc + DDSD_OFF_dwFlags, DDSD_CAPS);
    wr32(desc + DDSD_OFF_ddsCaps, DDSCAPS_PRIMARYSURFACE);
    method(c, dd, 6, {desc, s + 0x504, 0}); // CreateSurface
    uint32_t primary = rd32(s + 0x504);
    auto *surface = com_this(primary);
    check(surface != nullptr, "DirectDraw primary exists");
    if (surface) {
        for (uint32_t y = 0; y < surface->height; ++y)
            for (uint32_t x = 0; x < surface->width; ++x)
                wr16(surface->pixels + y * surface->pitch + x * 2, 0x7e0);
        host_present(g_mem + surface->pixels, 640, 480, 16, nullptr, surface->pitch);
        check(frame_pixel(20, 20) == 0x00ff00, "DirectDraw primary pixels remain green");
        check(frame_pixel(4, 4) == 0xff0000,
              "DirectDraw primary excludes unrelated top-level windows");
        for (unsigned n = 0; n < 3; ++n) {
            os_sleep_us(20000);
            host_present(g_mem + surface->pixels, 640, 480, 16, nullptr, surface->pitch);
            uint32_t presented = g_present_count;
            headless_tick();
            check(g_present_count == presented, "recent primary presents own the refresh cadence");
        }
        uint32_t presented = g_present_count;
        os_sleep_us(40000);
        headless_tick();
        check(g_present_count == presented + 1 && frame_pixel(20, 20) == 0x00ff00,
              "an idle primary remains the base when window refresh resumes");
        dc = call(c, "USER32.dll", "GetDC", {top});
        call(c, "USER32.dll", "FillRect", {dc, rect, red});
        call(c, "USER32.dll", "ReleaseDC", {top, dc});
        check(frame_pixel(20, 20) == 0x00ff00 && frame_pixel(4, 4) == 0xff0000,
              "GDI release composites over an owned snapshot of the current primary");
        check(rd16(surface->pixels + 4 * surface->pitch + 8) == 0x7e0,
              "window composition never overwrites guest primary storage");
    }
    method(c, primary, 2, {});
    method(c, dd, 2, {});
    for (uint32_t w : {hidden, later, top, hwnd})
        call(c, "USER32.dll", "DestroyWindow", {w});
    uint32_t no_windows = g_present_count;
    os_sleep_us(20000);
    headless_tick();
    check(g_present_count == no_windows, "clock does not present without visible window surfaces");
    call(c, "GDI32.dll", "DeleteObject", {blue});
    call(c, "GDI32.dll", "DeleteObject", {red});
    printf("%d checks, %d failures\n", checks, failures);
    mem_shutdown();
    return failures ? 1 : 0;
}
