#include "preferences.h"

#include "game_config.h"
#include "../dx/adapter_profiles.h"
#include "../mods/mods_internal.h"

#include <algorithm>
#include <vector>

namespace host_preferences {
namespace {

constexpr SettingInfo kSettings[] = {
    {Setting::TouchKeyboardSize, "touch_keyboard_size", "Touch keyboard size",
     "Larger keys and labels. Fits the available screen space.", 75, 250, 25, 150, nullptr, true},
    {Setting::TouchPadSize, "touch_pad_size", "Touch gamepad size",
     "Larger buttons and sticks. Fits the available screen space.", 75, 250, 25, 150, nullptr,
     true},
    {Setting::ConfigureSize, "configure_size", "Configure menu size",
     "Adjust text and buttons in Configure.", 75, 250, 25, 150, nullptr, true},
    {Setting::KeyboardNames, "keyboard_names", "Keyboard key names",
     "Name keys using your system keyboard layout or US labels.", 0, 1, 1,
     RECOMP_INPUT_KEYBOARD_NAMES, "System|US"},
    {Setting::MouseSensitivity, "mouse_sensitivity", "Mouse sensitivity",
     "Adjust captured physical mouse movement.", 25, 400, 25, RECOMP_INPUT_MOUSE_SENSITIVITY,
     nullptr},
    {Setting::MouseAcceleration, "mouse_acceleration", "Use system mouse acceleration",
     "Apply operating system speed and acceleration to the captured physical mouse.", 0, 1, 1,
     RECOMP_INPUT_MOUSE_ACCELERATION, "Off|System"},
    {Setting::WindowMode, "window_mode", "Window mode", "Choose the display mode.", 0, 2, 1,
     RECOMP_SETTINGS_WINDOW_MODE, "Windowed|Borderless|Fullscreen"},
    {Setting::PerformanceOverlay, "performance_overlay", "Performance overlay",
     "Choose which performance information appears on screen.", 0, 2, 1,
     RECOMP_SETTINGS_PERFORMANCE_OVERLAY, "Off|Counters|Graph"},
    {Setting::PresentationFilter, "presentation_filter", "Image scaling",
     "Choose sharp pixels or smooth bilinear filtering when resizing the game image.", 0, 1, 1,
     RECOMP_SETTINGS_PRESENTATION_FILTER, "Nearest|Bilinear"},
    {Setting::ReportedGpu, "reported_gpu", "Reported graphics card",
     "Choose which graphics card Direct3D reports to the game.", 0,
     int(std::size(kAdapterProfiles)), 1, 0, RECOMP_ADAPTER_PROFILE_CHOICES},
};

constexpr const char *kInputModId = "host.input";
constexpr const char *kDisplayModId = "host.display";
constexpr uint32_t kOwner = MODS_OWNER_RUNTIME;
bool g_loaded = false;

const char *key(Setting setting) {
    switch (setting) {
    case Setting::WindowMode:
        return "window";
    case Setting::PerformanceOverlay:
        return "performance_overlay";
    default:
        for (const SettingInfo &info : kSettings)
            if (info.setting == setting)
                return info.id;
        return nullptr;
    }
}

const char *mod_id(Setting setting) {
    return setting == Setting::WindowMode || setting == Setting::PerformanceOverlay ||
                   setting == Setting::ReportedGpu || setting == Setting::PresentationFilter
               ? kDisplayModId
               : kInputModId;
}

int fallback(Setting setting) {
    for (const SettingInfo &info : kSettings)
        if (info.setting == setting)
            return info.recommended;
    return 0;
}

void assign_error(std::string *error, const char *value) {
    if (error)
        *error = value;
}

bool ensure_loaded(std::string *error) {
    if (g_loaded)
        return true;
    return load(error);
}

} // namespace

const SettingInfo *settings(std::size_t *count) {
    if (count)
        *count = sizeof(kSettings) / sizeof(kSettings[0]);
    return kSettings;
}

bool load(std::string *error) {
    if (g_loaded)
        return true;
    if (!mods_settings_load(mods_settings_path())) {
        if (error)
            *error = "Could not read settings, or the settings file is invalid: " +
                     std::string(mods_settings_path());
        return false;
    }
    for (const SettingInfo &info : kSettings) {
        mods_settings_declare(kOwner, mod_id(info.setting), key(info.setting), info.label,
                              POP_SETTING_INT, info.recommended, info.minimum, info.maximum);
    }
    g_loaded = true;
    if (error)
        error->clear();
    return true;
}

bool reload(std::string *error) {
    mods_settings_reset();
    g_loaded = false;
    return load(error);
}

int get(Setting setting) {
    if (!ensure_loaded(nullptr))
        return fallback(setting);
    const char *setting_key = key(setting);
    int64_t value = fallback(setting);
    if (!setting_key || mods_settings_get(kOwner, setting_key, &value) != POP_OK)
        return fallback(setting);
    for (const SettingInfo &info : kSettings) {
        if (info.setting == setting) {
            value = std::clamp<int64_t>(value, info.minimum, info.maximum);
            return info.minimum + static_cast<int>((value - info.minimum) / info.step) * info.step;
        }
    }
    return fallback(setting);
}

KeyboardNames keyboard_names() {
    return static_cast<KeyboardNames>(get(Setting::KeyboardNames));
}

int mouse_sensitivity() {
    return get(Setting::MouseSensitivity);
}

MouseAcceleration mouse_acceleration() {
    return static_cast<MouseAcceleration>(get(Setting::MouseAcceleration));
}

bool set(Setting setting, int value, std::string *error) {
    if (!ensure_loaded(error))
        return false;
    const SettingInfo *info = nullptr;
    for (const SettingInfo &candidate : kSettings)
        if (candidate.setting == setting)
            info = &candidate;
    if (!info || value < info->minimum || value > info->maximum ||
        (value - info->minimum) % info->step != 0) {
        assign_error(error, "The selected preference value is out of range.");
        return false;
    }
    const PopModStatus status = mods_settings_set(kOwner, key(setting), value);
    if (status != POP_OK) {
        assign_error(error, "Could not save host preferences.");
        return false;
    }
    if (error)
        error->clear();
    return true;
}

bool reset(std::string *error) {
    if (!ensure_loaded(error))
        return false;
    std::vector<std::pair<Setting, int>> previous;
    previous.reserve(sizeof(kSettings) / sizeof(kSettings[0]));
    for (const SettingInfo &info : kSettings)
        previous.emplace_back(info.setting, get(info.setting));
    mods_settings_txn_begin();
    for (const SettingInfo &info : kSettings) {
        if (mods_settings_set(kOwner, key(info.setting), info.recommended) != POP_OK) {
            mods_settings_txn_rollback();
            assign_error(error, "Could not reset host preferences.");
            return false;
        }
    }
    mods_settings_txn_commit();
    if (!mods_settings_save()) {
        mods_settings_txn_begin();
        for (const auto &entry : previous)
            mods_settings_set(kOwner, key(entry.first), entry.second);
        mods_settings_txn_commit();
        assign_error(error, "Could not save host preferences.");
        return false;
    }
    if (error)
        error->clear();
    return true;
}

} // namespace host_preferences
