#include "pad_mapper_editor.h"

#include "../keypad_layout.h"

#include <algorithm>
#include <cmath>
#include <cstdio>

namespace controls {
namespace {
const char *const axes[] = {"Left stick horizontal",  "Left stick vertical",
                            "Right stick horizontal", "Right stick vertical",
                            "Left trigger",           "Right trigger"};
const char *const buttons[] = {"South / A / Cross",
                               "East / B / Circle",
                               "West / X / Square",
                               "North / Y / Triangle",
                               "Left shoulder",
                               "Right shoulder",
                               "Left trigger button",
                               "Right trigger button",
                               "Left stick click",
                               "Right stick click",
                               "Back / View",
                               "Start / Menu",
                               "Guide"};
const char *const hats[] = {"D-pad up", "D-pad right", "D-pad down", "D-pad left"};
const uint16_t bits[] = {kPadCross, kPadCircle, kPadSquare, kPadTriangle, kPadL1,    kPadR1, kPadL2,
                         kPadR2,    kPadL3,     kPadR3,     kPadSelect,   kPadStart, kPadPs};
const uint8_t hat_bits[] = {kHatUp, kHatRight, kHatDown, kHatLeft};
const char *const presets[] = {"Standard controller", "Combined triggers / pedals"};
constexpr int kPresetCount = sizeof(presets) / sizeof(presets[0]);
const char *const preset_help[] = {
    "Pass sticks, triggers, buttons and D-pad through to the game controller.",
    "Combine the two triggers into opposite halves of one output axis."};
float axis(const PadState &p, int i) {
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
std::string number(float v, bool percent = false) {
    char text[24];
    if (percent)
        snprintf(text, sizeof text, "%.0f%%", v * 100);
    else
        snprintf(text, sizeof text, "%.2f", v);
    return text;
}
std::string source_name(int source) {
    return source < 6 ? axes[source] : source < 19 ? buttons[source - 6] : hats[source - 19];
}
std::string output_name(int target) {
    return target == 0 ? "None" : target <= 13 ? buttons[target - 1] : hats[target - 14];
}
std::string button_name(const ButtonBinding &b) {
    for (int i = 0; i < 13; ++i)
        if (b.guest_buttons == bits[i] && !b.guest_hat)
            return buttons[i];
    for (int i = 0; i < 4; ++i)
        if (b.guest_hat == hat_bits[i] && !b.guest_buttons)
            return hats[i];
    return b.guest_buttons || b.guest_hat ? "Multiple outputs" : "None";
}
bool active(const PadState &p, int source) {
    return source < 6    ? std::abs(axis(p, source)) > 0.2f
           : source < 19 ? (p.buttons & bits[source - 6]) != 0
                         : (p.hat & hat_bits[source - 19]) != 0;
}
} // namespace

void PadMapperEditor::open(const PadMapperProfile &profile, const Screen &screen) {
    profile_ = profile;
    original_ = serialize_pad_mapper_profile(profile_);
    screen_ = screen;
    page_ = Page::Overview;
    popup_ = Popup::None;
    capture_ = Capture::None;
    action_ = PadMapperEditorAction::None;
    validation_error_.clear();
    open_ = true;
    close_requested_ = false;
    advanced_ = sweep_ = center_capture_ = false;
    selected_ = group_ = scroll_ = detail_scroll_ = 0;
    focus_ = 1;
    pressed_ = -1;
    preset_pending_ = -1;
    changed();
}
void PadMapperEditor::close() {
    suspend();
    open_ = false;
}
void PadMapperEditor::set_screen(const Screen &s) {
    screen_ = s;
}
void PadMapperEditor::set_device(const std::string &name, bool connected) {
    if (device_ != name || connected_ != connected) {
        device_ = name;
        connected_ = connected;
        if (!connected)
            suspend();
        changed();
    }
}
void PadMapperEditor::set_message(const std::string &message) {
    validation_error_ = message;
    changed();
}
void PadMapperEditor::suspend() {
    capture_ = Capture::None;
    pressed_ = -1;
    cancel_calibration();
}
bool PadMapperEditor::is_open() const {
    return open_;
}
const PadMapperProfile &PadMapperEditor::profile() const {
    return profile_;
}
void PadMapperEditor::changed() {
    ++revision_;
}
ButtonBinding &PadMapperEditor::button() {
    return selected_ < 19 ? profile_.buttons[size_t(selected_ - 6)]
                          : profile_.hat[size_t(selected_ - 19)];
}
void PadMapperEditor::add_key(int x, int y, int w, int h, const std::string &label, bool lit,
                              int id) {
    DrawControl d;
    d.kind = Kind::Key;
    d.rect = {x, y, w, h};
    d.label = label;
    d.text_scale = std::max(1, int(std::lround(screen_.scale)));
    d.lit = lit;
    d.focused = focus_ == id;
    frame_.toolbar.push_back(d);
    widget_ids_.push_back({d.rect, id});
}
void PadMapperEditor::add_text(int x, int y, int w, int h, const std::string &text, bool muted,
                               int scale) {
    frame_.text.push_back({{x, y, w, h}, text, scale, muted});
}
void PadMapperEditor::add_meter(int x, int y, int w, int h, const std::string &label, float value,
                                bool bipolar) {
    frame_.meters.push_back(
        {{x, y, w, h}, label, value, bipolar, std::max(1, int(std::lround(screen_.scale)))});
}

void PadMapperEditor::observe_input(const PadState &raw) {
    if (capture_ == Capture::Identify && connected_) {
        int found = -1;
        float movement = 0.35f;
        for (int i = 0; i < 6; ++i) {
            const float delta = std::abs(axis(raw, i) - axis(identify_start_, i));
            if (delta > movement) {
                movement = delta;
                found = i;
            }
        }
        for (int i = 0; i < 13; ++i)
            if ((raw.buttons & bits[i]) && !(identify_start_.buttons & bits[i]))
                found = i + 6;
        for (int i = 0; i < 4; ++i)
            if ((raw.hat & hat_bits[i]) && !(identify_start_.hat & hat_bits[i]))
                found = i + 19;
        if (found >= 0) {
            capture_ = Capture::None;
            select(found);
            validation_error_ = "Selected " + source_name(found) + ". Choose its outputs below.";
        }
    }
    last_raw_ = raw;
}

ControlsView PadMapperEditor::view(const Screen &screen, const PadState &raw,
                                   const PadState &calibrated, const PadState &guest_preview,
                                   const std::string &status) {
    set_screen(screen);
    observe_input(raw);
    if (center_capture_ && connected_) {
        center_sum_ += axis(raw, selected_);
        if (++center_samples_ >= 30) {
            AxisCalibration next = profile_.axes[size_t(selected_)];
            next.center = float(center_sum_ / center_samples_);
            if (next.mode == MapperAxisMode::Pedal)
                next.minimum = next.center;
            if (next.center < next.maximum &&
                (next.mode == MapperAxisMode::Pedal || next.center > next.minimum)) {
                profile_.axes[size_t(selected_)] = next;
                validation_error_ = "Rest position recorded. Move the input to check its response.";
            } else
                validation_error_ =
                    "Rest position is outside the range. Restore this axis and try again.";
            center_capture_ = false;
        }
    }
    if (sweep_ && connected_) {
        sweep_min_ = std::min(sweep_min_, axis(raw, selected_));
        sweep_max_ = std::max(sweep_max_, axis(raw, selected_));
    }
    frame_ = ControlsView{};
    frame_.wanted = frame_.editing = true;
    frame_.dw = screen.dw;
    frame_.dh = screen.dh;
    frame_.layers.push_back({{0, 0, screen.dw, screen.dh}, ++revision_});
    widget_ids_.clear();
    const double scale = std::max(0.75, screen.scale);
    const auto px = [scale](int v) { return int(std::lround(v * scale)); };
    const int w = screen.dw, h = screen.dh, m = px(18), gap = px(8), row = px(30);
    const int text_scale = std::max(1, int(std::lround(scale)));
    const auto text = [&](int x, int y, int width, int height, const std::string &s,
                          bool muted = true) {
        add_text(x, y, width, height, s, muted, text_scale);
    };
    add_text(m, m, w - 2 * m, px(24), "Controller setup", false, 2 * text_scale);
    text(m, px(46), w - 2 * m, px(22), connected_ ? device_ : "No controller connected");
    const int tab_w = std::min(px(148), (w - 2 * m - 2 * gap) / 3);
    const char *tabs[] = {"Overview", "Mapping", "Calibration"};
    for (int i = 0; i < 3; ++i)
        add_key(m + i * (tab_w + gap), px(76), tab_w, row, tabs[i], int(page_) == i, i + 1);
    const int top = px(154), bottom = h - px(76), body_h = bottom - top;
    const bool dirty = serialize_pad_mapper_profile(profile_) != original_;
    const int footer_y = h - px(52), button_w = px(100);
    add_key(w - m - 2 * button_w - gap, footer_y, button_w, row, "Cancel", false, 11);
    add_key(w - m - button_w, footer_y, button_w, row, "Save", dirty, 10);
    if (dirty)
        text(m, footer_y + px(8), w - 3 * m - 2 * button_w - gap, px(22), "Unsaved changes");
    std::string message = validation_error_.empty() ? status : validation_error_;
    if (capture_ == Capture::Identify)
        message = "Move one stick or trigger, or press a button. Esc stops identifying.";
    else if (capture_ != Capture::None)
        message = "Press the keyboard key to assign. Use Clear to remove the assignment.";
    else if (center_capture_)
        message = "Keep this input at rest. Recording its resting position...";
    else if (sweep_)
        message = "Move this input through its full travel, then choose Finish range. Cancel "
                  "capture restores it.";
    if (!message.empty())
        text(m, px(114), w - 2 * m, px(34), message, false);

    if (page_ == Page::Overview) {
        const int col = (w - 2 * m - gap) / 2, right = m + col + gap;
        text(m, top, col, px(22), "Controller mappings", false);
        text(m, top + px(30), col, px(46),
             "Change controller outputs and add keyboard keys in Mapping.");
        add_key(m, top + px(112), px(148), row, "Presets...", false, 12);
        add_key(m, top + px(152), px(148), row, "Test rumble", false, 14);
        add_key(m, top + px(192), col, row,
                std::string("DirectInput force feedback as rumble: ") +
                    (profile_.directinput_rumble ? "On" : "Off"),
                profile_.directinput_rumble, 15);
        text(m, top + px(230), col, px(46),
             "Approximate older games' force effects using controller vibration. "
             "Off hides rumble-based force feedback from games.");
        text(right, top, col, px(22), "Test controller", false);
        for (int i = 0; i < 6; ++i)
            add_meter(right, top + px(26) + i * px(28), col, px(22), axes[i], axis(raw, i), i < 4);

        std::string held;
        for (int i = 6; i < 23; ++i)
            if (active(raw, i))
                held += (held.empty() ? "" : ", ") + source_name(i);
        text(right, top + px(204), col, std::max(px(22), body_h - px(204)),
             held.empty() ? "Press a button or move a stick to test." : held, false);
    } else {
        const int side_w = std::min(px(208), (w - 2 * m) / 3), x = m + side_w + gap,
                  content_w = w - m - x;
        sidebar_edge_ = x;
        const int first = page_ == Page::Calibration ? 0 : group_ == 0 ? 0 : group_ == 1 ? 6 : 19;
        const int count = page_ == Page::Calibration || group_ == 0 ? 6 : group_ == 1 ? 13 : 4;
        if (page_ == Page::Mapping) {
            const char *groups[] = {"Axes", "Buttons", "D-pad"};
            const int gw = (side_w - 2 * gap) / 3;
            for (int i = 0; i < 3; ++i)
                add_key(m + i * (gw + gap), top, gw, row, groups[i], group_ == i, 30 + i);
        } else
            text(m, top + px(7), side_w, row, "Physical axes", false);
        const int list_top = top + row + gap, list_bottom = bottom - 2 * row - 2 * gap;
        visible_rows_ = std::max(1, (list_bottom - list_top) / row);
        scroll_ = std::clamp(scroll_, 0, std::max(0, count - visible_rows_));
        for (int i = scroll_; i < std::min(count, scroll_ + visible_rows_); ++i) {
            const int source = first + i;
            add_key(m, list_top + (i - scroll_) * row, side_w, row - px(3),
                    source_name(source) + (active(raw, source) ? " *" : ""), selected_ == source,
                    100 + source);
        }
        if (count > visible_rows_) {
            add_key(m, bottom - 2 * row - gap, (side_w - gap) / 2, row, "Earlier", false, 20);
            add_key(m + (side_w + gap) / 2, bottom - 2 * row - gap, (side_w - gap) / 2, row,
                    "Later", false, 21);
        }
        if (page_ == Page::Mapping)
            add_key(m, bottom - row, side_w, row,
                    capture_ == Capture::Identify ? "Identifying..." : "Identify input",
                    capture_ == Capture::Identify, 16);
        text(x, top, content_w, px(24), source_name(selected_), false);
        const int detail_top = top + px(32), detail_bottom = bottom - row - gap;
        int index = 0;
        const auto line = [&](const std::string &label, const std::string &value, int id,
                              int minus = 0, int plus = 0) {
            const int y = detail_top + (index++ - detail_scroll_) * px(35);
            if (y < detail_top || y + row > detail_bottom)
                return;
            const int label_w = std::min(px(170), content_w * 44 / 100);
            text(x, y + px(8), label_w - gap, row - px(8), label, false);
            const int value_x = x + label_w, value_w = std::min(px(260), content_w - label_w);
            if (minus) {
                const int small = px(28);
                add_key(value_x, y, small, row, "-", false, minus);
                text(value_x + small + gap, y + px(8), value_w - 2 * (small + gap), row - px(8),
                     value, false);
                add_key(value_x + value_w - small, y, small, row, "+", false, plus);
            } else if (id)
                add_key(value_x, y, value_w, row, value, false, id);
            else
                text(value_x, y + px(8), value_w, row - px(8), value);
        };
        const auto note = [&](const std::string &s) {
            const int y = detail_top + (index++ - detail_scroll_) * px(35);
            if (y >= detail_top && y + row <= detail_bottom)
                text(x, y + px(3), content_w, row, s);
        };
        if (page_ == Page::Mapping) {
            if (selected_ < 6) {
                const AxisBinding &b = profile_.axis_bindings[size_t(selected_)];
                line("Controller output",
                     b.guest_axis == MapperAxis::None ? "None" : axes[int(b.guest_axis) - 1], 200);
                line("", advanced_ ? "Fewer options" : "More options...", 13);
                if (advanced_) {
                    line("Output scale", number(b.scale), 0, 210, 211);
                    line("Output offset", number(b.offset), 0, 212, 213);
                    note("Output = calibrated input x scale + offset; clipped to the axis range.");
                    line("Negative direction button", button_name({b.negative_guest_buttons, 0}),
                         214);
                    line("Positive direction button", button_name({b.positive_guest_buttons, 0}),
                         215);
                    note("Direction outputs engage at half travel.");
                }
            } else {
                const ButtonBinding &b = button();
                line("Controller output", button_name(b), 205);
                const char *key = scancode_name(b.guest_key);
                line("Keyboard output",
                     capture_ == Capture::Key ? "Press a key..."
                     : key                    ? key
                                              : "None",
                     206);
                line("", "Clear", 207);
            }
            if (selected_ < 6) {
                const int y = detail_top + (index++ - detail_scroll_) * px(35);
                if (y >= detail_top && y + row <= detail_bottom) {
                    const auto target = profile_.axis_bindings[size_t(selected_)].guest_axis;
                    add_meter(x, y, content_w, row, "Output",
                              target != MapperAxis::None ? axis(guest_preview, int(target) - 1) : 0,
                              target != MapperAxis::L2 && target != MapperAxis::R2);
                }
            }
        } else {
            const AxisCalibration &c = profile_.axes[size_t(selected_)];
            const int meter_y = detail_top + (index++ - detail_scroll_) * px(35);
            if (meter_y >= detail_top && meter_y + row <= detail_bottom) {
                const int mw = (content_w - gap) / 2;
                add_meter(x, meter_y, mw, row, "Device " + number(axis(raw, selected_)),
                          axis(raw, selected_), selected_ < 4);
                add_meter(x + mw + gap, meter_y, mw, row,
                          "Adjusted " + number(axis(calibrated, selected_)),
                          axis(calibrated, selected_), c.mode == MapperAxisMode::Centered);
            }
            line("Axis mode",
                 c.mode == MapperAxisMode::Centered ? "Centered (stick / wheel)"
                                                    : "One-sided (trigger / pedal)",
                 236);
            line("Deadzone", number(c.deadzone, true), 0, 220, 221);

            line("Reverse direction", c.inverted ? "[x] Reversed" : "[ ] Reversed", 222);
            line("Resting position", center_capture_ ? "Recording..." : "Record at rest", 223);
            line("Full movement range", sweep_ ? "Finish range" : "Record range",
                 sweep_ ? 225 : 224);
            if (sweep_ || center_capture_)
                line("Calibration in progress", "Cancel capture", 226);
            line("", advanced_ ? "Fewer options" : "More options...", 13);
            if (advanced_) {
                line("Reach full output at", number(c.saturation, true), 0, 230, 231);
                line("Response curve", number(c.curve), 0, 232, 233);
                note("1.00 is linear. Higher values soften movement near rest.");
                line("Stick pair deadzone", number(profile_.stick_deadzone, true), 0, 234, 235);
                note("Shared circular deadzone for both sticks; separate from the axis deadzone "
                     "above.");
                line("Recorded minimum", number(c.minimum), 0);
                line("Recorded rest", number(c.center), 0);
                line("Recorded maximum", number(c.maximum), 0);
            }
            line("", "Reset this axis", 227);
        }
        const int visible = std::max(1, (detail_bottom - detail_top) / px(35));
        detail_max_ = std::max(0, index - visible);
        detail_scroll_ = std::clamp(detail_scroll_, 0, detail_max_);
        if (detail_max_ > 0) {
            const int small = px(92);
            add_key(x, bottom - row, small, row, "Scroll up", false, 90);
            add_key(x + small + gap, bottom - row, small, row, "Scroll down", false, 91);
        }
    }
    if (popup_ != Popup::None) {
        widget_ids_.clear();
        frame_.guides.clear();
        const int popup_height = popup_ == Popup::Discard                           ? 240
                                 : popup_ == Popup::Preset && preset_pending_ >= 0  ? 260
                                 : popup_ == Popup::Axis || popup_ == Popup::Preset ? 300
                                                                                    : 480;
        const int pw = std::min(w - 2 * m, px(560)), ph = std::min(h - 2 * m, px(popup_height));
        const int x = (w - pw) / 2, y = (h - ph) / 2;
        frame_.picker = {x - gap, y - gap, pw + 2 * gap, ph + 2 * gap};
        const size_t text_start = frame_.text.size();
        const size_t toolbar_start = frame_.toolbar.size();
        const bool preset_popup = popup_ == Popup::Preset;
        const bool discard_popup = popup_ == Popup::Discard;
        add_text(x + gap, y + gap, pw - 2 * gap, px(24),
                 discard_popup  ? "Discard unsaved changes?"
                 : preset_popup ? "Choose a starting preset"
                                : "Choose controller output",
                 false, text_scale);
        int at = y + px(38);
        if (discard_popup) {
            add_text(x + gap, at, pw - 2 * gap, px(55),
                     "Your saved profile will be kept. Changes made in this editor will be lost.",
                     true, text_scale);
            add_key(x, at + px(72), pw, row, "Keep editing", true, 521);
            add_key(x, at + px(112), pw, row, "Discard changes", false, 522);
        } else if (preset_popup) {
            if (preset_pending_ >= 0) {
                add_text(x + gap, at, pw - 2 * gap, px(80),
                         std::string(presets[preset_pending_]) + ": " +
                             preset_help[preset_pending_] +
                             " This replaces all draft mappings and calibration.",
                         false, text_scale);
                add_key(x, at + px(96), pw, row, "Replace draft with this preset", false, 520);
                add_key(x, at + px(136), pw, row, "Back to presets", false, 523);
            } else
                for (int i = 0; i < kPresetCount; ++i) {
                    add_key(x, at, pw, row, presets[i], false, 500 + i);
                    add_text(x + gap, at + row + px(3), pw - 2 * gap, px(26), preset_help[i], true,
                             text_scale);
                    at += px(67);
                }
        } else {
            const int count = popup_ == Popup::Axis ? 7 : popup_ == Popup::Button ? 18 : 14;
            const int cw = (pw - gap) / 2;
            for (int i = 0; i < count; ++i) {
                const std::string label =
                    popup_ == Popup::Axis ? (i ? axes[i - 1] : "None") : output_name(i);
                bool selected = false;
                if (popup_ == Popup::Button)
                    selected = label == button_name(button());
                else {
                    const auto &binding = profile_.axis_bindings[size_t(selected_)];
                    if (popup_ == Popup::Axis)
                        selected = i == int(binding.guest_axis);
                    else
                        selected = label == button_name({popup_ == Popup::NegativeButton
                                                             ? binding.negative_guest_buttons
                                                             : binding.positive_guest_buttons,
                                                         0});
                }
                add_key(x + (i % 2) * (cw + gap), at + (i / 2) * (row + gap), cw, row, label,
                        selected, 400 + i);
            }
        }
        if (!discard_popup)
            add_key(x, y + ph - row, pw, row, "Back", false, 521);
        frame_.picker_rows.assign(frame_.toolbar.begin() + toolbar_start, frame_.toolbar.end());
        frame_.toolbar.resize(toolbar_start);
        frame_.popup_text.assign(frame_.text.begin() + text_start, frame_.text.end());
        frame_.text.resize(text_start);
    }
    std::stable_sort(widget_ids_.begin(), widget_ids_.end(), [](const auto &a, const auto &b) {
        return a.first.y == b.first.y ? a.first.x < b.first.x : a.first.y < b.first.y;
    });
    frame_.revision = revision_;
    return frame_;
}

void PadMapperEditor::cancel_calibration() {
    if (sweep_ || center_capture_)
        profile_.axes[size_t(selected_)] = before_capture_;
    sweep_ = center_capture_ = false;
    center_sum_ = 0;
    center_samples_ = 0;
}
void PadMapperEditor::select(int source) {
    cancel_calibration();
    selected_ = std::clamp(source, 0, 22);
    group_ = selected_ < 6 ? 0 : selected_ < 19 ? 1 : 2;
    const int first = group_ == 0 ? 0 : group_ == 1 ? 6 : 19;
    scroll_ = std::max(0, selected_ - first - visible_rows_ + 1);
    detail_scroll_ = 0;
    capture_ = Capture::None;
    changed();
}
void PadMapperEditor::preset(int index) {
    bool directinput_rumble = profile_.directinput_rumble;
    switch (index) {
    case 0:
        profile_ = pad_mapper_native_profile();
        break;
    case 1:
        profile_ = pad_mapper_combined_pedals();
        break;
    }
    profile_.directinput_rumble = directinput_rumble;
}
void PadMapperEditor::activate(int id) {
    validation_error_.clear();
    if (id >= 1 && id <= 3) {
        cancel_calibration();
        capture_ = Capture::None;
        page_ = Page(id - 1);
        if (page_ == Page::Calibration && selected_ >= 6)
            select(0);
        scroll_ = detail_scroll_ = 0;
    } else if (id == 10) {
        if (sweep_ || center_capture_ || capture_ != Capture::None)
            validation_error_ = "Finish or cancel input capture before saving.";
        else
            action_ = PadMapperEditorAction::Apply;
    } else if (id == 11)
        escape();
    else if (id == 12) {
        popup_ = Popup::Preset;
        preset_pending_ = -1;
        focus_ = 500;
    } else if (id == 13) {
        advanced_ = !advanced_;
        detail_scroll_ = 0;
    } else if (id == 14) {
        action_ = PadMapperEditorAction::TestRumble;
    } else if (id == 15) {
        profile_.directinput_rumble = !profile_.directinput_rumble;
    } else if (id == 16) {
        if (!connected_)
            validation_error_ = "Connect a controller before identifying an input.";
        else {
            capture_ = Capture::Identify;
            identify_start_ = last_raw_;
        }
    } else if (id == 20)
        scroll_ = std::max(0, scroll_ - 1);
    else if (id == 21)
        ++scroll_;
    else if (id >= 30 && id <= 32)
        select(id == 30 ? 0 : id == 31 ? 6 : 19);
    else if (id == 90)
        detail_scroll_ = std::max(0, detail_scroll_ - 1);
    else if (id == 91)
        detail_scroll_ = std::min(detail_max_, detail_scroll_ + 1);
    else if (id >= 100 && id < 123)
        select(id - 100);
    else if (id == 200) {
        popup_ = Popup::Axis;
        focus_ = 400;
    } else if (id == 205) {
        popup_ = Popup::Button;
        focus_ = 400;
    } else if (id == 206) {
        capture_ = Capture::Key;
    } else if (id == 207) {
        button().guest_key = 0;
    } else if (id >= 210 && id <= 215) {
        AxisBinding &b = profile_.axis_bindings[size_t(selected_)];
        if (id == 210 || id == 211)
            b.scale = std::clamp(b.scale + (id == 210 ? -0.05f : 0.05f), -4.0f, 4.0f);
        else if (id == 212 || id == 213)
            b.offset = std::clamp(b.offset + (id == 212 ? -0.05f : 0.05f), -1.0f, 1.0f);
        else {
            popup_ = id == 214 ? Popup::NegativeButton : Popup::PositiveButton;
            focus_ = 400;
        }
    } else if (id >= 220 && id <= 236 && selected_ < 6) {
        AxisCalibration &c = profile_.axes[size_t(selected_)];
        if (id != 225 && id != 226 && (sweep_ || center_capture_)) {
            validation_error_ = "Finish or cancel the current calibration first.";
            return;
        }
        if (id == 220 || id == 221)
            c.deadzone = std::clamp(c.deadzone + (id == 220 ? -0.01f : 0.01f), 0.0f,
                                    std::min(0.95f, c.saturation - 0.01f));
        else if (id == 222)
            c.inverted = !c.inverted;
        else if (id == 223 || id == 224) {
            if (!connected_)
                validation_error_ = "Connect a controller before recording calibration.";
            else {
                before_capture_ = c;
                if (id == 223) {
                    center_capture_ = true;
                    center_sum_ = 0;
                    center_samples_ = 0;
                } else {
                    sweep_ = true;
                    sweep_min_ = sweep_max_ = axis(last_raw_, selected_);
                }
            }
        } else if (id == 225) {
            if (sweep_max_ - sweep_min_ < 0.05f ||
                (c.mode == MapperAxisMode::Centered &&
                 (sweep_min_ >= c.center || sweep_max_ <= c.center)))
                validation_error_ = "Move through both ends of the input's range, including its "
                                    "resting position, then finish.";
            else {
                c.minimum = sweep_min_;
                c.maximum = sweep_max_;
                if (c.mode == MapperAxisMode::Pedal)
                    c.center = c.minimum;
                sweep_ = false;
                validation_error_ = "Range recorded. Move the input to check its response.";
            }
        } else if (id == 226)
            cancel_calibration();
        else if (id == 227)
            c = pad_mapper_default_profile().axes[size_t(selected_)];
        else if (id == 230 || id == 231)
            c.saturation =
                std::clamp(c.saturation + (id == 230 ? -0.01f : 0.01f), c.deadzone + 0.01f, 1.0f);
        else if (id == 232 || id == 233)
            c.curve = std::clamp(c.curve + (id == 232 ? -0.1f : 0.1f), 0.1f, 5.0f);
        else if (id == 234 || id == 235)
            profile_.stick_deadzone =
                std::clamp(profile_.stick_deadzone + (id == 234 ? -0.01f : 0.01f), 0.0f, 0.95f);
        else if (id == 236) {
            c.mode = c.mode == MapperAxisMode::Centered ? MapperAxisMode::Pedal
                                                        : MapperAxisMode::Centered;
            c.minimum = selected_ < 4 ? -1 : 0;
            c.maximum = 1;
            c.center = c.mode == MapperAxisMode::Pedal ? c.minimum : selected_ < 4 ? 0 : 0.5f;
        }
    } else if (id >= 400 && id < 418 && popup_ != Popup::None) {
        const int target = id - 400;
        if (popup_ == Popup::Axis && target < 7)
            profile_.axis_bindings[size_t(selected_)].guest_axis = MapperAxis(target);
        else if (popup_ == Popup::Button) {
            ButtonBinding &b = button();
            b.guest_buttons = target > 0 && target <= 13 ? bits[target - 1] : 0;
            b.guest_hat = target >= 14 ? hat_bits[target - 14] : 0;
        } else if (popup_ == Popup::NegativeButton || popup_ == Popup::PositiveButton) {
            uint16_t &out = popup_ == Popup::NegativeButton
                                ? profile_.axis_bindings[size_t(selected_)].negative_guest_buttons
                                : profile_.axis_bindings[size_t(selected_)].positive_guest_buttons;
            out = target > 0 && target <= 13 ? bits[target - 1] : 0;
        }
        popup_ = Popup::None;
        focus_ = selected_ < 6 ? 200 : 205;
    } else if (id >= 500 && id < 500 + kPresetCount) {
        preset_pending_ = id - 500;
        focus_ = 523;
    } else if (id == 520 && preset_pending_ >= 0) {
        cancel_calibration();
        preset(preset_pending_);
        popup_ = Popup::None;
        validation_error_ = "Preset loaded into the draft. Test it, then Save to keep it.";
        focus_ = 12;
    } else if (id == 521) {
        close_requested_ = false;
        popup_ = Popup::None;
        focus_ = int(page_) + 1;
    } else if (id == 522)
        action_ = PadMapperEditorAction::Cancel;
    else if (id == 523) {
        preset_pending_ = -1;
        focus_ = 500;
    }
    widget_ids_.clear();
    changed();
}
void PadMapperEditor::pointer(double x, double y, int state) {
    pointer_x_ = x;
    if (!open_)
        return;
    int hit = -1;
    for (const auto &widget : widget_ids_)
        if (widget.first.contains(x, y)) {
            hit = widget.second;
            break;
        }
    if (state == 1) {
        pressed_ = hit;
        if (hit >= 0)
            focus_ = hit;
    } else if (state < 0) {
        const int was_pressed = pressed_;
        pressed_ = -1;
        if (hit >= 0 && hit == was_pressed) {
            if (capture_ != Capture::None)
                capture_ = Capture::None;
            activate(hit);
        }
    }
}
void PadMapperEditor::wheel(double notches) {
    if (popup_ != Popup::None || page_ == Page::Overview)
        return;
    const int delta = int(std::lround(-notches * 2));
    if (pointer_x_ < sidebar_edge_)
        scroll_ = std::max(0, scroll_ + delta);
    else
        detail_scroll_ = std::clamp(detail_scroll_ + delta, 0, detail_max_);
    changed();
}
void PadMapperEditor::escape() {
    if (capture_ != Capture::None)
        capture_ = Capture::None;
    else if (sweep_ || center_capture_)
        cancel_calibration();
    else if (popup_ != Popup::None) {
        popup_ = Popup::None;
        close_requested_ = false;
    } else if (serialize_pad_mapper_profile(profile_) != original_) {
        popup_ = Popup::Discard;
        focus_ = 521;
    } else
        action_ = PadMapperEditorAction::Cancel;
    changed();
}
void PadMapperEditor::request_close() {
    suspend();
    popup_ = Popup::None;
    close_requested_ = true;
    escape();
}
void PadMapperEditor::key(int scancode, bool down) {
    if (!open_)
        return;
    if (!down)
        return;
    if (capture_ == Capture::Key) {
        if (scancode_name(scancode)) {
            button().guest_key = scancode;
            capture_ = Capture::None;
            validation_error_.clear();
        } else
            validation_error_ = "This keyboard key is not supported.";
    } else if (scancode == kScanEscape)
        escape();
    else if (scancode == kScanTab || scancode == kScanDown || scancode == kScanUp ||
             scancode == kScanLeft || scancode == kScanRight) {
        if (widget_ids_.empty())
            return;
        auto it = std::find_if(widget_ids_.begin(), widget_ids_.end(),
                               [&](const auto &item) { return item.second == focus_; });
        int index = it == widget_ids_.end() ? -1 : int(it - widget_ids_.begin());
        const int direction = scancode == kScanUp || scancode == kScanLeft ? -1 : 1;
        index = (index + direction + int(widget_ids_.size())) % int(widget_ids_.size());
        focus_ = widget_ids_[size_t(index)].second;
    } else if (scancode == kScanReturn || scancode == kScanSpace) {
        if (std::any_of(widget_ids_.begin(), widget_ids_.end(),
                        [&](const auto &item) { return item.second == focus_; })) {
            capture_ = Capture::None;
            activate(focus_);
        }
    }
    changed();
}
PadMapperEditorAction PadMapperEditor::take_action() {
    const auto action = action_;
    action_ = PadMapperEditorAction::None;
    if (action == PadMapperEditorAction::Apply) {
        if (!validate_pad_mapper_profile(profile_, &validation_error_)) {
            changed();
            return PadMapperEditorAction::None;
        }
    }
    return action == PadMapperEditorAction::Cancel && close_requested_
               ? PadMapperEditorAction::CloseWindow
               : action;
}
} // namespace controls
