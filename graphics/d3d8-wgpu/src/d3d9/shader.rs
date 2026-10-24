use super::wgsl as shader_wgsl;
use crate::RenderError;
use std::collections::BTreeMap;

const OP_PHASE: u32 = 0xfffd;
const OP_COMMENT: u32 = 0xfffe;
const OP_END: u32 = 0xffff;

pub const MAX_SHADER_WORDS: usize = 4096;
pub const MAX_DECL_ELEMENTS: usize = 64;
pub const D3DDECL_END_STREAM: u16 = 0xff;

pub const D3DDECLTYPE_UNUSED: u8 = 17;
pub const D3DDECLUSAGE_SAMPLE: u8 = 13;
pub const D3DDECLMETHOD_LOOKUPPRESAMPLED: u8 = 6;

pub const D3DFVF_RESERVED0: u32 = 0x0001;
pub const D3DFVF_POSITION_MASK: u32 = 0x400e;
pub const D3DFVF_XYZ: u32 = 0x0002;
pub const D3DFVF_XYZRHW: u32 = 0x0004;
pub const D3DFVF_XYZB1: u32 = 0x0006;
pub const D3DFVF_XYZB5: u32 = 0x000e;
pub const D3DFVF_XYZW: u32 = 0x4002;
pub const D3DFVF_NORMAL: u32 = 0x0010;
pub const D3DFVF_PSIZE: u32 = 0x0020;
pub const D3DFVF_DIFFUSE: u32 = 0x0040;
pub const D3DFVF_SPECULAR: u32 = 0x0080;
pub const D3DFVF_TEXCOUNT_MASK: u32 = 0x0f00;
pub const D3DFVF_TEXCOUNT_SHIFT: u32 = 8;
pub const D3DFVF_LASTBETA_UBYTE4: u32 = 0x1000;
pub const D3DFVF_LASTBETA_D3DCOLOR: u32 = 0x8000;
pub const D3DFVF_RESERVED2: u32 = 0x6000;

const D3DDECLTYPE_FLOAT1: u8 = 0;
const D3DDECLTYPE_FLOAT2: u8 = 1;
const D3DDECLTYPE_FLOAT3: u8 = 2;
const D3DDECLTYPE_FLOAT4: u8 = 3;
const D3DDECLTYPE_D3DCOLOR: u8 = 4;
const D3DDECLTYPE_UBYTE4: u8 = 5;

const D3DDECLUSAGE_POSITION: u8 = 0;
const D3DDECLUSAGE_BLENDWEIGHT: u8 = 1;
const D3DDECLUSAGE_BLENDINDICES: u8 = 2;
const D3DDECLUSAGE_NORMAL: u8 = 3;
const D3DDECLUSAGE_PSIZE: u8 = 4;
const D3DDECLUSAGE_TEXCOORD: u8 = 5;
const D3DDECLUSAGE_POSITIONT: u8 = 9;
const D3DDECLUSAGE_COLOR: u8 = 10;

