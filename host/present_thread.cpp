#include "page_overlay.h"
#include "../runtime/display_seam.h"
// Worker-owned swapchain and sealed-frame mailbox, over gpu.h. No window code here.
#include "present.h"
#include "present_frame.h"
#include "present_state.h"
#include "present_surface.h"
#include "present_test.h"
#include "performance_overlay.h"
#include "d3d_render.h"
#include "input_gate.h"
#include "../dx/passes.h"
#include "../dx/ddraw.h"
#include "../runtime/mods_seam.h"
#include "ui_layer.h"
#include <string.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <functional>
#include <memory>
#include <mutex>
#include <set>
#include "../platform/os.h"
#include "../platform/profile_markers.h"

// Packed into one atomic so a producer never combines dimensions from two
// renderer restarts. Already sealed frames retain their own target and layout.
static std::atomic<uint64_t> render_resolution{0};
extern "C" void host_set_render_resolution(int w, int h) {
    const uint64_t size = w > 0 && h > 0 && w <= 16384 && h <= 16384
                              ? (uint64_t(uint32_t(w)) << 32) | uint32_t(h)
                              : 0;
    render_resolution.store(size);
    fprintf(stderr, "presenter: render resolution %dx%d (%s)\n", int(size >> 32),
            int(uint32_t(size)), size ? "logical canvas retained" : "automatic");
}

// Minimal hosts can link the service without the app's present.cpp. Unknown
// mode keeps first-write acquisition on the service's last guest dimensions.
// App and smoke hosts override this with their current display mode.
extern "C" __attribute__((weak)) void host_present_mode(int *w, int *h, int *bpp) {
    if (w)
        *w = 0;
    if (h)
        *h = 0;
    if (bpp)
        *bpp = 0;
}

static std::atomic<bool> g_present_suspended{false};
static std::atomic<bool> g_present_bilinear{false};

void host_present_set_bilinear(bool enabled) {
    g_present_bilinear = enabled;
}
static std::mutex g_present_controls_mutex;
static controls::ControlsView g_present_controls; // wanted = false until the host publishes

namespace {
// The refresh rate of the offscreen presenter's synthetic display link.
constexpr double kOffscreenHz = 120.0;
using Clock = std::chrono::steady_clock;
template <class T> struct AtomicShared {
    std::shared_ptr<T> value;
    std::shared_ptr<T> load() const {
        return std::atomic_load(&value);
    }
    void store(std::shared_ptr<T> p) {
        std::atomic_store(&value, std::move(p));
    }
};
struct Metric {
    double origin = -1, latest = -1;
    // 10 warm-up seconds followed by the binding 30 second observation.
    std::array<uint64_t, 30> buckets{};
    void tick(double ts) {
        if (origin < 0)
            origin = ts;
        latest = std::max(latest, ts);
    }
    void complete(double ts) {
        if (origin < 0 || ts < origin)
            return;
        const int bucket = int(std::floor(ts - origin + 1e-7));
        if (bucket >= 10 && bucket < 40)
            ++buckets[bucket - 10];
    }
    int read(double *minimum, double *elapsed) const {
        const double span = origin < 0 ? 0 : latest - origin;
        if (elapsed)
            *elapsed = span;
        if (minimum)
            *minimum = span >= 40 ? *std::min_element(buckets.begin(), buckets.end()) : 0;
        return span >= 40;
    }
};
struct Service : std::enable_shared_from_this<Service> {
    HostCaptureFactory capture_factory = nullptr;
    std::mutex mutex;
    std::condition_variable completed;
    RcPresenterWorker *worker = nullptr;
    RcPresenterDeliveries *deliveries = nullptr;
    size_t capture_backlog() const {
        return rc_presenter_deliveries_pending(deliveries);
    }
    void notify_worker() const {
        rc_presenter_worker_notify(worker);
    }
    bool stop = false, fake = false, automatic = true, offscreen = false;
    bool duration_pacing = true;
    unsigned faults = 0, logged_faults = 0;
    double frame_period = 1.0 / 60, fake_now = 0, queued_flip = 0;
    void fault(unsigned bit, const char *reason) { // mutex held
        if (logged_faults & bit)
            return;
        logged_faults |= bit;
        ++faults;
        fprintf(stderr, "presenter: FAULT: %s (unique=%llu drops=%llu flight=%llu mailbox=%zu)\n",
                reason, (unsigned long long)timeline.counters().unique,
                (unsigned long long)timeline.counters().drops,
                (unsigned long long)(timeline.size() == 0 ? 0 : timeline.frame(0)->frame_id),
                mailbox.size());
    }
    RcTurnInput turn_input(double ts) const {
        return {ts,
                frame_period,
                flight_limit,
                capture_backlog(),
                surface.viewport(),
                uint8_t(offscreen),
                uint8_t(fake),
                uint8_t(duration_pacing),
                uint8_t(device && device->minimum_present_duration()),
                uint8_t(surface.pending())};
    }
    bool window_ready() const {
        return rc_presenter_ready(presenter.value, turn_input(now()));
    }
    void signal_tick(double ts) {
        std::lock_guard lock(mutex);
        rc_presenter_signal(presenter.value, ts, true);
        notify_worker();
    }
    std::atomic<int> drawable_w{640}, drawable_h{480};
    int guest_w = 640, guest_h = 480, requested_w = 0;
    // The game rectangle's size as last told to the input gate; zero until a
    // resize first publishes one.
    int gate_rect_w = 0, gate_rect_h = 0;
    // Mutex held. Where this frame is composed (present.h): the whole drawable
    // in landscape, the top of it in portrait. A portrait rectangle follows the
    // guest mode, so a mode change re-publishes its size to the input gate the
    // way a resize does; in landscape it never differs from what resize sent.
    // `placed` is set when the rectangle is not the whole drawable.
    HostGameRect game_rect(bool *placed = nullptr) {
        const int dw = drawable_w, dh = drawable_h;
        const HostGameRect r =
            host_present_game_rect(dw, dh, guest_w, guest_h, host_present_safe_top());
        if (placed)
            *placed = r.x != 0 || r.y != 0 || r.w != dw || r.h != dh;
        if (gate_rect_w > 0 && (r.w != gate_rect_w || r.h != gate_rect_h))
            publish_gate_size(r);
        published_rect.store(pack_rect(r), std::memory_order_relaxed);
        return r;
    }
    // The last rectangle game_rect() computed, packed into one word so the
    // window host reads it without the mutex (guest threads hold that across
    // GPU allocation) and can never see half of an update.
    std::atomic<uint64_t> published_rect{0};
    static uint64_t pack_rect(const HostGameRect &r) {
        const auto field = [](int v) { return uint64_t(std::clamp(v, 0, 0xffff)); };
        return field(r.x) << 48 | field(r.y) << 32 | field(r.w) << 16 | field(r.h);
    }
    static HostGameRect unpack_rect(uint64_t v) {
        return {int(v >> 48 & 0xffff), int(v >> 32 & 0xffff), int(v >> 16 & 0xffff),
                int(v & 0xffff)};
    }
    void publish_gate_size(const HostGameRect &r) { // mutex held
        gate_rect_w = r.w;
        gate_rect_h = r.h;
        host_gate_publish_drawable_size(r.w, r.h);
    }
    unsigned fail_allocations = 0;
    gpu::Device *device = nullptr;
    PresentSurface surface;
    uint64_t fake_texture_ids = 0x100000000ull; // fake mode: scene handles with no device
    double now() const {                        // mutex held
        return fake ? fake_now : device ? device->now_seconds() : 0.0;
    }
    // Up to three display submissions, two mailbox frames, a writer and history.
    // Offscreen and the single-flight control still use only four targets.
    std::array<std::shared_ptr<Target>, 7> targets;
    size_t flight_limit = 1;
    std::shared_ptr<Frame> writing;
    PresentOwner presenter;
    PresentMailbox mailbox{rc_presenter_mailbox(presenter.value)};
    PresentTimeline timeline{rc_presenter_timeline(presenter.value)};
    std::shared_ptr<Target> history_lease;
    CompositorSceneHistory history;
    uint64_t history_epoch = 0;
    std::vector<std::shared_ptr<Composite>> composite_pool;
#ifdef RECOMP_PRESENT_HAS_UI_LAYER
    UiFrame previous_ui{}; // guest thread only, including sealed frames that were dropped
    uint64_t previous_ui_epoch = 0;
#endif
    uint8_t last_pixel = 0;
    uint64_t waits = 0;
    uint64_t settings_pages = 0; // sealed frames that carried the settings page
    uint64_t epoch = 0;
    bool class_known = false;
    HostScreenClass last_class = HOST_SCREEN_MENU;
    Metric metric;
    FramePacing pacing;
    FILE *timings = nullptr;
    unsigned timing_rows = 0;
    FILE *ack_trace = nullptr;
    ~Service() {
        rc_presenter_worker_free(worker);
        rc_presenter_deliveries_free(deliveries);
        if (timings)
            fclose(timings);
        if (ack_trace)
            fclose(ack_trace);
    }
    void trace_ack(const Frame &f, const char *event, double ts) { // mutex held
        if (!ack_trace)
            return;
        const double now = this->now();
        const auto status = f.status();
        fprintf(ack_trace, "%llu,%d,%d,%s,%.9f,%.9f,%.9f,%.9f,%.9f,%d,%d,%d\n",
                (unsigned long long)f.frame_id, int(f.cls), int(status.repeat), event, now, ts,
                status.submitted_ts, status.gpu_ts, status.acknowledgement_deadline,
                int(status.shown), int(status.completion_fallback), int(status.released));
    }
    PerformanceOverlay performance_overlay;
    controls::Overlay controls_overlay;
    std::array<LayoutSnapshot, 3> layouts;
    unsigned layout_slot = 0;
    bool layout_valid = false;
    LayoutSnapshot sealed_layout;
    bool sealed_layout_valid = false;
    std::set<uint64_t> test_released;

