use crate::opcodes::*;
use crate::uber_abi::*;

pub fn source() -> String {
    let mut source = format!(
        "struct Instruction {{ dst: vec4<u32>, a: vec4<u32>, b: vec4<u32>, c: vec4<u32> }}\n\
         struct Definition {{ index: vec4<u32>, value: vec4<f32> }}\n\
         struct Program {{ header: vec4<u32>, code: array<Instruction, {MAX_INSTRUCTIONS}>, definitions: array<Definition, {MAX_DEFINITIONS}> }}\n\
         struct VSP {{ halfpix: vec4<f32>, ascale: array<vec4<f32>, 16> }}\n\
         struct PSP {{ alpha_ref: f32, alpha_func: i32, fog_mode: i32, pad0: i32, fog_color: vec4<f32>, fog_start: f32, fog_end: f32, fog_density: f32, pad1: f32, texture_flags: vec4<u32> }}\n\
         struct V2F {{ @builtin(position) pos: vec4<f32>, @location(0) d0: vec4<f32>, @location(1) d1: vec4<f32>, @location(2) t0: vec4<f32>, @location(3) t1: vec4<f32>, @location(4) t2: vec4<f32>, @location(5) t3: vec4<f32>, @location(6) t4: vec4<f32>, @location(7) t5: vec4<f32>, @location(8) t6: vec4<f32>, @location(9) t7: vec4<f32>, @location(10) fog: f32, @location(11) fogz: f32 }}\n\
         @group(0) @binding(0) var<uniform> vcb: array<vec4<f32>, {VERTEX_CONSTANTS}>;\n\
         @group(0) @binding(1) var<uniform> vsp: VSP;\n\
         @group(0) @binding(2) var<uniform> pcb: array<vec4<f32>, {PIXEL_CONSTANTS}>;\n\
         @group(0) @binding(3) var<uniform> psp: PSP;\n\
         @group(0) @binding({}) var<uniform> vp: Program;\n\
         @group(0) @binding({}) var<uniform> pp: Program;\n\
         struct Registers {{ temp: array<vec4<f32>, {TEMP_REGISTERS}>, input: array<vec4<f32>, {VERTEX_INPUTS}>, pos: vec4<f32>, color: array<vec4<f32>, 2>, address: vec4<f32>, tex: array<vec4<f32>, {TEXTURE_COORDINATES}> }}\n",
        PROGRAM_BINDINGS[0], PROGRAM_BINDINGS[1]
    );
    for slot in 0..TEXTURE_SLOTS {
        let kind = if slot < TEXTURES_2D { "2d" } else { "cube" };
        source += &format!(
            "@group(0) @binding({}) var tx{slot}: texture_{kind}<f32>;\n@group(0) @binding({}) var sm{slot}: sampler;\n",
            4 + slot,
            20 + slot
        );
    }
    source += "fn sample_texture(slot: u32, coord: vec4<f32>) -> vec4<f32> { if ((psp.texture_flags.z & (1u << slot)) != 0u) { return vec4<f32>(0.0, 0.0, 0.0, 1.0); } var value = vec4<f32>(0.0); switch slot {\n";
    for slot in 0..TEXTURE_SLOTS {
        let components = if slot < TEXTURES_2D { "xy" } else { "xyz" };
        source += &format!(
            "case {slot}u: {{ value = textureSample(tx{slot}, sm{slot}, coord.{components}); }}\n"
        );
    }
    source += "default: {} } if ((psp.texture_flags.x & (1u << slot)) != 0u) { value = vec4<f32>(value.rgb, 1.0); } return value; }\n";
    source += &format!(
        "fn register_source_count(opcode: u32) -> u32 {{ switch opcode {{\n\
         case {OP_MOV}u, {OP_FRC}u, {OP_RCP}u, {OP_RSQ}u, {OP_NRM}u, {OP_EXP}u, {OP_EXPP}u, {OP_LOG}u, {OP_SINCOS}u, {OP_ABS}u, {OP_MOVA}u, {OP_TEXKILL}u, {OP_TEX}u: {{ return 1u; }}\n\
         case {OP_MAD}u, {OP_LRP}u, {OP_CMP}u, {OP_DP2ADD}u: {{ return 3u; }}\n\
         default: {{ return 2u; }} }} }}\n"
    );
    source += r#"
fn d9_nrm(a: vec4<f32>) -> vec4<f32> { let l = length(a.xyz); if (l > 0.0) { return a / l; } return vec4<f32>(0.0); }
fn swizzle(v: vec4<f32>, s: u32) -> vec4<f32> {
    return vec4<f32>(v[s & 3u], v[(s >> 2u) & 3u], v[(s >> 4u) & 3u], v[(s >> 6u) & 3u]);
}
fn vertex_constant(index: i32) -> vec4<f32> {
    let slot = u32(clamp(index, 0, 255));
    for (var i = 0u; i < vp.header.w; i++) {
        if (vp.definitions[i].index.x == slot) { return vp.definitions[i].value; }
    }
    return vcb[slot];
}
fn vertex_read(s: vec4<u32>, r: ptr<function, Registers>, immediate: bool) -> vec4<f32> {
    if (immediate) { return bitcast<vec4<f32>>(s); }
    var value = vec4<f32>(0.0);
    switch s.x {
        case 0u: { value = (*r).temp[s.y]; }
        case 1u: { value = (*r).input[s.y]; }
        case 2u: {
            value = vcb[s.y];
            if ((s.w & 256u) != 0u) {
                value = vertex_constant(i32(s.y) + i32((*r).address[(s.w >> 9u) & 3u]));
            }
        }
        default: {}
    }
    return select(swizzle(value, s.z), -swizzle(value, s.z), (s.w & 1u) != 0u);
}
fn pixel_read(s: vec4<u32>, r: ptr<function, Registers>, immediate: bool) -> vec4<f32> {
    if (immediate) { return bitcast<vec4<f32>>(s); }
    var value = vec4<f32>(0.0);
    switch s.x {
        case 0u: { value = (*r).temp[s.y]; }
        case 1u: { value = (*r).input[s.y]; }
        case 2u: { value = pcb[s.y]; }
        case 3u: { value = (*r).tex[s.y]; }
        default: {}
    }
    return select(swizzle(value, s.z), -swizzle(value, s.z), (s.w & 1u) != 0u);
}
fn masked(old: vec4<f32>, value: vec4<f32>, mask: u32) -> vec4<f32> {
    return select(old, value, (vec4<u32>(mask) & vec4<u32>(1u, 2u, 4u, 8u)) != vec4<u32>(0u));
}
fn vertex_write(r: ptr<function, Registers>, d: vec4<u32>, value: vec4<f32>) {
    switch d.y {
        case 0u: { (*r).temp[d.z] = masked((*r).temp[d.z], value, d.w); }
        case 4u: { (*r).pos = masked((*r).pos, value, d.w); }
        case 5u: { (*r).color[d.z] = masked((*r).color[d.z], value, d.w); }
        case 3u: { (*r).address = masked((*r).address, sign(value) * floor(abs(value) + 0.5), d.w); }
        case 6u: { (*r).tex[d.z] = masked((*r).tex[d.z], value, d.w); }
        default: {}
    }
}
fn pixel_write(r: ptr<function, Registers>, d: vec4<u32>, value: vec4<f32>) {
    switch d.y {
        case 0u: { (*r).temp[d.z] = masked((*r).temp[d.z], value, d.w); }
        case 8u: { (*r).color[0] = masked((*r).color[0], value, d.w); }
        default: {}
    }
}
"#;
    source += "struct VIn {";
    for slot in 0..VERTEX_INPUTS {
        source += &format!(" @location({slot}) v{slot}: vec4<f32>,");
    }
    source += " }\n@vertex fn vs_main(v: VIn) -> V2F {\n var r: Registers;\n";
    for slot in 0..VERTEX_INPUTS {
        source += &format!("r.input[{slot}] = v.v{slot} * vsp.ascale[{slot}];\n");
    }
    source += &format!(
        "for (var index = 0u; index < min(vp.header.z, {MAX_INSTRUCTIONS}u); index++) {{\n\
             let i = vp.code[index];\n\
             let count = register_source_count(i.dst.x & {OPCODE_MASK}u);\n\
             let a = vertex_read(i.a, &r, (i.dst.x & {}u) != 0u);\n\
             var b = vec4<f32>(0.0); var c = vec4<f32>(0.0);\n\
             if (count > 1u) {{ b = vertex_read(i.b, &r, (i.dst.x & {}u) != 0u); }}\n\
             if (count > 2u) {{ c = vertex_read(i.c, &r, (i.dst.x & {}u) != 0u); }}\n\
             var value = a;\n\
             switch (i.dst.x & {OPCODE_MASK}u) {{\n\
             case {OP_MOV}u: {{}}\n\
             case {OP_FRC}u: {{ value = fract(a); }}\n\
             case {OP_SGE}u: {{ value = select(vec4<f32>(0.0), vec4<f32>(1.0), a >= b); }}\n\
             case {OP_EXP}u, {OP_EXPP}u: {{ value = vec4<f32>(exp2(a.x)); }}\n\
             case {OP_LOG}u: {{ value = vec4<f32>(log2(abs(a.x))); }}\n\
             case {OP_DP2ADD}u: {{ value = vec4<f32>(dot(a.xy, b.xy) + c.x); }}\n\
             case {OP_ABS}u: {{ value = abs(a); }}\n\
             case {OP_POW}u: {{ value = vec4<f32>(pow(abs(a.x), b.x)); }}\n\
             case {OP_ADD}u: {{ value = a + b; }}\n\
             case {OP_RCP}u: {{ value = vec4<f32>(1.0 / a.x); }}\n\
             case {OP_RSQ}u: {{ value = vec4<f32>(inverseSqrt(abs(a.x))); }}\n\
             case {OP_DP3}u: {{ value = vec4<f32>(dot(a.xyz, b.xyz)); }}\n\
             case {OP_MIN}u: {{ value = min(a, b); }}\n\
             case {OP_MAX}u: {{ value = max(a, b); }}\n\
             case {OP_SLT}u: {{ value = select(vec4<f32>(0.0), vec4<f32>(1.0), a < b); }}\n\
             case {OP_LRP}u: {{ value = mix(c, b, a); }}\n\
             case {OP_CMP}u: {{ value = select(c, b, a >= vec4<f32>(0.0)); }}\n\
             case {OP_NRM}u: {{ value = d9_nrm(a); }}\n\
             case {OP_MUL}u: {{ value = a * b; }}\n\
             case {OP_MAD}u: {{ value = a * b + c; }}\n\
             case {OP_SINCOS}u: {{ value = vec4<f32>(cos(a.x), sin(a.x), 0.0, 0.0); }}\n\
             case {OP_MOVA}u: {{}}\n\
             case {OP_DP4}u: {{ value = vec4<f32>(dot(a, b)); }}\n\
             default: {{}}\n\
             }}\n\
             if ((i.dst.x & {SATURATE_FLAG}u) != 0u) {{ value = clamp(value, vec4<f32>(0.0), vec4<f32>(1.0)); }}\n\
             vertex_write(&r, i.dst, value);\n\
             }}\n",
        1u32 << IMMEDIATE_SOURCE_SHIFT,
        1u32 << (IMMEDIATE_SOURCE_SHIFT + 1),
        1u32 << (IMMEDIATE_SOURCE_SHIFT + 2),
    );
    source += r#"
    var o: V2F;
    o.pos = vec4<f32>(r.pos.xy + vsp.halfpix.xy * r.pos.w, r.pos.zw);
    o.d0 = r.color[0];
    o.d1 = r.color[1];
    o.t0 = r.tex[0];
    o.t1 = r.tex[1];
    o.t2 = r.tex[2];
    o.t3 = r.tex[3];
    o.t4 = r.tex[4];
    o.t5 = r.tex[5];
    o.t6 = r.tex[6];
    o.t7 = r.tex[7];
    o.fog = 1.0;
    o.fogz = r.pos.z;
    return o;
}
@fragment fn ps_main(v: V2F) -> @location(0) vec4<f32> {
    var r: Registers;
    r.input[0] = v.d0;
    r.input[1] = v.d1;
    r.tex[0] = v.t0;
    r.tex[1] = v.t1;
    r.tex[2] = v.t2;
    r.tex[3] = v.t3;
    r.tex[4] = v.t4;
    r.tex[5] = v.t5;
    r.tex[6] = v.t6;
    r.tex[7] = v.t7;
    var killed = false;
"#;
    source += &format!(
        "for (var index = 0u; index < min(pp.header.z, {MAX_INSTRUCTIONS}u); index++) {{\n\
             let i = pp.code[index];\n\
             let count = register_source_count(i.dst.x & {OPCODE_MASK}u);\n\
             let a = pixel_read(i.a, &r, (i.dst.x & {}u) != 0u);\n\
             var b = vec4<f32>(0.0); var c = vec4<f32>(0.0);\n\
             if (count > 1u) {{ b = pixel_read(i.b, &r, (i.dst.x & {}u) != 0u); }}\n\
             if (count > 2u) {{ c = pixel_read(i.c, &r, (i.dst.x & {}u) != 0u); }}\n\
             var value = a;\n\
             switch (i.dst.x & {OPCODE_MASK}u) {{\n\
             case {OP_TEXKILL}u: {{ killed = killed || any(a < vec4<f32>(0.0)); }}\n\
             case {OP_MOV}u: {{}}\n\
             case {OP_FRC}u: {{ value = fract(a); }}\n\
             case {OP_SGE}u: {{ value = select(vec4<f32>(0.0), vec4<f32>(1.0), a >= b); }}\n\
             case {OP_EXP}u, {OP_EXPP}u: {{ value = vec4<f32>(exp2(a.x)); }}\n\
             case {OP_LOG}u: {{ value = vec4<f32>(log2(abs(a.x))); }}\n\
             case {OP_DP2ADD}u: {{ value = vec4<f32>(dot(a.xy, b.xy) + c.x); }}\n\
             case {OP_ABS}u: {{ value = abs(a); }}\n\
             case {OP_POW}u: {{ value = vec4<f32>(pow(abs(a.x), b.x)); }}\n\
             case {OP_ADD}u: {{ value = a + b; }}\n\
             case {OP_RCP}u: {{ value = vec4<f32>(1.0 / a.x); }}\n\
             case {OP_RSQ}u: {{ value = vec4<f32>(inverseSqrt(abs(a.x))); }}\n\
             case {OP_DP3}u: {{ value = vec4<f32>(dot(a.xyz, b.xyz)); }}\n\
             case {OP_MIN}u: {{ value = min(a, b); }}\n\
             case {OP_MAX}u: {{ value = max(a, b); }}\n\
             case {OP_SLT}u: {{ value = select(vec4<f32>(0.0), vec4<f32>(1.0), a < b); }}\n\
             case {OP_LRP}u: {{ value = mix(c, b, a); }}\n\
             case {OP_CMP}u: {{ value = select(c, b, a >= vec4<f32>(0.0)); }}\n\
             case {OP_NRM}u: {{ value = d9_nrm(a); }}\n\
             case {OP_MUL}u: {{ value = a * b; }}\n\
             case {OP_MAD}u: {{ value = a * b + c; }}\n\
             case {OP_DP4}u: {{ value = vec4<f32>(dot(a, b)); }}\n\
             case {OP_TEX}u: {{ let coord = select(a, a / a.w, (i.dst.x & 65536u) != 0u); value = sample_texture(i.b.y, coord); }}\n\
             default: {{}}\n\
             }}\n\
             if ((i.dst.x & {SATURATE_FLAG}u) != 0u) {{ value = clamp(value, vec4<f32>(0.0), vec4<f32>(1.0)); }}\n\
             pixel_write(&r, i.dst, value);\n\
             }}\n",
        1u32 << IMMEDIATE_SOURCE_SHIFT,
        1u32 << (IMMEDIATE_SOURCE_SHIFT + 1),
        1u32 << (IMMEDIATE_SOURCE_SHIFT + 2),
    );
    source += "if (killed) { discard; }\n\
        let col = d9_output(r.color[0], v);\n\
        if (psp.texture_flags.y != 0u) { return vec4<f32>(mix(col.rgb, vec3<f32>(1.0, 0.0, 0.0), 0.15), col.a); }\n\
        return col;\n}\n";
    source += include_str!("../raster.wgsl");
    source
}
