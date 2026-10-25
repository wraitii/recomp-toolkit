use super::identity::ProgramKey;
use super::program::*;
use std::sync::Arc;

mod abi;
pub(crate) use abi::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct EncodedKey {
    version: u32,
    original: ProgramKey,
}

impl EncodedKey {
    pub(crate) fn retained_bytecode_bytes(&self) -> usize {
        self.original.byte_len()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Requirements {
    pub(crate) temporaries: u32,
    pub(crate) inputs: u32,
    pub(crate) constants: u32,
    pub(crate) texture_coordinates: u32,
    pub(crate) sampled_slots: u16,
}

#[derive(Clone, Debug)]
pub(crate) struct EncodedProgram {
    pub(crate) key: EncodedKey,
    pub(crate) words: Arc<[u32]>,
    pub(crate) requirements: Requirements,
}

#[derive(Clone, Debug)]
pub(crate) enum Exclusion {
    ShaderModel,
    InstructionCapacity,
    Definitions,
    Declarations,
    Opcode { instruction: usize, opcode: u32 },
    Control { instruction: usize },
    Destination { instruction: usize },
    Source { instruction: usize, operand: u32 },
    SourceCount { instruction: usize },
}

impl Exclusion {
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::ShaderModel => "shader-model",
            Self::InstructionCapacity => "instruction-capacity",
            Self::Definitions => "embedded-definitions",
            Self::Declarations => "declarations",
            Self::Opcode { .. } => "opcode",
            Self::Control { .. } => "instruction-control",
            Self::Destination { .. } => "destination",
            Self::Source { .. } => "source",
            Self::SourceCount { .. } => "source-count",
        }
    }
}

impl std::fmt::Display for Exclusion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())?;
        match self {
            Self::Opcode {
                instruction,
                opcode,
            } => write!(f, " instruction={instruction} opcode={opcode}"),
            Self::Control { instruction }
            | Self::Destination { instruction }
            | Self::SourceCount { instruction } => write!(f, " instruction={instruction}"),
            Self::Source {
                instruction,
                operand,
            } => write!(f, " instruction={instruction} operand={operand}"),
            _ => Ok(()),
        }
    }
}

pub(crate) fn sampler_mapping(p: &Program) -> Vec<(u32, u32)> {
    let mut next_2d = 0;
    p.samplers
        .iter()
        .map(|(&guest, &kind)| {
            let physical = if kind == S_CUBE {
                TEXTURES_2D
            } else {
                let slot = next_2d;
                next_2d += 1;
                slot
            };
            (guest, physical)
        })
        .collect()
}

pub(crate) fn interface_candidate(p: &Program) -> bool {
    if p.pixel {
        (p.samplers.keys().all(|&slot| slot < 16)
            && p.samplers.values().all(|&kind| matches!(kind, 2 | S_CUBE))
            && p.samplers.values().filter(|&&kind| kind == 2).count() <= TEXTURES_2D as usize
            && p.samplers.values().filter(|&&kind| kind == S_CUBE).count() <= 1)
            && p.inputs.iter().all(|d| match d.ty {
                R_INPUT => d.index < 2,
                R_ADDR => d.index < TEXTURE_COORDINATES,
                _ => false,
            })
    } else {
        p.samplers.is_empty()
            && !p.inputs.is_empty()
            && p.inputs.len() <= VERTEX_INPUTS as usize
            && (0..p.inputs.len() as u32)
                .all(|index| p.inputs.iter().filter(|d| d.index == index).count() == 1)
            && p.inputs.iter().all(|d| {
                d.ty == R_INPUT
                    && d.index < VERTEX_INPUTS
                    && matches!(
                        (d.usage, d.usage_index),
                        (0, 0) | (1, 0) | (2, 0) | (3, 0) | (5, 0) | (6, 0) | (10, 0) | (10, 1)
                    )
            })
    }
}

