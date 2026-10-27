#pragma once
#include "launcher_canvas.h"
#include "launcher_ui.h"

#include <functional>
#include <string>
#include <vector>

namespace launcher {

struct ConfigureSetting {
    std::string id;
    std::string label;
    std::string description;
    std::vector<std::string> values;
    int value = 0;
    int recommended = 0;
    bool wrap = true;
};

struct ConfigureActions {
    std::function<bool(std::vector<ConfigureSetting> *, std::string *)> read;
    std::function<bool(const std::string &, int, std::string *)> set;
    std::function<bool(std::string *)> restore_recommended;
    std::function<bool(std::string *)> test_rumble;
};

class ConfigureUi {
  public:
    ConfigureUi(std::string game, ConfigureActions actions);
    bool ready() const;
    bool done() const;
    bool dirty() const;
    bool take_controller_request();
    void key(Key key);
    void pointer(int x, int y, bool pressed);
    void draw(Canvas &canvas, int scale);

  private:
    void change(int delta);
    void action(int id);
    std::string game_;
    ConfigureActions actions_;
    std::vector<ConfigureSetting> settings_;
    std::vector<Rect> rows_;
    std::vector<Rect> previous_;
    std::vector<Rect> buttons_;
    std::string message_;
    int focus_ = 0;
    int pressed_ = -1;
    bool ready_ = false;
    bool dirty_ = true;
    bool done_ = false;
    bool controller_requested_ = false;
};

} // namespace launcher