    void release(const std::shared_ptr<Frame> &f) { // mutex held; never guest data access
        if (!rc_frame_release(f->state))
            return;
        if (!f->status().repeat) {
            if (fake)
                test_released.insert(f->frame_id);
            else
                ddraw_present_release({f->frame_id});
        }
        // The target history lease is separate from the retired frame arena.
        if (f->target)
            f->target->release_retired();
        f->target.reset();
        f->scene_lease.reset();
        completed.notify_all();
    }
    struct CaptureDelivery {
        gpu::Device *device;
        std::shared_ptr<Composite> texture;
        HostCompletedComposite result;
        HostFrameCapture capture;
        std::weak_ptr<Service> service;
    };
    void queue_capture(const std::shared_ptr<Frame> &frame) {
        if (!deliveries) {
            deliveries = rc_presenter_deliveries_new();
            if (!deliveries)
                abort();
        }
        auto delivery = std::make_unique<CaptureDelivery>();
        delivery->device = device;
        delivery->texture = frame->composed;
        delivery->capture = std::move(frame->capture);
        delivery->service = shared_from_this();
        auto &result = delivery->result;
        result.w = frame->composed->w;
        result.h = frame->composed->h;
        result.guest_w = frame->input.guest_w;
        result.guest_h = frame->input.guest_h;
        result.cls = frame->cls;
        result.frame_id = frame->frame_id;
        result.layout = frame->layout;
        RcPresenterDelivery task{};
        task.context = delivery.release();
        task.run = [](void *context) noexcept {
            auto &delivery = *static_cast<CaptureDelivery *>(context);
            auto texture = std::move(delivery.texture);
            auto &result = delivery.result;
            result.rgba.resize(size_t(result.w) * result.h * 4);
            const bool success = delivery.device->readback(
                texture->texture, {0, 0, result.w, result.h}, result.rgba.data(), result.w * 4);
            texture.reset();
            if (!success) {
                fprintf(stderr, "presenter: capture readback failed for frame %llu\n",
                        (unsigned long long)result.frame_id);
                return;
            }
            auto capture = std::move(delivery.capture);
            capture(result);
        };
        task.release = [](void *context) noexcept {
            std::unique_ptr<CaptureDelivery> delivery(static_cast<CaptureDelivery *>(context));
            if (auto service = delivery->service.lock())
                service->notify_worker();
        };
        rc_presenter_deliveries_enqueue(deliveries, task);
    }
    void sweep() {
        while (true) {
            auto [f, publish] = timeline.retire(stop);
            if (!f)
                break;
            if (publish) {
                const auto status = f->status();
                if (!offscreen &&
                    (!status.completion_fallback || (device && !device->presentation_timestamps())))
                    pacing.displayed(status.presented_ts, status.repeat, status.gpu_ms,
                                     (status.presented_ts - status.sealed_ts) * 1000);
                if (!status.repeat && !status.completion_fallback &&
                    f->cls == HOST_SCREEN_GAMEPLAY && f->epoch == epoch)
                    metric.complete(std::max(status.presented_ts, status.gpu_ts));
                if (timings) {
                    fprintf(timings, "%llu,%d,%d,%.9f,%.9f,%.9f,%.3f,%llu,%d,%d,%d\n",
                            (unsigned long long)f->frame_id, int(f->cls), int(status.repeat),
                            status.sealed_ts, status.submitted_ts, status.presented_ts,
                            status.gpu_ms, (unsigned long long)timeline.counters().drops,
                            int(!offscreen && !status.completion_fallback), f->input.drawable_w,
                            f->input.drawable_h);
                    // Keep diagnostics live without a synchronous write on
                    // every display callback while holding the queue mutex.
                    // fclose flushes the remaining rows during shutdown.
                    if (++timing_rows % 60 == 0)
                        fflush(timings);
                }
                if (fake && f->target && !f->target->test_pixels.empty())
                    last_pixel = f->target->test_pixels[0];
                layout_slot = (layout_slot + 1) % layouts.size();
                layouts[layout_slot] = f->layout;
                layout_valid = true;
                host_gate_publish_layout(f->layout);
                if (f->capture && f->composed && device)
                    queue_capture(f);
            }
            release(f);
            notify_worker();
        }
    }
    void drop(const std::shared_ptr<Frame> &f) {
        f->pending_native.reset();
        timeline.defer(f);
        sweep();
    }
    void ack_command(const std::shared_ptr<Frame> &f, bool success, double ts, double gpu_ms = 0) {
        std::lock_guard lock(mutex);
        const RcFrameCompletion completion{ts,
                                           frame_period,
                                           gpu_ms,
                                           uint8_t(success),
                                           uint8_t(offscreen),
                                           uint8_t(stop),
                                           uint8_t(device && !device->presentation_timestamps())};
        if (!rc_frame_command(f->state, completion))
            return;
        trace_ack(*f, "gpu", ts);
        sweep();
        completed.notify_all();
        notify_worker();
    }
    void ack_presented(const std::shared_ptr<Frame> &f, double ts) {
        std::lock_guard lock(mutex);
        trace_ack(*f, "presented", ts);
        if (rc_frame_presented(f->state, ts, fake))
            sweep();
    }
    void commit_present(std::shared_ptr<Frame> frame, gpu::Texture drawable,
                        gpu::CommandBuffer cb) {
        struct Submission {
            std::shared_ptr<Service> service;
            std::shared_ptr<Frame> frame;
            gpu::CommandBuffer command;
        } context{shared_from_this(), std::move(frame), cb};
        RcSubmission submission{};
        submission.context = &context;
        submission.command = cb.id;
        submission.drawable = drawable.id;
        {
            std::lock_guard lock(mutex);
            submission.period = frame_period;
        }
        submission.duration_pacing = duration_pacing;
        submission.arm = [](void *opaque) noexcept {
            const auto &context = *static_cast<Submission *>(opaque);
            auto self = context.service;
            auto f = context.frame;
            {
                std::lock_guard lock(self->mutex);
                self->timeline.command_started();
                self->timeline.committed(f);
            }
            self->device->on_complete(
                context.command, [self, f](gpu::CommandStatus status, double gpu_ms) {
                    double ts;
                    {
                        std::lock_guard lock(self->mutex);
                        ts = self->now();
                    }
                    self->ack_command(f, status == gpu::CommandStatus::Completed, ts,
                                      self->fake ? 0 : gpu_ms);
                    std::lock_guard lock(self->mutex);
                    self->timeline.command_finished();
                    self->completed.notify_all();
                });
        };
        submission.commit = [](void *opaque) noexcept {
            const auto &context = *static_cast<Submission *>(opaque);
            context.service->device->commit(context.command);
        };
        submission.presented = PresentSurface::callback(
            [weak = std::weak_ptr<Service>(context.service), f = context.frame](double ts) {
                if (auto service = weak.lock()) {
                    if (ts > 0)
                        service->ack_presented(f, ts);
                    else {
                        std::lock_guard lock(service->mutex);
                        service->trace_ack(*f, "presented", ts);
                    }
                }
            });
        rc_presenter_submit(surface.get(), submission);
    }
    void seal(uint64_t id, HostScreenClass cls, bool had_draws, bool prefix_pending = false) {
        std::lock_guard lock(mutex);
        if (stop || !writing)
            return;
        auto f = std::move(writing);
        f->frame_id = id;
        f->cls = cls;
        f->had_draws = had_draws;
        rc_frame_seal(f->state, now(), prefix_pending);
        if (class_known && last_class != cls) {
            ++epoch;
            metric = {};
        }
        class_known = true;
        last_class = cls;
        f->epoch = epoch;
        // This frame was produced with the old mode. Apply live rendering
        // changes only after taking its snapshot, for the next producer frame.
        f->input.classic = mods_display_classic() != 0;
        f->input.narrow = mods_display_wide() == 0;
        mods_display_transition(epoch, cls);
        // CPU-only Classic classes use the entire guest surface, including
        // menu backgrounds that Enhanced extracts as individual UI records.
        if (f->input.classic && cls != HOST_SCREEN_GAMEPLAY && f->staged_pixels) {
            f->input.legacy = true;
            f->input.legacy_frame = f->target->pixels;
            f->input.guest_w = guest_w;
            f->input.guest_h = guest_h;
            f->input.world = {};
            f->input.overlay = {};
            f->supplied = true;
        }
        f->input.scale_override = mods_display_scale();
        f->input.presentation_filter =
            g_present_bilinear.load() ? gpu::Filter::Linear : gpu::Filter::Nearest;
        if (f->status().dropped) {
            timeline.defer(f, false);
            sweep();
            return;
        }
        f->input.cls = cls;
        const HostGameRect rect = game_rect();
        f->input.drawable_w = rect.w;
        f->input.drawable_h = rect.h;
        if (f->input.world && f->input.guest_w > 0 && f->input.guest_h > 0) {
            const int domain =
                f->input.scene.domain_w > 0 ? f->input.scene.domain_w : f->input.guest_w;
            f->input.scene = {float(rect.w) / domain, float(rect.h) / f->input.guest_h, 0, 0,
                              domain};
        }
        if (!f->supplied) {
            // Explicit CPU-staged/legacy compatibility input. Incremental
            // gameplay and the UI extractor supply their layered inputs.
            f->input.legacy = true;
            f->input.legacy_frame = f->target->pixels;
            f->input.guest_w = guest_w;
            f->input.guest_h = guest_h;
        }
        f->input.ui = &f->ui;
        if (!f->target->scene.w || (!fake && !f->supplied && !f->target->pixels)) {
            drop(f);
            return;
        }
        auto discarded = mailbox.enqueue(f, surface.viewport());
        if (discarded)
            drop(discarded);
        if (discarded == f)
            return;
        sealed_layout = frame_layout(*f);
        sealed_layout_valid = true;
        notify_worker();
    }
    gpu::Texture texture(int w, int h, gpu::Format format = gpu::Format::RGBA8) {
        if (!device || w <= 0 || h <= 0)
            return {};
        return device->create_texture(
            {w, h, format, gpu::UsageSampled | gpu::UsageRenderTarget | gpu::UsageCpu, 1});
    }
    std::shared_ptr<Composite> composite(int w, int h, gpu::Format format) {
        gpu::Texture t = texture(w, h, format);
        if (!t)
            return nullptr;
        auto c = std::make_shared<Composite>();
        c->device = device;
        c->texture = t;
        c->w = w;
        c->h = h;
        c->format = format;
        return c;
    }
    std::shared_ptr<Composite> pooled_composite(int w, int h, gpu::Format format) {
        for (const auto &c : composite_pool)
            if (c.use_count() == 1 && c->w == w && c->h == h && c->format == format)
                return c;
        std::erase_if(composite_pool, [](const auto &c) { return c.use_count() == 1; });
        auto c = composite(w, h, format);
        if (c)
            composite_pool.push_back(c);
        return c;
    }
    // Apply backpressure before the first write. Dropping an unrendered frame
    // loses persistent surface updates (dirty HUD rectangles, cursor erasure),
    // even if later whole-scene frames can still be displayed. Completed frames
    // may be discarded by the mailbox after their surface contents are retained.
    HostSceneTarget acquire(int gw, int gh, int rw, int rh) {
        std::unique_lock lock(mutex);
        if (stop || gw <= 0 || gh <= 0)
            return {};
        if (writing)
            return writing->target ? writing->target->scene : HostSceneTarget{};
        guest_w = gw;
        guest_h = gh;
        const HostGameRect rect = game_rect();
        const uint64_t resolution = render_resolution.load();
        if (rw <= 0 && rh <= 0 && resolution) {
            rw = int(resolution >> 32);
            rh = int(uint32_t(resolution));
        }
        if (rw <= 0)
            rw = rect.w;
        if (rh <= 0)
            rh = rect.h;
        if (mods_display_classic()) {
            rw = gw;
            rh = gh;
        }
        requested_w = rw;
        auto free_slot = [&]() -> int {
            for (size_t i = 0; i < (flight_limit == 1 ? 4 : flight_limit + 4); ++i)
                if (!targets[i] || targets[i].use_count() == 1)
                    return int(i);
            return -1;
        };
        int slot = free_slot();
        if (slot < 0) {
            ++waits;
            notify_worker();
            // Keep a stalled GPU/worker from trapping the guest forever. This
            // is a fault deadline, not the normal frame pacing mechanism.
            completed.wait_for(lock, std::chrono::milliseconds(250), [&] {
                slot = free_slot();
                return stop || slot >= 0;
            });
            if (stop)
                return {};
        }
        if (slot < 0) {
            // Latch the drop until seal: later draws/CPU staging in this same
            // guest frame must neither retry acquisition nor count it twice.
            fault(RC_FAULT_POOL_STALLED,
                  "target pool stalled for 250ms; guest frame dropped, check "
                  "worker/GPU/presentation progress");
            writing = std::make_shared<Frame>();
            timeline.mark_drop(writing);
            return {};
        }
        int w = std::max(gw, rw), h = std::max(gh, rh);
        auto target = targets[slot];
        if (!target)
            target = std::make_shared<Target>();
        target->device = fake ? nullptr : device;
        const gpu::TextureDesc overlay_desc = !fake && target->scene.overlay
                                                  ? device->describe(target->scene.overlay)
                                                  : gpu::TextureDesc{};
        if (target->scene.w != w || target->scene.h != h ||
            (!fake && (overlay_desc.width != gw || overlay_desc.height != gh))) {
            target->release_scene();
            while (true) {
                bool failed = fail_allocations > 0;
                if (failed)
                    --fail_allocations;
                gpu::Texture world, overlay;
                if (!fake && !failed) {
                    world = texture(w, h, gpu::Format::BGRA8);
                    overlay = texture(gw, gh, gpu::Format::BGRA8);
                    failed = !world || !overlay;
                    if (failed) {
                        if (world)
                            device->destroy(world);
                        if (overlay)
                            device->destroy(overlay);
                    }
                }
                if (!failed) {
                    if (fake) {
                        world = {fake_texture_ids++};
                        overlay = {fake_texture_ids++};
                    }
                    target->scene = {world, overlay, w, h};
                    break;
                }
                if (w == gw && h == gh) {
                    fprintf(stderr,
                            "presenter: allocation failed at guest size %dx%d; frame target "
                            "unavailable\n",
                            gw, gh);
                    target->scene = {};
                    break; // reserve a failed writer too; its GPU prefixes still retire safely
                }
                w = std::max(gw, w / 2);
                h = std::max(gh, h / 2);
                fprintf(
                    stderr,
                    "presenter: scene allocation failed; retry %dx%d (requested %dx%d unchanged)\n",
                    w, h, rw, rh);
            }
        }
        targets[slot] = target;
        writing = std::make_shared<Frame>();
        writing->target = target;
        return target->scene;
    }
    LayoutSnapshot frame_layout(const Frame &f) const {
        LayoutSnapshot layout = compositor_layout_snapshot(&f.input);
        layout.frame_id = f.frame_id;
        layout.viewport = f.viewport;
        layout.viewport_style = f.viewport_style;
        layout.guest_origin_x = f.viewport_x;
        layout.guest_origin_y = f.viewport_y;
        return layout;
    }
    void publish_layout(Frame &f) {
        f.layout = frame_layout(f);
    }

