// gamepad_sdl.cpp - see gamepad_sdl.h.
#include "gamepad_sdl.h"

#include "controls_host.h"
#include "vpad.h"
#include "../../dx/host_api.h"
#include "../../runtime/win32.h"
#include "../sdl/platform_ui.h"

#include <algorithm>
#include <condition_variable>
#include <deque>
#include <memory>
#include <map>
#include <mutex>
#include <unordered_set>

#include <stdio.h>

namespace controls {

namespace {

SDL_Gamepad *g_pad = nullptr;
SDL_Haptic *g_haptic = nullptr;
uint32_t g_haptic_generation = 1;
std::unordered_set<int> g_haptic_effects;
struct HapticPlayback {
    HostHapticEffect effect{};
    uint32_t iterations = 0;
    uint64_t started = 0, deadline = 0;
};
std::map<int, HapticPlayback> g_haptic_playing;
uint64_t g_haptic_paused = 0;
struct HapticCall {
    HostHapticRequest request;
    int result = 0;
    bool done = false;
    std::condition_variable completed;
};
std::mutex g_haptic_mutex;
std::deque<std::shared_ptr<HapticCall>> g_haptic_queue;
bool g_haptic_accepting = true;
bool g_haptic_focused = true;

SDL_JoystickID g_id = 0;
bool g_buttons[kSdlPadButtonCount] = {};
int16_t g_axes[kSdlAxisCount] = {};

void publish() {
    host_gamepad_state(pad_from_sdl_raw(g_buttons, g_axes));
}

// Reads every mapped button and axis from SDL's current state.
void read_all() {
    for (int i = 0; i < kSdlPadButtonCount; ++i)
        g_buttons[i] = SDL_GetGamepadButton(g_pad, SDL_GamepadButton(i));
    for (int i = 0; i < kSdlAxisCount; ++i)
        g_axes[i] = SDL_GetGamepadAxis(g_pad, SDL_GamepadAxis(i));
}

int haptic_execute(HostHapticRequest &r);

void open_pad(SDL_JoystickID id) {
    if (!g_haptic_accepting)
        return;
    SDL_Gamepad *pad = SDL_OpenGamepad(id);
    if (!pad) {
        fprintf(stderr, "[controls] could not open gamepad %u: %s\n", unsigned(id), SDL_GetError());
        return;
    }
    g_pad = pad;
    if (!(SDL_WasInit(SDL_INIT_HAPTIC) & SDL_INIT_HAPTIC) && !SDL_InitSubSystem(SDL_INIT_HAPTIC))
        fprintf(stderr, "[controls] haptic initialization failed: %s\n", SDL_GetError());
    SDL_Joystick *joystick = SDL_GetGamepadJoystick(pad);
    if (joystick && SDL_IsJoystickHaptic(joystick)) {
        g_haptic = SDL_OpenHapticFromJoystick(joystick);
        if (!g_haptic)
            fprintf(stderr, "[controls] haptic open failed: %s\n", SDL_GetError());
    }
    g_id = id;
    const char *name = SDL_GetGamepadName(pad);
    fprintf(stderr, "[controls] gamepad connected: %s\n", name ? name : "(unnamed)");
    read_all();
    publish();
    vpad().set_controller_connected(true);
    HostHapticRequest caps{};
    caps.operation = HOST_HAPTIC_CAPS;
    haptic_execute(caps);
    const bool rumble = gamepad_rumble_supported();
    const char *fallback = !vpad().dinput_rumble_enabled() ? "disabled in controller setup"
                           : rumble                        ? "available (constant/periodic only)"
                                                           : "unavailable";
    if (g_haptic) {
        fprintf(stderr,
                "[controls] force feedback: SDL haptic open, raw features 0x%08x, native effects "
                "0x%03x, mapped axes 0x%x, gamepad rumble fallback %s\n",
                SDL_GetHapticFeatures(g_haptic), caps.features, (caps.flags >> 8) & 0xf, fallback);
    } else if (!SDL_GetHintBoolean("SDL2_COMPAT", false) && SDL_IsGamepad(id)) {
        fprintf(stderr,
                "[controls] force feedback: SDL3 cannot associate haptics with recognized "
                "gamepads; native capabilities unknown; gamepad rumble fallback %s\n",
                fallback);
    } else {
        fprintf(stderr,
                "[controls] force feedback: no associated SDL haptic device; native capabilities "
                "unknown; gamepad rumble fallback %s\n",
                fallback);
    }
}

// Opens the first attached pad, if any, when none is open. `skip` is an
// instance SDL may still list although it is going away (the one whose
// SDL_EVENT_GAMEPAD_REMOVED is being handled); 0 skips nothing.
void open_any(SDL_JoystickID skip) {
    if (g_pad)
        return;
    int count = 0;
    SDL_JoystickID *ids = SDL_GetGamepads(&count);
    if (!ids)
        return;
    for (int i = 0; i < count && !g_pad; ++i)
        if (ids[i] != skip)
            open_pad(ids[i]);
    SDL_free(ids);
}

void close_pad() {
    // Zero first, so the pad consumer or a buffered reader sees every release
    // before the pad is reported gone.
    for (bool &b : g_buttons)
        b = false;
    for (int16_t &a : g_axes)
        a = 0;
    publish();
    vpad().set_controller_connected(false);
    if (g_haptic) {
        SDL_StopHapticEffects(g_haptic);
        SDL_CloseHaptic(g_haptic);
        g_haptic = nullptr;
        g_haptic_effects.clear();
        g_haptic_playing.clear();
        g_haptic_paused = 0;
        ++g_haptic_generation;
    }
    SDL_CloseGamepad(g_pad);
    g_pad = nullptr;
    g_id = 0;
    fprintf(stderr, "[controls] gamepad disconnected\n");
}

const uint32_t kHapticTypes[] = {
    SDL_HAPTIC_CONSTANT, SDL_HAPTIC_RAMP,       SDL_HAPTIC_SQUARE,       SDL_HAPTIC_SINE,
    SDL_HAPTIC_TRIANGLE, SDL_HAPTIC_SAWTOOTHUP, SDL_HAPTIC_SAWTOOTHDOWN, SDL_HAPTIC_SPRING,
    SDL_HAPTIC_DAMPER,   SDL_HAPTIC_INERTIA,    SDL_HAPTIC_FRICTION,
};

uint32_t haptic_features() {
    if (!g_haptic)
        return 0;
    uint32_t features = SDL_GetHapticFeatures(g_haptic), result = 0;
    for (unsigned i = 0; i < std::size(kHapticTypes); ++i)
        if (features & kHapticTypes[i])
            result |= 1u << i;
    return result;
}

int16_t signed_force(int32_t value, uint32_t gain = 10000) {
    int64_t n = std::clamp<int64_t>(value, -10000, 10000) * std::min(gain, 10000u);
    return int16_t(n * 32767 / 100000000);
}
uint16_t unsigned_force(uint32_t value, uint32_t gain = 10000) {
    return uint16_t(uint64_t(std::min(value, 10000u)) * std::min(gain, 10000u) * 65535 / 100000000);
}
bool haptic_time(uint32_t us, uint16_t *out) {
    // DIVERGENCE(original): SDL times have millisecond resolution and round up.
    uint64_t ms = (uint64_t(us) + 999) / 1000;
    if (ms > 65535)
        return false;
    *out = uint16_t(ms);
    return true;
}
template <typename T> bool haptic_common(T &out, const HostHapticEffect &in) {
    out.direction.type = in.coordinates == 1   ? SDL_HAPTIC_CARTESIAN
                         : in.coordinates == 2 ? SDL_HAPTIC_POLAR
                                               : SDL_HAPTIC_SPHERICAL;
    for (int i = 0; i < 3; ++i)
        out.direction.dir[i] = in.direction[i];
    out.length = in.duration == 0xffffffffu ? SDL_HAPTIC_INFINITY
                                            : uint32_t((uint64_t(in.duration) + 999) / 1000);
    if (!haptic_time(in.start_delay, &out.delay) ||
        !haptic_time(in.trigger_interval, &out.interval))
        return false;
    out.button = uint16_t(in.trigger_button);
    return true;
}
template <typename T> bool haptic_envelope(T &out, const HostHapticEffect &in) {
    if (!in.envelope)
        return true;
    out.attack_level = unsigned_force(in.attack_level, in.gain) / 2;
    out.fade_level = unsigned_force(in.fade_level, in.gain) / 2;
    return haptic_time(in.attack_time, &out.attack_length) &&
           haptic_time(in.fade_time, &out.fade_length);
}
int haptic_axis(uint32_t guest_axis) {
    if (!g_pad || guest_axis >= 4)
        return -1;
    PadMapperProfile profile = host_mapper_profile();
    // DIVERGENCE(original): SDL conditions read raw axes before the mapper's stick deadzone.
    int mapped = -1;
    for (int i = 0; i < 4; ++i) {
        const AxisBinding &binding = profile.axis_bindings[size_t(i)];
        const AxisCalibration &cal = profile.axes[size_t(i)];
        if (int(binding.guest_axis) != int(guest_axis) + 1)
            continue;
        if (mapped >= 0 || binding.scale != 1 || binding.offset != 0 || cal.inverted ||
            cal.mode != MapperAxisMode::Centered || cal.minimum != -1 || cal.center != 0 ||
            cal.maximum != 1 || cal.deadzone != 0 || cal.saturation != 1 || cal.curve != 1)
            return -1;
        mapped = i;
    }
    if (mapped < 0)
        return -1;
    int count = 0, axis = -1;
    SDL_GamepadBinding **bindings = SDL_GetGamepadBindings(g_pad, &count);
    if (!bindings)
        return -1;
    for (int i = 0; i < count; ++i) {
        const SDL_GamepadBinding &b = *bindings[i];
        if (b.output_type != SDL_GAMEPAD_BINDTYPE_AXIS || int(b.output.axis.axis) != mapped)
            continue;
        if (axis >= 0 || b.input_type != SDL_GAMEPAD_BINDTYPE_AXIS || b.input.axis.axis < 0 ||
            b.input.axis.axis >= 3 || b.input.axis.axis_min >= 0 || b.input.axis.axis_max <= 0 ||
            b.output.axis.axis_min >= 0 || b.output.axis.axis_max <= 0) {
            axis = -1;
            break;
        }
        axis = b.input.axis.axis;
    }
    SDL_free(bindings);
    return axis;
}
int haptic_button(uint32_t guest_button) {
    static const int sdl_buttons[] = {0, 1, 2, 3, 9, 10, -1, -1, 7, 8, 4, 6, 5};
    if (!guest_button || guest_button > 13)
        return -1;
    uint16_t bit = uint16_t(1u << (guest_button - 1));
    PadMapperProfile profile = host_mapper_profile();
    for (const auto &axis : profile.axis_bindings)
        if ((axis.negative_guest_buttons | axis.positive_guest_buttons) & bit)
            return -1;
    for (const auto &hat : profile.hat)
        if (hat.guest_buttons & bit)
            return -1;
    int physical = -1;
    for (int i = 0; i < 13; ++i) {
        if (!(profile.buttons[size_t(i)].guest_buttons & bit))
            continue;
        if (physical >= 0 || sdl_buttons[i] < 0)
            return -1;
        physical = sdl_buttons[i];
    }
    if (physical < 0)
        return -1;
    int count = 0, button = -1;
    SDL_GamepadBinding **bindings = SDL_GetGamepadBindings(g_pad, &count);
    if (!bindings)
        return -1;
    for (int i = 0; i < count; ++i) {
        const SDL_GamepadBinding &b = *bindings[i];
        if (b.output_type != SDL_GAMEPAD_BINDTYPE_BUTTON || int(b.output.button) != physical)
            continue;
        if (button >= 0 || b.input_type != SDL_GAMEPAD_BINDTYPE_BUTTON || b.input.button < 0 ||
            b.input.button >= 65535) {
            button = -1;
            break;
        }
        button = b.input.button + 1;
    }
    SDL_free(bindings);
    return button;
}
bool haptic_convert(const HostHapticEffect &in, SDL_HapticEffect &out) {
    if (in.type >= std::size(kHapticTypes) || in.sample_period || !in.axes || in.axes > 3 ||
        in.trigger_button > 65535 || in.coordinates < 1 || in.coordinates > 3 || in.gain > 10000 ||
        in.magnitude < -10000 || in.magnitude > 10000 || in.offset < -10000 || in.offset > 10000 ||
        in.ramp_start < -10000 || in.ramp_start > 10000 || in.ramp_end < -10000 ||
        in.ramp_end > 10000)
        return false;
    HostHapticEffect mapped = in;
    if (in.trigger_button) {
        int button = haptic_button(in.trigger_button);
        if (button < 0)
            return false;
        mapped.trigger_button = uint32_t(button);
    }
    for (uint32_t i = 0; i < in.axes; ++i) {
        int axis = haptic_axis(in.axis[i]);
        if (axis < 0 || axis >= SDL_GetNumHapticAxes(g_haptic) ||
            (in.coordinates != 1 && axis != int(i)))
            return false;
        for (uint32_t j = 0; j < i; ++j)
            if (mapped.axis[j] == uint32_t(axis))
                return false;
        mapped.axis[i] = uint32_t(axis);
    }
    if (in.coordinates == 1) {
        std::fill(std::begin(mapped.direction), std::end(mapped.direction), 0);
        for (uint32_t i = 0; i < in.axes; ++i)
            mapped.direction[mapped.axis[i]] = in.direction[i];
    }
    out = {};
    out.type = kHapticTypes[in.type];
    if (in.type == 0) {
        out.constant.type = out.type;
        out.constant.level = signed_force(in.magnitude, in.gain);
        return haptic_common(out.constant, mapped) && haptic_envelope(out.constant, in);
    }
    if (in.type == 1) {
        if (in.duration == 0xffffffffu)
            return false;
        out.ramp.type = out.type;
        out.ramp.start = signed_force(in.ramp_start, in.gain);
        out.ramp.end = signed_force(in.ramp_end, in.gain);
        return haptic_common(out.ramp, mapped) && haptic_envelope(out.ramp, in);
    }
    if (in.type <= 6) {
        out.periodic.type = out.type;
        out.periodic.magnitude = signed_force(in.magnitude, in.gain);
        out.periodic.offset = signed_force(in.offset, in.gain);
        out.periodic.phase = uint16_t(in.phase % 36000);
        return haptic_common(out.periodic, mapped) && haptic_envelope(out.periodic, in) &&
               haptic_time(in.period, &out.periodic.period) && out.periodic.period;
    }
    out.condition.type = out.type;
    if (in.envelope || !haptic_common(out.condition, mapped))
        return false;
    for (uint32_t i = 0; i < in.axes; ++i) {
        if (mapped.axis[i] >= 3)
            return false;
        unsigned a = mapped.axis[i];
        const HostHapticCondition &c = in.condition[i];
        if (c.offset < -10000 || c.offset > 10000 || c.positive_coefficient < -10000 ||
            c.positive_coefficient > 10000 || c.negative_coefficient < -10000 ||
            c.negative_coefficient > 10000 || c.positive_saturation > 10000 ||
            c.negative_saturation > 10000 || c.deadband > 10000)
            return false;
        out.condition.center[a] = signed_force(c.offset);
        out.condition.right_coeff[a] = signed_force(c.positive_coefficient, in.gain);
        out.condition.left_coeff[a] = signed_force(c.negative_coefficient, in.gain);
        out.condition.right_sat[a] = unsigned_force(c.positive_saturation, in.gain);
        out.condition.left_sat[a] = unsigned_force(c.negative_saturation, in.gain);
        out.condition.deadband[a] = unsigned_force(c.deadband);
    }
    return true;
}
uint64_t haptic_deadline(const HapticPlayback &playing) {
    const HostHapticEffect &effect = playing.effect;
    uint64_t duration = (uint64_t(effect.duration) + 999) / 1000;
    uint64_t delay = (uint64_t(effect.start_delay) + 999) / 1000;
    if (effect.trigger_button || effect.duration == 0xffffffffu ||
        playing.iterations == 0xffffffffu || !playing.iterations ||
        delay > UINT64_MAX - playing.started ||
        duration > (UINT64_MAX - playing.started - delay) / playing.iterations)
        return UINT64_MAX;
    return playing.started + delay + duration * playing.iterations;
}
bool haptic_playing(int id) {
    auto it = g_haptic_playing.find(id);
    if (it == g_haptic_playing.end())
        return false;
    const HapticPlayback &playing = it->second;
    if (g_haptic_paused)
        return true;
    uint64_t now = SDL_GetTicks();
    bool active = now < playing.deadline;
    uint64_t delay = (uint64_t(playing.effect.start_delay) + 999) / 1000;
    if (!playing.effect.trigger_button && now - playing.started >= delay &&
        (SDL_GetHapticFeatures(g_haptic) & SDL_HAPTIC_STATUS)) {
        SDL_ClearError();
        bool status = SDL_GetHapticEffectStatus(g_haptic, id);
        if (!SDL_GetError()[0])
            active = status;
    }
    if (!active) {
        g_haptic_playing.erase(it);
        if (g_haptic_playing.empty())
            vpad().refresh_rumble();
    }
    return active;
}
int haptic_execute(HostHapticRequest &r) {
    if (r.operation == HOST_HAPTIC_CAPS) {
        r.features = haptic_features();
        r.flags = vpad().dinput_rumble_enabled() &&
                          (gamepad_rumble_supported() || platform_ui_touch_device())
                      ? 1u
                      : 0u;
        if (r.features) {
            int axes = SDL_GetNumHapticAxes(g_haptic);
            for (unsigned i = 0; i < 4; ++i) {
                int axis = haptic_axis(i);
                if (axis >= 0 && axis < axes)
                    r.flags |= 1u << (8 + i);
            }
            if (!(r.flags & 0xf00))
                r.features = 0;
        }
        r.result_generation = g_haptic_generation;
        return 1;
    }
    if (!g_haptic || (r.generation && r.generation != g_haptic_generation))
        return 0;
    if (((r.operation >= HOST_HAPTIC_UPDATE && r.operation <= HOST_HAPTIC_DESTROY) ||
         r.operation == HOST_HAPTIC_STATUS || r.operation == HOST_HAPTIC_TRACKED_STATUS) &&
        !g_haptic_effects.count(int(r.id)))
        return 0;
    switch (r.operation) {
    case HOST_HAPTIC_CREATE:
    case HOST_HAPTIC_UPDATE: {
        SDL_HapticEffect effect;
        if (!haptic_convert(r.effect, effect)) {
            static uint32_t warned = 0;
            uint32_t bit = r.effect.type < 31 ? 1u << r.effect.type : 0x80000000u;
            if (!(warned & bit)) {
                warned |= bit;
                fprintf(stderr, "[controls] unsupported SDL haptic parameters for effect type %u\n",
                        r.effect.type);
            }
            return -1;
        }
        if (!SDL_HapticEffectSupported(g_haptic, &effect)) {
            static uint32_t warned = 0;
            uint32_t bit = 1u << r.effect.type;
            if (!(warned & bit)) {
                warned |= bit;
                fprintf(stderr, "[controls] SDL haptic device rejected effect type %u: %s\n",
                        r.effect.type, SDL_GetError());
            }
            return -1;
        }
        if (r.operation == HOST_HAPTIC_UPDATE) {
            if (!SDL_UpdateHapticEffect(g_haptic, int(r.id), &effect))
                return -2;
            auto it = g_haptic_playing.find(int(r.id));
            if (it != g_haptic_playing.end()) {
                it->second.effect = r.effect;
                // DIVERGENCE(original): SDL may restart an updated effect; bound its remaining time from the update.
                it->second.started = g_haptic_paused ? g_haptic_paused : SDL_GetTicks();
                it->second.deadline = haptic_deadline(it->second);
            }
            return 1;
        }
        int id = SDL_CreateHapticEffect(g_haptic, &effect);
        if (id < 0) {
            fprintf(stderr, "[controls] SDL_CreateHapticEffect failed: %s\n", SDL_GetError());
            return -2;
        }
        g_haptic_effects.insert(id);
        r.result_id = uint32_t(id);
        r.result_generation = g_haptic_generation;
        return 1;
    }
    case HOST_HAPTIC_START: {
        if (!g_haptic_focused || !r.value)
            return 0;
        // DIVERGENCE(original): native effects temporarily take priority over XInput motors.
        if (g_haptic_playing.empty() && g_pad)
            SDL_RumbleGamepad(g_pad, 0, 0, 0);
        if (!SDL_RunHapticEffect(g_haptic, int(r.id), r.value)) {
            if (g_haptic_playing.empty())
                vpad().refresh_rumble();
            return 0;
        }
        HapticPlayback playing{};
        playing.effect = r.effect;
        playing.iterations = r.value;
        playing.started = g_haptic_paused ? g_haptic_paused : SDL_GetTicks();
        playing.deadline = haptic_deadline(playing);
        g_haptic_playing[int(r.id)] = playing;
        return 1;
    }
    case HOST_HAPTIC_STOP:
        if (!SDL_StopHapticEffect(g_haptic, int(r.id)))
            return 0;
        g_haptic_playing.erase(int(r.id));
        if (g_haptic_playing.empty())
            vpad().refresh_rumble();
        return 1;
    case HOST_HAPTIC_DESTROY:
        SDL_DestroyHapticEffect(g_haptic, int(r.id));
        g_haptic_effects.erase(int(r.id));
        g_haptic_playing.erase(int(r.id));
        if (g_haptic_playing.empty())
            vpad().refresh_rumble();
        return 1;
    case HOST_HAPTIC_STOP_ALL:
        if (!SDL_StopHapticEffects(g_haptic))
            return 0;
        g_haptic_playing.clear();
        vpad().refresh_rumble();
        return 1;
    case HOST_HAPTIC_PAUSE:
        if (g_haptic_paused)
            return 1;
        if (!SDL_PauseHaptic(g_haptic))
            return 0;
        g_haptic_paused = SDL_GetTicks();
        return 1;
    case HOST_HAPTIC_RESUME:
        if (!SDL_ResumeHaptic(g_haptic))
            return 0;
        if (g_haptic_paused) {
            uint64_t elapsed = SDL_GetTicks() - g_haptic_paused;
            for (auto &entry : g_haptic_playing) {
                entry.second.started += elapsed;
                entry.second.deadline = haptic_deadline(entry.second);
            }
            g_haptic_paused = 0;
        }
        return 1;
    case HOST_HAPTIC_GAIN:
        return SDL_SetHapticGain(g_haptic, int(std::min(r.value, 10000u) / 100));
    case HOST_HAPTIC_AUTOCENTER:
        return SDL_SetHapticAutocenter(g_haptic, int(std::min(r.value, 10000u) / 100));
    case HOST_HAPTIC_TRACKED_STATUS:
        return haptic_playing(int(r.id)) ? 2 : 1;
    case HOST_HAPTIC_STATUS: {
        if (!(SDL_GetHapticFeatures(g_haptic) & SDL_HAPTIC_STATUS))
            return -1;
        bool playing = SDL_GetHapticEffectStatus(g_haptic, int(r.id));
        return playing ? 2 : SDL_GetError()[0] ? -2 : 1;
    }
    default:
        return 0;
    }
}

int haptic_execute_checked(HostHapticRequest &request) {
    SDL_ClearError();
    int result = haptic_execute(request);
    if (!result && SDL_GetError()[0])
        result = -2;
    if (result <= 0 && request.operation <= HOST_HAPTIC_TRACKED_STATUS) {
        static bool warned[HOST_HAPTIC_TRACKED_STATUS + 1]{};
        if (!warned[request.operation]) {
            warned[request.operation] = true;
            const char *error = SDL_GetError();
            static const char *const names[] = {"capabilities",  "create",  "update",     "start",
                                                "stop",          "destroy", "stop all",   "pause",
                                                "resume",        "gain",    "autocenter", "status",
                                                "tracked status"};
            fprintf(stderr, "[controls] SDL haptic %s failed%s%s\n", names[request.operation],
                    error && *error ? ": " : "", error && *error ? error : "");
        }
    }
    return result;
}

} // namespace

void gamepad_handle_event(const SDL_Event &e) {
    switch (e.type) {
    case SDL_EVENT_GAMEPAD_ADDED:
        if (!g_pad)
            open_pad(e.gdevice.which);
        break;
    case SDL_EVENT_GAMEPAD_REMOVED:
        if (g_pad && e.gdevice.which == g_id) {
            const SDL_JoystickID gone = g_id;
            close_pad();
            open_any(gone);
        }
        break;
    case SDL_EVENT_GAMEPAD_BUTTON_DOWN:
    case SDL_EVENT_GAMEPAD_BUTTON_UP:
        if (g_pad && e.gbutton.which == g_id && e.gbutton.button < kSdlPadButtonCount) {
            g_buttons[e.gbutton.button] = e.gbutton.down;
            publish();
        }
        break;
    case SDL_EVENT_GAMEPAD_AXIS_MOTION:
        if (g_pad && e.gaxis.which == g_id && e.gaxis.axis < kSdlAxisCount) {
            g_axes[e.gaxis.axis] = e.gaxis.value;
            publish();
        }
        break;
    default:
        break;
    }
}

void gamepad_haptic_service() {
    for (;;) {
        std::shared_ptr<HapticCall> call;
        {
            std::lock_guard<std::mutex> lock(g_haptic_mutex);
            if (g_haptic_queue.empty())
                return;
            call = g_haptic_queue.front();
            g_haptic_queue.pop_front();
        }
        int result = haptic_execute_checked(call->request);
        {
            std::lock_guard<std::mutex> lock(g_haptic_mutex);
            call->result = result;
            call->done = true;
            call->completed.notify_one();
        }
    }
}
void gamepad_haptic_focus(bool focused) {
    g_haptic_focused = focused;
    if (!focused && g_haptic) {
        SDL_StopHapticEffects(g_haptic);
        g_haptic_playing.clear();
    }
}
void gamepad_haptic_shutdown() {
    std::lock_guard<std::mutex> lock(g_haptic_mutex);
    g_haptic_accepting = false;
    for (const auto &call : g_haptic_queue) {
        call->done = true;
        call->completed.notify_one();
    }
    g_haptic_queue.clear();
    if (g_pad)
        close_pad();
}
int haptic_request(HostHapticRequest *request) {
    if (!request)
        return 0;
    if (SDL_IsMainThread()) {
        {
            std::lock_guard<std::mutex> lock(g_haptic_mutex);
            if (!g_haptic_accepting)
                return -3;
        }
        return haptic_execute_checked(*request);
    }
    auto call = std::make_shared<HapticCall>();
    call->request = *request;
    {
        std::lock_guard<std::mutex> lock(g_haptic_mutex);
        if (!g_haptic_accepting)
            return -3;
        g_haptic_queue.push_back(call);
    }
    SDL_Event event{};
    event.type = SDL_EVENT_USER;
    SDL_PushEvent(&event);
    bool modal = sched_host_modal_begin();
    {
        std::unique_lock<std::mutex> lock(g_haptic_mutex);
        call->completed.wait(lock, [&] { return call->done; });
    }
    sched_host_modal_end(modal);
    *request = call->request;
    return call->result;
}

void gamepad_poll() {
    if (!g_haptic_paused) {
        for (auto it = g_haptic_playing.begin(); it != g_haptic_playing.end();) {
            int id = it->first;
            ++it;
            haptic_playing(id);
        }
    }
    // A pad whose SDL_EVENT_GAMEPAD_ADDED went somewhere else (the launcher
    // drains its own events) never reaches gamepad_handle_event, so while
    // nothing is open, look for one every kScanIntervalMs.
    if (!g_pad) {
        constexpr uint64_t kScanIntervalMs = 2000;
        static uint64_t next_scan_ms = 0;
        const uint64_t now_ms = SDL_GetTicks();
        if (now_ms >= next_scan_ms) {
            next_scan_ms = now_ms + kScanIntervalMs;
            open_any(0);
        }
    }
    if (!g_pad)
        return;
    read_all();
    publish();
}

std::string gamepad_name() {
    const char *name = g_pad ? SDL_GetGamepadName(g_pad) : nullptr;
    return name ? name : "Controller";
}

bool gamepad_connected() {
    return g_pad != nullptr;
}

bool gamepad_rumble_supported() {
    return g_pad && SDL_GetBooleanProperty(SDL_GetGamepadProperties(g_pad),
                                           SDL_PROP_GAMEPAD_CAP_RUMBLE_BOOLEAN, false);
}

bool gamepad_rumble(uint16_t low, uint16_t high, uint32_t ms) {
    return g_pad && (!g_haptic_playing.empty() || SDL_RumbleGamepad(g_pad, low, high, ms));
}

} // namespace controls

extern "C" int host_haptic_request(HostHapticRequest *request) {
    return controls::haptic_request(request);
}
