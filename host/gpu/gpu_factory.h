#pragma once
#include "gpu.h"
#include <memory>

namespace gpu {
std::unique_ptr<Device> create_default_device();
const char *default_backend_name();
uint64_t window_surface_flags();
void *test_native_surface(int w, int h);
void *native_surface_for_window(void *sdl_window);
void release_window_surface(void *surface);
} // namespace gpu
