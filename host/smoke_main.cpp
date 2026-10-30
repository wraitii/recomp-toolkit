// smoke_main.cpp - the recompiled game, driven by a script, with no window.
//
// build/recomp/smoke exists so gameplay is checked before a person sees a
// build. It boots the real game exactly as the windowed host does, presses the
// buttons a script tells it to through exactly the paths a person's input
// takes, runs the real Metal renderer against an offscreen target, and then
// says whether what came out satisfies the script's expectations.
//
// It is headless by construction and by intent:
//
//   * No window, no NSApplication, no drawable. The Metal renderer draws into
//     an MTLTexture, which needs none of those.
//   * No audio device. host_audio_play measures the PCM the guest handed over -
//     which is what "was there a sound" actually asks - and plays nothing.
//   * Input is injected into the same host_input_* state the real keyboard and
//     mouse feed, announced through the same dinput_host_input_changed, and
//     posted as the same Win32 messages. A path that only a script could take
//     would be testing something the game does not do.
//
// What it cannot do is tell you the game looks right. It can tell you the
// device uploaded textures, that the scene is not black, that a sound had
// amplitude in it, and it can leave you the frames to look at.
#include "boot.h"
#include <algorithm>
#include <cassert>
#include <cstdint>
#include "game_config.h"
#include "script.h"
#include "script_touch.h"
#include "controls/vpad.h"
#include "sdl/keymap.h"
#include "smoke_dumpat.h"
#include "../mods/mods_internal.h"
#include "input.h"
#include "input_gate.h"
#include "page_overlay.h"
#include "audio.h"
#include "present.h"
#include "d3d_render.h"
#include "gpu/gpu_factory.h"
#include "../runtime/display_seam.h"
#include "../runtime/guest.h"
#include "../runtime/loader.h"
#include "../runtime/win32.h"
#include "../runtime/gdi32_internal.h"
#include "../dx/ddraw.h"
#include "../dx/dx.h"
#include "../dx/host_api.h"
#include "../mods/display_settings.h"
#include "../mods/mods_internal.h"

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <atomic>
#include <sstream>
#include <string>
#include <vector>
#include "../platform/os.h"

