#pragma once
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct RcFrameInfo {
    uint8_t viewport, capture, native;
    uint32_t readiness;
    uint64_t frame_id;
} RcFrameInfo;
typedef struct RcPacing RcPacing;
typedef struct RcPacingSnapshot {
    double new_fps, display_fps, median_ms, p95_ms, gpu_ms, age_ms, repeat_percent;
    uint64_t new_frames, repeats, drops;
} RcPacingSnapshot;
typedef struct RcMailbox RcMailbox;
typedef struct RcFrameState RcFrameState;
typedef struct RcFrameSnapshot {
    double sealed_ts, submitted_ts, gpu_ts, presented_ts, acknowledgement_deadline, gpu_ms;
    uint32_t pending_prefixes;
    uint8_t gpu, shown, released, dropped, success, repeat, completion_fallback, prefix_done;
} RcFrameSnapshot;
RcFrameState *rc_frame_state_new(void);
void rc_frame_state_free(RcFrameState *state);
RcFrameSnapshot rc_frame_snapshot(const RcFrameState *state);
void rc_frame_seal(const RcFrameState *state, double ts, uint8_t prefix);
void rc_frame_repeated(const RcFrameState *state, double sealed);
uint8_t rc_frame_release(const RcFrameState *state);
typedef struct RcFrameCompletion {
    double ts, period, gpu_ms;
    uint8_t success, offscreen, stopped, completion_only;
} RcFrameCompletion;
uint8_t rc_frame_command(const RcFrameState *state, RcFrameCompletion completion);
uint8_t rc_frame_presented(const RcFrameState *state, double ts, uint8_t fake);
void rc_frame_prefix_begin(const RcFrameState *state);
void rc_frame_prefix_complete(const RcFrameState *state, uint8_t success);
void rc_frame_synthetic_prefix_complete(const RcFrameState *state);
typedef struct RcFrameLease RcFrameLease;
struct RcFrameLease {
    void *frame;
    RcFrameInfo (*inspect)(void *frame, uint8_t readiness);
    void (*release)(void *frame);
    RcFrameState *state;
    RcFrameLease (*repeat)(void *frame);
};
typedef struct RcTimeline RcTimeline;
typedef struct RcTimelineCounters {
    uint64_t unique, repeats, drops, pending_commands;
} RcTimelineCounters;
typedef struct RcRetirement {
    void *frame;
    uint8_t publish;
} RcRetirement;
RcTimelineCounters rc_timeline_counters(const RcTimeline *timeline);
size_t rc_timeline_len(const RcTimeline *timeline, uint8_t retiring);
void *rc_timeline_frame(const RcTimeline *timeline, uint8_t retiring, size_t index);
void rc_timeline_defer(RcTimeline *timeline, RcFrameLease frame, uint8_t count);
void rc_timeline_mark_drop(RcTimeline *timeline, const RcFrameState *state);
void rc_timeline_committed(RcTimeline *timeline, const RcFrameState *state);
void rc_timeline_command_started(RcTimeline *timeline);
void rc_timeline_command_finished(RcTimeline *timeline);
RcRetirement rc_timeline_retire(RcTimeline *timeline, uint8_t stopped);
void *rc_timeline_cached(const RcTimeline *timeline);
void rc_timeline_clear_cached(RcTimeline *timeline);
double rc_timeline_deadline(const RcTimeline *timeline, double now);
typedef struct RcMailboxSelection {
    void *discarded[2];
    uint32_t readiness[2];
} RcMailboxSelection;
size_t rc_mailbox_len(const RcMailbox *mailbox);
void *rc_mailbox_frame(const RcMailbox *mailbox, size_t index);
void *rc_mailbox_enqueue(RcMailbox *mailbox, RcFrameLease frame, int32_t viewport);
typedef struct RcPresenterSurface RcPresenterSurface;
typedef struct RcPresented {
    void *context;
    void (*invoke)(void *context, double timestamp);
    void (*release)(void *context);
} RcPresented;
typedef struct RcSurfaceBackend {
    void *context;
    uint64_t (*create)(void *context, void *native, int32_t width, int32_t height);
    void (*destroy)(void *context, uint64_t chain);
    void (*resize)(void *context, uint64_t chain, int32_t width, int32_t height);
    uint64_t (*acquire)(void *context, uint64_t chain);
    void (*release)(void *context, uint64_t chain, uint64_t drawable);
    void (*present)(void *context, uint64_t chain, uint64_t command, uint64_t drawable,
                    double duration, RcPresented callback);
    double (*refresh)(void *context, uint64_t chain);
} RcSurfaceBackend;
typedef struct RcSurfaceTransition {
    uint8_t installed, changed;
} RcSurfaceTransition;
RcPresenterSurface *rc_presenter_surface_new(RcSurfaceBackend backend, uint8_t enabled);
void rc_presenter_surface_free(RcPresenterSurface *surface);
void rc_presenter_surface_install(const RcPresenterSurface *surface, void *native, int32_t width,
                                  int32_t height, int32_t viewport);
