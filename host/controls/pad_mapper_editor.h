#pragma once

#include "overlay.h"
#include "pad_mapper.h"

#include <string>
#include <utility>
#include <vector>

namespace controls {

enum class PadMapperEditorAction { None, Apply, Cancel, CloseWindow, TestRumble };

class PadMapperEditor {
  public:
    void open(const PadMapperProfile &profile, const Screen &screen);
    void close();
    void set_screen(const Screen &screen);
    void set_device(const std::string &name, bool connected);
    void set_message(const std::string &message);
    void suspend();
    void observe_input(const PadState &raw);
    bool is_open() const;
    const PadMapperProfile &profile() const;
    ControlsView view(const Screen &screen, const PadState &raw, const PadState &calibrated,
                      const PadState &guest_preview, const std::string &status);
    void pointer(double px, double py, int state);
    void wheel(double notches);
    void escape();
    void request_close();
    void key(int scancode, bool down);
    PadMapperEditorAction take_action();

  private:
    enum class Page { Overview, Mapping, Calibration };
    enum class Popup { None, Axis, Button, NegativeButton, PositiveButton, Preset, Discard };
    enum class Capture { None, Identify };
    void changed();
    void activate(int id);
    void select(int source);
    void cancel_calibration();
    void preset(int index);
    void add_key(int x, int y, int w, int h, const std::string &label, bool lit, int id);
    void add_text(int x, int y, int w, int h, const std::string &text, bool muted = true,
                  int scale = 1);
    void add_meter(int x, int y, int w, int h, const std::string &label, float value, bool bipolar);
    ButtonBinding &button();
    PadMapperProfile profile_;
    std::string original_;
    Screen screen_;
    Page page_ = Page::Overview;
    Popup popup_ = Popup::None;
    Capture capture_ = Capture::None;
    PadMapperEditorAction action_ = PadMapperEditorAction::None;
    std::string validation_error_;
    std::string device_;
    bool connected_ = false;
    bool open_ = false;
    bool close_requested_ = false;
    bool advanced_ = false;
    bool sweep_ = false;
    bool center_capture_ = false;
    double center_sum_ = 0;
    unsigned center_samples_ = 0;
    AxisCalibration before_capture_;
    float sweep_min_ = 0, sweep_max_ = 1;
    int selected_ = 0;
    int group_ = 0;
    int scroll_ = 0;
    int visible_rows_ = 1;
    int detail_scroll_ = 0;
    int detail_max_ = 0;
    int focus_ = 0;
    int pressed_ = -1;
    int preset_pending_ = -1;
    double pointer_x_ = 0;
    int sidebar_edge_ = 0;
    PadState identify_start_;
    PadState last_raw_;
    uint64_t revision_ = 1;
    ControlsView frame_;
    std::vector<std::pair<Rect, int>> widget_ids_;
};

} // namespace controls
