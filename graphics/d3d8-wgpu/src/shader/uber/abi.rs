pub(crate) const ABI_VERSION: u32 = 6;
pub(crate) const INTERPRETER_VERSION: u32 = 12;
pub(crate) const MAX_INSTRUCTIONS: usize = 128;
pub(crate) const INSTRUCTION_WORDS: usize = 16;
pub(crate) const TEMP_REGISTERS: u32 = 8;
pub(crate) const VERTEX_INPUTS: u32 = 7;
pub(crate) const VERTEX_CONSTANTS: u32 = 256;
pub(crate) const PIXEL_CONSTANTS: u32 = 32;

pub(crate) const MAX_DEFINITIONS: usize = 32;
pub(crate) const PROGRAM_BYTES: u64 =
    ((4 + MAX_INSTRUCTIONS * INSTRUCTION_WORDS + MAX_DEFINITIONS * 8) * 4) as u64;
pub(crate) const PROGRAM_BINDINGS: [u32; 2] = [36, 37];
pub(crate) const VERTEX_STRIDE: u64 = VERTEX_INPUTS as u64 * 16;
pub(crate) const INTER_STAGE_VARIABLES: u32 = 12;
pub(crate) const IMMEDIATE_SOURCE_SHIFT: u32 = 8;
pub(crate) const OPCODE_MASK: u32 = 255;
pub(crate) const TEXTURE_COORDINATES: u32 = 8;
pub(crate) const TEXTURES_2D: u32 = 5;
pub(crate) const TEXTURE_SLOTS: u32 = TEXTURES_2D + 2;
pub(crate) const SATURATE_FLAG: u32 = 1 << 17;