void rc_presenter_surface_resize(const RcPresenterSurface *surface, int32_t width, int32_t height);
uint8_t rc_presenter_surface_pending(const RcPresenterSurface *surface);
int32_t rc_presenter_surface_viewport(const RcPresenterSurface *surface);
uint8_t rc_presenter_surface_attached(const RcPresenterSurface *surface);
RcSurfaceTransition rc_presenter_surface_apply(const RcPresenterSurface *surface);
uint64_t rc_presenter_surface_acquire(const RcPresenterSurface *surface, uint8_t suspended);
double rc_presenter_surface_refresh(const RcPresenterSurface *surface, double fallback);
void rc_presenter_surface_adopt(const RcPresenterSurface *surface, uint64_t chain);
void rc_presenter_surface_close(const RcPresenterSurface *surface);
typedef struct RcPresenterDeliveries RcPresenterDeliveries;
typedef struct RcPresenterDelivery {
    void *context;
    void (*run)(void *context);
    void (*release)(void *context);
} RcPresenterDelivery;
RcPresenterDeliveries *rc_presenter_deliveries_new(void);
void rc_presenter_deliveries_free(RcPresenterDeliveries *deliveries);
uint8_t rc_presenter_deliveries_enqueue(const RcPresenterDeliveries *deliveries,
                                        RcPresenterDelivery delivery);
size_t rc_presenter_deliveries_pending(const RcPresenterDeliveries *deliveries);
void rc_presenter_deliveries_stop(const RcPresenterDeliveries *deliveries);
typedef struct RcPresenterWorker RcPresenterWorker;
typedef struct RcPresenterWorkerState {
    double now, period, acknowledgement;
    uint8_t ready, native_pending, poll_native;
} RcPresenterWorkerState;
typedef struct RcPresenterWorkerCallbacks {
    void *context;
    RcPresenterWorkerState (*state)(void *context);
    void (*tick)(void *context, double timestamp, uint8_t refresh);
    void (*release)(void *context);
} RcPresenterWorkerCallbacks;
RcPresenterWorker *rc_presenter_worker_start(RcPresenterWorkerCallbacks callbacks,
                                             uint8_t offscreen);
void rc_presenter_worker_notify(const RcPresenterWorker *worker);
void rc_presenter_worker_stop(const RcPresenterWorker *worker);
void rc_presenter_worker_free(RcPresenterWorker *worker);
RcPacing *rc_pacing_new(void);
void rc_pacing_free(RcPacing *pacing);
void rc_pacing_displayed(RcPacing *pacing, double ts, uint8_t repeat, double gpu, double age);
RcPacingSnapshot rc_pacing_snapshot(const RcPacing *pacing, double now, uint64_t drops,
                                    double *intervals, size_t *count);
