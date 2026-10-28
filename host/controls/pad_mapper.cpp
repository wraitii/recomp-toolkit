#include "pad_mapper.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <set>

namespace controls {

namespace {

const uint16_t kButtonBits[13] = {kPadCross,  kPadCircle, kPadSquare, kPadTriangle, kPadL1,
                                  kPadR1,     kPadL2,     kPadR2,     kPadL3,       kPadR3,
                                  kPadSelect, kPadStart,  kPadPs};
const uint8_t kHatBits[4] = {kHatUp, kHatRight, kHatDown, kHatLeft};
const MapperAxis kAxes[kSdlAxisCount] = {MapperAxis::LX, MapperAxis::LY, MapperAxis::RX,
                                         MapperAxis::RY, MapperAxis::L2, MapperAxis::R2};
float axis_value(const PadState &p, int i) {
    switch (i) {
    case 0:
        return p.lx;
    case 1:
        return p.ly;
    case 2:
        return p.rx;
    case 3:
        return p.ry;
    case 4:
        return p.l2;
    default:
        return p.r2;
    }
}

void set_axis_value(PadState *p, int i, float v) {
    switch (i) {
    case 0:
        p->lx = v;
        break;
    case 1:
        p->ly = v;
        break;
    case 2:
        p->rx = v;
        break;
    case 3:
        p->ry = v;
        break;
    case 4:
        p->l2 = v;
        break;
    default:
        p->r2 = v;
        break;
    }
}

float calibrated_axis(float value, const AxisCalibration &c) {
    float v;
    if (c.mode == MapperAxisMode::Pedal) {
        v = (value - c.minimum) / (c.maximum - c.minimum);
        v = std::max(0.0f, std::min(1.0f, v));
        if (c.inverted)
            v = 1.0f - v;
    } else {
        const float den = value < c.center ? c.center - c.minimum : c.maximum - c.center;
        v = den > 0 ? (value - c.center) / den : 0;
        v = std::max(-1.0f, std::min(1.0f, v));
        if (c.inverted)
            v = -v;
    }
    const float mag = std::fabs(v);
    if (mag <= c.deadzone)
        return 0;
    const float scaled =
        mag >= c.saturation ? 1.0f : (mag - c.deadzone) / (c.saturation - c.deadzone);
    const float curved = std::pow(std::max(0.0f, std::min(1.0f, scaled)), c.curve);
    const float out = v < 0 ? -curved : curved;
    return c.mode == MapperAxisMode::Pedal ? std::max(0.0f, out) : out;
}

int axis_index(MapperAxis axis) {
    switch (axis) {
    case MapperAxis::LX:
        return 0;
    case MapperAxis::LY:
        return 1;
    case MapperAxis::RX:
        return 2;
    case MapperAxis::RY:
        return 3;
    case MapperAxis::L2:
        return 4;
    case MapperAxis::R2:
        return 5;
    case MapperAxis::None:
        return -1;
    }
    return -1;
}

void route_axis(float values[kSdlAxisCount], MapperAxis target, float value) {
    const int i = axis_index(target);
    if (i < 0)
        return;
    values[i] += value;
}

const char *axis_name(MapperAxis axis) {
    switch (axis) {
    case MapperAxis::None:
        return "none";
    case MapperAxis::LX:
        return "lx";
    case MapperAxis::LY:
        return "ly";
    case MapperAxis::RX:
        return "rx";
    case MapperAxis::RY:
        return "ry";
    case MapperAxis::L2:
        return "l2";
    case MapperAxis::R2:
        return "r2";
    }
    return "none";
}

bool parse_axis(const std::string &s, MapperAxis *out) {
    for (MapperAxis a : {MapperAxis::None, MapperAxis::LX, MapperAxis::LY, MapperAxis::RX,
                         MapperAxis::RY, MapperAxis::L2, MapperAxis::R2}) {
        if (s == axis_name(a)) {
            *out = a;
            return true;
        }
    }
    return false;
}

std::string number(float v) {
    char b[48];
    snprintf(b, sizeof b, "%.9g", double(v));
    return b;
}

bool parse_float(const std::string &s, float *out) {
    if (s.empty())
        return false;
    char *end = nullptr;
    const float v = strtof(s.c_str(), &end);
    if (end != s.c_str() + s.size() || !std::isfinite(v))
        return false;
    *out = v;
    return true;
}

bool parse_int(const std::string &s, int *out) {
    if (s.empty())
        return false;
    char *end = nullptr;
    const long v = strtol(s.c_str(), &end, 10);
    if (end != s.c_str() + s.size() || v < 0 || v > 65535)
        return false;
    *out = int(v);
    return true;
}

bool parse_bool(const std::string &s, bool *out) {
    if (s == "0") {
        *out = false;
        return true;
    }
    if (s == "1") {
        *out = true;
        return true;
    }
    return false;
}

bool parse_field(const std::string &key, const std::string &value, PadMapperProfile *p) {
    if (key == "version") {
        int v;
        if (!parse_int(value, &v))
            return false;
        p->version = uint32_t(v);
        return true;
    }
    if (key == "directinput_rumble")
        return parse_bool(value, &p->directinput_rumble);
    if (key == "stick_deadzone")
        return parse_float(value, &p->stick_deadzone);

    for (int i = 0; i < kSdlAxisCount; ++i) {
        const std::string prefix = "axis." + std::to_string(i) + ".";
        if (key.rfind(prefix, 0) == 0) {
            const std::string field = key.substr(prefix.size());
            AxisCalibration &c = p->axes[size_t(i)];
            if (field == "mode") {
                if (value == "centered")
                    c.mode = MapperAxisMode::Centered;
                else if (value == "pedal")
                    c.mode = MapperAxisMode::Pedal;
                else
                    return false;
                return true;
            }
            if (field == "inverted")
                return parse_bool(value, &c.inverted);
            float n;
            if (!parse_float(value, &n))
                return false;
            if (field == "minimum")
                c.minimum = n;
            else if (field == "center")
                c.center = n;
            else if (field == "maximum")
                c.maximum = n;
            else if (field == "deadzone")
                c.deadzone = n;
            else if (field == "saturation")
                c.saturation = n;
            else if (field == "curve")
                c.curve = n;
            else
                return false;
            return true;
        }
        const std::string bp = "axis_binding." + std::to_string(i) + ".";
        if (key.rfind(bp, 0) == 0) {
            const std::string field = key.substr(bp.size());
            AxisBinding &b = p->axis_bindings[size_t(i)];
            if (field == "guest")
                return parse_axis(value, &b.guest_axis);
            if (field == "scale")
                return parse_float(value, &b.scale);
            if (field == "offset")
                return parse_float(value, &b.offset);
            if (field == "negative_guest_buttons" || field == "positive_guest_buttons") {
                int bits;
                if (!parse_int(value, &bits))
                    return false;
                (field == "negative_guest_buttons" ? b.negative_guest_buttons
                                                   : b.positive_guest_buttons) = uint16_t(bits);
                return true;
            }
            return false;
        }
    }

    for (int i = 0; i < 13; ++i) {
        const std::string prefix = "button." + std::to_string(i) + ".";
        if (key.rfind(prefix, 0) == 0) {
            const std::string field = key.substr(prefix.size());
            ButtonBinding &b = p->buttons[size_t(i)];
            int v;
            if (field == "guest_buttons") {
                if (!parse_int(value, &v))
                    return false;
                b.guest_buttons = uint16_t(v);
                return true;
            }
            if (field == "guest_hat") {
                if (!parse_int(value, &v))
                    return false;
                if (v > 15)
                    return false;
                b.guest_hat = uint8_t(v);
                return true;
            }
            return false;
        }
    }
    for (int i = 0; i < 4; ++i) {
        const std::string prefix = "hat." + std::to_string(i) + ".";
        if (key.rfind(prefix, 0) == 0) {
            const std::string field = key.substr(prefix.size());
            ButtonBinding &b = p->hat[size_t(i)];
            int v;
            if (field == "guest_buttons") {
                if (!parse_int(value, &v))
                    return false;
                b.guest_buttons = uint16_t(v);
                return true;
            }
            if (field == "guest_hat") {
                if (!parse_int(value, &v))
                    return false;
                if (v > 15)
                    return false;
                b.guest_hat = uint8_t(v);
                return true;
            }
            return false;
        }
    }
    return false;
}

} // namespace

PadMapperProfile pad_mapper_default_profile() {
    PadMapperProfile p;
    for (int i = 0; i < kSdlAxisCount; ++i) {
        p.axes[size_t(i)].mode = i >= 4 ? MapperAxisMode::Pedal : MapperAxisMode::Centered;
        p.axes[size_t(i)].deadzone = 0.0f;
        p.axes[size_t(i)].minimum = i >= 4 ? 0.0f : -1.0f;
        p.axes[size_t(i)].center = i >= 4 ? 0.0f : 0.0f;
        p.axes[size_t(i)].maximum = 1.0f;
        p.axis_bindings[size_t(i)].guest_axis = kAxes[i];
    }
    for (int i = 0; i < 13; ++i)
        p.buttons[size_t(i)].guest_buttons = kButtonBits[i];
    for (int i = 0; i < 4; ++i)
        p.hat[size_t(i)].guest_hat = kHatBits[i];
    return p;
}

PadMapperProfile pad_mapper_centered_sticks() {
    PadMapperProfile p = pad_mapper_default_profile();
    p.axis_bindings[4].guest_axis = MapperAxis::None;
    p.axis_bindings[5].guest_axis = MapperAxis::None;
    return p;
}

PadMapperProfile pad_mapper_separate_pedals() {
    return pad_mapper_default_profile();
}

PadMapperProfile pad_mapper_combined_pedals() {
    PadMapperProfile p = pad_mapper_default_profile();
    p.axis_bindings[1].guest_axis = MapperAxis::None;
    p.axis_bindings[4].guest_axis = MapperAxis::LY;
    p.axis_bindings[4].scale = -1;
    p.axis_bindings[5].guest_axis = MapperAxis::LY;
    p.buttons[6].guest_buttons = 0;
    p.buttons[7].guest_buttons = 0;
    return p;
}

PadMapperProfile pad_mapper_native_profile() {
    return pad_mapper_default_profile();
}

bool validate_pad_mapper_profile(const PadMapperProfile &p, std::string *error) {
    auto fail = [&](const std::string &s) {
        if (error)
            *error = s;
        return false;
    };
    if (p.version != 1)
        return fail("unsupported pad mapper profile version");
    if (!std::isfinite(p.stick_deadzone) || p.stick_deadzone < 0 || p.stick_deadzone >= 1)
        return fail("invalid radial stick deadzone");
    for (int i = 0; i < kSdlAxisCount; ++i) {
        const AxisCalibration &c = p.axes[size_t(i)];
        if (!std::isfinite(c.minimum) || !std::isfinite(c.center) || !std::isfinite(c.maximum) ||
            !std::isfinite(c.deadzone) || !std::isfinite(c.saturation) || !std::isfinite(c.curve))
            return fail("axis " + std::to_string(i) + " contains a non-finite value");
        if (c.mode == MapperAxisMode::Centered) {
            if (c.minimum < -1 || c.maximum > 1 || !(c.minimum < c.center && c.center < c.maximum))
                return fail("axis " + std::to_string(i) +
                            " has invalid centered calibration range");
        } else if (c.mode == MapperAxisMode::Pedal) {
            if (c.minimum < -1 || c.maximum > 1 || !(c.minimum <= c.center && c.center < c.maximum))
                return fail("axis " + std::to_string(i) + " has invalid pedal calibration range");
        } else {
            return fail("axis " + std::to_string(i) + " has invalid mode");
        }
        if (c.deadzone < 0 || c.deadzone >= 1 || c.saturation <= c.deadzone || c.saturation > 1 ||
            c.curve <= 0 || c.curve > 8)
            return fail("axis " + std::to_string(i) + " has invalid response settings");
        const AxisBinding &b = p.axis_bindings[size_t(i)];
        if (axis_index(b.guest_axis) < 0 && b.guest_axis != MapperAxis::None)
            return fail("axis " + std::to_string(i) + " has invalid guest axis");
        if (!std::isfinite(b.scale) || !std::isfinite(b.offset) || std::fabs(b.scale) > 8 ||
            std::fabs(b.offset) > 8)
            return fail("axis " + std::to_string(i) + " has invalid output transform");
        if ((b.negative_guest_buttons | b.positive_guest_buttons) & ~uint16_t(0x1fff))
            return fail("axis " + std::to_string(i) + " has invalid guest button bits");
    }
    for (int i = 0; i < 13; ++i) {
        const ButtonBinding &b = p.buttons[size_t(i)];
        if (b.guest_buttons & ~uint16_t(0x1fff))
            return fail("button " + std::to_string(i) + " has invalid guest button bits");
        if (b.guest_hat & ~uint8_t(0x0f))
            return fail("button " + std::to_string(i) + " has invalid guest hat bits");
    }
    for (int i = 0; i < 4; ++i) {
        const ButtonBinding &b = p.hat[size_t(i)];
        if (b.guest_buttons & ~uint16_t(0x1fff))
            return fail("hat direction " + std::to_string(i) + " has invalid guest button bits");
        if (b.guest_hat & ~uint8_t(0x0f))
            return fail("hat direction " + std::to_string(i) + " has invalid guest hat bits");
    }
    return true;
}

std::string serialize_pad_mapper_profile(const PadMapperProfile &p, std::string *error) {
    std::string validation_error;
    if (!validate_pad_mapper_profile(p, &validation_error)) {
        if (error)
            *error = validation_error;
        return {};
    }
    if (error)
        error->clear();
    std::string out = "version=1\nstick_deadzone=" + number(p.stick_deadzone) + "\n";
    out += std::string("directinput_rumble=") + (p.directinput_rumble ? "1\n" : "0\n");
    for (int i = 0; i < kSdlAxisCount; ++i) {
        const AxisCalibration &c = p.axes[size_t(i)];
        const std::string x = "axis." + std::to_string(i) + ".";
        out += x + "mode=" + (c.mode == MapperAxisMode::Pedal ? "pedal" : "centered") + "\n";
        out += x + "minimum=" + number(c.minimum) + "\n";
        out += x + "center=" + number(c.center) + "\n";
        out += x + "maximum=" + number(c.maximum) + "\n";
        out += x + "deadzone=" + number(c.deadzone) + "\n";
        out += x + "saturation=" + number(c.saturation) + "\n";
        out += x + "curve=" + number(c.curve) + "\n";
        out += x + "inverted=" + (c.inverted ? "1" : "0") + "\n";
        const AxisBinding &b = p.axis_bindings[size_t(i)];
        const std::string y = "axis_binding." + std::to_string(i) + ".";
        out += y + "guest=" + axis_name(b.guest_axis) + "\n";
        out += y + "scale=" + number(b.scale) + "\n";
        out += y + "offset=" + number(b.offset) + "\n";
        out += y + "negative_guest_buttons=" + std::to_string(b.negative_guest_buttons) + "\n";
        out += y + "positive_guest_buttons=" + std::to_string(b.positive_guest_buttons) + "\n";
    }
    for (int i = 0; i < 13; ++i) {
        const ButtonBinding &b = p.buttons[size_t(i)];
        const std::string x = "button." + std::to_string(i) + ".";
        out += x + "guest_buttons=" + std::to_string(b.guest_buttons) + "\n";
        out += x + "guest_hat=" + std::to_string(b.guest_hat) + "\n";
    }
    for (int i = 0; i < 4; ++i) {
        const ButtonBinding &b = p.hat[size_t(i)];
        const std::string x = "hat." + std::to_string(i) + ".";
        out += x + "guest_buttons=" + std::to_string(b.guest_buttons) + "\n";
        out += x + "guest_hat=" + std::to_string(b.guest_hat) + "\n";
    }
    return out;
}

bool parse_pad_mapper_profile(const std::string &text, PadMapperProfile *profile,
                              std::string *error) {
    auto fail = [&](const std::string &s) {
        if (error)
            *error = s;
        return false;
    };
    if (!profile)
        return fail("missing profile output");
    PadMapperProfile next = pad_mapper_default_profile();
    std::set<std::string> seen;
    size_t pos = 0;
    while (pos < text.size()) {
        const size_t end = text.find('\n', pos);
        std::string line =
            text.substr(pos, end == std::string::npos ? std::string::npos : end - pos);
        if (!line.empty() && line.back() == '\r')
            line.pop_back();
        pos = end == std::string::npos ? text.size() : end + 1;
        if (line.empty())
            continue;
        const size_t eq = line.find('=');
        if (eq == std::string::npos || eq == 0 || eq + 1 == line.size())
            return fail("invalid profile line: " + line);
        const std::string key = line.substr(0, eq), value = line.substr(eq + 1);
        if (!seen.insert(key).second)
            return fail("duplicate profile field: " + key);
        if (!parse_field(key, value, &next))
            return fail("invalid profile field: " + key);
    }
    if (!seen.count("version"))
        return fail("missing profile version");
    if (seen.size() - seen.count("directinput_rumble") != 114)
        return fail("incomplete pad mapper profile");
    std::string validation;
    if (!validate_pad_mapper_profile(next, &validation))
        return fail(validation);
    *profile = next;
    if (error)
        error->clear();
    return true;
}

PadMapper::PadMapper() : profile_(pad_mapper_default_profile()) {}

bool PadMapper::set_profile(const PadMapperProfile &profile, std::string *error) {
    if (!validate_pad_mapper_profile(profile, error))
        return false;
    std::lock_guard<std::mutex> lock(mutex_);
    profile_ = profile;
    return true;
}

PadMapperProfile PadMapper::snapshot() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return profile_;
}

