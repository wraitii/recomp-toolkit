const KCOMPOSITOR: &str = recomp_compositor::gpu::COMPOSITOR_SHADER;
const KHUD: &str = r#"
diagnostic(off, derivative_uniformity);
@group(0) @binding(0) var<storage, read> r: vec4f;
@group(0) @binding(16) var t: texture_2d<f32>;
@group(0) @binding(24) var smp: sampler;
struct V2F { @builtin(position) pos: vec4f, @location(0) uv: vec2f };
@vertex fn vs_main(@builtin(vertex_index) vid: u32) -> V2F {
    let q = vec2f(f32(vid & 1u), f32(vid >> 1u));
    var o: V2F;
    o.pos = vec4f(r.xy + q * r.zw, 0, 1);
    o.uv = q;
    return o;
}
@fragment fn fs_main(i: V2F) -> @location(0) vec4f {
    return textureSample(t, smp, i.uv);
}
"#;
const KSURFACEUPLOAD: &str = r#"
@group(0) @binding(4) var<storage, read> words: array<u32>;
@group(0) @binding(5) var<storage, read> p: array<vec4u, 5>;
@group(0) @binding(6) var<storage, read> palette: array<u32, 256>;
@vertex fn vs_main(@builtin(vertex_index) vid: u32) -> @builtin(position) vec4f {
    let corner = vec2f(f32((vid << 1u) & 2u), f32(vid & 2u));
    return vec4f(corner * 2.0 - 1.0, 0, 1);
}
fn byte_at(off: u32) -> u32 { return (words[off >> 2u] >> ((off & 3u) * 8u)) & 255u; }
@fragment fn fs_main(@builtin(position) frag: vec4f) -> @location(0) vec4f {
    let xy = vec2u(frag.xy) * p[0].xy / p[0].zw;
    let bpp = p[1].y;
    var step = 4u;
    if (bpp == 8u) { step = 1u; } else if (bpp <= 16u) { step = 2u; }
    let offset = xy.y * p[1].x + xy.x * step;
    var rgb: vec3u;
    if (bpp == 8u) {
        let index = byte_at(offset);
        var c = index * 0x010101u;
        if (p[1].z != 0u) { c = palette[index]; }
        rgb = vec3u((c >> 16u) & 255u, (c >> 8u) & 255u, c & 255u);
    } else {
        var value = byte_at(offset) | (byte_at(offset + 1u) << 8u);
        if (bpp > 16u) { value = value | (byte_at(offset + 2u) << 16u) | (byte_at(offset + 3u) << 24u); }
        rgb = ((vec3u(value) & p[2].xyz) >> p[3].xyz) * 255u / p[4].xyz;
    }
    return vec4f(vec3f(rgb) / 255.0, 1);
}
"#;
const KD3D: &str = r#"
diagnostic(off, derivative_uniformity);
struct HVertex { x: f32, y: f32, z: f32, w: f32, u: f32, v: f32, r: f32, g: f32, b: f32, a: f32,
                 sr: f32, sg: f32, sb: f32, sa: f32 };
