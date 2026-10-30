#include "gpu_factory.h"
#include <SDL3/SDL.h>
#include <SDL3/SDL_metal.h>
#import <QuartzCore/CAMetalLayer.h>
#include <mutex>
#include <unordered_set>
#include <unordered_map>

namespace gpu {
namespace {
std::mutex surface_mutex;
std::unordered_set<void *> test_surfaces;
std::unordered_map<void *, SDL_MetalView> window_surfaces;
} // namespace

uint64_t window_surface_flags() {
    return SDL_WINDOW_METAL;
}

void *native_surface_for_window(void *window) {
    if (!window)
        return nullptr;
    SDL_MetalView view = SDL_Metal_CreateView(static_cast<SDL_Window *>(window));
    if (!view)
        return nullptr;
    void *surface = SDL_Metal_GetLayer(view);
    if (!surface) {
        SDL_Metal_DestroyView(view);
        return nullptr;
    }
    std::lock_guard lock(surface_mutex);
    window_surfaces.emplace(surface, view);
    return surface;
}

void *test_native_surface(int w, int h) {
    if (w <= 0 || h <= 0)
        return nullptr;
    CAMetalLayer *layer = [CAMetalLayer layer];
    layer.drawableSize = CGSizeMake(w, h);
    void *surface = (__bridge_retained void *)layer;
    std::lock_guard lock(surface_mutex);
    test_surfaces.insert(surface);
    return surface;
}

void release_window_surface(void *surface) {
    bool owned;
    SDL_MetalView view = nullptr;
    {
        std::lock_guard lock(surface_mutex);
        owned = test_surfaces.erase(surface) != 0;
        if (auto found = window_surfaces.find(surface); found != window_surfaces.end()) {
            view = found->second;
            window_surfaces.erase(found);
        }
    }
    if (view)
        SDL_Metal_DestroyView(view);
    if (owned)
        (void)CFBridgingRelease(surface);
}
} // namespace gpu