    bool submit_native(const std::shared_ptr<Frame> &frame,
                       std::shared_ptr<PendingNativeTexture> pending) {
        {
            std::lock_guard lock(mutex);
            if (stop)
                return false;
        }
        auto self = shared_from_this();
        gpu::CommandBuffer cb;
        std::shared_ptr<std::atomic<bool>> finished;
        bool tracked = false;
        try {
            cb = device->begin();
            if (!cb)
                return false;
            device->blit(cb, pending->source, {0, 0, pending->w, pending->h}, pending->destination,
                         0, 0);
            finished = std::make_shared<std::atomic<bool>>(false);
            {
                std::lock_guard lock(mutex);
                rc_frame_prefix_begin(frame->state);
                timeline.command_started();
                tracked = true;
            }
            device->on_complete(
                cb, [self, frame, pending, finished](gpu::CommandStatus status, double) mutable {
                    pending.reset();
                    if (finished->exchange(true))
                        return;
                    std::lock_guard lock(self->mutex);
                    rc_frame_prefix_complete(frame->state, status == gpu::CommandStatus::Completed);
                    self->timeline.command_finished();
                    self->sweep();
                    self->completed.notify_all();
                    self->notify_worker();
                });
            device->commit(cb);
            return true;
        } catch (...) {
            fprintf(stderr, "presenter: deferred native texture copy failed\n");
            if (cb) {
                device->commit(cb);
                device->wait(cb);
            }
            if (tracked && !finished->exchange(true)) {
                std::lock_guard lock(mutex);
                rc_frame_prefix_complete(frame->state, false);
                timeline.command_finished();
            }
            return false;
        }
    }

