//! Shader-model 1 token translation. Token layouts are from the pinned Wine
//! d3d8types.h; texbem follows Wine's shader_glsl_texbem. No guest pointers.
//! Unsupported versions, instructions and declarations fail during creation.
use super::fixed_function::{FvfLayout, TEXTURED_WGSL, lit_shader_source};
use crate::RenderError;

fn fail(s: impl Into<String>) -> RenderError {
    RenderError::new("D3D8 shader", s)
}

#[derive(Clone, Debug)]
pub struct Declaration {
    pub layout: FvfLayout,
    fields: Vec<(u32, u32)>,
    source_fields: Vec<(u32, u64)>,
    pub stream_count: u32,
    pub constants: Vec<(usize, [f32; 4])>,
}
impl Declaration {
    /// Decode a copied declaration, checking stream layout and constant bounds.
    pub fn parse(words: &[u32]) -> Result<Self, RenderError> {
        let mut attributes: Vec<wgpu::VertexAttribute> = Vec::new();
        let mut fields = Vec::new();
        let mut source_fields = Vec::new();
        let mut offsets = [0u64; 16];
        let mut constants = Vec::new();
        let mut offset = 0;
        let mut stream = None;
        let mut i = 0;
        while i < words.len() {
            let t = words[i];
            i += 1;
            if t == u32::MAX {
                if i != words.len() || fields.is_empty() {
                    return Err(fail("invalid declaration END"));
                }
                let mut bases = [0u64; 16];
                let mut stride = 0;
                for (index, size) in offsets.iter().enumerate() {
                    bases[index] = stride;
                    stride += size;
                }
                for (attr, &(stream, source_offset)) in attributes.iter_mut().zip(&source_fields) {
                    attr.offset = bases[stream as usize] + source_offset;
                }
                return Ok(Self {
                    layout: FvfLayout {
                        stride,
                        attributes,
                        pre_transformed: false,
                        texcoord_sets: 2,
                    },
                    fields,
                    stream_count: source_fields.iter().map(|(s, _)| s + 1).max().unwrap_or(0),
                    source_fields,
                    constants,
                });
            }
            match t >> 29 {
                0 if t == 0 => {}
                1 if t & !15 == 0x20000000 => {
                    stream = Some(t & 15);
                    offset = offsets[(t & 15) as usize];
                }
                2 if stream.is_some() => {
                    if t & 0x10000000 != 0 {
                        offset += u64::from((t >> 16) & 15) * 4;
                        offsets[stream.unwrap() as usize] = offset;
                        continue;
                    }
                    let reg = t & 31;
                    let ty = (t >> 16) & 15;
                    if reg >= 16 || fields.iter().any(|(r, _)| *r == reg) {
                        return Err(fail("duplicate/out-of-range declaration register"));
                    }
                    let format = match ty {
                        0 => wgpu::VertexFormat::Float32,
                        1 => wgpu::VertexFormat::Float32x2,
                        2 => wgpu::VertexFormat::Float32x3,
                        3 => wgpu::VertexFormat::Float32x4,
                        4 => wgpu::VertexFormat::Uint32,
                        5 => wgpu::VertexFormat::Uint8x4,
                        6 => wgpu::VertexFormat::Sint16x2,
                        7 => wgpu::VertexFormat::Sint16x4,
                        _ => return Err(fail(format!("declaration data type {ty}"))),
                    };
                    attributes.push(wgpu::VertexAttribute {
                        format,
                        offset,
                        shader_location: reg,
                    });
                    offset += format.size();
                    offsets[stream.unwrap() as usize] = offset;
                    source_fields.push((stream.unwrap(), attributes.last().unwrap().offset));
                    fields.push((reg, ty));
                }
                4 => {
                    let start = (t & 127) as usize;
                    let n = ((t >> 25) & 15) as usize;
                    if start + n > 96 || i + n * 4 > words.len() {
                        return Err(fail("declaration constant range"));
                    }
                    for k in 0..n {
                        constants.push((
                            start + k,
                            std::array::from_fn(|c| f32::from_bits(words[i + k * 4 + c])),
                        ));
                    }
                    i += n * 4;
                }
                _ => {
                    return Err(fail(format!("unsupported declaration token {t:#010x}")));
                }
            }
        }
        Err(fail("unterminated declaration"))
    }
    pub fn pack_vertices(
        &self,
        streams: &[crate::bridge::ffi::VertexStream],
        order: &[u32],
    ) -> Result<Vec<u8>, RenderError> {
        let stride = self.layout.stride as usize;
        let size = order
            .len()
            .checked_mul(stride)
            .ok_or_else(|| RenderError::invalid("Draw", "vertex repack size overflow"))?;
        let mut out = Vec::new();
        out.try_reserve_exact(size)
            .map_err(|_| RenderError::out_of_memory("Draw", "vertex repack allocation failed"))?;
        out.resize(size, 0);
        for (vertex, &index) in order.iter().enumerate() {
            for (attr, &(stream, offset)) in self.layout.attributes.iter().zip(&self.source_fields)
            {
                let source = streams
                    .iter()
                    .find(|s| s.stream == stream)
                    .ok_or_else(|| fail(format!("vertex stream {stream} is not bound")))?;
                let bytes = crate::d3d9::draw::vertex_element_bytes(
                    source.data,
                    source.stream,
                    source.stride,
                    0,
                    offset,
                    attr.format.size() as usize,
                    index,
                )?;
                let at = vertex * stride + attr.offset as usize;
                out[at..at + bytes.len()].copy_from_slice(bytes);
            }
        }
        Ok(out)
    }

