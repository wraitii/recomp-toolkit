#include "gpu_factory.h"
#include <SDL3/SDL.h>
#include <cstdio>
#include <mutex>
#include <unordered_set>

namespace gpu {
namespace {
struct WindowSurface {
    uint32_t kind;
    void *window;
    void *display;
    uint64_t id;
    int32_t screen;
    SDL_Window *owned_window;
};
std::mutex surface_mutex;
std::unordered_set<WindowSurface *> surfaces;

WindowSurface *make_surface(SDL_Window *window, bool owned) {
    if (!window)
        return nullptr;
    auto props = SDL_GetWindowProperties(window);
    auto *surface = new WindowSurface{};
    surface->window = SDL_GetPointerProperty(props, SDL_PROP_WINDOW_WIN32_HWND_POINTER, nullptr);
    if (surface->window) {
        surface->kind = 1;
        surface->display =
            SDL_GetPointerProperty(props, SDL_PROP_WINDOW_WIN32_INSTANCE_POINTER, nullptr);
    } else {
        surface->window =
            SDL_GetPointerProperty(props, SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER, nullptr);
        surface->display =
            SDL_GetPointerProperty(props, SDL_PROP_WINDOW_WAYLAND_DISPLAY_POINTER, nullptr);
        if (surface->window && surface->display) {
            surface->kind = 2;
        } else {
            surface->display =
                SDL_GetPointerProperty(props, SDL_PROP_WINDOW_X11_DISPLAY_POINTER, nullptr);
            surface->id =
                uint64_t(SDL_GetNumberProperty(props, SDL_PROP_WINDOW_X11_WINDOW_NUMBER, 0));
            surface->screen =
                int32_t(SDL_GetNumberProperty(props, SDL_PROP_WINDOW_X11_SCREEN_NUMBER, 0));
            if (surface->display && surface->id)
                surface->kind = 3;
        }
    }
    if (!surface->kind) {
        fprintf(stderr, "host GPU: SDL window has no supported native surface handle\n");
        delete surface;
        return nullptr;
    }
    surface->owned_window = owned ? window : nullptr;
    std::lock_guard lock(surface_mutex);
    surfaces.insert(surface);
    return surface;
}
} // namespace

uint64_t window_surface_flags() {
    return 0;
}
void *native_surface_for_window(void *window) {
    return make_surface(static_cast<SDL_Window *>(window), false);
}
void *test_native_surface(int w, int h) {
    if (w <= 0 || h <= 0 || !SDL_InitSubSystem(SDL_INIT_VIDEO))
        return nullptr;
    SDL_Window *window = SDL_CreateWindow("GPU surface", w, h, SDL_WINDOW_HIDDEN);
    auto *surface = make_surface(window, true);
    if (!surface && window)
        SDL_DestroyWindow(window);
    return surface;
}
void release_window_surface(void *ptr) {
    auto *surface = static_cast<WindowSurface *>(ptr);
    {
        std::lock_guard lock(surface_mutex);
        if (!surfaces.erase(surface))
            return;
    }
    if (surface->owned_window)
        SDL_DestroyWindow(surface->owned_window);
    delete surface;
}
} // namespace gpu
