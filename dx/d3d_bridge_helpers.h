#pragma once

#include "d3d_bridge.h"

#include <array>
#include <cstdint>
#include <map>
#include <memory>
#include <set>
#include <unordered_map>
#include <utility>
#include <vector>

struct D3dTextureSync {
    bool valid = false;
    uint32_t texture_id = 0;
    uint32_t format = 0;
    std::vector<uint64_t> generations;

    bool matches(uint32_t id, uint32_t fmt,
                 const std::vector<recomp::d3d::TextureLevel> &levels) const {
        if (!valid || texture_id != id || format != fmt || generations.size() != levels.size())
            return false;
        for (size_t index = 0; index < levels.size(); ++index)
            if (levels[index].force_upload || generations[index] != levels[index].generation)
                return false;
        return true;
    }

    void record(uint32_t id, uint32_t fmt, const std::vector<recomp::d3d::TextureLevel> &levels) {
        valid = true;
        texture_id = id;
        format = fmt;
        generations.clear();
        for (const auto &level : levels) {
            valid &= !level.force_upload;
            generations.push_back(level.generation);
        }
    }
};

struct D3dHostDevice {
    rust::Box<recomp::d3d::Device> value;
    std::array<D3dTextureSync, 8> texture_sync;

    void forget_textures() {
        texture_sync = {};
    }

    D3dHostDevice() : value(recomp::d3d::new_device()) {}
};

struct D3d9HostResource;

struct D3d9HostDevice {
    rust::Box<recomp::d3d::D3d9Device> value;
    bool native_present_available = true;

    D3d9HostDevice() : value(recomp::d3d::new_d3d9_device()) {}