    struct Turn {
        std::shared_ptr<Frame> frame;
        HostGameRect rect;
        bool placed;
        double ts;
        std::shared_ptr<Composite> output;
        gpu::TextureDesc drawable;
        gpu::CommandBuffer command;
        Clock::time_point started;
    };
    Turn *select_turn(double ts) {
        std::shared_ptr<Frame> f;
        HostGameRect rect{};
        bool placed = false;
        {
            std::lock_guard lock(mutex);
            if (stop)
                return nullptr;
            if (fake)
                fake_now = ts;
            rect = game_rect(&placed);
            if (class_known && last_class == HOST_SCREEN_GAMEPLAY)
                metric.tick(ts);
            sweep();
            const auto input = turn_input(ts);
            const auto check = rc_presenter_check(presenter.value, input);
            if (check.faults & RC_FAULT_DISPLAY_STALLED)
                fault(RC_FAULT_DISPLAY_STALLED,
                      "display link is not firing; worker is using timed fallback");
            if (check.faults & RC_FAULT_DISPLAY_FALLBACK)
                fault(RC_FAULT_DISPLAY_FALLBACK,
                      "drawable acknowledgement exceeded queue grace; using command completion "
                      "fallback");
            if (check.faults & RC_FAULT_GPU_TIMEOUT)
                fault(RC_FAULT_GPU_TIMEOUT,
                      "GPU completion timed out; retaining target until completion");
            for (auto opaque : check.fallback)
                if (auto pending = PresentFrameLease::borrow(opaque))
                    trace_ack(*pending, "timeout", ts);
            sweep();
            const auto selection = rc_presenter_admit(presenter.value, input);
            f = PresentFrameLease::borrow(selection.frame);
            for (size_t i = 0; i < 2; ++i) {
                auto old = PresentFrameLease::take(selection.discarded[i]);
                if (!old)
                    continue;
                if (selection.readiness[i] > 1)
                    fprintf(stderr, "presenter: native texture producer failed (%u)\n",
                            selection.readiness[i]);
                drop(old);
            }
            if (!f)
                return nullptr;
            if (f->status().repeat) {
                f->epoch = epoch;
                if (f->input.drawable_w > 0 && f->input.drawable_h > 0) {
                    float sx = float(rect.w) / f->input.drawable_w;
                    float sy = float(rect.h) / f->input.drawable_h;
                    f->input.scene.scale_x *= sx;
                    f->input.scene.offset_x *= sx;
                    f->input.scene.scale_y *= sy;
                    f->input.scene.offset_y *= sy;
                }
            }
            f->input.drawable_w = rect.w;
            f->input.drawable_h = rect.h;
        }
        return new Turn{std::move(f), rect, placed, ts};
    }
    void prepare_turn(Turn &turn) {
        const auto &f = turn.frame;
        std::lock_guard lock(mutex);
        if (!f->status().repeat) {
            if (history_epoch != f->epoch) {
                history.world = {};
                history.overlay = {};
                history_lease.reset();
                history_epoch = f->epoch;
            }
            bool reused = compositor_resolve_scene(&history, &f->input, f->had_draws);
            if (!reused)
                history_lease = history.world ? f->target : nullptr;
            f->scene_lease = history_lease;
        }
        publish_layout(*f);
    }
    RcTurnEncoding describe_turn(Turn &turn, gpu::Texture drawable) {
        const auto &f = turn.frame;
        const auto &rect = turn.rect;
        turn.drawable = drawable ? device->describe(drawable) : gpu::TextureDesc{};
        const auto format = offscreen ? gpu::Format::RGBA8 : turn.drawable.format;
        const auto &previous = f->composed;
        return {rect.w,
                rect.h,
                turn.drawable.width,
                turn.drawable.height,
                rect.x,
                rect.y,
                uint32_t(format),
                uint32_t(turn.drawable.format),
                previous ? previous->w : 0,
                previous ? previous->h : 0,
                previous ? uint32_t(previous->format) : 0,
                uint8_t(bool(previous)),
                f->status().repeat,
                uint8_t(turn.placed)};
    }
    uint64_t begin_turn(Turn &turn, bool reuse) {
        const auto format = offscreen ? gpu::Format::RGBA8 : turn.drawable.format;
        turn.output =
            reuse ? turn.frame->composed : pooled_composite(turn.rect.w, turn.rect.h, format);
        if (!turn.output)
            return 0;
        turn.command = device->begin();
        if (!turn.command)
            return 0;
        turn.started = Clock::now();
        turn.frame->composed = turn.output;
        return turn.command.id;
    }
    void encode_turn(Turn &turn, gpu::Texture drawable, uint32_t operation) {
        const auto &f = turn.frame;
        const auto &out = turn.output;
        const auto cb = turn.command;
        switch (operation) {
        case RC_TURN_COMPOSE:
            compositor_compose(device, &f->input, out->texture, cb);
            break;
        case RC_TURN_COPY:
            device->blit(cb, out->texture, {0, 0, out->w, out->h}, drawable, 0, 0);
            break;
        case RC_TURN_PLACE: {
            gpu::RenderPass clear;
            clear.color_count = 1;
            clear.color[0].texture = drawable;
            clear.color[0].load = gpu::Load::Clear;
            clear.color[0].store = gpu::Store::Store;
            clear.color[0].clear[0] = 12 / 255.0f;
            clear.color[0].clear[1] = 14 / 255.0f;
            clear.color[0].clear[2] = 18 / 255.0f;
            clear.color[0].clear[3] = 1;
            device->begin_render_pass(cb, clear);
            device->end_render_pass(cb);
            device->blit(cb, out->texture, {0, 0, out->w, out->h}, drawable, turn.rect.x,
                         turn.rect.y);
            break;
        }
        case RC_TURN_FIT: {
            CompositorInput in{};
            in.presentation_filter = f->input.presentation_filter;
            in.legacy = true;
            in.legacy_frame = out->texture;
            in.guest_w = out->w;
            in.guest_h = out->h;
            in.drawable_w = turn.drawable.width;
            in.drawable_h = turn.drawable.height;
            compositor_compose(device, &in, drawable, cb);
            break;
        }
        case RC_TURN_OVERLAYS: {
            FramePacingSnapshot snapshot;
            {
                std::lock_guard lock(mutex);
                snapshot = pacing.snapshot(turn.ts, timeline.counters().drops);
            }
            performance_overlay.draw(device, cb, drawable, turn.drawable.width,
                                     turn.drawable.height, snapshot, turn.ts,
                                     mods_display_overlay());
            const auto controls = host_present_controls();
            if (controls.wanted)
                controls_overlay.draw(device, cb, drawable, turn.drawable.width,
                                      turn.drawable.height, controls);
            break;
        }
        default:
            fprintf(stderr, "presenter: unknown encoding operation %u\n", operation);
            abort();
        }
    }
    void tick(double ts) {
        RcTurnCallbacks callbacks{};
        callbacks.context = this;
        callbacks.transition = [](void *opaque, RcSurfaceTransition transition) noexcept {
            auto &s = *static_cast<Service *>(opaque);
            if (!transition.installed)
                return;
            std::lock_guard lock(s.mutex);
            const auto cancelled = rc_presenter_transition(s.presenter.value, s.surface.viewport(),
                                                           transition.changed);
            for (auto opaque : cancelled.discarded)
                if (auto frame = PresentFrameLease::take(opaque))
                    s.drop(frame);
            s.sweep();
        };
        callbacks.select = [](void *opaque, double ts) noexcept -> void * {
            return static_cast<Service *>(opaque)->select_turn(ts);
        };
        callbacks.prefix = [](void *opaque, void *payload) noexcept -> uint8_t {
            auto &s = *static_cast<Service *>(opaque);
            auto &f = static_cast<Turn *>(payload)->frame;
            return !f->pending_native || s.submit_native(f, std::move(f->pending_native));
        };
        callbacks.prepare = [](void *opaque, void *payload) noexcept {
            static_cast<Service *>(opaque)->prepare_turn(*static_cast<Turn *>(payload));
        };
        callbacks.describe = [](void *opaque, void *payload, uint64_t drawable) noexcept {
            return static_cast<Service *>(opaque)->describe_turn(*static_cast<Turn *>(payload),
                                                                 {drawable});
        };
        callbacks.begin = [](void *opaque, void *payload, uint8_t reuse) noexcept {
            return static_cast<Service *>(opaque)->begin_turn(*static_cast<Turn *>(payload), reuse);
        };
        callbacks.encode = [](void *opaque, void *payload, uint64_t drawable,
                              uint32_t operation) noexcept {
            static_cast<Service *>(opaque)->encode_turn(*static_cast<Turn *>(payload), {drawable},
                                                        operation);
        };
        callbacks.commit = [](void *opaque, void *payload, uint64_t drawable,
                              uint64_t command) noexcept {
            auto &s = *static_cast<Service *>(opaque);
            auto &turn = *static_cast<Turn *>(payload);
            host_stats_note_phase(
                HOST_PHASE_COMPOSITE,
                std::chrono::duration<double, std::milli>(Clock::now() - turn.started).count());
            s.commit_present(turn.frame, {drawable}, {command});
        };
        callbacks.complete = [](void *opaque, void *payload, double ts,
                                uint8_t automatic) noexcept {
            auto &s = *static_cast<Service *>(opaque);
            const auto &f = static_cast<Turn *>(payload)->frame;
            {
                std::lock_guard lock(s.mutex);
                s.timeline.committed(f);
            }
            if (automatic) {
                s.ack_command(f, true, ts);
                if (!s.offscreen)
                    s.ack_presented(f, ts);
            }
        };
        callbacks.failed = [](void *opaque, void *payload, uint8_t reason) noexcept {
            const bool retry = reason == RC_TURN_NO_DRAWABLE;
            auto &s = *static_cast<Service *>(opaque);
            const auto &f = static_cast<Turn *>(payload)->frame;
            std::lock_guard lock(s.mutex);
            if (retry)
                s.fault(RC_FAULT_NO_DRAWABLE,
                        "swapchain returned no drawable; retrying newest frame");
            const auto rejected =
                rc_presenter_reject(s.presenter.value, PresentFrameLease::retain(f), retry);
            if (auto frame = PresentFrameLease::take(rejected.frame)) {
                if (rejected.release)
                    s.release(frame);
                else
                    s.drop(frame);
            }
        };
        callbacks.release = [](void *payload) noexcept { delete static_cast<Turn *>(payload); };
        rc_presenter_run(callbacks, surface.get(), ts, fake, automatic, offscreen,
                         g_present_suspended.load());
    }
    void start_worker() {
        std::lock_guard lock(mutex);
        auto context = new std::shared_ptr<Service>(shared_from_this());
        RcPresenterWorkerCallbacks callbacks{};
        callbacks.context = context;
        callbacks.state = [](void *opaque) noexcept -> RcPresenterWorkerState {
            auto &s = **static_cast<std::shared_ptr<Service> *>(opaque);
            std::lock_guard lock(s.mutex);
            s.frame_period = s.surface.refresh(s.frame_period);
            const double now = s.now();
            const double deadline = s.timeline.deadline(now);
            const bool native_pending = rc_presenter_native_pending(s.presenter.value);
            const bool poll_native = rc_presenter_poll_native(s.presenter.value, s.turn_input(now));
            return {now,
                    s.frame_period,
                    deadline,
                    uint8_t(s.window_ready()),
                    uint8_t(native_pending),
                    uint8_t(poll_native)};
        };
        callbacks.tick = [](void *opaque, double ts, uint8_t refresh) noexcept {
            auto &s = **static_cast<std::shared_ptr<Service> *>(opaque);
            {
                std::lock_guard lock(s.mutex);
                if (s.stop)
                    return;
                if (refresh) {
                    rc_presenter_signal(s.presenter.value, ts, false);
                }
            }
            s.tick(ts);
        };
        callbacks.release = [](void *opaque) noexcept {
            delete static_cast<std::shared_ptr<Service> *>(opaque);
        };
        worker = rc_presenter_worker_start(callbacks, offscreen);
        if (!worker) {
            fprintf(stderr, "presenter: could not start presentation worker\n");
            abort();
        }
    }
};
AtomicShared<Service> active;
gpu::Device *g_device = nullptr;

std::shared_ptr<Service> begin(bool fake, bool automatic, bool offscreen) {
    host_present_stop();
    auto s = std::make_shared<Service>();
    s->fake = fake;
    s->automatic = automatic;
    s->offscreen = offscreen;
    s->device = g_device;
    s->surface.configure(s->device, !fake);
    if (!fake && !s->device) {
        fprintf(stderr, "presenter: no GPU device installed; call host_present_set_device first\n");
        abort();
    }
    if (const char *mode = recomp_env("HOST_PRESENT_PACING"))
        s->duration_pacing = strcmp(mode, "immediate") != 0;
    if (!fake)
        if (const char *path = recomp_env("PRESENT_ACK_TRACE")) {
            s->ack_trace = fopen(path, "w");
            if (s->ack_trace)
                fprintf(s->ack_trace, "frame_id,screen_class,repeat,event,observed_s,event_s,"
                                      "submitted_s,gpu_s,deadline_s,shown,fallback,released\n");
            else
                fprintf(stderr, "presenter: cannot open acknowledgement trace %s\n", path);
        }
    if (!fake)
        if (const char *path = recomp_env("FRAME_TIMINGS")) {
            s->timings = fopen(path, "w");
            if (s->timings) {
                setvbuf(s->timings, nullptr, _IOFBF, 65536);
                fprintf(s->timings, "frame_id,screen_class,repeat,sealed_s,submitted_s,presented_s,"
                                    "gpu_ms,drops,display_ack,drawable_w,drawable_h\n");
                fflush(s->timings);
            } else
                fprintf(stderr, "presenter: cannot open frame timings %s\n", path);
        }
    active.store(s);
    if (!fake)
        ddraw_set_present_callbacks(host_present_first_write, host_frame_seal);
    return s;
}
void test_ack(uint64_t id, int kind, double ts) {
    auto s = active.load();
    if (!s)
        return;
    std::shared_ptr<Frame> f;
    {
        std::lock_guard lock(s->mutex);
        if (kind == 2) {
            auto complete = [&](const auto &a) {
                if (a->frame_id == id)
                    rc_frame_synthetic_prefix_complete(a->state);
            };
            s->timeline.each(complete, true);
            s->mailbox.each(complete);
            s->timeline.each(complete);
            s->sweep();
            return;
        }
        auto find = [&](const auto &candidate) {
            if (!f && candidate->frame_id == id)
                f = candidate;
        };
        s->timeline.each(find);
        if (!f)
            s->timeline.each(find, true);
    }
    if (!f)
        return;
    if (kind == 0)
        s->ack_command(f, true, ts);
    else
        s->ack_presented(f, ts);
}
} // namespace

