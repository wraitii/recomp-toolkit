use super::wgsl as shader_wgsl;
use crate::RenderError;
pub use crate::shader::validate::{PS_OPCODES, VS_OPCODES, shader_word_count};
use std::collections::BTreeMap;

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