    recomp::d3d::Status register_resource(uint32_t id, const D3d9HostResource &resource);
    void unregister_resource(uint32_t id) {
        recomp::d3d::d3d9_unregister_resource(*value, id);
    }
    recomp::d3d::Status register_resource_view(uint32_t id, const D3d9HostResource &resource,
                                               uint32_t face, uint32_t level, bool volume);
    recomp::d3d::Status register_implicit_targets(uint32_t backbuffer, uint32_t depth) {
        return recomp::d3d::d3d9_register_implicit_targets(*value, backbuffer, depth);
    }
    recomp::d3d::Status begin_scene() {
        return recomp::d3d::d3d9_begin_scene(*value);
    }
    recomp::d3d::Status end_scene() {
        return recomp::d3d::d3d9_end_scene(*value);
    }
    recomp::d3d::Status flush() {
        return recomp::d3d::d3d9_flush(*value);
    }
    recomp::d3d::Status set_render_target(uint32_t index, uint32_t id, uint32_t face,
                                          uint32_t level, bool volume) {
        return recomp::d3d::d3d9_set_render_target(*value, index, id, face, level, volume);
    }
    recomp::d3d::Status set_depth_stencil(uint32_t id, uint32_t face, uint32_t level, bool volume) {
        return recomp::d3d::d3d9_set_depth_stencil(*value, id, face, level, volume);
    }
    recomp::d3d::ValueResult render_target() const {
        return recomp::d3d::d3d9_get_render_target(*value);
    }
    recomp::d3d::ValueResult depth_stencil() const {
        return recomp::d3d::d3d9_get_depth_stencil(*value);
    }
    recomp::d3d::Status clear(uint32_t flags, uint32_t color, float z, uint32_t stencil,
                              rust::Slice<const recomp::d3d::D3d9Rect> rects) {
        return recomp::d3d::d3d9_clear(*value, flags, color, z, stencil, rects);
    }
    recomp::d3d::Status draw_primitive(uint32_t primitive, uint32_t start_vertex,
                                       uint32_t primitive_count) {
        return recomp::d3d::d3d9_draw_primitive(*value, primitive, start_vertex, primitive_count);
    }
    recomp::d3d::Status draw_indexed_primitive(uint32_t primitive, int32_t base_vertex,
                                               uint32_t min_index, uint32_t num_vertices,
                                               uint32_t start_index, uint32_t primitive_count) {
        return recomp::d3d::d3d9_draw_indexed_primitive(*value, primitive, base_vertex, min_index,
                                                        num_vertices, start_index, primitive_count);
    }
    recomp::d3d::Status draw_primitive_up(uint32_t primitive, uint32_t primitive_count,
                                          rust::Slice<const uint8_t> vertices, uint32_t stride) {
        return recomp::d3d::d3d9_draw_primitive_up(*value, primitive, primitive_count, vertices,
                                                   stride);
    }
    recomp::d3d::Status draw_indexed_primitive_up(uint32_t primitive, uint32_t min_vertex,
                                                  uint32_t num_vertices, uint32_t primitive_count,
                                                  rust::Slice<const uint8_t> indices,
                                                  uint32_t index_format,
                                                  rust::Slice<const uint8_t> vertices,
                                                  uint32_t stride) {
        return recomp::d3d::d3d9_draw_indexed_primitive_up(*value, primitive, min_vertex,
                                                           num_vertices, primitive_count, indices,
                                                           index_format, vertices, stride);
    }
    recomp::d3d::Status
    stretch_rect(uint32_t src, rust::Slice<const recomp::d3d::D3d9Rect> src_rect, uint32_t dst,
                 rust::Slice<const recomp::d3d::D3d9Rect> dst_rect, uint32_t filter) {
        return recomp::d3d::d3d9_stretch_rect(*value, src, src_rect, dst, dst_rect, filter);
    }
    recomp::d3d::Status update_surface(D3d9HostResource &source, uint32_t src_face,
                                       uint32_t src_level,
                                       rust::Slice<const recomp::d3d::D3d9Rect> src_rect,
                                       D3d9HostResource &destination, uint32_t dst_face,
                                       uint32_t dst_level, int32_t dst_x, int32_t dst_y);
    recomp::d3d::Status update_texture(D3d9HostResource &source, D3d9HostResource &destination);
    recomp::d3d::Status get_render_target_data(uint32_t src, D3d9HostResource &destination,
                                               uint32_t dst_face, uint32_t dst_level);
    recomp::d3d::Status color_fill(uint32_t dst, rust::Slice<const recomp::d3d::D3d9Rect> rect,
                                   uint32_t color) {
        return recomp::d3d::d3d9_color_fill(*value, dst, rect, color);
    }
    recomp::d3d::ValueResult create_query(uint32_t kind) {
        return recomp::d3d::d3d9_create_query(*value, kind);
    }
    recomp::d3d::Status destroy_query(uint32_t handle) {
        return recomp::d3d::d3d9_destroy_query(*value, handle);
    }
    recomp::d3d::ValueResult query_type(uint32_t handle) const {
        return recomp::d3d::d3d9_query_type(*value, handle);
    }
    recomp::d3d::ValueResult query_data_size(uint32_t kind) const {
        return recomp::d3d::d3d9_query_data_size(kind);
    }
    recomp::d3d::Status query_issue(uint32_t handle, uint32_t flags) {
        return recomp::d3d::d3d9_query_issue(*value, handle, flags);
    }
    recomp::d3d::ValueResult query_get_data(uint32_t handle, rust::Slice<uint8_t> data,
                                            uint32_t size, uint32_t flags) {
        return recomp::d3d::d3d9_query_get_data(*value, handle, data, size, flags);
    }
    recomp::d3d::D3d9ResetResult reset(const recomp::d3d::D3d9ResetRequest &request) {
        return recomp::d3d::d3d9_reset(*value, request);
    }
    recomp::d3d::ValueResult test_cooperative_level() {
        return recomp::d3d::d3d9_test_cooperative_level(*value);
    }
    recomp::d3d::ValuesResult registered_resource_ids() const {
        return recomp::d3d::d3d9_registered_resource_ids(*value);
    }
    recomp::d3d::Status prepare_resource_reset(uint32_t id, const D3d9HostResource &resource);
    recomp::d3d::Status reset_ready() {
        return recomp::d3d::d3d9_reset_ready(*value);
    }
    void commit_resource_reset(uint32_t id, D3d9HostResource &resource);
    recomp::d3d::D3d9ResetResult finish_reset() {
        return recomp::d3d::d3d9_finish_reset(*value);
    }
    recomp::d3d::Status abort_reset() {
        return recomp::d3d::d3d9_abort_reset(*value);
    }
    recomp::d3d::D3d9FrameResult present(rust::Slice<const recomp::d3d::D3d9Rect> src_rect,
                                         rust::Slice<const recomp::d3d::D3d9Rect> dst_rect) {
        return recomp::d3d::d3d9_present(*value, src_rect, dst_rect);
    }
    recomp::d3d::FrameHandoffResult
    present_handoff(rust::Slice<const recomp::d3d::D3d9Rect> src_rect,
                    rust::Slice<const recomp::d3d::D3d9Rect> dst_rect) {
        return recomp::d3d::d3d9_present_handoff(*value, src_rect, dst_rect);
    }
    recomp::d3d::D3d9FrameResult present_readback() const {
        return recomp::d3d::d3d9_present_readback(*value);
    }
    void commit_present() {
        recomp::d3d::d3d9_commit_present(*value);
    }