void host_present_set_device(gpu::Device *device) {
    g_device = device;
}
gpu::Device *host_present_device() {
    return g_device;
}
void host_present_start(void *native_surface, int w, int h) {
    auto s = begin(false, true, false);
    // Display acknowledgements can arrive three refreshes after submission.
    // Match the swapchain's three drawables so that delay does not cap a 120 Hz
    // producer near 80 FPS. Keep smaller queues available for comparisons.
    s->flight_limit = 3;
    if (const char *value = recomp_env("HOST_PRESENT_FRAMES")) {
        if (!strcmp(value, "1"))
            s->flight_limit = 1;
        else if (!strcmp(value, "2"))
            s->flight_limit = 2;
    }
    s->drawable_w = w;
    s->drawable_h = h;
    s->surface.install(native_surface, w, h);
    s->surface.apply();
    fprintf(stderr, "presenter: %dx%d drawable, at most %zu display submissions in flight\n",
            int(s->drawable_w), int(s->drawable_h), s->flight_limit);
    if (!s->surface.attached() || w <= 0 || h <= 0) {
        fprintf(stderr, "presenter: requires an attached, sized surface on the renderer device\n");
        abort();
    }
    s->frame_period = s->surface.refresh(s->frame_period);
#ifdef __EMSCRIPTEN__
    // The browser's main loop owns the GPU and calls host_present_pump once
    // per animation frame: no worker, no pacer.
    (void)0;
#else
    s->start_worker();
#endif
}
// One presenter turn on the calling thread: for hosts whose GPU belongs to a
// loop they do not own (the browser's main thread).
void host_present_pump(void) {
    auto s = active.load();
    if (!s)
        return;
    const double ts = s->device ? s->device->now_seconds() : 0.0;
    {
        std::lock_guard lock(s->mutex);
        if (s->stop)
            return;
        rc_presenter_signal(s->presenter.value, ts, false);
    }
    s->tick(ts);
}
void host_present_start_offscreen(int w, int h) {
    auto s = begin(false, true, true);
    s->drawable_w = w;
    s->drawable_h = h;
    // The synthetic link below ticks at this rate; a Present that waits for a
    // refresh has to wait for the same one.
    s->frame_period = 1.0 / kOffscreenHz;
    s->start_worker();
}
void host_present_resize(int w, int h) {
    auto s = active.load();
    if (!s || w <= 0 || h <= 0)
        return;
    {
        // Both sizes change together under the mutex, so game_rect() never
        // sees (and publishes) a new width with an old height.
        std::lock_guard lock(s->mutex);
        s->drawable_w = w;
        s->drawable_h = h;
        s->publish_gate_size(s->game_rect());
    }
    s->surface.resize(w, h);
    s->notify_worker();
}
void host_present_install_surface(void *native_surface, int w, int h, int viewport) {
    auto s = active.load();
    if (!s)
        return;
    {
        // Both sizes change together under the mutex, so game_rect() never
        // sees (and publishes) a new width with an old height.
        std::lock_guard lock(s->mutex);
        s->drawable_w = w;
        s->drawable_h = h;
        s->publish_gate_size(s->game_rect());
    }
    s->surface.install(native_surface, w, h, viewport);
    s->notify_worker();
}
extern "C" void host_present_stop() {
    auto s = active.load();
    if (!s)
        return;
    {
        std::lock_guard lock(s->mutex);
        s->stop = true;
        if (s->writing)
            s->writing->pending_native.reset();
        s->mailbox.each([](const auto &frame) { frame->pending_native.reset(); });
        s->timeline.each([](const auto &frame) { frame->pending_native.reset(); }, true);
        s->notify_worker();
        s->completed.notify_all();
    }
    rc_presenter_worker_stop(s->worker);
    {
        std::unique_lock lock(s->mutex);
        rc_presenter_shutdown(s->presenter.value, s->fake);
        s->sweep();
        s->completed.wait(lock, [&] {
            s->sweep();
            return s->timeline.empty() && s->timeline.counters().pending_commands == 0;
        });
        lock.unlock();
        rc_presenter_deliveries_stop(s->deliveries);
        lock.lock();
        s->writing.reset();
        s->history_lease.reset();
        s->history.world = {};
        s->history.overlay = {};
        s->timeline.clear_cached();
        s->composite_pool.clear();
        s->sealed_layout = {};
        s->sealed_layout_valid = false;
        s->targets = {};
        s->surface.close();
    }
    if (!s->fake) {
        ddraw_set_present_callbacks(nullptr, nullptr);
        ddraw_drain_present_releases();
    }
    // Retain counters for the final report. begin() replaces this stopped service.
}
void host_present_drop_current() {
    auto s = active.load();
    if (!s)
        return;
    std::lock_guard lock(s->mutex);
    if (s->writing)
        s->timeline.mark_drop(s->writing);
}
bool host_present_running() {
    auto s = active.load();
    if (!s)
        return false;
    std::lock_guard lock(s->mutex);
    return !s->stop && !s->fake;
}
std::shared_ptr<void> host_present_target_lease(gpu::Texture world) {
    auto s = active.load();
    if (!s || !world)
        return {};
    std::lock_guard lock(s->mutex);
    for (auto &t : s->targets)
        if (t && t->scene.world == world)
            return t;
    return {};
}
HostSceneTarget host_present_acquire_target(int gw, int gh, int w, int h) {
    auto s = active.load();
    return s ? s->acquire(gw, gh, w, h) : HostSceneTarget{};
}
extern "C" void host_present_first_write() {
    auto s = active.load();
    if (!s)
        return;
    host_d3d_collect_present_targets();
    int w = 0, h = 0, bpp = 0;
    host_present_mode(&w, &h, &bpp);
    s->acquire(w > 0 ? w : s->guest_w, h > 0 ? h : s->guest_h, 0, 0);
}
extern "C" int host_present_needs_legacy_pixels() {
#ifdef RECOMP_PRESENT_HAS_UI_LAYER
    auto s = active.load();
    if (!s || mods_display_classic())
        return 1;
    {
        std::lock_guard lock(s->mutex);
        if (s->stop || !s->writing)
            return 1;
    }
    const auto frame = host_frame_current();
    if (frame.id && host_frame_class(frame) == HOST_SCREEN_GAMEPLAY && !host_frame_legacy(frame))
        return 0;
#endif
    return 1;
}
extern "C" void host_present_stage_rgba(const uint8_t *rgba, int w, int h) {
    auto s = active.load();
    if (!s || !rgba || w <= 0 || h <= 0)
        return;
    s->acquire(w, h, 0, 0);
    std::lock_guard lock(s->mutex);
    if (s->stop || !s->writing || !s->writing->target)
        return;
    const bool had_staged = s->writing->staged_pixels && !s->writing->pending_native;
    s->guest_w = w;
    s->guest_h = h;
    s->writing->viewport = false;
    auto &t = *s->writing->target;
    if (s->fake) {
        s->writing->pending_native.reset();
        s->writing->staged_pixels = true;
        t.test_pixels.assign(rgba, rgba + size_t(w) * h * 4);
    } else {
        t.device = s->device;
        if (!t.pixels || t.pixels_w != w || t.pixels_h != h ||
            t.pixels_format != gpu::Format::RGBA8) {
            if (t.pixels && had_staged)
                t.retired_pixels.reserve(t.retired_pixels.size() + 1);
            auto replacement = s->texture(w, h);
            if (!replacement)
                return;
            if (t.pixels) {
                if (had_staged)
                    t.retired_pixels.push_back(t.pixels);
                else
                    s->device->destroy(t.pixels);
            }
            t.pixels = replacement;
            t.pixels_format = gpu::Format::RGBA8;
            t.pixels_w = w;
            t.pixels_h = h;
        }
        if (t.pixels) {
            s->writing->pending_native.reset();
            s->writing->staged_pixels = true;
            s->device->upload(t.pixels, {0, 0, w, h}, rgba, w * 4);
        }
    }
}
extern "C" void host_present_stage_rgba_viewport(const uint8_t *rgba, int w, int h,
                                                 int32_t origin_x, int32_t origin_y,
                                                 uint32_t style) {
    host_present_stage_rgba(rgba, w, h);
    auto s = active.load();
    if (!s)
        return;
    std::lock_guard lock(s->mutex);
    if (!s->stop && s->writing) {
        s->writing->viewport = true;
        s->writing->viewport_style = style;
        s->writing->viewport_x = origin_x;
        s->writing->viewport_y = origin_y;
    }
}

