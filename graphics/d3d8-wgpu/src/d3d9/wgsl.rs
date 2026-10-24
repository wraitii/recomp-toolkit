use super::shader::shader_word_count;
use crate::RenderError;
use std::collections::{BTreeMap, BTreeSet};

const R_TEMP: u32 = 0;
const R_INPUT: u32 = 1;
const R_CONST: u32 = 2;
const R_ADDR: u32 = 3;
const R_RASTOUT: u32 = 4;
const R_ATTROUT: u32 = 5;
const R_TEXCRDOUT: u32 = 6;
const R_CONSTINT: u32 = 7;
const R_COLOROUT: u32 = 8;
const R_DEPTHOUT: u32 = 9;
const R_SAMPLER: u32 = 10;
const R_CONSTBOOL: u32 = 14;

const OP_NOP: u32 = 0;
const OP_MOV: u32 = 1;
const OP_ADD: u32 = 2;
const OP_SUB: u32 = 3;
const OP_MAD: u32 = 4;
const OP_MUL: u32 = 5;
const OP_RCP: u32 = 6;
const OP_RSQ: u32 = 7;
const OP_DP3: u32 = 8;
const OP_DP4: u32 = 9;
const OP_MIN: u32 = 10;
const OP_MAX: u32 = 11;
const OP_SLT: u32 = 12;
const OP_SGE: u32 = 13;
const OP_EXP: u32 = 14;
const OP_LOG: u32 = 15;
const OP_LIT: u32 = 16;
const OP_DST: u32 = 17;
const OP_LRP: u32 = 18;
const OP_FRC: u32 = 19;
const OP_M4X4: u32 = 20;
const OP_M4X3: u32 = 21;
const OP_M3X4: u32 = 22;
const OP_M3X3: u32 = 23;
const OP_M3X2: u32 = 24;
const OP_REP: u32 = 38;
const OP_ENDREP: u32 = 39;
const OP_IF: u32 = 40;
const OP_IFC: u32 = 41;
const OP_ELSE: u32 = 42;
const OP_ENDIF: u32 = 43;
const OP_BREAK: u32 = 44;
const OP_BREAKC: u32 = 45;
const OP_DCL: u32 = 31;
const OP_POW: u32 = 32;
const OP_CRS: u32 = 33;
const OP_SGN: u32 = 34;
const OP_ABS: u32 = 35;
const OP_NRM: u32 = 36;
const OP_SINCOS: u32 = 37;
const OP_MOVA: u32 = 46;
const OP_DEFB: u32 = 47;
const OP_DEFI: u32 = 48;
const OP_TEXCOORD: u32 = 64;
const OP_TEXKILL: u32 = 65;
const OP_TEX: u32 = 66;
const OP_TEXREG2AR: u32 = 69;
const OP_TEXREG2GB: u32 = 70;
const OP_EXPP: u32 = 78;
const OP_LOGP: u32 = 79;
const OP_CND: u32 = 80;
const OP_DEF: u32 = 81;
const OP_TEXDP3: u32 = 85;
const OP_CMP: u32 = 88;
const OP_DP2ADD: u32 = 90;
const OP_DSX: u32 = 91;
const OP_DSY: u32 = 92;
const OP_PHASE: u32 = 0xfffd;
const OP_COMMENT: u32 = 0xfffe;
const OP_END: u32 = 0xffff;

const S_CUBE: u32 = 3;
const S_VOLUME: u32 = 4;

