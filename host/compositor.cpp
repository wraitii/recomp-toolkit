#include "compositor.h"
#include "ui_layer.h"
#include "../graphics/compositor/include/compositor.h"

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <limits>
#include <memory>
#include <vector>

namespace {
struct CompositorDeleter {
    void operator()(RcCompositor *value) const {
        rc_compositor_free(value);
    }
};
struct ResultDeleter {
    void operator()(RcResult *value) const {
        rc_result_free(value);
    }
};
using Result = std::unique_ptr<RcResult, ResultDeleter>;
using CompositeRect = RcRect;
RcCompositor *compositor() {
    static const std::unique_ptr<RcCompositor, CompositorDeleter> value(rc_compositor_new());
    if (!value) {
        fprintf(stderr, "compositor: Rust state allocation failed\n");
        abort();
    }
    return value.get();
}
bool valid(const CompositorInput *in) {
    return in && in->guest_w > 0 && in->guest_h > 0 && in->drawable_w > 0 && in->drawable_h > 0;
}
RcElement element(const UiElement &e) {
    return {e.id, e.last_seq, e.x, e.y, e.w, e.h, uint8_t(e.is_cursor), uint8_t(e.is_hud)};
}
RcInput input(const CompositorInput &in) {
    return {int32_t(in.cls),
            in.guest_w,
            in.guest_h,
            in.drawable_w,
            in.drawable_h,
            in.scale_override,
            {in.scene.scale_x, in.scene.scale_y, in.scene.offset_x, in.scene.offset_y,
             in.scene.domain_w},
            uint8_t(in.legacy),
            uint8_t(in.classic),
            uint8_t(in.narrow),
            uint8_t(bool(in.world)),
            uint8_t(bool(in.overlay)),
            uint8_t(bool(in.legacy_frame)),
            uint8_t(bool(in.settings_page))};
}
Result build(const CompositorInput &in, uint32_t outputs) {
    std::vector<RcElement> elements;
    if (in.ui) {
        elements.reserve(in.ui->elements.size());
        for (const auto &e : in.ui->elements)
            elements.push_back(element(e));
    }
    const auto request = input(in);
    Result result(rc_build(compositor(), &request, elements.data(), elements.size(), outputs));
    if (!result) {
        fprintf(stderr, "compositor: Rust composition planning failed\n");
        abort();
    }
    return result;
}
enum Blend { Opaque, Straight, Premultiplied };
struct Quad {
    float rect[4], uv[4];
    float drawable[2];
    uint32_t opaque, pad;
};
// One pipeline per blend convention and target format; the device caches by
// the render state, so asking every compose is free.
gpu::Pipeline pipeline(gpu::Device *device, gpu::Format format, Blend blend) {
    gpu::RenderState state;
    state.color_format[0] = format;
    state.color_count = 1;
    state.blend_enabled = blend != Opaque;
    state.src_rgb = blend == Straight ? gpu::Blend::SrcAlpha : gpu::Blend::One;
    state.dst_rgb = gpu::Blend::OneMinusSrcAlpha;
    state.src_alpha = gpu::Blend::One;
    state.dst_alpha = gpu::Blend::OneMinusSrcAlpha;
    return device->render_pipeline("compositor", state);
}
struct Pass {
    gpu::Device *device;
    gpu::CommandBuffer cb;
    gpu::Format format;
    std::vector<gpu::Texture> transient; // destroyed when cb completes
};
// Encode a textured compositor quad with explicit destination, UVs and alpha convention.
// Reject empty or wholly offscreen rectangles before issuing draw commands.
void draw(Pass &pass, const CompositorInput &in, gpu::Texture tex, CompositeRect dst,
          CompositeRect uv, Blend blend) {
    if (!tex || dst.w <= 0 || dst.h <= 0 || dst.x >= in.drawable_w || dst.y >= in.drawable_h ||
        dst.x + dst.w <= 0 || dst.y + dst.h <= 0)
        return;
    gpu::Pipeline p = pipeline(pass.device, pass.format, blend);
    if (!p)
        return;
    Quad q{{float(dst.x), float(dst.y), float(dst.w), float(dst.h)},
           {float(uv.x), float(uv.y), float(uv.w), float(uv.h)},
           {float(in.drawable_w), float(in.drawable_h)},
           uint32_t(blend == Opaque),
           0};
    pass.device->set_pipeline(pass.cb, p);
    pass.device->set_bytes(pass.cb, gpu::Stage::Vertex, 0, &q, sizeof q);
    pass.device->set_bytes(pass.cb, gpu::Stage::Fragment, 0, &q, sizeof q);
    pass.device->set_texture(pass.cb, gpu::Stage::Fragment, 0, tex);
    gpu::SamplerState sampler;
    if (tex != in.settings_page)
        sampler.mag = sampler.min = in.presentation_filter;
    pass.device->set_sampler(pass.cb, gpu::Stage::Fragment, 0, sampler);
    pass.device->draw(pass.cb, gpu::Primitive::TriangleStrip, 0, 4);
}
gpu::Texture upload(Pass &pass, const UiElement &e) {
    if (e.w <= 0 || e.h <= 0)
        return {};
    const size_t n = size_t(e.w) * size_t(e.h);
    if (n > std::numeric_limits<size_t>::max() / 4 || e.rgba.size() < n * 4 || e.mask.size() < n)
        return {};
    std::vector<uint8_t> pixels(e.rgba.begin(), e.rgba.begin() + n * 4);
    for (size_t i = 0; i < n; ++i)
        if (!e.mask[i])
            pixels[4 * i + 3] = 0;
    gpu::Texture tex = pass.device->create_texture(
        {e.w, e.h, gpu::Format::RGBA8, gpu::UsageSampled | gpu::UsageCpu, 1});
    if (!tex) {
        fprintf(stderr, "compositor: UI texture allocation failed (%dx%d)\n", e.w, e.h);
        return {};
    }
    pass.device->upload(tex, {0, 0, e.w, e.h}, pixels.data(), e.w * 4);
    pass.transient.push_back(tex);
    return tex;
}

} // namespace