    /// Declaration-only shaders select fixed-function processing. Restrict
    /// this mapping to existing FVFs, including byte offsets and stride.
    pub fn fixed_fvf(&self) -> Result<u32, RenderError> {
        if self.stream_count != 1 {
            return Err(fail(
                "multi-stream fixed-function declarations are not supported",
            ));
        }
        for fvf in [0x42, 0x142, 0x242, 0x112, 0x152] {
            let layout = FvfLayout::decode(fvf)?;
            let expected: Vec<_> = match fvf {
                0x42 => vec![(0, 2), (5, 4)],
                0x142 => vec![(0, 2), (5, 4), (7, 1)],
                0x242 => vec![(0, 2), (5, 4), (7, 1), (8, 1)],
                0x112 => vec![(0, 2), (3, 2), (7, 1)],
                _ => vec![(0, 2), (3, 2), (5, 4), (7, 1)],
            };
            if self.fields == expected
                && self.layout.stride == layout.stride
                && self
                    .layout
                    .attributes
                    .iter()
                    .zip(expected.iter())
                    .all(|(a, (_, ty))| {
                        a.format.size()
                            == match ty {
                                2 => 12,
                                1 => 8,
                                _ => 4,
                            }
                    })
            {
                // No SKIP padding may occur between fields either.
                let mut offset = 0;
                let packed = self.layout.attributes.iter().all(|a| {
                    let ok = a.offset == offset;
                    offset += a.format.size();
                    ok
                });
                if packed {
                    return Ok(fvf);
                }
            }
        }
        if let Some(fvf) = self.truncated_fvf() {
            return Ok(fvf);
        }
        let offsets: Vec<_> = self.layout.attributes.iter().map(|a| a.offset).collect();
        Err(fail(format!(
            "declaration-only shader layout has no supported fixed-function FVF \
             (fields {:?}, offsets {:?}, stride {}, streams {})",
            self.fields, offsets, self.layout.stride, self.stream_count
        )))
    }
    /// A declaration whose leading fields are exactly a supported FVF's
    /// packed layout, followed only by extra texture-coordinate sets the
    /// fixed-function pipeline cannot sample (registers 8..15 beyond the
    /// FVF's own sets). Draws then keep each vertex's first `stride` bytes.
    /// DIVERGENCE(original): the extra sets are dropped; a stage that selects
    /// them reads what the reduced FVF supplies for a missing set.
    fn truncated_fvf(&self) -> Option<u32> {
        if self.stream_count != 1 {
            return None;
        }
        for (fvf, expected) in [
            (0x112u32, &[(0u32, 2u32), (3, 2), (7, 1)][..]),
            (0x152, &[(0, 2), (3, 2), (5, 4), (7, 1)][..]),
        ] {
            let n = expected.len();
            if self.fields.len() <= n || self.fields[..n] != *expected {
                continue;
            }
            if !self.fields[n..]
                .iter()
                .all(|&(reg, _)| (8..16).contains(&reg))
            {
                continue;
            }
            let layout = FvfLayout::decode(fvf).ok()?;
            let mut offset = 0;
            let packed = self.layout.attributes[..n].iter().all(|a| {
                let ok = a.offset == offset;
                offset += a.format.size();
                ok
            });
            if packed && offset == layout.stride {
                return Some(fvf);
            }
        }
        None
    }

    /// Byte count of each vertex that a truncated FVF keeps, if this
    /// declaration resolves to one (see [`Self::truncated_fvf`]).
    pub fn truncated_stride(&self) -> Option<u64> {
        let fvf = self.truncated_fvf()?;
        let stride = FvfLayout::decode(fvf).ok()?.stride;
        (stride < self.layout.stride).then_some(stride)
    }