pub(crate) fn lower(p: &Program, original: ProgramKey) -> Result<EncodedProgram, Exclusion> {
    if p.major != 2 || p.minor > 1 {
        return Err(Exclusion::ShaderModel);
    }
    if p.code.is_empty() || p.code.len() > MAX_INSTRUCTIONS {
        return Err(Exclusion::InstructionCapacity);
    }
    if !p.idefs.is_empty()
        || !p.bdefs.is_empty()
        || p.defs.len() > MAX_DEFINITIONS
        || p.defs.keys().any(|&index| {
            index
                >= if p.pixel {
                    PIXEL_CONSTANTS
                } else {
                    VERTEX_CONSTANTS
                }
        })
    {
        return Err(Exclusion::Definitions);
    }
    let precision_modifier = if p.pixel { 2 } else { 0 };
    let allowed_modifiers = precision_modifier | 1;
    if !interface_candidate(p) || p.declaration_modifiers & !precision_modifier != 0 {
        return Err(Exclusion::Declarations);
    }
    let mut requirements = Requirements::default();
    let mut words = Vec::with_capacity(4 + p.code.len() * INSTRUCTION_WORDS);
    words.extend([
        ABI_VERSION,
        u32::from(p.pixel),
        p.code.len() as u32,
        p.defs.len() as u32,
    ]);
    for (instruction, inst) in p.code.iter().enumerate() {
        let expected = match inst.op {
            OP_MOV | OP_FRC | OP_RCP | OP_RSQ | OP_NRM | OP_EXP | OP_ABS => 1,
            OP_MOVA if !p.pixel => 1,
            OP_TEXKILL if p.pixel => 1,
            OP_MAD | OP_LRP | OP_CMP => 3,
            OP_MUL | OP_ADD | OP_DP3 | OP_DP4 | OP_MIN | OP_MAX | OP_SLT | OP_SGE | OP_POW => 2,
            OP_TEX if p.pixel => 2,
            opcode => {
                return Err(Exclusion::Opcode {
                    instruction,
                    opcode,
                });
            }
        };
        if (inst.ctrl != 0 && !(p.pixel && inst.op == OP_TEX && inst.ctrl == 1)) || inst.coissue {
            return Err(Exclusion::Control { instruction });
        }
        if inst.nsrc != expected {
            return Err(Exclusion::SourceCount { instruction });
        }
        if inst.op == OP_TEXKILL {
            let s = inst.src[0];
            match s.ty {
                R_TEMP if s.index < TEMP_REGISTERS => {
                    requirements.temporaries = requirements.temporaries.max(s.index + 1);
                }
                R_ADDR if s.index < TEXTURE_COORDINATES => {
                    requirements.texture_coordinates =
                        requirements.texture_coordinates.max(s.index + 1);
                }
                _ => {
                    return Err(Exclusion::Source {
                        instruction,
                        operand: 0,
                    });
                }
            }
            if s.rel || s.md != 0 || s.swz != 15 {
                return Err(Exclusion::Source {
                    instruction,
                    operand: 0,
                });
            }
            words.extend([OP_TEXKILL, u32::MAX, 0, 0, s.ty, s.index, 0xe4, 0]);
            words.extend([0; 8]);
            continue;
        }
        let d = inst.dst;
        let destination = match d.ty {
            R_TEMP if d.index < TEMP_REGISTERS => {
                requirements.temporaries = requirements.temporaries.max(d.index + 1);
                true
            }
            R_COLOROUT => p.pixel && d.index == 0 && inst.op != OP_TEX,
            R_RASTOUT => !p.pixel && d.index == 0,
            R_TEXCRDOUT => !p.pixel && d.index < TEXTURE_COORDINATES,
            R_ATTROUT => !p.pixel && d.index < 2,
            R_ADDR => !p.pixel && d.index == 0 && inst.op == OP_MOVA,
            _ => false,
        };
        if !destination
            || (inst.op == OP_MOVA && d.ty != R_ADDR)
            || d.mask == 0
            || d.mask > 15
            || d.modifiers & !allowed_modifiers != 0
            || d.shift != 0
            || (inst.op == OP_TEX && d.mask != 15)
        {
            return Err(Exclusion::Destination { instruction });
        }
        let opcode_word = words.len();
        words.extend([
            (inst.op & OPCODE_MASK)
                | (inst.ctrl << 16)
                | if d.saturate { SATURATE_FLAG } else { 0 },
            d.ty,
            d.index,
            d.mask,
        ]);
        for operand in 0..3 {
            if operand >= inst.nsrc {
                words.extend([0; 4]);
                continue;
            }
            let s = inst.src[operand as usize];
            let source = match s.ty {
                R_TEMP if s.index < TEMP_REGISTERS => {
                    requirements.temporaries = requirements.temporaries.max(s.index + 1);
                    true
                }
                R_INPUT if s.index < if p.pixel { 2 } else { VERTEX_INPUTS } => {
                    requirements.inputs = requirements.inputs.max(s.index + 1);
                    p.pixel
                        || p.inputs
                            .iter()
                            .any(|d| d.ty == R_INPUT && d.index == s.index)
                }
                R_CONST
                    if s.index
                        < if p.pixel {
                            PIXEL_CONSTANTS
                        } else {
                            VERTEX_CONSTANTS
                        } =>
                {
                    requirements.constants = requirements.constants.max(if s.rel {
                        VERTEX_CONSTANTS
                    } else {
                        s.index + 1
                    });
                    true
                }
                R_ADDR if p.pixel && s.index < TEXTURE_COORDINATES => {
                    requirements.texture_coordinates =
                        requirements.texture_coordinates.max(s.index + 1);
                    true
                }
                R_SAMPLER
                    if p.pixel
                        && inst.op == OP_TEX
                        && operand == 1
                        && p.samplers.contains_key(&s.index)
                        && (inst.ctrl == 0 || p.samplers.get(&s.index) == Some(&2)) =>
                {
                    requirements.sampled_slots |= 1 << s.index;
                    true
                }
                _ => false,
            };
            if !source
                || s.md > 1
                || (s.rel && (p.pixel || s.ty != R_CONST || s.rel_comp > 3))
                || s.swz > 255
                || (inst.op == OP_TEX && (operand == 1) != (s.ty == R_SAMPLER))
                || (inst.op == OP_TEX
                    && (s.md != 0
                        || s.swz != 0xe4
                        || (operand == 0 && !matches!(s.ty, R_TEMP | R_ADDR))))
            {
                return Err(Exclusion::Source {
                    instruction,
                    operand,
                });
            }
            if let Some(value) = (s.ty == R_CONST && !s.rel)
                .then(|| p.defs.get(&s.index))
                .flatten()
            {
                words[opcode_word] |= 1 << (IMMEDIATE_SOURCE_SHIFT + operand);
                words.extend((0..4).map(|component| {
                    value[((s.swz >> (component * 2)) & 3) as usize].to_bits()
                        ^ if s.md == 1 { 0x8000_0000 } else { 0 }
                }));
            } else {
                let flags = s.md | (u32::from(s.rel) << 8) | (s.rel_comp << 9);
                let index = if s.ty == R_SAMPLER {
                    sampler_mapping(p)
                        .into_iter()
                        .find(|&(guest, _)| guest == s.index)
                        .unwrap()
                        .1
                } else {
                    s.index
                };
                words.extend([s.ty, index, s.swz, flags]);
            }
        }
    }
    words.resize(4 + MAX_INSTRUCTIONS * INSTRUCTION_WORDS, 0);
    for (&index, value) in &p.defs {
        words.extend([index, 0, 0, 0]);
        words.extend(value.map(f32::to_bits));
    }
    words.resize(PROGRAM_BYTES as usize / 4, 0);
    Ok(EncodedProgram {
        key: EncodedKey {
            version: ABI_VERSION,
            original,
        },
        words: words.into(),
        requirements,
    })
}