void PadMapper::process(const PadState &input, PadState *calibrated, PadState *guest) {
    std::lock_guard<std::mutex> lock(mutex_);
    PadState physical;
    physical.buttons = input.buttons & uint16_t(0x1fff);
    physical.hat = input.hat & uint8_t(0x0f);
    for (int i = 0; i < kSdlAxisCount; ++i)
        set_axis_value(&physical, i,
                       calibrated_axis(axis_value(input, i), profile_.axes[size_t(i)]));
    if (profile_.axes[0].mode == MapperAxisMode::Centered &&
        profile_.axes[1].mode == MapperAxisMode::Centered &&
        std::hypot(physical.lx, physical.ly) < profile_.stick_deadzone)
        physical.lx = physical.ly = 0;
    if (profile_.axes[2].mode == MapperAxisMode::Centered &&
        profile_.axes[3].mode == MapperAxisMode::Centered &&
        std::hypot(physical.rx, physical.ry) < profile_.stick_deadzone)
        physical.rx = physical.ry = 0;
    physical.buttons &= uint16_t(~(kPadL2 | kPadR2));
    if (physical.l2 > 0.5f)
        physical.buttons |= kPadL2;
    if (physical.r2 > 0.5f)
        physical.buttons |= kPadR2;
    if (calibrated)
        *calibrated = physical;

    PadState mapped;
    float mapped_axes[kSdlAxisCount] = {};
    for (int i = 0; i < kSdlAxisCount; ++i) {
        const AxisBinding &b = profile_.axis_bindings[size_t(i)];
        const float value = axis_value(physical, i);
        route_axis(mapped_axes, b.guest_axis, value * b.scale + b.offset);
        if (value < -0.5f)
            mapped.buttons |= b.negative_guest_buttons;
        if (value > 0.5f)
            mapped.buttons |= b.positive_guest_buttons;
    }
    for (int i = 0; i < 13; ++i) {
        if (physical.buttons & kButtonBits[i]) {
            const ButtonBinding &b = profile_.buttons[size_t(i)];
            mapped.buttons |= b.guest_buttons;
            mapped.hat |= b.guest_hat;
        }
    }
    for (int i = 0; i < 4; ++i) {
        if (physical.hat & kHatBits[i]) {
            const ButtonBinding &b = profile_.hat[size_t(i)];
            mapped.buttons |= b.guest_buttons;
            mapped.hat |= b.guest_hat;
        }
    }
    for (int i = 0; i < kSdlAxisCount; ++i) {
        const float lo = i < 4 ? -1.0f : 0.0f;
        set_axis_value(&mapped, i, std::max(lo, std::min(1.0f, mapped_axes[i])));
    }
    if (guest)
        *guest = mapped;
}

} // namespace controls
