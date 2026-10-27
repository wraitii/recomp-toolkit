#include "configure_ui.h"

#include <algorithm>

namespace launcher {

namespace {
const Color kBackground{11, 21, 48, 255};
const Color kPanel{20, 37, 68, 255};
const Color kLine{48, 73, 108, 255};
const Color kText{237, 249, 255, 255};
const Color kDim{173, 190, 214, 255};
const Color kButton{24, 46, 80, 255};
const Color kButtonHot{32, 61, 99, 255};
const Color kPrimary{21, 85, 195, 255};
const Color kPrimaryHot{28, 100, 216, 255};
const Color kPrimaryLine{99, 159, 233, 255};
const Color kFocus{140, 232, 255, 255};
const Color kError{242, 170, 162, 255};
constexpr int kRestore = 0;
constexpr int kDone = 1;
constexpr int kController = 2;
constexpr int kRumble = 3;
} // namespace

ConfigureUi::ConfigureUi(std::string game, ConfigureActions actions)
    : game_(std::move(game)), actions_(std::move(actions)) {
    if (!actions_.read) {
        message_ = "Settings are unavailable.";
        return;
    }
    ready_ = actions_.read(&settings_, &message_);
    if (ready_)
        message_.clear();
    dirty_ = true;
}

bool ConfigureUi::ready() const {
    return ready_;
}

bool ConfigureUi::done() const {
    return done_;
}

bool ConfigureUi::dirty() const {
    return dirty_;
}

bool ConfigureUi::take_controller_request() {
    const bool requested = controller_requested_;
    controller_requested_ = false;
    return requested;
}

void ConfigureUi::change(int delta) {
    if (focus_ < 0 || focus_ >= int(settings_.size()))
        return;
    ConfigureSetting &setting = settings_[size_t(focus_)];
    if (setting.values.empty())
        return;
    const int count = int(setting.values.size());
    const int value = setting.wrap ? (setting.value + delta % count + count) % count
                                   : std::clamp(setting.value + delta, 0, count - 1);
    if (value == setting.value)
        return;
    std::string error;
    if (!actions_.set || !actions_.set(setting.id, value, &error)) {
        message_ = error.empty() ? "Could not change " + setting.label + "." : error;
    } else {
        setting.value = value;
        message_.clear();
    }
    dirty_ = true;
}

void ConfigureUi::action(int id) {
    std::string error;
    if (id == kRestore) {
        if (!actions_.restore_recommended || !actions_.restore_recommended(&error)) {
            message_ = error.empty() ? "Could not restore the recommended settings." : error;
        } else if (!actions_.read(&settings_, &error)) {
            message_ = error.empty() ? "Could not read the restored settings." : error;
        } else {
            message_ = "Recommended settings restored.";
        }
    } else if (id == kController) {
        controller_requested_ = true;
    } else if (id == kRumble) {
        if (!actions_.test_rumble || !actions_.test_rumble(&error))
            message_ = error.empty() ? "Could not test controller rumble." : error;
        else
            message_ = "Rumble command sent for one second. Did you feel it?";
    } else if (id == kDone) {
        done_ = true;
    }
    dirty_ = true;
}

void ConfigureUi::key(Key key) {
    if (done_)
        return;
    const int count = int(settings_.size()) + 4;
    if (count == 0)
        return;
    switch (key) {
    case Key::Up:
    case Key::Previous:
        focus_ = (focus_ + count - 1) % count;
        dirty_ = true;
        break;
    case Key::Down:
    case Key::Next:
        focus_ = (focus_ + 1) % count;
        dirty_ = true;
        break;
    case Key::Left:
        if (focus_ < int(settings_.size()))
            change(-1);
        else {
            focus_ = (focus_ + count - 1) % count;
            dirty_ = true;
        }
        break;
    case Key::Right:
        if (focus_ < int(settings_.size()))
            change(1);
        else {
            focus_ = (focus_ + 1) % count;
            dirty_ = true;
        }
        break;
    case Key::Activate:
        if (focus_ < int(settings_.size()))
            change(1);
        else
            action(focus_ - int(settings_.size()));
        break;
    case Key::Back:
        action(kDone);
        break;
    }
}

void ConfigureUi::pointer(int x, int y, bool pressed) {
    if (done_)
        return;
    const auto hit = [&](const Rect &r) { return r.contains(x, y); };
    for (size_t i = 0; i < settings_.size(); ++i) {
        if (i >= rows_.size())
            break;
        if (!rows_[i].contains(x, y))
            continue;
        focus_ = int(i);
        if (pressed) {
            pressed_ = int(i);
            dirty_ = true;
            return;
        }
        if (pressed_ == int(i)) {
            if (previous_[i].contains(x, y))
                change(-1);
            else
                change(1);
        }
        pressed_ = -1;
        dirty_ = true;
        return;
    }
    for (int i = 0; i < int(buttons_.size()); ++i) {
        if (!hit(buttons_[size_t(i)]))
            continue;
        focus_ = int(settings_.size()) + i;
        if (pressed) {
            pressed_ = int(settings_.size()) + i;
            dirty_ = true;
        } else {
            const bool activate_button = pressed_ == int(settings_.size()) + i;
            pressed_ = -1;
            if (activate_button)
                action(i);
        }
        return;
    }
    if (!pressed)
        pressed_ = -1;
}

void ConfigureUi::draw(Canvas &canvas, int scale) {
    scale = std::max(1, scale);
    dirty_ = false;
    const int w = canvas.width(), h = canvas.height(), margin = 24 * scale;
    const int title_size = 2 * scale;
    canvas.clear(kBackground);
    std::string title = "Configure " + game_;
    const size_t title_chars = size_t(std::max(1, (w - 2 * margin) / (6 * title_size)));
    if (title.size() > title_chars)
        title = title.substr(0, title_chars > 3 ? title_chars - 3 : 1) + "...";
    canvas.text(margin, margin, title, title_size, kText);
    int y = margin + Canvas::text_height(title_size) + 8 * scale;
    canvas.text(margin, y, "Changes are saved automatically. Used next time you play.", scale,
                kDim);
    y += Canvas::text_height(scale) + 12 * scale;
    canvas.fill({margin, y, w - 2 * margin, scale}, kLine);
    y += 12 * scale;

    rows_.assign(settings_.size(), {});
    previous_.assign(settings_.size(), {});
    const int value_w = std::min(145 * scale, w / 3);
    const int row_h =
        std::max(34 * scale, (h - y - 72 * scale) / std::max(1, int(settings_.size())));
    for (size_t i = 0; i < settings_.size(); ++i) {
        const Rect row{margin, y, w - 2 * margin, row_h - 6 * scale};
        rows_[i] = row;
        const bool focused = int(i) == focus_;
        canvas.fill(row, focused ? kButtonHot : kPanel);
        canvas.frame(row, scale, focused ? kFocus : kLine);
        const int value_x = row.x + row.w - value_w;
        const ConfigureSetting &setting = settings_[i];
        canvas.text(row.x + 9 * scale, row.y + 6 * scale, setting.label, scale, kText);
        const int description_y = row.y + 18 * scale;
        const int description_w = std::max(1, value_x - row.x - 18 * scale);
        canvas.paragraph(row.x + 9 * scale, description_y, description_w, setting.description,
                         scale, kDim);
        const Rect value_area{value_x, row.y, value_w, row.h};
        canvas.fill({value_x, row.y + scale, value_w - scale, row.h - 2 * scale}, kBackground);
        canvas.fill({value_x, row.y, scale, row.h}, focused ? kFocus : kLine);
        const Rect prev{value_area.x, row.y, 20 * scale, row.h};
        const Rect next{row.x + row.w - 20 * scale, row.y, 20 * scale, row.h};
        previous_[i] = prev;
        canvas.text(prev.x + 6 * scale, row.y + (row.h - Canvas::text_height(scale)) / 2, "<",
                    scale, kFocus);
        const std::string value = setting.value >= 0 && setting.value < int(setting.values.size())
                                      ? setting.values[size_t(setting.value)]
                                      : "?";
        const std::string clipped =
            value.size() * size_t(6 * scale) > size_t(value_w - 48 * scale)
                ? value.substr(0, size_t(std::max(1, value_w / (6 * scale) - 8))) + "..."
                : value;
        const int tw = Canvas::text_width(clipped, scale);
        canvas.text(value_x + (value_w - tw) / 2, row.y + (row.h - Canvas::text_height(scale)) / 2,
                    clipped, scale, kText);
        canvas.text(next.x + 6 * scale, row.y + (row.h - Canvas::text_height(scale)) / 2, ">",
                    scale, kFocus);
        y += row_h;
    }

    const int button_h = 24 * scale;
    const int button_gap = 8 * scale;
    const int button_w = (w - 2 * margin - 3 * button_gap) / 4;
    const int button_y = std::max(y + 3 * scale, h - 40 * scale);
    const char *labels[] = {"Restore recommended", "Done", "Controller setup", "Test rumble"};
    buttons_.resize(4);
    for (int i = 0; i < 4; ++i) {
        const int x = margin + i * (button_w + button_gap);
        const Rect rect{x, button_y, button_w, button_h};
        buttons_[size_t(i)] = rect;
        const int index = int(settings_.size()) + i;
        const bool focused = focus_ == index;
        const bool hot = focused || pressed_ == index;
        canvas.fill(rect,
                    i == kDone ? (hot ? kPrimaryHot : kPrimary) : (hot ? kButtonHot : kButton));
        canvas.frame(rect, scale, focused ? kFocus : i == kDone ? kPrimaryLine : kLine);
        std::string label = labels[i];
        const int max_chars = std::max(1, button_w / (6 * scale));
        if (int(label.size()) > max_chars)
            label.resize(size_t(max_chars));
        const int tw = Canvas::text_width(label, scale);
        canvas.text(x + (button_w - tw) / 2, button_y + (button_h - Canvas::text_height(scale)) / 2,
                    label, scale, kText);
    }
    const int status_y =
        std::min(h - Canvas::text_height(scale) - 4 * scale, button_y + button_h + 5 * scale);
    const Color status_color = message_.find("Could not") == 0 || !ready_ ? kError : kDim;
    if (!message_.empty())
        canvas.text(margin, status_y,
                    message_.substr(0, size_t(std::max(1, (w - 2 * margin) / (6 * scale)))), scale,
                    status_color);
}

} // namespace launcher
