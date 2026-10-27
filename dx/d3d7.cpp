#include "com.h"
#include "ddraw.h"
#include "dx.h"
#include "dxt_decode.h"
#include "dxtypes.h"
#include "host_api.h"
#include "../platform/os.h"
#include "../platform/profile_markers.h"
#include "../runtime/display_seam.h"
#include "../runtime/guest.h"
#include "../runtime/memory.h"

#include "d3d_bridge_helpers.h"

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <algorithm>
#include <array>
#include <bit>
#include <map>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

uint32_t ddraw_surface_revision(uint32_t id);

// Reconciles a readback into the guest surface. `dst` is the host pointer for
// the surface's first row, `pitch` its byte pitch. The store is a shim copy
// (from the GPU target), so it addresses memory directly rather than through
// the guest store hook: the hook only exists to narrow a guest Lock's write
// diff, and a completed Lock cannot be open for the surface being written back
// (Flip rejects a locked chain, and Lock flushes before it opens its shadow).
bool d3d7_store_rgba_surface(uint8_t *dst, uint32_t pitch, uint32_t bpp, uint32_t w, uint32_t h,
                             const uint8_t *rgba) {
    if (!dst || !rgba || !w || !h)
        return false;
    if (bpp == 16) {
        // Same truncating conversion as d3d7_rgb888_to_rgb565, written over
        // whole 32-bit pixels so the compiler vectorizes it (this runs for
        // every presented frame).
        for (uint32_t y = 0; y < h; ++y) {
            const uint8_t *__restrict src = rgba + (size_t)y * w * 4;
            uint8_t *__restrict row = dst + (size_t)y * pitch;
            for (uint32_t x = 0; x < w; ++x) {
                uint32_t px;
                memcpy(&px, src + x * 4, 4); // R, G, B, A in memory order
                uint16_t v =
                    (uint16_t)((((px & 0xff) >> 3) << 11) | ((((px >> 8) & 0xff) >> 2) << 5) |
                               (((px >> 16) & 0xff) >> 3));
                memcpy(row + x * 2, &v, 2);
            }
        }
        return true;
    }
    if (bpp == 32) {
        for (uint32_t y = 0; y < h; ++y) {
            const uint8_t *src = rgba + (size_t)y * w * 4;
            uint8_t *row = dst + (size_t)y * pitch;
            for (uint32_t x = 0; x < w; ++x) {
                row[x * 4 + 0] = src[x * 4 + 2]; // B
                row[x * 4 + 1] = src[x * 4 + 1]; // G
                row[x * 4 + 2] = src[x * 4 + 0]; // R
                row[x * 4 + 3] = 0;
            }
        }
        return true;
    }
    return false;
}

