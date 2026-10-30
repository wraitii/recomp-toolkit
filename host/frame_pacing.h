#pragma once
#include "../graphics/compositor/include/presenter.h"
#include <cstdint>
#include <vector>

struct FramePacingSnapshot {
    double new_fps = 0, display_fps = 0, median_ms = 0, p95_ms = 0, gpu_ms = 0, age_ms = 0;
    double repeat_percent = 0;
    uint64_t new_frames = 0, repeats = 0, drops = 0;
    std::vector<double> intervals_ms;
};
class FramePacing {
    RcPacing *pacing = rc_pacing_new();

  public:
    ~FramePacing() {
        rc_pacing_free(pacing);
    }
    FramePacing(const FramePacing &) = delete;
    FramePacing &operator=(const FramePacing &) = delete;
    FramePacing() = default;
    void displayed(double ts, bool repeat, double gpu_ms, double age_ms) {
        rc_pacing_displayed(pacing, ts, repeat, gpu_ms, age_ms);
    }
    FramePacingSnapshot snapshot(double now, uint64_t drops) const {
        double values[120];
        size_t count = 120;
        const auto p = rc_pacing_snapshot(pacing, now, drops, values, &count);
        FramePacingSnapshot s;
        s.new_fps = p.new_fps;
        s.display_fps = p.display_fps;
        s.median_ms = p.median_ms;
        s.p95_ms = p.p95_ms;
        s.gpu_ms = p.gpu_ms;
        s.age_ms = p.age_ms;
        s.repeat_percent = p.repeat_percent;
        s.new_frames = p.new_frames;
        s.repeats = p.repeats;
        s.drops = p.drops;
        s.intervals_ms.assign(values, values + count);
        return s;
    }
};