    recomp::d3d::D3dxSurfaceReadResult d3dx_surface_read(D3d9HostResource &resource, uint32_t face,
                                                         uint32_t level,
                                                         const recomp::d3d::D3d9Rect &rect,
                                                         bool rect_present, uint32_t color_key);
    recomp::d3d::Status d3dx_surface_store(D3d9HostResource &resource, uint32_t face,
                                           uint32_t level, const recomp::d3d::D3d9Rect &rect,
                                           bool rect_present,
                                           const recomp::d3d::D3dxSurfaceData &snapshot,
                                           uint32_t filter);
    recomp::d3d::D3dxBytesResult d3dx_save_surface(D3d9HostResource &resource, uint32_t face,
                                                   uint32_t level,
                                                   const recomp::d3d::D3d9Rect &rect,
                                                   bool rect_present, uint32_t format);
    recomp::d3d::D3dxBytesResult d3dx_save_texture(D3d9HostResource &resource, uint32_t format);
    recomp::d3d::D3d9SurfaceDescriptorResult backbuffer_descriptor() const {
        return recomp::d3d::d3d9_backbuffer_descriptor(*value);
    }
    recomp::d3d::D3d9SurfaceDescriptorResult depthbuffer_descriptor() const {
        return recomp::d3d::d3d9_depthbuffer_descriptor(*value);
    }
    recomp::d3d::ValueResult number_of_swap_chains() const {
        return recomp::d3d::d3d9_number_of_swap_chains(*value);
    }
    recomp::d3d::ValueResult available_texture_mem() const {
        return recomp::d3d::d3d9_available_texture_mem(*value);
    }
    recomp::d3d::D3d9DisplayModeResult
    display_mode(const recomp::d3d::D3d9DisplayModeRequest &request) const {
        return recomp::d3d::d3d9_display_mode(*value, request);
    }
    recomp::d3d::D3d9PresentParams effective_present() const {
        return recomp::d3d::d3d9_effective_present(*value);
    }
    recomp::d3d::D3d9CapsResult caps() const {
        return recomp::d3d::d3d9_device_caps(*value);
    }
    recomp::d3d::Status set_render_state(uint32_t state, uint32_t v) {
        return recomp::d3d::d3d9_set_render_state(*value, state, v);
    }
    recomp::d3d::ValueResult render_state(uint32_t state) const {
        return recomp::d3d::d3d9_get_render_state(*value, state);
    }
    recomp::d3d::Status set_texture_stage_state(uint32_t stage, uint32_t state, uint32_t v) {
        return recomp::d3d::d3d9_set_texture_stage_state(*value, stage, state, v);
    }
    recomp::d3d::ValueResult texture_stage_state(uint32_t stage, uint32_t state) const {
        return recomp::d3d::d3d9_get_texture_stage_state(*value, stage, state);
    }
    recomp::d3d::Status set_sampler_state(uint32_t sampler, uint32_t state, uint32_t v) {
        return recomp::d3d::d3d9_set_sampler_state(*value, sampler, state, v);
    }
    recomp::d3d::ValueResult sampler_state(uint32_t sampler, uint32_t state) const {
        return recomp::d3d::d3d9_get_sampler_state(*value, sampler, state);
    }
    recomp::d3d::Status set_transform(uint32_t state, rust::Slice<const float> values) {
        return recomp::d3d::d3d9_set_transform(*value, state, values);
    }
    recomp::d3d::FloatValuesResult transform(uint32_t state) const {
        return recomp::d3d::d3d9_get_transform(*value, state);
    }
    recomp::d3d::Status multiply_transform(uint32_t state, rust::Slice<const float> values) {
        return recomp::d3d::d3d9_multiply_transform(*value, state, values);
    }
    recomp::d3d::Status set_viewport(const recomp::d3d::Viewport &viewport) {
        return recomp::d3d::d3d9_set_viewport(*value, viewport);
    }
    recomp::d3d::ViewportResult viewport() const {
        return recomp::d3d::d3d9_get_viewport(*value);
    }
    recomp::d3d::Status set_scissor(int32_t l, int32_t t, int32_t r, int32_t b) {
        return recomp::d3d::d3d9_set_scissor(*value, l, t, r, b);
    }
    recomp::d3d::D3d9ScissorResult scissor() const {
        return recomp::d3d::d3d9_get_scissor(*value);
    }
    recomp::d3d::Status set_material(rust::Slice<const float> values) {
        return recomp::d3d::d3d9_set_material(*value, values);
    }
    recomp::d3d::FloatValuesResult material() const {
        return recomp::d3d::d3d9_get_material(*value);
    }
    recomp::d3d::Status set_light(uint32_t index, uint32_t type, rust::Slice<const float> values) {
        return recomp::d3d::d3d9_set_light(*value, index, type, values);
    }
    recomp::d3d::D3d9LightResult light(uint32_t index) const {
        return recomp::d3d::d3d9_get_light(*value, index);
    }
    recomp::d3d::Status set_light_enable(uint32_t index, bool enable) {
        return recomp::d3d::d3d9_set_light_enable(*value, index, enable);
    }
    recomp::d3d::ValueResult light_enable(uint32_t index) const {
        return recomp::d3d::d3d9_get_light_enable(*value, index);
    }
    recomp::d3d::Status set_clip_plane(uint32_t index, rust::Slice<const float> values) {
        return recomp::d3d::d3d9_set_clip_plane(*value, index, values);
    }
    recomp::d3d::FloatValuesResult clip_plane(uint32_t index) const {
        return recomp::d3d::d3d9_get_clip_plane(*value, index);
    }
    recomp::d3d::Status set_texture(uint32_t stage, uint32_t id) {
        return recomp::d3d::d3d9_set_texture(*value, stage, id);
    }
    recomp::d3d::ValueResult texture(uint32_t stage) const {
        return recomp::d3d::d3d9_get_texture(*value, stage);
    }
    recomp::d3d::Status set_stream_source(uint32_t stream, uint32_t buffer, uint32_t offset,
                                          uint32_t stride) {
        return recomp::d3d::d3d9_set_stream_source(*value, stream, buffer, offset, stride);
    }
    recomp::d3d::D3d9StreamResult stream_source(uint32_t stream) const {
        return recomp::d3d::d3d9_get_stream_source(*value, stream);
    }
    recomp::d3d::Status set_stream_freq(uint32_t stream, uint32_t divider) {
        return recomp::d3d::d3d9_set_stream_freq(*value, stream, divider);
    }
    recomp::d3d::ValueResult stream_freq(uint32_t stream) const {
        return recomp::d3d::d3d9_get_stream_freq(*value, stream);
    }
    recomp::d3d::Status set_indices(uint32_t id) {
        return recomp::d3d::d3d9_set_indices(*value, id);
    }
    recomp::d3d::ValueResult indices() const {
        return recomp::d3d::d3d9_get_indices(*value);
    }
    recomp::d3d::ValueResult create_fvf_declaration(uint32_t fvf) {
        return recomp::d3d::d3d9_create_fvf_declaration(*value, fvf);
    }
    recomp::d3d::Status set_fvf_binding(uint32_t handle, uint32_t fvf) {
        return recomp::d3d::d3d9_set_fvf_binding(*value, handle, fvf);
    }
    recomp::d3d::ValueResult fvf() const {
        return recomp::d3d::d3d9_get_fvf(*value);
    }
    recomp::d3d::ValueResult
    create_vertex_declaration(rust::Slice<const recomp::d3d::D3d9VertexElement> elements) {
        return recomp::d3d::d3d9_create_vertex_declaration(*value, elements);
    }
    recomp::d3d::Status destroy_declaration(uint32_t handle) {
        return recomp::d3d::d3d9_destroy_declaration(*value, handle);
    }
    recomp::d3d::D3d9DeclarationResult declaration_elements(uint32_t handle) const {
        return recomp::d3d::d3d9_declaration_elements(*value, handle);
    }
    recomp::d3d::Status set_vertex_declaration(uint32_t handle) {
        return recomp::d3d::d3d9_set_vertex_declaration(*value, handle);
    }
    recomp::d3d::ValueResult vertex_declaration() const {
        return recomp::d3d::d3d9_get_vertex_declaration(*value);
    }
    recomp::d3d::ValueResult create_shader(bool pixel, rust::Slice<const uint32_t> code) {
        return recomp::d3d::d3d9_create_shader(*value, pixel, code);
    }
    recomp::d3d::Status destroy_shader(bool pixel, uint32_t handle) {
        return recomp::d3d::d3d9_destroy_shader(*value, pixel, handle);
    }
    recomp::d3d::Status set_shader(bool pixel, uint32_t handle) {
        return recomp::d3d::d3d9_set_shader(*value, pixel, handle);
    }
    recomp::d3d::ValueResult shader(bool pixel) const {
        return recomp::d3d::d3d9_get_shader(*value, pixel);
    }
    recomp::d3d::ValuesResult shader_function(bool pixel, uint32_t handle) const {
        return recomp::d3d::d3d9_shader_function(*value, pixel, handle);
    }
    recomp::d3d::ValueResult shader_size(bool pixel, uint32_t handle) const {
        return recomp::d3d::d3d9_shader_size(*value, pixel, handle);
    }
    recomp::d3d::Status set_shader_constants_f(bool pixel, uint32_t start,
                                               rust::Slice<const float> values) {
        return recomp::d3d::d3d9_set_shader_constants_f(*value, pixel, start, values);
    }
    recomp::d3d::FloatValuesResult shader_constants_f(bool pixel, uint32_t start,
                                                      uint32_t count) const {
        return recomp::d3d::d3d9_get_shader_constants_f(*value, pixel, start, count);
    }
    recomp::d3d::Status set_shader_constants_i(bool pixel, uint32_t start,
                                               rust::Slice<const int32_t> values) {
        return recomp::d3d::d3d9_set_shader_constants_i(*value, pixel, start, values);
    }
    recomp::d3d::ValuesResult shader_constants_i(bool pixel, uint32_t start, uint32_t count) const {
        return recomp::d3d::d3d9_get_shader_constants_i(*value, pixel, start, count);
    }
    recomp::d3d::Status set_shader_constants_b(bool pixel, uint32_t start,
                                               rust::Slice<const uint32_t> values) {
        return recomp::d3d::d3d9_set_shader_constants_b(*value, pixel, start, values);
    }
    recomp::d3d::ValuesResult shader_constants_b(bool pixel, uint32_t start, uint32_t count) const {
        return recomp::d3d::d3d9_get_shader_constants_b(*value, pixel, start, count);
    }
    recomp::d3d::ValueResult create_state_block(uint32_t type) {
        return recomp::d3d::d3d9_create_state_block(*value, type);
    }
    recomp::d3d::Status begin_state_block() {
        return recomp::d3d::d3d9_begin_state_block(*value);
    }
    recomp::d3d::ValueResult end_state_block() {
        return recomp::d3d::d3d9_end_state_block(*value);
    }
    recomp::d3d::Status capture_state_block(uint32_t handle) {
        return recomp::d3d::d3d9_capture_state_block(*value, handle);
    }
    recomp::d3d::Status apply_state_block(uint32_t handle) {
        return recomp::d3d::d3d9_apply_state_block(*value, handle);
    }
    recomp::d3d::Status destroy_state_block(uint32_t handle) {
        return recomp::d3d::d3d9_destroy_state_block(*value, handle);
    }
    recomp::d3d::D3d9BlockBindingsResult state_block_bindings(uint32_t handle) const {
        return recomp::d3d::d3d9_state_block_bindings(*value, handle);
    }
    recomp::d3d::D3d9BindingsResult current_bindings() const {
        return recomp::d3d::d3d9_current_bindings(*value);
    }