namespace {

// --- the script -------------------------------------------------------------
const int MAX_STEPS = 512;
HostScriptStep g_steps[MAX_STEPS];
int g_step_count = 0;
int g_next_step = 0;
uint32_t g_script_start_ms = 0;
// An await in progress: when it began, and how many gave up. A script that
// waits for something that never happens fails rather than hangs.
bool g_await_started = false;
uint32_t g_await_since = 0;
// When the claim last became true, for an await that asks it to hold.
bool g_await_true = false;
uint32_t g_await_true_since = 0;
// Presents at the moment the claim became true. The hold is counted in frames,
// not in script time: see the note on the two units in script.h. A hold in
// time can be satisfied by a guest spinning on the clock without drawing,
// which is what the front end does under the pin while it waits for a sound
// cursor, and five pinned runs in twelve failed that way.
uint32_t g_await_true_presents = 0;
uint32_t g_await_timeouts = 0;
bool g_script_started = false;
bool g_quit_requested = false;

// --- what the run measured --------------------------------------------------
uint32_t g_presents = 0;
// Published only at the completed-present boundary used by dumpat (at seal
// for drawable smoke). Surface refreshes in flight do not age BODY records.
uint32_t g_completed_presents = 0;
// Presents whose picture differed from the one before. A game that is running
// keeps changing what it shows; a frozen one keeps presenting the same frame,
// and the present count alone cannot tell those apart.
uint32_t g_present_changes = 0;
// The share of the LAST presented frame that was not black, kept continuously
// rather than only at a dump. It is what a script waits on to know the game
// has something on screen: the menu is a picture, the loading screens before
// it are not.
double g_picture = 0.0;
// Presents that replaced the picture rather than changed it: more than a
// quarter of the sample differs from the frame before. Startup is a sequence
// of whole screens - a loading screen, the copyright screen, the menu - and
// counting the changes of screen is how a script names one of them without
// having to recognise its contents. `g_prev_sample` is the sample the last
// present was measured against.
uint32_t g_screen_changes = 0;
std::vector<uint8_t> g_prev_sample;
// The share of the sample that differed from the frame before, and how long
// the picture has been holding still. A fade is not a screen: every frame of
// one differs from the last, so a script that waits for a bright picture can
// be answered in the middle of the cross-fade into the menu, half a second
// before the menu will take a click. Waiting for the picture to stop moving
// is what tells a finished screen from a screen on its way in.
double g_moved = 0.0;
uint32_t g_still_since_ms = 0;
// Set by RECOMP_SMOKE_TRACE: print the continuous metrics once a second, which is
// how the thresholds in the script were chosen.
bool g_trace = false;
uint32_t g_trace_next_ms = 0;
uint64_t g_last_frame_hash = 0;
uint32_t g_audio_plays = 0;
double g_audio_peak = 0.0;
double g_scene_nonblack = 0.0; // the best any scene dump reached
double g_present_nonblack = 0.0;
int g_mode_w = 0, g_mode_h = 0, g_mode_bpp = 0;
std::vector<std::string> g_dumps;

// The last presented surface, kept so a dump can be written on demand rather
// than only when a frame arrives.
std::vector<uint8_t> g_last_rgb;
// Bumped wherever g_last_rgb is refilled. A Direct3D 9 device on the GPU
// presents no CPU pixels, so that buffer is only refreshed when a dump reads
// one back: without this the frame hash below re-reads the same picture every
// present and reports that nothing ever changed.
uint64_t g_last_rgb_version = 0, g_hashed_version = 0;
uint32_t g_present_samples = 0;
int g_last_w = 0, g_last_h = 0;

D3DRenderer *g_renderer = nullptr;

// A channel's play cursor has to advance in real time from the moment Play was
// called, whether or not anything is audible: the video player at 0057a9a0
// queues a chunk, polls GetCurrentPosition and sleeps until the cursor has
// moved far enough to queue the next one. A cursor pinned at zero stops the
// video, which is what this host did on its first run - 42 frames and then a
// wait that never ended.
struct SmokeChannel {
    bool playing = false;
    double started = 0.0;
    uint32_t rate = 22050, total = 0, start_offset = 0;
    int bits = 16, channels = 2;
    bool loop = false;
};
const int32_t SMOKE_CHANNELS = 64;
SmokeChannel g_channels[SMOKE_CHANNELS];

// The share of pixels that are not black. A scene that rendered is not black;
// a scene that did not is. It is a coarse measure and it is the right one for
// "did anything reach the target at all".
double nonblack_ratio(const uint8_t *rgb, int w, int h) {
    if (!rgb || w <= 0 || h <= 0)
        return 0.0;
    size_t lit = 0, n = (size_t)w * (size_t)h;
    for (size_t i = 0; i < n; ++i) {
        if (rgb[i * 3 + 0] > 8 || rgb[i * 3 + 1] > 8 || rgb[i * 3 + 2] > 8)
            ++lit;
    }
    return n ? (double)lit / (double)n : 0.0;
}

// The script's pointer steps are in display-mode space, which they assume does
// not move under them. Compare the mode with the mode: g_last_w/h is the size
// of the last frame the device presented, and a Direct3D 9 device renders at
// the window's own scale, so the two differ by design wherever a game draws
// larger than the mode it reports.
void pointer_space(uint32_t *w, uint32_t *h) {
    uint32_t bpp = 0;
    win32_display_mode(w, h, &bpp);
    static uint32_t mode_w = 0, mode_h = 0;
    assert((!mode_w && !mode_h) || (*w == mode_w && *h == mode_h));
    mode_w = *w;
    mode_h = *h;
}

void write_dump(const char *name) {
    uint32_t space_w, space_h;
    pointer_space(&space_w, &space_h);
    char path[1024];
    if (g_last_w && g_last_h) {
        snprintf(path, sizeof path, "%s/smoke_%s_present.ppm", host_dump_dir(), name);
        if (host_write_ppm(path, g_last_rgb.data(), g_last_w, g_last_h)) {
            g_dumps.push_back(path);
            double ratio = nonblack_ratio(g_last_rgb.data(), g_last_w, g_last_h);
            if (ratio > g_present_nonblack)
                g_present_nonblack = ratio;
        }
    }
    // The Direct3D target as the device left it, which is a different question
    // from what the surface ended up holding.
    int w = 0, h = 0;
    std::vector<uint8_t> bgra;
    if (g_renderer && g_renderer->colorTarget()) {
        const gpu::TextureDesc target = g_renderer->device()->describe(g_renderer->colorTarget());
        w = target.width;
        h = target.height;
        bgra.resize((size_t)w * (size_t)h * 4);
        if (g_renderer->readPixels(bgra.data(), &w, &h)) {
            std::vector<uint8_t> rgb((size_t)w * (size_t)h * 3);
            for (size_t i = 0, n = (size_t)w * (size_t)h; i < n; ++i) {
                rgb[i * 3 + 0] = bgra[i * 4 + 2];
                rgb[i * 3 + 1] = bgra[i * 4 + 1];
                rgb[i * 3 + 2] = bgra[i * 4 + 0];
            }
            snprintf(path, sizeof path, "%s/smoke_%s_scene.ppm", host_dump_dir(), name);
            if (host_write_ppm(path, rgb.data(), w, h)) {
                g_dumps.push_back(path);
                double ratio = nonblack_ratio(rgb.data(), w, h);
                if (ratio > g_scene_nonblack)
                    g_scene_nonblack = ratio;
            }
        }
    }
}

// Print a bounded guest-memory sample for script diagnostics.
void peek(uint32_t addr, uint32_t len) {
    if (!gm_valid(addr, len)) {
        printf("[smoke] peek %08x+%u: outside the guest address space\n", addr, len);
        fflush(stdout);
        return;
    }
    const uint8_t *p = gm_ptr(addr);
    printf("[smoke] peek %08x+%u:", addr, len);
    for (uint32_t i = 0; i < len; ++i) {
        if (i % 16 == 0)
            printf("\n    %08x ", addr + i);
        printf(" %02x", p[i]);
    }
    printf("\n");
    fflush(stdout);
}

HostDumpAt g_dumpat;

// The metrics a script may make a claim about.
double metric(const char *name) {
    if (!strcmp(name, "dumpat_fired"))
        return g_dumpat.fired;
    if (!strcmp(name, "hd_draws"))
        return g_renderer ? g_renderer->hdTextureStats().draws : 0;
    if (!strcmp(name, "hd_refused"))
        return g_renderer ? g_renderer->hdTextureStats().refused : 0;
    if (!strcmp(name, "textures"))
        return host_d3d_total_textures();
    if (!strcmp(name, "draws"))
        return host_d3d_total_draws();
    if (!strcmp(name, "writebacks"))
        return host_d3d_total_flushes();
    if (!strcmp(name, "presents"))
        return g_presents;
    if (!strcmp(name, "present_changes"))
        return g_present_changes;
    if (!strcmp(name, "picture"))
        return g_picture;
    if (!strcmp(name, "screen_changes"))
        return g_screen_changes;
    if (!strcmp(name, "still_ms"))
        return g_still_since_ms ? (double)(boot_guest_millis() - g_still_since_ms) : 0.0;
    // Counted at the end whether a script mentions it or not, because a run
    // that lost the game cannot be trusted about anything after that.
    if (!strcmp(name, "awaits_timed_out"))
        return g_await_timeouts;
    if (!strcmp(name, "audio_plays"))
        return g_audio_plays;
    if (!strcmp(name, "audio_peak"))
        return g_audio_peak;
    // The best any frame the device produced ever reached, not the best a dump
    // happened to catch.
    if (!strcmp(name, "scene_nonblack"))
        return host_d3d_peak_nonblack();
    if (!strcmp(name, "scene_dump_nonblack"))
        return g_scene_nonblack;
    if (!strcmp(name, "present_nonblack"))
        return g_present_nonblack;
    return host_script_counter_metric(name, [](uint32_t address) -> uint32_t {
        if (!gm_valid(address, 4))
            return 0;
        const uint8_t *p = gm_ptr(address);
        return uint32_t(p[0]) | (uint32_t(p[1]) << 8) | (uint32_t(p[2]) << 16) |
               (uint32_t(p[3]) << 24);
    });
}

// Where the script's pointer is, so a move can report the distance travelled
// as well as the destination. DirectInput gives a game relative counts and
// nothing else; a game that integrates them into its own cursor - which this
// one does, since that is what a DirectInput mouse is for - never sees an
// absolute position at all. A move that reported only the destination moved
// the cursor nowhere.
int32_t g_pointer_x = 320, g_pointer_y = 240;
uint8_t g_buttons = 0;
// RECOMP_SMOKE_WINDOW_INPUT routes every scripted pointer step through the
// window mapping a real mouse uses, rather than placing the guest pointer
// directly. It is how a host-side input defect is reproduced without a hand on
// the mouse. `guestclick` turns it on for its own step whatever this says.
bool g_window_gestures = recomp_env("SMOKE_WINDOW_INPUT") != nullptr;
HitResult g_window_hit;
int g_window_mapping_failures = 0;

// Win32 messages, which is the other half of how input reaches this game and
// the half a script has to post for itself. The windowed host posts these from
// its NSEvent pump; nothing else does, so a scripted run that only fed the
// DirectInput state was talking to a device the front end never reads. The
// measurement that showed it: twelve reads of the input state in thirty
// seconds, one for each announcement and not one more, which is a game that
// answers notifications rather than polling.
enum {
    WM_KEYDOWN_ = 0x0100,
    WM_KEYUP_ = 0x0101,
    WM_MOUSEMOVE_ = 0x0200,
    WM_LBUTTONDOWN_ = 0x0201,
    WM_LBUTTONUP_ = 0x0202,
    WM_RBUTTONDOWN_ = 0x0204,
    WM_RBUTTONUP_ = 0x0205,
    WM_MBUTTONDOWN_ = 0x0207,
    WM_MBUTTONUP_ = 0x0208,
    WM_ACTIVATE_ = 0x0006,
    WM_SETFOCUS_ = 0x0007,
    WM_KILLFOCUS_ = 0x0008,
    WM_PAINT_ = 0x000f,
    WM_ACTIVATEAPP_ = 0x001c,
};

uint32_t make_lparam(int32_t x, int32_t y) {
    return ((uint32_t)(y & 0xffff) << 16) | (uint32_t)(x & 0xffff);
}

void post(uint32_t msg, uint32_t wparam, uint32_t lparam) {
    if (msg >= 0x0200 && msg <= 0x020e && dinput_host_mouse_exclusive())
        return;
    if (msg >= 0x0100 && msg <= 0x0109) {
        host_post_key_message(msg, wparam, lparam);
        return;
    }
    if (msg >= 0x200 && msg <= 0x209) {
        host_post_client_mouse_message(host_main_window(), msg, wparam, int16_t(lparam),
                                       int16_t(lparam >> 16));
        return;
    }
    uint32_t hwnd = host_main_window();
    if (hwnd)
        host_post_message(hwnd, msg, wparam, lparam);
}

void move_by(int32_t dx, int32_t dy) {
    // The absolute position the host reports is clamped to the guest's frame,
    // because that is what a cursor position means; the delta is not, because
    // that is what a mouse actually produced. A script pins the game's own
    // pointer to a corner by sending a delta far larger than the screen, which
    // is the only position it can be sure of: the game integrates the deltas
    // itself and started its pointer wherever it chose.
    uint32_t width, height;
    pointer_space(&width, &height);
    int32_t x = int32_t(std::clamp<int64_t>(int64_t(g_pointer_x) + dx, 0, width - 1));
    int32_t y = int32_t(std::clamp<int64_t>(int64_t(g_pointer_y) + dy, 0, height - 1));
    if (host_gate_motion(x, y, dx, dy))
        return; // consumed
    host_input_motion(x, y, dx, dy);
    g_pointer_x = x;
    g_pointer_y = y;
    post(WM_MOUSEMOVE_, host_mouse_wparam(g_buttons, 0), make_lparam(x, y));
}

void move_pointer(int32_t x, int32_t y) {
    if (g_window_gestures) {
        LayoutSnapshot layout;
        if (!host_present_copy_layout(&layout) || layout.scene.scale_x <= 0 ||
            layout.scene.scale_y <= 0) {
            ++g_window_mapping_failures;
            printf("[smoke] FAIL window gesture: no published layout\n");
            return;
        }
        // Target the centre of the guest pixel, then use the SAME mapping and
        // DirectInput correction as an NSEvent. UI occlusion must still win.
        const int wx = int(std::floor((x + .5) * layout.scene.scale_x + layout.scene.offset_x));
        const int wy = int(std::floor((y + .5) * layout.scene.scale_y + layout.scene.offset_y));
        if (!host_gate_window_motion(wx, wy, 0, 0, &g_window_hit) ||
            g_window_hit.kind != HitResult::HIT_SCENE || abs(g_window_hit.gx - x) > 1 ||
            abs(g_window_hit.gy - y) > 1) {
            ++g_window_mapping_failures;
            printf("[smoke] FAIL window mapping (%d,%d) -> (%d,%d)\n", wx, wy, g_window_hit.gx,
                   g_window_hit.gy);
            return;
        }
        g_pointer_x = g_window_hit.gx;
        g_pointer_y = g_window_hit.gy;
        post(WM_MOUSEMOVE_, host_mouse_wparam(g_buttons, 0), make_lparam(g_pointer_x, g_pointer_y));
        printf("[smoke] window input (%d,%d) -> guest (%d,%d), domain %d\n", wx, wy, g_pointer_x,
               g_pointer_y, layout.scene.domain_w);
        return;
    }
    move_by(x - g_pointer_x, y - g_pointer_y);
}

// True when the event was delivered; false when the mod layer consumed it and
// nothing reached the guest on any path.
bool press_button(int button, bool down) {
    if (button < 0 || button > 2)
        return false;
    if (host_gate_button(button, down, g_pointer_x, g_pointer_y))
        return false;
    if (g_window_gestures) {
        if (down)
            host_gate_begin_drag(&g_window_hit);
        else
            host_gate_end_drag();
    }
    if (down)
        g_buttons |= (uint8_t)(1u << button);
    else
        g_buttons &= (uint8_t)~(1u << button);
    host_input_button(button, down);
    static const uint32_t msgs[3][2] = {
        {WM_LBUTTONUP_, WM_LBUTTONDOWN_},
        {WM_RBUTTONUP_, WM_RBUTTONDOWN_},
        {WM_MBUTTONUP_, WM_MBUTTONDOWN_},
    };
    post(msgs[button][down ? 1 : 0], host_mouse_wparam(g_buttons, 0),
         make_lparam(g_pointer_x, g_pointer_y));
    return true;
}

// A click has to be held long enough for the guest to see it.
//
// The DirectInput shim builds its buffered events by comparing the host state
// against what it saw last time it was asked, and the front end asks when it is
// told the state changed. Pressing and releasing in the same host turn leaves
// the button at the level it started from, so the comparison finds no change
// and BOTH events are lost - the click never happened as far as the game is
// concerned. It has to still be down when the guest next looks.
const uint32_t kClickHoldMs = 120;
int32_t g_holding_button = -1;
// Presents, not milliseconds. A click is seen only if the guest polls the
// device while the button is down, and it polls once a frame: 120 ms is two
// and a half frames at a 50 ms step, so a click could go down and come up
// between two polls and never happen at all. See script.h for the floor.
uint32_t g_release_at_presents = 0;
// And the wall the press happened at. The release needs BOTH, for the same
// reason the await hold does: frames, because the guest only sees the button
// while it polls, once a frame; and time, because an unpinned run presents far
// faster than 50 ms a frame and four frames there can be under ten
// milliseconds, which is shorter than the 120 the click was written to hold.
uint32_t g_press_at_ms = 0;

HostScriptTouch g_touch;
int g_touch_drawable_w = 640, g_touch_drawable_h = 480;

// Mirror SDL apply_motion / PLACE / apply_button on the scheduler baton.
// Smoke window points equal drawable pixels; guest coordinates are mapped into
// that window before entering the mapper, never injected as a shortcut click.
void deliver_touch_actions(const std::vector<TouchAction> &actions) {
    for (const TouchAction &a : actions) {
        const int x = (int)std::lround(a.x), y = (int)std::lround(a.y);
        host_gate_fallback_layout(g_touch_drawable_w, g_touch_drawable_h);
        if (a.kind == TouchAction::Key) {
            const uint16_t code = host_keycode_from_scancode(a.scancode);
            if (code == 0xffff || host_gate_key(code, a.down))
                continue;
            const HostKeyMapping m = host_key_mapping(code);
            host_input_key(code, a.down);
            post(a.down ? WM_KEYDOWN_ : WM_KEYUP_, m.vk,
                 host_key_lparam(m, a.down, false, !a.down));
            printf("[smoke-pad-key] scancode %d %s\n", a.scancode, a.down ? "down" : "up");
        } else if (a.kind == TouchAction::Wheel) {
            if (!host_gate_wheel(a.wheel * 120))
                host_input_wheel(a.wheel * 120);
        } else if (a.kind == TouchAction::Motion) {
            HitResult hit;
            if (host_gate_window_motion(x, y, 0, 0, &hit)) {
                g_pointer_x = hit.gx;
                g_pointer_y = hit.gy;
                post(WM_MOUSEMOVE_, host_mouse_wparam(g_buttons, 0), make_lparam(hit.gx, hit.gy));
            }
            if (a.place)
                host_gate_pointer_place(x, y);
            printf("[smoke-tap] motion guest %d,%d present %u ms %u\n", g_pointer_x, g_pointer_y,
                   g_presents, boot_guest_millis());
        } else if (a.kind == TouchAction::Button) {
            int32_t dx, dy;
            auto hit = host_gate_window_pointer(x, y, &dx, &dy);
            if (hit.kind == HitResult::HIT_NONE && a.down)
                continue;
            if (hit.kind == HitResult::HIT_NONE) {
                hit.gx = g_pointer_x;
                hit.gy = g_pointer_y;
            }
            if (!a.down && !(g_buttons & ~(1u << a.button)))
                host_gate_end_drag();
            const bool consumed = host_gate_button(a.button, a.down, hit.gx, hit.gy);
            printf("[smoke-tap] button %d %s guest %d,%d consumed %d present %u ms %u\n", a.button,
                   a.down ? "down" : "up", hit.gx, hit.gy, int(consumed), g_presents,
                   boot_guest_millis());
            if (consumed)
                continue;
            if (a.down)
                host_gate_begin_drag(&hit);
            g_pointer_x = hit.gx;
            g_pointer_y = hit.gy;
            host_input_motion(hit.gx, hit.gy, dx, dy);
            post(WM_MOUSEMOVE_, host_mouse_wparam(g_buttons, 0), make_lparam(hit.gx, hit.gy));
            if (a.down)
                g_buttons |= (uint8_t)(1u << a.button);
            else
                g_buttons &= (uint8_t)~(1u << a.button);
            host_input_button(a.button, a.down);
            static const uint32_t msgs[3][2] = {{WM_LBUTTONUP_, WM_LBUTTONDOWN_},
                                                {WM_RBUTTONUP_, WM_RBUTTONDOWN_},
                                                {WM_MBUTTONUP_, WM_MBUTTONDOWN_}};
            post(msgs[a.button][a.down ? 1 : 0], host_mouse_wparam(g_buttons, 0),
                 make_lparam(hit.gx, hit.gy));
        }
    }
}

void start_touch(const HostScriptStep &step) {
    LayoutSnapshot layout;
    const bool published =
        host_present_copy_layout(&layout) && layout.scene.scale_x > 0 && layout.scene.scale_y > 0;
    const double scale = std::min(double(g_touch_drawable_w) / std::max(1, g_mode_w),
                                  double(g_touch_drawable_h) / std::max(1, g_mode_h));
    const double x = step.op == HOST_SCRIPT_TAP_DRAWABLE ? step.x
                     : published
                         ? (step.x + .5) * layout.scene.scale_x + layout.scene.offset_x
                         : (step.x + .5) * scale + (g_touch_drawable_w - g_mode_w * scale) / 2;
    const double y = step.op == HOST_SCRIPT_TAP_DRAWABLE ? step.y
                     : published
                         ? (step.y + .5) * layout.scene.scale_y + layout.scene.offset_y
                         : (step.y + .5) * scale + (g_touch_drawable_h - g_mode_h * scale) / 2;
    std::vector<TouchAction> actions;
    g_touch.start(x, y, g_touch_drawable_w, g_touch_drawable_h,
                  uint64_t(boot_guest_millis()) * 1000000ull, g_presents, &actions);
    deliver_touch_actions(actions);
}

// Guest paths a `readfile` step asked for and did not get. Counted so the run
// fails on a miss the same way an unmet expectation does: an overlay layer the
// game's own file API cannot see is a broken overlay, not a warning.
int g_readfile_misses = 0;
// Probes that found the wrong colour. Counted like a readfile miss and added
// to the same tally, because a probe is an assertion the script made and a run
// that failed one has not passed.
int g_probe_misses = 0;

// An armed dumpat: the claim it is waiting for, and the name to write when it
// fires. Only one can be armed at a time - the script arms it and the next
// present fires it - so this is a single slot rather than a queue, and a
// second dumpat arriving while one is armed is a script mistake worth saying
// out loud rather than silently replacing.
// A guestclick's release, owed at a present index the way a click's is. It is
// a separate slot from the physical click's because the two go out through
// different paths and a script may hold one while the other is idle.
bool g_guestclick_held = false;
int32_t g_guestclick_button = 0;
int32_t g_guestclick_x = 0, g_guestclick_y = 0;
uint32_t g_guestclick_release_at = 0;

// The recorded path the `mode` verb replays, and where it has got to. A
// separate array from the main script rather than a splice into it: splicing
// would have to rebase every later step's time and would run out of the fixed
// array, and a sub-script that finishes and hands back is the same shape the
// verb reads as.
std::atomic<unsigned> g_completed_captures{0}, g_capture_failures{0};
HostFrameCapture fire_dumpat(HostScreenClass cls, bool at_seal);

// Reads a guest path exactly as the game would: through the file seam, which
// is where the overlay's read precedence lives, and then with an ordinary
// fopen on whatever host path that resolved to.
void read_guest_file(const char *guest_path, const char *want) {
    std::string host = win32_host_path_op(guest_path, WIN32_FILE_READ);
    std::string text;
    if (!host.empty()) {
        if (FILE *f = fopen(host.c_str(), "rb")) {
            char buf[4096];
            size_t n;
            while ((n = fread(buf, 1, sizeof buf, f)) > 0)
                text.append(buf, n);
            fclose(f);
        }
    }
    if (!text.empty() && text.find(want) != std::string::npos) {
        printf("readfile %s: ok\n", guest_path);
    } else {
        printf("readfile %s: MISSING (wanted \"%s\" in %s)\n", guest_path, want,
               host.empty() ? "(unresolved)" : host.c_str());
        ++g_readfile_misses;
    }
    fflush(stdout);
}

bool produces_input(int op);

// Load a recorded path for the `mode` verb to replay. Parsed with the same
// parser the main script uses, so a mistake in it is caught the same way and
// says the same thing.
// Where this binary was started from, so a path in the tree can be found
// whatever the working directory is. build/recomp/smoke is three levels
// below the root.
std::string g_argv0;

// Defined with the rest of the frame handling, below the executor that calls
// them, because they read the presented frame and belong beside it.
void write_composite_dump(const char *name);
void probe_pixel(const HostScriptStep &step);

// Execute one smoke-script operation through the same input and display interfaces as play.
// Trace input notification counts so a swallowed event is distinguishable from a game response.
void run_step(const HostScriptStep &step) {
    // Under RECOMP_SMOKE_TRACE, every input step says what it did and whether the
    // shim announced it. A step that produces no announcement was swallowed,
    // and this is the line that names which one.
    const uint32_t notify_before = produces_input(step.op) ? host_input_notify_count() : 0;
    const int32_t px_before = g_pointer_x, py_before = g_pointer_y;
    switch (step.op) {
    case HOST_SCRIPT_MOVE:
        move_pointer(step.x, step.y);
        break;
    case HOST_SCRIPT_MOVEBY:
        move_by(step.x, step.y);
        break;
    case HOST_SCRIPT_CLICK:
        move_pointer(step.x, step.y);
        // Only a press that was actually delivered owes a release. A press
        // the mod layer consumed never reached the guest, and a release for it
        // would be a lone up-transition the guest never saw a down for.
        if (!press_button((int)step.button, true))
            break;
        g_holding_button = step.button;
        g_release_at_presents =
            g_presents + host_script_input_hold_frames(kClickHoldMs, host_pinned_clock_step());
        g_press_at_ms = boot_guest_millis();
        break;
    case HOST_SCRIPT_TAP:
    case HOST_SCRIPT_TAP_DRAWABLE:
        start_touch(step);
        break;
    case HOST_SCRIPT_BUTTON:
        // A press that stays down until the script releases it: the moves in
        // between are a drag. Nothing is scheduled, unlike a click's release.
        if (step.down) {
            if (press_button((int)step.button, true))
                g_holding_button = -1; // the script owns this release
        } else {
            press_button((int)step.button, false);
        }
        break;
    case HOST_SCRIPT_KEY: {
        HostKeyMapping m = host_key_mapping_for_dik(step.dik);
        if (!m.dik)
            break;
        if (host_gate_key(m.mac, step.down != 0))
            break; // consumed
        host_input_key(m.mac, step.down != 0);
        // The same lParam the windowed host builds, so a key arrives here
        // exactly as it would from a real keyboard.
        uint32_t lparam = host_key_lparam(m, step.down != 0, false, step.down == 0);
        post(step.down ? WM_KEYDOWN_ : WM_KEYUP_, m.vk, lparam);
        break;
    }
    case HOST_SCRIPT_PAD: {
        static controls::PadState pad;
        const int index = step.button;
        if (index < 13) {
            const uint16_t bit = uint16_t(1u << index);
            pad.buttons = step.x ? pad.buttons | bit : pad.buttons & ~bit;
            if (index == 6)
                pad.l2 = float(step.x);
            if (index == 7)
                pad.r2 = float(step.x);
        } else if (index < 17) {
            const uint8_t bit = uint8_t(1u << (index - 13));
            pad.hat = step.x ? pad.hat | bit : pad.hat & ~bit;
        } else {
            float *axes[] = {&pad.lx, &pad.ly, &pad.rx, &pad.ry, &pad.l2, &pad.r2};
            *axes[index - 17] = step.x / 32767.0f;
        }
        controls::vpad().set_source(controls::kPadSourceTouch, pad);
        printf("[smoke-pad] control %d value %d packet %u\n", index, step.x,
               controls::vpad().packet());
        break;
    }
    case HOST_SCRIPT_FOCUS:
        // What the windowed host posts when the window gains or loses focus.
        post(WM_ACTIVATEAPP_, step.down ? 1 : 0, 0);
        post(WM_ACTIVATE_, step.down ? 1 : 0, 0);
        post(step.down ? WM_SETFOCUS_ : WM_KILLFOCUS_, 0, 0);
        if (step.down)
            post(WM_PAINT_, 0, 0);
        else
            host_gate_release_all();
        break;
    case HOST_SCRIPT_DUMP:
        write_dump(step.name);
        break;
    case HOST_SCRIPT_DUMPC:
        write_composite_dump(step.name);
        break;
    case HOST_SCRIPT_PROBE:
        probe_pixel(step);
        break;
    case HOST_SCRIPT_DUMPAT:
        // Armed here and fired by the presenter. A dump taken at the moment
        // the script reaches this line is a dump taken at a millisecond, and
        // two runs reach a millisecond at different points of the simulation.
        if (g_dumpat.armed) {
            printf("[smoke] dumpat %s replaced %s, which never fired; only one "
                   "can be armed at a time\n",
                   step.text, g_dumpat.name.c_str());
        }
        g_dumpat.arm(step.name, step.text, step.threshold, step.at_least);
        printf("[smoke] dumpat %s armed on %s%s%g\n", g_dumpat.name.c_str(),
               g_dumpat.metric.c_str(), g_dumpat.at_least ? ">=" : ">", g_dumpat.threshold);
        fflush(stdout);
        break;
    case HOST_SCRIPT_GUESTCLICK:
        // Placed, not moved. The press goes out here and the release is owed
        // at a present index, because a press and a release the guest never
        // got a frame between are a pair it cannot see: the shim derives
        // buffered events by diffing the state, and a state that returns to
        // where it started has no difference to report.
        if (g_guestclick_held)
            break; // one at a time, like a click
        // Same rule: a consumed press owes no release.
        if (host_gate_inject_guest_click(step.x, step.y, (int)step.button, true))
            break;
        g_guestclick_held = true;
        g_guestclick_button = step.button;
        g_guestclick_x = step.x;
        g_guestclick_y = step.y;
        g_guestclick_release_at =
            g_presents + host_script_input_hold_frames(step.press_ms, host_pinned_clock_step());
        break;
    case HOST_SCRIPT_PEEK:
        peek(step.addr, step.len);
        break;
    case HOST_SCRIPT_READFILE:
        read_guest_file(step.name, step.text);
        break;
    case HOST_SCRIPT_EXPECT:
        break; // checked at the end
    case HOST_SCRIPT_AWAIT:
        break; // handled by tick, which can wait
    case HOST_SCRIPT_QUIT:
        g_quit_requested = true;
        boot_request_close("the script asked to quit");
        break;
    default:
        break;
    }
    if (g_trace && produces_input(step.op)) {
        printf("[trace] step %d op %d at %u ms: pointer (%d, %d) -> (%d, %d), "
               "announced %u -> %u, guest reads %u, present %u\n",
               g_next_step, step.op, step.at_ms, px_before, py_before, g_pointer_x, g_pointer_y,
               notify_before, host_input_notify_count(), host_input_read_count(), g_presents);
        fflush(stdout);
    }
}

// Does this step put something into the guest's input? Those are the steps
// that must not share a host turn: the guest sees deltas, and two in one turn
// arrive summed. Everything else is bookkeeping the guest never sees.
bool produces_input(int op) {
    // INVERTED on purpose: everything ends the turn unless it is listed here
    // as producing no input. A new verb that feeds the guest is the dangerous
    // case, and with the list the other way round it would silently share a
    // turn until somebody noticed - which is how GUESTCLICK was missed in the
    // first version of this. A new verb that produces nothing costs at most an
    // extra turn until it is added here.
    switch (op) {
    case HOST_SCRIPT_WAIT:
    case HOST_SCRIPT_DUMP:
    case HOST_SCRIPT_PEEK:
    case HOST_SCRIPT_READFILE:
    case HOST_SCRIPT_EXPECT:
    case HOST_SCRIPT_AWAIT:
    case HOST_SCRIPT_QUIT:
    // T11's verbs that only observe. MODE and GUESTCLICK are NOT here: a mode
    // change and a synthesised click both reach the guest, and a verb that
    // feeds the guest sharing a turn with another is the defect this list
    // exists to prevent.
    case HOST_SCRIPT_PROBE:
    case HOST_SCRIPT_DUMPC:
    case HOST_SCRIPT_DUMPAT:
        return false;
    default:
        return true;
    }
}

// The host's turn, from inside the guest's own clock reads.
// One tick at a time, whichever thread arrives.
//
// The tick now has two entrances: the guest's own clock read, which is where
// it has always come from, and the scheduler's input drain, which runs on
// whichever thread was about to sleep. Both hold the baton when they call, but
// not against each other - the drain takes the baton at its checkpoint while
// the run thread can still be inside boot's own re-entrancy guard, which is a
// different flag on a different layer.
//
// Measured before this guard existed: a traced run showed two run_step calls
// overlapping, step 6 starting before step 5 finished, both reporting the same
// present and each seeing an announcement count the other had already moved.
// A script that executes two steps at once is the coalescing bug with a second
// thread instead of a second clock tick.
//
// The loser returns rather than waits. Whoever holds it is doing the work, and
// the script will be looked at again on the next clock read.
std::atomic<bool> g_ticking{false};

struct TickGuard {
    bool held;
    TickGuard() {
        bool expected = false;
        held = g_ticking.compare_exchange_strong(expected, true, std::memory_order_acq_rel);
    }
    ~TickGuard() {
        if (held)
            g_ticking.store(false, std::memory_order_release);
    }
};

// Advance scripted input from the guest heartbeat, checking waits and assertions.
// Separate press and release turns give the guest input workers a chance to observe both.
void tick() {
    TickGuard guard;
    if (!guard.held)
        return;
    if (!boot_close_requested())
        boot_present_windows();
    if (!g_script_started) {
        g_script_started = true;
        g_script_start_ms = boot_guest_millis();
    }
    if (g_touch.active()) {
        std::vector<TouchAction> actions;
        g_touch.tick(uint64_t(boot_guest_millis()) * 1000000ull, g_presents, &actions);
        deliver_touch_actions(actions);
        return; // a touch action cannot share a turn with the next script input
    }
    // A held button comes up on a later turn than it went down, so the guest
    // has a chance to see it down at all.
    //
    // And the turn ENDS here when it does. A release is an input event, and
    // this path does not go through the step loop, so without the return it
    // could share a turn with the next move - which is the same coalescing the
    // loop's rule exists to prevent, arriving by the one route that bypasses
    // it. impl-t7 named this as the path their fix did not cover; it is closed
    // here rather than left for the next twelve-run measurement to find.
    if (g_holding_button >= 0 && g_presents >= g_release_at_presents &&
        boot_guest_millis() - g_press_at_ms >= kClickHoldMs) {
        press_button((int)g_holding_button, false);
        g_holding_button = -1;
        return;
    }
    // The same for a guestclick, and the same reason for the return: a release
    // is an input event and must not share a turn with what comes next.
    if (g_guestclick_held && g_presents >= g_guestclick_release_at) {
        host_gate_inject_guest_click(g_guestclick_x, g_guestclick_y, (int)g_guestclick_button,
                                     false);
        g_guestclick_held = false;
        return;
    }
    uint32_t elapsed = boot_guest_millis() - g_script_start_ms;
    while (g_next_step < g_step_count && g_steps[g_next_step].at_ms <= elapsed) {
        if (g_holding_button >= 0)
            break; // wait for the click to finish
        if (g_guestclick_held)
            break; // and for a guestclick's release
        const HostScriptStep &step = g_steps[g_next_step];
        // An await holds the script here, and holds the CLOCK with it: every
        // step after it is timed from the moment it passed, so the waits that
        // follow keep the spacing they were written with however long the game
        // took to get here.
        if (step.op == HOST_SCRIPT_AWAIT) {
            if (!g_await_started) {
                g_await_started = true;
                g_await_since = boot_guest_millis();
                g_await_true = false;
            }
            double now = metric(step.name);
            uint32_t at = boot_guest_millis();
            if (step.at_least ? now >= step.threshold : now > step.threshold) {
                if (!g_await_true) {
                    g_await_true = true;
                    g_await_true_since = at;
                    g_await_true_presents = g_presents;
                }
            } else {
                g_await_true = false;
            }
            // The claim has to be true now and to have been true for the hold,
            // so a value that only passed through the threshold on its way
            // somewhere else does not answer the wait. The hold is FRAMES: a
            // fade is a sequence of frames, and only frames can outlast one.
            uint32_t need = host_script_hold_frames(step.hold_ms, host_pinned_clock_step());
            uint32_t held = g_presents - g_await_true_presents;
            uint32_t held_ms = at - g_await_true_since;
            // BOTH, and for different reasons. Frames, because a fade is a
            // sequence of frames and time alone can be satisfied by a guest
            // spinning on the clock without drawing. Time as well, because an
            // unpinned run presents far faster than 50 ms a frame, so frames
            // alone would let `for 2000` be answered in a fraction of the two
            // seconds Gate B was written to wait for.
            bool passed = g_await_true && held >= need && held_ms >= step.hold_ms;
            uint32_t waited = at - g_await_since;
            if (!passed && waited < step.timeout_ms)
                break;
            // The value reached is printed whether the wait passed or not. A
            // timeout that says only "did NOT hold above 100" leaves the
            // reader unable to tell a claim that never moved from one that
            // nearly made it, and choosing a turn to anchor a reference dump
            // needs the number rather than the verdict.
            printf("[smoke] %s %s%s %g (reached %g) for %u frames and %u ms "
                   "(%u ms asked) after %.1fs\n",
                   step.name, passed ? "held above" : "did NOT hold above",
                   step.at_least ? " or equal to" : "", step.threshold, now, held, held_ms,
                   step.hold_ms, waited / 1000.0);
            fflush(stdout);
            if (!passed)
                ++g_await_timeouts;
            if (!passed && !strcmp(step.name, "dumpat_fired")) {
                g_await_started = false;
                g_quit_requested = true;
                boot_request_close("script readiness assertion timed out");
                break; // never inject dependent input or replace an unfired dump
            }
            g_script_start_ms += waited; // the clock waited too
            // ...and so does this loop's read of it. Without this line the
            // steps after an await are all due at once, because `elapsed` was
            // measured before the clock moved: the corner pin, the move onto
            // New Game and the click on it then happen in a single host turn,
            // the two moves coalesce into one delta that lands nowhere, and
            // the run fails exactly as it did with a wait that was too short.
            elapsed = boot_guest_millis() - g_script_start_ms;
            g_await_started = false;
            ++g_next_step;
            continue;
        }
        run_step(step);
        ++g_next_step;
        // ONE INPUT-PRODUCING STEP PER HOST TURN.
        //
        // The guest reads the mouse once a turn and sees a DELTA. Two moveby
        // steps in one turn are therefore not two moves: the shim sends their
        // sum, and `moveby -2000 -2000` followed by `moveby 320 140` becomes a
        // single delta of (-1680, -1860), which leaves the pointer pinned in
        // the corner instead of on New Game. The click that follows lands on
        // nothing, the menu is still up to receive the next one, and the run
        // ends on the LOAD GAME screen. impl-t7 traced it: failing runs
        // announce exactly one input change fewer than passing ones, 29
        // against 30, and inserting a dump between the two moves - which costs
        // time inside the tick and so separates them into different turns -
        // makes it vanish in five runs of five.
        //
        // Why it needs a rule rather than a longer wait: under the pin the
        // script clock moves 50 ms per presented frame, so a single tick can
        // cross a 400 ms gap in one step. Widening every short wait to 2500 ms
        // does not fix it, and no choice of step size can, because a gap a
        // single tick swallows is not a gap. Unpinned the clock changes every
        // millisecond and the script gets a turn between the two moves every
        // time, which is why unpinned runs never showed it.
        //
        // Steps that produce no input - dump, peek, expect, readfile -
        // still drain in the same turn, so this costs a script nothing except
        // where it matters. It also subsumes the click case: the
        // `g_holding_button >= 0` break above is one instance of this rule.
        if (produces_input(step.op))
            break;
    }
    // A script that ran out without saying quit still has to end the run.
    if (g_next_step >= g_step_count && !g_touch.active() && !boot_close_requested())
        boot_request_close("the script ended");
}

// The runtime calls this on the run thread whenever the guest is about to
// block. Without it the run thread parks in the scheduler, nothing reads the
// clock, and the script never advances a single step - which is exactly what
// this host did before it had one.
extern std::atomic<bool> g_ticking;

// Has the script work that is due right now?
//
// Asked by the SCHEDULER while it holds its own mutex, so this only looks: no
// lock, no allocation, no guest call. Everything it reads is either a plain
// int written by the run thread or an atomic.
//
// It has to be exact rather than optimistic. The scheduler drains whenever
// this says yes and then asks again, so a `yes` that the drain does not
// consume is a spin. A held button says yes only once its release is actually
// owed, and a script waiting for its next step says no and lets the scheduler
// sleep on its own deadline.
bool script_pending() {
    if (!g_quit_requested && !g_ticking.load(std::memory_order_acquire) && g_touch.active())
        return g_touch.pending(uint64_t(boot_guest_millis()) * 1000000ull, g_presents);
    HostScriptDrainState st;
    memset(&st, 0, sizeof st);
    st.quit_requested = g_quit_requested ? 1 : 0;
    st.ticking = g_ticking.load(std::memory_order_acquire) ? 1 : 0;
    st.holding_button = g_holding_button >= 0 ? 1 : 0;
    st.hold_reached = g_presents >= g_release_at_presents ? 1 : 0;
    st.guestclick_held = g_guestclick_held ? 1 : 0;
    st.guestclick_reached = g_presents >= g_guestclick_release_at ? 1 : 0;
    st.await_started = g_await_started ? 1 : 0;
    st.script_started = g_script_started ? 1 : 0;
    st.steps_left = g_next_step < g_step_count ? 1 : 0;
    st.step_due = g_next_step < g_step_count && g_script_started &&
                  g_steps[g_next_step].at_ms <= boot_guest_millis() - g_script_start_ms;
    return host_script_drain_wanted(&st) != 0;
}

void script_drain() {
    tick();
}

// The scheduler's idle slice, which is NOT a safe place to touch the guest.
//
// Another guest thread may hold the baton and be running guest code for the
// whole of this slice, so input applied here races it and a mod callback
// dispatched here runs inside whatever that thread is doing. This used to call
// tick() directly, which did both.
//
// So it applies nothing. It says the script has work, which wakes a thread
// parked with a deadline of up to a second, and the scheduler drains it at
// kernel32.cpp's checkpoint - where it takes the baton first, so the drain
// runs with nothing else in guest code. The tick still runs from the guest's
// own clock read as well; that path was always under the baton and is
// unchanged.
int idle_wait(double seconds) {
    (void)seconds;
    if (script_pending())
        sched_input_arrived();
    return 0;
}

// Report script progress and rendering evidence for this run.
// Shared metric formatters keep smoke reports comparable with the interactive host.
void report(FILE *out, bool abnormal) {
    (void)abnormal;
    fprintf(out, "\n== smoke run ==\n");
    fprintf(out, "stopped:            %s\n", boot_stop_reason());
    fprintf(out, "elapsed:            %.1fs\n", boot_elapsed());
    fprintf(out, "script:             %d of %d steps\n",
            g_next_step < g_step_count ? g_next_step : g_step_count, g_step_count);
    if (g_mode_w)
        fprintf(out, "display mode:       %dx%d %dbpp\n", g_mode_w, g_mode_h, g_mode_bpp);
    fprintf(out, "presented frames:   %u (%u of the %u sampled differed from the one before)\n",
            g_presents, g_present_changes, g_present_samples);
    if (g_renderer) {
        auto hd = g_renderer->hdTextureStats();
        fprintf(out,
                "HD textures:        %llu world draws, %llu loads, %llu hits, %llu refused, %llu / "
                "%llu bytes\n",
                (unsigned long long)hd.draws, (unsigned long long)hd.loads,
                (unsigned long long)hd.hits, (unsigned long long)hd.refused,
                (unsigned long long)hd.resident_bytes, (unsigned long long)hd.budget_bytes);
    }
    fprintf(out, "Direct3D:           %u draws, %u textures, %u write-backs\n",
            host_d3d_total_draws(), host_d3d_total_textures(), host_d3d_total_flushes());
    fprintf(out, "input:              %u changes announced, %u reads by the guest\n",
            host_input_notify_count(), host_input_read_count());
    fprintf(out, "audio:              %u plays, loudest sample %.3f\n", g_audio_plays,
            g_audio_peak);
    fprintf(out, "non-black:          scene best %.3f (at a dump %.3f), presented %.3f\n",
            host_d3d_peak_nonblack(), g_scene_nonblack, g_present_nonblack);
    // The two lines the display baseline is made of, in the same words the
    // windowed host uses, because display_compare.py parses the text and a
    // second copy of the formatter would drift from this one.
    {
        char line[768];
        if (host_stats_gameplay_line(line, sizeof line))
            fprintf(out, "gameplay: %s\n", line);
        if (host_stats_access_line(line, sizeof line))
            fprintf(out, "%s\n", line);
    }
    boot_print_dx_objects(out);
    boot_print_exit_code(out);
    boot_print_undeliverable(out);
    fflush(out);
}

} // namespace

