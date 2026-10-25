use crate::opcodes::*;
use crate::uber_abi::*;

pub fn source() -> String {
    let mut source = format!(
        "const ABI_VERSION: u32 = {ABI_VERSION}u;\n\
         const INTERPRETER_VERSION: u32 = {INTERPRETER_VERSION}u;\n\
         struct Instruction {{ dst: vec4<u32>, a: vec4<u32>, b: vec4<u32>, c: vec4<u32> }}\n\
         struct Program {{ header: vec4<u32>, code: array<Instruction, {MAX_INSTRUCTIONS}> }}\n\
         struct VSP {{ halfpix: vec4<f32>, ascale: array<vec4<f32>, 16> }}\n\
         struct PSP {{ alpha_ref: f32, alpha_func: i32, fog_mode: i32, pad0: i32, fog_color: vec4<f32>, fog_start: f32, fog_end: f32, fog_density: f32, pad1: f32, texture_flags: vec4<u32> }}\n\
         struct V2F {{ @builtin(position) pos: vec4<f32>, @location(0) d0: vec4<f32>, @location(1) d1: vec4<f32>, @location(2) t0: vec4<f32>, @location(3) t1: vec4<f32>, @location(4) t2: vec4<f32>, @location(5) t3: vec4<f32>, @location(6) t4: vec4<f32>, @location(7) t5: vec4<f32>, @location(8) t6: vec4<f32>, @location(9) t7: vec4<f32>, @location(10) fog: f32, @location(11) fogz: f32 }}\n\
         @group(0) @binding(0) var<uniform> vcb: array<vec4<f32>, {VERTEX_CONSTANTS}>;\n\
         @group(0) @binding(1) var<uniform> vsp: VSP;\n\
         @group(0) @binding(2) var<uniform> pcb: array<vec4<f32>, {PIXEL_CONSTANTS}>;\n\
         @group(0) @binding(3) var<uniform> psp: PSP;\n\
         @group(0) @binding(4) var tx0: texture_2d<f32>;\n\
         @group(0) @binding(20) var sm0: sampler;\n\
         @group(0) @binding({}) var<uniform> vp: Program;\n\
         @group(0) @binding({}) var<uniform> pp: Program;\n\
         struct Registers {{ temp: array<vec4<f32>, {TEMP_REGISTERS}>, input: array<vec4<f32>, {VERTEX_INPUTS}>, pos: vec4<f32>, color: vec4<f32>, tex: vec4<f32> }}\n",
        PROGRAM_BINDINGS[0], PROGRAM_BINDINGS[1]
    );
    source += r#"
fn swizzle(v: vec4<f32>, s: u32) -> vec4<f32> {
    return vec4<f32>(v[s & 3u], v[(s >> 2u) & 3u], v[(s >> 4u) & 3u], v[(s >> 6u) & 3u]);
}
fn vertex_read(s: vec4<u32>, r: Registers, immediate: bool) -> vec4<f32> {
    if (immediate) { return bitcast<vec4<f32>>(s); }
    var value = vec4<f32>(0.0);
    switch s.x {
        case 0u: { value = r.temp[s.y]; }
        case 1u: { value = r.input[s.y]; }
        case 2u: { value = vcb[s.y]; }
        default: {}
    }
    return swizzle(value, s.z);
}
fn pixel_read(s: vec4<u32>, r: Registers, immediate: bool) -> vec4<f32> {
    if (immediate) { return bitcast<vec4<f32>>(s); }
    var value = vec4<f32>(0.0);
    switch s.x {
        case 0u: { value = r.temp[s.y]; }
        case 1u: { value = r.input[0]; }
        case 2u: { value = pcb[s.y]; }
        case 3u: { value = r.tex; }
        default: {}
    }
    return swizzle(value, s.z);
}
fn masked(old: vec4<f32>, value: vec4<f32>, mask: u32) -> vec4<f32> {
    return select(old, value, (vec4<u32>(mask) & vec4<u32>(1u, 2u, 4u, 8u)) != vec4<u32>(0u));
}
fn vertex_write(r: ptr<function, Registers>, d: vec4<u32>, value: vec4<f32>) {
    switch d.y {
        case 0u: { (*r).temp[d.z] = masked((*r).temp[d.z], value, d.w); }
        case 4u: { (*r).pos = masked((*r).pos, value, d.w); }
        case 5u: { (*r).color = masked((*r).color, value, d.w); }
        case 6u: { (*r).tex = masked((*r).tex, value, d.w); }
        default: {}
    }
}
fn pixel_write(r: ptr<function, Registers>, d: vec4<u32>, value: vec4<f32>) {
    switch d.y {
        case 0u: { (*r).temp[d.z] = masked((*r).temp[d.z], value, d.w); }
        case 8u: { (*r).color = masked((*r).color, value, d.w); }
        default: {}
    }
}
struct VIn { @location(0) v0: vec4<f32>, @location(1) v1: vec4<f32>, @location(2) v2: vec4<f32> }
@vertex fn vs_main(v: VIn) -> V2F {
    var r: Registers;
    r.input[0] = v.v0 * vsp.ascale[0];
    r.input[1] = v.v1 * vsp.ascale[1];
    r.input[2] = v.v2 * vsp.ascale[2];
"#;
    for index in 0..MAX_INSTRUCTIONS {
        source += &format!(
            "if (vp.header.z > {index}u) {{\n\
             let i = vp.code[{index}];\n\
             let a = vertex_read(i.a, r, (i.dst.x & {}u) != 0u); let b = vertex_read(i.b, r, (i.dst.x & {}u) != 0u); let c = vertex_read(i.c, r, (i.dst.x & {}u) != 0u);\n\
             var value = a;\n\
             switch (i.dst.x & {OPCODE_MASK}u) {{\n\
             case {OP_MOV}u: {{}}\n\
             case {OP_MUL}u: {{ value = a * b; }}\n\
             case {OP_MAD}u: {{ value = a * b + c; }}\n\
             case {OP_DP4}u: {{ value = vec4<f32>(dot(a, b)); }}\n\
             default: {{}}\n\
             }}\n\
             vertex_write(&r, i.dst, value);\n\
             }}\n",
            1u32 << IMMEDIATE_SOURCE_SHIFT,
            1u32 << (IMMEDIATE_SOURCE_SHIFT + 1),
            1u32 << (IMMEDIATE_SOURCE_SHIFT + 2),
        );
    }
    source += r#"
    var o: V2F;
    o.pos = vec4<f32>(r.pos.xy + vsp.halfpix.xy * r.pos.w, r.pos.zw);
    o.d0 = r.color;
    o.t0 = r.tex;
    o.fog = 1.0;
    o.fogz = r.pos.z;
    return o;
}
@fragment fn ps_main(v: V2F) -> @location(0) vec4<f32> {
    var r: Registers;
    r.input[0] = v.d0;
    r.tex = v.t0;
"#;
    for index in 0..MAX_INSTRUCTIONS {
        source += &format!(
            "if (pp.header.z > {index}u) {{\n\
             let i = pp.code[{index}];\n\
             let a = pixel_read(i.a, r, (i.dst.x & {}u) != 0u); let b = pixel_read(i.b, r, (i.dst.x & {}u) != 0u); let c = pixel_read(i.c, r, (i.dst.x & {}u) != 0u);\n\
             var value = a;\n\
             switch (i.dst.x & {OPCODE_MASK}u) {{\n\
             case {OP_MOV}u: {{}}\n\
             case {OP_MUL}u: {{ value = a * b; }}\n\
             case {OP_MAD}u: {{ value = a * b + c; }}\n\
             case {OP_TEX}u: {{ value = textureSample(tx0, sm0, a.xy); if (psp.texture_flags.x != 0u) {{ value = vec4<f32>(value.rgb, 1.0); }} }}\n\
             default: {{}}\n\
             }}\n\
             pixel_write(&r, i.dst, value);\n\
             }}\n",
            1u32 << IMMEDIATE_SOURCE_SHIFT,
            1u32 << (IMMEDIATE_SOURCE_SHIFT + 1),
            1u32 << (IMMEDIATE_SOURCE_SHIFT + 2),
        );
    }
    source += "let col = d9_output(r.color, v);\n\
        if (psp.texture_flags.y != 0u) { return vec4<f32>(mix(col.rgb, vec3<f32>(1.0, 0.0, 0.0), 0.15), col.a); }\n\
        return col;\n}\n";
    source += include_str!("../raster.wgsl");
    source
}