    std::unordered_map<uint32_t, uint32_t> shader_views;
    std::unordered_map<uint32_t, uint32_t> decl_views;
    std::set<uint32_t> fvf_decls;
    std::unordered_map<uint32_t, uint32_t> block_views;
    std::array<uint32_t, 38> live_bindings{};
    std::array<uint32_t, 38> recording_bindings{};
    bool recording_block = false;
    std::map<uint32_t, std::set<uint32_t>> block_refs;
};

struct D3dxConstantTableHost {
    rust::Box<recomp::d3d::D3dxConstantTable> value;

    D3dxConstantTableHost() : value(recomp::d3d::new_d3dx_constant_table()) {}
};

struct D3dxEffectPoolHost {
    rust::Box<recomp::d3d::D3dxEffectPool> value;
    std::set<uint32_t> texture_refs;

    D3dxEffectPoolHost() : value(recomp::d3d::new_d3dx_effect_pool()) {}
};

struct D3dxEffectHost {
    rust::Box<recomp::d3d::D3dxEffect> value;
    uint32_t device_id = 0;
    uint32_t pool_obj = 0;
    uint32_t state_manager = 0;
    bool restores_state = false;
    std::set<uint32_t> texture_refs;
    std::set<uint32_t> saved_refs;
    std::set<uint32_t> published_shaders;
    std::map<uint32_t, uint32_t> handle_tokens;
    std::map<uint32_t, uint32_t> handle_reverse;
    std::map<std::string, uint32_t> strings;
    std::map<uint32_t, std::pair<uint32_t, uint32_t>> pass_shaders;
    uint32_t dispatch_scratch = 0;
    uint32_t dispatch_scratch_size = 0;