// ---------------------------------------------------------------------------
// The host callbacks. Nothing here opens a window or an audio device.
// ---------------------------------------------------------------------------
extern "C" void host_present_mode(int *w, int *h, int *bpp) {
    if (w)
        *w = g_mode_w;
    if (h)
        *h = g_mode_h;
    if (bpp)
        *bpp = g_mode_bpp;
}

extern "C" void host_set_display_mode(int w, int h, int bpp) {
    g_mode_w = w;
    g_mode_h = h;
    g_mode_bpp = bpp;
    // The armed claim, answered here because this is the moment the game
    // applies a mode - when it recreates its surfaces at a level start, not
    // when the options screen is left.
    printf("[smoke] display mode %dx%d %dbpp\n", w, h, bpp);
    fflush(stdout);
}

// Measure a presented guest frame and advance deterministic smoke time.
// Capture and UI instrumentation operate on copies, preserving the game surface.
extern "C" void host_present(const void *pixels, int w, int h, int bpp, const uint32_t *palette,
                             int pitch) {
    // Match the app's first-present initialization so scripted F10 presses
    // reach the settings page even when the scene has no CPU pixels.
    host_page_overlay(nullptr, 0, 0, 0, 0, nullptr);
    if (bpp == 8 || bpp == 16)
        boot_note_primary_present();
    ++g_presents;
    // The frame boundary, and so the one thing that moves a pinned clock.
    // Counted even for a frame this host will not look at: what the guest is
    // told the time is must not depend on whether the host could read the
    // picture.
    boot_clock_advance();
    if (!pixels || w <= 0 || h <= 0 || (bpp != 8 && bpp != 16 && bpp != 32))
        return;
    const uint8_t *frame = (const uint8_t *)pixels;
    std::vector<uint8_t> rgba((size_t)w * (size_t)h * 4);
    if (bpp == 8)
        host_present_expand_indexed(frame, w, h, pitch, palette, rgba.data());
    else if (bpp == 16)
        host_present_expand_rgb565(frame, w, h, pitch, rgba.data());
    else
        for (int y = 0; y < h; ++y)
            for (int x = 0; x < w; ++x) {
                const auto *row = reinterpret_cast<const uint32_t *>(frame + size_t(y) * pitch);
                size_t i = size_t(y) * w + x;
                rgba[4 * i] = uint8_t(row[x] >> 16);
                rgba[4 * i + 1] = uint8_t(row[x] >> 8);
                rgba[4 * i + 2] = uint8_t(row[x]);
                rgba[4 * i + 3] = 255;
            }
    if (bpp != 32) {
        std::vector<uint32_t> composed(size_t(w) * h);
        for (size_t i = 0; i < composed.size(); ++i)
            composed[i] = 0xff000000u | uint32_t(rgba[4 * i]) << 16 |
                          uint32_t(rgba[4 * i + 1]) << 8 | rgba[4 * i + 2];
        gdi_composite_windows(composed.data(), w, h);
        for (size_t i = 0; i < composed.size(); ++i) {
            rgba[4 * i] = uint8_t(composed[i] >> 16);
            rgba[4 * i + 1] = uint8_t(composed[i] >> 8);
            rgba[4 * i + 2] = uint8_t(composed[i]);
        }
    }
    // RECOMP_PRESENT_TRACE=1 names every frame that reaches the screen by its
    // geometry and depth, which is what tells one presenter from another: the
    // window compositor arrives 32-bit, a DirectDraw primary at the mode's
    // depth, a media session at the mode's size in 16. "blank" is the frame
    // being wholly black, so a picture that is overwritten can be told from a
    // picture that was never drawn.
    if (recomp_env("PRESENT_TRACE")) {
        bool blank = true;
        for (size_t i = 0, n = (size_t)w * (size_t)h * 4; i < n && blank; i += 4)
            if (rgba[i] || rgba[i + 1] || rgba[i + 2])
                blank = false;
        fprintf(stderr, "[present] #%llu %dx%d %dbpp %s\n", (unsigned long long)g_presents, w, h,
                bpp, blank ? "blank" : "picture");
    }
    host_present_stage_rgba(rgba.data(), w, h);
    g_last_rgb.resize((size_t)w * (size_t)h * 3);
    ++g_last_rgb_version;
    for (size_t i = 0, n = (size_t)w * (size_t)h; i < n; ++i) {
        g_last_rgb[i * 3 + 0] = rgba[i * 4 + 0];
        g_last_rgb[i * 3 + 1] = rgba[i * 4 + 1];
        g_last_rgb[i * 3 + 2] = rgba[i * 4 + 2];
    }
    g_last_w = w;
    g_last_h = h;
    // Optional frame-clock capture uses the same sampling switch as headless.
    const char *frames = recomp_env("FRAMES");
    const char *every_spec = recomp_env("FRAME_EVERY");
    unsigned every = every_spec ? unsigned(strtoul(every_spec, nullptr, 10)) : 1;
    if (frames && *frames && every && (g_presents - 1) % every == 0) {
        std::string dir(frames);
        for (size_t i = 1; i <= dir.size(); ++i)
            if (i == dir.size() || dir[i] == '/' || dir[i] == '\\')
                os_mkdir(dir.substr(0, i).c_str());
        char path[1024];
        snprintf(path, sizeof path, "%s/frame_%04u.ppm", frames, g_presents - 1);
        host_write_ppm(path, g_last_rgb.data(), w, h);
    }

    // How much of this frame is not black, and how much of it is new, from one
    // sample of it. A script waits on these instead of on a stopwatch.
    {
        std::vector<uint8_t> sample;
        sample.reserve(g_last_rgb.size() / 16 + 1);
        size_t lit = 0, seen = 0;
        for (size_t i = 0; i + 2 < g_last_rgb.size(); i += 3 * 16) {
            ++seen;
            uint8_t r = g_last_rgb[i], g = g_last_rgb[i + 1], b = g_last_rgb[i + 2];
            if (r > 8 || g > 8 || b > 8)
                ++lit;
            // One byte per sampled pixel, because what is being compared is
            // whether this is the same picture, not how it was shaded.
            sample.push_back((uint8_t)((r + g + b) / 3));
        }
        g_picture = seen ? (double)lit / (double)seen : 0.0;
        if (g_prev_sample.size() == sample.size() && !sample.empty()) {
            size_t moved = 0;
            for (size_t i = 0; i < sample.size(); ++i)
                if (abs((int)sample[i] - (int)g_prev_sample[i]) > 12)
                    ++moved;
            g_moved = (double)moved / (double)sample.size();
            // A quarter is far above the few per cent an animated menu or a
            // blinking cursor moves and far below the two thirds one screen
            // replacing another does.
            if (moved * 4 > sample.size())
                ++g_screen_changes;
            // Two per cent is above the cursor and the odd repainted word and
            // below any fade, which moves the whole frame at once.
            if (g_moved > 0.02 || !g_still_since_ms)
                g_still_since_ms = boot_guest_millis();
        } else {
            g_still_since_ms = boot_guest_millis();
        }
        g_prev_sample.swap(sample);
    }
    if (g_trace && boot_guest_millis() >= g_trace_next_ms) {
        g_trace_next_ms = boot_guest_millis() + 500;
        printf("[trace] %6.2fs picture %.4f moved %.4f still %.2fs screens %u "
               "presents %u scene %.3f textures %u draws %u\n",
               boot_guest_millis() / 1000.0, g_picture, g_moved, metric("still_ms") / 1000.0,
               g_screen_changes, g_presents, host_d3d_peak_nonblack(), host_d3d_total_textures(),
               host_d3d_total_draws());
        fflush(stdout);
    }

    // Drawable mode completes at the eligible seal, not an interim primary
    // surface refresh. Both paths publish BODY records before firing dumpat.
    if (!recomp_env("SMOKE_DRAWABLE")) {
        ++g_completed_presents;
        fire_dumpat(host_frame_class(host_frame_current()), false);
    } else {
    }

    // A cheap hash over a sample of the frame: enough to tell one picture from
    // the next without walking every pixel of every frame.
    if (g_last_rgb_version != g_hashed_version) {
        g_hashed_version = g_last_rgb_version;
        ++g_present_samples;
        uint64_t hash = 1469598103934665603ull;
        for (size_t i = 0; i < g_last_rgb.size(); i += 997) {
            hash ^= g_last_rgb[i];
            hash *= 1099511628211ull;
        }
        if (hash != g_last_frame_hash) {
            ++g_present_changes;
            g_last_frame_hash = hash;
        }
    }
}

