// Text for guest canvases: a TrueType face the program registered when its font
// names one, the bundled stand-in when it names a Windows interface face, else
// fixed 8x16 bitmap cells. No host font services or guest
// pointers escape this file; callbacks receive temporary guest-heap records.
#include "gdi32_internal.h"
#include <cstdlib>
#include "../platform/os.h"
#include "gdi32_font8x16.h"
#include "gdi32_truetype.h"
#include "memory.h"
#include "win32.h"
#include <algorithm>
#include <cstring>
#include <climits>
using namespace gdi;
namespace {
uint32_t font_word(const Object &font, int offset) {
    uint32_t v;
    memcpy(&v, font.logfont.data() + offset, 4);
    return v;
}
int32_t font_scale(uint32_t dc) {
    auto *d = dc_of(dc);
    auto it = objects().find(d ? d->font : 0);
    int64_t height = it == objects().end() ? 16 : int32_t(font_word(it->second, 0));
    return int32_t(std::max<int64_t>(1, std::abs(height) / 16));
}
void create_font(X86 *c) {
    uint32_t p = arg(c, 0);
    if (!p || !gm_valid(p, 92)) {
        set_eax(c, 0);
        return;
    }
    Object font;
    font.kind = Object::Font;
    memcpy(font.logfont.data(), g_mem + p, 92);
    set_eax(c, make_object(font));
}
// LOGFONTA is the 60-byte prefix of LOGFONTW with a CHAR face name; the stored
// font always uses the wide layout, so widen the name into the 92-byte record.
void create_font_a(X86 *c) {
    uint32_t p = arg(c, 0);
    if (!p || !gm_valid(p, 60)) {
        set_eax(c, 0);
        return;
    }
    Object font;
    font.kind = Object::Font;
    memcpy(font.logfont.data(), g_mem + p, 28);
    for (size_t i = 0; i < 32; ++i) {
        uint8_t ch = g_mem[p + 28 + i];
        font.logfont[28 + 2 * i] = ch;
        if (!ch)
            break;
    }
    set_eax(c, make_object(font));
}
// CreateFontA passes the LOGFONT fields as arguments: five LONGs, eight BYTEs
// and the face name, stored in the same wide record as create_font_a.
void create_font_args(X86 *c) {
    Object font;
    font.kind = Object::Font;
    for (int i = 0; i < 5; ++i) {
        uint32_t v = arg(c, i);
        memcpy(font.logfont.data() + 4 * i, &v, 4);
    }
    for (int i = 0; i < 8; ++i)
        font.logfont[20 + i] = (uint8_t)arg(c, 5 + i);
    const std::string face = gm_str(arg(c, 13), 31);
    for (size_t i = 0; i < face.size(); ++i)
        font.logfont[28 + 2 * i] = (uint8_t)face[i];
    set_eax(c, make_object(font));
}
// TEXTMETRICW is 60 bytes on x86: eleven DWORDs, four UTF-16
// characters, five BYTE fields and three bytes of trailing padding.
void metrics(uint32_t out, int32_t scale, uint32_t weight, bool wide = true) {
    memset(g_mem + out, 0, wide ? 60 : 56);
    wr32(out, 16u * scale);
    wr32(out + 4, 13u * scale);
    wr32(out + 8, 3u * scale);
    wr32(out + 20, 8u * scale);
    wr32(out + 24, 8u * scale);
    wr32(out + 28, weight);
    wr32(out + 36, 96);
    wr32(out + 40, 96);
    for (uint32_t i = 0; i < 4; ++i) {
        const uint8_t chars[] = {32, 127, '?', ' '};
        if (wide)
            wr16(out + 44 + i * 2, chars[i]);
        else
            wr8(out + 44 + i, chars[i]);
    }
    wr8(out + (wide ? 55 : 51), 0x30);
}
// All wide text entry points share these bitmap glyphs and DC colour/clip rules.
void glyph(uint32_t hdc, int64_t x, int64_t y, uint32_t ch, bool underline = false) {
    auto *dc = dc_of(hdc);
    if (ch < 32 || ch > 127)
        ch = '?';
    int64_t scale = font_scale(hdc), width = 8 * scale, height = 16 * scale;
    Rect clip = clip_box(hdc);
    uint32_t fg = argb(dc->text_color), bg = argb(dc->bk_color);
    for (int64_t yy = std::max<int64_t>(y, clip.t); yy < std::min<int64_t>(y + height, clip.b);
         ++yy)
        for (int64_t xx = std::max<int64_t>(x, clip.l); xx < std::min<int64_t>(x + width, clip.r);
             ++xx) {
            uint8_t row = recomp_font::font8x16[ch - 32][size_t((yy - y) / scale)];
            if ((row & (0x80 >> ((xx - x) / scale))) || (underline && yy - y >= 14 * scale))
                write_pixel(hdc, xx, yy, fg);
            else if (dc->bk_mode == 2)
                write_pixel(hdc, xx, yy, bg);
        }
}
// How one DC's text is measured and drawn: from the registered TrueType face
// its font names, or from the bitmap cells.
struct Pen {
    const TrueTypeFace *face = nullptr;
    double scale = 0;
    int64_t cells = 1;
    TrueTypeMetrics metrics;
    int64_t advance(uint32_t ch) const {
        return face ? truetype_advance(face, scale, ch) : 8 * cells;
    }
    int64_t height() const {
        return face ? metrics.height : 16 * cells;
    }
    void draw(uint32_t hdc, int64_t x, int64_t y, uint32_t ch, bool underline) const {
        if (!face) {
            glyph(hdc, x, y, ch, underline);
            return;
        }
        auto *dc = dc_of(hdc);
        if (!dc)
            return;
        const uint32_t fg = argb(dc->text_color) & 0xffffffu;
        const int64_t width = advance(ch);
        if (dc->bk_mode == 2)
            fill(hdc, {int32_t(x), int32_t(y), int32_t(x + width), int32_t(y + metrics.height)},
                 argb(dc->bk_color));
        const TrueTypeGlyph &g = truetype_glyph(face, scale, ch);
        const int64_t baseline = y + metrics.ascent;
        for (int32_t j = 0; j < g.h; ++j)
            for (int32_t i = 0; i < g.w; ++i)
                if (uint8_t cover = g.coverage[size_t(j) * size_t(g.w) + size_t(i)])
                    write_pixel(hdc, x + g.x + i, baseline + g.y + j, (uint32_t(cover) << 24) | fg,
                                true);
        if (underline)
            for (int64_t yy = baseline + 1;
                 yy <= baseline + std::max<int64_t>(1, metrics.height / 16); ++yy)
                for (int64_t xx = x; xx < x + width; ++xx)
                    write_pixel(hdc, xx, yy, 0xff000000u | fg);
    }
};
Pen pen_of(uint32_t hdc) {
    Pen pen;
    pen.cells = font_scale(hdc);
    auto *dc = dc_of(hdc);
    auto it = objects().find(dc ? dc->font : 0);
    if (it == objects().end() || it->second.kind != Object::Font)
        return pen;
    std::string family;
    for (size_t i = 0; i < 32; ++i) {
        uint16_t unit =
            uint16_t(it->second.logfont[28 + 2 * i] | (it->second.logfont[29 + 2 * i] << 8));
        if (!unit)
            break;
        family.push_back(unit < 128 ? char(unit) : '?');
    }
    if (family.empty())
        return pen;
    pen.face =
        truetype_find(family, int32_t(font_word(it->second, 16)), it->second.logfont[20] != 0);
    if (!pen.face) {
        // A face every Windows has and the program never supplied: what it
        // gets with no font of its own set, from the VCL's default font.
        const int32_t weight = int32_t(font_word(it->second, 16));
        pen.face = truetype_windows_substitute(family, weight);
        if (pen.face)
            log_once(("gdi-font:" + family).c_str(),
                     "gdi: \"%s\" is a Windows font the program did not register; drawing it "
                     "with the bundled Open Sans",
                     family.c_str());
    }
    if (!pen.face)
        return pen;
    pen.scale = truetype_scale(pen.face, int32_t(font_word(it->second, 0)));
    pen.metrics = truetype_metrics(pen.face, pen.scale);
    return pen;
}
template <bool Wide> void get_metrics(X86 *c) {
    auto *dc = dc_of(arg(c, 0));
    uint32_t out = arg(c, 1);
    if (!dc || !out || !gm_valid(out, Wide ? 60 : 56)) {
        set_eax(c, 0);
        return;
    }
    auto font = objects().find(dc->font);
    uint32_t weight = font == objects().end() ? 400 : font_word(font->second, 16);
    if (!weight)
        weight = 400;
    metrics(out, font_scale(arg(c, 0)), weight, Wide);
    const Pen pen = pen_of(arg(c, 0));
    if (pen.face) {
        wr32(out, uint32_t(pen.metrics.height));
        wr32(out + 4, uint32_t(pen.metrics.ascent));
        wr32(out + 8, uint32_t(pen.metrics.descent));
        wr32(out + 12, uint32_t(pen.metrics.internal));
        wr32(out + 16, uint32_t(pen.metrics.external));
        wr32(out + 20, uint32_t(pen.metrics.average));
        wr32(out + 24, uint32_t(pen.metrics.maximum));
    }
    set_eax(c, 1);
}
void metrics_a(uint8_t *out, uint32_t weight, const Pen &pen) {
    auto put32 = [&](size_t at, uint32_t value) { memcpy(out + at, &value, 4); };
    put32(0, uint32_t(pen.metrics.height));
    put32(4, uint32_t(pen.metrics.ascent));
    put32(8, uint32_t(pen.metrics.descent));
    put32(12, uint32_t(pen.metrics.internal));
    put32(16, uint32_t(pen.metrics.external));
    put32(20, uint32_t(pen.metrics.average));
    put32(24, uint32_t(pen.metrics.maximum));
    put32(28, weight);
    put32(36, 96);
    put32(40, 96);
    out[44] = 32;
    out[45] = 127;
    out[46] = '?';
    out[47] = ' ';
    out[51] = 0x30;
}
void outline_text_metrics(X86 *c) {
    auto *dc = dc_of(arg(c, 0));
    uint32_t size = arg(c, 1), out = arg(c, 2);
    if (!dc) {
        set_eax(c, 0);
        return;
    }
    const Pen pen = pen_of(arg(c, 0));
    if (!pen.face) {
        set_eax(c, 0);
        return;
    }
    constexpr size_t fixed_size = 212;
    const std::string &family = truetype_family(pen.face);
    const std::string &style = truetype_style(pen.face);
    const std::string &full_name = truetype_full_name(pen.face);
    const size_t required = fixed_size + (family.size() + 1) * 2 +
                            (style.empty() ? 0 : style.size() + 1) +
                            (full_name.empty() ? 0 : full_name.size() + 1);
    if (!out) {
        set_eax(c, uint32_t(required));
        return;
    }
    if (!gm_valid(out, size)) {
        set_eax(c, 0);
        return;
    }
    std::vector<uint8_t> data(required, 0);
    auto put32 = [&](size_t at, uint32_t value) { memcpy(data.data() + at, &value, 4); };
    const size_t copied = std::min<size_t>(size, data.size());
    put32(0, uint32_t(copied));
    auto font = objects().find(dc->font);
    uint32_t weight = font == objects().end() ? 400 : font_word(font->second, 16);
    if (!weight)
        weight = 400;
    metrics_a(data.data() + 4, weight, pen);
    const TrueTypeOutlineMetrics om = truetype_outline_metrics(pen.face, pen.scale);
    memcpy(data.data() + 61, om.panose, sizeof(om.panose));
    put32(72, om.selection);
    put32(76, om.type);
    put32(80, uint32_t(om.slope_rise));
    put32(84, uint32_t(om.slope_run));
    put32(88, uint32_t(om.italic_angle));
    put32(92, om.em_square);
    put32(96, uint32_t(om.ascent));
    put32(100, uint32_t(om.descent));
    put32(104, uint32_t(om.line_gap));
    put32(116, uint32_t(om.x_min));
    put32(120, uint32_t(om.y_max));
    put32(124, uint32_t(om.x_max));
    put32(128, uint32_t(om.y_min));
    put32(132, uint32_t(om.mac_ascent));
    put32(136, uint32_t(om.mac_descent));
    put32(140, uint32_t(om.mac_line_gap));
    put32(144, om.minimum_ppem);
    put32(148, uint32_t(om.subscript_x_size));
    put32(152, uint32_t(om.subscript_y_size));
    put32(156, uint32_t(om.subscript_x_offset));
    put32(160, uint32_t(om.subscript_y_offset));
    put32(164, uint32_t(om.superscript_x_size));
    put32(168, uint32_t(om.superscript_y_size));
    put32(172, uint32_t(om.superscript_x_offset));
    put32(176, uint32_t(om.superscript_y_offset));
    put32(180, uint32_t(om.strikeout_size));
    put32(184, uint32_t(om.strikeout_position));
    put32(188, uint32_t(om.underline_size));
    put32(192, uint32_t(om.underline_position));
    size_t names_at = fixed_size;
    auto append_name = [&](size_t field, const std::string &name) {
        if (name.empty())
            return;
        put32(field, uint32_t(names_at));
        memcpy(data.data() + names_at, name.c_str(), name.size() + 1);
        names_at += name.size() + 1;
    };
    append_name(196, family);
    append_name(200, family);
    append_name(204, style);
    append_name(208, full_name);
    memcpy(g_mem + out, data.data(), copied);
    set_eax(c, uint32_t(copied));
}
void char_width32(X86 *c) {
    uint32_t dc = arg(c, 0), first = arg(c, 1), last = arg(c, 2), out = arg(c, 3);
    if (!dc_of(dc) || last < first || !out || last - first > GUEST_SIZE / 4 - 1 ||
        !gm_valid(out, (last - first + 1) * 4)) {
        set_eax(c, 0);
        return;
    }
    const Pen pen = pen_of(dc);
    for (uint32_t ch = first; ch <= last; ++ch)
        wr32(out + (ch - first) * 4, uint32_t(pen.advance(ch)));
    set_eax(c, 1);
}
void char_abc_widths(X86 *c) {
    uint32_t dc = arg(c, 0), first = arg(c, 1), last = arg(c, 2), out = arg(c, 3);
    if (!dc_of(dc) || last < first || !out || last - first > GUEST_SIZE / 12 - 1 ||
        !gm_valid(out, (last - first + 1) * 12)) {
        set_eax(c, 0);
        return;
    }
    const Pen pen = pen_of(dc);
    for (uint32_t ch = first; ch <= last; ++ch) {
        uint32_t at = out + (ch - first) * 12;
        int32_t bearing = 0, width = int32_t(pen.advance(ch));
        if (pen.face) {
            const auto &glyph = truetype_glyph(pen.face, pen.scale, ch);
            bearing = glyph.x;
            width = glyph.w;
        }
        wr32(at, uint32_t(bearing));
        wr32(at + 4, uint32_t(width));
        wr32(at + 8, uint32_t(pen.advance(ch) - bearing - width));
    }
    set_eax(c, 1);
}
template <bool Wide> void extent(X86 *c) {
    uint32_t dc = arg(c, 0), text = arg(c, 1), n = arg(c, 2), out = arg(c, 3);
    if (!dc_of(dc) || !out || !gm_valid(out, 8) || n > GUEST_SIZE / 2 ||
        (n && (!text || !gm_valid(text, n * (Wide ? 2 : 1))))) {
        set_eax(c, 0);
        return;
    }
    const Pen pen = pen_of(dc);
    uint64_t width = 0;
    for (uint32_t i = 0; i < n; ++i)
        width += uint64_t(pen.advance(Wide ? rd16(text + 2 * i) : rd8(text + i)));
    if (width > INT_MAX) {
        set_eax(c, 0);
        return;
    }
    wr32(out, uint32_t(width));
    wr32(out + 4, uint32_t(pen.height()));
    set_eax(c, 1);
}
void text_out(X86 *c, uint32_t hdc, int32_t x_origin, int32_t y_origin, uint32_t flags, uint32_t rp,
              uint32_t text, uint32_t n, uint32_t dx, bool wide) {
    auto *dc = dc_of(hdc);
    int w, h;
    if (!dc || !dc_size(hdc, &w, &h) || n > GUEST_SIZE / 4 ||
        (n && (!text || !gm_valid(text, n * (wide ? 2 : 1)))) || (dx && !gm_valid(dx, n * 4)) ||
        ((flags & 6) && (!rp || !gm_valid(rp, 16)))) {
        set_eax(c, 0);
        return;
    }
    bool old_clipped = dc->clipped;
    std::vector<Rect> old_clip = dc->clip;
    Rect rect{};
    if (flags & 6)
        rect = {int32_t(rd32(rp)), int32_t(rd32(rp + 4)), int32_t(rd32(rp + 8)),
                int32_t(rd32(rp + 12))};
    if (flags & 4) {
        Rect cut = to_device(hdc, rect);
        std::vector<Rect> source = dc->clipped ? dc->clip : std::vector<Rect>{{0, 0, w, h}};
        dc->clip.clear();
        dc->clipped = true;
        for (Rect r : source) {
            Rect hit{std::max(r.l, cut.l), std::max(r.t, cut.t), std::min(r.r, cut.r),
                     std::min(r.b, cut.b)};
            if (hit.l < hit.r && hit.t < hit.b)
                dc->clip.push_back(hit);
        }
    }
    uint32_t bg = argb(dc->bk_color);
    if (flags & 2)
        fill(hdc, rect, bg);
    const Pen pen = pen_of(hdc);
    int64_t x = x_origin, y = y_origin;
    for (uint32_t i = 0; i < n; ++i) {
        uint32_t unit = wide ? rd16(text + 2 * i) : rd8(text + i);
        pen.draw(hdc, x, y, unit, false);
        x += dx ? int32_t(rd32(dx + 4 * i)) : pen.advance(unit);
    }
    dc->clip = std::move(old_clip);
    dc->clipped = old_clipped;
    set_eax(c, 1);
}
void text_out_a(X86 *c) {
    text_out(c, arg(c, 0), int32_t(arg(c, 1)), int32_t(arg(c, 2)), 0, 0, arg(c, 3), arg(c, 4), 0,
             false);
}
void text_out_w(X86 *c) {
    text_out(c, arg(c, 0), int32_t(arg(c, 1)), int32_t(arg(c, 2)), 0, 0, arg(c, 3), arg(c, 4), 0,
             true);
}
void ext_text_out_a(X86 *c) {
    text_out(c, arg(c, 0), int32_t(arg(c, 1)), int32_t(arg(c, 2)), arg(c, 3), arg(c, 4), arg(c, 5),
             arg(c, 6), arg(c, 7), false);
}
void ext_text_out_w(X86 *c) {
    text_out(c, arg(c, 0), int32_t(arg(c, 1)), int32_t(arg(c, 2)), arg(c, 3), arg(c, 4), arg(c, 5),
             arg(c, 6), arg(c, 7), true);
}
// The fonts are kept, so a DC whose font names one of them draws with it. Data
// the rasterizer cannot read still gets a handle, as it always has here.
void memory_font(X86 *c) {
    uint32_t data = arg(c, 0), size = arg(c, 1), count = arg(c, 3);
    if (count && !gm_valid(count, 4)) {
        set_eax(c, 0);
        return;
    }
    uint32_t fonts =
        data && size && gm_valid(data, size) ? truetype_add_memory(g_mem + data, size) : 0;
    Object font;
    font.kind = Object::Font;
    uint32_t handle = make_object(font);
    if (count)
        wr32(count, std::max<uint32_t>(fonts, 1));
    set_eax(c, handle);
}
void enumerate(X86 *c) {
    uint32_t cb = arg(c, 2), param = arg(c, 3);
    if (!dc_of(arg(c, 0)) || !cb) {
        set_eax(c, 0);
        return;
    }
    uint32_t data = heap_alloc(92 + 60, true);
    if (!data) {
        set_eax(c, 0);
        return;
    }
    wr32(data, 16);
    wr32(data + 4, 8);
    wr32(data + 16, 400);
    wr8(data + 27, 0x30);
    gm_put_wstr(data + 28, "recomp", 32);
    metrics(data + 92, 1, 400);
    uint32_t result = guest_call(c, cb, data, data + 92, 1, param);
    heap_free(data);
    set_eax(c, result);
}
void unsupported(X86 *c) {
    set_eax(c, 0);
}
} // namespace
namespace gdi {
// DrawText's basic VCL layout uses the same fixed font as ExtTextOutW.
// Measurement must use that font too; CALCRECT never mutates canvas pixels.
uint32_t draw_text(uint32_t hdc, uint32_t text, uint32_t count, uint32_t rp, uint32_t flags) {
    auto *dc = dc_of(hdc);
    if (recomp_env("TRACE_GDI")) {
        int dw = -1, dh = -1;
        dc_size(hdc, &dw, &dh);
        LOGW("gdi: DrawText hdc=%08x(%dx%d)%s flags=%08x \"%s\"", hdc, dw, dh,
             dc ? "" : " NO SUCH DC", flags, gm_wstr(text, 64).c_str());
    }
    if (!dc || !rp || !gm_valid(rp, 16) || !text)
        return 0;
    if (count == UINT32_MAX) {
        count = 0;
        while (uint64_t(text) + 2ull * count + 2 <= GUEST_SIZE && gm_valid(text + count * 2, 2) &&
               rd16(text + count * 2))
            ++count;
    }
    if (!count || count > GUEST_SIZE / 2 || !gm_valid(text, count * 2))
        return 0;
    Rect rect{int32_t(rd32(rp)), int32_t(rd32(rp + 4)), int32_t(rd32(rp + 8)),
              int32_t(rd32(rp + 12))};
    const Pen pen = pen_of(hdc);
    const int64_t ch = pen.height() + ((flags & 0x200) ? pen.metrics.external : 0);
    struct Cell {
        uint16_t ch;
        bool underline;
    };
    std::vector<std::vector<Cell>> lines(1);
    auto width_of = [&](const std::vector<Cell> &line) {
        int64_t width = 0;
        for (Cell cell : line)
            width += pen.advance(cell.ch);
        return width;
    };
    bool prefix = false;
    for (uint32_t i = 0; i < count; ++i) {
        uint16_t unit = rd16(text + 2 * i);
        if (!(flags & 0x800) && unit == '&') { // DT_NOPREFIX
            if (i + 1 < count && rd16(text + 2 * (i + 1)) == '&')
                ++i;
            else {
                prefix = true;
                continue;
            }
        }
        if (unit == '\r' || unit == '\n') {
            if (unit == '\r' && i + 1 < count && rd16(text + 2 * (i + 1)) == '\n')
                ++i;
            if (!(flags & 0x20)) {
                lines.emplace_back();
                prefix = false;
                continue;
            }
            unit = ' ';
        }
        if (unit == '\t' && (flags & 0x40)) {
            size_t spaces = 8 - lines.back().size() % 8;
            lines.back().insert(lines.back().end(), spaces, Cell{' ', false});
        } else
            lines.back().push_back({unit, prefix && !(flags & 0x100000)});
        prefix = false;
    }
    if ((flags & 0x10) && !(flags & 0x20) && rect.r > rect.l) { // DT_WORDBREAK
        const int64_t limit = int64_t(rect.r) - rect.l;
        for (size_t i = 0; i < lines.size(); ++i) {
            auto &line = lines[i];
            size_t cols = 0; // how many characters fit the width
            for (int64_t used = 0; cols < line.size(); ++cols) {
                used += pen.advance(line[cols].ch);
                if (used > limit)
                    break;
            }
            if (!cols || cols >= line.size())
                continue;
            size_t split = std::min(cols, line.size() - 1);
            while (split && line[split].ch != ' ')
                --split;
            if (!split) { // A long word is never split in the middle.
                split = cols;
                while (split < line.size() && line[split].ch != ' ')
                    ++split;
            }
            if (split < line.size()) {
                std::vector<Cell> tail(line.begin() + split + 1, line.end());
                line.resize(split);
                lines.insert(lines.begin() + i + 1, std::move(tail));
            }
        }
    }
    int64_t height = ch * lines.size(), maxwidth = 0;
    for (const auto &line : lines)
        maxwidth = std::max(maxwidth, width_of(line));
    if (height > INT_MAX || maxwidth > INT_MAX)
        return 0;
    if (flags & 0x400) {
        wr32(rp + 8, uint32_t(int64_t(rect.l) + maxwidth));
        wr32(rp + 12, uint32_t(int64_t(rect.t) + height));
        return uint32_t(height);
    }
    int w, h;
    if (!dc_size(hdc, &w, &h))
        return 0;
    bool old_clipped = dc->clipped;
    std::vector<Rect> old_clip = dc->clip;
    if (!(flags & 0x100)) { // DT_NOCLIP
        Rect cut = to_device(hdc, rect);
        std::vector<Rect> source = dc->clipped ? dc->clip : std::vector<Rect>{{0, 0, w, h}};
        dc->clip.clear();
        dc->clipped = true;
        for (Rect r : source) {
            Rect hit{std::max(r.l, cut.l), std::max(r.t, cut.t), std::min(r.r, cut.r),
                     std::min(r.b, cut.b)};
            if (hit.l < hit.r && hit.t < hit.b)
                dc->clip.push_back(hit);
        }
    }
    int64_t y = rect.t;
    if (flags & 0x20) {
        if (flags & 4)
            y += (int64_t(rect.b) - rect.t - height) / 2;
        else if (flags & 8)
            y = int64_t(rect.b) - height;
    }
    for (const auto &line : lines) {
        int64_t x = rect.l, width = width_of(line);
        if (flags & 1)
            x += (int64_t(rect.r) - rect.l - width) / 2;
        else if (flags & 2)
            x = int64_t(rect.r) - width;
        for (Cell cell : line) {
            pen.draw(hdc, x, y, cell.ch, cell.underline);
            x += pen.advance(cell.ch);
        }
        y += ch;
    }
    dc->clip = std::move(old_clip);
    dc->clipped = old_clipped;
    return uint32_t(y - rect.t);
}
void register_text() {
#define G(n, a, f)                                                                                 \
    {                                                                                              \
        "GDI32.dll", n, a, f                                                                       \
    }
    static const ImportShim shims[] = {G("CreateFontIndirectW", 1, create_font),
                                       G("CreateFontIndirectA", 1, create_font_a),
                                       G("CreateFontA", 14, create_font_args),
                                       G("TextOutA", 5, text_out_a),
                                       G("TextOutW", 5, text_out_w),
                                       G("ExtTextOutA", 8, ext_text_out_a),
                                       G("ExtTextOutW", 8, ext_text_out_w),
                                       G("GetTextExtentPoint32W", 4, extent<true>),
                                       G("GetTextExtentPoint32A", 4, extent<false>),
                                       G("GetTextExtentPointA", 4, extent<false>),
                                       G("GetTextExtentPointW", 4, extent<true>),
                                       G("GetTextMetricsW", 2, get_metrics<true>),
                                       G("GetTextMetricsA", 2, get_metrics<false>),
                                       G("GetOutlineTextMetricsA", 3, outline_text_metrics),
                                       G("GetCharABCWidthsA", 4, char_abc_widths),
                                       G("GetCharWidth32A", 4, char_width32),
                                       G("AddFontMemResourceEx", 4, memory_font),
                                       G("EnumFontsW", 4, enumerate),
                                       G("EnumFontFamiliesExW", 5, enumerate),
                                       G("CreateEnhMetaFileW", 4, unsupported),
                                       G("CreateEnhMetaFileA", 4, unsupported),
                                       G("CloseEnhMetaFile", 1, unsupported),
                                       G("DeleteEnhMetaFile", 1, unsupported),
                                       G("GetEnhMetaFileW", 1, unsupported),
                                       G("GetEnhMetaFileA", 1, unsupported),
                                       G("GetEnhMetaFileBits", 3, unsupported),
                                       G("GetEnhMetaFileHeader", 3, unsupported),
                                       G("GetEnhMetaFileDescriptionW", 3, unsupported),
                                       G("GetEnhMetaFileDescriptionA", 3, unsupported),
                                       G("GetEnhMetaFilePaletteEntries", 3, unsupported),
                                       G("GetEnhMetaFilePixelFormat", 3, unsupported),
                                       G("SetEnhMetaFileBits", 2, unsupported),
                                       G("SetWinMetaFileBits", 4, unsupported),
                                       G("GetWinMetaFileBits", 5, unsupported),
                                       G("PlayEnhMetaFile", 3, unsupported),
                                       G("CopyEnhMetaFileW", 2, unsupported),
                                       G("CreateDCW", 4, unsupported),
                                       G("CreateICW", 4, unsupported),
                                       G("StartDocW", 2, unsupported),
                                       G("EndDoc", 1, unsupported),
                                       G("StartPage", 1, unsupported),
                                       G("EndPage", 1, unsupported),
                                       G("AbortDoc", 1, unsupported),
                                       G("SetAbortProc", 2, unsupported)};
#undef G
    imports_register(shims, sizeof(shims) / sizeof(shims[0]));
}
} // namespace gdi
