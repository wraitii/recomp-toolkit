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
constexpr int kPreviousPage = 3;
constexpr int kNextPage = 4;
constexpr int kButtonCount = 5;
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
    } else if (id == kPreviousPage || id == kNextPage) {
        const int direction = id == kPreviousPage ? -1 : 1;
        const int row = first_row_ + direction * page_rows_;
        if (row >= 0 && row < int(settings_.size()))
            focus_ = first_row_ = row;
    } else if (id == kController) {
        controller_requested_ = true;
    } else if (id == kDone) {
        done_ = true;
    }
    dirty_ = true;
}

void ConfigureUi::key(Key key) {
    if (done_)
        return;
    const int count = int(settings_.size()) + kPreviousPage;
    if (focus_ >= count)
        focus_ = first_row_;
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

void ConfigureUi::draw(Canvas &canvas, int scale, Rect bounds) {
    scale = std::max(1, scale);
    if (bounds.w <= 0 || bounds.h <= 0)
        bounds = {0, 0, canvas.width(), canvas.height()};
    dirty_ = false;
    const int margin = 14 * scale;
    const int left = bounds.x + margin;
    const int width = bounds.w - 2 * margin;
    const int title_size = 2 * scale;
    canvas.clear(kBackground);
    const std::string title = "Configure " + game_;
    const int title_chars = std::max(1, width / (6 * title_size));
    canvas.text(left, bounds.y + margin, title.substr(0, title_chars), title_size, kText);
    int y = bounds.y + margin + Canvas::text_height(title_size) + 10 * scale;

    rows_.assign(settings_.size(), {});
    previous_.assign(settings_.size(), {});
    const int row_h = 41 * scale;
    const int button_h = 28 * scale;
    const int button_gap = 6 * scale;
    const int button_y = bounds.y + bounds.h - margin - button_h;
    const int page_y = button_y - button_h - button_gap;
    const int status_y = page_y - 13 * scale;
    page_rows_ = std::max(1, (status_y - y) / row_h);
    if (focus_ < int(settings_.size()))
        first_row_ = focus_ / page_rows_ * page_rows_;
    first_row_ =
        std::min(first_row_, std::max(0, (int(settings_.size()) - 1) / page_rows_) * page_rows_);
    const int end = std::min(first_row_ + page_rows_, int(settings_.size()));
    const int value_w = std::min(122 * scale, width / 2);
    for (int i = first_row_; i < end; ++i) {
        const Rect row{left, y, width, row_h - 4 * scale};
        rows_[i] = row;
        const bool focused = int(i) == focus_;
        canvas.fill(row, focused ? kButtonHot : kPanel);
        canvas.frame(row, scale, focused ? kFocus : kLine);
        const int value_x = row.x + row.w - value_w;
        const ConfigureSetting &setting = settings_[i];
        const int description_w = std::max(1, value_x - row.x - 14 * scale);
        const auto label = Canvas::wrap(setting.label, description_w, scale);
        const int label_lines = std::min<int>(2, label.size());
        for (int line = 0; line < label_lines; ++line)
            canvas.text(row.x + 7 * scale, row.y + (5 + 10 * line) * scale, label[line], scale,
                        kText);
        if (label_lines == 1) {
            const auto description = Canvas::wrap(setting.description, description_w, scale);
            for (int line = 0; line < std::min<int>(2, description.size()); ++line)
                canvas.text(row.x + 7 * scale, row.y + (15 + 10 * line) * scale, description[line],
                            scale, kDim);
        }
        canvas.fill({value_x, row.y + scale, value_w - scale, row.h - 2 * scale}, kBackground);
        canvas.fill({value_x, row.y, scale, row.h}, focused ? kFocus : kLine);
        const int arrow_w = std::min(24 * scale, value_w / 3);
        const Rect prev{value_x, row.y, arrow_w, row.h};
        const Rect next{row.x + row.w - arrow_w, row.y, arrow_w, row.h};
        previous_[i] = prev;
        canvas.text(prev.x + 6 * scale, row.y + (row.h - Canvas::text_height(scale)) / 2, "<",
                    scale, kFocus);
        const std::string value = setting.value >= 0 && setting.value < int(setting.values.size())
                                      ? setting.values[size_t(setting.value)]
                                      : "?";
        const int value_chars = std::max(1, (value_w - 2 * arrow_w) / (6 * scale));
        const std::string clipped = value.substr(0, value_chars);
        const int tw = Canvas::text_width(clipped, scale);
        canvas.text(value_x + (value_w - tw) / 2, row.y + (row.h - Canvas::text_height(scale)) / 2,
                    clipped, scale, kText);
        canvas.text(next.x + 6 * scale, row.y + (row.h - Canvas::text_height(scale)) / 2, ">",
                    scale, kFocus);
        y += row_h;
    }

    const int button_w = (width - 2 * button_gap) / 3;
    const char *labels[] = {"Restore defaults", "Done", "Controller setup", "Previous", "Next"};
    buttons_.assign(kButtonCount, {});
    for (int i = 0; i < kButtonCount; ++i) {
        if ((i == kPreviousPage && first_row_ == 0) ||
            (i == kNextPage && end == int(settings_.size())))
            continue;
        const int column = i == kPreviousPage ? 0 : i == kNextPage ? 2 : i;
        const int x = left + column * (button_w + button_gap);
        const Rect rect{x, i < kPreviousPage ? button_y : page_y, button_w, button_h};
        buttons_[i] = rect;
        const int index = int(settings_.size()) + i;
        const bool focused = focus_ == index;
        const bool hot = focused || pressed_ == index;
        canvas.fill(rect,
                    i == kDone ? (hot ? kPrimaryHot : kPrimary) : (hot ? kButtonHot : kButton));
        canvas.frame(rect, scale, focused ? kFocus : i == kDone ? kPrimaryLine : kLine);
        const auto lines = Canvas::wrap(labels[i], button_w - 4 * scale, scale);
        for (int line = 0; line < std::min<int>(2, lines.size()); ++line) {
            const auto &label = lines[line];
            const int tx = x + (button_w - Canvas::text_width(label, scale)) / 2;
            const int ty =
                rect.y + (button_h - 10 * scale * int(lines.size())) / 2 + 10 * scale * line;
            canvas.text(tx, ty, label, scale, kText);
        }
    }
    if (page_rows_ < int(settings_.size())) {
        const std::string page =
            std::to_string(first_row_ / page_rows_ + 1) + " / " +
            std::to_string((int(settings_.size()) + page_rows_ - 1) / page_rows_);
        canvas.text(left + (width - Canvas::text_width(page, scale)) / 2,
                    page_y + (button_h - Canvas::text_height(scale)) / 2, page, scale, kDim);
    }
    const Color status_color = message_.find("Could not") == 0 || !ready_ ? kError : kDim;
    if (!message_.empty())
        canvas.text(left, status_y, message_.substr(0, std::max(1, width / (6 * scale))), scale,
                    status_color);
}

} // namespace launcher