    fn inputs(&self) -> (String, String) {
        let mut fields = String::from("struct VertexInput {\n");
        let mut loads = String::new();
        for &(r, ty) in &self.fields {
            let typ = match ty {
                0 => "f32",
                1 => "vec2<f32>",
                2 => "vec3<f32>",
                3 => "vec4<f32>",
                4 => "u32",
                5 => "vec4<u32>",
                6 => "vec2<i32>",
                _ => "vec4<i32>",
            };
            fields += &format!("@location({r}) v{r}: {typ},\n");
            let expr = match ty {
                0 => format!("vec4<f32>(in.v{r}, 0.0, 0.0, 1.0)"),
                1 | 6 => format!("vec4<f32>(vec2<f32>(in.v{r}), 0.0, 1.0)"),
                2 => format!("vec4<f32>(in.v{r}, 1.0)"),
                4 => format!(
                    "vec4<f32>(f32((in.v{r} >> 16u) & 255u), f32((in.v{r} >> 8u) & 255u), f32(in.v{r} & 255u), f32(in.v{r} >> 24u)) / 255.0"
                ),
                _ => format!("vec4<f32>(in.v{r})"),
            };
            loads += &format!("v[{r}] = {expr};\n");
        }
        fields += "};\n";
        (fields, loads)
    }
}

#[derive(Clone, Debug)]
pub struct Program {
    pub pixel: bool,
    pub declaration_only: bool,
    pub body: String,
    pub samplers: [bool; 4],
    pub cube_reflection: bool,
}
fn register(t: u32, pixel: bool, dest: bool) -> Result<String, RenderError> {
    if t & 0x80000000 == 0 {
        return Err(fail("parameter missing high bit"));
    }
    let ty = (t >> 28) & 7;
    let n = t & 0x7ff;
    let reg = match (ty, n, pixel) {
        (0, 0..=1, true) | (0, 0..=11, false) => format!("r[{n}]"),
        (1, 0..=1, true) | (1, 0..=15, false) if !dest => format!("v[{n}]"),
        (2, 0..=7, true) | (2, 0..=95, false) if !dest => {
            if t & 0x2000 != 0 {
                if pixel {
                    return Err(fail("relative pixel constant"));
                }
                // DIVERGENCE(original): out-of-range relative constant indices
                // clamp to the available bank; original GPU behavior is unknown.
                format!("c[u32(clamp(i32(a.x) + {n}, 0, 95))]")
            } else {
                format!("c[{n}]")
            }
        }
        (2, 0..=7, true) | (2, 0..=95, false) if dest => format!("c[{n}]"),
        (3, 0..=3, true) => format!("t[{n}]"),
        (3, 0, false) => "a".into(),
        (4, 0, false) if dest => "opos".into(),
        (4, 1, false) if dest => "ofog".into(),
        (5, 0..=1, false) if dest => format!("od[{n}]"),
        (6, 0..=7, false) if dest => format!("ot[{n}]"),
        _ => {
            return Err(fail(format!(
                "invalid {} register type {ty}, index {n}",
                if dest { "destination" } else { "source" }
            )));
        }
    };
    if t & 0x2000 != 0 && ty != 2 {
        return Err(fail("relative nonconstant register"));
    }
    Ok(reg)
}
fn source(t: u32, pixel: bool) -> Result<String, RenderError> {
    let reg = register(t, pixel, false)?;
    let sw: String = (0..4)
        .map(|i| b"xyzw"[((t >> (16 + i * 2)) & 3) as usize] as char)
        .collect();
    let e = format!("({reg}).{sw}");
    Ok(match (t >> 24) & 15 {
        0 => e,
        1 => format!("-({e})"),
        2 => format!("({e} - vec4<f32>(0.5))"),
        3 => format!("-( {e} - vec4<f32>(0.5))"),
        4 => format!("({e} * 2.0 - vec4<f32>(1.0))"),
        5 => format!("-({e} * 2.0 - vec4<f32>(1.0))"),
        6 => format!("(vec4<f32>(1.0) - {e})"),
        _ => return Err(fail(format!("source modifier {}", (t >> 24) & 15))),
    })
}
impl Program {
    pub fn validate_draw(&self) -> Result<(), RenderError> {
        if self.cube_reflection {
            return Err(fail(
                "TEXM3X3VSPEC requires unsupported cube/volume sampling and four-component texture coordinates",
            ));
        }
        Ok(())
    }