    D3dxEffectHost() : value(recomp::d3d::new_d3dx_effect()) {}
};

struct D3d9Staging {
    uint32_t addr = 0;
    uint32_t size = 0;
    bool active = false;
    bool synchronized = false;
    bool writable = false;
    bool retain = false;
};

struct D3d9HostResource {
    rust::Box<recomp::d3d::D3d9Resource> value;
    std::map<uint64_t, D3d9Staging> staging;

    D3d9HostResource() : value(recomp::d3d::new_d3d9_resource()) {}

    recomp::d3d::Status create_buffer(const D3d9HostDevice &device,
                                      const recomp::d3d::D3d9BufferRequest &request) {
        return recomp::d3d::d3d9_create_buffer(*device.value, *value, request);
    }

    recomp::d3d::D3d9TextureCreateResult
    create_texture(const D3d9HostDevice &device, const recomp::d3d::D3d9TextureRequest &request) {
        return recomp::d3d::d3d9_create_texture(*device.value, *value, request);
    }

    recomp::d3d::D3dxCreateResult create_d3dx_texture(const D3d9HostDevice &device,
                                                      const recomp::d3d::D3dxCreateRequest &request,
                                                      rust::Slice<const uint8_t> bytes) {
        return recomp::d3d::d3d9_d3dx_create_texture(*device.value, *value, request, bytes);
    }

