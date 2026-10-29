#pragma once

#include "vpad.h"

#include <array>
#include <cstdint>
#include <mutex>
#include <string>

namespace controls {

enum class MapperAxis : uint8_t { None, LX, LY, RX, RY, L2, R2 };
enum class MapperAxisMode : uint8_t { Centered, Pedal };

struct AxisCalibration {
    float minimum = -1;
    float center = 0;
    float maximum = 1;
    float deadzone = 0;
    float saturation = 1;
    float curve = 1;
    bool inverted = false;
    MapperAxisMode mode = MapperAxisMode::Centered;
};

struct AxisBinding {
    MapperAxis guest_axis = MapperAxis::None;
    float scale = 1;
    float offset = 0;
    uint16_t negative_guest_buttons = 0;
    uint16_t positive_guest_buttons = 0;
};

struct ButtonBinding {
    uint16_t guest_buttons = 0;
    uint8_t guest_hat = 0;
    int guest_key = 0;
};

struct PadMapperProfile {
    uint32_t version = 1;
    bool directinput_rumble = true;
    float stick_deadzone = 0.08f;
    std::array<AxisCalibration, kSdlAxisCount> axes{};
    std::array<AxisBinding, kSdlAxisCount> axis_bindings{};
    std::array<ButtonBinding, 13> buttons{};
    std::array<ButtonBinding, 4> hat{}; // up, right, down, left
};

PadMapperProfile pad_mapper_default_profile();
PadMapperProfile pad_mapper_centered_sticks();
PadMapperProfile pad_mapper_separate_pedals();
PadMapperProfile pad_mapper_combined_pedals();
PadMapperProfile pad_mapper_native_profile();

bool validate_pad_mapper_profile(const PadMapperProfile &profile, std::string *error = nullptr);
std::string serialize_pad_mapper_profile(const PadMapperProfile &profile,
                                         std::string *error = nullptr);
bool parse_pad_mapper_profile(const std::string &text, PadMapperProfile *profile,
                              std::string *error = nullptr);

class PadMapper {
  public:
    PadMapper();

    bool set_profile(const PadMapperProfile &profile, std::string *error = nullptr);
    PadMapperProfile snapshot() const;

    // `input` is canonical SDL-free raw physical state from the host.
    void process(const PadState &input, PadState *calibrated, PadState *guest);

  private:
    mutable std::mutex mutex_;
    PadMapperProfile profile_;
};

} // namespace controls