    /// Decode one bounded SM1.0/1.1 program; unknown instructions fail by name.
    pub fn parse(words: &[u32], pixel: bool) -> Result<Self, RenderError> {
        let version = if pixel { 0xffff0100 } else { 0xfffe0100 };
        if !matches!(words.first(), Some(&word) if word == version || word == version + 1) {
            return Err(fail("only vs.1.0/1.1 and ps.1.0/1.1 are supported"));
        }
        let mut i = 1;
        let mut body = String::new();
        let mut samplers = [false; 4];
        let mut cube_reflection = false;
        let mut matrix_rows: Vec<(u32, u32)> = Vec::new();
        let mut seq = 0;
        // Stores of the latest instruction, held back so a co-issued
        // successor computes its result from the same pre-pair registers.
        let mut pending = String::new();
        while i < words.len() {
            let ins = words[i];
            i += 1;
            let op = ins & 0xffff;
            if op == 0xffff {
                if !matrix_rows.is_empty() {
                    return Err(fail("incomplete TEXM3X3PAD sequence"));
                }
                if i != words.len() {
                    return Err(fail("trailing shader tokens"));
                }
                body += &pending;
                return Ok(Self {
                    pixel,
                    declaration_only: false,
                    body,
                    samplers,
                    cube_reflection,
                });
            }
            if op == 0xfffe {
                i += ((ins >> 16) & 0x7fff) as usize;
                if i > words.len() {
                    return Err(fail("truncated comment"));
                }
                continue;
            }
            // ps.1.x co-issue (`+`): both instructions of the pair read the
            // registers as they were before the pair.
            let coissue = pixel && ins & 0x40000000 != 0;
            if ins & !0xffff & !if pixel { 0x40000000 } else { 0 } != 0 {
                return Err(fail(format!("instruction flags {ins:#010x}")));
            }
            if coissue {
                if pending.is_empty() || !(1..=24).contains(&op) && op != 80 {
                    return Err(fail("co-issue needs a preceding arithmetic instruction"));
                }
            } else {
                body += &pending;
                pending.clear();
            }
            if !matrix_rows.is_empty() && op != 73 && op != 77 {
                return Err(fail(
                    "TEXM3X3PAD must be followed by TEXM3X3PAD/TEXM3X3VSPEC",
                ));
            }
            if op == 0 {
                continue;
            }
            let n = match op {
                1 | 6 | 7 | 14 | 15 | 16 | 19 | 73 | 77 | 78 | 79 => 1,
                2 | 3 | 5 | 8 | 9 | 10 | 11 | 12 | 13 | 17 | 20..=24 | 67..=70 => 2,
                4 | 18 | 80 => 3,
                64..=66 => 0,
                81 => 4,
                _ => return Err(fail(format!("unsupported opcode {op}"))),
            };
            // TEXBEM / TEXREG have one source, plus destination.
            let n = if (67..=70).contains(&op) { 1 } else { n };
            if i + 1 + n > words.len() {
                return Err(fail("truncated instruction"));
            }
            let dst = words[i];
            let args = &words[i + 1..i + 1 + n];
            i += 1 + n;
            let name = register(dst, pixel, true)?;
            if op == 73 || op == 77 {
                let stage = dst & 0x7ff;
                let src = args[0];
                source(src, pixel)?;
                if !pixel
                    || dst != (0xb00f0000 | stage)
                    || stage >= 4
                    || (src >> 28) & 7 != 3
                    || src & 0x7ff >= stage
                    || src & 0x00ff0000 != 0x00e40000
                    || !matches!((src >> 24) & 15, 0 | 4)
                {
                    return Err(fail("invalid TEXM3X3PAD/TEXM3X3VSPEC operands"));
                }
                if let Some(&(first, input)) = matrix_rows.first() {
                    if stage != first + matrix_rows.len() as u32 || src != input {
                        return Err(fail("nonconsecutive TEXM3X3PAD/TEXM3X3VSPEC sequence"));
                    }
                }
                if op == 73 {
                    if matrix_rows.len() >= 2 || stage >= 2 + matrix_rows.len() as u32 {
                        return Err(fail("too many/out-of-range TEXM3X3PAD rows"));
                    }
                    matrix_rows.push((stage, src));
                } else {
                    if matrix_rows.len() != 2 {
                        return Err(fail("TEXM3X3VSPEC requires two TEXM3X3PAD rows"));
                    }
                    matrix_rows.clear();
                    cube_reflection = true;
                    samplers[stage as usize] = true;
                }
                continue;
            }
            if op == 81 {
                if (dst >> 28) & 7 != 2 {
                    return Err(fail("DEF destination must be a constant"));
                }
                body += &format!(
                    "{name} = bitcast<vec4<f32>>(vec4<u32>({}u,{}u,{}u,{}u));\n",
                    args[0], args[1], args[2], args[3]
                );
                continue;
            }
            if (dst >> 28) & 7 == 2 {
                return Err(fail("constant destination outside DEF"));
            }
            let src: Vec<String> = args
                .iter()
                .map(|&t| source(t, pixel))
                .collect::<Result<_, _>>()?;
            let x = src.first().map(String::as_str).unwrap_or("");
            let y = src.get(1).map(String::as_str).unwrap_or("");
            let z = src.get(2).map(String::as_str).unwrap_or("");
            let expr = match op {
                1 => x.into(),
                2 => format!("({x}+{y})"),
                3 => format!("({x}-{y})"),
                4 => format!("({x}*{y}+{z})"),
                5 => format!("({x}*{y})"),
                6 => format!("vec4<f32>(1.0/({x}).w)"),
                7 => format!("vec4<f32>(inverseSqrt(abs(({x}).w)))"),
                8 | 9 => format!(
                    "vec4<f32>(dot(({x}).{},({y}).{}))",
                    if op == 8 { "xyz" } else { "xyzw" },
                    if op == 8 { "xyz" } else { "xyzw" }
                ),
                10 => format!("min({x},{y})"),
                11 => format!("max({x},{y})"),
                12 | 13 => format!(
                    "select(vec4<f32>(0.0),vec4<f32>(1.0),{x} {} {y})",
                    if op == 12 { "<" } else { ">=" }
                ),
                14 => format!("vec4<f32>(exp2(({x}).w))"),
                15 => format!("vec4<f32>(log2(abs(({x}).w)))"),
                78 => format!("vec4<f32>(exp2(floor(({x}).w)),fract(({x}).w),exp2(({x}).w),1.0)"),
                79 => format!(
                    "vec4<f32>(floor(log2(abs(({x}).w))),abs(({x}).w)/exp2(floor(log2(abs(({x}).w)))),log2(abs(({x}).w)),1.0)"
                ),
                16 => format!(
                    "vec4<f32>(1.0,max(({x}).x,0.0),select(0.0,pow(max(({x}).y,0.0),clamp(({x}).w,-128.0,128.0)),({x}).x>0.0),1.0)"
                ),
                17 => format!("vec4<f32>(1.0,({x}).y*({y}).y,({x}).z,({y}).w)"),
                18 => format!("({x}*{y}+(vec4<f32>(1.0)-{x})*{z})"),
                19 => format!("fract({x})"),
                20..=24 if !pixel => {
                    let rows = match op {
                        20 | 22 => 4,
                        21 | 23 => 3,
                        _ => 2,
                    };
                    let dim = if op <= 21 { "xyzw" } else { "xyz" };
                    let mut e = Vec::new();
                    for row in 0..rows {
                        let s = source(args[1] + row, pixel)?;
                        e.push(format!("dot(({x}).{dim},({s}).{dim})"));
                    }
                    while e.len() < 4 {
                        e.push("0.0".into());
                    }
                    format!("vec4<f32>({})", e.join(","))
                }
                64..=70 if pixel => {
                    let stage = (dst & 0x7ff) as usize;
                    if (dst >> 28) & 7 != 3 || stage >= 4 {
                        return Err(fail("texture instruction destination"));
                    }
                    if op == 65 {
                        body +=
                            &format!("if (any(({name}).xyz < vec3<f32>(0.0))) {{ discard; }}\n");
                        continue;
                    }
                    if op == 64 {
                        format!("clamp({name},vec4<f32>(0.0),vec4<f32>(1.0))")
                    } else {
                        samplers[stage] = true;
                        let uv = match op {
                            66 => format!("({name}).xy"),
                            67 | 68 => format!(
                                "({name}).xy + vec2<f32>(dot(program.bump[{stage}].xz,({x}).xy),dot(program.bump[{stage}].yw,({x}).xy))"
                            ),
                            69 => format!("({x}).wx"),
                            _ => format!("({x}).yz"),
                        };
                        let sample = format!(
                            "textureSampleBias(stage{stage}_tex,stage{stage}_sampler,{uv},program.lod[{stage}].x)"
                        );
                        if op == 68 {
                            format!(
                                "({sample} * (({x}).z * program.lum[{stage}].x + program.lum[{stage}].y))"
                            )
                        } else {
                            sample
                        }
                    }
                }
                80 if pixel => format!("select({z},{y},({x}).a > 0.5)"),
                _ => {
                    return Err(fail(format!(
                        "opcode {op} is invalid for this shader stage"
                    )));
                }
            };
            let mask = (dst >> 16) & 15;
            if mask == 0 {
                return Err(fail("empty destination mask"));
            }
            let shift = (dst >> 24) & 15;
            let scale = match shift {
                0 => 1.0,
                1 => 2.0,
                2 => 4.0,
                3 => 8.0,
                13 => 0.125,
                14 => 0.25,
                15 => 0.5,
                _ => return Err(fail("destination shift")),
            };
            let mut expr = format!("({expr}) * {scale:.3}");
            match (dst >> 20) & 15 {
                0 => {}
                1 => expr = format!("clamp({expr},vec4<f32>(0.0),vec4<f32>(1.0))"),
                _ => return Err(fail("destination modifier")),
            }
            // A temporary preserves reads when source and destination alias,
            // and scalar stores implement masks without WGSL swizzle lvalues.
            body += &format!("let result{seq} = {expr};\n");
            for k in 0..4 {
                if mask & (1 << k) != 0 {
                    let c = b"xyzw"[k] as char;
                    let value = if !pixel && name == "a" {
                        format!("floor(result{seq}.{c})")
                    } else {
                        format!("result{seq}.{c}")
                    };
                    pending += &format!("{name}.{c} = {value};\n");
                }
            }
            seq += 1;
        }
        Err(fail("unterminated shader"))
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniform {
    pub vc: [[f32; 4]; 96],
    pub pc: [[f32; 4]; 8],
    pub bump: [[f32; 4]; 4],
    pub lum: [[f32; 4]; 4],
    /// `lod[stage]`: x LOD bias; y texture set, 8 none or 9 camera-space
    /// position; z the generated coordinate's divisor component (3 z, 4 w).
    pub lod: [[f32; 4]; 4],
    /// Per-stage `D3DTS_TEXTUREn` for camera-space generation.
    pub texm: [[[f32; 4]; 4]; 4],
}
impl Default for Uniform {
    fn default() -> Self {
        bytemuck::Zeroable::zeroed()
    }
}

fn replace_block(s: &mut String, start: &str, replacement: &str) {
    let begin = s.find(start).expect("shader template marker");
    let brace = s[begin..].find('{').unwrap() + begin;
    let mut depth = 1;
    let mut end = brace + 1;
    while depth > 0 {
        match s.as_bytes()[end] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        end += 1;
    }
    if s.as_bytes().get(end) == Some(&b';') {
        end += 1;
    }
    s.replace_range(begin..end, replacement);
}
/// Compose programmable and fixed-function stages independently. Raster fog
/// and alpha testing remain post-fragment operations; specular addition is
/// bypassed for a pixel shader. D3D7 can reuse the token-free fixed stages.
pub fn compose(vs: Option<(&Declaration, &Program)>, ps: Option<&Program>, lit: bool) -> String {
    if let Some(p) = ps {
        p.validate_draw().expect("validated pixel shader draw");
    }
    let mut s = if lit {
        lit_shader_source(true)
    } else {
        TEXTURED_WGSL.into()
    };
    s += "\nstruct ProgramUniform { vc: array<vec4<f32>,96>, pc: array<vec4<f32>,8>, bump: array<vec4<f32>,4>, lum: array<vec4<f32>,4>, lod: array<vec4<f32>,4>, texm: array<mat4x4<f32>,4>, };\n@group(0) @binding(4) var<uniform> program: ProgramUniform;\n@group(1) @binding(4) var stage2_tex: texture_2d<f32>;\n@group(1) @binding(5) var stage2_sampler: sampler;\n@group(1) @binding(6) var stage3_tex: texture_2d<f32>;\n@group(1) @binding(7) var stage3_sampler: sampler;\n";
    s=s.replace("    out.specular = vec3<f32>(\n", "    out.specular_alpha = f32((in.specular >> 24u) & 255u) / 255.0;\n    out.specular = vec3<f32>(\n");
    let out = "    @location(7) uv2: vec2<f32>,\n    @location(8) uv3: vec2<f32>,\n    @location(9) specular_alpha: f32,\n";
    s = s.replace(
        "    @location(6) cam_pos: vec4<f32>,",
        &format!("    @location(6) cam_pos: vec4<f32>,\n{out}"),
    );
    if vs.is_some() {
        s = s.replace("@interpolate(linear) ", "");
    }
    if let Some((decl, p)) = vs {
        let (input, mut loads) = decl.inputs();
        // Wine load_local_constants keeps D3DVSD_CONST values on the shader,
        // independently of the Set/GetVertexShaderConstant device bank.
        for &(reg, value) in &decl.constants {
            let b = value.map(f32::to_bits);
            loads += &format!(
                "c[{reg}]=bitcast<vec4<f32>>(vec4<u32>({}u,{}u,{}u,{}u));\n",
                b[0], b[1], b[2], b[3]
            );
        }
        replace_block(&mut s, "struct VertexInput {", &input);
        let body = format!(
            "@vertex\nfn vs_main(in:VertexInput)->VertexOutput{{\nvar r:array<vec4<f32>,12>;var v:array<vec4<f32>,16>;var c=program.vc;var a=vec4<f32>(0.0);var opos=vec4<f32>(0.0);var ofog=vec4<f32>(1.0);var od:array<vec4<f32>,2>;var ot:array<vec4<f32>,8>;\n{loads}\n{}\nvar out:VertexOutput;out.position=opos;out.color=clamp(od[0],vec4<f32>(0.0),vec4<f32>(1.0));out.specular=clamp(od[1].xyz,vec3<f32>(0.0),vec3<f32>(1.0));out.specular_alpha=clamp(od[1].w,0.0,1.0);out.uv0=ot[0].xy;out.uv1=ot[1].xy;out.uv2=ot[2].xy;out.uv3=ot[3].xy;out.fogdist=abs(opos.w);out.fogfactor=clamp(ofog.x,0.0,1.0);return out;}}",
            p.body
        );
        replace_block(&mut s, "@vertex\nfn vs_main", &body);
        // The fixed RHW entry references the original input independently.
    }
    if let Some(p) = ps {
        let body = format!(
            "@fragment\nfn fs_main(in:VertexOutput)->@location(0) vec4<f32>{{\nvar r:array<vec4<f32>,2>;var v:array<vec4<f32>,2>;v[0]=clamp(in.color,vec4<f32>(0.0),vec4<f32>(1.0));v[1]=clamp(vec4<f32>(in.specular,in.specular_alpha),vec4<f32>(0.0),vec4<f32>(1.0));var c=program.pc;var t:array<vec4<f32>,4>;t[0]=vec4<f32>(in.uv0,0.0,1.0);t[1]=vec4<f32>(in.uv1,0.0,1.0);t[2]=vec4<f32>(in.uv2,0.0,1.0);t[3]=vec4<f32>(in.uv3,0.0,1.0);\n{}\nif(!alpha_test_pass(r[0].a)){{discard;}}return vec4<f32>(apply_fog(r[0].rgb,in.fogdist,in.fogfactor),r[0].a);}}",
            p.body
        );
        let body = if vs.is_none() {
            let mut body = body;
            for stage in 0..4 {
                let init = format!("t[{stage}]=vec4<f32>(in.uv{stage},0.0,1.0);");
                let replace = format!(
                    "t[{stage}]=vec4<f32>(0.0,0.0,0.0,1.0);if(program.lod[{stage}].y==0.0){{t[{stage}]=vec4<f32>(in.uv0,0.0,1.0);}}else if(program.lod[{stage}].y==1.0){{t[{stage}]=vec4<f32>(in.uv1,0.0,1.0);}}else if(program.lod[{stage}].y==9.0){{let g=program.texm[{stage}]*in.cam_pos;var q=1.0;if(program.lod[{stage}].z==3.0){{q=g.z;}}else if(program.lod[{stage}].z==4.0){{q=g.w;}}t[{stage}]=vec4<f32>(g.xy/q,0.0,1.0);}}"
                );
                body = body.replace(&init, &replace);
            }
            body
        } else {
            body
        };
        replace_block(&mut s, "@fragment\nfn fs_main", &body);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dst(ty: u32, n: u32, mask: u32) -> u32 {
        0x80000000 | (ty << 28) | (mask << 16) | n
    }
    fn src(ty: u32, n: u32) -> u32 {
        0x80e40000 | (ty << 28) | n
    }
    fn validate(s: &str) {
        let module =
            naga::front::wgsl::parse_str(s).unwrap_or_else(|e| panic!("{}", e.emit_to_string(s)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .unwrap();
    }
    #[test]
    fn declarations_offsets_and_constants() {
        let d = Declaration::parse(&[
            0x20000000,
            0x40020000,
            0x50010000,
            0x40020003,
            0x40010007,
            0x82000002,
            1,
            2,
            3,
            4,
            u32::MAX,
        ])
        .unwrap();
        assert_eq!(d.layout.stride, 36);
        assert_eq!(d.layout.attributes[1].offset, 16);
        assert_eq!(d.constants[0].0, 2);
        assert_eq!(d.constants[0].1[3].to_bits(), 4);
        assert!(Declaration::parse(&[0x20000010, 0x40020000, u32::MAX]).is_err());
        assert!(Declaration::parse(&[0x20000000, 0x40020000, 0x40020000, u32::MAX]).is_err());
    }
    #[test]
    fn coissued_pair_reads_registers_from_before_the_pair() {
        // ps.1.1: mov r0.rgb, c0 / +mov r0.a, r0 / mov r0, r0
        let words = [
            0xffff0101,
            1,
            dst(0, 0, 7),
            src(2, 0),
            0x40000001,
            dst(0, 0, 8),
            src(0, 0),
            1,
            dst(0, 0, 15),
            src(0, 0),
            0xffff,
        ];
        let p = Program::parse(&words, true).unwrap();
        let second = p.body.find("let result1").unwrap();
        let first_store = p.body.find("r[0].x = result0.x").unwrap();
        assert!(second < first_store, "{}", p.body);
        assert!(p.body.find("r[0].w = result1.w").unwrap() < p.body.find("let result2").unwrap());
        // Co-issue needs a preceding arithmetic instruction.
        assert!(
            Program::parse(
                &[0xffff0101, 0x40000001, dst(0, 0, 8), src(2, 0), 0xffff],
                true
            )
            .is_err()
        );
        // Vertex shaders have no co-issue.
        assert!(
            Program::parse(
                &[
                    0xfffe0101,
                    1,
                    dst(0, 0, 15),
                    src(2, 0),
                    0x40000001,
                    dst(0, 1, 15),
                    src(2, 0),
                    0xffff
                ],
                false
            )
            .is_err()
        );
    }
    #[test]
    fn extra_texture_sets_truncate_to_a_supported_fvf() {
        // XYZ, NORMAL, TEX0..1 float2, TEX2..3 float3: stride 64.
        let d = Declaration::parse(&[
            0x20000000,
            0x40020000,
            0x40020003,
            0x40010007,
            0x40010008,
            0x40020009,
            0x4002000a,
            u32::MAX,
        ])
        .unwrap();
        assert_eq!(d.layout.stride, 64);
        assert_eq!(d.fixed_fvf().unwrap(), 0x112);
        assert_eq!(d.truncated_stride(), Some(32));
        // An exact 0x112 declaration needs no truncation.
        let exact = Declaration::parse(&[0x20000000, 0x40020000, 0x40020003, 0x40010007, u32::MAX])
            .unwrap();
        assert_eq!(exact.truncated_stride(), None);
        // A trailing field that is not a texture set still fails.
        let bad = Declaration::parse(&[
            0x20000000,
            0x40020000,
            0x40020003,
            0x40010007,
            0x40040005,
            u32::MAX,
        ])
        .unwrap();
        assert!(bad.fixed_fvf().is_err());
    }
    #[test]
    fn compose_stages_independently_and_water_instructions() {
        let d = Declaration::parse(&[0x20000000, 0x40020000, 0x40020003, 0x40010007, u32::MAX])
            .unwrap();
        let mut words = vec![0xfffe0101];
        for k in 0..4 {
            words.extend([9, dst(4, 0, 1 << k), src(1, 0), src(2, k)]);
        }
        words.extend([
            1,
            dst(5, 0, 15),
            src(2, 5),
            1,
            dst(5, 1, 15),
            src(2, 6),
            1,
            dst(4, 1, 1),
            src(2, 7),
        ]);
        for k in 0..4 {
            words.extend([1, dst(6, k, 3), src(1, 7)]);
        }
        words.push(0xffff);
        let vs = Program::parse(&words, false).unwrap();
        let ps = Program::parse(
            &[
                0xffff0101,
                66,
                dst(3, 0, 15),
                66,
                dst(3, 1, 15),
                67,
                dst(3, 2, 15),
                src(3, 1),
                67,
                dst(3, 3, 15),
                src(3, 1),
                4,
                dst(0, 1, 15),
                src(3, 3) | (4 << 24),
                src(3, 0) | (6 << 24),
                src(3, 0),
                18,
                dst(0, 0, 15),
                src(1, 0),
                src(1, 1),
                src(0, 1),
                0xffff,
            ],
            true,
        )
        .unwrap();
        assert_eq!(ps.samplers, [true; 4]);
        for (v, p, lit) in [
            (Some((&d, &vs)), Some(&ps), false),
            (Some((&d, &vs)), None, false),
            (None, Some(&ps), false),
            (None, Some(&ps), true),
        ] {
            validate(&compose(v, p, lit));
        }
    }
    #[test]
    fn unsupported_and_truncated_tokens_fail_at_creation() {
        for words in [
            vec![0xffff0104, 0xffff],
            vec![0xffff0101, 4, dst(0, 0, 15)],
            vec![0xffff0101, 0x40000001, dst(0, 0, 15), src(1, 0), 0xffff],
            vec![0xffff0101, 71, 0xffff],
        ] {
            assert!(Program::parse(&words, true).is_err());
        }
        assert!(
            Program::parse(&[0xfffe0101, 1, dst(0, 12, 15), src(1, 0), 0xffff], false).is_err()
        );
    }
    #[test]
    fn immediate_end_pattern_and_comment_are_not_terminators() {
        let p = Program::parse(
            &[
                0xffff0101,
                0x0002fffe,
                0xffff,
                0xffffffff,
                81,
                dst(2, 0, 15),
                0xffffffff,
                0xffff,
                0,
                0,
                1,
                dst(0, 0, 15),
                src(2, 0),
                0xffff,
            ],
            true,
        )
        .unwrap();
        validate(&compose(None, Some(&p), false));
    }
}