pub const VS_OPCODES: &[(u32, u16, u16, u16)] = &[
    (0x0000, 0, 0, 0),
    (0x0001, 2, 0, 0),
    (0x002e, 2, 512, 65535),
    (0x0002, 3, 0, 0),
    (0x0003, 3, 0, 0),
    (0x0004, 4, 0, 0),
    (0x0005, 3, 0, 0),
    (0x0006, 2, 0, 0),
    (0x0007, 2, 0, 0),
    (0x0008, 3, 0, 0),
    (0x0009, 3, 0, 0),
    (0x000a, 3, 0, 0),
    (0x000b, 3, 0, 0),
    (0x000c, 3, 0, 0),
    (0x000d, 3, 0, 0),
    (0x0023, 2, 0, 0),
    (0x000e, 2, 0, 0),
    (0x000f, 2, 0, 0),
    (0x004e, 2, 0, 0),
    (0x004f, 2, 0, 0),
    (0x0010, 2, 0, 0),
    (0x0011, 3, 0, 0),
    (0x0012, 4, 0, 0),
    (0x0013, 2, 0, 0),
    (0x0020, 3, 0, 0),
    (0x0021, 3, 0, 0),
    (0x0022, 4, 512, 513),
    (0x0022, 2, 768, 65535),
    (0x0024, 2, 0, 0),
    (0x0025, 4, 512, 513),
    (0x0025, 2, 768, 65535),
    (0x0014, 3, 0, 0),
    (0x0015, 3, 0, 0),
    (0x0016, 3, 0, 0),
    (0x0017, 3, 0, 0),
    (0x0018, 3, 0, 0),
    (0x001f, 2, 0, 0),
    (0x0051, 5, 0, 0),
    (0x002f, 2, 0, 0),
    (0x0030, 5, 0, 0),
    (0x0026, 1, 512, 65535),
    (0x0027, 0, 512, 65535),
    (0x0028, 1, 512, 65535),
    (0x0029, 2, 513, 65535),
    (0x002a, 0, 512, 65535),
    (0x002b, 0, 512, 65535),
    (0x002c, 0, 513, 65535),
    (0x002d, 2, 513, 65535),
    (0x0060, 1, 0, 0),
    (0x0019, 1, 512, 65535),
    (0x001a, 2, 512, 65535),
    (0x001b, 2, 512, 65535),
    (0x001c, 0, 512, 65535),
    (0x001d, 0, 512, 65535),
    (0x001e, 1, 512, 65535),
    (0x005e, 3, 0, 0),
    (0x005f, 3, 768, 65535),
];