// Whether a real device is presenting, so a guest-side GPU path has a queue to
// put its work on and a frame to stage it into.
bool host_present_gpu_ready() {
    auto s = active.load();
    return s && !s->fake && s->device && !s->stop;
}
bool host_present_stage_pending_native(const std::shared_ptr<PendingNativeTexture> &pending) {
    auto s = active.load();
    if (!s || s->fake || !s->device || !pending || !pending->source || pending->w <= 0 ||
        pending->h <= 0)
        return false;
    const auto format = s->device->describe(pending->source).format;
    s->acquire(pending->w, pending->h, 0, 0);
    std::lock_guard lock(s->mutex);
    if (s->stop || !s->writing || !s->writing->target)
        return false;
    auto &target = *s->writing->target;
    const bool had_staged = s->writing->staged_pixels && !s->writing->pending_native;
    if (!target.pixels || target.pixels_w != pending->w || target.pixels_h != pending->h ||
        target.pixels_format != format || had_staged) {
        if (target.pixels && had_staged)
            target.retired_pixels.reserve(target.retired_pixels.size() + 1);
        auto destination = s->texture(pending->w, pending->h, format);
        if (!destination)
            return false;
        if (target.pixels) {
            if (had_staged)
                target.retired_pixels.push_back(target.pixels);
            else
                s->device->destroy(target.pixels);
        }
        target.device = s->device;
        target.pixels = destination;
        target.pixels_w = pending->w;
        target.pixels_h = pending->h;
        target.pixels_format = format;
    }
    pending->destination = target.pixels;
    s->guest_w = pending->w;
    s->guest_h = pending->h;
    s->writing->viewport = false;
    s->writing->staged_pixels = true;
    s->writing->pending_native = pending;
    return true;
}
// The GPU counterpart of host_present_stage_rgba: the frame's pixels are a copy
// of `src`, encoded into `cb`. The caller commits `cb` before it seals, and the
// one queue runs that copy before anything composes the frame.
bool host_present_stage_texture(gpu::Texture src, int w, int h, int guest_w, int guest_h,
                                gpu::CommandBuffer cb) {
    auto s = active.load();
    if (!s || s->fake || !src || !cb || w <= 0 || h <= 0 || guest_w <= 0 || guest_h <= 0)
        return false;
    // A GPU blit copies bytes, not colors. Keep the source format so BGRA
    // backbuffers are not sampled as RGBA, including when a frame slot is reused.
    const auto format = s->device->describe(src).format;
    s->acquire(guest_w, guest_h, 0, 0);
    std::lock_guard lock(s->mutex);
    if (s->stop || !s->writing || !s->writing->target)
        return false;
    const bool had_staged = s->writing->staged_pixels && !s->writing->pending_native;
    s->writing->viewport = false;
    auto &t = *s->writing->target;
    t.device = s->device;
    if (!t.pixels || t.pixels_w != w || t.pixels_h != h || t.pixels_format != format) {
        if (t.pixels && had_staged)
            t.retired_pixels.reserve(t.retired_pixels.size() + 1);
        auto replacement = s->texture(w, h, format);
        if (!replacement)
            return false;
        if (t.pixels) {
            if (had_staged)
                t.retired_pixels.push_back(t.pixels);
            else
                s->device->destroy(t.pixels);
        }
        t.pixels = replacement;
        t.pixels_format = format;
        t.pixels_w = w;
        t.pixels_h = h;
    }
    if (!t.pixels)
        return false;
    s->writing->pending_native.reset();
    // Supersampled pixels do not enlarge the guest's client area. Publishing
    // their dimensions here sends touches beyond that area and hides cursors.
    s->guest_w = guest_w;
    s->guest_h = guest_h;
    s->writing->staged_pixels = true;
    s->device->blit(cb, src, {0, 0, w, h}, t.pixels, 0, 0);
    return true;
}
// The drawable the presenter composes into, in pixels. False before one exists.
bool host_present_drawable(int *w, int *h) {
    auto s = active.load();
    if (!s)
        return false;
    std::lock_guard lock(s->mutex);
    if (s->drawable_w <= 0 || s->drawable_h <= 0)
        return false;
    *w = s->drawable_w;
    *h = s->drawable_h;
    return true;
}
void host_present_set_input(const CompositorInput *input) {
    auto s = active.load();
    if (!s || !input)
        return;
    std::lock_guard lock(s->mutex);
    if (!s->writing || !s->writing->target)
        return;
    auto &f = *s->writing;
    if ((input->world && input->world != f.target->scene.world) ||
        (input->overlay && input->overlay != f.target->scene.overlay)) {
        fprintf(stderr,
                "presenter: compositor input does not belong to the acquired scene target\n");
        return;
    }
    f.input = *input;
    f.supplied = true;
    if (input->ui)
        f.ui = *input->ui;
    else
        f.ui = {};
    f.input.ui = &f.ui;
}
namespace {
struct PrefixHandle {
    std::shared_ptr<Service> service;
    std::shared_ptr<Frame> frame;
};
} // namespace
void *host_present_prefix_begin() {
    auto s = active.load();
    if (!s)
        return nullptr;
    auto handle = std::make_unique<PrefixHandle>();
    handle->service = s;
    std::shared_ptr<Frame> frame;
    {
        std::lock_guard lock(s->mutex);
        if (!s->writing)
            return nullptr;
        frame = s->writing;
        handle->frame = frame;
        rc_frame_prefix_begin(frame->state);
        s->timeline.command_started();
    }
    return handle.release();
}
void host_present_prefix_done(void *handle, bool success) {
    std::unique_ptr<PrefixHandle> h(static_cast<PrefixHandle *>(handle));
    if (!h)
        return;
    std::lock_guard lock(h->service->mutex);
    rc_frame_prefix_complete(h->frame->state, success);
    h->service->timeline.command_finished();
    h->service->sweep();
    h->service->completed.notify_all();
}
void host_present_track_command(gpu::CommandBuffer cb) {
    auto s = active.load();
    if (!s || !cb || !s->device)
        return;
    void *handle = host_present_prefix_begin();
    if (!handle)
        return;
    s->device->on_complete(cb, [handle](gpu::CommandStatus status, double) {
        host_present_prefix_done(handle, status == gpu::CommandStatus::Completed);
    });
}
// Seal guest-owned frame state and extract UI before publishing to the presentation worker.
// Only immutable snapshots cross that boundary; guest surface leases are resolved here.
// The open settings page rides on the frame about to be sealed, drawn by the
// compositor in host UI space. Every seal path takes it: a window present - a
// D3D11 renderer, a film - as much as a DirectDraw frame, or F10 opens a page
// nobody sees.
void attach_settings_page(const std::shared_ptr<Service> &s) {
    std::vector<uint8_t> page;
    if (!host_page_rgba(&page))
        return;
    std::lock_guard lock(s->mutex);
    if (!s->writing)
        return;
    ++s->settings_pages;
    if (s->fake)
        return;
    auto texture = s->composite(640, 480, gpu::Format::RGBA8);
    if (texture) {
        s->device->upload(texture->texture, {0, 0, 640, 480}, page.data(), 640 * 4);
        s->writing->settings_page = texture;
        s->writing->input.settings_page = texture->texture;
    }
}