int compositor_ui_scale(int drawable_h, int guest_h, int override_) {
    return rc_ui_scale(drawable_h, guest_h, override_);
}
Anchor compositor_default_anchor(const UiElement *e, int guest_w, int guest_h) {
    const auto request = e ? element(*e) : RcElement{};
    Anchor result;
    rc_default_anchor(e ? &request : nullptr, guest_w, guest_h, &result.h, &result.v);
    return result;
}
void compositor_set_anchor(uint64_t id, Anchor a) {
    rc_set_anchor(compositor(), id, a.h, a.v);
}
void compositor_clear_anchor(uint64_t id) {
    rc_clear_anchor(compositor(), id);
}
void compositor_set_pointer_position(bool valid, int32_t x, int32_t y) {
    rc_set_pointer(compositor(), uint8_t(valid), x, y);
}
bool compositor_element_rect_on_drawable(const CompositorInput *in, uint64_t id, int *x, int *y,
                                         int *w, int *h) {
    if (!valid(in) || !in->ui)
        return false;
    const auto request = input(*in);
    for (const auto &e : in->ui->elements)
        if (e.id == id) {
            const auto item = element(e);
            RcRect rect;
            if (!rc_element_rect(compositor(), &request, &item, &rect))
                return false;
            if (x)
                *x = int(rect.x);
            if (y)
                *y = int(rect.y);
            if (w)
                *w = int(rect.w);
            if (h)
                *h = int(rect.h);
            return true;
        }
    return false;
}
uint32_t compositor_element_ids(const UiFrame *ui, uint64_t *out, uint32_t max) {
    std::vector<const UiElement *> elements;
    if (ui)
        for (const auto &e : ui->elements)
            elements.push_back(&e);
    std::stable_sort(elements.begin(), elements.end(),
                     [](const auto *a, const auto *b) { return a->last_seq < b->last_seq; });
    const size_t count = std::min(elements.size(), size_t(UINT32_MAX));
    if (out)
        for (size_t i = 0; i < std::min(count, size_t(max)); ++i)
            out[i] = elements[i]->id;
    return uint32_t(count);
}
bool compositor_resolve_scene(CompositorSceneHistory *history, CompositorInput *in,
                              bool had_draws) {
    if (!history || !in)
        return false;
    const int domain = in->scene.domain_w > 0 ? in->scene.domain_w : in->guest_w;
    const bool changed = history->guest_w != in->guest_w || history->guest_h != in->guest_h ||
                         history->domain_w != domain || history->classic != in->classic;
    if (in->cls != HOST_SCREEN_GAMEPLAY || in->legacy || changed) {
        history->world = {};
        history->overlay = {};
        if (in->cls != HOST_SCREEN_GAMEPLAY || in->legacy)
            return false;
    }
    if (had_draws) {
        history->world = in->world;
        history->overlay = in->overlay;
        history->guest_w = in->guest_w;
        history->guest_h = in->guest_h;
        history->domain_w = domain;
        history->classic = in->classic;
        return false;
    }
    in->world = history->world;
    in->overlay = history->overlay;
    if (!history->world)
        return false;
    ++history->scene_reused;
    return true;
}