pub const PS_OPCODES: &[(u32, u16, u16, u16)] = &[
    (0x0000, 0, 0, 0),
    (0x0001, 2, 0, 0),
    (0x0002, 3, 0, 0),
    (0x0003, 3, 0, 0),
    (0x0004, 4, 0, 0),
    (0x0005, 3, 0, 0),
    (0x0006, 2, 0, 0),
    (0x0007, 2, 0, 0),
    (0x0008, 3, 0, 0),
    (0x0009, 3, 0, 0),
    (0x000a, 3, 0, 0),
    (0x000b, 3, 0, 0),
    (0x000c, 3, 0, 0),
    (0x000d, 3, 0, 0),
    (0x0023, 2, 0, 0),
    (0x000e, 2, 0, 0),
    (0x000f, 2, 0, 0),
    (0x004e, 2, 0, 0),
    (0x004f, 2, 0, 0),
    (0x0011, 3, 0, 0),
    (0x0012, 4, 0, 0),
    (0x0013, 2, 0, 0),
    (0x0050, 4, 256, 260),
    (0x0058, 4, 258, 768),
    (0x0020, 3, 0, 0),
    (0x0021, 3, 0, 0),
    (0x0024, 2, 0, 0),
    (0x0025, 4, 512, 513),
    (0x0025, 2, 768, 65535),
    (0x005a, 4, 512, 65535),
    (0x0014, 3, 0, 0),
    (0x0015, 3, 0, 0),
    (0x0016, 3, 0, 0),
    (0x0017, 3, 0, 0),
    (0x0018, 3, 0, 0),
    (0x001f, 2, 0, 0),
    (0x0026, 1, 513, 65535),
    (0x0027, 0, 513, 65535),
    (0x0028, 1, 513, 65535),
    (0x0029, 2, 513, 65535),
    (0x002a, 0, 513, 65535),
    (0x002b, 0, 513, 65535),
    (0x002c, 0, 513, 65535),
    (0x002d, 2, 513, 65535),
    (0x0060, 1, 0, 0),
    (0x0019, 1, 513, 65535),
    (0x001a, 2, 513, 65535),
    (0x001b, 2, 768, 65535),
    (0x001c, 0, 513, 65535),
    (0x001d, 0, 768, 65535),
    (0x001e, 1, 513, 65535),
    (0x0051, 5, 0, 0),
    (0x002f, 2, 0, 0),
    (0x0030, 5, 0, 0),
    (0x0040, 1, 0, 259),
    (0x0040, 2, 260, 260),
    (0x0041, 1, 256, 768),
    (0x0042, 1, 0, 259),
    (0x0042, 2, 260, 260),
    (0x0042, 3, 512, 65535),
    (0x0043, 2, 0, 259),
    (0x0044, 2, 256, 259),
    (0x0045, 2, 256, 259),
    (0x0046, 2, 256, 259),
    (0x0052, 2, 258, 259),
    (0x0047, 2, 256, 259),
    (0x0048, 2, 256, 259),
    (0x0049, 2, 256, 259),
    (0x004b, 2, 0, 0),
    (0x004c, 3, 256, 259),
    (0x004d, 2, 256, 259),
    (0x004a, 2, 256, 259),
    (0x0053, 2, 258, 259),
    (0x0054, 2, 259, 259),
    (0x0055, 2, 258, 259),
    (0x0056, 2, 258, 259),
    (0x0057, 1, 260, 260),
    (0x0059, 3, 260, 260),
    (0x005b, 2, 513, 65535),
    (0x005c, 2, 513, 65535),
    (0x005d, 5, 513, 65535),
    (0x005e, 3, 0, 0),
    (0x005f, 3, 768, 65535),
    (0xfffd, 0, 0, 0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexElement {
    pub stream: u16,
    pub offset: u16,
    pub element_type: u8,
    pub method: u8,
    pub usage: u8,
    pub usage_index: u8,
}

pub struct ShaderRecord {
    pub pixel: bool,
    pub code: Vec<u32>,
    pub program: shader_wgsl::Program,
    pub wgsl: String,
}

pub struct D3d9Objects {
    pub shaders: BTreeMap<u32, ShaderRecord>,
    pub next_shader: u32,
    pub declarations: BTreeMap<u32, Vec<VertexElement>>,
    pub next_declaration: u32,
    pub fvf_cache: BTreeMap<u32, u32>,
}

impl Default for D3d9Objects {
    fn default() -> Self {
        Self {
            shaders: BTreeMap::new(),
            next_shader: 1,
            declarations: BTreeMap::new(),
            next_declaration: 1,
            fvf_cache: BTreeMap::new(),
        }
    }
}

impl D3d9Objects {
    pub fn add_shader(&mut self, pixel: bool, code: &[u32]) -> Result<u32, RenderError> {
        let count = shader_word_count(code, pixel)?;
        if self.next_shader == u32::MAX {
            return Err(RenderError::out_of_memory(
                "CreateShader",
                "shader handle range exhausted",
            ));
        }
        let program = shader_wgsl::Program::decode(&code[..count], pixel)?;
        let wgsl = shader_wgsl::translate(&program, &shader_wgsl::Variant::default())?;
        shader_wgsl::validate(&wgsl)?;
        let handle = self.next_shader;
        self.next_shader += 1;
        self.shaders.insert(
            handle,
            ShaderRecord {
                pixel,
                code: code[..count].to_vec(),
                program,
                wgsl,
            },
        );
        Ok(handle)
    }

    pub fn destroy_shader(&mut self, pixel: bool, handle: u32) -> Result<(), RenderError> {
        match self.shaders.get(&handle) {
            Some(record) if record.pixel == pixel => {
                self.shaders.remove(&handle);
                Ok(())
            }
            _ => Err(invalid("DeleteShader", "invalid shader handle")),
        }
    }

    pub fn shader(&self, pixel: bool, handle: u32) -> Result<&ShaderRecord, RenderError> {
        match self.shaders.get(&handle) {
            Some(record) if record.pixel == pixel => Ok(record),
            _ => Err(invalid("GetShaderFunction", "invalid shader handle")),
        }
    }

    pub fn add_declaration(&mut self, elements: &[VertexElement]) -> Result<u32, RenderError> {
        validate_declaration(elements)?;
        if self.next_declaration == u32::MAX {
            return Err(RenderError::out_of_memory(
                "CreateVertexDeclaration",
                "declaration handle range exhausted",
            ));
        }
        let handle = self.next_declaration;
        self.next_declaration += 1;
        self.declarations.insert(handle, elements.to_vec());
        Ok(handle)
    }

    pub fn destroy_declaration(&mut self, handle: u32) -> Result<(), RenderError> {
        self.declarations
            .remove(&handle)
            .map(|_| ())
            .ok_or_else(|| invalid("DeleteDeclaration", "invalid declaration handle"))?;
        self.fvf_cache.retain(|_, cached| *cached != handle);
        Ok(())
    }

    pub fn add_fvf_declaration(
        &mut self,
        fvf: u32,
        elements: &[VertexElement],
    ) -> Result<u32, RenderError> {
        if let Some(handle) = self.fvf_cache.get(&fvf) {
            return Ok(*handle);
        }
        let handle = self.add_declaration(elements)?;
        self.fvf_cache.insert(fvf, handle);
        Ok(handle)
    }

    pub fn declaration(&self, handle: u32) -> Result<&[VertexElement], RenderError> {
        self.declarations
            .get(&handle)
            .map(Vec::as_slice)
            .ok_or_else(|| invalid("GetDeclaration", "invalid declaration handle"))
    }
}

fn invalid(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::invalid(op, why)
}

fn unsupported(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::new(op, why)
}

fn opcode_params(op: u32, pixel: bool, version: u32) -> Option<u16> {
    let table = if pixel { PS_OPCODES } else { VS_OPCODES };
    for &(candidate, params, min, max) in table {
        if candidate == op && version >= u32::from(min) && (max == 0 || version <= u32::from(max)) {
            return Some(params);
        }
    }
    None
}

fn register_operand_count(op: u32, params: u16) -> u16 {
    match op {
        0x1f => 0,
        0x51 | 0x30 | 0x2f => 1,
        _ => params,
    }
}

fn version_supported(pixel: bool, major: u32, minor: u32) -> bool {
    if pixel {
        (major == 1 && (1..=4).contains(&minor))
            || (major == 2 && minor <= 1)
            || (major == 3 && minor == 0)
    } else {
        (major == 1 && minor == 1) || (major == 2 && minor <= 1) || (major == 3 && minor == 0)
    }
}

pub fn shader_word_count(words: &[u32], pixel: bool) -> Result<usize, RenderError> {
    if words.is_empty() {
        return Err(invalid("CreateShader", "shader bytecode is empty"));
    }
    let version_token = words[0];
    let high = version_token >> 16;
    if high != 0xffff && high != 0xfffe {
        return Err(invalid(
            "CreateShader",
            "shader bytecode does not start with a version token",
        ));
    }
    if (high == 0xffff) != pixel {
        return Err(invalid(
            "CreateShader",
            "shader version token does not match the shader stage",
        ));
    }
    let major = (version_token >> 8) & 0xff;
    let minor = version_token & 0xff;
    if !version_supported(pixel, major, minor) {
        let stage = if pixel { "ps" } else { "vs" };
        return Err(unsupported(
            "CreateShader",
            format!(
                "shader model {stage}_{major}_{minor} (raw token {version_token:#010x}) is not implemented"
            ),
        ));
    }
    let version = major * 256 + minor;
    let mut index = 1usize;
    let mut phase_seen = false;
    while index < words.len() {
        let token = words[index];
        let op = token & 0xffff;
        if op == OP_END {
            if token != OP_END {
                return Err(invalid(
                    "CreateShader",
                    "END token is not the canonical 0x0000FFFF",
                ));
            }
            return Ok(index + 1);
        }
        if op == OP_COMMENT {
            let extra = ((token >> 16) & 0x7fff) as usize;
            index += 1 + extra;
        } else if op == OP_PHASE {
            if token != OP_PHASE {
                return Err(invalid(
                    "CreateShader",
                    "PHASE token carries unexpected bits",
                ));
            }
            if !pixel || major != 1 || minor != 4 {
                return Err(unsupported(
                    "CreateShader",
                    "PHASE is only legal in pixel shader model 1.4",
                ));
            }
            if phase_seen {
                return Err(invalid(
                    "CreateShader",
                    "pixel shader 1.x contains more than one PHASE marker",
                ));
            }
            if index + 1 >= words.len() {
                return Err(invalid(
                    "CreateShader",
                    "PHASE marker is not followed by a second-phase instruction",
                ));
            }
            phase_seen = true;
            index += 1;
        } else {
            let Some(params) = opcode_params(op, pixel, version) else {
                return Err(unsupported(
                    "CreateShader",
                    format!(
                        "opcode {op} is not valid for {}{major}.{minor}",
                        if pixel { "ps_" } else { "vs_" }
                    ),
                ));
            };
            if major >= 2 {
                let declared = ((token >> 24) & 0xf) as usize;
                let registers = register_operand_count(op, params);
                let mut expected = 0usize;
                let mut at = index + 1;
                for operand in 0..params {
                    if at >= words.len() {
                        return Err(invalid(
                            "CreateShader",
                            "instruction operands extend past the staged bytecode",
                        ));
                    }
                    expected += 1;
                    if operand < registers && (words[at] & 0x2000) != 0 {
                        expected += 1;
                        at += 1;
                    }
                    at += 1;
                }
                if expected != declared {
                    return Err(invalid(
                        "CreateShader",
                        format!(
                            "instruction length {declared} does not match its operand count {expected}"
                        ),
                    ));
                }
                index += 1 + declared;
            } else {
                index += 1 + usize::from(params);
            }
        }
        if index > words.len() {
            return Err(invalid(
                "CreateShader",
                "shader instruction extends past the end of the staged bytecode",
            ));
        }
    }
    Err(invalid(
        "CreateShader",
        "shader bytecode has no END token within the staged window",
    ))
}

fn element_size(element_type: u8) -> Option<u32> {
    Some(match element_type {
        0 => 4,
        1 => 8,
        2 => 12,
        3 => 16,
        4 => 4,
        5 => 4,
        6 => 4,
        7 => 8,
        8 => 4,
        9 => 4,
        10 => 8,
        11 => 4,
        12 => 8,
        13 | 14 => 4,
        15 => 4,
        16 => 8,
        _ => return None,
    })
}

fn element_alignment(_element_type: u8) -> u32 {
    4
}

pub fn validate_declaration(elements: &[VertexElement]) -> Result<(), RenderError> {
    if elements.is_empty() || elements.len() > MAX_DECL_ELEMENTS + 1 {
        return Err(invalid(
            "CreateVertexDeclaration",
            "declaration must contain between one and 64 elements",
        ));
    }
    let end = elements[elements.len() - 1];
    if end.stream != D3DDECL_END_STREAM
        || end.offset != 0
        || end.element_type != D3DDECLTYPE_UNUSED
        || end.method != 0
        || end.usage != 0
        || end.usage_index != 0
    {
        return Err(invalid(
            "CreateVertexDeclaration",
            "declaration is not terminated by D3DDECL_END",
        ));
    }
    let mut ranges: [Vec<(u32, u32)>; 16] = std::array::from_fn(|_| Vec::new());
    let mut seen: Vec<(u8, u8)> = Vec::new();
    for element in &elements[..elements.len() - 1] {
        if element.stream >= 16 {
            return Err(invalid(
                "CreateVertexDeclaration",
                format!("stream {} is out of range", element.stream),
            ));
        }
        if element.element_type > D3DDECLTYPE_UNUSED {
            return Err(invalid(
                "CreateVertexDeclaration",
                format!(
                    "vertex element type {} is out of range",
                    element.element_type
                ),
            ));
        }
        if element.usage > D3DDECLUSAGE_SAMPLE {
            return Err(invalid(
                "CreateVertexDeclaration",
                format!("vertex element usage {} is out of range", element.usage),
            ));
        }
        if element.usage_index > 15 {
            return Err(invalid(
                "CreateVertexDeclaration",
                format!(
                    "vertex element usage index {} is out of range",
                    element.usage_index
                ),
            ));
        }
        if element.method > D3DDECLMETHOD_LOOKUPPRESAMPLED {
            return Err(invalid(
                "CreateVertexDeclaration",
                format!("vertex element method {} is out of range", element.method),
            ));
        }
        if element.method != 0 {
            return Err(unsupported(
                "CreateVertexDeclaration",
                format!(
                    "vertex element method {} is not implemented",
                    element.method
                ),
            ));
        }
        if element.element_type == D3DDECLTYPE_UNUSED {
            return Err(invalid(
                "CreateVertexDeclaration",
                "D3DDECLTYPE_UNUSED is only valid for the D3DDECL_END marker",
            ));
        }
        let size = element_size(element.element_type).ok_or_else(|| {
            invalid(
                "CreateVertexDeclaration",
                format!(
                    "vertex element type {} is not implemented",
                    element.element_type
                ),
            )
        })?;
        let alignment = element_alignment(element.element_type);
        if u32::from(element.offset) % alignment != 0 {
            return Err(invalid(
                "CreateVertexDeclaration",
                format!(
                    "vertex element offset {} is not aligned to {} bytes",
                    element.offset, alignment
                ),
            ));
        }
        let start = u32::from(element.offset);
        let end = start + size;
        let stream = usize::from(element.stream);
        for (lo, hi) in &ranges[stream] {
            if start < *hi && *lo < end {
                return Err(invalid(
                    "CreateVertexDeclaration",
                    format!(
                        "vertex elements overlap on stream {} at byte {start}",
                        element.stream
                    ),
                ));
            }
        }
        ranges[stream].push((start, end));
        let key = (element.usage, element.usage_index);
        if seen.contains(&key) {
            return Err(invalid(
                "CreateVertexDeclaration",
                format!(
                    "duplicate usage {} index {}",
                    element.usage, element.usage_index
                ),
            ));
        }
        seen.push(key);
    }
    Ok(())
}

fn push_element(
    out: &mut Vec<VertexElement>,
    offset: &mut u16,
    element_type: u8,
    usage: u8,
    usage_index: u8,
) {
    out.push(VertexElement {
        stream: 0,
        offset: *offset,
        element_type,
        method: 0,
        usage,
        usage_index,
    });
    *offset += element_size(element_type).unwrap_or(0) as u16;
}

pub fn fvf_declaration(fvf: u32) -> Result<Vec<VertexElement>, RenderError> {
    if fvf & D3DFVF_RESERVED0 != 0 {
        return Err(invalid(
            "SetFVF",
            format!("FVF {fvf:#010x} sets the reserved bit D3DFVF_RESERVED0"),
        ));
    }
    if fvf & (D3DFVF_RESERVED2 & !D3DFVF_XYZW) != 0 {
        return Err(invalid(
            "SetFVF",
            format!("FVF {fvf:#010x} sets a reserved D3DFVF_RESERVED2 bit"),
        ));
    }
    let position = fvf & D3DFVF_POSITION_MASK;
    if position == D3DFVF_XYZRHW && fvf & D3DFVF_NORMAL != 0 {
        return Err(invalid(
            "SetFVF",
            "D3DFVF_NORMAL cannot be combined with D3DFVF_XYZRHW",
        ));
    }
    let blend_count = match position {
        D3DFVF_XYZ | D3DFVF_XYZRHW | D3DFVF_XYZW => 0,
        D3DFVF_XYZB1..=D3DFVF_XYZB5 => (position >> 1) - 2,
        _ => {
            return Err(invalid(
                "SetFVF",
                format!(
                    "FVF position mask {position:#06x} is not a known D3DFVF_POSITION_MASK value"
                ),
            ));
        }
    };
    if fvf & (D3DFVF_LASTBETA_UBYTE4 | D3DFVF_LASTBETA_D3DCOLOR) != 0 && blend_count == 0 {
        return Err(invalid(
            "SetFVF",
            "FVF last-beta flags require XYZB vertex blending",
        ));
    }
    let mut out = Vec::new();
    let mut offset = 0u16;
    match position {
        D3DFVF_XYZ => push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_FLOAT3,
            D3DDECLUSAGE_POSITION,
            0,
        ),
        D3DFVF_XYZW => push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_FLOAT4,
            D3DDECLUSAGE_POSITION,
            0,
        ),
        D3DFVF_XYZRHW => push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_FLOAT4,
            D3DDECLUSAGE_POSITIONT,
            0,
        ),
        _ => push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_FLOAT3,
            D3DDECLUSAGE_POSITION,
            0,
        ),
    }
    let (weight_floats, indices) = if fvf & D3DFVF_LASTBETA_UBYTE4 != 0 {
        (blend_count - 1, Some(D3DDECLTYPE_UBYTE4))
    } else if fvf & D3DFVF_LASTBETA_D3DCOLOR != 0 {
        (blend_count - 1, Some(D3DDECLTYPE_D3DCOLOR))
    } else if position == D3DFVF_XYZB5 {
        (4, Some(D3DDECLTYPE_FLOAT1))
    } else {
        (blend_count, None)
    };
    let mut remaining = weight_floats;
    let mut usage_index = 0u8;
    while remaining > 0 {
        let take = remaining.min(4);
        let element_type = match take {
            1 => D3DDECLTYPE_FLOAT1,
            2 => D3DDECLTYPE_FLOAT2,
            3 => D3DDECLTYPE_FLOAT3,
            _ => D3DDECLTYPE_FLOAT4,
        };
        out.push(VertexElement {
            stream: 0,
            offset,
            element_type,
            method: 0,
            usage: D3DDECLUSAGE_BLENDWEIGHT,
            usage_index,
        });
        offset += (take * 4) as u16;
        remaining -= take;
        usage_index += 1;
    }
    if let Some(element_type) = indices {
        out.push(VertexElement {
            stream: 0,
            offset,
            element_type,
            method: 0,
            usage: D3DDECLUSAGE_BLENDINDICES,
            usage_index: 0,
        });
        offset += 4;
    }
    if fvf & D3DFVF_NORMAL != 0 {
        push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_FLOAT3,
            D3DDECLUSAGE_NORMAL,
            0,
        );
    }
    if fvf & D3DFVF_PSIZE != 0 {
        push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_FLOAT1,
            D3DDECLUSAGE_PSIZE,
            0,
        );
    }
    if fvf & D3DFVF_DIFFUSE != 0 {
        push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_D3DCOLOR,
            D3DDECLUSAGE_COLOR,
            0,
        );
    }
    if fvf & D3DFVF_SPECULAR != 0 {
        push_element(
            &mut out,
            &mut offset,
            D3DDECLTYPE_D3DCOLOR,
            D3DDECLUSAGE_COLOR,
            1,
        );
    }
    let texture_count = (fvf & D3DFVF_TEXCOUNT_MASK) >> D3DFVF_TEXCOUNT_SHIFT;
    if texture_count > 8 {
        return Err(invalid(
            "SetFVF",
            format!("FVF texture coordinate count {texture_count} is out of range"),
        ));
    }
    for index in 0..texture_count {
        let size_bits = (fvf >> (16 + index * 2)) & 0x3;
        let (element_type, size) = match size_bits {
            0 => (D3DDECLTYPE_FLOAT2, 8),
            1 => (D3DDECLTYPE_FLOAT3, 12),
            2 => (D3DDECLTYPE_FLOAT4, 16),
            _ => (D3DDECLTYPE_FLOAT1, 4),
        };
        out.push(VertexElement {
            stream: 0,
            offset,
            element_type,
            method: 0,
            usage: D3DDECLUSAGE_TEXCOORD,
            usage_index: index as u8,
        });
        offset += size;
    }
    out.push(VertexElement {
        stream: D3DDECL_END_STREAM,
        offset: 0,
        element_type: D3DDECLTYPE_UNUSED,
        method: 0,
        usage: 0,
        usage_index: 0,
    });
    validate_declaration(&out)?;
    Ok(out)
}
