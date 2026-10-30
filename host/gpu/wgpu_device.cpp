#include "gpu_factory.h"
#include "../../graphics/d3d8-wgpu/include/host_gpu.h"
#include "../../platform/os.h"
#include "../../runtime/display_seam.h"
#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <mutex>
#include <unordered_map>
#include <vector>

namespace gpu {
namespace {
HostGpuRegion region(Region r) {
    return {r.x, r.y, r.w, r.h};
}
HostGpuTextureDesc descriptor(const TextureDesc &d) {
    return {d.width, d.height, int32_t(d.format), d.usage, d.mip_levels};
}
class WgpuDevice final : public Device {
    HostGpu *gpu_;
    std::mutex mapped_mutex_;
    std::unordered_map<uint64_t, std::vector<uint8_t>> mapped_;

  public:
    explicit WgpuDevice(HostGpu *gpu) : gpu_(gpu) {}
    bool presentation_timestamps() const override {
        return false;
    }
    bool minimum_present_duration() const override {
        return false;
    }
    ~WgpuDevice() override {
        host_gpu_free(gpu_);
    }
    bool compose_plan(CommandBuffer cb, Texture target, int w, int h, const CompositionDraw *draws,
                      size_t count) override {
        static_assert(sizeof(CompositionDraw) == sizeof(HostGpuCompositionDraw));
        std::vector<HostGpuCompositionDraw> commands;
        commands.reserve(count);
        for (size_t i = 0; i < count; ++i) {
            HostGpuCompositionDraw command{};
            command.texture = draws[i].texture;
            std::copy(draws[i].rect, draws[i].rect + 4, command.rect);
            std::copy(draws[i].uv, draws[i].uv + 4, command.uv);
            command.blend = draws[i].blend;
            command.filter = draws[i].filter;
            commands.push_back(command);
        }
        if (!host_gpu_compose_plan(gpu_, cb.id, target.id, w, h, commands.data(), count)) {
            fprintf(stderr, "host GPU: composition failed\n");
            abort();
        }
        return true;
    }
    Texture import_shared_texture(void *lease) override {
        return {host_gpu_texture_import_lease(gpu_, lease)};
    }
    Texture create_texture(const TextureDesc &desc) override;
    bool upload(Texture t, Region region, const void *bytes, int pitch, int level = 0) override;
    bool readback(Texture t, Region region, void *bytes, int pitch) override;
    void destroy(Texture t) override;
    TextureDesc describe(Texture t) override;
    uint64_t allocated_bytes(Texture t) override;
    Buffer create_buffer(uint64_t bytes, const void *contents) override;
    void update(Buffer b, uint64_t offset, const void *bytes, uint64_t count) override;
    const void *map_read(Buffer b) override;
    uint64_t buffer_bytes(Buffer b) override;
    void destroy(Buffer b) override;
    Pipeline render_pipeline(const std::string &shader, const RenderState &state) override;
    Pipeline compute_pipeline(const std::string &shader) override;
    int thread_execution_width(Pipeline p) override;
    CommandBuffer begin() override;
    void begin_render_pass(CommandBuffer cb, const RenderPass &pass) override;
    void set_pipeline(CommandBuffer cb, Pipeline p) override;
    void set_depth(CommandBuffer cb, const DepthState &d) override;
    void set_cull(CommandBuffer cb, Cull c) override;
    void set_viewport(CommandBuffer cb, const Viewport &v) override;
    void set_vertex_buffer(CommandBuffer cb, int slot, Buffer b, uint64_t offset) override;
    void set_bytes(CommandBuffer cb, Stage stage, int slot, const void *bytes,
                   uint64_t count) override;
    void set_buffer(CommandBuffer cb, Stage stage, int slot, Buffer b, uint64_t offset) override;
    void set_texture(CommandBuffer cb, Stage stage, int slot, Texture t) override;
    void set_sampler(CommandBuffer cb, Stage stage, int slot, const SamplerState &s) override;
    void draw(CommandBuffer cb, Primitive primitive, int first, int count) override;
    void end_render_pass(CommandBuffer cb) override;
    void begin_compute_pass(CommandBuffer cb) override;
    void dispatch_threads(CommandBuffer cb, int tx, int ty, int gx, int gy) override;
    void dispatch_groups(CommandBuffer cb, int groups_x, int groups_y, int gx, int gy) override;
    void end_compute_pass(CommandBuffer cb) override;
    void blit(CommandBuffer cb, Texture src, Region src_region, Texture dst, int dst_x,
              int dst_y) override;
    void copy_buffer_to_texture(CommandBuffer cb, Buffer src, uint64_t offset, int pitch,
                                Texture dst, Region dst_region) override;
    void copy_texture_to_buffer(CommandBuffer cb, Texture src, Region src_region, Buffer dst,
                                uint64_t offset, int pitch) override;
    void generate_mipmaps(CommandBuffer cb, Texture t) override;
    void on_complete(CommandBuffer cb,
                     std::function<void(CommandStatus, double gpu_ms)> fn) override;
    void commit(CommandBuffer cb) override;
    void wait(CommandBuffer cb) override;
    CommandStatus status(CommandBuffer cb) override;
    Swapchain create_swapchain(void *native_surface, int width, int height) override;
    void resize(Swapchain s, int width, int height) override;
    Format swapchain_format(Swapchain s) override;
    Texture acquire(Swapchain s) override;
    void release_drawable(Swapchain s, Texture t) override;
    void present(CommandBuffer cb, Swapchain s, Texture t, double min_duration_seconds,
                 std::function<void(double presented_seconds)> presented) override;
    double refresh_period(Swapchain s) override;
    void destroy(Swapchain s) override;
    double now_seconds() override;
};
Texture WgpuDevice::create_texture(const TextureDesc &d) {
    const auto request = descriptor(d);
    return {host_gpu_texture_create(gpu_, &request)};
}
bool WgpuDevice::upload(Texture t, Region r, const void *data, int pitch, int level) {
    return host_gpu_texture_upload(gpu_, t.id, region(r), data, pitch, level);
}
bool WgpuDevice::readback(Texture t, Region r, void *data, int pitch) {
    return host_gpu_texture_read(gpu_, t.id, region(r), data, pitch);
}
void WgpuDevice::destroy(Texture t) {
    host_gpu_texture_destroy(gpu_, t.id);
}
TextureDesc WgpuDevice::describe(Texture t) {
    HostGpuTextureDesc d{};
    if (!host_gpu_texture_describe(gpu_, t.id, &d, nullptr))
        return {};
    return {d.width, d.height, Format(d.format), d.usage, d.mip_levels};
}
uint64_t WgpuDevice::allocated_bytes(Texture t) {
    uint64_t size = 0;
    host_gpu_texture_describe(gpu_, t.id, nullptr, &size);
    return size;
}
Buffer WgpuDevice::create_buffer(uint64_t bytes, const void *data) {
    return {host_gpu_buffer_create(gpu_, bytes, data)};
}
void WgpuDevice::update(Buffer b, uint64_t offset, const void *data, uint64_t count) {
    if (!host_gpu_buffer_update(gpu_, b.id, offset, data, count)) {
        fprintf(stderr, "host GPU: buffer update failed\n");
        abort();
    }
}
const void *WgpuDevice::map_read(Buffer b) {
    const auto bytes = buffer_bytes(b);
    if (!bytes)
        return nullptr;
    std::vector<uint8_t> data(bytes);
    if (!host_gpu_buffer_read(gpu_, b.id, data.data(), bytes))
        return nullptr;
    std::lock_guard lock(mapped_mutex_);
    auto &mapped = mapped_[b.id];
    mapped = std::move(data);
    return mapped.data();
}
uint64_t WgpuDevice::buffer_bytes(Buffer b) {
    return host_gpu_buffer_size(gpu_, b.id);
}
void WgpuDevice::destroy(Buffer b) {
    {
        std::lock_guard lock(mapped_mutex_);
        mapped_.erase(b.id);
    }
    host_gpu_buffer_destroy(gpu_, b.id);
}
Pipeline WgpuDevice::render_pipeline(const std::string &shader, const RenderState &s) {
    HostGpuRenderState r{{int32_t(s.color_format[0]), int32_t(s.color_format[1])},
                         s.color_count,
                         uint8_t(s.depth_attachment),
                         uint8_t(s.blend_enabled),
                         int32_t(s.src_rgb),
                         int32_t(s.dst_rgb),
                         int32_t(s.src_alpha),
                         int32_t(s.dst_alpha),
                         uint8_t(s.write_color),
                         uint8_t(s.writes_point_size)};
    return {host_gpu_render_pipeline(gpu_, shader.c_str(), &r)};
}
Pipeline WgpuDevice::compute_pipeline(const std::string &shader) {
    return {host_gpu_compute_pipeline(gpu_, shader.c_str())};
}
int WgpuDevice::thread_execution_width(Pipeline p) {
    return host_gpu_thread_execution_width(gpu_, p.id);
}
CommandBuffer WgpuDevice::begin() {
    return {host_gpu_begin(gpu_)};
}
void WgpuDevice::begin_render_pass(CommandBuffer cb, const RenderPass &p) {
    HostGpuRenderPass r{};
    r.color_count = p.color_count;
    for (int i = 0; i < 2; ++i) {
        r.color[i].texture = p.color[i].texture.id;
        r.color[i].load = int(p.color[i].load);
        r.color[i].store = int(p.color[i].store);
        std::copy(p.color[i].clear, p.color[i].clear + 4, r.color[i].clear);
    }
    r.depth = {p.depth.texture.id, int(p.depth.load), int(p.depth.store), p.depth.clear};
    host_gpu_begin_render_pass(gpu_, cb.id, &r);
}
void WgpuDevice::set_pipeline(CommandBuffer cb, Pipeline p) {
    host_gpu_set_pipeline(gpu_, cb.id, p.id);
}
void WgpuDevice::set_depth(CommandBuffer cb, const DepthState &d) {
    host_gpu_set_depth(gpu_, cb.id, {int(d.compare), uint8_t(d.write)});
}
void WgpuDevice::set_cull(CommandBuffer cb, Cull c) {
    host_gpu_set_cull(gpu_, cb.id, int(c));
}
void WgpuDevice::set_viewport(CommandBuffer cb, const Viewport &v) {
    host_gpu_set_viewport(gpu_, cb.id, {v.x, v.y, v.w, v.h, v.near_z, v.far_z});
}
void WgpuDevice::set_vertex_buffer(CommandBuffer cb, int slot, Buffer b, uint64_t offset) {
    host_gpu_set_vertex_buffer(gpu_, cb.id, slot, b.id, offset);
}
void WgpuDevice::set_bytes(CommandBuffer cb, Stage stage, int slot, const void *data,
                           uint64_t count) {
    host_gpu_set_bytes(gpu_, cb.id, int(stage), slot, data, count);
}
void WgpuDevice::set_buffer(CommandBuffer cb, Stage stage, int slot, Buffer b, uint64_t offset) {
    host_gpu_set_buffer(gpu_, cb.id, int(stage), slot, b.id, offset);
}
void WgpuDevice::set_texture(CommandBuffer cb, Stage stage, int slot, Texture t) {
    host_gpu_set_texture(gpu_, cb.id, int(stage), slot, t.id);
}
void WgpuDevice::set_sampler(CommandBuffer cb, Stage stage, int slot, const SamplerState &s) {
    host_gpu_set_sampler(gpu_, cb.id, int(stage), slot,
                         {int(s.u), int(s.v), int(s.mag), int(s.min), int(s.mip), s.anisotropy});
}
void WgpuDevice::draw(CommandBuffer cb, Primitive p, int first, int count) {
    host_gpu_draw(gpu_, cb.id, int(p), first, count);
}
void WgpuDevice::end_render_pass(CommandBuffer cb) {
    host_gpu_end_render_pass(gpu_, cb.id);
}
void WgpuDevice::begin_compute_pass(CommandBuffer cb) {
    host_gpu_begin_compute_pass(gpu_, cb.id);
}
void WgpuDevice::dispatch_threads(CommandBuffer cb, int x, int y, int gx, int gy) {
    host_gpu_dispatch(gpu_, cb.id, x, y, gx, gy, 1);
}
void WgpuDevice::dispatch_groups(CommandBuffer cb, int x, int y, int gx, int gy) {
    host_gpu_dispatch(gpu_, cb.id, x, y, gx, gy, 0);
}
void WgpuDevice::end_compute_pass(CommandBuffer cb) {
    host_gpu_end_compute_pass(gpu_, cb.id);
}
void WgpuDevice::blit(CommandBuffer cb, Texture src, Region r, Texture dst, int x, int y) {
    host_gpu_blit(gpu_, cb.id, src.id, region(r), dst.id, x, y);
}
void WgpuDevice::copy_buffer_to_texture(CommandBuffer cb, Buffer b, uint64_t offset, int pitch,
                                        Texture t, Region r) {
    host_gpu_copy_buffer_to_texture(gpu_, cb.id, b.id, offset, pitch, t.id, region(r));
}
void WgpuDevice::copy_texture_to_buffer(CommandBuffer cb, Texture t, Region r, Buffer b,
                                        uint64_t offset, int pitch) {
    host_gpu_copy_texture_to_buffer(gpu_, cb.id, t.id, region(r), b.id, offset, pitch);
}
void WgpuDevice::generate_mipmaps(CommandBuffer cb, Texture t) {
    host_gpu_generate_mipmaps(gpu_, cb.id, t.id);
}
void WgpuDevice::on_complete(CommandBuffer cb, std::function<void(CommandStatus, double)> fn) {
    using Callback = std::function<void(CommandStatus, double)>;
    auto *callback = new Callback(std::move(fn));
    host_gpu_on_complete(
        gpu_, cb.id,
        [](void *data, int32_t status, double ms) {
            std::unique_ptr<Callback> callback(static_cast<Callback *>(data));
            try {
                (*callback)(CommandStatus(status), ms);
            } catch (...) {
                fprintf(stderr, "host GPU: completion callback threw\n");
                abort();
            }
        },
        callback);
}
void WgpuDevice::commit(CommandBuffer cb) {
    host_gpu_commit(gpu_, cb.id);
}
void WgpuDevice::wait(CommandBuffer cb) {
    host_gpu_wait(gpu_, cb.id);
}
CommandStatus WgpuDevice::status(CommandBuffer cb) {
    return CommandStatus(host_gpu_status(gpu_, cb.id));
}
Swapchain WgpuDevice::create_swapchain(void *surface, int w, int h) {
    return {host_gpu_swapchain_create(gpu_, surface, w, h)};
}
void WgpuDevice::resize(Swapchain s, int w, int h) {
    host_gpu_swapchain_resize(gpu_, s.id, w, h);
}
Format WgpuDevice::swapchain_format(Swapchain s) {
    return Format(host_gpu_swapchain_format(gpu_, s.id));
}
Texture WgpuDevice::acquire(Swapchain s) {
    return {host_gpu_swapchain_acquire(gpu_, s.id)};
}
void WgpuDevice::release_drawable(Swapchain s, Texture t) {
    host_gpu_swapchain_release(gpu_, s.id, t.id);
}
void WgpuDevice::present(CommandBuffer cb, Swapchain s, Texture t, double duration,
                         std::function<void(double)> fn) {
    using Callback = std::function<void(double)>;
    auto *callback = new Callback(std::move(fn));
    host_gpu_swapchain_present(
        gpu_, cb.id, s.id, t.id, duration,
        [](void *data, double time) {
            std::unique_ptr<Callback> callback(static_cast<Callback *>(data));
            try {
                (*callback)(time);
            } catch (...) {
                fprintf(stderr, "host GPU: presentation callback threw\n");
                abort();
            }
        },
        callback);
}
double WgpuDevice::refresh_period(Swapchain) {
    const double hz = host_display_refresh_rate();
    return hz > 1.0 ? 1.0 / hz : 1.0 / 60;
}
void WgpuDevice::destroy(Swapchain s) {
    host_gpu_swapchain_destroy(gpu_, s.id);
}
double WgpuDevice::now_seconds() {
    return double(os_monotonic_ns()) * 1e-9;
}

} // namespace
std::unique_ptr<Device> create_default_device() {
    HostGpu *gpu = host_gpu_create();
    return gpu ? std::make_unique<WgpuDevice>(gpu) : nullptr;
}
const char *default_backend_name() {
    return "wgpu";
}
} // namespace gpu