void compositor_compose(gpu::Device *device, const CompositorInput *in, gpu::Texture out,
                        gpu::CommandBuffer cb) {
    if (!device || !valid(in) || !out || !cb)
        return;
    const gpu::TextureDesc desc = device->describe(out);
    if (desc.width != in->drawable_w || desc.height != in->drawable_h)
        return;
    const auto plan = build(*in, RC_OUTPUT_DRAWS);
    Pass pass{device, cb, desc.format, {}};
    gpu::RenderPass rp;
    rp.color_count = 1;
    rp.color[0].texture = out;
    rp.color[0].load = gpu::Load::Clear;
    rp.color[0].store = gpu::Store::Store;
    rp.color[0].clear[0] = rp.color[0].clear[1] = rp.color[0].clear[2] = 0;
    rp.color[0].clear[3] = 1;
    size_t count = 0;
    const auto *draws = rc_result_draws(plan.get(), &count);
    std::vector<gpu::CompositionDraw> commands;
    commands.reserve(count);
    for (size_t i = 0; i < count; ++i) {
        const auto &command = draws[i];
        gpu::Texture texture;
        switch (command.source) {
        case RC_WORLD:
            texture = in->world;
            break;
        case RC_UI:
            texture = upload(pass, in->ui->elements.at(command.element_index));
            break;
        case RC_OVERLAY:
            texture = in->overlay;
            break;
        case RC_LEGACY:
            texture = in->legacy_frame;
            break;
        case RC_SETTINGS:
            texture = in->settings_page;
            break;
        default:
            fprintf(stderr, "compositor: unknown Rust image source %u\n", command.source);
            abort();
        }
        if (texture)
            commands.push_back({texture.id,
                                {command.dst.x, command.dst.y, command.dst.w, command.dst.h},
                                {command.uv.x, command.uv.y, command.uv.w, command.uv.h},
                                command.blend,
                                uint32_t(texture == in->settings_page ? gpu::Filter::Nearest
                                                                      : in->presentation_filter)});
    }
    if (!device->compose_plan(cb, out, in->drawable_w, in->drawable_h, commands.data(),
                              commands.size())) {
        device->begin_render_pass(cb, rp);
        for (const auto &command : commands)
            draw(pass, *in, {command.texture},
                 {command.rect[0], command.rect[1], command.rect[2], command.rect[3]},
                 {command.uv[0], command.uv[1], command.uv[2], command.uv[3]},
                 Blend(command.blend));
        device->end_render_pass(cb);
    }
    if (!pass.transient.empty()) {
        auto transient = std::make_shared<std::vector<gpu::Texture>>(std::move(pass.transient));
        device->on_complete(cb, [device, transient](gpu::CommandStatus, double) {
            for (gpu::Texture texture : *transient)
                device->destroy(texture);
        });
    }
}

LayoutSnapshot compositor_layout_snapshot(const CompositorInput *in) {
    LayoutSnapshot result;
    if (!valid(in))
        return result;
    const auto plan = build(*in, RC_OUTPUT_LAYOUT);
    const auto layout = rc_result_layout(plan.get());
    result.drawable_w = in->drawable_w;
    result.drawable_h = in->drawable_h;
    result.guest_w = in->guest_w;
    result.guest_h = in->guest_h;
    result.ui_scale = layout.ui_scale;
    result.scene = {layout.scene.scale_x, layout.scene.scale_y, layout.scene.offset_x,
                    layout.scene.offset_y, layout.scene.domain_w};
    result.cls = in->cls;
    result.scale_override = in->scale_override;
    result.legacy = in->legacy;
    result.classic = in->classic;
    result.narrow = in->narrow;
    size_t count = 0;
    const auto *elements = rc_result_elements(plan.get(), &count);
    result.elements.reserve(count);
    for (size_t i = 0; i < count; ++i) {
        const auto &e = elements[i].element;
        const auto &rect = elements[i].drawable;
        result.elements.push_back({e.id,
                                   e.last_seq,
                                   {e.x, e.y, e.w, e.h},
                                   {int(rect.x), int(rect.y), int(rect.w), int(rect.h)},
                                   bool(e.is_cursor)});
    }
    return result;
}

LayoutSnapshot compositor_resize_layout(const LayoutSnapshot &layout, int w, int h) {
    if (w <= 0 || h <= 0 || layout.drawable_w <= 0 || layout.drawable_h <= 0)
        return layout;
    UiFrame ui{};
    for (const auto &e : layout.elements) {
        UiElement item{};
        item.id = e.id;
        item.last_seq = e.last_seq;
        item.x = e.guest.x;
        item.y = e.guest.y;
        item.w = e.guest.w;
        item.h = e.guest.h;
        item.is_cursor = e.is_cursor;
        ui.elements.push_back(std::move(item));
    }
    CompositorInput in{};
    in.ui = &ui;
    in.cls = layout.cls;
    in.guest_w = layout.guest_w;
    in.guest_h = layout.guest_h;
    in.drawable_w = w;
    in.drawable_h = h;
    in.scale_override = layout.scale_override;
    in.legacy = layout.legacy;
    in.classic = layout.classic;
    in.narrow = layout.narrow;
    in.scene = layout.scene;
    in.scene.scale_x *= double(w) / layout.drawable_w;
    in.scene.offset_x *= double(w) / layout.drawable_w;
    in.scene.scale_y *= double(h) / layout.drawable_h;
    in.scene.offset_y *= double(h) / layout.drawable_h;
    auto result = compositor_layout_snapshot(&in);
    result.frame_id = layout.frame_id;
    result.viewport = layout.viewport;
    result.viewport_style = layout.viewport_style;
    result.guest_origin_x = layout.guest_origin_x;
    result.guest_origin_y = layout.guest_origin_y;
    return result;
}