    recomp::d3d::Status create_surface(const D3d9HostDevice &device,
                                       const recomp::d3d::D3d9SurfaceRequest &request) {
        return recomp::d3d::d3d9_create_surface(*device.value, *value, request);
    }

    recomp::d3d::D3d9BufferDescriptorResult descriptor() const {
        return recomp::d3d::d3d9_buffer_descriptor(*value);
    }

    recomp::d3d::D3d9BufferLockResult
    lock_prepare(const recomp::d3d::D3d9BufferLockRequest &request) {
        return recomp::d3d::d3d9_buffer_lock_prepare(*value, request);
    }

    recomp::d3d::D3d9BufferLockResult
    lock_commit(const recomp::d3d::D3d9BufferLockRequest &request) {
        return recomp::d3d::d3d9_buffer_lock_commit(*value, request);
    }

    rust::Slice<const uint8_t> bytes() const {
        return recomp::d3d::d3d9_buffer_bytes(*value);
    }

    recomp::d3d::D3d9BufferUnlockResult unlock(rust::Slice<const uint8_t> bytes) {
        return recomp::d3d::d3d9_buffer_unlock(*value, bytes);
    }

    recomp::d3d::Status sync_locked(rust::Slice<const uint8_t> bytes) {
        return recomp::d3d::d3d9_buffer_sync_locked(*value, bytes);
    }

    recomp::d3d::ValueResult resource_type() const {
        return recomp::d3d::d3d9_resource_type(*value);
    }

    recomp::d3d::D3d9TextureDescriptorResult texture_descriptor() const {
        return recomp::d3d::d3d9_texture_descriptor(*value);
    }

    recomp::d3d::D3d9SurfaceDescriptorResult surface_descriptor() const {
        return recomp::d3d::d3d9_surface_descriptor(*value);
    }

    recomp::d3d::D3d9LevelDescriptorResult texture_level_descriptor(uint32_t face,
                                                                    uint32_t level) const {
        return recomp::d3d::d3d9_texture_level_descriptor(*value, face, level);
    }

    recomp::d3d::D3d9LevelDescriptorResult texture_volume_descriptor(uint32_t level) const {
        return recomp::d3d::d3d9_texture_volume_descriptor(*value, level);
    }

    recomp::d3d::D3d9LevelLockResult
    level_lock_prepare(const recomp::d3d::D3d9LevelLockRequest &request) {
        return recomp::d3d::d3d9_level_lock_prepare(*value, request);
    }

    recomp::d3d::D3d9LevelLockResult
    level_lock_commit(const recomp::d3d::D3d9LevelLockRequest &request) {
        return recomp::d3d::d3d9_level_lock_commit(*value, request);
    }

    rust::Slice<const uint8_t> level_bytes(uint32_t face, uint32_t level) const {
        return recomp::d3d::d3d9_level_bytes(*value, face, level);
    }

    recomp::d3d::D3d9BufferUnlockResult level_unlock(uint32_t face, uint32_t level,
                                                     rust::Slice<const uint8_t> bytes) {
        return recomp::d3d::d3d9_level_unlock(*value, face, level, bytes);
    }