fn fail(cause: impl Into<String>) -> RenderError {
    RenderError::new("CreateShader", cause)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Dst {
    pub ty: u32,
    pub index: u32,
    pub mask: u32,
    pub saturate: bool,
    pub shift: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Src {
    pub ty: u32,
    pub index: u32,
    pub swz: u32,
    pub md: u32,
    pub rel: bool,
    pub rel_comp: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Inst {
    pub op: u32,
    pub ctrl: u32,
    pub dst: Dst,
    pub src: [Src; 4],
    pub nsrc: u32,
    pub coissue: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct DclIn {
    pub usage: u32,
    pub usage_index: u32,
    pub ty: u32,
    pub index: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub pixel: bool,
    pub major: u32,
    pub minor: u32,
    pub code: Vec<Inst>,
    pub inputs: Vec<DclIn>,
    pub outputs: Vec<DclIn>,
    pub samplers: BTreeMap<u32, u32>,
    pub defs: BTreeMap<u32, [f32; 4]>,
    pub idefs: BTreeMap<u32, [i32; 4]>,
    pub bdefs: BTreeMap<u32, bool>,
    pub max_const: u32,
    pub relative: bool,
}

impl Program {
    pub fn version(&self) -> String {
        format!(
            "{}_{}_{}",
            if self.pixel { "ps" } else { "vs" },
            self.major,
            self.minor
        )
    }

    pub fn const_limit(&self) -> u32 {
        if self.relative {
            if self.pixel { 32 } else { 256 }
        } else {
            self.max_const.max(1)
        }
    }
}

fn operand_count(op: u32, major: u32, minor: u32, pixel: bool) -> Option<u32> {
    Some(match op {
        OP_NOP => 0,
        OP_MOV | OP_MOVA | OP_RCP | OP_RSQ | OP_EXP | OP_LOG | OP_LIT | OP_FRC | OP_ABS
        | OP_NRM | OP_EXPP | OP_LOGP | OP_TEXDP3 | OP_DSX | OP_DSY => 2,
        OP_ADD | OP_SUB | OP_MUL | OP_DP3 | OP_DP4 | OP_MIN | OP_MAX | OP_SLT | OP_SGE | OP_DST
        | OP_M4X4 | OP_M4X3 | OP_M3X4 | OP_M3X3 | OP_M3X2 | OP_POW | OP_CRS => 3,
        OP_MAD | OP_LRP | OP_SGN | OP_CND | OP_CMP | OP_DP2ADD => 4,
        OP_TEXCOORD => {
            if pixel && major == 1 && minor == 4 {
                2
            } else {
                1
            }
        }
        OP_TEXKILL => 1,
        OP_TEXREG2AR | OP_TEXREG2GB => 2,
        OP_TEX => {
            if major >= 2 {
                3
            } else if major == 1 && minor == 4 {
                2
            } else {
                1
            }
        }
        OP_SINCOS => {
            if major >= 3 {
                2
            } else {
                4
            }
        }
        OP_REP | OP_IF => 1,
        OP_IFC | OP_BREAKC => 2,
        OP_ENDREP | OP_ELSE | OP_ENDIF | OP_BREAK => 0,
        OP_DCL => 2,
        OP_DEF | OP_DEFI => 5,
        OP_DEFB => 2,
        _ => return None,
    })
}

fn dst_of(t: u32) -> Dst {
    let s = ((t >> 24) & 0xf) as i32;
    Dst {
        ty: ((t >> 28) & 7) | ((t & 0x1800) >> 8),
        index: t & 0x7ff,
        mask: (t >> 16) & 0xf,
        saturate: (t >> 20) & 1 != 0,
        shift: if s > 7 { s - 16 } else { s },
    }
}

fn src_of(t: u32, major: u32) -> (Src, bool) {
    let rel = (t >> 13) & 1 != 0;
    (
        Src {
            ty: ((t >> 28) & 7) | ((t & 0x1800) >> 8),
            index: t & 0x7ff,
            swz: (t >> 16) & 0xff,
            md: (t >> 24) & 0xf,
            rel,
            rel_comp: 0,
        },
        rel && major >= 2,
    )
}

fn read_src(
    p: &mut Program,
    words: &[u32],
    k: &mut usize,
    end: usize,
    inst: &mut Inst,
) -> Result<(), RenderError> {
    if *k >= end || *k >= words.len() {
        return Err(fail("instruction source extends past the staged bytecode"));
    }
    let (mut s, extra) = src_of(words[*k], p.major);
    *k += 1;
    if extra {
        if *k >= end || *k >= words.len() {
            return Err(fail("relative address token extends past the instruction"));
        }
        let (addr, _) = src_of(words[*k], p.major);
        *k += 1;
        if addr.ty != R_ADDR || addr.index != 0 {
            return Err(fail("relative address token is not a0"));
        }
        s.rel_comp = uniform_component(addr.swz)
            .ok_or_else(|| fail("relative address component is not uniform"))?;
    }
    if s.rel && (p.pixel || s.ty != R_CONST) {
        return Err(fail(
            "relative addressing is only implemented for vertex float constants",
        ));
    }
    check_reg(s.ty, s.index, p.pixel, p.major, p.minor, false)?;
    if s.ty == R_CONST {
        if s.rel {
            p.relative = true;
        }
        p.max_const = p.max_const.max(s.index + 1);
    }
    inst.src[inst.nsrc as usize] = s;
    inst.nsrc += 1;
    Ok(())
}

fn check_reg(
    ty: u32,
    index: u32,
    pixel: bool,
    major: u32,
    minor: u32,
    dest: bool,
) -> Result<(), RenderError> {
    let stage = if pixel { "ps" } else { "vs" };
    let invalid = |kind: &str| {
        fail(format!(
            "{kind} register type {ty} index {index} is not valid for {stage}_{major}_{minor}"
        ))
    };
    match ty {
        R_TEMP => {
            let limit = if !pixel {
                if major >= 3 || (major == 2 && minor == 1) {
                    31
                } else {
                    11
                }
            } else if major == 1 && minor <= 3 {
                1
            } else if major == 1 {
                5
            } else {
                31
            };
            if index > limit {
                return Err(invalid("temporary"));
            }
        }
        R_INPUT => {
            if dest {
                return Err(invalid("input"));
            }
            let limit = if pixel {
                if major >= 3 { 9 } else { 1 }
            } else {
                15
            };
            if index > limit {
                return Err(invalid("input"));
            }
        }
        R_CONST => {
            let limit = if !pixel {
                255
            } else if major >= 3 {
                223
            } else if major >= 2 {
                31
            } else {
                7
            };
            if index > limit {
                return Err(invalid("constant"));
            }
        }
        R_ADDR => {
            if !pixel {
                if index != 0 {
                    return Err(invalid("address"));
                }
            } else {
                if dest && major >= 2 {
                    return Err(invalid("texture"));
                }
                let limit = if major == 1 && minor <= 3 {
                    3
                } else if major == 1 {
                    5
                } else {
                    7
                };
                if index > limit {
                    return Err(invalid("texture"));
                }
            }
        }
        R_RASTOUT => {
            if pixel || !dest || index > 2 {
                return Err(invalid("rastout"));
            }
        }
        R_ATTROUT => {
            if pixel || !dest || index > 1 {
                return Err(invalid("attribout"));
            }
        }
        R_TEXCRDOUT => {
            let limit = if major >= 3 { 11 } else { 7 };
            if pixel || !dest || index > limit {
                return Err(invalid("output"));
            }
        }
        R_CONSTINT => {
            if dest || major < 2 || index > 15 {
                return Err(invalid("integer constant"));
            }
        }
        R_COLOROUT => {
            if !pixel || major < 2 || !dest || index > 3 {
                return Err(invalid("colorout"));
            }
        }
        R_DEPTHOUT => {
            if !pixel || major < 2 || !dest || index != 0 {
                return Err(invalid("depthout"));
            }
        }
        R_SAMPLER => {
            if !pixel || dest {
                return Err(invalid("sampler"));
            }
            let limit = if major >= 2 { 15 } else { 3 };
            if index > limit {
                return Err(invalid("sampler"));
            }
        }
        R_CONSTBOOL => {
            if dest || major < 2 || index > 15 {
                return Err(invalid("boolean constant"));
            }
        }
        _ => return Err(invalid("register type")),
    }
    Ok(())
}

fn uniform_component(swz: u32) -> Option<u32> {
    let c = swz & 3;
    if (0..4).all(|k| (swz >> (2 * k)) & 3 == c) {
        Some(c)
    } else {
        None
    }
}

impl Program {
    pub fn decode(words: &[u32], pixel: bool) -> Result<Self, RenderError> {
        let count = shader_word_count(words, pixel)?;
        let words = &words[..count];
        let version = words[0];
        let mut p = Program {
            pixel,
            major: (version >> 8) & 0xff,
            minor: version & 0xff,
            ..Default::default()
        };
        let mut i = 1usize;
        while i < words.len() {
            let t = words[i];
            let op = t & 0xffff;
            if op == OP_END {
                break;
            }
            if op == OP_COMMENT {
                i += 1 + ((t >> 16) & 0x7fff) as usize;
                continue;
            }
            if op == OP_PHASE {
                i += 1;
                continue;
            }
            let count = operand_count(op, p.major, p.minor, p.pixel)
                .ok_or_else(|| fail(format!("instruction {op} is not implemented")))?;
            let len = if p.major >= 2 {
                ((t >> 24) & 0xf) as usize
            } else {
                count as usize
            };
            let at = i + 1;
            if at + len > words.len() {
                return Err(fail("shader instruction extends past the staged bytecode"));
            }
            if op == OP_DCL {
                let decl = words[at];
                let d = dst_of(words[at + 1]);
                let mut seen = false;
                match d.ty {
                    R_SAMPLER => {
                        check_reg(R_SAMPLER, d.index, p.pixel, p.major, p.minor, false)?;
                        let kind = (decl >> 27) & 0xf;
                        if !matches!(kind, 2 | 3 | 4) {
                            return Err(fail(format!("sampler texture type {kind}")));
                        }
                        if let Some(&existing) = p.samplers.get(&d.index) {
                            if existing != kind {
                                return Err(fail(format!(
                                    "sampler s{} is declared with conflicting texture types",
                                    d.index
                                )));
                            }
                            seen = true;
                        }
                        if !seen {
                            p.samplers.insert(d.index, kind);
                        }
                    }
                    R_INPUT => {
                        if decl & 0x1f > 13 {
                            return Err(fail(format!("declaration usage {}", decl & 0x1f)));
                        }
                        check_reg(R_INPUT, d.index, p.pixel, p.major, p.minor, false)?;
                        for existing in &p.inputs {
                            if existing.ty == d.ty && existing.index == d.index {
                                if existing.usage != decl & 0x1f
                                    || existing.usage_index != (decl >> 16) & 0xf
                                {
                                    return Err(fail(format!(
                                        "input register {} is declared with conflicting semantics",
                                        d.index
                                    )));
                                }
                                seen = true;
                            }
                        }
                        if !seen {
                            p.inputs.push(DclIn {
                                usage: decl & 0x1f,
                                usage_index: (decl >> 16) & 0xf,
                                ty: d.ty,
                                index: d.index,
                            });
                        }
                    }
                    R_TEXCRDOUT if !p.pixel && p.major >= 3 => {
                        if decl & 0x1f > 13 {
                            return Err(fail(format!("declaration usage {}", decl & 0x1f)));
                        }
                        check_reg(R_TEXCRDOUT, d.index, p.pixel, p.major, p.minor, true)?;
                        for existing in &p.outputs {
                            if existing.index == d.index {
                                if existing.usage != decl & 0x1f
                                    || existing.usage_index != (decl >> 16) & 0xf
                                {
                                    return Err(fail(format!(
                                        "output register {} is declared with conflicting semantics",
                                        d.index
                                    )));
                                }
                                seen = true;
                            }
                        }
                        if !seen {
                            p.outputs.push(DclIn {
                                usage: decl & 0x1f,
                                usage_index: (decl >> 16) & 0xf,
                                ty: d.ty,
                                index: d.index,
                            });
                        }
                    }
                    R_ADDR if p.pixel => {
                        check_reg(R_ADDR, d.index, p.pixel, p.major, p.minor, false)?;
                        for existing in &p.inputs {
                            if existing.ty == d.ty && existing.index == d.index {
                                if existing.usage != decl & 0x1f
                                    || existing.usage_index != (decl >> 16) & 0xf
                                {
                                    return Err(fail(format!(
                                        "input register {} is declared with conflicting semantics",
                                        d.index
                                    )));
                                }
                                seen = true;
                            }
                        }
                        if !seen {
                            p.inputs.push(DclIn {
                                usage: decl & 0x1f,
                                usage_index: (decl >> 16) & 0xf,
                                ty: d.ty,
                                index: d.index,
                            });
                        }
                    }
                    _ => {
                        return Err(fail(format!("declaration register type {}", d.ty)));
                    }
                }
                i = at + len;
                continue;
            }
            if op == OP_DEF {
                let d = dst_of(words[at]);
                if d.ty != R_CONST {
                    return Err(fail("DEF destination is not a constant register"));
                }
                check_reg(R_CONST, d.index, p.pixel, p.major, p.minor, false)?;
                p.defs.insert(
                    d.index,
                    std::array::from_fn(|k| f32::from_bits(words[at + 1 + k])),
                );
                p.max_const = p.max_const.max(d.index + 1);
                i = at + len;
                continue;
            }
            if op == OP_DEFI {
                let d = dst_of(words[at]);
                if d.ty != R_CONSTINT || d.index > 15 {
                    return Err(fail("DEFI destination is not an integer constant register"));
                }
                p.idefs
                    .insert(d.index, std::array::from_fn(|k| words[at + 1 + k] as i32));
                i = at + len;
                continue;
            }
            if op == OP_DEFB {
                let d = dst_of(words[at]);
                if d.ty != R_CONSTBOOL || d.index > 15 {
                    return Err(fail("DEFB destination is not a boolean constant register"));
                }
                p.bdefs.insert(d.index, words[at + 1] != 0);
                i = at + len;
                continue;
            }
            let mut inst = Inst {
                op,
                ctrl: (t >> 16) & 0xff,
                coissue: t & 0x4000_0000 != 0,
                ..Default::default()
            };
            if t & 0x1000_0000 != 0 {
                return Err(fail("predicated instructions are not implemented"));
            }
            if inst.coissue && !(p.pixel && p.major == 1) {
                return Err(fail(
                    "coissue is only implemented for pixel shader model 1.x",
                ));
            }
            let comparison = matches!(op, OP_IFC | OP_BREAKC);
            if comparison {
                if !(1..=6).contains(&inst.ctrl) {
                    return Err(fail(format!(
                        "comparison control {} is not implemented",
                        inst.ctrl
                    )));
                }
            } else if inst.ctrl != 0 && !(op == OP_TEX && inst.ctrl <= 2) {
                return Err(fail(format!(
                    "instruction control {} is not implemented",
                    inst.ctrl
                )));
            }
            let control_sources = match op {
                OP_REP | OP_IF => Some(1),
                OP_IFC | OP_BREAKC => Some(2),
                OP_ENDREP | OP_ELSE | OP_ENDIF | OP_BREAK => Some(0),
                _ => None,
            };
            if let Some(expected) = control_sources {
                let mut k = at;
                let end = at + len;
                while k < end && inst.nsrc < 4 {
                    read_src(&mut p, words, &mut k, end, &mut inst)?;
                }
                if inst.nsrc != expected {
                    return Err(fail(format!(
                        "instruction {op} has {} source operands, expected {expected}",
                        inst.nsrc
                    )));
                }
                p.code.push(inst);
                i = end;
                continue;
            }
            if op == OP_TEXKILL {
                let (s, _) = src_of(words[at], p.major);
                check_reg(s.ty, s.index, p.pixel, p.major, p.minor, false)?;
                inst.src[0] = s;
                inst.nsrc = 1;
                p.code.push(inst);
                i = at + len;
                continue;
            }
            let mut k = at;
            let end = at + len;
            if count > 0 {
                inst.dst = dst_of(words[k]);
                check_reg(inst.dst.ty, inst.dst.index, p.pixel, p.major, p.minor, true)?;
                if inst.dst.mask == 0 {
                    return Err(fail("instruction has an empty destination mask"));
                }
                k += 1;
                if p.major >= 2 && (words[k - 1] >> 13) & 1 != 0 {
                    return Err(fail("relative destination addressing is not implemented"));
                }
                if op == OP_MOVA && (p.pixel || inst.dst.ty != R_ADDR) {
                    return Err(fail("MOVA destination is not a0"));
                }
            }
            while k < end && inst.nsrc < 4 {
                read_src(&mut p, words, &mut k, end, &mut inst)?;
            }
            if matches!(op, OP_M4X4 | OP_M4X3 | OP_M3X4 | OP_M3X3 | OP_M3X2)
                && inst.nsrc > 1
                && inst.src[1].ty == R_CONST
            {
                p.max_const = p.max_const.max(inst.src[1].index + 4);
            }
            p.code.push(inst);
            i = end;
        }
        if p.relative {
            p.max_const = if pixel { 32 } else { 256 };
        }
        Ok(p)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Variant {
    pub unbound: u16,
    pub cube: u16,
    pub volume: u16,
    pub projected: u16,
    pub depth: u16,
    pub alpha: u16,
    pub raw_inputs: u16,
}

const PRELUDE: &str = r#"diagnostic(off, derivative_uniformity);
fn d9_dz(v: vec4<f32>) -> vec4<f32> { return vec4<f32>(v.x / v.z, v.y / v.z, v.z, v.w); }
fn d9_dw(v: vec4<f32>) -> vec4<f32> { return vec4<f32>(v.x / v.w, v.y / v.w, v.z, v.w); }
fn d9_lit(a: vec4<f32>) -> vec4<f32> { var o = vec4<f32>(1.0, max(a.x, 0.0), 0.0, 1.0); if (a.x > 0.0 && a.y > 0.0) { o.z = pow(a.y, clamp(a.w, -127.9961, 127.9961)); } return o; }
fn d9_nrm(a: vec4<f32>) -> vec4<f32> { let l = length(a.xyz); if (l > 0.0) { return a / l; } return vec4<f32>(0.0); }
fn d9_round(v: vec4<f32>) -> vec4<f32> { return sign(v) * floor(abs(v) + 0.5); }
fn d9_alpha(v: vec4<f32>) -> vec4<f32> { return vec4<f32>(0.0, 0.0, 0.0, v.r); }
fn d9_shadow(t: texture_depth_2d, sm: sampler_comparison, c: vec4<f32>) -> vec4<f32> { let r = textureSampleCompareLevel(t, sm, c.xy, clamp(c.z, 0.0, 1.0)); return vec4<f32>(r, r, r, 1.0); }
struct VSP { halfpix: vec4<f32>, ascale: array<vec4<f32>, 16> }
struct PSP { alpha_ref: f32, alpha_func: i32, fog_mode: i32, pad0: i32, fog_color: vec4<f32>, fog_start: f32, fog_end: f32, fog_density: f32, pad1: f32 }
struct V2F { @builtin(position) pos: vec4<f32>, @location(0) d0: vec4<f32>, @location(1) d1: vec4<f32>, @location(2) t0: vec4<f32>, @location(3) t1: vec4<f32>, @location(4) t2: vec4<f32>, @location(5) t3: vec4<f32>, @location(6) t4: vec4<f32>, @location(7) t5: vec4<f32>, @location(8) t6: vec4<f32>, @location(9) t7: vec4<f32>, @location(10) fog: f32, @location(11) fogz: f32 }
"#;

const COMP: &[u8; 4] = b"xyzw";

fn swizzle(swz: u32) -> String {
    (0..4)
        .map(|k| COMP[((swz >> (2 * k)) & 3) as usize] as char)
        .collect()
}

struct Gen<'a> {
    p: &'a Program,
    variant: &'a Variant,
    body: String,
    stages: BTreeSet<u32>,
    failed: Option<String>,
    seq: u32,
}

impl<'a> Gen<'a> {
    fn new(p: &'a Program, variant: &'a Variant) -> Self {
        Self {
            p,
            variant,
            body: String::new(),
            stages: BTreeSet::new(),
            failed: None,
            seq: 0,
        }
    }

    fn fail(&mut self, cause: impl Into<String>) {
        if self.failed.is_none() {
            self.failed = Some(cause.into());
        }
    }

    fn const_limit(&self) -> u32 {
        self.p.const_limit()
    }

    fn stage_kind(&self, stage: u32) -> u32 {
        match self.p.samplers.get(&stage).copied() {
            Some(kind) => kind,
            None if self.variant.cube & (1u16 << stage) != 0 => S_CUBE,
            None if self.variant.volume & (1u16 << stage) != 0 => S_VOLUME,
            None => 0,
        }
    }

    fn base(&mut self, s: &Src) -> String {
        let pixel = self.p.pixel;
        match s.ty {
            R_TEMP => format!("r[{}]", s.index),
            R_INPUT => {
                if !pixel {
                    let declared = self
                        .p
                        .inputs
                        .iter()
                        .any(|d| d.ty == R_INPUT && d.index == s.index);
                    if !declared {
                        self.fail(format!(
                            "vertex input v{} is read without a declaration",
                            s.index
                        ));
                    }
                    format!("vv[{}]", s.index)
                } else {
                    format!("iv[{}]", s.index)
                }
            }
            R_CONST => {
                let bank = if pixel { "pcb" } else { "vcb" };
                if s.rel {
                    let index = format!(
                        "clamp(i32(a0.{}) + {}, 0, {})",
                        COMP[s.rel_comp as usize] as char,
                        s.index,
                        self.const_limit() - 1
                    );
                    if self.p.defs.is_empty() {
                        format!("{bank}[{index}]")
                    } else {
                        format!("d9_const({index})")
                    }
                } else if self.p.defs.contains_key(&s.index) {
                    format!("d9_c{}()", s.index)
                } else {
                    format!("{bank}[{}]", s.index)
                }
            }
            R_ADDR => {
                if !pixel {
                    "a0".into()
                } else {
                    format!("t[{}]", s.index)
                }
            }
            R_CONSTINT => match self.p.idefs.get(&s.index) {
                Some(v) => format!("vec4<f32>({}.0, {}.0, {}.0, {}.0)", v[0], v[1], v[2], v[3]),
                None => {
                    self.fail(format!("integer constant i{} has no defi", s.index));
                    "vec4<f32>(0.0)".into()
                }
            },
            R_CONSTBOOL => match self.p.bdefs.get(&s.index) {
                Some(true) => "vec4<f32>(1.0)".into(),
                Some(false) => "vec4<f32>(0.0)".into(),
                None => {
                    self.fail(format!("boolean constant b{} has no defb", s.index));
                    "vec4<f32>(0.0)".into()
                }
            },
            _ => {
                self.fail(format!("source register type {}", s.ty));
                "junk".into()
            }
        }
    }

    fn src(&mut self, s: &Src) -> String {
        let b = self.base(s);
        let v = format!("({b}).{}", swizzle(s.swz));
        match s.md {
            0 => v,
            1 => format!("(-{v})"),
            2 => format!("({v} - vec4<f32>(0.5))"),
            3 => format!("(-({v} - vec4<f32>(0.5)))"),
            4 => format!("(2.0 * {v} - vec4<f32>(1.0))"),
            5 => format!("(-(2.0 * {v} - vec4<f32>(1.0)))"),
            6 => format!("(vec4<f32>(1.0) - {v})"),
            7 => format!("(2.0 * {v})"),
            8 => format!("(-(2.0 * {v}))"),
            9 => format!("d9_dz({v})"),
            10 => format!("d9_dw({v})"),
            11 => format!("abs({v})"),
            12 => format!("(-abs({v}))"),
            13 => format!("select(vec4<f32>(1.0), vec4<f32>(0.0), {v} != vec4<f32>(0.0))"),
            _ => {
                self.fail(format!("source modifier {}", s.md));
                v
            }
        }
    }

    fn s(&mut self, inst: &Inst, k: usize) -> String {
        self.src(&inst.src[k])
    }

    fn dst_name(&mut self, d: &Dst) -> String {
        let pixel = self.p.pixel;
        match d.ty {
            R_TEMP => format!("r[{}]", d.index),
            R_ADDR => {
                if !pixel {
                    "a0".into()
                } else {
                    format!("t[{}]", d.index)
                }
            }
            R_RASTOUT => match d.index {
                0 => "opos".into(),
                1 => "ofog".into(),
                _ => {
                    self.fail("point size output is not implemented");
                    "junk".into()
                }
            },
            R_ATTROUT => format!("od[{}]", d.index),
            R_TEXCRDOUT => {
                if self.p.major >= 3 {
                    format!("ov[{}]", d.index)
                } else {
                    format!("ot[{}]", d.index)
                }
            }
            R_COLOROUT => {
                if d.index != 0 {
                    eprintln!(
                        "d3d9: dropping color output oC{}; multiple render targets are not implemented",
                        d.index
                    );
                }
                format!("oc[{}]", d.index)
            }
            R_DEPTHOUT => {
                self.fail("pixel shader depth output is not implemented");
                "junk".into()
            }
            _ => {
                self.fail(format!("destination register type {}", d.ty));
                "junk".into()
            }
        }
    }

    fn dest_value(&mut self, d: &Dst, v: &str) -> String {
        let mut v = format!("({v})");
        if d.shift > 0 {
            v = format!("({v} * {}.0)", 1u32 << d.shift);
        } else if d.shift < 0 {
            v = format!("({v} / {}.0)", 1u32 << (-d.shift));
        }
        if d.saturate {
            v = format!("clamp({v}, vec4<f32>(0.0), vec4<f32>(1.0))");
        }
        if !self.p.pixel && d.ty == R_ADDR {
            v = format!("d9_round({v})");
        }
        v
    }

    fn commit(&mut self, d: &Dst, temp: &str) {
        if d.mask == 0 {
            return;
        }
        let name = self.dst_name(d);
        if d.mask == 0xf {
            self.body += &format!("{name} = {temp};\n");
            return;
        }
        let mut line = format!("{{ let w_ = {temp};");
        for k in 0..4 {
            if d.mask & (1 << k) != 0 {
                line += &format!(" {name}.{} = w_.{};", COMP[k] as char, COMP[k] as char);
            }
        }
        line += " }\n";
        self.body += &line;
    }

    fn write(&mut self, d: &Dst, v: &str) {
        let value = self.dest_value(d, v);
        self.commit(d, &value);
    }

    fn sample(&mut self, stage: u32, coord: &str, projected: bool, bias: bool) -> String {
        if self.p.pixel && self.p.major >= 2 && !self.p.samplers.contains_key(&stage) {
            self.fail(format!(
                "pixel shader 2.0 samples undeclared sampler s{stage}"
            ));
            return "vec4<f32>(0.0)".into();
        }
        if self.variant.unbound & (1u16 << stage) != 0 {
            return "vec4<f32>(0.0, 0.0, 0.0, 1.0)".into();
        }
        self.stages.insert(stage);
        let kind = self.stage_kind(stage);
        let c = if projected {
            format!("({coord} / ({coord}).w)")
        } else {
            coord.to_string()
        };
        if self.variant.depth & (1u16 << stage) != 0 {
            return format!("d9_shadow(tx{stage}, sm{stage}, {c})");
        }
        let lookup = if kind == S_CUBE || kind == S_VOLUME {
            format!("({c}).xyz")
        } else {
            format!("({c}).xy")
        };
        let sampled = if bias {
            format!("textureSampleBias(tx{stage}, sm{stage}, {lookup}, ({coord}).w)")
        } else {
            format!("textureSample(tx{stage}, sm{stage}, {lookup})")
        };
        if self.variant.alpha & (1u16 << stage) != 0 {
            format!("d9_alpha({sampled})")
        } else {
            sampled
        }
    }

    fn matrix_expr(&mut self, inst: &Inst) -> String {
        let cols = if inst.op == OP_M4X4 || inst.op == OP_M4X3 {
            4
        } else {
            3
        };
        let rows = match inst.op {
            OP_M4X4 | OP_M3X4 => 4,
            OP_M3X2 => 2,
            _ => 3,
        };
        let a = self.s(inst, 0);
        let mut e = Vec::new();
        for row in 0..4 {
            if row >= rows {
                e.push("0.0".into());
                continue;
            }
            let mut m = inst.src[1];
            m.index += row as u32;
            let line = self.src(&m);
            e.push(if cols == 4 {
                format!("dot({a}, {line})")
            } else {
                format!("dot(({a}).xyz, ({line}).xyz)")
            });
        }
        format!("vec4<f32>({})", e.join(", "))
    }

    fn pure(&mut self, inst: &Inst) -> Option<String> {
        let major = self.p.major;
        let minor = self.p.minor;
        let pixel = self.p.pixel;
        Some(match inst.op {
            OP_MOV | OP_MOVA => self.s(inst, 0),
            OP_ADD => format!("({} + {})", self.s(inst, 0), self.s(inst, 1)),
            OP_SUB => format!("({} - {})", self.s(inst, 0), self.s(inst, 1)),
            OP_MUL => format!("({} * {})", self.s(inst, 0), self.s(inst, 1)),
            OP_MAD => format!(
                "({} * {} + {})",
                self.s(inst, 0),
                self.s(inst, 1),
                self.s(inst, 2)
            ),
            OP_MIN => format!("min({}, {})", self.s(inst, 0), self.s(inst, 1)),
            OP_MAX => format!("max({}, {})", self.s(inst, 0), self.s(inst, 1)),
            OP_SLT => format!(
                "select(vec4<f32>(0.0), vec4<f32>(1.0), {} < {})",
                self.s(inst, 0),
                self.s(inst, 1)
            ),
            OP_SGE => format!(
                "select(vec4<f32>(0.0), vec4<f32>(1.0), {} >= {})",
                self.s(inst, 0),
                self.s(inst, 1)
            ),
            OP_DP3 => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(dot(({a}).xyz, ({b}).xyz))")
            }
            OP_DP4 => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(dot({a}, {b}))")
            }
            OP_DP2ADD => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                let c = self.s(inst, 2);
                format!("vec4<f32>(dot(({a}).xy, ({b}).xy) + ({c}).x)")
            }
            OP_DSX => format!("dpdx({})", self.s(inst, 0)),
            OP_DSY => format!("dpdy({})", self.s(inst, 0)),
            OP_RCP => format!("vec4<f32>(1.0 / ({}).x)", self.s(inst, 0)),
            OP_RSQ => format!("vec4<f32>(inverseSqrt(abs(({}).x)))", self.s(inst, 0)),
            OP_FRC => format!("fract({})", self.s(inst, 0)),
            OP_ABS => format!("abs({})", self.s(inst, 0)),
            OP_EXP => format!("vec4<f32>(exp2(({}).x))", self.s(inst, 0)),
            OP_EXPP => {
                let a = self.s(inst, 0);
                if major == 1 {
                    format!("vec4<f32>(exp2(floor(({a}).x)), fract(({a}).x), exp2(({a}).x), 1.0)")
                } else {
                    format!("vec4<f32>(exp2(({a}).x))")
                }
            }
            OP_LOG => format!("vec4<f32>(log2(abs(({}).x)))", self.s(inst, 0)),
            OP_LOGP => {
                let a = self.s(inst, 0);
                if major == 1 {
                    format!(
                        "vec4<f32>(floor(log2(abs(({a}).x))), abs(({a}).x) / exp2(floor(log2(abs(({a}).x)))), log2(abs(({a}).x)), 1.0)"
                    )
                } else {
                    format!("vec4<f32>(log2(abs(({a}).x)))")
                }
            }
            OP_POW => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(pow(abs(({a}).x), ({b}).x))")
            }
            OP_NRM => format!("d9_nrm({})", self.s(inst, 0)),
            OP_LRP => format!(
                "mix({}, {}, {})",
                self.s(inst, 2),
                self.s(inst, 1),
                self.s(inst, 0)
            ),
            OP_CND => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                let c = self.s(inst, 2);
                if pixel && major == 1 && minor == 4 {
                    format!("select({c}, {b}, {a} > vec4<f32>(0.5))")
                } else {
                    format!("select({c}, {b}, ({a}).w > 0.5)")
                }
            }
            OP_CMP => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                let c = self.s(inst, 2);
                format!("select({c}, {b}, {a} >= vec4<f32>(0.0))")
            }
            OP_LIT => format!("d9_lit({})", self.s(inst, 0)),
            OP_SINCOS => {
                let a = self.s(inst, 0);
                format!("vec4<f32>(cos(({a}).x), sin(({a}).x), 0.0, 0.0)")
            }
            OP_DST => {
                let a = self.s(inst, 0);
                let b = self.s(inst, 1);
                format!("vec4<f32>(1.0, ({a}).y * ({b}).y, ({a}).z, ({b}).w)")
            }
            OP_M4X4 | OP_M4X3 | OP_M3X4 | OP_M3X3 | OP_M3X2 => self.matrix_expr(inst),
            _ => return None,
        })
    }

    fn texcoord(&mut self, inst: &Inst) {
        if inst.nsrc > 0 {
            let a = self.s(inst, 0);
            self.write(&inst.dst, &format!("vec4<f32>(({a}).xyz, 1.0)"));
        } else {
            let n = inst.dst.index;
            self.write(
                &inst.dst,
                &format!("vec4<f32>(clamp(t[{n}].xyz, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0)"),
            );
        }
    }

    fn texkill(&mut self, inst: &Inst) {
        let s = inst.src[0];
        let v = if self.p.pixel && self.p.major == 1 && self.p.minor <= 3 && s.ty == R_ADDR {
            format!("tc[{}]", s.index)
        } else {
            self.base(&s)
        };
        if self.p.major >= 2 {
            self.body += &format!("if (any({v} < vec4<f32>(0.0))) {{ discard; }}\n");
        } else {
            self.body += &format!("if (any(({v}).xyz < vec3<f32>(0.0))) {{ discard; }}\n");
        }
    }

    fn texreg(&mut self, inst: &Inst, a: u32, b: u32) {
        let n = inst.dst.index;
        let src = self.s(inst, 0);
        let coord = format!(
            "vec4<f32>(({src}).{}, ({src}).{}, 0.0, 1.0)",
            COMP[a as usize] as char, COMP[b as usize] as char
        );
        let sampled = self.sample(n, &coord, false, false);
        self.write(&inst.dst, &sampled);
    }

    fn tex(&mut self, inst: &Inst) {
        if self.p.major >= 2 {
            let sampler = inst.src[1];
            if sampler.ty != R_SAMPLER || sampler.md != 0 || sampler.rel || sampler.swz != 0xe4 {
                self.fail("TEX sampler operand is not a canonical sampler register");
            }
            let stage = sampler.index;
            let coord = self.s(inst, 0);
            let sampled = self.sample(stage, &coord, inst.ctrl == 1, inst.ctrl == 2);
            self.write(&inst.dst, &sampled);
        } else if inst.nsrc > 0 {
            let n = inst.dst.index;
            let coord = self.s(inst, 0);
            let projected = self.variant.projected & (1u16 << n) != 0;
            let sampled = self.sample(n, &coord, projected, false);
            self.write(&inst.dst, &sampled);
        } else {
            let n = inst.dst.index;
            let coord = format!("t[{n}]");
            let projected = self.variant.projected & (1u16 << n) != 0;
            let sampled = self.sample(n, &coord, projected, false);
            self.write(&inst.dst, &sampled);
        }
    }

    fn instruction(&mut self, inst: &Inst) {
        match inst.op {
            OP_NOP => {}
            OP_TEXCOORD => self.texcoord(inst),
            OP_TEXKILL => self.texkill(inst),
            OP_TEX => self.tex(inst),
            OP_TEXREG2AR => self.texreg(inst, 3, 0),
            OP_TEXREG2GB => self.texreg(inst, 1, 2),
            _ => {
                if let Some(v) = self.pure(inst) {
                    self.write(&inst.dst, &v);
                } else {
                    self.fail(format!("instruction {} is not translated", inst.op));
                }
            }
        }
    }

    fn coissue_pair(&mut self, first: &Inst, second: &Inst) {
        let (Some(va), Some(vb)) = (self.pure(first), self.pure(second)) else {
            self.fail("coissue pair contains a non-arithmetic instruction");
            return;
        };
        let (da, db) = (first.dst, second.dst);
        let na = self.dst_name(&da);
        let nb = self.dst_name(&db);
        if na == nb && (da.mask & db.mask) != 0 {
            self.fail("coissue pair writes overlapping destination components");
            return;
        }
        let ta = self.dest_value(&da, &va);
        let tb = self.dest_value(&db, &vb);
        let n = self.seq;
        self.seq += 1;
        self.body += &format!("let pair{n}a = {ta};\nlet pair{n}b = {tb};\n");
        self.commit(&da, &format!("pair{n}a"));
        self.commit(&db, &format!("pair{n}b"));
    }

    fn comparison(&mut self, inst: &Inst) -> String {
        let a = self.s(inst, 0);
        let b = self.s(inst, 1);
        let op = match inst.ctrl {
            1 => ">",
            2 => "==",
            3 => ">=",
            4 => "<",
            5 => "!=",
            6 => "<=",
            _ => {
                self.fail(format!(
                    "comparison control {} is not implemented",
                    inst.ctrl
                ));
                "=="
            }
        };
        format!("((({a}).x) {op} (({b}).x))")
    }

    fn rep(&mut self, code: &[Inst], i: &mut usize, loops: u32) {
        let inst = code[*i];
        *i += 1;
        if inst.src[0].ty != R_CONSTINT {
            self.fail("REP count is not an integer constant");
            return;
        }
        let count = self.s(&inst, 0);
        let n = self.seq;
        self.seq += 1;
        self.body += &format!("let rep{n} = bitcast<u32>(i32(({count}).x));\n");
        self.body += &format!("for (var rep_i{n}: u32 = 0u; rep_i{n} < rep{n}; rep_i{n}++) {{\n");
        let stop = self.emit_sequence(code, i, &[OP_ENDREP], loops + 1);
        if stop == Some(OP_ENDREP) {
            *i += 1;
        } else {
            self.fail("REP has no matching ENDREP");
        }
        self.body += "}\n";
    }

    fn conditional(&mut self, code: &[Inst], i: &mut usize, loops: u32) {
        let inst = code[*i];
        *i += 1;
        let cond = if inst.op == OP_IFC {
            self.comparison(&inst)
        } else {
            let value = self.s(&inst, 0);
            format!("(({value}).x != 0.0)")
        };
        self.body += &format!("if ({cond}) {{\n");
        let stop = self.emit_sequence(code, i, &[OP_ELSE, OP_ENDIF], loops);
        match stop {
            Some(OP_ENDIF) => {
                *i += 1;
            }
            Some(OP_ELSE) => {
                *i += 1;
                self.body += "} else {\n";
                let stop = self.emit_sequence(code, i, &[OP_ENDIF], loops);
                if stop == Some(OP_ENDIF) {
                    *i += 1;
                } else {
                    self.fail("IF has no matching ENDIF");
                }
            }
            _ => self.fail("IF has no matching ENDIF"),
        }
        self.body += "}\n";
    }

    fn emit_sequence(
        &mut self,
        code: &[Inst],
        i: &mut usize,
        stops: &[u32],
        loops: u32,
    ) -> Option<u32> {
        while *i < code.len() {
            let op = code[*i].op;
            if stops.contains(&op) {
                return Some(op);
            }
            if code[*i].coissue {
                self.fail("coissue instruction without a preceding instruction");
                *i += 1;
                continue;
            }
            if *i + 1 < code.len() && code[*i + 1].coissue {
                self.coissue_pair(&code[*i], &code[*i + 1]);
                *i += 2;
            } else {
                match op {
                    OP_REP => self.rep(code, i, loops),
                    OP_IF | OP_IFC => self.conditional(code, i, loops),
                    OP_BREAK => {
                        if loops == 0 {
                            self.fail("BREAK is used outside a REP loop");
                        } else {
                            self.body += "break;\n";
                        }
                        *i += 1;
                    }
                    OP_BREAKC => {
                        let inst = code[*i];
                        *i += 1;
                        if loops == 0 {
                            self.fail("BREAKC is used outside a REP loop");
                        } else {
                            let cond = self.comparison(&inst);
                            self.body += &format!("if ({cond}) {{ break; }}\n");
                        }
                    }
                    OP_ENDREP | OP_ELSE | OP_ENDIF => {
                        self.fail(format!("unexpected control instruction {op}"));
                        *i += 1;
                    }
                    _ => {
                        let inst = code[*i];
                        self.instruction(&inst);
                        *i += 1;
                    }
                }
            }
            if self.failed.is_some() {
                break;
            }
        }
        None
    }

    fn generate(&mut self) -> Result<(), RenderError> {
        let code = self.p.code.clone();
        let mut i = 0usize;
        self.emit_sequence(&code, &mut i, &[], 0);
        if let Some(cause) = self.failed.take() {
            return Err(fail(cause));
        }
        Ok(())
    }

    fn constants(&self) -> String {
        let (binding, bank) = if self.p.pixel { (2, "pcb") } else { (0, "vcb") };
        let mut s = format!(
            "@group(0) @binding({binding}) var<uniform> {bank}: array<vec4<f32>, {}>;\n",
            self.const_limit()
        );
        for (index, value) in &self.p.defs {
            let b = value.map(f32::to_bits);
            s += &format!(
                "fn d9_c{index}() -> vec4<f32> {{ return bitcast<vec4<f32>>(vec4<u32>({}u, {}u, {}u, {}u)); }}\n",
                b[0], b[1], b[2], b[3]
            );
        }
        if self.p.relative && !self.p.defs.is_empty() {
            s += "fn d9_const(index: i32) -> vec4<f32> {\nswitch index {\n";
            for index in self.p.defs.keys() {
                s += &format!("case {index}: {{ return d9_c{index}(); }}\n");
            }
            s += &format!("default: {{ return {bank}[index]; }}\n}}\n}}\n");
        }
        s
    }

    fn compose_vertex(&mut self) -> String {
        let mut inputs = Vec::new();
        for d in &self.p.inputs {
            if d.ty == R_INPUT && !inputs.contains(&d.index) {
                inputs.push(d.index);
            }
        }
        let mut s = String::from("struct VIn {\n");
        if inputs.is_empty() {
            s += "    @builtin(vertex_index) vid: u32,\n";
        } else {
            for i in &inputs {
                let ty = if self.variant.raw_inputs & (1 << i) != 0 {
                    "vec4<u32>"
                } else {
                    "vec4<f32>"
                };
                s += &format!("    @location({i}) v{i}: {ty},\n");
            }
        }
        s += "}\n";
        s += &self.constants();
        s += "@group(0) @binding(1) var<uniform> vsp: VSP;\n";
        s += "@vertex\nfn vs_main(vin: VIn) -> V2F {\n";
        s += "    var r: array<vec4<f32>, 32>;\n";
        s += "    var a0 = vec4<f32>(0.0);\n";
        s += "    var opos = vec4<f32>(0.0);\n";
        s += "    var ofog = vec4<f32>(1.0);\n";
        s += "    var opsize = vec4<f32>(1.0);\n";
        s += "    var od = array<vec4<f32>, 2>(vec4<f32>(0.0), vec4<f32>(0.0));\n";
        if self.p.major >= 3 {
            s += "    var ov: array<vec4<f32>, 12>;\n";
        } else {
            s += "    var ot: array<vec4<f32>, 8>;\n";
        }
        s += "    var vv: array<vec4<f32>, 16>;\n";
        for i in &inputs {
            let value = if self.variant.raw_inputs & (1 << i) != 0 {
                format!("vec4<f32>(vin.v{i})")
            } else {
                format!("vin.v{i}")
            };
            s += &format!("    vv[{i}] = {value} * vsp.ascale[{i}];\n");
        }
        s += &self.body;
        s += "    var o: V2F;\n";
        if self.p.major >= 3 {
            let mut position = "opos".to_string();
            for d in &self.p.outputs {
                let value = format!("ov[{}]", d.index);
                match d.usage {
                    0 | 9 => position = value,
                    4 => {}
                    5 if d.usage_index < 8 => {
                        s += &format!("    o.t{} = {value};\n", d.usage_index);
                    }
                    10 if d.usage_index < 2 => {
                        s += &format!("    o.d{} = {value};\n", d.usage_index);
                    }
                    11 => s += &format!("    o.fog = {value}.x;\n"),
                    _ => self.fail(format!("vertex output usage {}", d.usage)),
                }
            }
            s += &format!(
                "    let d9_pos = vec4<f32>({position}.xy + vsp.halfpix.xy * {position}.w, {position}.zw);\n"
            );
            s += "    o.pos = d9_pos;\n";
            s += "    o.fogz = d9_pos.z;\n";
        } else {
            s += "    o.pos = vec4<f32>(opos.xy + vsp.halfpix.xy * opos.w, opos.zw);\n";
            s += "    o.d0 = od[0];\n    o.d1 = od[1];\n";
            for i in 0..8 {
                s += &format!("    o.t{i} = ot[{i}];\n");
            }
            s += "    o.fog = ofog.x;\n    o.fogz = opos.z;\n";
        }
        s += "    return o;\n}\n";
        s
    }

    fn compose_pixel(&mut self) -> String {
        let mut s = String::new();
        s += &self.constants();
        s += "@group(0) @binding(3) var<uniform> psp: PSP;\n";
        for &stage in &self.stages {
            let depth = self.variant.depth & (1u16 << stage) != 0;
            let kind = self.stage_kind(stage);
            let ty = if depth {
                "texture_depth_2d"
            } else if kind == S_CUBE {
                "texture_cube<f32>"
            } else if kind == S_VOLUME {
                "texture_3d<f32>"
            } else {
                "texture_2d<f32>"
            };
            let sampler = if depth {
                "sampler_comparison"
            } else {
                "sampler"
            };
            s += &format!("@group(0) @binding({}) var tx{stage}: {ty};\n", 4 + stage);
            s += &format!(
                "@group(0) @binding({}) var sm{stage}: {sampler};\n",
                20 + stage
            );
        }
        s += "@fragment\nfn ps_main(vin: V2F) -> @location(0) vec4<f32> {\n";
        s += "    var r: array<vec4<f32>, 32>;\n";
        s += "    var tc = array<vec4<f32>, 8>(vin.t0, vin.t1, vin.t2, vin.t3, vin.t4, vin.t5, vin.t6, vin.t7);\n";
        s += "    var t = tc;\n";
        if self.p.major >= 3 {
            s += "    var iv: array<vec4<f32>, 16>;\n";
            let inputs = self.p.inputs.clone();
            for d in &inputs {
                if d.ty != R_INPUT {
                    continue;
                }
                let value = match (d.usage, d.usage_index) {
                    (5, i) if i < 8 => format!("vin.t{i}"),
                    (10, i) if i < 2 => format!("vin.d{i}"),
                    (11, _) => "vec4<f32>(vin.fog, 0.0, 0.0, 0.0)".to_string(),
                    _ => {
                        self.fail(format!("pixel input usage {}", d.usage));
                        continue;
                    }
                };
                s += &format!("    iv[{}] = {value};\n", d.index);
            }
        } else {
            s += "    var iv = array<vec4<f32>, 2>(vin.d0, vin.d1);\n";
        }
        s += "    var oc: array<vec4<f32>, 4>;\n";
        s += &self.body;
        if self.p.major >= 2 {
            s += "    var col = oc[0];\n";
        } else {
            s += "    var col = clamp(r[0], vec4<f32>(0.0), vec4<f32>(1.0));\n";
        }
        s += "    return col;\n}\n";
        s
    }

    fn compose(&mut self) -> String {
        let mut s = String::from(PRELUDE);
        if self.p.pixel {
            s += &self.compose_pixel();
        } else {
            s += &self.compose_vertex();
        }
        s
    }
}

pub fn translate(p: &Program, variant: &Variant) -> Result<String, RenderError> {
    let mut g = Gen::new(p, variant);
    g.generate()?;
    let source = g.compose();
    if let Some(cause) = g.failed.take() {
        return Err(fail(cause));
    }
    Ok(source)
}

pub fn validate(source: &str) -> Result<(), RenderError> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| {
        fail(format!(
            "translated WGSL does not parse: {}",
            e.emit_to_string(source)
        ))
    })?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .map_err(|e| fail(format!("translated WGSL does not validate: {e:?}")))?;
    Ok(())
}
