// present_frame.h - the presenter's frame records, shared with its tests.
// Every texture here is a gpu.h handle owned through a lease: Target owns the
// scene textures it allocated, Composite owns one composed image. Nothing is
// freed by a handle going out of scope.
#pragma once
#include "compositor.h"
#include "gpu/gpu.h"
#include "present.h"
#include "ui_layer.h"
#include "../graphics/compositor/include/presenter.h"

#include <memory>
#include <vector>

// A texture the presenter allocated, destroyed with the last reference.
struct Composite {
    gpu::Device *device = nullptr;
    gpu::Texture texture;
    int w = 0, h = 0;
    gpu::Format format = gpu::Format::RGBA8;
    ~Composite() {
        if (device && texture)
            device->destroy(texture);
    }
};
struct Target {
    gpu::Device *device = nullptr;
    HostSceneTarget scene;
    gpu::Texture pixels;
    std::vector<gpu::Texture> retired_pixels;
    int pixels_w = 0, pixels_h = 0;
    gpu::Format pixels_format = gpu::Format::RGBA8;
    std::vector<uint8_t> test_pixels;
    void release_retired() {
        if (device)
            for (auto texture : retired_pixels)
                device->destroy(texture);
        retired_pixels.clear();
    }
    void release_scene() {
        if (device) {
            if (scene.world)
                device->destroy(scene.world);
            if (scene.overlay)
                device->destroy(scene.overlay);
        }
        scene = {};
    }
    ~Target() {
        release_scene();
        if (device && pixels)
            device->destroy(pixels);
        release_retired();
    }
};
struct PendingNativeTexture {
    gpu::Device *device = nullptr;
    gpu::Texture source;
    gpu::Texture destination;
    int w = 0, h = 0;
    std::shared_ptr<void> lease;
    uint32_t (*ready)(void *) = nullptr;
    ~PendingNativeTexture() {
        if (device && source)
            device->destroy(source);
    }
};
struct Frame {
    uint64_t frame_id = 0, epoch = 0;
    HostScreenClass cls = HOST_SCREEN_MENU;
    RcFrameState *state = rc_frame_state_new();
    Frame() = default;
    Frame(const Frame &) = delete;
    Frame &operator=(const Frame &) = delete;
    ~Frame() {
        rc_frame_state_free(state);
    }
    RcFrameSnapshot status() const {
        return rc_frame_snapshot(state);
    }
    bool had_draws = false, supplied = false, staged_pixels = false;
    bool viewport = false;
    uint32_t viewport_style = 0;
    int32_t viewport_x = 0, viewport_y = 0;
    std::shared_ptr<Target> target;
    std::shared_ptr<PendingNativeTexture> pending_native;
    // Scene history can advance before this drawable is acknowledged.
    std::shared_ptr<Target> scene_lease;
    UiFrame ui{};
    CompositorInput input{};
    LayoutSnapshot layout;
    HostFrameCapture capture;
    std::shared_ptr<Composite> composed;
    // The settings page texture behind input.settings_page, if any.
    std::shared_ptr<Composite> settings_page;
};