typedef struct RcPresenter RcPresenter;
typedef struct RcTurnInput {
    double ts, period;
    size_t flight_limit, capture_backlog;
    int32_t viewport;
    uint8_t offscreen, fake, duration_pacing, minimum_duration, surface_pending;
} RcTurnInput;
enum RcPresenterFault {
    RC_FAULT_POOL_STALLED = 1,
    RC_FAULT_DISPLAY_STALLED = 2,
    RC_FAULT_DISPLAY_FALLBACK = 4,
    RC_FAULT_NO_DRAWABLE = 8,
    RC_FAULT_GPU_TIMEOUT = 16
};
enum RcTurnFailure { RC_TURN_ENCODING_FAILED = 0, RC_TURN_NO_DRAWABLE = 1 };
typedef struct RcTurnSelection {
    void *frame;
    void *discarded[2];
    uint32_t readiness[2];
    void *fallback[3];
    uint32_t faults;
} RcTurnSelection;
RcPresenter *rc_presenter_new(void);
void rc_presenter_free(RcPresenter *p);
void rc_presenter_shutdown(RcPresenter *p, uint8_t fake);
RcMailbox *rc_presenter_mailbox(RcPresenter *p);
RcTimeline *rc_presenter_timeline(RcPresenter *p);
void rc_presenter_signal(RcPresenter *p, double ts, uint8_t pending);
uint8_t rc_presenter_ready(const RcPresenter *p, RcTurnInput input);
uint8_t rc_presenter_native_pending(const RcPresenter *p);
uint8_t rc_presenter_poll_native(const RcPresenter *p, RcTurnInput input);
RcTurnSelection rc_presenter_check(RcPresenter *p, RcTurnInput input);
RcTurnSelection rc_presenter_admit(RcPresenter *p, RcTurnInput input);

typedef struct RcTurnEncoding {
    int32_t width, height, drawable_width, drawable_height, x, y;
    uint32_t format, drawable_format;
    int32_t previous_width, previous_height;
    uint32_t previous_format;
    uint8_t previous, repeat, placed;
} RcTurnEncoding;
enum RcTurnOperation {
    RC_TURN_COMPOSE,
    RC_TURN_COPY,
    RC_TURN_PLACE,
    RC_TURN_FIT,
    RC_TURN_OVERLAYS
};
typedef struct RcTurnCallbacks {
    void *context;
    void (*transition)(void *, RcSurfaceTransition);
    void *(*select)(void *, double);
    uint8_t (*prefix)(void *, void *);
    void (*prepare)(void *, void *);
    RcTurnEncoding (*describe)(void *, void *, uint64_t);
    uint64_t (*begin)(void *, void *, uint8_t);
    void (*encode)(void *, void *, uint64_t, uint32_t);
    void (*commit)(void *, void *, uint64_t, uint64_t);
    void (*complete)(void *, void *, double, uint8_t);
    void (*failed)(void *, void *, uint8_t);
    void (*release)(void *);
} RcTurnCallbacks;
void rc_presenter_run(RcTurnCallbacks callbacks, const RcPresenterSurface *surface, double ts,
                      uint8_t fake, uint8_t automatic, uint8_t offscreen, uint8_t suspended);

typedef struct RcRejected {
    void *frame;
    uint8_t release;
} RcRejected;
RcRejected rc_presenter_reject(RcPresenter *p, RcFrameLease frame, uint8_t retry);
RcMailboxSelection rc_presenter_transition(RcPresenter *p, int32_t viewport, uint8_t changed);

typedef struct RcSubmission {
    void *context;
    uint64_t command, drawable;
    double period;
    uint8_t duration_pacing;
    void (*arm)(void *);
    void (*commit)(void *);
    RcPresented presented;
} RcSubmission;
void rc_presenter_submit(const RcPresenterSurface *surface, RcSubmission submission);

#ifdef __cplusplus
}
#endif