extern "C" void host_frame_seal() {
    auto s = active.load();
    if (!s)
        return;
    auto f = host_frame_current();
    if (!f.id)
        return;
    profile_marker_frame();
    if (s->offscreen && s->capture_factory) {
        bool eligible = false;
        {
            std::lock_guard lock(s->mutex);
            eligible = s->writing && !s->writing->status().dropped && s->writing->target &&
                       s->writing->target->scene.w > 0;
        }
        // Pool exhaustion is not a completed present. Leave the request armed
        // for the next eligible frame; the runner still rejects a late turn.
        if (eligible) {
            auto capture = s->capture_factory(host_frame_class(f));
            std::lock_guard lock(s->mutex);
            s->writing->capture = std::move(capture);
        }
    }
    host_d3d_seal_commands();
#ifdef RECOMP_PRESENT_HAS_UI_LAYER
    // Extraction reads shim leases on the guest thread before publication.
    // Only immutable value types pass to the worker.
    if (s->previous_ui_epoch != s->epoch ||
        (s->class_known && s->last_class != host_frame_class(f)))
        s->previous_ui = {};
    UiFrame ui{};
    ui_layer_extract(f, s->guest_w, s->guest_h, &s->previous_ui, &ui);
    s->previous_ui = ui;
    {
        std::lock_guard lock(s->mutex);
        if (s->writing && s->writing->target) {
            s->writing->ui = std::move(ui);
            if (host_frame_class(f) != HOST_SCREEN_GAMEPLAY) {
                auto &writer = *s->writing;
                // Blit/Flip-only front-end presents already supplied a complete
                // image. Sparse UI records need not cover unchanged menu pixels.
                writer.supplied = true;
                writer.input.legacy = writer.staged_pixels;
                writer.input.legacy_frame =
                    writer.staged_pixels ? writer.target->pixels : gpu::Texture{};
                writer.input.guest_w = writer.staged_pixels ? s->guest_w : writer.ui.guest_w;
                writer.input.guest_h = writer.staged_pixels ? s->guest_h : writer.ui.guest_h;
            }
        }
    }
#endif
    {
        std::lock_guard lock(s->mutex);
        if (s->writing && s->writing->target && !s->writing->supplied) {
            auto &in = s->writing->input;
            in.guest_w = s->guest_w;
            in.guest_h = s->guest_h;
            const HostGameRect rect = s->game_rect();
            in.drawable_w = rect.w;
            in.drawable_h = rect.h;
            if (host_frame_class(f) == HOST_SCREEN_GAMEPLAY && !host_frame_legacy(f)) {
                s->writing->supplied = true;
                in.legacy = false; // compositor supplies the last scene on a no-draw frame
            }
        }
    }
    attach_settings_page(s);
    s->seal(f.id, host_frame_class(f), host_frame_had_draws(f) != 0);
#ifdef RECOMP_PRESENT_HAS_UI_LAYER
    s->previous_ui_epoch = s->epoch;
#endif
}
extern "C" void host_present_tick_for_test(double ts) {
    auto s = active.load();
    if (!s || !s->fake)
        return;
    s->signal_tick(ts);
    s->tick(ts);
}
#define PRESENT_COUNTER(name, field)                                                               \
    extern "C" uint64_t name() {                                                                   \
        auto s = active.load();                                                                    \
        if (!s)                                                                                    \
            return 0;                                                                              \
        std::lock_guard lock(s->mutex);                                                            \
        return s->field;                                                                           \
    }
PRESENT_COUNTER(host_present_unique_completed, timeline.counters().unique)
PRESENT_COUNTER(host_present_repeats, timeline.counters().repeats)
PRESENT_COUNTER(host_present_drops, timeline.counters().drops)
PRESENT_COUNTER(host_present_waits, waits)
PRESENT_COUNTER(host_present_settings_pages, settings_pages)
PRESENT_COUNTER(host_present_faults, faults)
PRESENT_COUNTER(host_present_scene_reused, history.scene_reused)
PRESENT_COUNTER(host_present_transition_epoch, epoch)
#undef PRESENT_COUNTER
extern "C" void mods_present_level_end() {
    auto s = active.load();
    if (!s)
        return;
    std::lock_guard lock(s->mutex);
    ++s->epoch;
    s->metric = {};
    s->class_known = false;
    mods_display_transition(s->epoch, s->last_class);
}
extern "C" int host_metric_continuous(double *minimum, double *elapsed) {
    auto s = active.load();
    if (!s || s->offscreen) {
        if (minimum)
            *minimum = 0;
        if (elapsed)
            *elapsed = 0;
        return 0;
    }
    std::lock_guard lock(s->mutex);
    return s->metric.read(minimum, elapsed);
}
extern "C" int host_metric_throughput(double *minimum, double *elapsed) {
    auto s = active.load();
    if (!s || !s->offscreen) {
        if (minimum)
            *minimum = 0;
        if (elapsed)
            *elapsed = 0;
        return 0;
    }
    std::lock_guard lock(s->mutex);
    return s->metric.read(minimum, elapsed);
}
bool host_present_copy_layout(LayoutSnapshot *out) {
    auto s = active.load();
    if (!s || !out)
        return false;
    std::lock_guard lock(s->mutex);
    if (!s->layout_valid)
        return false;
    *out = s->layouts[s->layout_slot];
    return true;
}
bool host_present_copy_sealed_layout(LayoutSnapshot *out) {
    auto s = active.load();
    if (!s || !out)
        return false;
    std::lock_guard lock(s->mutex);
    if (!s->sealed_layout_valid)
        return false;
    *out = s->sealed_layout;
    return true;
}
void host_present_test_begin(bool automatic, bool offscreen, unsigned display_frames) {
    auto s = begin(true, automatic, offscreen);
    s->flight_limit = !offscreen && display_frames >= 2 && display_frames <= 3 ? display_frames : 1;
}
unsigned host_present_test_flight_count() {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    return unsigned(s->timeline.size());
}
void host_present_test_seal(uint64_t id, HostScreenClass cls, bool had_draws, bool prefix_pending) {
    auto s = active.load();
    if (!s)
        return;
    s->acquire(s->guest_w, s->guest_h, 0, 0);
    s->seal(id, cls, had_draws, prefix_pending);
}
void host_present_test_command_done(uint64_t id) {
    test_ack(id, 0, 0);
}
void host_present_test_presented(uint64_t id, double ts) {
    test_ack(id, 1, ts);
}
void host_present_test_prefix_done(uint64_t id) {
    test_ack(id, 2, 0);
}
bool host_present_test_released(uint64_t id) {
    auto s = active.load();
    if (!s)
        return false;
    std::lock_guard lock(s->mutex);
    return s->test_released.count(id) != 0;
}
uint64_t host_present_test_last_id() {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    const auto cached = s->timeline.cached();
    return cached ? cached->frame_id : 0;
}
uint8_t host_present_test_last_pixel() {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    return s->last_pixel;
}
void host_present_test_fail_allocations(unsigned n) {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    s->fail_allocations = n;
}
int host_present_test_requested_width() {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    return s->requested_w;
}