// A GDI-only frame has no DirectDraw recorder to seal it. Keep the smoke
// measurements, captures and pin on the same path as a primary present.
extern "C" void host_display_present_window(const uint32_t *argb, int w, int h) {
    host_present_first_write();
    host_present(argb, w, h, 32, nullptr, w * 4);
    host_present_seal_window();
}
// File/smoke sinks consume ARGB; retain their original raw-alpha behavior.
extern "C" void host_display_present_window_rgba(uint8_t *rgba, int w, int h) {
    if (!rgba || w <= 0 || h <= 0)
        return;
    static std::vector<uint32_t> argb;
    argb.resize(size_t(w) * h);
    for (size_t i = 0; i < argb.size(); ++i) {
        const uint8_t *p = rgba + i * 4;
        argb[i] = (uint32_t(p[3]) << 24) | (uint32_t(p[0]) << 16) | (uint32_t(p[1]) << 8) | p[2];
    }
    host_display_present_window(argb.data(), w, h);
}

// The smoke's display is its drawable: RECOMP_SMOKE_DRAWABLE, when set.
extern "C" int host_display_screen_size(int *w, int *h) {
    const char *size = recomp_env("SMOKE_DRAWABLE");
    int sw = 0, sh = 0;
    char trailing = 0;
    if (!w || !h || !size || sscanf(size, "%dx%d%c", &sw, &sh, &trailing) != 2 || sw <= 0 ||
        sh <= 0)
        return 0;
    *w = sw;
    *h = sh;
    return 1;
}
namespace {

// The factory samples guest state at seal under the baton. Its closure owns
// only values; GPU completion writes the same frame's pixels and layout without
// reading guest state or waiting on the presenter from the guest thread.
HostFrameCapture fire_dumpat(HostScreenClass cls, bool at_seal) {
    if (!g_dumpat.armed)
        return {};
    const HostDumpAtSample sample{cls == HOST_SCREEN_GAMEPLAY, metric(g_dumpat.metric.c_str()),
                                  (uint32_t)metric("turn"),    (uint32_t)metric("command_frame"),
                                  g_completed_presents,        boot_guest_millis(),
                                  host_frame_current().id,     host_clock_description()};
    if (!host_dumpat_fire(g_dumpat, sample, host_dump_dir()))
        return {};
    const std::string base = std::string(host_dump_dir()) + "/smoke_" + g_dumpat.name;
    g_dumps.push_back(base + "_provenance.txt");
    write_dump(g_dumpat.name.c_str());
    if (!at_seal) {
        write_composite_dump(g_dumpat.name.c_str());
        return {};
    }
    return [base](const HostCompletedComposite &frame) {
        std::vector<uint8_t> rgb(size_t(frame.w) * frame.h * 3);
        for (size_t i = 0; i < rgb.size() / 3; ++i)
            memcpy(rgb.data() + i * 3, frame.rgba.data() + i * 4, 3);
        bool ok = host_write_ppm((base + "_composite.ppm").c_str(), rgb.data(), frame.w, frame.h);
        const auto &l = frame.layout;
        FILE *layout = fopen((base + "_layout.json").c_str(), "wb");
        if (layout) {
            fprintf(layout,
                    "{\"frame_id\":%llu,\"screen_class\":%d,\"guest\":[%d,%d],"
                    "\"drawable\":[%d,%d],\"legacy\":%s,\"classic\":%s,"
                    "\"scene_mapping\":[%.9g,%.9g,%.9g,%.9g,%d],\"elements\":[",
                    (unsigned long long)frame.frame_id, int(l.cls), l.guest_w, l.guest_h,
                    l.drawable_w, l.drawable_h, l.legacy ? "true" : "false",
                    l.classic ? "true" : "false", l.scene.scale_x, l.scene.scale_y,
                    l.scene.offset_x, l.scene.offset_y, l.scene.domain_w);
            bool first = true;
            for (const auto &e : l.elements) {
                fprintf(layout, "%s{\"id\":\"%llu\",\"rect\":[%d,%d,%d,%d]}", first ? "" : ",",
                        (unsigned long long)e.id, e.drawable.x, e.drawable.y, e.drawable.w,
                        e.drawable.h);
                first = false;
            }
            fprintf(layout, "]}\n");
            const bool written = !ferror(layout);
            ok = (fclose(layout) == 0) && written && ok;
        } else
            ok = false;
        if (ok)
            ++g_completed_captures;
        else {
            ++g_capture_failures;
            fprintf(stderr, "[smoke] anchored composite FAILED: %s\n", base.c_str());
        }
    };
}

HostFrameCapture capture_at_seal(HostScreenClass cls) {
    ++g_completed_presents;
    return fire_dumpat(cls, true);
}

void write_composite_dump(const char *name) {
    if (recomp_env("SMOKE_DRAWABLE")) {
        HostCompletedComposite frame;
        if (!host_present_copy_composite(&frame)) {
            fprintf(stderr, "[smoke] Drawable dumpc FAILED: no completed composition\n");
            ++g_probe_misses;
            return;
        }
        std::vector<uint8_t> rgb(size_t(frame.w) * frame.h * 3);
        for (size_t i = 0; i < rgb.size() / 3; ++i)
            memcpy(rgb.data() + i * 3, frame.rgba.data() + i * 4, 3);
        char path[1024];
        snprintf(path, sizeof path, "%s/smoke_%s_composite.ppm", host_dump_dir(), name);
        if (!host_write_ppm(path, rgb.data(), frame.w, frame.h)) {
            ++g_probe_misses;
            return;
        }
        g_dumps.push_back(path);
        printf("[smoke] Drawable dumpc completed frame=%llu class=%d guest=%dx%d drawable=%dx%d\n",
               (unsigned long long)frame.frame_id, int(frame.cls), frame.guest_w, frame.guest_h,
               frame.w, frame.h);
        return;
    }
    if (!g_last_w || !g_last_h) {
        printf("[smoke] dumpc %s: nothing has been presented yet\n", name);
        fflush(stdout);
        return;
    }
    char path[1024];
    snprintf(path, sizeof path, "%s/smoke_%s_composite.ppm", host_dump_dir(), name);
    if (host_write_ppm(path, g_last_rgb.data(), g_last_w, g_last_h))
        g_dumps.push_back(path);
}

// One pixel of the composited frame, against what the script said it would be.
void probe_pixel(const HostScriptStep &step) {
    int r = 0, g = 0, b = 0, worst = 0;
    const int verdict =
        host_probe_pixel(g_last_rgb.empty() ? nullptr : g_last_rgb.data(), g_last_w, g_last_h,
                         step.x, step.y, step.r, step.g, step.b, step.tol, &r, &g, &b, &worst);
    if (verdict == HOST_PROBE_NO_FRAME) {
        printf("[smoke] probe at (%d, %d): nothing has been presented yet\n", step.x, step.y);
        fflush(stdout);
        ++g_probe_misses;
        return;
    }
    if (verdict == HOST_PROBE_OUTSIDE) {
        printf("[smoke] probe at (%d, %d) is outside the %dx%d frame\n", step.x, step.y, g_last_w,
               g_last_h);
        fflush(stdout);
        ++g_probe_misses;
        return;
    }
    const bool ok = verdict == HOST_PROBE_MATCH;
    // Said whether it passed or not, and with what was actually there. A probe
    // that only speaks up when it fails makes a passing script unreadable to
    // anyone trying to choose the next probe's colour.
    printf("[smoke] probe (%d, %d) %s: wanted %d %d %d, found %d %d %d, "
           "worst channel off by %d, tolerance %d\n",
           step.x, step.y, ok ? "ok" : "FAILED", step.r, step.g, step.b, r, g, b, worst, step.tol);
    fflush(stdout);
    if (!ok)
        ++g_probe_misses;
}

} // namespace