namespace {

// ---------------------------------------------------------------- IIDs
// The device class GUIDs. Both are offered because both are real host devices:
// the HAL device delegates vertex processing to the engine (the engine submits
// XYZRHW vertices and never calls CreateVertexBuffer when its own HardwareTnL
// option is off), while the TnL device exposes D3DDEVCAPS_HWTRANSFORMANDLIGHT.
// GetCaps reports exactly the class that was created, so the engine's own
// detection (it compares deviceGUID against IID_IDirect3DTnLHalDevice) sees a
// consistent picture. The game's default config leaves HardwareTnL unset, so
// the default device on this host is the HAL one.
#define GUID_BYTES(a, b, c, d0, d1, d2, d3, d4, d5, d6, d7)                                        \
    {(uint8_t)((a) & 0xff),                                                                        \
     (uint8_t)(((a) >> 8) & 0xff),                                                                 \
     (uint8_t)(((a) >> 16) & 0xff),                                                                \
     (uint8_t)(((a) >> 24) & 0xff),                                                                \
     (uint8_t)((b) & 0xff),                                                                        \
     (uint8_t)(((b) >> 8) & 0xff),                                                                 \
     (uint8_t)((c) & 0xff),                                                                        \
     (uint8_t)(((c) >> 8) & 0xff),                                                                 \
     d0,                                                                                           \
     d1,                                                                                           \
     d2,                                                                                           \
     d3,                                                                                           \
     d4,                                                                                           \
     d5,                                                                                           \
     d6,                                                                                           \
     d7}

const uint8_t IID_IDirect3D7_[16] =
    GUID_BYTES(0xF5049E77, 0x4861, 0x11D2, 0xA4, 0x07, 0x00, 0xA0, 0xC9, 0x06, 0x29, 0xA8);
const uint8_t IID_IDirect3DHALDevice_[16] =
    GUID_BYTES(0x84E63DE0, 0x46AA, 0x11CF, 0x81, 0x6F, 0x00, 0x00, 0xC0, 0x20, 0x15, 0x6E);
const uint8_t IID_IDirect3DTnLHalDevice_[16] =
    GUID_BYTES(0xF5049E78, 0x4861, 0x11D2, 0xA4, 0x07, 0x00, 0xA0, 0xC9, 0x06, 0x29, 0xA8);
const uint8_t IID_IDirect3DVertexBuffer7_[16] =
    GUID_BYTES(0xF5049E7D, 0x4861, 0x11D2, 0xA4, 0x07, 0x00, 0xA0, 0xC9, 0x06, 0x29, 0xA8);

bool guid_eq(uint32_t guest_addr, const uint8_t *want) {
    return guest_addr && gm_valid(guest_addr, 16) && memcmp(gm_ptr(guest_addr), want, 16) == 0;
}

// ---------------------------------------------------------------- scratch
uint32_t g_scratch = 0, g_scratch_size = 0;
uint32_t scratch(uint32_t n) {
    if (g_scratch_size < n) {
        if (g_scratch)
            heap_free(g_scratch);
        g_scratch = heap_alloc(n, true, 16);
        g_scratch_size = g_scratch ? n : 0;
    }
    if (g_scratch)
        memset(gm_ptr(g_scratch), 0, g_scratch_size);
    return g_scratch;
}

// ---------------------------------------------------------------- loud aborts
// Every slot that would need rendering lands here, naming itself. Returning a
// success would let the engine believe work happened; returning an error would
// be swallowed by callers that do not check (the engine ignores Clear's and
// BeginScene's HRESULTs), so the only honest option is to stop.
[[noreturn]] void needs_render(const char *iface, const char *method) {
    LOGW("d3d7: %s::%s is not implemented (needs rendering); stopping", iface, method);
    fflush(stderr);
    abort();
}

// ---------------------------------------------------------------- accessors
ComObj *this_d3d7(X86 *c) {
    ComObj *o = com_this_arg(c);
    return (o && o->kind == K_DDRAW) ? o : nullptr;
}
ComObj *this_device7(X86 *c) {
    ComObj *o = com_this_arg(c);
    return (o && o->kind == K_D3D7DEVICE && o->d3d7) ? o : nullptr;
}
ComObj *this_vb7(X86 *c) {
    ComObj *o = com_this_arg(c);
    return (o && o->kind == K_D3D7VB) ? o : nullptr;
}

// ---------------------------------------------------------------- FVF stride
// Byte width of one vertex for the FVF layouts the engine uses. Zero means the
// layout is not decoded here and CreateVertexBuffer refuses it rather than
// guessing a size the engine would then Lock past.
uint32_t fvf_stride(uint32_t fvf) {
    uint32_t n = 0;
    switch (fvf & 0x00e) { // D3DFVF_POSITION_MASK
    case 0x002:            // XYZ
        n += 12;
        break;
    case 0x004: // XYZRHW
        n += 16;
        break;
    default:
        return 0; // XYZB* blend-weight layouts are not used by this game.
    }
    if (fvf & 0x010)
        n += 12; // NORMAL
    if (fvf & 0x020)
        n += 4; // PSIZE
    if (fvf & 0x040)
        n += 4; // DIFFUSE
    if (fvf & 0x080)
        n += 4; // SPECULAR
    uint32_t texcount = (fvf >> 8) & 0xf;
    for (uint32_t i = 0; i < texcount; ++i) {
        uint32_t size = (fvf >> (16 + i * 2)) & 0x3;
        n += size == 0 ? 8 : size * 4; // 0 means the SDK's default of 2 floats
    }
    return n;
}

// ---------------------------------------------------------------- D3D7 trace
// Env-gated diagnostic for the draw path. RECOMP_TRACE_D3D7=1 turns it on;
// RECOMP_TRACE_D3D7_FRAMES=a-b limits it to a 1-based inclusive frame range
// ("a-" and "-b" open one end). Two optional knobs bound the bulky parts:
// RECOMP_TRACE_D3D7_VERTEX_FRAMES (default 3) is how many leading frames dump
// the first vertices of the frame's biggest draw, and
// RECOMP_TRACE_D3D7_MATRIX_FRAMES (default 3) is how many leading frames print
// the full 4x4 world/view/projection after a change. Later frames print one
// compact line per transform change instead.
//
// RECOMP_TRACE_D3D7_SMALL=1 is a separate opt-in for the flat UI quads the
// aggregate hides: every draw with vcount <= 8 is decoded vertex by vertex
// (position, diffuse AARRGGBB, specular) alongside the key render state
// (blend, alpha test, z state, texture-stage 0 ops/args). Only untextured
// draws are included by default; RECOMP_TRACE_D3D7_SMALL_TEXTURED=1 adds small
// draws that have a texture bound on stage 0. Within a frame each distinct
// (vertex bytes, state) pair logs once with a count; identical consecutive
// frames collapse to one emitting frame plus a "frames a-b: unchanged" line.
//
// A frame is BeginScene..EndScene: the engine's D3D7 draws require an open
// scene, so every drawn frame has both. Per frame the trace prints one summary
// line per distinct draw signature (repeated identical draws are counted), the
// vertex-buffer Lock/Unlock activity, and the first vertices of the biggest
// draw. It writes to stderr with a `[d3d7-trace]` prefix and is independent of
// RECOMP_LOG. It never changes the guest path.
struct D3d7TraceConfig {
    bool enabled = false;
    bool inited = false;
    uint32_t lo = 1;
    uint32_t hi = 0xffffffffu;
    uint32_t vertex_frames = 3;
    uint32_t matrix_frames = 3;
    uint32_t max_vertex_dump = 16;
    bool small = false;
    bool small_textured = false;
};

struct D3d7TraceLockAgg {
    uint32_t vb = 0;
    uint32_t bytes = 0;
    uint32_t flags = 0;
    uint64_t locks = 0;
    uint64_t unlocks = 0;
};

struct D3d7TraceDrawAgg {
    std::string text;
    uint64_t count = 0;
};

// One distinct small draw within a frame: the raw vertex bytes plus the state,
// so a change in either is a new line. `text` is the decoded, printable form.
struct D3d7TraceSmallDraw {
    std::string key;
    std::string text;
    uint64_t count = 0;
};

static D3d7TraceConfig g_trace;
static uint32_t g_trace_frame = 0;
static bool g_trace_frame_open = false;
static bool g_trace_frame_log = false;
// How many frames that actually contained a draw have been seen, so
// `vertex_frames` counts drawing frames rather than startup frames (the intro
// plays several empty scenes before the first draw).
static uint32_t g_trace_draw_frames = 0;
static std::vector<D3d7TraceLockAgg> g_trace_locks;
static std::map<std::string, D3d7TraceDrawAgg> g_trace_draws;
// Small-draw trace state, independent of the aggregate map above.
static std::vector<D3d7TraceSmallDraw> g_trace_small_draws;
static D3d7TraceSmallCollapser g_trace_small_collapse;
// The biggest draw of the frame, captured so the vertices can be printed at
// frame end even though the guest may reuse the buffer before then.
static bool g_trace_big_valid = false;
static uint32_t g_trace_big_vcount = 0;
static uint32_t g_trace_big_stride = 0;
static uint32_t g_trace_big_fvf = 0;
static std::string g_trace_big_kind;
static std::vector<uint8_t> g_trace_big_bytes;
// Last matrix logged per transform state, so a SetTransform with the same
// value is not reported twice.
static float g_trace_last_matrix[256][16];
static bool g_trace_last_matrix_set[256];

static void trace_log(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void trace_log(const char *fmt, ...) {
    fputs("[d3d7-trace] ", stderr);
    va_list ap;
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fputc('\n', stderr);
}

static void trace_appendf(std::string &s, const char *fmt, ...)
    __attribute__((format(printf, 2, 3)));
static void trace_appendf(std::string &s, const char *fmt, ...) {
    char buf[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    s += buf;
}

static void trace_init() {
    if (g_trace.inited)
        return;
    g_trace.inited = true;
    const char *e = recomp_env("TRACE_D3D7");
    g_trace.enabled = e && e[0] && strcmp(e, "0") != 0;
    if (!g_trace.enabled)
        return;
    const char *fr = recomp_env("TRACE_D3D7_FRAMES");
    uint32_t lo = g_trace.lo, hi = g_trace.hi;
    if (fr && d3d7_trace_parse_frames(fr, &lo, &hi)) {
        g_trace.lo = lo;
        g_trace.hi = hi;
    }
    const char *vf = recomp_env("TRACE_D3D7_VERTEX_FRAMES");
    if (vf && vf[0])
        g_trace.vertex_frames = (uint32_t)strtoul(vf, nullptr, 10);
    const char *mf = recomp_env("TRACE_D3D7_MATRIX_FRAMES");
    if (mf && mf[0])
        g_trace.matrix_frames = (uint32_t)strtoul(mf, nullptr, 10);
    const char *sm = recomp_env("TRACE_D3D7_SMALL");
    g_trace.small = sm && sm[0] && strcmp(sm, "0") != 0;
    const char *smt = recomp_env("TRACE_D3D7_SMALL_TEXTURED");
    g_trace.small_textured = smt && smt[0] && strcmp(smt, "0") != 0;
}

static float trace_f32(const uint32_t *p) {
    float f;
    memcpy(&f, p, 4);
    return f;
}

// Decodes one vertex per the FVF, matching the stride rule in fvf_stride. The
// byte layout follows the D3DFVF field order (position, normal, point size,
// diffuse, specular, then coordinate sets).
static std::string format_vertex(uint32_t fvf, const uint8_t *v) {
    if (!fvf_stride(fvf))
        return "<undecoded FVF>";
    auto f32 = [&](size_t o) {
        float x;
        memcpy(&x, v + o, 4);
        return x;
    };
    std::string s;
    size_t off = 0;
    switch (fvf & 0x00e) { // D3DFVF_POSITION_MASK
    case 0x002:            // XYZ
        trace_appendf(s, "pos=(%.4g,%.4g,%.4g)", f32(0), f32(4), f32(8));
        off = 12;
        break;
    case 0x004: // XYZRHW
        trace_appendf(s, "pos=(%.4g,%.4g,%.4g,%.4g)", f32(0), f32(4), f32(8), f32(12));
        off = 16;
        break;
    default:
        return "<unhandled position layout>";
    }
    if (fvf & 0x010) { // NORMAL
        trace_appendf(s, " n=(%.3g,%.3g,%.3g)", f32(off), f32(off + 4), f32(off + 8));
        off += 12;
    }
    if (fvf & 0x020) { // PSIZE
        trace_appendf(s, " psize=%.3g", f32(off));
        off += 4;
    }
    if (fvf & 0x040) { // DIFFUSE
        uint32_t d;
        memcpy(&d, v + off, 4);
        trace_appendf(s, " diff=%08x", d);
        off += 4;
    }
    if (fvf & 0x080) { // SPECULAR
        uint32_t d;
        memcpy(&d, v + off, 4);
        trace_appendf(s, " spec=%08x", d);
        off += 4;
    }
    uint32_t texcount = (fvf >> 8) & 0xf;
    for (uint32_t i = 0; i < texcount && i < 4; ++i) {
        uint32_t size = (fvf >> (16 + i * 2)) & 0x3;
        uint32_t n = size == 0 ? 2 : size; // 0 means the SDK's default of 2 floats
        trace_appendf(s, " uv%u=(", i);
        for (uint32_t k = 0; k < n; ++k)
            trace_appendf(s, "%s%.3g", k ? "," : "", f32(off + k * 4));
        trace_appendf(s, ")");
        off += n * 4;
    }
    return s;
}

static std::string trace_tex_desc(ComObj *dev, uint32_t stage) {
    ComObj *t = com_get(recomp::d3d::d3d7_get_texture(*dev->d3d7->value, stage).value);
    if (!t)
        return "-";
    char buf[64];
    snprintf(buf, sizeof buf, "#%u:%ux%ux%u", t->id, t->width, t->height, t->bpp);
    return buf;
}

static std::string trace_draw_text(ComObj *dev, const char *kind, uint32_t type, uint32_t fvf,
                                   uint32_t vcount, uint32_t icount, uint32_t stride,
                                   uint32_t start, uint32_t vb_id, uint32_t prims) {
    uint32_t rs[256];
    for (uint32_t i = 0; i < 256; ++i)
        rs[i] = recomp::d3d::d3d7_get_render_state(*dev->d3d7->value, i).value;
    const auto viewport = recomp::d3d::d3d7_get_viewport(*dev->d3d7->value).viewport;
    std::string s;
    trace_appendf(s,
                  "%s type=%u fvf=%08x vcount=%u icount=%u prims=%u stride=%u vb=%s start=%u "
                  "vp=%u,%u,%ux%u z[%.3g,%.3g]",
                  kind, type, fvf, vcount, icount, prims, stride, vb_id ? "vb" : "inline", start,
                  viewport.x, viewport.y, viewport.width, viewport.height, viewport.min_z,
                  viewport.max_z);
    if (vb_id)
        trace_appendf(s, " vb#%u startoff=%u", vb_id, start * stride);
    trace_appendf(s,
                  " | z=%u,%u,%u cull=%u fog=%u,%u,%u,%.4g,%.4g,%.4g,%08x clip=%u lit=%u "
                  "blend=%u,%u,%u atest=%u,%u,%u",
                  rs[7], rs[14], rs[23], rs[22], rs[28], rs[35], rs[140], trace_f32(&rs[36]),
                  trace_f32(&rs[37]), trace_f32(&rs[38]), rs[34], rs[136], rs[137], rs[27], rs[19],
                  rs[20], rs[15], rs[24], rs[25]);
    for (uint32_t stage = 0; stage < 2; ++stage)
        trace_appendf(s, " tex%u=%s", stage, trace_tex_desc(dev, stage).c_str());
    return s;
}

// The render state the small-draw trace keys on: a draw whose vertices are the
// same but whose blend/alpha/z/texture-stage state changed is a new line.
static std::string trace_small_state(ComObj *dev) {
    uint32_t rs[256], t[256];
    for (uint32_t i = 0; i < 256; ++i) {
        rs[i] = recomp::d3d::d3d7_get_render_state(*dev->d3d7->value, i).value;
        t[i] = recomp::d3d::d3d7_get_texture_stage_state(*dev->d3d7->value, 0, i).value;
    }
    char buf[256];
    snprintf(buf, sizeof buf,
             "blend=%u,%u,%u atest=%u,%u,%u z=%u,%u,%u tss0=col(%u,%u,%u)alpha(%u,%u,%u)", rs[27],
             rs[19], rs[20], rs[15], rs[24], rs[25], rs[7], rs[14], rs[23], t[1], t[2], t[3], t[4],
             t[5], t[6]);
    return buf;
}

// Collects one small draw for the frame. Grouping is on the exact vertex bytes
// plus state, so two floats that decode the same but differ in the low bits
// stay distinct. The guest vertex base is offset by StartVertex, which the
// biggest-draw dump above does not do; VB draws pass the buffer base here.
static void trace_small_draw(ComObj *dev, const char *kind, uint32_t type, uint32_t fvf,
                             uint32_t verts, uint32_t vcount, uint32_t icount, uint32_t stride,
                             uint32_t start, uint32_t prims) {
    if (!g_trace.small || !stride || !vcount || vcount > 8)
        return;
    const bool textured = recomp::d3d::d3d7_get_texture(*dev->d3d7->value, 0).value != 0;
    if (textured && !g_trace.small_textured)
        return;
    const uint64_t voff = (uint64_t)start * stride;
    const uint64_t bytes = (uint64_t)vcount * stride;
    if (voff > 0xffffffffu || bytes > 0xffffffffu ||
        !gm_valid(verts + (uint32_t)voff, (uint32_t)bytes))
        return;
    const uint8_t *v = gm_ptr(verts + (uint32_t)voff);
    std::string state = trace_small_state(dev);
    std::string key = state;
    key += '|';
    static const char hex[] = "0123456789abcdef";
    for (uint64_t i = 0; i < bytes; ++i) {
        key += hex[v[i] >> 4];
        key += hex[v[i] & 0xf];
    }
    std::string text;
    trace_appendf(text, "%s type=%u fvf=%08x vcount=%u icount=%u prims=%u stride=%u start=%u %s",
                  kind, type, fvf, vcount, icount, prims, stride, start, state.c_str());
    for (uint32_t i = 0; i < vcount; ++i) {
        std::string vs = d3d7_trace_vertex(fvf, v + (size_t)i * stride);
        trace_appendf(text, " | v%u:%s", i, vs.c_str());
    }
    for (auto &d : g_trace_small_draws)
        if (d.key == key) {
            ++d.count;
            return;
        }
    D3d7TraceSmallDraw d;
    d.key = std::move(key);
    d.text = std::move(text);
    d.count = 1;
    g_trace_small_draws.push_back(std::move(d));
}

static void trace_frame_end();
static void trace_frame_begin() {
    trace_init();
    if (!g_trace.enabled)
        return;
    // A well-behaved device ends every scene, but a frame left open (a present
    // boundary or an interrupted run) should still report what it held.
    if (g_trace_frame_open)
        trace_frame_end();
    ++g_trace_frame;
    g_trace_frame_open = true;
    g_trace_frame_log = g_trace_frame >= g_trace.lo && g_trace_frame <= g_trace.hi;
    g_trace_locks.clear();
    g_trace_draws.clear();
    g_trace_small_draws.clear();
    g_trace_big_valid = false;
    g_trace_big_vcount = 0;
    g_trace_big_bytes.clear();
    if (g_trace_frame > g_trace.hi)
        g_trace.enabled = false; // past the requested range: stop counting
    if (g_trace_frame_log)
        trace_log("=== frame %u begin ===", g_trace_frame);
}

static void trace_frame_end() {
    if (!g_trace_frame_open)
        return;
    g_trace_frame_open = false;
    if (!g_trace_frame_log)
        return;
    uint64_t total_draws = 0;
    for (const auto &kv : g_trace_draws)
        total_draws += kv.second.count;
    trace_log("--- frame %u summary: %zu distinct draws, %llu total, %zu lock groups ---",
              g_trace_frame, g_trace_draws.size(), (unsigned long long)total_draws,
              g_trace_locks.size());
    for (const auto &l : g_trace_locks) {
        if (l.locks)
            trace_log("    lock vb#%u bytes=%u flags=0x%x x%llu unlocks x%llu", l.vb, l.bytes,
                      l.flags, (unsigned long long)l.locks, (unsigned long long)l.unlocks);
        else
            trace_log("    lock vb#%u bytes=%u unlock-only x%llu", l.vb, l.bytes,
                      (unsigned long long)l.unlocks);
    }
    for (const auto &kv : g_trace_draws)
        trace_log("    x%llu %s", (unsigned long long)kv.second.count, kv.first.c_str());
    if (g_trace_big_valid) {
        const uint32_t show =
            (uint32_t)(g_trace_big_bytes.size() / (g_trace_big_stride ? g_trace_big_stride : 1));
        trace_log("    biggest draw: %s fvf=%08x vcount=%u stride=%u (showing %u)",
                  g_trace_big_kind.c_str(), g_trace_big_fvf, g_trace_big_vcount, g_trace_big_stride,
                  show);
        for (uint32_t i = 0; i < show; ++i) {
            std::string vs = format_vertex(g_trace_big_fvf, g_trace_big_bytes.data() +
                                                                (size_t)i * g_trace_big_stride);
            trace_log("      v%u: %s", i, vs.c_str());
        }
    }
    if (total_draws)
        ++g_trace_draw_frames;
    if (g_trace.small) {
        std::string digest;
        for (const auto &d : g_trace_small_draws)
            trace_appendf(digest, "%s#%llu;", d.key.c_str(), (unsigned long long)d.count);
        const bool prev_had_content = !g_trace_small_collapse.digest.empty();
        std::string collapsed;
        if (d3d7_trace_small_step(&g_trace_small_collapse, g_trace_frame, digest, &collapsed)) {
            if (!collapsed.empty() && prev_had_content)
                trace_log("%s", collapsed.c_str());
            for (const auto &d : g_trace_small_draws)
                trace_log("small x%llu %s", (unsigned long long)d.count, d.text.c_str());
        }
    }
    g_trace_frame_log = false;
}

static void trace_draw(ComObj *dev, const char *kind, uint32_t type, uint32_t fvf, uint32_t verts,
                       uint32_t vcount, uint32_t icount, uint32_t stride, uint32_t start,
                       uint32_t vb_id, uint32_t prims) {
    if (!g_trace_frame_log || !dev->d3d7)
        return;
    std::string text =
        trace_draw_text(dev, kind, type, fvf, vcount, icount, stride, start, vb_id, prims);
    g_trace_draws[text].text = text;
    ++g_trace_draws[text].count;
    trace_small_draw(dev, kind, type, fvf, verts, vcount, icount, stride, start, prims);
    if (g_trace_draw_frames < g_trace.vertex_frames && stride && vcount > g_trace_big_vcount &&
        (uint64_t)stride * vcount <= 0xffffffffu && gm_valid(verts, stride * vcount)) {
        uint32_t ndump = vcount < g_trace.max_vertex_dump ? vcount : g_trace.max_vertex_dump;
        g_trace_big_valid = true;
        g_trace_big_vcount = vcount;
        g_trace_big_stride = stride;
        g_trace_big_fvf = fvf;
        g_trace_big_kind = kind;
        g_trace_big_bytes.assign(gm_ptr(verts), gm_ptr(verts) + (size_t)ndump * stride);
    }
}

static void trace_transform(ComObj *dev, uint32_t state) {
    if (!g_trace_frame_log || !dev->d3d7 || state >= 256)
        return;
    if (state != 1 && state != 2 && state != 3 && !(state >= 16 && state <= 23))
        return;
    const auto values = recomp::d3d::d3d7_get_transform(*dev->d3d7->value, state);
    if (values.values.size() != 16)
        return;
    const float *m = values.values.data();
    if (g_trace_last_matrix_set[state] && memcmp(g_trace_last_matrix[state], m, 64) == 0)
        return;
    memcpy(g_trace_last_matrix[state], m, 64);
    g_trace_last_matrix_set[state] = true;
    const char *name = state == 1   ? "WORLD"
                       : state == 2 ? "VIEW"
                       : state == 3 ? "PROJECTION"
                                    : "TEX";
    if (state > 3) {
        trace_log("frame %u transform %s(%u) changed", g_trace_frame, name, state);
        return;
    }
    if (g_trace_frame <= g_trace.matrix_frames) {
        trace_log("frame %u transform %s:", g_trace_frame, name);
        for (int row = 0; row < 4; ++row)
            trace_log("    [% .5f % .5f % .5f % .5f]", m[row * 4], m[row * 4 + 1], m[row * 4 + 2],
                      m[row * 4 + 3]);
    } else {
        trace_log("frame %u transform %s changed: row3=(%.4f,%.4f,%.4f,%.4f)", g_trace_frame, name,
                  m[12], m[13], m[14], m[15]);
    }
}

static void trace_vb_lock(ComObj *vb, uint32_t flags) {
    if (!g_trace_frame_log || !vb)
        return;
    for (auto &l : g_trace_locks)
        if (l.vb == vb->id && l.bytes == vb->pixels_bytes && l.flags == flags) {
            ++l.locks;
            return;
        }
    D3d7TraceLockAgg l;
    l.vb = vb->id;
    l.bytes = vb->pixels_bytes;
    l.flags = flags;
    l.locks = 1;
    g_trace_locks.push_back(l);
}

static void trace_vb_unlock(ComObj *vb) {
    if (!g_trace_frame_log || !vb)
        return;
    for (auto &l : g_trace_locks)
        if (l.vb == vb->id) {
            ++l.unlocks;
            return;
        }
    D3d7TraceLockAgg l;
    l.vb = vb->id;
    l.bytes = vb->pixels_bytes;
    l.unlocks = 1;
    g_trace_locks.push_back(l);
}

static void trace_reset() {
    if (g_trace.small && !g_trace_small_collapse.digest.empty()) {
        std::string collapsed;
        if (d3d7_trace_small_flush(&g_trace_small_collapse, &collapsed) && !collapsed.empty())
            trace_log("%s", collapsed.c_str());
    }
    g_trace_frame = 0;
    g_trace_frame_open = false;
    g_trace_frame_log = false;
    g_trace_draw_frames = 0;
    g_trace_locks.clear();
    g_trace_draws.clear();
    g_trace_small_draws.clear();
    g_trace_big_valid = false;
    g_trace_big_bytes.clear();
    g_trace_small_collapse = D3d7TraceSmallCollapser{};
    memset(g_trace_last_matrix, 0, sizeof g_trace_last_matrix);
    memset(g_trace_last_matrix_set, 0, sizeof g_trace_last_matrix_set);
}

// ---------------------------------------------------------------- host renderer
// The Rust d3d8-wgpu device owns a 32-bit internal render target; the guest's
// 16bpp DirectDraw back buffer stays the truth of what the guest sees. The two
// are reconciled by d3d7_writeback, which ddraw.cpp calls at every point it is
// about to read or present those bytes. DIVERGENCE(original): the original
// rendered directly into the 16bpp target, so its clear color and every pixel
// landed in R5G6B5; here the target is A8R8G8B8 and the 32->16 copy truncates
// the low bits (d3d7_rgb888_to_rgb565).
constexpr uint32_t D3DCLEAR_STENCIL_ = 0x00000004u;
constexpr uint32_t D3D8FMT_A8R8G8B8 = 21;
constexpr uint32_t D3D8FMT_D16 = 80;
constexpr uint32_t D3D8_ERR_INVALIDCALL = 0x8876086Cu;

bool host_ok(const recomp::d3d::Status &status, const char *what) {
    if (d3d_bridge_ok(status))
        return true;
    LOGW("d3d7: host renderer %s failed: %.*s", what, static_cast<int>(status.message.size()),
         status.message.data());
    return false;
}

void host_require(const recomp::d3d::Status &status, const char *what) {
    if (!host_ok(status, what)) {
        fflush(stderr);
        abort();
    }
}

uint32_t state_block_hresult(const recomp::d3d::Status &status) {
    switch (status.state_block_error) {
    case recomp::d3d::StateBlockError::InBeginStateBlock:
        return D3DERR_INBEGINSTATEBLOCK;
    case recomp::d3d::StateBlockError::NotInBeginStateBlock:
        return D3DERR_NOTINBEGINSTATEBLOCK;
    case recomp::d3d::StateBlockError::InvalidStateBlock:
        return D3DERR_INVALIDSTATEBLOCK;
    case recomp::d3d::StateBlockError::OutOfMemory:
        return DDERR_OUTOFMEMORY;
    case recomp::d3d::StateBlockError::None:
        return D3DERR_INVALID_DEVICE;
    }
    return D3DERR_INVALID_DEVICE;
}

// Forward one recorded render state to the host. The state is already in the
// store; this is the seam, and every render-state forward goes through it so
// the D3D7->D3D8 table exists once.
void forward_render_state(ComObj *dev, uint32_t state, uint32_t value) {
    if (!dev->d3d7_host)
        return;
    const auto translated = recomp::d3d::translate_render_state(state, value);
    if (translated.kind == recomp::d3d::D3d7TranslationKind::Invalid) {
        LOGW("d3d7: render state %u (value %u) has no D3D8 equivalent; stopping", state, value);
        fflush(stderr);
        abort();
    }
    if (translated.kind == recomp::d3d::D3d7TranslationKind::Ignore)
        return;
    host_require(recomp::d3d::set_render_state(*dev->d3d7_host->value, translated.state, value),
                 "SetRenderState");
}

void forward_texture_stage_state(ComObj *dev, uint32_t stage, uint32_t type, uint32_t value) {
    if (!dev->d3d7_host)
        return;
    const auto translated = recomp::d3d::translate_texture_stage_state(type);
    if (translated.count < 0) {
        LOGW("d3d7: texture-stage state %u has no D3D8 equivalent; stopping", type);
        fflush(stderr);
        abort();
    }
    for (int i = 0; i < translated.count; ++i) {
        const uint32_t state = i == 0 ? translated.first : translated.second;
        host_require(
            recomp::d3d::set_texture_stage_state(*dev->d3d7_host->value, stage, state, value),
            "SetTextureStageState");
    }
}

void forward_transform(ComObj *dev, uint32_t state) {
    if (!dev->d3d7_host)
        return;
    const auto translated = recomp::d3d::translate_transform(state);
    if (!translated.valid)
        return; // Get/SetTransform keeps states the D3D8 table has no slot for.
    const auto values = recomp::d3d::d3d7_get_transform(*dev->d3d7->value, state);
    host_require(values.status, "GetTransform");
    const rust::Slice<const float> matrix(values.values.data(), values.values.size());
    host_require(recomp::d3d::set_transform(*dev->d3d7_host->value, translated.state, matrix),
                 "SetTransform");
}

void forward_viewport(ComObj *dev) {
    if (!dev->d3d7_host)
        return;
    const auto result = recomp::d3d::d3d7_get_viewport(*dev->d3d7->value);
    if (!host_ok(result.status, "GetViewport"))
        return;
    host_require(recomp::d3d::set_viewport(*dev->d3d7_host->value, result.viewport), "SetViewport");
}

void forward_material(ComObj *dev) {
    if (!dev->d3d7_host)
        return;
    const auto values = recomp::d3d::d3d7_get_material(*dev->d3d7->value);
    if (!host_ok(values.status, "GetMaterial") || values.values.size() != 17)
        return;
    recomp::d3d::Material material{};
    auto f = [&](size_t index) { return std::bit_cast<float>(values.values[index]); };
    material.diffuse_r = f(0);
    material.diffuse_g = f(1);
    material.diffuse_b = f(2);
    material.diffuse_a = f(3);
    material.ambient_r = f(4);
    material.ambient_g = f(5);
    material.ambient_b = f(6);
    material.ambient_a = f(7);
    material.specular_r = f(8);
    material.specular_g = f(9);
    material.specular_b = f(10);
    material.specular_a = f(11);
    material.emissive_r = f(12);
    material.emissive_g = f(13);
    material.emissive_b = f(14);
    material.emissive_a = f(15);
    material.power = f(16);
    host_require(recomp::d3d::set_material(*dev->d3d7_host->value, material), "SetMaterial");
}

void forward_light(ComObj *dev, uint32_t index) {
    if (!dev->d3d7_host)
        return;
    const auto values = recomp::d3d::d3d7_get_light(*dev->d3d7->value, index);
    if (!host_ok(values.status, "GetLight") || values.values.size() != 26)
        return;
    recomp::d3d::Light light{};
    light.light_type = values.values[0];
    auto f = [&](size_t index) { return std::bit_cast<float>(values.values[index]); };
    light.diffuse_r = f(1);
    light.diffuse_g = f(2);
    light.diffuse_b = f(3);
    light.diffuse_a = f(4);
    light.specular_r = f(5);
    light.specular_g = f(6);
    light.specular_b = f(7);
    light.specular_a = f(8);
    light.ambient_r = f(9);
    light.ambient_g = f(10);
    light.ambient_b = f(11);
    light.ambient_a = f(12);
    light.position_x = f(13);
    light.position_y = f(14);
    light.position_z = f(15);
    light.direction_x = f(16);
    light.direction_y = f(17);
    light.direction_z = f(18);
    light.range = f(19);
    light.falloff = f(20);
    light.attenuation0 = f(21);
    light.attenuation1 = f(22);
    light.attenuation2 = f(23);
    light.theta = f(24);
    light.phi = f(25);
    host_require(recomp::d3d::set_light(*dev->d3d7_host->value, index, light), "SetLight");
}

// Replays every state the engine set before the host device existed. The
// device is created on the first Clear, but the engine sets transforms,
// material, lights, viewport and render states before that.
void replay_state(ComObj *dev) {
    for (uint32_t i = 0; i < 256; ++i)
        if (recomp::d3d::d3d7_render_state_was_set(*dev->d3d7->value, i))
            forward_render_state(dev, i,
                                 recomp::d3d::d3d7_get_render_state(*dev->d3d7->value, i).value);
    for (uint32_t stage = 0; stage < 8; ++stage)
        for (uint32_t type = 0; type < 256; ++type)
            if (recomp::d3d::d3d7_texture_stage_state_was_set(*dev->d3d7->value, stage, type))
                forward_texture_stage_state(
                    dev, stage, type,
                    recomp::d3d::d3d7_get_texture_stage_state(*dev->d3d7->value, stage, type)
                        .value);
    for (uint32_t t = 0; t < 256; ++t)
        if (recomp::d3d::d3d7_transform_was_set(*dev->d3d7->value, t))
            forward_transform(dev, t);
    if (recomp::d3d::d3d7_viewport_was_set(*dev->d3d7->value))
        forward_viewport(dev);
    if (recomp::d3d::d3d7_material_was_set(*dev->d3d7->value))
        forward_material(dev);
    for (uint32_t i = 0; i < 8; ++i) {
        if (recomp::d3d::d3d7_get_light_enable(*dev->d3d7->value, i).value)
            forward_light(dev, i);
        if (dev->d3d7_host && !recomp::d3d::d3d7_recording(*dev->d3d7->value))
            host_require(recomp::d3d::light_enable(
                             *dev->d3d7_host->value, i,
                             recomp::d3d::d3d7_get_light_enable(*dev->d3d7->value, i).value != 0),
                         "LightEnable");
    }
}

void replay_state_block(ComObj *dev, uint32_t handle) {
    auto &state = *dev->d3d7->value;
    for (uint32_t i = 0; i < 256; ++i) {
        if (recomp::d3d::d3d7_state_block_render_state_was_set(state, handle, i))
            forward_render_state(dev, i, recomp::d3d::d3d7_get_render_state(state, i).value);
        if (recomp::d3d::d3d7_state_block_transform_was_set(state, handle, i))
            forward_transform(dev, i);
        for (uint32_t stage = 0; stage < 8; ++stage)
            if (recomp::d3d::d3d7_state_block_texture_stage_state_was_set(state, handle, stage, i))
                forward_texture_stage_state(
                    dev, stage, i,
                    recomp::d3d::d3d7_get_texture_stage_state(state, stage, i).value);
    }
    if (recomp::d3d::d3d7_state_block_viewport_was_set(state, handle))
        forward_viewport(dev);
    if (recomp::d3d::d3d7_state_block_material_was_set(state, handle))
        forward_material(dev);
    for (uint32_t i = 0; i < 8; ++i) {
        if (recomp::d3d::d3d7_state_block_light_was_set(state, handle, i))
            forward_light(dev, i);
        if (dev->d3d7_host && recomp::d3d::d3d7_state_block_light_enable_was_set(state, handle, i))
            host_require(
                recomp::d3d::light_enable(*dev->d3d7_host->value, i,
                                          recomp::d3d::d3d7_get_light_enable(state, i).value != 0),
                "LightEnable");
    }
}

// Creates the Rust device for a D3D7 device's DirectDraw back buffer. Host
// truth: the internal target is A8R8G8B8 because that is what the wgpu
// path can read back as tightly packed RGBA8; the depth attachment is D16
// because the z-buffer the engine enumerated and attached is 16-bit.
bool ensure_host(ComObj *dev) {
    if (dev->d3d7_host)
        return true;
    ComObj *target = com_get(recomp::d3d::d3d7_get_render_target(*dev->d3d7->value));
    if (!target || target->kind != K_SURFACE || !target->width || !target->height) {
        LOGW("d3d7: cannot create the host renderer without a render target surface");
        return false;
    }
    uint32_t depth = target->zbuffer_obj ? D3D8FMT_D16 : 0;
    auto host = std::make_shared<D3dHostDevice>();
    const auto status = recomp::d3d::initialize_gpu(*host->value, target->width, target->height,
                                                    D3D8FMT_A8R8G8B8, depth);
    if (!host_ok(status, "device creation")) {
        return false;
    }
    dev->d3d7_host = host;
    dev->d3d7_width = target->width;
    dev->d3d7_height = target->height;
    replay_state(dev);
    return true;
}

// Copies the Rust 32-bit target into the guest surface. The writeback is a
// full replacement: the RenderTarget for this device is authoritative for the
// 3D content of that surface. DIVERGENCE(original): 2D the guest drew into the
// surface through GDI or a Lock is not uploaded back into the 32-bit target,
// so a surface that mixed 2D and 3D loses the 2D at the reconcile point (see
// docs/d3d7-inventory.md open questions).
//
// Lazy policy: the readback only runs while `target_dirty`, i.e. since the
// last successful reconcile a Clear or draw has changed the GPU target. A
// flush with no such change (the ordinary repeated flush of a surface the
// device has not rendered into this frame) is skipped, so the guest bytes are
// left as they are. This can only avoid clobbering CPU-written bytes; it never
// hides GPU content, because every GPU-target write sets the flag.
void d3d7_writeback(ComObj *dev) {
    if (!dev || !dev->d3d7_host)
        return;
    ComObj *s = com_get(recomp::d3d::d3d7_get_render_target(*dev->d3d7->value));
    if (!s || s->kind != K_SURFACE || !s->pixels)
        return;
    if (!recomp::d3d::d3d7_target_dirty(*dev->d3d7->value))
        return;
    const uint32_t w = dev->d3d7_width, h = dev->d3d7_height;
    if (!w || !h)
        return;
    const uint64_t bytes = (uint64_t)w * h * 4;
    static std::vector<uint8_t> rgba;
    if (rgba.size() != (size_t)bytes)
        rgba.resize((size_t)bytes);
    uint64_t got = 0;
    const uint64_t marker_start = profile_marker_now();
    const bool length_ok =
        host_ok(recomp::d3d::read_pixels_len(*dev->d3d7_host->value, got), "ReadPixelsLength");
    const bool read_ok =
        length_ok && got == bytes &&
        host_ok(recomp::d3d::read_pixels(*dev->d3d7_host->value,
                                         rust::Slice<uint8_t>(rgba.data(), rgba.size())),
                "ReadPixels");
    profile_marker_end("readback", marker_start);
    if (!read_ok)
        return;
    if (got != bytes) {
        log_once("d3d7.writeback.size", "d3d7: readback returned %llu bytes, expected %llu",
                 (unsigned long long)got, (unsigned long long)bytes);
        return;
    }
    if (!d3d7_store_rgba_surface(gm_ptr(s->pixels), s->pitch, s->bpp, w, h, rgba.data())) {
        log_once("d3d7.writeback.bpp", "d3d7: writeback to a %u-bpp surface is not implemented",
                 s->bpp);
        return; // leave dirty: a later flush may still service it
    }
    recomp::d3d::d3d7_set_target_dirty(*dev->d3d7->value, false);
    ddraw_sync_gpu_pixels(s);
}

// ---------------------------------------------------------------- textures
// A bound D3D7 texture is a guest DirectDraw surface whose pixels stay
// authoritative. The Rust renderer samples linear RGBA8, so the 16bpp formats
// the engine creates are expanded to A8R8G8B8 at this upload point only.
// DIVERGENCE(original): the original D3D7 device sampled the guest's native
// R5G6B5/A1R5G5B5/A4R4G4B4 texels in the sampler; the guest bytes are still
// what is stored and locked, and the expansion happens solely on the way into
// the host texture. DXT/undecoded layouts have no entry and stop by name.
enum D3d7TexFormat {
    D7TEX_ARGB8888,
    D7TEX_R5G6B5,
    D7TEX_A1R5G5B5,
    D7TEX_A4R4G4B4,
    D7TEX_DXT,
    D7TEX_NONE,
};

D3d7TexFormat d3d7_classify_texture(const ComObj *s) {
    if (!s || !s->pixels)
        return D7TEX_NONE;
    // A compressed surface carries its layout in fourcc and reports bpp 0.
    // The engine's DXT surfaces are normally DirectDraw-only decode sources
    // (Blt into a pool texture), so this is the defensive path for a DXT
    // surface bound directly as a texture.
    if (dxdxt::is_dxt(s->fourcc))
        return D7TEX_DXT;
    if (s->bpp == 32)
        return D7TEX_ARGB8888;
    if (s->bpp != 16)
        return D7TEX_NONE;
    if (s->rmask == 0xf800 && s->gmask == 0x07e0 && s->bmask == 0x001f)
        return D7TEX_R5G6B5;
    if (s->rmask == 0x7c00 && s->gmask == 0x03e0 && s->bmask == 0x001f)
        return D7TEX_A1R5G5B5;
    if (s->rmask == 0x0f00 && s->gmask == 0x00f0 && s->bmask == 0x000f)
        return D7TEX_A4R4G4B4;
    return D7TEX_NONE;
}

// D3D8's channel expansion (bit replication), matching the renderer's
// ColorFormat decoder so both sides agree in the middle of the range.
uint8_t d3d7_expand_channel(uint32_t value, uint32_t bits) {
    switch (bits) {
    case 1:
        return value ? 0xff : 0x00;
    case 4:
        return (uint8_t)((value << 4) | value);
    case 5:
        return (uint8_t)((value << 3) | (value >> 2));
    default:
        return (uint8_t)((value << 2) | (value >> 4)); // 6 bits
    }
}

// Expand a surface into the tightly packed little-endian A8R8G8B8 block the
// renderer uploads (`B,G,R,A`). Returns false for a layout without a decoder.
bool d3d7_convert_texture(const ComObj *tex, std::vector<uint8_t> &out) {
    const D3d7TexFormat fmt = d3d7_classify_texture(tex);
    out.clear();
    const uint32_t w = tex->width, h = tex->height;
    if (fmt == D7TEX_NONE || !w || !h)
        return false;
    out.resize((size_t)w * h * 4);
    if (fmt == D7TEX_DXT) {
        const uint8_t *base = gm_ptr(tex->pixels);
        for (uint32_t y = 0; y < h; ++y) {
            uint8_t *dst = out.data() + (size_t)y * w * 4;
            for (uint32_t x = 0; x < w; ++x) {
                dxdxt::Rgba px;
                if (!dxdxt::sample(base, tex->pitch, tex->fourcc, x, y, &px))
                    return false;
                dst[x * 4 + 0] = px.b;
                dst[x * 4 + 1] = px.g;
                dst[x * 4 + 2] = px.r;
                dst[x * 4 + 3] = px.a;
            }
        }
        return true;
    }
    for (uint32_t y = 0; y < h; ++y) {
        const uint32_t row = tex->pixels + (uint32_t)((size_t)y * tex->pitch);
        uint8_t *dst = out.data() + (size_t)y * w * 4;
        for (uint32_t x = 0; x < w; ++x) {
            uint32_t r, g, b, a;
            if (fmt == D7TEX_ARGB8888) {
                const uint32_t px = rd32(row + x * 4);
                b = px & 0xff;
                g = (px >> 8) & 0xff;
                r = (px >> 16) & 0xff;
                a = (px >> 24) & 0xff;
            } else {
                const uint32_t t = rd16(row + x * 2);
                if (fmt == D7TEX_R5G6B5) {
                    r = d3d7_expand_channel((t >> 11) & 0x1f, 5);
                    g = d3d7_expand_channel((t >> 5) & 0x3f, 6);
                    b = d3d7_expand_channel(t & 0x1f, 5);
                    a = 0xff;
                } else if (fmt == D7TEX_A1R5G5B5) {
                    a = tex->amask ? d3d7_expand_channel((t >> 15) & 0x1, 1) : 0xff;
                    r = d3d7_expand_channel((t >> 10) & 0x1f, 5);
                    g = d3d7_expand_channel((t >> 5) & 0x1f, 5);
                    b = d3d7_expand_channel(t & 0x1f, 5);
                } else { // A4R4G4B4
                    a = d3d7_expand_channel((t >> 12) & 0xf, 4);
                    r = d3d7_expand_channel((t >> 8) & 0xf, 4);
                    g = d3d7_expand_channel((t >> 4) & 0xf, 4);
                    b = d3d7_expand_channel(t & 0xf, 4);
                }
            }
            dst[x * 4 + 0] = (uint8_t)b;
            dst[x * 4 + 1] = (uint8_t)g;
            dst[x * 4 + 2] = (uint8_t)r;
            dst[x * 4 + 3] = (uint8_t)a;
        }
    }
    return true;
}

// Upload the stage's bound surface to the host, or unbind the stage. Called by
// SetTexture and again before every draw, so a surface written through a Lock,
// Blt or BltFast after SetTexture is re-uploaded. The content generation is
// bumped by ddraw.cpp's surface_pixels_changed; a lock still open at draw time
// forces the upload instead of trusting it.
void d3d7_sync_texture(ComObj *dev, uint32_t stage) {
    if (!dev || !dev->d3d7_host || stage >= 8 || recomp::d3d::d3d7_recording(*dev->d3d7->value))
        return;
    ComObj *tex = com_get(recomp::d3d::d3d7_get_texture(*dev->d3d7->value, stage).value);
    auto &sync = dev->d3d7_host->texture_sync[stage];
    if (!tex || tex->kind != K_SURFACE || !tex->pixels || !tex->width || !tex->height) {
        const std::vector<recomp::d3d::TextureLevel> none;
        if (!sync.matches(0, 0, none)) {
            host_require(recomp::d3d::set_texture(*dev->d3d7_host->value, stage, 0, 0,
                                                  rust::Slice<const recomp::d3d::TextureLevel>()),
                         "SetTexture");
            sync.record(0, 0, none);
        }
        return;
    }
    // Reuse the conversion of this surface while its content generation is
    // unchanged and it is not locked. The engine rebinds dozens of distinct
    // textures on stage 0 every frame, so the cache is per surface (ids are
    // never reused) and not per stage; a single entry per stage was missed on
    // nearly every bind and made the expansion about a third of the frame.
    // Bounded by a byte budget, dropping the least recently bound surfaces.
    struct ConvEntry {
        uint64_t generation = UINT64_MAX; // UINT64_MAX: never fresh
        uint64_t used = 0;
        std::vector<uint8_t> bytes;
    };
    static std::unordered_map<uint32_t, ConvEntry> conv_cache;
    static size_t conv_bytes = 0;
    static uint64_t conv_tick = 0;
    constexpr size_t CONV_BUDGET = 256u << 20;
    if (conv_bytes > CONV_BUDGET) {
        std::vector<std::pair<uint64_t, uint32_t>> order;
        order.reserve(conv_cache.size());
        for (const auto &kv : conv_cache)
            if (kv.first != tex->id)
                order.emplace_back(kv.second.used, kv.first);
        std::sort(order.begin(), order.end());
        for (const auto &o : order) {
            if (conv_bytes <= CONV_BUDGET / 2)
                break;
            auto it = conv_cache.find(o.second);
            conv_bytes -= it->second.bytes.size();
            conv_cache.erase(it);
        }
    }
    std::vector<recomp::d3d::TextureLevel> levels;
    // DirectDraw SetTexture binds the root of an implicit mip chain, not
    // just its base pixels. Reconcile and version each level independently;
    // updating a child must not require changing/rebinding the root.
    for (ComObj *level = tex; level; level = com_get(level->mip_next)) {
        const int32_t full[4] = {0, 0, (int32_t)level->width, (int32_t)level->height};
        ddraw_refresh_retained_writes(level, full);
        ConvEntry &sc = conv_cache[level->id];
        sc.used = ++conv_tick;
        std::vector<uint8_t> &converted = sc.bytes;
        const uint64_t generation = level->d3d8_resource ? level->d3d8_resource->generation()
                                                         : ddraw_surface_revision(level->id);
        const bool fresh =
            sc.generation == generation && level->lock_count == 0 && !converted.empty();
        if (!fresh) {
            conv_bytes -= converted.size();
            const bool ok = d3d7_convert_texture(level, converted);
            conv_bytes += converted.size();
            if (!ok) {
                LOGW("d3d7: SetTexture stage %u surface %ux%u bpp=%u masks=%08x/%08x/%08x has no "
                     "decoded format (DXT and other compressed layouts are deferred); stopping",
                     stage, level->width, level->height, level->bpp, level->rmask, level->gmask,
                     level->bmask);
                fflush(stderr);
                abort();
            }
        }
        sc.generation = level->lock_count == 0 ? generation : UINT64_MAX;
        const uint32_t dirty = level->lock_count > 0 ? 1u : 0u;
        recomp::d3d::TextureLevel lvl{};
        lvl.level = (uint32_t)levels.size();
        lvl.width = level->width;
        lvl.height = level->height;
        lvl.force_upload = dirty != 0;
        lvl.generation = generation;
        lvl.data = rust::Slice<const uint8_t>(converted.data(), converted.size());
        levels.push_back(lvl);
    }
    if (sync.matches(tex->id, D3D8FMT_A8R8G8B8, levels))
        return;
    sync.valid = false;
    host_require(recomp::d3d::set_texture(
                     *dev->d3d7_host->value, stage, tex->id, D3D8FMT_A8R8G8B8,
                     rust::Slice<const recomp::d3d::TextureLevel>(levels.data(), levels.size())),
                 "SetTexture");
    sync.record(tex->id, D3D8FMT_A8R8G8B8, levels);
}

// ---------------------------------------------------------------- device caps
// Fills a D3DDEVICEDESC7. "Host truth" means it follows from what the host
// really is; "chosen" means the value is a reasonable stand-in the renderer
// must be able to keep when the state is forwarded.
void fill_device_desc7(uint32_t addr, const uint8_t guid[16], bool tnl) {
    gm_zero(addr, D3DDEVICEDESC7_SIZE);
    // Host truth: the class decides the TnL bit; the arena holds system-memory
    // textures, hardware rasterizes. Chosen: the primitive and draw caps below
    // are the fixed-function set wgpu can keep.
    uint32_t caps = D3DDEVCAPS_TEXTURENONLOCALVIDMEM | D3DDEVCAPS_HWRASTERIZATION |
                    D3DDEVCAPS_DRAWPRIMITIVES2 | D3DDEVCAPS_DRAWPRIMTLVERTEX |
                    D3DDEVCAPS_TEXTUREVIDEOMEMORY | D3DDEVCAPS_TLVERTEXSYSTEMMEMORY;
    if (tnl)
        caps |= D3DDEVCAPS_HWTRANSFORMANDLIGHT;
    wr32(addr + D3DDD7_OFF_dwDevCaps, caps);
    wr32(addr + D3DDD7_OFF_dpcLineCaps + D3DPC_OFF_dwSize, D3DPRIMCAPS_SIZE);
    wr32(addr + D3DDD7_OFF_dpcTriCaps + D3DPC_OFF_dwSize, D3DPRIMCAPS_SIZE);
    // These filters are implemented by the renderer, including implicit
    // DirectDraw mip chains. Do not advertise anisotropic/cubic filters.
    for (uint32_t pc : {D3DDD7_OFF_dpcLineCaps, D3DDD7_OFF_dpcTriCaps}) {
        wr32(addr + pc + D3DPC_OFF_dwTextureCaps, D3DPTEXTURECAPS_MIPMAP);
        wr32(addr + pc + D3DPC_OFF_dwTextureFilterCaps,
             D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MINFLINEAR | D3DPTFILTERCAPS_MIPFPOINT |
                 D3DPTFILTERCAPS_MIPFLINEAR | D3DPTFILTERCAPS_MAGFPOINT |
                 D3DPTFILTERCAPS_MAGFLINEAR);
    }
    // Host truth: the display mode is 16bpp and the z-buffer the engine asks
    // for is 16-bit, so both bit-depth masks advertise 16 only.
    wr32(addr + D3DDD7_OFF_dwDeviceRenderBitDepth, DDBD_16);
    wr32(addr + D3DDD7_OFF_dwDeviceZBufferBitDepth, DDBD_16);
    // Chosen: the engine creates 256x256 textures and DXT ones it makes itself.
    wr32(addr + D3DDD7_OFF_dwMinTextureWidth, 1);
    wr32(addr + D3DDD7_OFF_dwMinTextureHeight, 1);
    wr32(addr + D3DDD7_OFF_dwMaxTextureWidth, 2048);
    wr32(addr + D3DDD7_OFF_dwMaxTextureHeight, 2048);
    wr32(addr + D3DDD7_OFF_dwMaxTextureRepeat, 0);
    wr32(addr + D3DDD7_OFF_dwMaxTextureAspectRatio, 0);
    wr32(addr + D3DDD7_OFF_dwMaxAnisotropy, 1);
    wr32(addr + D3DDD7_OFF_dwStencilCaps, 0);
    wr32(addr + D3DDD7_OFF_dwFVFCaps, 0x100);
    // Chosen: the engine writes at most three texture stages; advertise the
    // D3D7 maximum so it is not refused, with two simultaneous textures.
    wr32(addr + D3DDD7_OFF_dwTextureOpCaps, 0x0000ffffu);
    wr16(addr + D3DDD7_OFF_wMaxTextureBlendStages, 8);
    wr16(addr + D3DDD7_OFF_wMaxSimultaneousTextures, 2);
    wr32(addr + D3DDD7_OFF_dwMaxActiveLights, 8);
    wr32(addr + D3DDD7_OFF_dvMaxVertexW, 0x41200000u); // 10.0f, chosen
    memcpy(gm_ptr(addr + D3DDD7_OFF_deviceGUID), guid, 16);
    wr16(addr + D3DDD7_OFF_wMaxUserClipPlanes, 0);
    wr16(addr + D3DDD7_OFF_wMaxVertexBlendMatrices, 0);
    wr32(addr + D3DDD7_OFF_dwVertexProcessingCaps, 0);
}

// ---------------------------------------------------------------- factory
void D3D7_EnumDevices(X86 *c) {
    ComObj *d3d = this_d3d7(c);
    uint32_t cb = arg(c, 1), ctx = arg(c, 2);
    if (!d3d || !cb) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    // One device, the HAL class: the guest's default path. The callback is the
    // D3D7 form (description, name, D3DDEVICEDESC7*, ctx).
    uint32_t desc7 = scratch(256 + D3DDEVICEDESC7_SIZE);
    if (!desc7) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    uint32_t desc_str = desc7;
    uint32_t name_str = desc7 + 128;
    gm_put_str(desc_str, "Primary Display Driver (HAL)", 128);
    gm_put_str(name_str, "display", 128);
    fill_device_desc7(desc7 + 256, IID_IDirect3DHALDevice_, false);
    guest_call(c, cb, desc_str, name_str, desc7 + 256, ctx);
    com_ret(c, D3D_OK_);
}

void D3D7_EnumZBufferFormats(X86 *c) {
    ComObj *d3d = this_d3d7(c);
    uint32_t guid = arg(c, 1), cb = arg(c, 2), ctx = arg(c, 3);
    if (!d3d || !cb)
        com_ret(c, DDERR_INVALIDPARAMS);
    else if (!(guid_eq(guid, IID_IDirect3DHALDevice_) || guid_eq(guid, IID_IDirect3DTnLHalDevice_)))
        com_ret(c, DDERR_NOTFOUND);
    else {
        // One DDPF_ZBUFFER entry, 16-bit. The display mode is 16bpp and the
        // callback at 0x82b370 records dwRGBBitCount into DAT_00c386d8 (as
        // 16 or not), then OpenD3D creates whichever format first succeeds.
        // A 16-bit z-buffer is therefore the format the original run used;
        // reporting a 24/32-bit one would set DAT_00c386d8=0 and change the
        // engine's depth-buffer reporting. The callback accepts any
        // DDPF_ZBUFFER entry, so the single justified entry is enough.
        uint32_t pf = scratch(DDPF_SIZE);
        if (!pf)
            com_ret(c, E_OUTOFMEMORY);
        else {
            wr32(pf + DDPF_OFF_dwSize, DDPF_SIZE);
            wr32(pf + DDPF_OFF_dwFlags, DDPF_ZBUFFER);
            wr32(pf + DDPF_OFF_dwFourCC, 0);
            wr32(pf + DDPF_OFF_dwRGBBitCount, 16);
            wr32(pf + 0x10, 0);      // dwStencilBitDepth (union with RBitMask)
            wr32(pf + 0x14, 0xffff); // dwZBitMask (union with GBitMask)
            wr32(pf + 0x18, 0);      // dwStencilBitMask
            wr32(pf + 0x1c, 0);
            guest_call(c, cb, pf, ctx);
            com_ret(c, D3D_OK_);
        }
    }
}

void D3D7_EvictManagedTextures(X86 *c) {
    // No host-side managed texture cache exists yet; there is nothing to drop.
    // When textures forward to wgpu this must evict the upload cache by
    // revision (d3d.cpp's EvictManagedTextures is the model).
    com_ret(c, this_d3d7(c) ? D3D_OK_ : DDERR_INVALIDOBJECT);
}

void D3D7_CreateVertexBuffer(X86 *c) {
    ComObj *d3d = this_d3d7(c);
    uint32_t desc = arg(c, 1), out = arg(c, 2), flags = arg(c, 3);
    (void)flags;
    if (!d3d || !desc || !out || !gm_valid(desc, D3DVERTEXBUFFERDESC_SIZE) || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    uint32_t fvf = rd32(desc + D3DVBD_OFF_dwFVF);
    uint32_t count = rd32(desc + D3DVBD_OFF_dwNumVertices);
    uint32_t stride = fvf_stride(fvf);
    if (!stride || !count || (uint64_t)stride * count > 0xffffffffu) {
        log_once("d3d7.vb.format", "d3d7: CreateVertexBuffer refuses FVF %08x (%u verts)", fvf,
                 count);
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    uint32_t bytes = stride * count;
    uint32_t data = heap_alloc(bytes, true, 16);
    if (!data) {
        com_ret(c, E_OUTOFMEMORY);
        return;
    }
    // No count*stride overflow: bytes is a 32-bit value that fits the arena
    // because heap_alloc refuses anything larger.
    ComObj *vb = com_new(K_D3D7VB);
    vb->pixels = data;
    vb->pixels_bytes = bytes;
    vb->owns_pixels = true;
    vb->vb_fvf = fvf;
    vb->vb_num_vertices = count;
    vb->vb_caps = rd32(desc + D3DVBD_OFF_dwCaps);
    com_out_ptr(out, com_view(vb, IF_D3DVERTEXBUFFER7));
    LOGV("d3d7: created a vertex buffer FVF %08x, %u vertices, %u bytes", fvf, count, bytes);
    com_ret(c, D3D_OK_);
}

void D3D7_CreateDevice(X86 *c) {
    ComObj *d3d = this_d3d7(c);
    uint32_t guid = arg(c, 1), surf = arg(c, 2), out = arg(c, 3);
    if (!d3d || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    com_out_ptr(out, 0);
    ComObj *target = surf ? com_this(surf) : nullptr;
    if (!target || target->kind != K_SURFACE) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    bool tnl = guid_eq(guid, IID_IDirect3DTnLHalDevice_);
    bool hal = guid_eq(guid, IID_IDirect3DHALDevice_);
    if (!tnl && !hal) {
        log_once("d3d7.device.guid", "d3d7: CreateDevice for an unrecognised device class");
        com_ret(c, DDERR_NOTFOUND);
        return;
    }
    ComObj *dev = com_new(K_D3D7DEVICE);
    dev->d3d7 = std::make_shared<D3dHostDevice7State>();
    dev->d3d7->d3d_obj = d3d->id;
    recomp::d3d::d3d7_set_render_target(*dev->d3d7->value, target->id);
    // ddraw.cpp finds the Rust target through the surface: the guest bytes are
    // authoritative, and this is how a Flip/Lock/Blt knows to reconcile first.
    target->d3d7_target_device = dev->id;
    memcpy(dev->d3d7->device_guid, tnl ? IID_IDirect3DTnLHalDevice_ : IID_IDirect3DHALDevice_, 16);
    dev->d3d7->tnl = tnl;
    com_addref(target);
    com_out_ptr(out, com_view(dev, IF_D3DDEVICE7));
    LOGV("d3d7: created a %s device on the %ux%ux%u surface #%u", tnl ? "TnL" : "HAL",
         target->width, target->height, target->bpp, target->id);
    com_ret(c, D3D_OK_);
}

// ---------------------------------------------------------------- device
void Device7_GetCaps(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t out = arg(c, 1);
    if (!dev || !out || !gm_valid(out, D3DDEVICEDESC7_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    fill_device_desc7(out, dev->d3d7->device_guid, dev->d3d7->tnl);
    com_ret(c, D3D_OK_);
}

void Device7_EnumTextureFormats(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t cb = arg(c, 1), ctx = arg(c, 2);
    if (!dev || !cb) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    // The engine's callback at 0x85dc20 keeps the last 16-bit R5G6B5, the last
    // 16-bit A4R4G4B4 and the last 32-bit ARGB it sees, then creates every
    // surface from those globals. Report only those three.
    //
    // DXT1/DXT3/DXT5 are deliberately absent. From the callback's own logic
    // (0x85dc20): every branch is guarded by `dwRGBBitCount` (param[3] == 0x10
    // or 0x20) and only records into g_texfmt16_565, g_texfmt16_Alpha or
    // g_texfmt32_ARGB. A DDPF_FOURCC entry reports dwRGBBitCount == 0, so the
    // callback returns DDENUMRET_OK and records nothing. Adding a DXT entry
    // would therefore change no engine state; it would only be noise. The
    // engine's DXT surfaces are created directly through DirectDraw
    // CreateSurface (fn_0087F6D0) and decoded by this shim's Blt; they are not
    // selected from this list. A format the renderer can decode is not a
    // reason to advertise a format the guest would ignore.
    struct Fmt {
        uint32_t flags, bits, r, g, b, a;
    };
    // Order is fixed and asserted by the tests: R5G6B5, A4R4G4B4, A8R8G8B8.
    const Fmt fmts[] = {
        {DDPF_RGB, 16, 0x7c00, 0x03e0, 0x001f, 0},                                   // R5G6B5
        {DDPF_RGB | DDPF_ALPHAPIXELS, 16, 0x0f00, 0x00f0, 0x000f, 0xf000},           // A4R4G4B4
        {DDPF_RGB | DDPF_ALPHAPIXELS, 32, 0xff0000, 0x00ff00, 0x0000ff, 0xff000000}, // A8R8G8B8
    };
    for (const Fmt &f : fmts) {
        uint32_t pf = scratch(DDPF_SIZE);
        if (!pf)
            break;
        wr32(pf + DDPF_OFF_dwSize, DDPF_SIZE);
        wr32(pf + DDPF_OFF_dwFlags, f.flags);
        wr32(pf + DDPF_OFF_dwFourCC, 0);
        wr32(pf + DDPF_OFF_dwRGBBitCount, f.bits);
        wr32(pf + DDPF_OFF_dwRBitMask, f.r);
        wr32(pf + DDPF_OFF_dwGBitMask, f.g);
        wr32(pf + DDPF_OFF_dwBBitMask, f.b);
        wr32(pf + DDPF_OFF_dwRGBAlphaBitMask, f.a);
        if (guest_call(c, cb, pf, ctx) != DDENUMRET_OK)
            break;
    }
    com_ret(c, D3D_OK_);
}

void Device7_BeginScene(X86 *c) {
    ComObj *dev = this_device7(c);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    if (recomp::d3d::d3d7_in_scene(*dev->d3d7->value)) {
        com_ret(c, D3DERR_SCENEINSCENE);
        return;
    }
    recomp::d3d::d3d7_set_in_scene(*dev->d3d7->value, true);
    host_d3d7_begin_scene();
    trace_frame_begin();
    // The Rust draw path requires an open scene. The host exists from the
    // first Clear onward; a scene opened before that is only recorded (the
    // draws that need it come after the host exists).
    if (dev->d3d7_host)
        host_require(recomp::d3d::begin_scene(*dev->d3d7_host->value), "BeginScene");
    com_ret(c, D3D_OK_);
}

void Device7_EndScene(X86 *c) {
    ComObj *dev = this_device7(c);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    if (!recomp::d3d::d3d7_in_scene(*dev->d3d7->value)) {
        com_ret(c, D3DERR_SCENENOTINSCENE);
        return;
    }
    recomp::d3d::d3d7_set_in_scene(*dev->d3d7->value, false);
    if (dev->d3d7_host)
        host_require(recomp::d3d::end_scene(*dev->d3d7_host->value), "EndScene");
    trace_frame_end();
    com_ret(c, D3D_OK_);
}

void Device7_GetDirect3D(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t out = arg(c, 1);
    if (!dev || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    ComObj *d3d = com_get(dev->d3d7->d3d_obj);
    if (!d3d) {
        com_out_ptr(out, 0);
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    com_addref(d3d);
    com_out_ptr(out, com_view(d3d, IF_D3D7));
    com_ret(c, D3D_OK_);
}

void Device7_SetTransform(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t state = arg(c, 1), matrix = arg(c, 2);
    if (!dev || !state || state >= 256 || !matrix || !gm_valid(matrix, 64)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto values =
        rust::Slice<const float>(reinterpret_cast<const float *>(gm_ptr(matrix)), 16);
    if (!host_ok(recomp::d3d::d3d7_set_transform(*dev->d3d7->value, state, values),
                 "SetTransform")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!recomp::d3d::d3d7_recording(*dev->d3d7->value))
        forward_transform(dev, state);
    trace_transform(dev, state);
    com_ret(c, D3D_OK_);
}

void Device7_GetTransform(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t state = arg(c, 1), out = arg(c, 2);
    if (!dev || !state || state >= 256 || !out || !gm_valid(out, 64)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto values = recomp::d3d::d3d7_get_transform(*dev->d3d7->value, state);
    if (!host_ok(values.status, "GetTransform") || values.values.size() != 16) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    memcpy(gm_ptr(out), values.values.data(), 64);
    com_ret(c, D3D_OK_);
}

void Device7_SetViewport(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t vp = arg(c, 1);
    if (!dev || !vp || !gm_valid(vp, D3DVIEWPORT7_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    recomp::d3d::Viewport viewport{};
    memcpy(&viewport, gm_ptr(vp), D3DVIEWPORT7_SIZE);
    if (!host_ok(recomp::d3d::d3d7_set_viewport(*dev->d3d7->value, viewport), "SetViewport")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!recomp::d3d::d3d7_recording(*dev->d3d7->value))
        forward_viewport(dev);
    com_ret(c, D3D_OK_);
}

void Device7_GetViewport(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t out = arg(c, 1);
    if (!dev || !out || !gm_valid(out, D3DVIEWPORT7_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto result = recomp::d3d::d3d7_get_viewport(*dev->d3d7->value);
    if (!host_ok(result.status, "GetViewport")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    memcpy(gm_ptr(out), &result.viewport, D3DVIEWPORT7_SIZE);
    com_ret(c, D3D_OK_);
}

void Device7_SetMaterial(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t m = arg(c, 1);
    if (!dev || !m || !gm_valid(m, D3DMATERIAL7_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    std::array<uint32_t, 17> values{};
    memcpy(values.data(), gm_ptr(m), D3DMATERIAL7_SIZE);
    if (!host_ok(recomp::d3d::d3d7_set_material(
                     *dev->d3d7->value, rust::Slice<const uint32_t>(values.data(), values.size())),
                 "SetMaterial")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!recomp::d3d::d3d7_recording(*dev->d3d7->value))
        forward_material(dev);
    com_ret(c, D3D_OK_);
}

void Device7_GetMaterial(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t out = arg(c, 1);
    if (!dev || !out || !gm_valid(out, D3DMATERIAL7_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto values = recomp::d3d::d3d7_get_material(*dev->d3d7->value);
    if (!host_ok(values.status, "GetMaterial") || values.values.size() != 17) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    memcpy(gm_ptr(out), values.values.data(), D3DMATERIAL7_SIZE);
    com_ret(c, D3D_OK_);
}

void Device7_SetLight(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t idx = arg(c, 1), data = arg(c, 2);
    if (!dev || idx >= 8 || !data || !gm_valid(data, D3DLIGHT7_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    std::array<uint32_t, 26> values{};
    memcpy(values.data(), gm_ptr(data), D3DLIGHT7_SIZE);
    if (!host_ok(
            recomp::d3d::d3d7_set_light(*dev->d3d7->value, idx,
                                        rust::Slice<const uint32_t>(values.data(), values.size())),
            "SetLight")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!recomp::d3d::d3d7_recording(*dev->d3d7->value))
        forward_light(dev, idx);
    com_ret(c, D3D_OK_);
}

void Device7_GetLight(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t idx = arg(c, 1), out = arg(c, 2);
    if (!dev || idx >= 8 || !out || !gm_valid(out, D3DLIGHT7_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto values = recomp::d3d::d3d7_get_light(*dev->d3d7->value, idx);
    if (!host_ok(values.status, "GetLight") || values.values.size() != 26) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    memcpy(gm_ptr(out), values.values.data(), D3DLIGHT7_SIZE);
    com_ret(c, D3D_OK_);
}

void Device7_SetRenderState(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t state = arg(c, 1), value = arg(c, 2);
    if (!dev || state >= 256) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!host_ok(recomp::d3d::d3d7_set_render_state(*dev->d3d7->value, state, value),
                 "SetRenderState")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!recomp::d3d::d3d7_recording(*dev->d3d7->value))
        forward_render_state(dev, state, value);
    com_ret(c, D3D_OK_);
}

void Device7_GetRenderState(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t state = arg(c, 1), out = arg(c, 2);
    if (!dev || state >= 256 || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto value = recomp::d3d::d3d7_get_render_state(*dev->d3d7->value, state);
    if (!host_ok(value.status, "GetRenderState")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    wr32(out, value.value);
    com_ret(c, D3D_OK_);
}

void Device7_GetTextureStageState(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t stage = arg(c, 1), type = arg(c, 2), out = arg(c, 3);
    if (!dev || stage >= 8 || type >= 256 || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto value = recomp::d3d::d3d7_get_texture_stage_state(*dev->d3d7->value, stage, type);
    if (!host_ok(value.status, "GetTextureStageState")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    wr32(out, value.value);
    com_ret(c, D3D_OK_);
}

void Device7_SetTextureStageState(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t stage = arg(c, 1), type = arg(c, 2), value = arg(c, 3);
    // The engine sets type 0 twice at 0x82ccd2 (a type the D3DTSS enum does
    // not name). Refusing it would leave the engine's cache at -1 and reissue
    // the call forever, so every in-range type is stored; Get returns it.
    if (!dev || stage >= 8 || type >= 256) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!host_ok(recomp::d3d::d3d7_set_texture_stage_state(*dev->d3d7->value, stage, type, value),
                 "SetTextureStageState")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!recomp::d3d::d3d7_recording(*dev->d3d7->value))
        forward_texture_stage_state(dev, stage, type, value);
    com_ret(c, D3D_OK_);
}

void Device7_ValidateDevice(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t out = arg(c, 1);
    if (!dev || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    // The front end has no fixed-function validation to do yet, so it accepts
    // every combination. The engine probes blend modes with this; returning a
    // failing count would make it discard a mode it later needs.
    wr32(out, 1);
    com_ret(c, D3D_OK_);
}

void Device7_GetTexture(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t stage = arg(c, 1), out = arg(c, 2);
    if (!dev || stage >= 8 || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto value = recomp::d3d::d3d7_get_texture(*dev->d3d7->value, stage);
    ComObj *tex = com_get(value.value);
    if (tex)
        com_addref(tex);
    wr32(out, tex ? com_view(tex, IF_DDSURFACE7) : 0);
    com_ret(c, D3D_OK_);
}

void Device7_SetTexture(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t stage = arg(c, 1), ptr = arg(c, 2);
    if (!dev || stage >= 8) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    ComObj *tex = ptr ? com_this(ptr) : nullptr;
    if (ptr && (!tex || tex->kind != K_SURFACE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (tex)
        com_addref(tex);
    auto result = recomp::d3d::d3d7_set_texture(*dev->d3d7->value, stage, tex ? tex->id : 0);
    if (!host_ok(result.status, "SetTexture")) {
        if (tex)
            com_release(tex);
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (ComObj *old = com_get(result.old_id))
        com_release(old);
    // Forward the bind now if the host exists; draws re-sync anyway so a
    // surface written after this call is still current.
    d3d7_sync_texture(dev, stage);
    if (tex)
        host_d3d7_texture();
    com_ret(c, D3D_OK_);
}

void Device7_LightEnable(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t idx = arg(c, 1), enable = arg(c, 2);
    if (!dev || idx >= 8) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (!host_ok(recomp::d3d::d3d7_set_light_enable(*dev->d3d7->value, idx, enable ? 1u : 0u),
                 "LightEnable")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (dev->d3d7_host && !recomp::d3d::d3d7_recording(*dev->d3d7->value))
        host_require(recomp::d3d::light_enable(*dev->d3d7_host->value, idx, enable != 0),
                     "LightEnable");
    com_ret(c, D3D_OK_);
}

void Device7_GetLightEnable(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t idx = arg(c, 1), out = arg(c, 2);
    if (!dev || idx >= 8 || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto value = recomp::d3d::d3d7_get_light_enable(*dev->d3d7->value, idx);
    if (!host_ok(value.status, "GetLightEnable")) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    wr32(out, value.value);
    com_ret(c, D3D_OK_);
}

void Device7_GetRenderTarget(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t out = arg(c, 1);
    if (!dev || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    ComObj *s = com_get(recomp::d3d::d3d7_get_render_target(*dev->d3d7->value));
    if (s)
        com_addref(s);
    wr32(out, s ? com_view(s, IF_DDSURFACE7) : 0);
    com_ret(c, D3D_OK_);
}

// `IDirect3DDevice7::Clear` for the full-target clear the engine uses. Flags,
// color, z and stencil are passed to the Rust device unchanged; a rectangle
// clear or a flag outside D3DCLEAR_TARGET|ZBUFFER|STENCIL stops loudly rather
// than clearing something the guest did not ask for.
void Device7_Clear(X86 *c) {
    ComObj *dev = this_device7(c);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    uint32_t count = arg(c, 1), rects = arg(c, 2), flags = arg(c, 3), color = arg(c, 4);
    uint32_t zb = arg(c, 5);
    float z;
    memcpy(&z, &zb, 4);
    uint32_t stencil = arg(c, 6);
    if (count || rects) {
        LOGW("d3d7: IDirect3DDevice7::Clear with %u rectangle(s) is not implemented; stopping",
             count);
        fflush(stderr);
        abort();
    }
    if (flags & ~(D3DCLEAR_TARGET | D3DCLEAR_ZBUFFER | D3DCLEAR_STENCIL_)) {
        LOGW("d3d7: IDirect3DDevice7::Clear with flags 0x%x is not implemented; stopping", flags);
        fflush(stderr);
        abort();
    }
    if (ensure_host(dev)) {
        const auto status =
            recomp::d3d::clear(*dev->d3d7_host->value, rust::Slice<const recomp::d3d::Rect>(),
                               flags, color, z, stencil);
        if (host_ok(status, "Clear")) {
            recomp::d3d::d3d7_set_target_dirty(*dev->d3d7->value, true);
            host_d3d7_clear();
            com_ret(c, D3D_OK_);
        } else {
            com_ret(c, D3D8_ERR_INVALIDCALL);
        }
        return;
    }
    needs_render("IDirect3DDevice7", "Clear without a GPU backend");
}

void Device7_BeginStateBlock(X86 *c) {
    ComObj *dev = this_device7(c);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    const auto status = recomp::d3d::d3d7_begin_state_block(*dev->d3d7->value);
    if (!host_ok(status, "BeginStateBlock")) {
        com_ret(c, state_block_hresult(status));
        return;
    }
    com_ret(c, D3D_OK_);
}

void Device7_EndStateBlock(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t out = arg(c, 1);
    if (!dev || !out || !gm_valid(out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const auto result = recomp::d3d::d3d7_end_state_block(*dev->d3d7->value);
    if (!host_ok(result.status, "EndStateBlock")) {
        com_ret(c, state_block_hresult(result.status));
        return;
    }
    wr32(out, result.handle);
    com_ret(c, D3D_OK_);
}

void Device7_ApplyStateBlock(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t handle = arg(c, 1);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    const auto result = recomp::d3d::d3d7_apply_state_block(*dev->d3d7->value, handle);
    if (!host_ok(result.status, "ApplyStateBlock")) {
        com_ret(c, state_block_hresult(result.status));
        return;
    }
    for (size_t stage = 0; stage < result.bindings.new_values.size(); ++stage) {
        const uint32_t old_id = result.bindings.old_values[stage];
        const uint32_t new_id = result.bindings.new_values[stage];
        if (old_id == new_id)
            continue;
        if (ComObj *texture = com_get(new_id))
            com_addref(texture);
        if (ComObj *texture = com_get(old_id))
            com_release(texture);
    }
    replay_state_block(dev, handle);
    for (uint32_t stage = 0; stage < 8; ++stage)
        if (recomp::d3d::d3d7_state_block_texture_was_set(*dev->d3d7->value, handle, stage))
            d3d7_sync_texture(dev, stage);
    com_ret(c, D3D_OK_);
}

void Device7_DeleteStateBlock(X86 *c) {
    ComObj *dev = this_device7(c);
    uint32_t handle = arg(c, 1);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    const auto result = recomp::d3d::d3d7_delete_state_block(*dev->d3d7->value, handle);
    if (!host_ok(result.status, "DeleteStateBlock")) {
        com_ret(c, state_block_hresult(result.status));
        return;
    }
    for (uint32_t id : result.values)
        if (ComObj *texture = com_get(id))
            com_release(texture);
    com_ret(c, D3D_OK_);
}

// ---------------------------------------------------------------- draws
// D3D7 submits the same primitive types and FVF values as D3D8; the Rust
// device owns index expansion and the fixed-function pipeline. `prims` is the
// primitive count the renderer expects (D3D7 passes a vertex/index count).
// The strided variants and ProcessVertices stay aborted by name.
bool d3d7_prim_count(uint32_t type, uint32_t count, uint32_t *out) {
    switch (type) {
    case 4: // D3DPT_TRIANGLELIST
        *out = count / 3;
        return true;
    case 2: // D3DPT_TRIANGLESTRIP
    case 6: // D3DPT_TRIANGLEFAN
        *out = count > 2 ? count - 2 : 0;
        return true;
    default:
        return false;
    }
}

// A draw the host renderer refused is never a silent no-op: it is the named
// diagnosis and the run stops. The renderer's own message carries the FVF,
// state or range that was unsupported.
[[noreturn]] void draw_host_stop(const char *method, const recomp::d3d::Status &status) {
    LOGW("d3d7: IDirect3DDevice7::%s host renderer rejected the draw: %.*s; stopping", method,
         static_cast<int>(status.message.size()), status.message.data());
    fflush(stderr);
    abort();
}

void d3d7_submit_primitive(ComObj *dev, uint32_t type, uint32_t fvf, uint32_t verts,
                           uint32_t vertex_bytes, uint32_t stride, uint32_t start_vertex,
                           uint32_t prims, const char *method) {
    if (ensure_host(dev)) {
        d3d7_sync_texture(dev, 0);
        d3d7_sync_texture(dev, 1);
        const auto status = recomp::d3d::draw_primitive(
            *dev->d3d7_host->value, type, fvf,
            rust::Slice<const uint8_t>(gm_ptr(verts), vertex_bytes), stride, start_vertex, prims);
        if (!d3d_bridge_ok(status))
            draw_host_stop(method, status);
        recomp::d3d::d3d7_set_target_dirty(*dev->d3d7->value, true);
        host_d3d7_draw();
        return;
    }
    (void)dev;
    (void)type;
    (void)fvf;
    (void)verts;
    (void)vertex_bytes;
    (void)stride;
    (void)start_vertex;
    (void)prims;
    (void)method;
    needs_render("IDirect3DDevice7", method);
}

void d3d7_submit_indexed(ComObj *dev, uint32_t type, uint32_t fvf, uint32_t verts,
                         uint32_t vertex_bytes, uint32_t stride, uint32_t indices,
                         uint32_t index_bytes, uint32_t base_vertex, uint32_t min_index,
                         uint32_t num_vertices, uint32_t prims, const char *method) {
    if (ensure_host(dev)) {
        d3d7_sync_texture(dev, 0);
        d3d7_sync_texture(dev, 1);
        const auto status = recomp::d3d::draw_indexed_primitive(
            *dev->d3d7_host->value, type, fvf,
            rust::Slice<const uint8_t>(gm_ptr(verts), vertex_bytes), stride,
            rust::Slice<const uint8_t>(gm_ptr(indices), index_bytes), 101, base_vertex, min_index,
            num_vertices, 0, prims);
        if (!d3d_bridge_ok(status))
            draw_host_stop(method, status);
        recomp::d3d::d3d7_set_target_dirty(*dev->d3d7->value, true);
        host_d3d7_draw();
        return;
    }
    (void)dev;
    (void)type;
    (void)fvf;
    (void)verts;
    (void)vertex_bytes;
    (void)stride;
    (void)indices;
    (void)index_bytes;
    (void)base_vertex;
    (void)min_index;
    (void)num_vertices;
    (void)prims;
    (void)method;
    needs_render("IDirect3DDevice7", method);
}

// `(this, D3DPRIMITIVETYPE, fvf, vertices, vertex_count, flags)`. Lists,
// strips and fans are accepted (see d3d7_prim_count); other types stop by name.
void Device7_DrawPrimitive(X86 *c) {
    ComObj *dev = this_device7(c);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    const uint32_t type = arg(c, 1), fvf = arg(c, 2), verts = arg(c, 3), count = arg(c, 4);
    const uint32_t stride = fvf_stride(fvf);
    uint32_t prims = 0;
    const uint64_t bytes = (uint64_t)stride * count;
    if (!stride || !verts || !count || bytes > 0xffffffffu || !gm_valid(verts, (uint32_t)bytes) ||
        !d3d7_prim_count(type, count, &prims)) {
        LOGW("d3d7: DrawPrimitive(type=%u, fvf=%08x, verts=%08x, count=%u) is not a draw this "
             "front end can submit; stopping",
             type, fvf, verts, count);
        fflush(stderr);
        abort();
    }
    d3d7_submit_primitive(dev, type, fvf, verts, (uint32_t)bytes, stride, 0, prims,
                          "DrawPrimitive");
    trace_draw(dev, "DrawPrimitive", type, fvf, verts, count, 0, stride, 0, 0, prims);
    com_ret(c, D3D_OK_);
}

// `(this, type, fvf, vertices, vertex_count, indices, index_count, flags)`.
void Device7_DrawIndexedPrimitive(X86 *c) {
    ComObj *dev = this_device7(c);
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    const uint32_t type = arg(c, 1), fvf = arg(c, 2), verts = arg(c, 3), vcount = arg(c, 4),
                   indices = arg(c, 5), icount = arg(c, 6);
    const uint32_t stride = fvf_stride(fvf);
    uint32_t prims = 0;
    const uint64_t vbytes = (uint64_t)stride * vcount;
    const uint64_t ibytes = (uint64_t)icount * 2;
    if (!stride || !verts || !indices || !vcount || !icount || (type == 4 && (icount % 3)) ||
        vbytes > 0xffffffffu || ibytes > 0xffffffffu || !gm_valid(verts, (uint32_t)vbytes) ||
        !gm_valid(indices, (uint32_t)ibytes) || !d3d7_prim_count(type, icount, &prims)) {
        LOGW("d3d7: DrawIndexedPrimitive(type=%u, fvf=%08x, verts=%08x, vcount=%u, indices=%08x, "
             "icount=%u) is not a draw this front end can submit; stopping",
             type, fvf, verts, vcount, indices, icount);
        fflush(stderr);
        abort();
    }
    d3d7_submit_indexed(dev, type, fvf, verts, (uint32_t)vbytes, stride, indices, (uint32_t)ibytes,
                        0, 0, vcount, prims, "DrawIndexedPrimitive");
    trace_draw(dev, "DrawIndexedPrimitive", type, fvf, verts, vcount, icount, stride, 0, 0, prims);
    com_ret(c, D3D_OK_);
}

// `(this, type, vb, start_vertex, vertex_count, flags)`.
void Device7_DrawPrimitiveVB(X86 *c) {
    ComObj *dev = this_device7(c);
    ComObj *vb = com_this(arg(c, 2));
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    if (!vb || vb->kind != K_D3D7VB) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const uint32_t type = arg(c, 1), start = arg(c, 3), count = arg(c, 4);
    const uint32_t stride = fvf_stride(vb->vb_fvf);
    uint32_t prims = 0;
    const uint64_t end = ((uint64_t)start + count) * stride;
    if (!stride || !vb->pixels || end > vb->pixels_bytes || !d3d7_prim_count(type, count, &prims)) {
        LOGW("d3d7: DrawPrimitiveVB(type=%u, fvf=%08x, start=%u, count=%u) is not a draw this "
             "front end can submit; stopping",
             type, vb->vb_fvf, start, count);
        fflush(stderr);
        abort();
    }
    d3d7_submit_primitive(dev, type, vb->vb_fvf, vb->pixels, vb->pixels_bytes, stride, start, prims,
                          "DrawPrimitiveVB");
    trace_draw(dev, "DrawPrimitiveVB", type, vb->vb_fvf, vb->pixels, count, 0, stride, start,
               vb->id, prims);
    com_ret(c, D3D_OK_);
}

// `(this, type, vb, start_vertex, vertex_count, indices, index_count, flags)`.
// D3D7's indices are relative to StartVertex, which maps to the D3D8 base
// vertex index. `min_index` is 0 because the interval is expressed against the
// start of the VB.
void Device7_DrawIndexedPrimitiveVB(X86 *c) {
    ComObj *dev = this_device7(c);
    ComObj *vb = com_this(arg(c, 2));
    if (!dev) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    if (!vb || vb->kind != K_D3D7VB) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    const uint32_t type = arg(c, 1), start = arg(c, 3), vcount = arg(c, 4), indices = arg(c, 5),
                   icount = arg(c, 6);
    const uint32_t stride = fvf_stride(vb->vb_fvf);
    uint32_t prims = 0;
    const uint64_t end = ((uint64_t)start + vcount) * stride;
    const uint64_t ibytes = (uint64_t)icount * 2;
    if (!stride || !vb->pixels || !indices || !icount || (type == 4 && (icount % 3)) ||
        end > vb->pixels_bytes || ibytes > 0xffffffffu || !gm_valid(indices, (uint32_t)ibytes) ||
        !d3d7_prim_count(type, icount, &prims)) {
        LOGW("d3d7: DrawIndexedPrimitiveVB(type=%u, fvf=%08x, start=%u, vcount=%u, indices=%08x, "
             "icount=%u) is not a draw this front end can submit; stopping",
             type, vb->vb_fvf, start, vcount, indices, icount);
        fflush(stderr);
        abort();
    }
    d3d7_submit_indexed(dev, type, vb->vb_fvf, vb->pixels, vb->pixels_bytes, stride, indices,
                        (uint32_t)ibytes, start, 0, vcount, prims, "DrawIndexedPrimitiveVB");
    trace_draw(dev, "DrawIndexedPrimitiveVB", type, vb->vb_fvf, vb->pixels, vcount, icount, stride,
               start, vb->id, prims);
    com_ret(c, D3D_OK_);
}

// --- Loudly unimplemented device slots. Naming each one is the whole point:
// the abort's message is the diagnosis. ---
#define D3D7_ABORT(fn, method)                                                                     \
    void fn(X86 *) {                                                                               \
        needs_render("IDirect3DDevice7", method);                                                  \
    }

D3D7_ABORT(Device7_SetRenderTarget, "SetRenderTarget")
D3D7_ABORT(Device7_MultiplyTransform, "MultiplyTransform")
D3D7_ABORT(Device7_PreLoad, "PreLoad")
D3D7_ABORT(Device7_SetClipStatus, "SetClipStatus")
D3D7_ABORT(Device7_GetClipStatus, "GetClipStatus")
D3D7_ABORT(Device7_DrawPrimitiveStrided, "DrawPrimitiveStrided")
D3D7_ABORT(Device7_DrawIndexedPrimitiveStrided, "DrawIndexedPrimitiveStrided")
D3D7_ABORT(Device7_ComputeSphereVisibility, "ComputeSphereVisibility")
D3D7_ABORT(Device7_CaptureStateBlock, "CaptureStateBlock")
D3D7_ABORT(Device7_CreateStateBlock, "CreateStateBlock")
D3D7_ABORT(Device7_Load, "Load")
D3D7_ABORT(Device7_SetClipPlane, "SetClipPlane")
D3D7_ABORT(Device7_GetClipPlane, "GetClipPlane")
D3D7_ABORT(Device7_GetInfo, "GetInfo")

// ---------------------------------------------------------------- VB
void VB7_Lock(X86 *c) {
    ComObj *vb = this_vb7(c);
    uint32_t data_out = arg(c, 2), size_out = arg(c, 3);
    if (!vb) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    if (data_out && !gm_valid(data_out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (size_out && !gm_valid(size_out, 4)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    if (data_out)
        wr32(data_out, vb->pixels);
    if (size_out)
        wr32(size_out, vb->pixels_bytes);
    ++vb->lock_count;
    trace_vb_lock(vb, arg(c, 1));
    com_ret(c, D3D_OK_);
}

void VB7_Unlock(X86 *c) {
    ComObj *vb = this_vb7(c);
    if (!vb) {
        com_ret(c, DDERR_INVALIDOBJECT);
        return;
    }
    if (vb->lock_count > 0)
        --vb->lock_count;
    trace_vb_unlock(vb);
    com_ret(c, D3D_OK_);
}

void VB7_GetVertexBufferDesc(X86 *c) {
    ComObj *vb = this_vb7(c);
    uint32_t out = arg(c, 1);
    if (!vb || !out || !gm_valid(out, D3DVERTEXBUFFERDESC_SIZE)) {
        com_ret(c, DDERR_INVALIDPARAMS);
        return;
    }
    wr32(out + D3DVBD_OFF_dwSize, D3DVERTEXBUFFERDESC_SIZE);
    wr32(out + D3DVBD_OFF_dwCaps, vb->vb_caps);
    wr32(out + D3DVBD_OFF_dwFVF, vb->vb_fvf);
    wr32(out + D3DVBD_OFF_dwNumVertices, vb->vb_num_vertices);
    com_ret(c, D3D_OK_);
}

void VB7_Optimize(X86 *c) {
    // The guest buffer is already the only storage; there is no repack to do.
    com_ret(c, this_vb7(c) ? D3D_OK_ : DDERR_INVALIDOBJECT);
}

void VB7_ProcessVertices(X86 *) {
    needs_render("IDirect3DVertexBuffer7", "ProcessVertices");
}
void VB7_ProcessVerticesStrided(X86 *) {
    needs_render("IDirect3DVertexBuffer7", "ProcessVerticesStrided");
}

// ---------------------------------------------------------------- vtables
const ComMethod g_d3d7[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"EnumDevices", 3, D3D7_EnumDevices},
    {"CreateDevice", 4, D3D7_CreateDevice},
    {"CreateVertexBuffer", 4, D3D7_CreateVertexBuffer},
    {"EnumZBufferFormats", 4, D3D7_EnumZBufferFormats},
    {"EvictManagedTextures", 1, D3D7_EvictManagedTextures},
};

const ComMethod g_device7[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"GetCaps", 2, Device7_GetCaps},                                         // 0x0c
    {"EnumTextureFormats", 3, Device7_EnumTextureFormats},                   // 0x10
    {"BeginScene", 1, Device7_BeginScene},                                   // 0x14
    {"EndScene", 1, Device7_EndScene},                                       // 0x18
    {"GetDirect3D", 2, Device7_GetDirect3D},                                 // 0x1c
    {"SetRenderTarget", 3, Device7_SetRenderTarget},                         // 0x20
    {"GetRenderTarget", 2, Device7_GetRenderTarget},                         // 0x24
    {"Clear", 7, Device7_Clear},                                             // 0x28
    {"SetTransform", 3, Device7_SetTransform},                               // 0x2c
    {"GetTransform", 3, Device7_GetTransform},                               // 0x30
    {"SetViewport", 2, Device7_SetViewport},                                 // 0x34
    {"MultiplyTransform", 3, Device7_MultiplyTransform},                     // 0x38
    {"GetViewport", 2, Device7_GetViewport},                                 // 0x3c
    {"SetMaterial", 2, Device7_SetMaterial},                                 // 0x40
    {"GetMaterial", 2, Device7_GetMaterial},                                 // 0x44
    {"SetLight", 3, Device7_SetLight},                                       // 0x48
    {"GetLight", 3, Device7_GetLight},                                       // 0x4c
    {"SetRenderState", 3, Device7_SetRenderState},                           // 0x50
    {"GetRenderState", 3, Device7_GetRenderState},                           // 0x54
    {"BeginStateBlock", 1, Device7_BeginStateBlock},                         // 0x58
    {"EndStateBlock", 2, Device7_EndStateBlock},                             // 0x5c
    {"PreLoad", 2, Device7_PreLoad},                                         // 0x60
    {"DrawPrimitive", 6, Device7_DrawPrimitive},                             // 0x64
    {"DrawIndexedPrimitive", 8, Device7_DrawIndexedPrimitive},               // 0x68
    {"SetClipStatus", 2, Device7_SetClipStatus},                             // 0x6c
    {"GetClipStatus", 2, Device7_GetClipStatus},                             // 0x70
    {"DrawPrimitiveStrided", 6, Device7_DrawPrimitiveStrided},               // 0x74
    {"DrawIndexedPrimitiveStrided", 8, Device7_DrawIndexedPrimitiveStrided}, // 0x78
    {"DrawPrimitiveVB", 6, Device7_DrawPrimitiveVB},                         // 0x7c
    {"DrawIndexedPrimitiveVB", 8, Device7_DrawIndexedPrimitiveVB},           // 0x80
    {"ComputeSphereVisibility", 6, Device7_ComputeSphereVisibility},         // 0x84
    {"GetTexture", 3, Device7_GetTexture},                                   // 0x88
    {"SetTexture", 3, Device7_SetTexture},                                   // 0x8c
    {"GetTextureStageState", 4, Device7_GetTextureStageState},               // 0x90
    {"SetTextureStageState", 4, Device7_SetTextureStageState},               // 0x94
    {"ValidateDevice", 2, Device7_ValidateDevice},                           // 0x98
    {"ApplyStateBlock", 2, Device7_ApplyStateBlock},                         // 0x9c
    {"CaptureStateBlock", 2, Device7_CaptureStateBlock},                     // 0xa0
    {"DeleteStateBlock", 2, Device7_DeleteStateBlock},                       // 0xa4
    {"CreateStateBlock", 3, Device7_CreateStateBlock},                       // 0xa8
    {"Load", 6, Device7_Load},                                               // 0xac
    {"LightEnable", 3, Device7_LightEnable},                                 // 0xb0
    {"GetLightEnable", 3, Device7_GetLightEnable},                           // 0xb4
    {"SetClipPlane", 3, Device7_SetClipPlane},                               // 0xb8
    {"GetClipPlane", 3, Device7_GetClipPlane},                               // 0xbc
    {"GetInfo", 4, Device7_GetInfo},                                         // 0xc0
};

const ComMethod g_vb7[] = {
    {"QueryInterface", 3, com_QueryInterface},
    {"AddRef", 1, com_AddRef},
    {"Release", 1, com_Release},
    {"Lock", 4, VB7_Lock},                                     // 0x0c
    {"Unlock", 1, VB7_Unlock},                                 // 0x10
    {"ProcessVertices", 8, VB7_ProcessVertices},               // 0x14
    {"GetVertexBufferDesc", 2, VB7_GetVertexBufferDesc},       // 0x18
    {"Optimize", 3, VB7_Optimize},                             // 0x1c
    {"ProcessVerticesStrided", 8, VB7_ProcessVerticesStrided}, // 0x20
};

} // namespace

bool d3d7_stage_surface(ComObj *s) {
    ComObj *dev = s ? com_get(s->d3d7_target_device) : nullptr;
    if (!dev || dev->kind != K_D3D7DEVICE || !dev->d3d7_host ||
        !recomp::d3d::d3d7_target_dirty(*dev->d3d7->value) || dev->d3d7_native_handoff_failed ||
        (s->bpp != 16 && s->bpp != 32) || s->width != dev->d3d7_width ||
        s->height != dev->d3d7_height)
        return false;
    const auto result = recomp::d3d::present_handoff(*dev->d3d7_host->value, s->bpp);
    if (host_ok(result.status, "PresentHandoff")) {
        void *native = result.handoff.texture;
        uint32_t *busy = result.handoff.busy;
        const uint32_t w = result.handoff.width, h = result.handoff.height;
        ddraw_external_present_begin();
        if (host_display_stage_native_texture(native, (int)w, (int)h, busy,
                                              result.handoff.ready_event,
                                              result.handoff.ready_value))
            return true;
        __atomic_store_n(busy, 0u, __ATOMIC_RELEASE);
    }
    dev->d3d7_native_handoff_failed = true;
    log_once("d3d7.present.handoff",
             "d3d7: native present handoff unavailable; using CPU presentation");
    return false;
}

// Called by ddraw.cpp before it reads or presents a surface the D3D7 device
// renders into. A no-op unless the surface has a live D3D7 target.
void d3d7_flush_surface(ComObj *s) {
    if (!s || s->kind != K_SURFACE || !s->d3d7_target_device)
        return;
    ComObj *dev = com_get(s->d3d7_target_device);
    if (dev && dev->kind == K_D3D7DEVICE)
        d3d7_writeback(dev);
}

// ---------------------------------------------------------------- translation
D3d7StateMap d3d7_translate_render_state(uint32_t d3d7_state, uint32_t value,
                                         uint32_t *d3d8_state) {
    const auto result = recomp::d3d::translate_render_state(d3d7_state, value);
    if (d3d8_state)
        *d3d8_state = result.state;
    switch (result.kind) {
    case recomp::d3d::D3d7TranslationKind::Forward:
        return D3D7_STATE_FORWARD;
    case recomp::d3d::D3d7TranslationKind::Ignore:
        return D3D7_STATE_IGNORE;
    case recomp::d3d::D3d7TranslationKind::Invalid:
        return D3D7_STATE_INVALID;
    }
    return D3D7_STATE_INVALID;
}

bool d3d7_translate_transform(uint32_t d3d7_state, uint32_t *d3d8_state) {
    const auto result = recomp::d3d::translate_transform(d3d7_state);
    if (result.valid && d3d8_state)
        *d3d8_state = result.state;
    return result.valid;
}

int d3d7_translate_texture_stage_state(uint32_t d3d7_type, uint32_t out[2]) {
    const auto result = recomp::d3d::translate_texture_stage_state(d3d7_type);
    if (result.count > 0 && out) {
        out[0] = result.first;
        if (result.count > 1)
            out[1] = result.second;
    }
    return result.count;
}

uint16_t d3d7_rgb888_to_rgb565(uint32_t argb) {
    const uint32_t r = (argb >> 16) & 0xff, g = (argb >> 8) & 0xff, b = argb & 0xff;
    // Truncating shift, not bit replication or rounding: this is the copy from
    // the 32-bit internal target to the 16bpp guest back buffer, and a real
    // 16bpp target would have truncated the same low bits when it stored the
    // clear color or rasterized a pixel.
    return (uint16_t)(((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3));
}

uint32_t d3d7_rgb565_to_rgb888(uint16_t rgb565) {
    const uint32_t r = (rgb565 >> 11) & 0x1f, g = (rgb565 >> 5) & 0x3f, b = rgb565 & 0x1f;
    // The same `*255/max` expansion the headless presenter uses for a 16bpp
    // frame, so a written-back pixel round-trips to the color the PNG shows.
    // Bit replication ((r<<3)|(r>>2)) agrees only at the endpoints; documented
    // in docs/d3d7-inventory.md.
    const uint32_t R = (r * 255) / 31, G = (g * 255) / 63, B = (b * 255) / 31;
    return 0xff000000u | (R << 16) | (G << 8) | B;
}

// See dx.h. The trace's vertex dump and dx_tests share this one decoder.
std::string d3d7_trace_vertex(uint32_t fvf, const uint8_t *v) {
    return format_vertex(fvf, v);
}

// See dx.h. Grammar: "a-b" (inclusive), "a-" (a to open end), "-b" (1 to b),
// "a" (exactly a). Frame numbers are 1-based, so 0 is rejected.
bool d3d7_trace_parse_frames(const char *s, uint32_t *lo, uint32_t *hi) {
    if (!s || !s[0] || !lo || !hi)
        return false;
    char *end = nullptr;
    if (s[0] == '-') {
        unsigned long b = strtoul(s + 1, &end, 10);
        if (end == s + 1 || *end || b == 0 || b > 0xffffffffu)
            return false;
        *lo = 1;
        *hi = (uint32_t)b;
        return true;
    }
    unsigned long a = strtoul(s, &end, 10);
    if (end == s || a == 0 || a > 0xffffffffu)
        return false;
    if (*end == '\0') {
        *lo = *hi = (uint32_t)a;
        return true;
    }
    if (*end != '-')
        return false;
    const char *rest = end + 1;
    if (*rest == '\0') {
        *lo = (uint32_t)a;
        *hi = 0xffffffffu;
        return true;
    }
    unsigned long b = strtoul(rest, &end, 10);
    if (end == rest || *end || b == 0 || b > 0xffffffffu || b < a)
        return false;
    *lo = (uint32_t)a;
    *hi = (uint32_t)b;
    return true;
}

// See dx.h. A run starts on the first digest and is reported as changed; equal
// frames extend it silently, and a change emits the previous range (when it
// spanned more than one frame) before the new run starts.
bool d3d7_trace_small_step(D3d7TraceSmallCollapser *c, uint32_t frame, const std::string &digest,
                           std::string *collapsed) {
    if (!c)
        return false;
    if (collapsed)
        collapsed->clear();
    if (!c->active) {
        c->active = true;
        c->digest = digest;
        c->first = c->last = frame;
        return true;
    }
    if (digest == c->digest) {
        c->last = frame;
        return false;
    }
    if (collapsed && c->last > c->first)
        *collapsed =
            "frames " + std::to_string(c->first) + "-" + std::to_string(c->last) + ": unchanged";
    c->digest = digest;
    c->first = c->last = frame;
    return true;
}

// See dx.h. Closes a run left open when the trace stops or the device resets;
// returns true when a collapse line was produced.
bool d3d7_trace_small_flush(D3d7TraceSmallCollapser *c, std::string *collapsed) {
    if (!c || !c->active)
        return false;
    bool emitted = false;
    if (collapsed) {
        collapsed->clear();
        if (c->last > c->first) {
            *collapsed = "frames " + std::to_string(c->first) + "-" + std::to_string(c->last) +
                         ": unchanged";
            emitted = true;
        }
    }
    c->active = false;
    return emitted;
}

// Releasing a device also releases the render target and every bound texture
// it retained. Registered with com_set_destructor so com_destroy runs it.
void d3d7_device_destroy(ComObj *dev) {
    dev->d3d7_host.reset();
    if (!dev->d3d7)
        return;
    const auto retained = recomp::d3d::d3d7_release_refs(*dev->d3d7->value);
    for (uint32_t id : retained.values)
        if (ComObj *texture = com_get(id))
            com_release(texture);
    const uint32_t target_id = recomp::d3d::d3d7_get_render_target(*dev->d3d7->value);
    if (target_id) {
        if (ComObj *s = com_get(target_id)) {
            if (s->d3d7_target_device == dev->id)
                s->d3d7_target_device = 0;
            com_release(s);
        }
        recomp::d3d::d3d7_set_render_target(*dev->d3d7->value, 0);
    }
    dev->d3d7.reset();
}

void d3d7_vb_destroy(ComObj *vb) {
    if (vb->pixels && vb->owns_pixels) {
        heap_free(vb->pixels);
        vb->pixels = 0;
        vb->pixels_bytes = 0;
    }
}

void d3d7_reset() {
    g_scratch = 0;
    g_scratch_size = 0;
    trace_reset();
}

void d3d7_register() {
    static bool done = false;
    if (done)
        return;
    done = true;
    com_define_checked<IF_D3D7, g_d3d7>("DDRAW.dll", "IDirect3D7");
    com_define_checked<IF_D3DDEVICE7, g_device7>("DDRAW.dll", "IDirect3DDevice7");
    com_define_checked<IF_D3DVERTEXBUFFER7, g_vb7>("DDRAW.dll", "IDirect3DVertexBuffer7");
    // IDirect3D7 is an interface on the DirectDraw object: one refcount and
    // one controlling IUnknown, exactly like IDirect3D3.
    com_bind(IF_D3D7, K_DDRAW);
    com_bind(IF_D3DDEVICE7, K_D3D7DEVICE);
    com_register_iid(IF_D3D7, IID_IDirect3D7_);
    com_set_destructor(K_D3D7DEVICE, d3d7_device_destroy);
    com_set_destructor(K_D3D7VB, d3d7_vb_destroy);
    // Register the VB's own IID too, so naming it in a log is not a hex dump.
    com_register_iid(IF_D3DVERTEXBUFFER7, IID_IDirect3DVertexBuffer7_);
}