    recomp::d3d::ValueResult set_priority(uint32_t priority) {
        return recomp::d3d::d3d9_set_priority(*value, priority);
    }
    recomp::d3d::ValueResult priority() const {
        return recomp::d3d::d3d9_priority(*value);
    }
    recomp::d3d::ValueResult set_lod(uint32_t lod) {
        return recomp::d3d::d3d9_set_lod(*value, lod);
    }
    recomp::d3d::ValueResult lod() const {
        return recomp::d3d::d3d9_lod(*value);
    }
    recomp::d3d::Status set_private_data(uint32_t view, rust::Slice<const uint8_t> guid,
                                         rust::Slice<const uint8_t> data, uint32_t flags) {
        return recomp::d3d::d3d9_set_private_data(*value, view, guid, data, flags);
    }
    recomp::d3d::D3d9PrivateDataResult get_private_data(uint32_t view,
                                                        rust::Slice<const uint8_t> guid) const {
        return recomp::d3d::d3d9_get_private_data(*value, view, guid);
    }
    recomp::d3d::Status free_private_data(uint32_t view, rust::Slice<const uint8_t> guid) {
        return recomp::d3d::d3d9_free_private_data(*value, view, guid);
    }
    recomp::d3d::Status add_dirty_rect(uint32_t face, uint32_t level, bool full_region,
                                       const recomp::d3d::D3d9LockRect &rect) {
        return recomp::d3d::d3d9_add_dirty_rect(*value, face, level, full_region, rect);
    }
    recomp::d3d::Status add_dirty_box(uint32_t level, bool full_region,
                                      const recomp::d3d::D3d9LockBox &bounds) {
        return recomp::d3d::d3d9_add_dirty_box(*value, level, full_region, bounds);
    }
    recomp::d3d::Status preload() {
        return recomp::d3d::d3d9_preload(*value);
    }
    recomp::d3d::Status set_autogen_filter(uint32_t filter) {
        return recomp::d3d::d3d9_set_autogen_filter(*value, filter);
    }
    recomp::d3d::ValueResult autogen_filter() const {
        return recomp::d3d::d3d9_get_autogen_filter(*value);
    }
    recomp::d3d::Status generate_mips() {
        return recomp::d3d::d3d9_generate_mips(*value);
    }
    recomp::d3d::Status upload_buffer() {
        return recomp::d3d::d3d9_upload_buffer(*value);
    }
    recomp::d3d::Status upload_level(uint32_t face, uint32_t level) {
        return recomp::d3d::d3d9_upload_level(*value, face, level);
    }
    recomp::d3d::Status readback_level(uint32_t face, uint32_t level) {
        return recomp::d3d::d3d9_readback_level(*value, face, level);
    }
};

inline recomp::d3d::Status D3d9HostDevice::register_resource(uint32_t id,
                                                             const D3d9HostResource &resource) {
    return recomp::d3d::d3d9_register_resource(*value, id, *resource.value);
}

inline recomp::d3d::Status
D3d9HostDevice::prepare_resource_reset(uint32_t id, const D3d9HostResource &resource) {
    return recomp::d3d::d3d9_prepare_resource_reset(*value, id, *resource.value);
}

inline void D3d9HostDevice::commit_resource_reset(uint32_t id, D3d9HostResource &resource) {
    recomp::d3d::d3d9_commit_resource_reset(*value, id, *resource.value);
}

inline recomp::d3d::Status D3d9HostDevice::register_resource_view(uint32_t id,
                                                                  const D3d9HostResource &resource,
                                                                  uint32_t face, uint32_t level,
                                                                  bool volume) {
    return recomp::d3d::d3d9_register_resource_view(*value, id, *resource.value, face, level,
                                                    volume);
}

inline recomp::d3d::Status
D3d9HostDevice::update_surface(D3d9HostResource &source, uint32_t src_face, uint32_t src_level,
                               rust::Slice<const recomp::d3d::D3d9Rect> src_rect,
                               D3d9HostResource &destination, uint32_t dst_face, uint32_t dst_level,
                               int32_t dst_x, int32_t dst_y) {
    return recomp::d3d::d3d9_update_surface(*value, *source.value, src_face, src_level, src_rect,
                                            *destination.value, dst_face, dst_level, dst_x, dst_y);
}

inline recomp::d3d::Status D3d9HostDevice::update_texture(D3d9HostResource &source,
                                                          D3d9HostResource &destination) {
    return recomp::d3d::d3d9_update_texture(*value, *source.value, *destination.value);
}

inline recomp::d3d::Status D3d9HostDevice::get_render_target_data(uint32_t src,
                                                                  D3d9HostResource &destination,
                                                                  uint32_t dst_face,
                                                                  uint32_t dst_level) {
    return recomp::d3d::d3d9_get_render_target_data(*value, src, *destination.value, dst_face,
                                                    dst_level);
}

