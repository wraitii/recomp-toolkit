#pragma once

#include <cstddef>
#include <string>

namespace host_preferences {

enum class Setting {
    KeyboardNames,
    MouseSensitivity,
    MouseAcceleration,
    WindowMode,
    PerformanceOverlay,
    ReportedGpu,
    PresentationFilter,
    TouchKeyboardSize,
    TouchPadSize,
    ConfigureSize
};

enum class KeyboardNames { System = 0, US = 1 };
enum class MouseAcceleration { Off = 0, System = 1 };

struct SettingInfo {
    Setting setting;
    const char *id;
    const char *label;
    const char *description;
    int minimum;
    int maximum;
    int step;
    int recommended;
    const char *choices;
    bool touch_only = false;
};

const SettingInfo *settings(std::size_t *count);
bool load(std::string *error = nullptr);
bool reload(std::string *error = nullptr);
int get(Setting setting);
KeyboardNames keyboard_names();
int mouse_sensitivity();
MouseAcceleration mouse_acceleration();
bool set(Setting setting, int value, std::string *error = nullptr);
bool reset(std::string *error = nullptr);

} // namespace host_preferences
