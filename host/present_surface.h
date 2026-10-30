#pragma once
#include "gpu/gpu.h"
#include "../graphics/compositor/include/presenter.h"
#include <memory>
#include <utility>

class PresentSurface {
    RcPresenterSurface *surface = nullptr;

  public:
    PresentSurface() = default;
    PresentSurface(const PresentSurface &) = delete;
    PresentSurface &operator=(const PresentSurface &) = delete;
    ~PresentSurface() {
        rc_presenter_surface_free(surface);
    }
    void configure(gpu::Device *device, bool enabled) {
        RcSurfaceBackend backend{};
        backend.context = device;
        backend.create = [](void *context, void *native, int32_t w, int32_t h) noexcept {
            return static_cast<gpu::Device *>(context)->create_swapchain(native, w, h).id;
        };
        backend.destroy = [](void *context, uint64_t chain) noexcept {
            static_cast<gpu::Device *>(context)->destroy(gpu::Swapchain{chain});
        };
        backend.resize = [](void *context, uint64_t chain, int32_t w, int32_t h) noexcept {
            static_cast<gpu::Device *>(context)->resize(gpu::Swapchain{chain}, w, h);
        };
        backend.acquire = [](void *context, uint64_t chain) noexcept {
            return static_cast<gpu::Device *>(context)->acquire(gpu::Swapchain{chain}).id;
        };
        backend.release = [](void *context, uint64_t chain, uint64_t drawable) noexcept {
            static_cast<gpu::Device *>(context)->release_drawable({chain}, {drawable});
        };
        backend.present = [](void *context, uint64_t chain, uint64_t command, uint64_t drawable,
                             double duration, RcPresented callback) noexcept {
            auto lease = std::shared_ptr<void>(callback.context, callback.release);
            static_cast<gpu::Device *>(context)->present(
                {command}, {chain}, {drawable}, duration,
                [lease = std::move(lease), callback](double ts) {
                    callback.invoke(callback.context, ts);
                });
        };
        backend.refresh = [](void *context, uint64_t chain) noexcept {
            return static_cast<gpu::Device *>(context)->refresh_period({chain});
        };
        surface = rc_presenter_surface_new(backend, enabled);
    }
    RcPresenterSurface *get() const {
        return surface;
    }
    void install(void *native, int w, int h, int viewport = -1) {
        rc_presenter_surface_install(surface, native, w, h, viewport);
    }
    void resize(int w, int h) {
        rc_presenter_surface_resize(surface, w, h);
    }
    bool pending() const {
        return rc_presenter_surface_pending(surface);
    }
    int viewport() const {
        return rc_presenter_surface_viewport(surface);
    }
    bool attached() const {
        return rc_presenter_surface_attached(surface);
    }
    RcSurfaceTransition apply() {
        return rc_presenter_surface_apply(surface);
    }
    gpu::Texture acquire(bool suspended = false) {
        return {rc_presenter_surface_acquire(surface, suspended)};
    }
    static RcPresented callback(std::function<void(double)> function) {
        using Callback = std::function<void(double)>;
        return {new Callback(std::move(function)),
                [](void *context, double ts) noexcept { (*static_cast<Callback *>(context))(ts); },
                [](void *context) noexcept { delete static_cast<Callback *>(context); }};
    }
    double refresh(double fallback) const {
        return rc_presenter_surface_refresh(surface, fallback);
    }
    void adopt(gpu::Swapchain chain) {
        rc_presenter_surface_adopt(surface, chain.id);
    }
    void close() {
        rc_presenter_surface_close(surface);
    }
};