struct Uniforms {
    mvp: mat4x4f,
    pretransformed: u32, textured: u32, texblend: u32, alphatest: u32, alphafunc: u32,
    alpharef: f32,
    specular: u32, texture_has_alpha: u32, fogmode: u32,
    fogstart: f32, fogend: f32, fogdensity: f32, fogr: f32, fogg: f32, fogb: f32, pointsize: f32,
};
@group(0) @binding(0) var<storage, read> verts: array<HVertex>;
@group(0) @binding(1) var<storage, read> vu: Uniforms;
@group(0) @binding(5) var<storage, read> u: Uniforms;
@group(0) @binding(16) var tex: texture_2d<f32>;
@group(0) @binding(24) var tex_s: sampler;
struct V2F {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
    @location(2) spec: vec4f,
    @location(3) fogdist: f32,
};
@vertex fn vs_main(@builtin(vertex_index) vid: u32) -> V2F {
    let s = verts[vid];
    var pos = vu.mvp * vec4f(s.x, s.y, s.z, 1.0);
    if (vu.pretransformed != 0u) { pos = vec4f(s.x, s.y, s.z, s.w); }
    var o: V2F;
    o.pos = pos;
    o.uv = vec2f(s.u, s.v);
    o.color = vec4f(s.r, s.g, s.b, s.a);
    o.spec = vec4f(s.sr, s.sg, s.sb, s.sa);
    o.fogdist = abs(pos.w);
    return o;
}
fn sat3(x: vec3f) -> vec3f { return clamp(x, vec3f(0.0), vec3f(1.0)); }
struct PSOut { @location(0) color: vec4f, @location(1) coverage: f32 };
@fragment fn fs_main(i: V2F) -> PSOut {
    var c = i.color;
    var t = textureSample(tex, tex_s, i.uv);
    if (u.textured != 0u) {
        var ta = i.color.a;
        if (u.texture_has_alpha != 0u) { ta = t.a; }
        switch (u.texblend) {
            case 1u, 7u: { c = t; }
            case 2u: { c = vec4f(t.rgb * i.color.rgb, ta); }
            case 3u: { c = vec4f(mix(i.color.rgb, t.rgb, t.a), i.color.a); }
            case 5u: { c = vec4f(t.rgb, i.color.a); }
            case 8u: { c = vec4f(sat3(t.rgb + i.color.rgb), i.color.a); }
            case 4u: { c = vec4f(t.rgb * i.color.rgb, t.a * i.color.a); }
            default: { c = vec4f(t.rgb * i.color.rgb, ta); }
        }
    }
    if (u.specular != 0u) { c = vec4f(sat3(c.rgb + i.spec.rgb), c.a); }
    if (u.alphatest != 0u) {
        var alpha_pass = true;
        switch (u.alphafunc) {
            case 1u: { alpha_pass = false; }
            case 2u: { alpha_pass = c.a < u.alpharef; }
            case 3u: { alpha_pass = c.a == u.alpharef; }
            case 4u: { alpha_pass = c.a <= u.alpharef; }
            case 5u: { alpha_pass = c.a > u.alpharef; }
            case 6u: { alpha_pass = c.a != u.alpharef; }
            case 7u: { alpha_pass = c.a >= u.alpharef; }
            default: { alpha_pass = true; }
        }
        if (!alpha_pass) { discard; }
    }
    if (u.fogmode != 0u) {
        var f = 1.0;
        let d = i.fogdist;
        switch (u.fogmode) {
            case 1u: { f = i.spec.a; }
            case 2u: { f = exp(-u.fogdensity * d); }
            case 3u: { f = exp(-(u.fogdensity * d) * (u.fogdensity * d)); }
            default: { f = (u.fogend - d) / max(u.fogend - u.fogstart, 1e-6); }
        }
        f = clamp(f, 0.0, 1.0);
        c = vec4f(mix(vec3f(u.fogr, u.fogg, u.fogb), c.rgb, f), c.a);
    }
    var o: PSOut;
    o.color = c;
    o.coverage = 1.0;
    return o;
}
"#;
const KGUESTREADBACK: &str = r#"
@group(0) @binding(0) var<storage, read_write> output_words: array<vec2u>;
@group(0) @binding(1) var<storage, read> p: array<vec4u, 3>;
@group(0) @binding(16) var color: texture_2d<f32>;
@group(0) @binding(17) var coverage: texture_2d<f32>;
@compute @workgroup_size(8, 8) fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let tid = gid.xy;
    let span = p[0].zw - p[0].xy;
    if (any(tid >= span)) { return; }
    let guest = p[0].xy + tid;
    let native_size = p[1].xy;
    let guest_size = p[1].zw;
    let pos = min(((2u * guest + 1u) * native_size) / (2u * guest_size), p[2].yz - 1u);
    let rgb = vec3u(round(textureLoad(color, vec2i(pos), 0).rgb * 255.0));
    var covered = 0u;
    if (textureLoad(coverage, vec2i(pos), 0).r > 0.0) { covered = 1u; }
    output_words[p[2].x + tid.y * span.x + tid.x] =
        vec2u(rgb.b | (rgb.g << 8u) | (rgb.r << 16u) | (covered << 24u), 0u);
}
"#;
const KGUESTREADBACKFUSED: &str = r#"
@group(0) @binding(0) var<storage, read_write> output_words: array<vec2u>;
@group(0) @binding(1) var<storage, read> p: array<vec4u, 3>;
@group(0) @binding(16) var color: texture_2d<f32>;
@group(0) @binding(17) var coverage: texture_2d<f32>;
@compute @workgroup_size(8, 8) fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let tid = gid.xy;
    let span = p[0].zw - p[0].xy;
    if (any(tid >= span)) { return; }
    let guest = p[0].xy + tid;
    let native_size = p[1].xy;
    let guest_size = p[1].zw;
    let pos = min(((2u * guest + 1u) * native_size) / (2u * guest_size), p[2].yz - 1u);
    let rgb = vec3u(round(textureLoad(color, vec2i(pos), 0).rgb * 255.0));
    var covered = 0u;
    if (textureLoad(coverage, vec2i(pos), 0).r > 0.0) { covered = 1u; }
    let lo = (guest * native_size + guest_size - 1u) / guest_size;
    let hi = ((guest + 1u) * native_size + guest_size - 1u) / guest_size;
    var lit = 0u;
    for (var y = lo.y; y < hi.y; y++) {
        for (var x = lo.x; x < hi.x; x++) {
            if (any(round(textureLoad(color, vec2i(i32(x), i32(y)), 0).rgb * 255.0) > vec3f(8.0))) { lit++; }
        }
    }
    output_words[p[2].x + tid.y * span.x + tid.x] =
        vec2u(rgb.b | (rgb.g << 8u) | (rgb.r << 16u) | (covered << 24u), lit);
}
"#;
const KNATIVEBRIGHTNESS: &str = r#"
@group(0) @binding(0) var<storage, read_write> output_words: array<u32>;
@group(0) @binding(1) var<storage, read> p: array<vec4u, 2>;
@group(0) @binding(16) var color: texture_2d<f32>;
var<workgroup> partial: array<u32, 256>;
@compute @workgroup_size(16, 16) fn cs_main(@builtin(global_invocation_id) gid: vec3u,
                                             @builtin(local_invocation_index) li: u32,
                                             @builtin(workgroup_id) wid: vec3u) {
    let pos = p[0].xy + gid.xy * 2u;
    var lit = 0u;
    for (var y = 0u; y < 2u; y++) {
        for (var x = 0u; x < 2u; x++) {
            let q = pos + vec2u(x, y);
            if (all(q < p[0].zw)) {
                if (any(round(textureLoad(color, vec2i(q), 0).rgb * 255.0) > vec3f(8.0))) { lit++; }
            }
        }
    }
    partial[li] = lit;
    workgroupBarrier();
    for (var stride = 128u; stride > 0u; stride = stride >> 1u) {
        if (li < stride) { partial[li] += partial[li + stride]; }
        workgroupBarrier();
    }
    if (li == 0u) {
        output_words[p[1].x + (wid.y * p[1].y + wid.x) * p[1].z] = partial[0];
    }
}
"#;
pub(super) fn source(name: &str) -> Option<&'static str> {
    match name {
        "compositor" => Some(KCOMPOSITOR),
        "hud" => Some(KHUD),
        "surface_upload" => Some(KSURFACEUPLOAD),
        "d3d" => Some(KD3D),
        "guest_readback" => Some(KGUESTREADBACK),
        "guest_readback_fused" => Some(KGUESTREADBACKFUSED),
        "native_brightness" => Some(KNATIVEBRIGHTNESS),
        _ => None,
    }
}
