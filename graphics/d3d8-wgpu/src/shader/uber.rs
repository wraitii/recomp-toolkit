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

pub(crate) fn interface_candidate(p: &Program) -> bool {
    if p.pixel {
        p.samplers.len() == 1
            && p.samplers.get(&0) == Some(&2)
            && p.inputs
                .iter()
                .all(|d| d.index == 0 && matches!(d.ty, R_INPUT | R_ADDR))
    } else {
        p.samplers.is_empty()
            && [&[(0, 0), (1, 10), (2, 5)][..], &[(0, 0), (1, 5)][..]]
                .iter()
                .any(|inputs| {
                    p.inputs.len() == inputs.len()
                        && inputs.iter().all(|&(index, usage)| {
                            p.inputs.iter().any(|d| {
                                d.ty == R_INPUT
                                    && d.index == index
                                    && d.usage == usage
                                    && d.usage_index == 0
                            })
                        })
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
    if !interface_candidate(p) || p.relative || p.declaration_modifiers != 0 {
        return Err(Exclusion::Declarations);
    }
    let mut requirements = Requirements::default();
    let mut words = Vec::with_capacity(4 + p.code.len() * INSTRUCTION_WORDS);
    words.extend([ABI_VERSION, u32::from(p.pixel), p.code.len() as u32, 0]);
    for (instruction, inst) in p.code.iter().enumerate() {
        let expected = match inst.op {
            OP_MOV => 1,
            OP_MAD => 3,
            OP_MUL => 2,
            OP_DP4 if !p.pixel => 2,
            OP_TEX if p.pixel => 2,
            opcode => {
                return Err(Exclusion::Opcode {
                    instruction,
                    opcode,
                });
            }
        };
        if inst.ctrl != 0 || inst.coissue {
            return Err(Exclusion::Control { instruction });
        }
        if inst.nsrc != expected {
            return Err(Exclusion::SourceCount { instruction });
        }
        let d = inst.dst;
        let destination = match d.ty {
            R_TEMP if d.index < TEMP_REGISTERS => {
                requirements.temporaries = requirements.temporaries.max(d.index + 1);
                true
            }
            R_COLOROUT => p.pixel && d.index == 0 && inst.op != OP_TEX,
            R_RASTOUT | R_ATTROUT | R_TEXCRDOUT => !p.pixel && d.index == 0,
            _ => false,
        };
        if !destination
            || d.mask == 0
            || d.mask > 15
            || d.modifiers != 0
            || d.shift != 0
            || (inst.op == OP_TEX && d.mask != 15)
        {
            return Err(Exclusion::Destination { instruction });
        }
        let opcode_word = words.len();
        words.extend([inst.op & OPCODE_MASK, d.ty, d.index, d.mask]);
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
                R_INPUT if s.index < if p.pixel { 1 } else { VERTEX_INPUTS } => {
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
                    requirements.constants = requirements.constants.max(s.index + 1);
                    true
                }
                R_ADDR if p.pixel && s.index == 0 => {
                    requirements.texture_coordinates = 1;
                    true
                }
                R_SAMPLER if p.pixel && inst.op == OP_TEX && operand == 1 && s.index == 0 => {
                    requirements.sampled_slots |= 1;
                    true
                }
                _ => false,
            };
            if !source
                || s.md != 0
                || s.rel
                || s.swz > 255
                || (inst.op == OP_TEX && (operand == 1) != (s.ty == R_SAMPLER))
                || (inst.op == OP_TEX
                    && (s.swz != 0xe4 || (operand == 0 && !matches!(s.ty, R_TEMP | R_ADDR))))
            {
                return Err(Exclusion::Source {
                    instruction,
                    operand,
                });
            }
            if let Some(value) = (s.ty == R_CONST).then(|| p.defs.get(&s.index)).flatten() {
                words[opcode_word] |= 1 << (IMMEDIATE_SOURCE_SHIFT + operand);
                words.extend(
                    (0..4).map(|component| {
                        value[((s.swz >> (component * 2)) & 3) as usize].to_bits()
                    }),
                );
            } else {
                words.extend([s.ty, s.index, s.swz, s.md]);
            }
        }
    }
    words.resize(4 + MAX_INSTRUCTIONS * INSTRUCTION_WORDS, 0);
    Ok(EncodedProgram {
        key: EncodedKey {
            version: ABI_VERSION,
            original,
        },
        words: words.into(),
        requirements,
    })
}
