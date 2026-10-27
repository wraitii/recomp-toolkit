// controls_host.cpp - see controls_host.h.
#include "controls_host.h"

#include "../../mods/controls_settings.h"
#include "../../mods/mods_internal.h"
#include "../../platform/os.h"
#include "../../runtime/layout.h"
#include "../present.h"
#include "../sdl/platform_ui.h"
#include "editor.h"
#include "editor_actions.h"
#include "game_config.h"
#include "gamepad_sdl.h"
#include "haptics.h"
#include "layout_fallback.h"
#include "layout_store.h"
#include "overlay.h"
#include "router.h"
#include "vpad.h"

#include <filesystem>
#include <mutex>
#include <stdio.h>
#include <string>
#include <utility>
#include <vector>

extern "C" void dinput_feedback_tick();

namespace controls {

namespace {

// The settings' key sizes: small, medium (the layout's own) and large.
constexpr double kSizePt[3] = {32, 36, 40};

// The desktop mouse, as one more finger of the editor's: no real touch id
// is negative, so it can never collide with one.
constexpr int64_t kMouseFinger = -1;

HostHooks g_hooks;
LayoutStore g_store;
Layout g_layout;
Router g_router;
Screen g_screen;
bool g_keyboard_absent = false;
bool g_controller_present = false;
bool g_game_focused = false;

// RECOMP_CONTROLS_TRACE: log the merged pad on every change, and each
// auto-hide decision.
// Read on first use, after the host has applied its environment file.
bool trace() {
    static const bool on = recomp_env("CONTROLS_TRACE") != nullptr;
    return on;
}

PadMapper g_mapper;
std::mutex g_mapper_view_mutex;
PadState g_mapper_raw;
std::string g_mapper_status;

void set_mapper_status(std::string status) {
    std::lock_guard<std::mutex> lock(g_mapper_view_mutex);
    g_mapper_status = std::move(status);
}

// Reads a whole file as text; false (leaving *out alone) when it cannot be
// opened, same contract as layout_store.cpp's own copy.
bool read_text_file(const std::string &path, std::string *out) {
    const int fd = os_fd_open(path.c_str(), OS_O_RDONLY);
    if (fd < 0)
        return false;
    out->clear();
    char buf[4096];
    int64_t n;
    while ((n = os_fd_read(fd, buf, sizeof buf)) > 0)
        out->append(buf, size_t(n));
    os_fd_close(fd);
    return n >= 0;
}

std::string mapper_path() {
    return std::string(mods_overlay_profile_dir()) + "/controls/mapper-v1.txt";
}

PadMapperProfile default_mapper_profile() {
    return pad_mapper_default_profile();
}

PadMapperProfile load_mapper_profile() {
    PadMapperProfile profile = default_mapper_profile();
    std::string text, error;
    const std::string path = mapper_path();
    if (!read_text_file(path, &text)) {
        OsStat stat{};
        if (os_stat(path.c_str(), &stat) == 0)
            set_mapper_status("Could not read controller profile file");
        return profile;
    }
    PadMapperProfile loaded;
    if (!parse_pad_mapper_profile(text, &loaded, &error)) {
        set_mapper_status("Could not load controller profile: " + error);
        fprintf(stderr, "[controls] bad %s: %s\n", path.c_str(), error.c_str());
        return profile;
    }
    return loaded;
}

bool save_mapper_profile(const PadMapperProfile &profile, std::string *error) {
    const std::string dir = std::string(mods_overlay_profile_dir()) + "/controls";
    std::error_code ec;
    std::filesystem::create_directories(dir, ec);
    if (ec) {
        if (error)
            *error = "could not create controls profile directory";
        return false;
    }
    const std::string text = serialize_pad_mapper_profile(profile, error);
    if (text.empty()) {
        if (error && error->empty())
            *error = "profile could not be serialized";
        return false;
    }
    const std::string path = mapper_path();
    const std::string temp = path + ".tmp";
    const int fd = os_fd_open(temp.c_str(), OS_O_WRONLY | OS_O_CREAT | OS_O_TRUNC);
    if (fd < 0) {
        if (error)
            *error = "could not open profile file";
        return false;
    }
    const int64_t written = os_fd_write(fd, text.data(), text.size());
    const int synced = os_fd_fsync(fd);
    os_fd_close(fd);
    if (written != int64_t(text.size()) || synced != 0 ||
        os_rename(temp.c_str(), path.c_str()) != 0) {
        os_unlink(temp.c_str());
        if (error)
            *error = "could not replace profile file";
        return false;
    }
    return true;
}

// What g_layout was loaded and sized for.
bool g_loaded = false;      // a load has been attempted
bool g_have_layout = false; // g_layout holds a layout the router drives
std::string g_loaded_name;
Form g_loaded_form = Form::Tablet;
double g_file_scale = 1.0;
int g_size = -1;
bool g_enabled = false;
LayoutContent g_content = LayoutContent::Keys; // g_layout's, for auto-hide

bool g_published = false;
bool g_published_wanted = false;
uint64_t g_published_revision = 0;

// The size setting's multiplier over a layout file's own scale: the layout
// the router and the editor work on is g_file_scale * this.
double size_factor() {
    int size = mods_controls_value(CONTROLS_SIZE_ROW);
    size = size < 0 ? 0 : size > 2 ? 2 : size;
    return kSizePt[size] / 36.0;
}

// The groups the player has hidden, as the settings row stores them: bit i
// is groups[i], for the first kHiddenBits groups (layout_fallback.h).
uint32_t hidden_bits(const Layout &l) {
    uint32_t bits = 0;
    for (size_t i = 0; i < l.groups.size() && i < kHiddenBits; ++i)
        if (!l.groups[i].visible)
            bits |= 1u << i;
    return bits;
}

// Which lane of the settings' hidden-group bits a form factor owns. The
// forms of one layout name need not have the same groups, so each keeps its
// own bits: hiding the portrait keyboard must not hide the landscape one's
// left half after a rotation.
ControlsForm settings_form(Form f) {
    switch (f) {
    case Form::PhoneLandscape:
        return CONTROLS_FORM_PHONE_LANDSCAPE;
    case Form::PhonePortrait:
        return CONTROLS_FORM_PHONE_PORTRAIT;
    case Form::Tablet:
        break;
    }
    return CONTROLS_FORM_TABLET;
}

void apply_hidden_bits(Layout &l, uint32_t bits) {
    for (size_t i = 0; i < l.groups.size() && i < kHiddenBits; ++i)
        l.groups[i].visible = (bits & (1u << i)) == 0;
}

class HostSink : public ControlsSink {
  public:
    void key(int scancode, bool down) override {
        if (trace())
            fprintf(stderr, "[controls] key %d %s\n", scancode, down ? "down" : "up");
        if (g_hooks.key)
            g_hooks.key(scancode, down);
    }
    void action(const std::string &name) override {
        if (name == "settings" && g_hooks.open_settings)
            g_hooks.open_settings();
        else if (name == "system_keyboard" && g_hooks.system_keyboard)
            g_hooks.system_keyboard();
        else if (name == "edit_layout")
            // The same request the settings page's EDIT row makes; host_pump
            // takes it, so the editor never opens under the finger that asked.
            (void)mods_controls_set(CONTROLS_EDIT_ROW, 0);
    }
    // "next" steps to the following layout name, wrapping and never landing
    // on the Hidden choice; any other target selects that name.
    void switch_layout(const std::string &target) override {
        const std::vector<std::string> names = g_store.names();
        if (names.empty())
            return;
        int index = -1;
        if (target == "next") {
            const int current = mods_controls_value(CONTROLS_LAYOUT_ROW);
            index =
                current >= 0 && current < int(names.size()) ? (current + 1) % int(names.size()) : 0;
        } else {
            for (size_t i = 0; i < names.size(); ++i)
                if (names[i] == target)
                    index = int(i);
        }
        if (index >= 0)
            (void)mods_controls_set(CONTROLS_LAYOUT_ROW, index);
    }
    void group_visibility_changed() override {
        mods_controls_set_hidden_groups(settings_form(g_loaded_form), hidden_bits(g_layout));
    }
    // A light tick on each press, when the haptics row is on.
    void tap() override {
        if (mods_controls_value(CONTROLS_HAPTICS_ROW))
            platform_ui_haptic_tap();
    }
};

HostSink g_sink;

// The guest's rumble (vpad().request_rumble, from XInputSetState) drives the
// controller's motors when one is connected, else the device's own.
RumbleRouter g_rumble(RumbleOutputs{
    [](uint16_t low, uint16_t high, uint32_t ms) { (void)gamepad_rumble(low, high, ms); },
    [](uint16_t low, uint16_t high) { platform_ui_device_rumble(low, high); },
});

// Loads `name` for `form` into g_layout, releasing what the router held
// first -- a rotation swaps the layout under the player's fingers, and
// nothing else would ever lift a held key. "" (the Hidden choice) or a
// failed load leaves no layout.
void reload(const std::string &name, Form form) {
    g_router.set_layout(nullptr, g_sink); // releases against the old layout
    g_loaded = true;
    g_loaded_name = name;
    g_loaded_form = form;
    g_have_layout = false;
    g_layout = Layout();
    g_size = -1;
    if (name.empty())
        return;
    Layout fresh;
    std::string problem;
    bool fell_back = false;
    if (!load_with_tablet_fallback(g_store, name, form, &fresh, &problem, &fell_back)) {
        fprintf(stderr, "[controls] no %s layout \"%s\"%s%s\n", form_name(form), name.c_str(),
                problem.empty() ? "" : ": ", problem.c_str());
        return;
    }
    if (!problem.empty())
        fprintf(stderr, "[controls] skipped %s\n", problem.c_str());
    static bool fallback_logged = false;
    if (fell_back && !fallback_logged) {
        fallback_logged = true;
        fprintf(stderr, "[controls] no %s layout \"%s\"; using the tablet one\n", form_name(form),
                name.c_str());
    }
    g_layout = std::move(fresh);
    g_content = layout_content(g_layout);
    g_file_scale = g_layout.scale;
    g_have_layout = true;
    g_router.set_layout(&g_layout, g_sink);
}

// --- the layout editor ------------------------------------------------------

Editor g_editor;
std::string g_editor_name;     // the name the editor was opened on
bool g_editor_renamed = false; // a rename was started: Done removes the old file
bool g_force_reload = false;   // the editor wrote a file: re-read the layout

// The editor's results, as files and settings (editor_actions.h).
class HostEditorHost : public EditorHost {
  public:
    void save_layout(const Layout &l, Form form) override {
        // The editor works on the layout as it is drawn, size setting and
        // all; the file keeps the layout's own scale, so Large does not bake
        // itself into every save.
        Layout copy = l;
        const double factor = size_factor();
        if (factor > 0)
            copy.scale = l.scale / factor;
        std::string error;
        if (!g_store.save_user_copy(copy, form, &error))
            fprintf(stderr, "[controls] could not save layout \"%s\": %s\n", l.name.c_str(),
                    error.c_str());
    }
    void delete_layout(const std::string &name, Form form) override {
        (void)g_store.delete_user_copy(name, form);
    }
    std::vector<std::string> names() override {
        return g_store.names();
    }
    void set_names(const std::vector<std::string> &names) override {
        mods_controls_set_names(names);
        mods_controls_refresh_names();
    }
    void select_layout(const std::string &name) override {
        const std::vector<std::string> all = g_store.names();
        for (size_t i = 0; i < all.size(); ++i)
            if (all[i] == name)
                (void)mods_controls_set(CONTROLS_LAYOUT_ROW, int(i));
    }
    void set_snap(bool on) override {
        (void)mods_controls_set(CONTROLS_SNAP_ROW, on ? 1 : 0);
    }
};

HostEditorHost g_editor_host;

// Lets go of everything the guest is being told is held: the router's keys
// and pad buttons, and any finger the editor is tracking. Nothing else would
// ever lift them.
void release_everything() {
    if (g_editor.is_open())
        g_editor.cancel_fingers();
    g_router.cancel_all(g_sink);
    // The router's pad reaches the guest only through the shared vpad, and
    // host_pump publishes it after the editing early return -- so without
    // this line the cleared pad would never arrive while the editor is open
    // (pad = "native": a held on-screen button stays down for the whole
    // editing session) or while the app is in the background.
    vpad().set_source(kPadSourceTouch, g_router.pad());
    vpad().set_source(kPadSourceController, PadState{});
}

void close_editor();

// Opens the editor on the layout the player is looking at, and closes the
// F10 page behind it. A layout that would not load (the Hidden choice, or a
// user copy just deleted) leaves nothing to edit, so the editor closes
// instead of drawing a layout nothing backs.
void open_editor() {
    if (!g_have_layout) {
        close_editor();
        return;
    }
    // A finger still on a control when the editor opens never reaches the
    // router's finger_up, so whatever it held would stay down all session.
    release_everything();
    // The pump stops feeding g_rumble below, so a rumble still running would
    // buzz for the whole editing session.
    g_rumble.stop();
    // A reload just before this (a reset, a switch) left the file's own
    // scale; the editor shows what the player sees, size setting included.
    g_size = mods_controls_value(CONTROLS_SIZE_ROW);
    g_size = g_size < 0 ? 0 : g_size > 2 ? 2 : g_size;
    g_layout.scale = g_file_scale * size_factor();
    g_editor.set_screen(g_screen);
    g_editor.set_names(g_store.names());
    g_editor.open(g_layout, g_loaded_form, mods_controls_value(CONTROLS_SNAP_ROW) != 0);
    g_editor_name = g_layout.name;
    g_editor_renamed = false;
    mods_controls_set_editing(true);
    mods_page_close();
}

void close_editor() {
    g_editor.close();
    g_editor_renamed = false;
    mods_controls_set_editing(false);
}

void publish() {
    if (g_editor.is_open()) {
        host_present_set_controls(make_view(g_editor, g_screen));
        // The editor's revision follows its generation, which only ever
        // rises, so the next published view never matches this one.
        g_published = false;
        return;
    }
    const bool wanted = g_have_layout && g_enabled;
    ControlsView view;
    if (wanted)
        view = make_view(g_layout, g_router, g_screen,
                         g_layout.opacity * mods_controls_value(CONTROLS_OPACITY_ROW) / 100.0);
    view.wanted = wanted;
    if (g_published && g_published_wanted == wanted &&
        (!wanted || g_published_revision == view.revision))
        return;
    g_published = true;
    g_published_wanted = wanted;
    g_published_revision = view.revision;
    host_present_set_controls(view);
}

} // namespace

void host_init(const HostHooks &hooks) {
    g_hooks = hooks;
    // The player's copies, then the game's shipped layouts (its resources'
    // controls/), then the kit's built-ins.
    g_store.set_dirs(std::string(mods_overlay_profile_dir()) + "/controls",
                     host_resource("controls"));
    mods_controls_set_names(g_store.names());
    mods_controls_set_platform(platform_ui_touch_device());
    host_mapper_load();
}

void host_mapper_load() {
    set_mapper_status({});
    const PadMapperProfile profile = load_mapper_profile();
    std::string error;
    if (!g_mapper.set_profile(profile, &error)) {
        set_mapper_status("Could not initialize controller profile: " + error);
        fprintf(stderr, "[controls] bad controller profile: %s\n", error.c_str());
    }
}

void host_set_screen(const Screen &s, const Rect &game, int safe_bottom) {
    g_screen = s;
    g_screen.controls_area = controls_area_below(s.dw, s.dh, game, safe_bottom);
    g_router.set_screen(g_screen);
    g_router.set_claim_area(g_screen.controls_area);
}

void host_set_wanted(bool keyboard_absent, bool controller_present) {
    g_keyboard_absent = keyboard_absent;
    g_controller_present = controller_present;
}

void host_set_focused(bool focused) {
    if (g_game_focused == focused)
        return;
    g_game_focused = focused;
    if (!focused) {
        release_everything();
        g_rumble.stop();
        mods_controls_flush();
        vpad().set_source(kPadSourceTouch, PadState{});
        vpad().set_source(kPadSourceController, PadState{});
    } else {
        host_gamepad_state(host_mapper_raw_state());
    }
}

void host_gamepad_state(const PadState &raw) {
    PadState mapped;
    g_mapper.process(raw, nullptr, &mapped);
    {
        std::lock_guard<std::mutex> lock(g_mapper_view_mutex);
        g_mapper_raw = raw;
    }
    if (!g_game_focused || g_editor.is_open()) {
        vpad().set_source(kPadSourceController, PadState{});
        return;
    }
    vpad().set_source(kPadSourceController, mapped);
}

PadMapperProfile host_mapper_profile() {
    return g_mapper.snapshot();
}

PadState host_mapper_raw_state() {
    std::lock_guard<std::mutex> lock(g_mapper_view_mutex);
    return g_mapper_raw;
}

std::string host_mapper_status() {
    std::lock_guard<std::mutex> lock(g_mapper_view_mutex);
    return g_mapper_status;
}

bool host_mapper_apply(const PadMapperProfile &requested, bool persist) {
    const PadMapperProfile profile = requested;
    std::string error;
    if (!validate_pad_mapper_profile(profile, &error)) {
        set_mapper_status("Controller profile was not applied: " + error);
        return false;
    }
    if (persist && !save_mapper_profile(profile, &error)) {
        set_mapper_status("Controller profile was not saved: " + error);
        fprintf(stderr, "[controls] could not save %s: %s\n", mapper_path().c_str(), error.c_str());
        return false;
    }
    if (!g_mapper.set_profile(profile, &error)) {
        set_mapper_status("Controller profile was not applied: " + error);
        fprintf(stderr, "[controls] bad controller profile: %s\n", error.c_str());
        return false;
    }
    set_mapper_status({});
    host_gamepad_state(host_mapper_raw_state());
    return true;
}

// While the editor is open every finger is its own: the router sees none of
// them, so a control never fires while it is being moved.
bool host_finger_down(int64_t id, double px, double py, uint64_t now) {
    if (g_editor.is_open()) {
        g_editor.finger_down(id, px, py);
        return true;
    }
    const bool claimed = g_router.finger_down(id, px, py, now, g_sink);
    if (trace())
        fprintf(
            stderr,
            "[controls] touch %.1f,%.1f %s layout %s hidden %x drawable %dx%d safe %d,%d %dx%d\n",
            px, py, claimed ? "claimed" : "passed", g_loaded_name.c_str(), hidden_bits(g_layout),
            g_screen.dw, g_screen.dh, g_screen.safe.x, g_screen.safe.y, g_screen.safe.w,
            g_screen.safe.h);
    return claimed;
}

bool host_finger_motion(int64_t id, double px, double py, uint64_t now) {
    if (g_editor.is_open()) {
        g_editor.finger_motion(id, px, py);
        return true;
    }
    return g_router.finger_motion(id, px, py, now, g_sink);
}

bool host_finger_up(int64_t id, uint64_t now) {
    if (g_editor.is_open()) {
        g_editor.finger_up(id);
        return true;
    }
    return g_router.finger_up(id, now, g_sink);
}

bool host_finger_cancel(int64_t id) {
    if (g_editor.is_open()) {
        // Cancelled, not lifted: the finger's gesture is dropped, so a
        // system-taken finger never commits a drag or chooses a picker item.
        g_editor.finger_cancel(id);
        return true;
    }
    return g_router.finger_cancel(id, g_sink);
}

bool host_editing() {
    return g_editor.is_open();
}

void host_editor_pointer(double px, double py, int state) {
    if (!g_editor.is_open())
        return;
    if (state > 0)
        g_editor.finger_down(kMouseFinger, px, py);
    else if (state < 0)
        g_editor.finger_up(kMouseFinger);
    else
        g_editor.finger_motion(kMouseFinger, px, py);
}

void host_editor_wheel(double notches) {
    if (g_editor.is_open())
        g_editor.wheel(notches);
}

void host_editor_escape() {
    if (g_editor.is_open())
        g_editor.done();
}

bool host_editor_text_wanted() {
    // The editor's own state, not g_editor_renamed: that one stays set after
    // text_done so Done knows a rename happened, and driving the system
    // keyboard from it would leave it up for the rest of the session.
    return g_editor.is_open() && g_editor.renaming();
}

void host_editor_text(const char *utf8) {
    if (g_editor.is_open() && utf8)
        g_editor.text(utf8);
}

void host_editor_text_done() {
    if (g_editor.is_open())
        g_editor.text_done();
}

void host_release_all() {
    release_everything();
    // Backgrounding or losing focus: the pump may not run again, so a toggle
    // press from the last frame is written now rather than waiting for one.
    mods_controls_flush();
    publish();
}

void host_pump(uint64_t now) {
    dinput_feedback_tick();
    // The layout row means nothing until the settings are loaded (the first
    // presented frame runs mods_page_init); draw nothing before then.
    if (!mods_controls_initialized()) {
        publish();
        return;
    }
    // The settings page's "Edit controls" row, and the edit_layout action.
    if (mods_controls_take_edit_request() && !g_editor.is_open())
        open_editor();
    // The editor's pending results, before anything reloads: a save or a
    // rename writes files and may move the layout row, and close() (below)
    // would clear them.
    EditorNext next = EditorNext::Keep;
    if (g_editor.is_open())
        next = apply_editor_results(g_editor, g_editor_host, g_editor_name, &g_editor_renamed);
    const std::string name = mods_controls_layout_name();
    mods_controls_set_layout_available(g_have_layout && !name.empty());
    const Form form = g_screen.dw > 0 && g_screen.dh > 0
                          ? form_for(g_screen.dw, g_screen.dh, g_screen.scale)
                          : g_loaded_form;
    // The editor wrote or removed a file under the layout's feet, so the
    // active layout is read again even when its name did not change.
    if (next != EditorNext::Keep)
        g_force_reload = true;
    if (g_force_reload || !g_loaded || name != g_loaded_name || form != g_loaded_form) {
        g_force_reload = false;
        reload(mods_controls_layout_name(), form);
    }
    if (next == EditorNext::Close)
        close_editor();
    else if (next == EditorNext::Reopen)
        open_editor();

    // Editing: the router and the rumble stand still, and the presenter gets
    // the editor's view instead of the layout's.
    if (g_editor.is_open()) {
        g_editor.set_screen(g_screen);
        publish();
        return;
    }

    if (g_have_layout) {
        int size = mods_controls_value(CONTROLS_SIZE_ROW);
        size = size < 0 ? 0 : size > 2 ? 2 : size;
        if (size != g_size) {
            g_size = size;
            g_layout.scale = g_file_scale * size_factor();
        }
        // The hidden groups follow the row whoever set it: a toggle writes it
        // (group_visibility_changed), and a settings load or reset may too.
        // The row is per form factor, since one name's forms need not have the
        // same groups; the bits are still taken as they apply to the layout in
        // hand (hidden_bits_for) and the row is left alone, so a form whose
        // file has fewer groups never trims the other's.
        const uint32_t bits =
            hidden_bits_for(g_layout, mods_controls_hidden_groups(settings_form(g_loaded_form)));
        if (bits != hidden_bits(g_layout))
            apply_hidden_bits(g_layout, bits);
    }

    const int mobile = mods_controls_value(CONTROLS_MOBILE_ROW);
    const bool forced = recomp_env("KEYPAD") != nullptr;
    const bool controls_available =
        forced || mobile == 2 || (mobile == 1 && platform_ui_touch_device());
    const bool enabled = controls_available && !name.empty() && g_have_layout;
    if (enabled != g_enabled || g_router.enabled() != enabled) {
        g_enabled = enabled;
        g_router.set_enabled(enabled, g_sink);
    }
    // An auto-hidden layout keeps only layout-switch toggles; group HIDE/KEYS
    // tabs disappear with the controls they would otherwise toggle.
    const bool shown =
        enabled && layout_wanted(g_content, !(g_keyboard_absent || forced), g_controller_present,
                                 mods_controls_value(CONTROLS_PAD_WITH_CONTROLLER_ROW) != 0, false);
    const bool toggles_only = enabled && !shown;
    if (g_router.toggles_only() != toggles_only) {
        g_router.set_toggles_only(toggles_only, g_sink);
        if (trace())
            fprintf(stderr, "[controls] layout \"%s\" %s (keyboard %s, controller %s)\n",
                    name.c_str(), toggles_only ? "hidden, toggles only" : "shown",
                    g_keyboard_absent ? "absent" : "present",
                    g_controller_present ? "present" : "absent");
    }
    vpad().set_source(kPadSourceTouch, g_game_focused ? g_router.pad() : PadState{});

    {
        // Serial first: the values read after it are at least as new.
        const uint64_t serial = vpad().rumble_serial();
        uint16_t low = 0, high = 0;
        vpad().rumble(&low, &high);
        g_rumble.update(serial, low, high,
                        rumble_sink(gamepad_connected(), platform_ui_touch_device()), now);
    }

    if (trace()) {
        static PadState last;
        const PadState pad = vpad().state();
        if (pad != last) {
            last = pad;
            fprintf(
                stderr,
                "[controls] pad buttons %04x hat %x l %.2f,%.2f r %.2f,%.2f triggers %.2f,%.2f\n",
                pad.buttons, pad.hat, pad.lx, pad.ly, pad.rx, pad.ry, pad.l2, pad.r2);
        }
    }

    // One profile write per pump at most, off the input thread's path: a
    // toggle press only marks the hidden-group bits.
    mods_controls_flush();

    publish();
}

} // namespace controls
