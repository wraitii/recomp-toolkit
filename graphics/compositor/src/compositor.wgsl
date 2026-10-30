
diagnostic(off, derivative_uniformity);
struct Quad { rect: vec4f, uv: vec4f, drawable: vec2f, opaque: u32, pad: u32 };
@group(0) @binding(0) var<storage, read> vq: Quad;
@group(0) @binding(4) var<storage, read> fq: Quad;
@group(0) @binding(16) var tex: texture_2d<f32>;
@group(0) @binding(24) var smp: sampler;
struct V2F { @builtin(position) pos: vec4f, @location(0) uv: vec2f };
@vertex fn vs_main(@builtin(vertex_index) vid: u32) -> V2F {
    var corners = array<vec2f, 4>(vec2f(0, 0), vec2f(1, 0), vec2f(0, 1), vec2f(1, 1));
    let c = corners[vid];
    let p = vq.rect.xy + c * vq.rect.zw;
    var o: V2F;
    o.pos = vec4f(p.x / vq.drawable.x * 2 - 1, 1 - p.y / vq.drawable.y * 2, 0, 1);
    o.uv = vq.uv.xy + c * vq.uv.zw;
    return o;
}
@fragment fn fs_main(i: V2F) -> @location(0) vec4f {
    var c = textureSample(tex, smp, i.uv);
    if (fq.opaque != 0u) { c.a = 1.0; }
    return c;
}