// The audio question a smoke run can actually answer: was the guest handed
// something with sound in it. No device is opened and nothing is heard.
extern "C" void host_audio_play(const HostAudioPlay *p) {
    ++g_audio_plays;
    if (!p || !p->pcm || !p->bytes)
        return;
    int channels = p->channels == 2 ? 2 : 1;
    int bits = p->bits == 8 ? 8 : 16;
    uint32_t frame = host_audio_frame_bytes(bits, channels);
    if (!frame)
        return;
    if (p->channel >= 0 && p->channel < SMOKE_CHANNELS) {
        SmokeChannel &ch = g_channels[p->channel];
        ch.playing = true;
        ch.started = boot_guest_millis();
        ch.rate = (uint32_t)(p->sample_rate > 0 ? p->sample_rate : 22050);
        ch.bits = bits;
        ch.channels = channels;
        ch.total = p->bytes;
        ch.start_offset = p->start_offset < p->bytes ? p->start_offset : 0;
        ch.loop = p->loop != 0;
    }
    uint32_t frames = p->bytes / frame;
    if (frames > 65536)
        frames = 65536; // a tenth of a second is plenty
    std::vector<float> left(frames), right(frames);
    uint32_t got = host_audio_decode_pcm(p->pcm, frames * frame, bits, channels, left.data(),
                                         right.data(), frames);
    for (uint32_t i = 0; i < got; ++i) {
        double a = left[i] < 0 ? -left[i] : left[i];
        double b = right[i] < 0 ? -right[i] : right[i];
        if (a > g_audio_peak)
            g_audio_peak = a;
        if (b > g_audio_peak)
            g_audio_peak = b;
    }
}
extern "C" void host_audio_stop(int32_t channel) {
    if (channel >= 0 && channel < SMOKE_CHANNELS)
        g_channels[channel].playing = false;
}
extern "C" void host_audio_set_volume(int32_t, int32_t) {}
extern "C" void host_audio_set_pan(int32_t, int32_t) {}
extern "C" void host_audio_set_frequency(int32_t, uint32_t) {}
extern "C" int32_t host_audio_is_playing(int32_t channel) {
    if (channel < 0 || channel >= SMOKE_CHANNELS)
        return 0;
    host_audio_position(channel); // retires a finished one-shot
    return g_channels[channel].playing ? 1 : 0;
}

