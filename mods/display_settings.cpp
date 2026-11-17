#include "display_settings.h"
#include "mods_internal.h"
#include "game_config.h"

#include <algorithm>
#include <atomic>

namespace {
const char *const kWindowChoices[] = {"windowed", "borderless", "fullscreen"};
const char *const kOverlayChoices[] = {"off", "counters", "graph"};
struct RowSpec {
    const char *key;
    int maximum;
    int count;
    const char *const *choices;
};
constexpr RowSpec kRows[DISPLAY_ROW_COUNT] = {
    {"window", 2, 3, kWindowChoices},
    {"performance_overlay", 2, 3, kOverlayChoices},
};
int desired[DISPLAY_ROW_COUNT] = {};
std::atomic<int> overlay{0};
int scene_w = 0, scene_h = 0; // guest baton only
bool initialized = false;
} // namespace
void mods_display_reset() {
    initialized = false;
    for (int &value : desired)
        value = 0;
    overlay = 0;
    scene_w = scene_h = 0;
}
void mods_display_live_defaults() {
    if (initialized)
        return;
    desired[DISPLAY_OVERLAY] = 2;
}
// The overlay a fresh profile starts with, when the host wants something
// other than the live default: a player build starts without the graph.
void mods_display_default_overlay(int mode) {
    if (initialized)
        return;
    desired[DISPLAY_OVERLAY] = std::clamp(mode, 0, 2);
}
namespace {
#ifdef POPM_TESTING
unsigned listed_rows = (2u << DISPLAY_CONTROLS_BIT) - 1;
#else
const unsigned listed_rows = RECOMP_SETTINGS_ROWS;
#endif
} // namespace
bool mods_settings_row_listed(int bit) {
    return bit >= 0 && bit <= DISPLAY_CONTROLS_BIT && (listed_rows >> bit & 1);
}
#ifdef POPM_TESTING
void mods_settings_rows_for_test(unsigned mask) {
    listed_rows = mask;
}
#endif
// Declare display settings, restore saved values within their supported ranges and publish host state.
void mods_display_init() {
    if (initialized)
        return;
    initialized = true;
    for (int i = 0; i < DISPLAY_ROW_COUNT; ++i) {
        const RowSpec &row = kRows[i];
        mods_settings_declare(MODS_OWNER_RUNTIME, "host.display", row.key, row.key, POP_SETTING_INT,
                              desired[i], 0, row.maximum);
        int64_t value = desired[i];
        mods_settings_get(MODS_OWNER_RUNTIME, row.key, &value);
        desired[i] = int(std::clamp<int64_t>(value, 0, row.maximum));
    }
    overlay = desired[DISPLAY_OVERLAY];
    host_display_request_window(desired[DISPLAY_WINDOW]);
}
bool mods_display_row_applies(DisplayRow row) {
    return mods_settings_row_listed(row);
}
int mods_display_value(DisplayRow row) {
    return desired[row];
}
PopModStatus mods_display_nudge(DisplayRow row, int delta) {
    if (row < 0 || row >= DISPLAY_ROW_COUNT)
        return POP_E_RANGE;
    const RowSpec &spec = kRows[row];
    if (!spec.count)
        return POP_E_RANGE;
    const int current = mods_display_value(row);
    const int value = (current + delta + spec.count) % spec.count;
    return mods_display_set(row, value);
}
// Validate and persist a display change on the owning thread, then update its live host service.
PopModStatus mods_display_set(DisplayRow row, int value) {
    if (!mods_host_on_main_thread())
        return POP_E_WRONG_THREAD;
    if (row < 0 || row >= DISPLAY_ROW_COUNT)
        return POP_E_INVAL;
    if (!mods_settings_row_listed(row))
        return POP_E_STATE; // this game has no such row
    const RowSpec &spec = kRows[row];
    if (value < 0 || value > spec.maximum)
        return POP_E_RANGE;
    const auto status = mods_settings_set(MODS_OWNER_RUNTIME, spec.key, value);
    if (status != POP_OK)
        return status;
    desired[row] = value;
    if (row == DISPLAY_OVERLAY)
        overlay = value;
    if (row == DISPLAY_WINDOW)
        host_display_request_window(value);
    return POP_OK;
}
extern "C" void mods_display_transition(uint64_t next, int cls) {
    // The remaining display choices are fixed kit defaults, so there is no
    // render/projection change to publish at the boundary.
    (void)next;
    (void)cls;
}
// A game whose frames are presented as rendered, scaled to the window, with no
// separately rendered world layer.
extern "C" int mods_display_classic() {
    return RECOMP_RENDER_CLASSIC;
}
extern "C" int mods_display_scale() {
    return 0;
}
extern "C" int mods_display_wide() {
    return 1;
}
extern "C" void mods_display_scene_domain(int w, int h) {
    scene_w = w;
    scene_h = h;
}
extern "C" int mods_display_scene_width(int w, int h) {
    return scene_h == h ? std::max(w, scene_w) : w;
}
extern "C" int mods_display_textures() {
    return 1;
}
extern "C" int mods_display_filtering() {
    return 3;
}
extern "C" int mods_display_overlay() {
    return overlay.load();
}
std::string mods_display_line(DisplayRow row) {
    std::string result;
    switch (row) {
    case DISPLAY_WINDOW:
        result = std::string("Display: ") + kRows[DISPLAY_WINDOW].choices[desired[DISPLAY_WINDOW]];
        break;
    case DISPLAY_OVERLAY:
        result = std::string("Performance overlay: ") +
                 kRows[DISPLAY_OVERLAY].choices[desired[DISPLAY_OVERLAY]];
        break;
    default:
        return {};
    }
    return result;
}
