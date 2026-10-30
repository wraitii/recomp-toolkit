#pragma once
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct RcCompositor RcCompositor;
typedef struct RcRect {
    double x, y, w, h;
} RcRect;
typedef struct RcScene {
    float scale_x, scale_y, offset_x, offset_y;
    int32_t domain_w;
} RcScene;
typedef struct RcInput {
    int32_t cls, guest_w, guest_h, drawable_w, drawable_h, scale_override;
    RcScene scene;
    uint8_t legacy, classic, narrow, has_world, has_overlay, has_legacy, has_settings;
} RcInput;
typedef struct RcElement {
    uint64_t id;
    uint32_t last_seq;
    int32_t x, y, w, h;
    uint8_t is_cursor, is_hud;
} RcElement;
enum RcSource { RC_WORLD, RC_UI, RC_OVERLAY, RC_LEGACY, RC_SETTINGS };
enum RcBlend { RC_OPAQUE, RC_STRAIGHT, RC_PREMULTIPLIED };
typedef struct RcDraw {
    uint32_t source, element_index, blend;
    RcRect dst, uv;
} RcDraw;
typedef struct RcLayoutElement {
    RcElement element;
    RcRect drawable;
} RcLayoutElement;
typedef struct RcLayout {
    RcScene scene;
    int32_t ui_scale;
} RcLayout;
enum RcOutput { RC_OUTPUT_LAYOUT = 1, RC_OUTPUT_DRAWS = 2 };
typedef struct RcResult RcResult;
RcCompositor *rc_compositor_new(void);
void rc_compositor_free(RcCompositor *c);
void rc_set_anchor(RcCompositor *c, uint64_t id, int8_t h, int8_t v);
void rc_clear_anchor(RcCompositor *c, uint64_t id);
void rc_set_pointer(RcCompositor *c, uint8_t valid, int32_t x, int32_t y);
int32_t rc_ui_scale(int32_t drawable_h, int32_t guest_h, int32_t override_value);
void rc_default_anchor(const RcElement *e, int32_t guest_w, int32_t guest_h, int8_t *h, int8_t *v);
uint8_t rc_element_rect(RcCompositor *c, const RcInput *input, const RcElement *element,
                        RcRect *rect);
RcResult *rc_build(RcCompositor *c, const RcInput *input, const RcElement *elements, size_t count,
                   uint32_t outputs);
void rc_result_free(RcResult *result);
RcLayout rc_result_layout(const RcResult *result);
const RcLayoutElement *rc_result_elements(const RcResult *result, size_t *count);
const RcDraw *rc_result_draws(const RcResult *result, size_t *count);
#ifdef __cplusplus
}
#endif