// The play cursor still has to advance, or the video player waits for ever.
// That is also why this is a clock poll: the cursor is modelled on the guest's
// clock, so a guest spinning here while a pinned clock waits for a frame is
// waiting for itself.
extern "C" uint32_t host_audio_position(int32_t channel) {
    boot_clock_poll();
    if (channel < 0 || channel >= SMOKE_CHANNELS)
        return 0;
    SmokeChannel &ch = g_channels[channel];
    if (!ch.playing || !ch.total)
        return 0;
    double elapsed = (boot_guest_millis() - ch.started) / 1000.0;
    uint32_t at = host_audio_wall_clock_bytes(elapsed, ch.rate, ch.bits, ch.channels,
                                              ch.start_offset, ch.total, ch.loop ? 1 : 0);
    if (!ch.loop && at >= ch.total)
        ch.playing = false;
    return at;
}

// How much of what the voice is playing is still to play.
//
// The QMixer shim's refill gate reads this, and without an implementation here
// it fell through to the weak default of zero - so in the smoke every look at
// the gate said the voice had nothing left and refilled a channel that needed
// nothing. The counters showed it: about 430 refills in a run that plays 86
// sounds, five per sound where the wave is four buffers long.
//
// Modelled on the same cursor host_audio_position uses, which is the guest's
// clock and not the wall, so a pinned run's refills are as reproducible as the
// rest of it. That is not what the app host does - the audio device's own cursor
// answers there, because a device consumes at its own rate
// whatever the guest clock says - and the difference is the whole reason a
// pinned smoke run has a fixed sound count while an unpinned app run does not.
//
// A looping buffer reports the bytes to the end of the current lap. It has
// more after that by definition, but "how much is in front of the voice" is
// what the caller is asking, and a loop that answered "unbounded" would tell
// a refiller never to refill.
extern "C" uint32_t host_audio_voice_remaining_bytes(int32_t channel) {
    if (channel < 0 || channel >= SMOKE_CHANNELS)
        return 0;
    SmokeChannel &ch = g_channels[channel];
    if (!ch.playing || !ch.total)
        return 0;
    uint32_t at = host_audio_position(channel); // also retires a finished one-shot
    if (!ch.playing)
        return 0; // it just finished
    return at < ch.total ? ch.total - at : 0;
}