inline recomp::d3d::D3dxSurfaceReadResult
D3d9HostDevice::d3dx_surface_read(D3d9HostResource &resource, uint32_t face, uint32_t level,
                                  const recomp::d3d::D3d9Rect &rect, bool rect_present,
                                  uint32_t color_key) {
    return recomp::d3d::d3d9_d3dx_surface_read(*value, *resource.value, face, level, rect,
                                               rect_present, color_key);
}

inline recomp::d3d::Status
D3d9HostDevice::d3dx_surface_store(D3d9HostResource &resource, uint32_t face, uint32_t level,
                                   const recomp::d3d::D3d9Rect &rect, bool rect_present,
                                   const recomp::d3d::D3dxSurfaceData &snapshot, uint32_t filter) {
    return recomp::d3d::d3d9_d3dx_surface_store(*value, *resource.value, face, level, rect,
                                                rect_present, snapshot, filter);
}

inline recomp::d3d::D3dxBytesResult
D3d9HostDevice::d3dx_save_surface(D3d9HostResource &resource, uint32_t face, uint32_t level,
                                  const recomp::d3d::D3d9Rect &rect, bool rect_present,
                                  uint32_t format) {
    return recomp::d3d::d3d9_d3dx_save_surface(*value, *resource.value, face, level, rect,
                                               rect_present, format);
}

inline recomp::d3d::D3dxBytesResult D3d9HostDevice::d3dx_save_texture(D3d9HostResource &resource,
                                                                      uint32_t format) {
    return recomp::d3d::d3d9_d3dx_save_texture(*value, *resource.value, format);
}

inline recomp::d3d::D3dxImageInfoResult d3dx_image_info(rust::Slice<const uint8_t> bytes) {
    return recomp::d3d::d3d9_d3dx_image_info(bytes);
}

struct D3dHostStorage {
    rust::Box<recomp::d3d::Storage> value;

    D3dHostStorage() : value(recomp::d3d::new_storage()) {}

    rust::Slice<const uint8_t> bytes() const {
        return recomp::d3d::storage_bytes(*value);
    }

    rust::Slice<uint8_t> bytes_mut() {
        return recomp::d3d::storage_bytes_mut(*value);
    }
};

struct D3dHostResource {
    rust::Box<recomp::d3d::Resource> value;

    D3dHostResource() : value(recomp::d3d::new_resource()) {}

    void set_flags(uint32_t usage, uint32_t pool, bool depth, bool gpu_target, uint32_t buffer_fvf,
                   uint32_t index_format, uint32_t buffer_usage, uint32_t buffer_pool) {
        recomp::d3d::set_resource_flags(*value, usage, pool, depth, gpu_target, buffer_fvf,
                                        index_format, buffer_usage, buffer_pool);
    }

    recomp::d3d::ResourceInfo info() const {
        return recomp::d3d::resource_info(*value);
    }

    recomp::d3d::ValueResult set_lod(uint32_t lod) {
        return recomp::d3d::set_resource_lod(*value, lod);
    }
    recomp::d3d::ValueResult set_priority(uint32_t priority) {
        return recomp::d3d::set_resource_priority(*value, priority);
    }
    void set_generation(uint64_t generation) {
        recomp::d3d::set_resource_generation(*value, generation);
    }
    uint64_t generation() const {
        return recomp::d3d::resource_generation(*value);
    }

    recomp::d3d::Status set_level(uint32_t level, uint32_t width, uint32_t height, uint32_t pitch,
                                  uint32_t byte_len, uint64_t generation) {
        return recomp::d3d::set_resource_level(*value, level, width, height, pitch, byte_len,
                                               generation);
    }

    recomp::d3d::ResourceLevelResult level(uint32_t index) const {
        return recomp::d3d::resource_level(*value, index);
    }

    recomp::d3d::ResourceConfigurationResult configure_texture(uint32_t width, uint32_t height,
                                                               uint32_t levels, uint32_t format) {
        return recomp::d3d::configure_texture_resource(*value, width, height, levels, format);
    }

    uint64_t bump_generation() {
        return recomp::d3d::bump_resource_generation(*value);
    }
    recomp::d3d::ResourceGenerationResult bump_level_generation(uint32_t level) {
        return recomp::d3d::bump_resource_level_generation(*value, level);
    }
};

struct D3dHostDevice7State {
    rust::Box<recomp::d3d::Device7State> value;
    uint32_t d3d_obj = 0;
    uint8_t device_guid[16] = {};
    bool tnl = false;

    D3dHostDevice7State() : value(recomp::d3d::new_device7_state()) {}
};

inline bool d3d_bridge_ok(const recomp::d3d::Status &status) {
    return status.code == recomp::d3d::StatusCode::Ok;
}
