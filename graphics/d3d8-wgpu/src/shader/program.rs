pub(crate) use super::opcodes::{OP_DP4, OP_MAD, OP_MOV, OP_MUL, OP_TEX};
use super::validate::shader_word_count;
use crate::RenderError;
use std::collections::BTreeMap;

pub(crate) const R_TEMP: u32 = 0;
pub(crate) const R_INPUT: u32 = 1;
pub(crate) const R_CONST: u32 = 2;
pub(crate) const R_ADDR: u32 = 3;
pub(crate) const R_RASTOUT: u32 = 4;
pub(crate) const R_ATTROUT: u32 = 5;
pub(crate) const R_TEXCRDOUT: u32 = 6;
pub(crate) const R_CONSTINT: u32 = 7;
pub(crate) const R_COLOROUT: u32 = 8;
pub(crate) const R_DEPTHOUT: u32 = 9;
pub(crate) const R_SAMPLER: u32 = 10;
pub(crate) const R_CONSTBOOL: u32 = 14;

pub(crate) const OP_NOP: u32 = 0;
pub(crate) const OP_ADD: u32 = 2;
pub(crate) const OP_SUB: u32 = 3;
pub(crate) const OP_RCP: u32 = 6;
pub(crate) const OP_RSQ: u32 = 7;
pub(crate) const OP_DP3: u32 = 8;
pub(crate) const OP_MIN: u32 = 10;
pub(crate) const OP_MAX: u32 = 11;
pub(crate) const OP_SLT: u32 = 12;
pub(crate) const OP_SGE: u32 = 13;
pub(crate) const OP_EXP: u32 = 14;
pub(crate) const OP_LOG: u32 = 15;
pub(crate) const OP_LIT: u32 = 16;
pub(crate) const OP_DST: u32 = 17;
pub(crate) const OP_LRP: u32 = 18;
pub(crate) const OP_FRC: u32 = 19;
pub(crate) const OP_M4X4: u32 = 20;
pub(crate) const OP_M4X3: u32 = 21;
pub(crate) const OP_M3X4: u32 = 22;
pub(crate) const OP_M3X3: u32 = 23;
pub(crate) const OP_M3X2: u32 = 24;
pub(crate) const OP_REP: u32 = 38;
pub(crate) const OP_ENDREP: u32 = 39;
pub(crate) const OP_IF: u32 = 40;
pub(crate) const OP_IFC: u32 = 41;
pub(crate) const OP_ELSE: u32 = 42;
pub(crate) const OP_ENDIF: u32 = 43;
pub(crate) const OP_BREAK: u32 = 44;
pub(crate) const OP_BREAKC: u32 = 45;
pub(crate) const OP_DCL: u32 = 31;
pub(crate) const OP_POW: u32 = 32;
pub(crate) const OP_CRS: u32 = 33;
pub(crate) const OP_SGN: u32 = 34;
pub(crate) const OP_ABS: u32 = 35;
pub(crate) const OP_NRM: u32 = 36;
pub(crate) const OP_SINCOS: u32 = 37;
pub(crate) const OP_MOVA: u32 = 46;
pub(crate) const OP_DEFB: u32 = 47;
pub(crate) const OP_DEFI: u32 = 48;
pub(crate) const OP_TEXCOORD: u32 = 64;
pub(crate) const OP_TEXKILL: u32 = 65;
pub(crate) const OP_TEXREG2AR: u32 = 69;
pub(crate) const OP_TEXREG2GB: u32 = 70;
pub(crate) const OP_EXPP: u32 = 78;
pub(crate) const OP_LOGP: u32 = 79;
pub(crate) const OP_CND: u32 = 80;
pub(crate) const OP_DEF: u32 = 81;
pub(crate) const OP_TEXDP3: u32 = 85;
pub(crate) const OP_CMP: u32 = 88;
pub(crate) const OP_DP2ADD: u32 = 90;
pub(crate) const OP_DSX: u32 = 91;
pub(crate) const OP_DSY: u32 = 92;
pub(crate) const OP_PHASE: u32 = 0xfffd;
pub(crate) const OP_COMMENT: u32 = 0xfffe;
pub(crate) const OP_END: u32 = 0xffff;

pub(crate) const S_CUBE: u32 = 3;
pub(crate) const S_VOLUME: u32 = 4;

pub(crate) fn fail(cause: impl Into<String>) -> RenderError {
    RenderError::new("CreateShader", cause)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Dst {
    pub ty: u32,
    pub index: u32,
    pub mask: u32,
    pub saturate: bool,
    pub modifiers: u32,
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
    pub declaration_modifiers: u32,
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
        modifiers: (t >> 20) & 0xf,
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
                p.declaration_modifiers |= d.modifiers;
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