bool host_present_test_read_rgba(uint8_t *out, size_t bytes) {
    auto s = active.load();
    if (!s || !out || !s->device)
        return false;
    std::shared_ptr<Composite> texture;
    {
        std::lock_guard lock(s->mutex);
        const auto cached = s->timeline.cached();
        texture = cached ? cached->composed : nullptr;
    }
    if (!texture || texture->format != gpu::Format::RGBA8 ||
        bytes < size_t(texture->w) * texture->h * 4)
        return false;
    // cached is published only after completion, so no GPU wait is needed.
    return s->device->readback(texture->texture, {0, 0, texture->w, texture->h}, out,
                               texture->w * 4);
}

bool host_present_copy_composite(HostCompletedComposite *out) {
    auto s = active.load();
    if (!s || !out || !s->device)
        return false;
    HostCompletedComposite result;
    std::shared_ptr<Composite> texture;
    {
        std::lock_guard lock(s->mutex);
        const auto cached = s->timeline.cached();
        texture = cached ? cached->composed : nullptr;
        if (!texture || texture->format != gpu::Format::RGBA8)
            return false;
        result.w = texture->w;
        result.h = texture->h;
        result.guest_w = cached->input.guest_w;
        result.guest_h = cached->input.guest_h;
        result.cls = cached->input.cls;
        result.frame_id = cached->frame_id;
        result.layout = s->layouts[s->layout_slot];
    }
    result.rgba.resize(size_t(result.w) * result.h * 4);
    if (!s->device->readback(texture->texture, {0, 0, result.w, result.h}, result.rgba.data(),
                             result.w * 4))
        return false;
    *out = std::move(result);
    return true;
}

void host_present_set_capture_factory(HostCaptureFactory factory) {
    auto s = active.load();
    if (!s)
        return;
    std::lock_guard lock(s->mutex);
    s->capture_factory = factory;
}

uint8_t host_present_test_last_ui_red() {
    auto s = active.load();
    if (!s)
        return 0;
    std::lock_guard lock(s->mutex);
    const auto cached = s->timeline.cached();
    return !cached || cached->ui.elements.empty() || cached->ui.elements[0].rgba.empty()
               ? 0
               : cached->ui.elements[0].rgba[0];
}

// Deterministic fake link uses the production worker predicate and tick state
// machine. No worker thread, layer, drawable or Metal device is created.
bool host_present_test_window_wake(double ts, bool display_tick, bool timeout) {
    auto s = active.load();
    if (!s || !s->fake || s->offscreen)
        return false;
    if (display_tick)
        s->signal_tick(ts);
    {
        std::lock_guard lock(s->mutex);
        if (!timeout && !s->window_ready())
            return false;
    }
    s->tick(ts);
    return true;
}
uint64_t host_present_test_in_flight() {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    return s->timeline.size() == 0 ? 0 : s->timeline.frame(0)->frame_id;
}
unsigned host_present_test_faults() {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    return s->faults;
}
bool host_present_test_input_legacy() {
    auto s = active.load();
    std::lock_guard lock(s->mutex);
    return s->timeline.size() != 0 && s->timeline.frame(0)->input.legacy;
}
void host_present_test_commit_swapchain(gpu::Swapchain chain) {
    auto s = active.load();
    if (!s || !s->fake || !s->device)
        return;
    std::shared_ptr<Frame> f;
    {
        std::lock_guard lock(s->mutex);
        if (s->timeline.size() != 0)
            f = s->timeline.frame(s->timeline.size() - 1);
    }
    if (!f)
        return;
    s->surface.adopt(chain);
    gpu::Texture drawable = s->surface.acquire();
    gpu::CommandBuffer cb = s->device->begin();
    s->commit_present(f, drawable, cb);
}
extern "C" int32_t host_display_anchor(uint64_t id, int8_t h, int8_t v, int clear) {
    if (clear)
        compositor_clear_anchor(id);
    else
        compositor_set_anchor(id, {h, v});
    return 0;
}
extern "C" uint32_t host_display_elements(uint64_t *ids, uint32_t max) {
    LayoutSnapshot layout;
    if (!host_present_copy_layout(&layout))
        return 0;
    if (ids)
        for (size_t i = 0; i < std::min(size_t(max), layout.elements.size()); ++i)
            ids[i] = layout.elements[i].id;
    return uint32_t(layout.elements.size());
}
extern "C" float host_display_aspect() {
    auto s = active.load();
    if (!s)
        return 4.0f / 3.0f;
    std::lock_guard lock(s->mutex);
    const HostGameRect rect = s->game_rect();
    return rect.w > 0 && rect.h > 0 ? float(rect.w) / rect.h : 4.0f / 3.0f;
}
extern "C" uint64_t host_display_epoch() {
    return host_present_transition_epoch();
}

void host_present_suspend(bool suspended) {
    g_present_suspended.store(suspended);
    if (!suspended)
        return;
    // Going to the background: the GPU completes nothing there, so any command
    // buffer still in flight when the process is frozen stays in flight, and
    // on resume the worker would wait on completions that never come. Give the
    // worker up to a second to see the flag and let the in-flight work finish.
    auto s = active.load();
    if (!s)
        return;
    std::unique_lock lock(s->mutex);
    s->notify_worker();
    s->completed.wait_for(lock, std::chrono::seconds(1), [&] {
        s->sweep();
        return s->timeline.size() == 0 && s->timeline.counters().pending_commands == 0;
    });
}

extern "C" HostGameRect host_present_current_game_rect() {
    auto s = active.load();
    if (!s)
        return {0, 0, 0, 0};
    return Service::unpack_rect(s->published_rect.load(std::memory_order_relaxed));
}

void host_present_set_controls(const controls::ControlsView &view) {
    std::lock_guard lock(g_present_controls_mutex);
    g_present_controls = view;
}
controls::ControlsView host_present_controls(void) {
    std::lock_guard lock(g_present_controls_mutex);
    return g_present_controls;
}
bool host_present_suspended(void) {
    return g_present_suspended.load();
}

// Present with a sync interval queues a flip for a vertical blank on Windows:
// it blocks only while the previously queued frame is still waiting for its
// blank, so a renderer slower than the display never waits and one faster is
// paced to it. This is how long until that earlier frame reaches the screen,
// on the device clock. The caller waits it out in the
// scheduler: a host sleep here would hold the guest baton for the whole wait
// and freeze every other guest thread with it. A headless presenter has no
// display to wait for, so a smoke still runs as fast as it can.
extern "C" double host_present_refresh_delay(int intervals) {
    auto s = active.load();
    if (!s || s->fake || !s->device || intervals <= 0)
        return 0.0;
    std::lock_guard lock(s->mutex);
    const double period = s->frame_period > 0 ? s->frame_period : 1.0 / 60;
    const double now = s->device->now_seconds();
    const double previous = s->queued_flip;
    const double blank = std::ceil(now / period) * period;
    s->queued_flip = std::max(blank, previous + intervals * period);
    return previous > now ? previous - now : 0.0;
}

extern "C" void host_present_seal_window() {
    auto s = active.load();
    if (!s)
        return;
    // Window presents (including D3D8 native-texture and RGBA handoffs) bypass
    // host_frame_seal, but are the same guest-side frame boundary for profiling.
    profile_marker_frame();
    // A separate sequence avoids collisions with DirectDraw's frame leases.
    static uint64_t next = uint64_t(1) << 63;
    attach_settings_page(s);
    s->seal(next++, HOST_SCREEN_MENU, false);
}