// ---------------------------------------------------------------------------

int main(int argc, char **argv) {
    if (argc > 0 && argv[0])
        g_argv0 = argv[0];
    g_trace = recomp_env("SMOKE_TRACE") != nullptr;
    const char *script_path = recomp_env("SCRIPT");
    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--script") && i + 1 < argc)
            script_path = argv[++i];
    }
    if (!script_path) {
        fprintf(stderr, "smoke: RECOMP_SCRIPT=<file> or --script <file>\n");
        return 2;
    }
    FILE *f = fopen(script_path, "rb");
    if (!f) {
        fprintf(stderr, "smoke: cannot open %s\n", script_path);
        return 2;
    }
    std::string text;
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0)
        text.append(buf, n);
    fclose(f);

    char error[256];
    g_step_count = host_script_parse(text.c_str(), g_steps, MAX_STEPS, error, sizeof error);
    if (g_step_count < 0) {
        fprintf(stderr, "smoke: %s: %s\n", script_path, error);
        return 2;
    }
    printf("[smoke] %s: %d steps\n", script_path, g_step_count);

    {
        auto gpu_device = gpu::create_default_device();
        if (!gpu_device) {
            fprintf(stderr, "smoke: no GPU device\n");
            return 3;
        }
        fprintf(stderr, "GPU backend: %s\n", gpu::default_backend_name());
        g_renderer = new D3DRenderer(gpu_device.get());
        host_present_set_device(gpu_device.get());
        if (!g_renderer->ok()) {
            fprintf(stderr, "smoke: no renderer\n");
            return 3;
        }
        D3DRenderer::setShared(g_renderer);
        int drawable_w = 640, drawable_h = 480;
        if (const char *size = recomp_env("SMOKE_DRAWABLE")) {
            char trailing = 0;
            if (sscanf(size, "%dx%d%c", &drawable_w, &drawable_h, &trailing) != 2 ||
                drawable_w <= 0 || drawable_h <= 0 || drawable_w > 16384 || drawable_h > 16384) {
                fprintf(stderr, "smoke: invalid RECOMP_SMOKE_DRAWABLE: %s\n", size);
                return 2;
            }
        }
        host_display_set_screen(drawable_w, drawable_h);
        host_present_start_offscreen(drawable_w, drawable_h);
        g_touch_drawable_w = drawable_w;
        g_touch_drawable_h = drawable_h;
        if (recomp_env("SMOKE_DRAWABLE"))
            host_present_set_capture_factory(capture_at_seal);
        host_input_set_notify(dinput_host_input_changed);

        BootOptions options;
        options.name = "smoke";
        options.tick = tick;
        options.idle_wait = idle_wait;
        // The queue the scheduler drains under the baton. Registered before
        // the guest runs, so the very first step goes through the same path
        // as every later one.
        sched_set_input_queue(script_pending, script_drain);
        options.report = report;
        // A smoke run is not allowed to hang: the script's own end closes it,
        // and these are the backstops for a guest that will not go.
        // RECOMP_SMOKE_SECONDS raises the limit for slow renderers (software Vulkan).
        options.deadline_seconds = 180.0;
        if (const char *s = recomp_env("SMOKE_SECONDS"); s && atof(s) > 0)
            options.deadline_seconds = atof(s);
        options.deadline_grace = 20.0;
        options.close_unwind_grace = 10.0;

        if (!boot_load(options)) {
            host_present_stop();
            fprintf(stderr, "smoke: %s\n", loader_error());
            return 2;
        }
        printf("[smoke] %s, entry %08x\n", loader_exe_path().c_str(), loader_entry_point());
        fflush(stdout);
        boot_run();
        if (!sched_guest_threads_stopped()) {
            fprintf(stderr,
                    "presenter: guest workers outlived shutdown; refusing unsafe presenter join\n");
            fflush(nullptr);
            _Exit(4);
        }
        host_present_stop();
        report(stdout, false);
    }

    // The expectations, all of them, so one run says everything that is wrong
    // rather than the first thing.
    int failed = 0;
    for (int i = 0; i < g_step_count; ++i) {
        if (g_steps[i].op != HOST_SCRIPT_EXPECT)
            continue;
        double got = metric(g_steps[i].name);
        if (got < 0) {
            printf("EXPECT %-18s UNKNOWN METRIC\n", g_steps[i].name);
            ++failed;
            continue;
        }
        bool ok = got > g_steps[i].threshold;
        printf("EXPECT %-18s %s  wanted > %-8g got %g\n", g_steps[i].name, ok ? "ok    " : "FAILED",
               g_steps[i].threshold, got);
        if (!ok)
            ++failed;
    }
    // A run that stopped early has not proved what its script says. The
    // expectations it never reached cannot fail, so without this a truncated
    // run reads as cleaner than a complete failure does.
    if (host_script_run_unfinished(boot_abnormal_exit() ? 1 : 0, g_next_step, g_step_count)) {
        printf("\n%s: %d of %d steps ran\n",
               boot_abnormal_exit() ? "the run ended abnormally" : "the script did not finish",
               g_next_step < g_step_count ? g_next_step : g_step_count, g_step_count);
        ++failed;
    }
    if (recomp_env("SMOKE_DRAWABLE") &&
        (g_completed_captures.load() != g_dumpat.fired || g_capture_failures.load())) {
        fprintf(stderr,
                "smoke: anchored composite FAILED: %u requested, %u completed, %u write failures\n",
                g_dumpat.fired, g_completed_captures.load(), g_capture_failures.load());
        ++failed;
    }
    if (g_probe_misses) {
        printf("\n%d probe%s found the wrong colour\n", g_probe_misses,
               g_probe_misses == 1 ? "" : "s");
        failed += g_probe_misses;
    }
    if (g_window_mapping_failures) {
        printf("%d window mapping failures\n", g_window_mapping_failures);
        failed += g_window_mapping_failures;
    }
    // An armed dumpat that never fired means the reference frame it was for
    // does not exist, and a run that quietly produced no reference is worse
    // than one that failed: the next comparison would read the previous run's
    // file and call it today's.
    const uint32_t g_dumpat_unfired = g_dumpat.unfired();
    failed += g_dumpat.write_failures;
    if (g_dumpat_unfired) {
        printf("\n%d dumpat%s never fired; the reference frame%s missing\n", g_dumpat_unfired,
               g_dumpat_unfired == 1 ? "" : "s", g_dumpat_unfired == 1 ? " is" : "s are");
        failed += g_dumpat_unfired;
    }
    if (g_readfile_misses) {
        printf("\n%d readfile step%s did not find what it wanted\n", g_readfile_misses,
               g_readfile_misses == 1 ? "" : "s");
        failed += g_readfile_misses;
    }
    // An await that gave up is a failure in its own right, and it has to be
    // said here: everything the script did afterwards was done to a game that
    // was not where the script believed it was, so the assertions below it are
    // measuring the wrong thing and their verdict cannot be trusted either.
    if (g_await_timeouts) {
        printf("\n%d await%s timed out; every step after one ran against a "
               "game the script had lost track of\n",
               g_await_timeouts, g_await_timeouts == 1 ? "" : "s");
        failed += g_await_timeouts;
    }
    if (!g_dumps.empty()) {
        printf("\ndumps:\n");
        for (const std::string &d : g_dumps)
            printf("    %s\n", d.c_str());
    }
    if (failed) {
        printf("\n%d expectation%s not met\n", failed, failed == 1 ? "" : "s");
        return 1;
    }
    printf("\nall expectations met\n");
    return 0;
}
