#pragma once
#include "present_frame.h"
#include "../graphics/compositor/include/presenter.h"
#include <utility>

struct PresentFrameLease {
    using Lease = std::shared_ptr<Frame>;

    static RcFrameInfo inspect(void *opaque, uint8_t readiness) noexcept {
        const auto &frame = *static_cast<Lease *>(opaque);
        const auto &pending = frame->pending_native;
        return {uint8_t(frame->viewport), uint8_t(bool(frame->capture)), uint8_t(bool(pending)),
                pending && readiness ? pending->ready(pending->lease.get()) : 1u, frame->frame_id};
    }
    static void release(void *opaque) noexcept {
        delete static_cast<Lease *>(opaque);
    }
    static RcFrameLease retain(Lease frame) {
        auto state = frame->state;
        return {new Lease(std::move(frame)), inspect, release, state, repeated};
    }
    static Lease take(void *opaque) {
        if (!opaque)
            return {};
        auto frame = std::move(*static_cast<Lease *>(opaque));
        release(opaque);
        return frame;
    }

    static Lease borrow(void *opaque) {
        return opaque ? *static_cast<Lease *>(opaque) : Lease{};
    }
    static RcFrameLease repeated(void *opaque) noexcept {
        const auto &source = *static_cast<Lease *>(opaque);
        const auto status = source->status();
        auto frame = std::make_shared<Frame>();
        rc_frame_repeated(frame->state, status.sealed_ts);
        frame->frame_id = source->frame_id;
        frame->cls = source->cls;
        frame->epoch = source->epoch;
        frame->input = source->input;
        frame->ui = source->ui;
        frame->input.ui = &frame->ui;
        frame->target =
            status.repeat || source->input.legacy ? source->target : source->scene_lease;
        frame->viewport = source->viewport;
        frame->viewport_style = source->viewport_style;
        frame->viewport_x = source->viewport_x;
        frame->viewport_y = source->viewport_y;
        frame->layout = source->layout;
        frame->composed = source->composed;
        frame->settings_page = source->settings_page;
        return retain(std::move(frame));
    }
};

class PresentMailbox : private PresentFrameLease {
    RcMailbox *mailbox;

  public:
    explicit PresentMailbox(RcMailbox *value) : mailbox(value) {}
    PresentMailbox(const PresentMailbox &) = delete;
    PresentMailbox &operator=(const PresentMailbox &) = delete;
    size_t size() const {
        return rc_mailbox_len(mailbox);
    }
    bool empty() const {
        return size() == 0;
    }
    template <class F> void each(F &&f) const {
        for (size_t i = 0; i < size(); ++i)
            f(*static_cast<Lease *>(rc_mailbox_frame(mailbox, i)));
    }
    Lease enqueue(Lease frame, int viewport) {
        return take(rc_mailbox_enqueue(mailbox, retain(std::move(frame)), viewport));
    }
};

class PresentTimeline : private PresentFrameLease {
    RcTimeline *timeline;

  public:
    explicit PresentTimeline(RcTimeline *value) : timeline(value) {}
    PresentTimeline(const PresentTimeline &) = delete;
    PresentTimeline &operator=(const PresentTimeline &) = delete;
    RcTimelineCounters counters() const {
        return rc_timeline_counters(timeline);
    }
    size_t size(bool retiring = false) const {
        return rc_timeline_len(timeline, retiring);
    }
    bool empty() const {
        return size() == 0 && size(true) == 0;
    }
    Lease frame(size_t index, bool retiring = false) const {
        return borrow(rc_timeline_frame(timeline, retiring, index));
    }
    template <class F> void each(F &&f, bool retiring = false) const {
        for (size_t i = 0; i < size(retiring); ++i)
            f(frame(i, retiring));
    }
    void defer(Lease frame, bool count = true) {
        rc_timeline_defer(timeline, retain(std::move(frame)), count);
    }
    void mark_drop(const Lease &frame) {
        rc_timeline_mark_drop(timeline, frame->state);
    }
    void committed(const Lease &frame) {
        rc_timeline_committed(timeline, frame->state);
    }
    void command_started() {
        rc_timeline_command_started(timeline);
    }
    void command_finished() {
        rc_timeline_command_finished(timeline);
    }
    std::pair<Lease, bool> retire(bool stopped) {
        const auto result = rc_timeline_retire(timeline, stopped);
        return {take(result.frame), bool(result.publish)};
    }
    Lease cached() const {
        return borrow(rc_timeline_cached(timeline));
    }
    void clear_cached() {
        rc_timeline_clear_cached(timeline);
    }
    double deadline(double now) const {
        return rc_timeline_deadline(timeline, now);
    }
};

struct PresentOwner {
    RcPresenter *value = rc_presenter_new();
    ~PresentOwner() {
        rc_presenter_free(value);
    }
    PresentOwner() = default;
    PresentOwner(const PresentOwner &) = delete;
    PresentOwner &operator=(const PresentOwner &) = delete;
};
