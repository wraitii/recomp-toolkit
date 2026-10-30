#pragma once
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef struct HostGpu HostGpu;
typedef struct HostGpuTextureDesc {
    int32_t width, height, format;
    uint32_t usage;
    int32_t mip_levels;
} HostGpuTextureDesc;
typedef struct HostGpuRegion {
    int32_t x, y, w, h;
} HostGpuRegion;
typedef struct HostGpuCompositionDraw {
    uint64_t texture;
    double rect[4], uv[4];
    uint32_t blend, filter;
} HostGpuCompositionDraw;
uint8_t host_gpu_compose_plan(HostGpu *, uint64_t command, uint64_t target, int32_t width,
                              int32_t height, const HostGpuCompositionDraw *, size_t count);
HostGpu *host_gpu_create(void);
void host_gpu_free(HostGpu *gpu);
uint64_t host_gpu_texture_create(HostGpu *gpu, const HostGpuTextureDesc *desc);
uint64_t host_gpu_texture_import_lease(HostGpu *gpu, void *lease);
uint8_t host_gpu_texture_upload(HostGpu *gpu, uint64_t texture, HostGpuRegion region,
                                const void *data, int32_t pitch, int32_t level);
uint8_t host_gpu_texture_read(HostGpu *gpu, uint64_t texture, HostGpuRegion region, void *data,
                              int32_t pitch);
uint8_t host_gpu_texture_describe(HostGpu *gpu, uint64_t texture, HostGpuTextureDesc *desc,
                                  uint64_t *allocated);
void host_gpu_texture_destroy(HostGpu *gpu, uint64_t texture);
uint64_t host_gpu_buffer_create(HostGpu *gpu, uint64_t size, const void *data);
uint8_t host_gpu_buffer_update(HostGpu *gpu, uint64_t buffer, uint64_t offset, const void *data,
                               uint64_t size);
uint8_t host_gpu_buffer_read(HostGpu *gpu, uint64_t buffer, void *data, uint64_t size);
uint64_t host_gpu_buffer_size(HostGpu *gpu, uint64_t buffer);
void host_gpu_buffer_destroy(HostGpu *gpu, uint64_t buffer);
typedef struct HostGpuRenderState {
    int32_t color_format[2], color_count;
    uint8_t depth_attachment, blend_enabled;
    int32_t src_rgb, dst_rgb, src_alpha, dst_alpha;
    uint8_t write_color, writes_point_size;
} HostGpuRenderState;
typedef struct HostGpuDepthState {
    int32_t compare;
    uint8_t write;
} HostGpuDepthState;
typedef struct HostGpuSamplerState {
    int32_t u, v, mag, min, mip, anisotropy;
} HostGpuSamplerState;
typedef struct HostGpuColorAttachment {
    uint64_t texture;
    int32_t load, store;
    float clear[4];
} HostGpuColorAttachment;
typedef struct HostGpuDepthAttachment {
    uint64_t texture;
    int32_t load, store;
    float clear;
} HostGpuDepthAttachment;
typedef struct HostGpuRenderPass {
    HostGpuColorAttachment color[2];
    int32_t color_count;
    HostGpuDepthAttachment depth;
} HostGpuRenderPass;
typedef struct HostGpuViewport {
    double x, y, w, h, near_z, far_z;
} HostGpuViewport;
typedef void (*HostGpuCompletion)(void *userdata, int32_t status, double gpu_ms);
typedef void (*HostGpuPresented)(void *userdata, double time);
uint64_t host_gpu_render_pipeline(HostGpu *, const char *, const HostGpuRenderState *);
uint64_t host_gpu_compute_pipeline(HostGpu *, const char *);
int32_t host_gpu_thread_execution_width(HostGpu *, uint64_t);
uint64_t host_gpu_begin(HostGpu *);
void host_gpu_begin_render_pass(HostGpu *, uint64_t, const HostGpuRenderPass *);
void host_gpu_set_pipeline(HostGpu *, uint64_t, uint64_t);
void host_gpu_set_depth(HostGpu *, uint64_t, HostGpuDepthState);
void host_gpu_set_cull(HostGpu *, uint64_t, int32_t);
void host_gpu_set_viewport(HostGpu *, uint64_t, HostGpuViewport);
void host_gpu_set_vertex_buffer(HostGpu *, uint64_t, int32_t, uint64_t, uint64_t);
void host_gpu_set_bytes(HostGpu *, uint64_t, int32_t, int32_t, const void *, uint64_t);
void host_gpu_set_buffer(HostGpu *, uint64_t, int32_t, int32_t, uint64_t, uint64_t);
void host_gpu_set_texture(HostGpu *, uint64_t, int32_t, int32_t, uint64_t);
void host_gpu_set_sampler(HostGpu *, uint64_t, int32_t, int32_t, HostGpuSamplerState);
void host_gpu_draw(HostGpu *, uint64_t, int32_t, int32_t, int32_t);
void host_gpu_end_render_pass(HostGpu *, uint64_t);
void host_gpu_begin_compute_pass(HostGpu *, uint64_t);
void host_gpu_dispatch(HostGpu *, uint64_t, int32_t, int32_t, int32_t, int32_t, uint8_t);
void host_gpu_end_compute_pass(HostGpu *, uint64_t);
void host_gpu_blit(HostGpu *, uint64_t, uint64_t, HostGpuRegion, uint64_t, int32_t, int32_t);
void host_gpu_copy_buffer_to_texture(HostGpu *, uint64_t, uint64_t, uint64_t, int32_t, uint64_t,
                                     HostGpuRegion);
void host_gpu_copy_texture_to_buffer(HostGpu *, uint64_t, uint64_t, HostGpuRegion, uint64_t,
                                     uint64_t, int32_t);
void host_gpu_generate_mipmaps(HostGpu *, uint64_t, uint64_t);
void host_gpu_on_complete(HostGpu *, uint64_t, HostGpuCompletion, void *);
void host_gpu_commit(HostGpu *, uint64_t);
void host_gpu_wait(HostGpu *, uint64_t);
int32_t host_gpu_status(HostGpu *, uint64_t);
uint64_t host_gpu_swapchain_create(HostGpu *, void *, int32_t, int32_t);
void host_gpu_swapchain_resize(HostGpu *, uint64_t, int32_t, int32_t);
int32_t host_gpu_swapchain_format(HostGpu *, uint64_t);
uint64_t host_gpu_swapchain_acquire(HostGpu *, uint64_t);
void host_gpu_swapchain_release(HostGpu *, uint64_t, uint64_t);
void host_gpu_swapchain_present(HostGpu *, uint64_t, uint64_t, uint64_t, double, HostGpuPresented,
                                void *);
void host_gpu_swapchain_destroy(HostGpu *, uint64_t);
#ifdef __cplusplus
}
#endif
