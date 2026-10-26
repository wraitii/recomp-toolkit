use super::gpu::{D3d9Gpu, PendingClear};
use super::query::D3d9Queries;
use super::shader::VertexElement;
use super::state::{D3d9State, MAX_ANISOTROPY, MAX_SAMPLERS};
use super::{SurfaceView, TargetRect, wgsl};
use crate::RenderError;
use crate::fast_map::{FastHasher, FastMap};
use crate::render::bindings::{DrawParameters, uniform_entry};
use crate::render::cache::LruCache;
use crate::render::interface::{InterfaceKey, LayoutAttribute, VertexAttributes, VertexLayouts};
use crate::render::pipeline::{
    CompiledPipeline, PipelineDescriptor, PipelineRecipe, PipelineRecipeKey,
    PreparedPipeline as PipelineKey,
};
use crate::shader::identity::ProgramKey;
use crate::shader::interface::{
    self as shader_interface, SAMPLER_BINDING, TEXTURE_BINDING, VARYINGS as V2F,
};
use crate::shader::recipe::{ProgramSettings, ShaderRecipe, shading_source};
use smallvec::{SmallVec, smallvec};
use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};
use std::time::Instant;

mod ffvs;
mod geometry;
pub(crate) use geometry::vertex_element_bytes;
use geometry::*;
mod trace;
pub(crate) mod uber;

fn trace_compilation() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RECOMP_D3D9_TRACE_PIPELINES").is_ok_and(|value| value == "1")
    })
}

fn fingerprint(value: &impl Hash) -> u64 {
    let mut hasher = FastHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

fn warn_cache_eviction(name: &str, evicted: usize) {
    if evicted == 0 {
        return;
    }
    eprintln!(
        "[d3d9-cache] WARNING eviction cache={name} evicted={evicted} limit_entries=512 limit_bytes=33554432; may require recompilation"
    );
}

fn evict_one<K: Clone + Eq + Hash, V>(cache: &mut FastMap<K, V>) {
    if let Some(key) = cache.keys().next().cloned() {
        cache.remove(&key);
    }
}

fn trace_selected(frame: u64, frame_draw: u32, total_draw: u64) -> bool {
    static WINDOW: std::sync::OnceLock<Option<(u64, u64)>> = std::sync::OnceLock::new();
    let window = WINDOW.get_or_init(|| {
        let start = std::env::var("RECOMP_D3D9_TRACE_FRAME_START")
            .ok()?
            .parse::<u64>()
            .ok()?;
        let count = std::env::var("RECOMP_D3D9_TRACE_FRAME_COUNT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2);
        Some((start, start.saturating_add(count)))
    });
    if let Some((start, end)) = window {
        frame >= *start && frame < *end && frame_draw < super::trace_draw_limit()
    } else {
        total_draw < u64::from(super::trace_draw_limit())
    }
}

fn trace_readback() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RECOMP_D3D9_TRACE_READBACK").is_ok_and(|v| v == "1"))
}

pub const D3DPT_POINTLIST: u32 = 1;
pub const D3DPT_LINELIST: u32 = 2;
pub const D3DPT_LINESTRIP: u32 = 3;
pub const D3DPT_TRIANGLELIST: u32 = 4;
pub const D3DPT_TRIANGLESTRIP: u32 = 5;
pub const D3DPT_TRIANGLEFAN: u32 = 6;

const D3DDECLTYPE_FLOAT1: u8 = 0;
const D3DDECLTYPE_FLOAT2: u8 = 1;
const D3DDECLTYPE_FLOAT3: u8 = 2;
const D3DDECLTYPE_FLOAT4: u8 = 3;
const D3DDECLTYPE_D3DCOLOR: u8 = 4;
const D3DDECLTYPE_UBYTE4: u8 = 5;
const D3DDECLTYPE_SHORT2: u8 = 6;
const D3DDECLTYPE_SHORT4: u8 = 7;
const D3DDECLTYPE_UBYTE4N: u8 = 8;
const D3DDECLTYPE_SHORT2N: u8 = 9;
const D3DDECLTYPE_SHORT4N: u8 = 10;
const D3DDECLTYPE_USHORT2N: u8 = 11;
const D3DDECLTYPE_USHORT4N: u8 = 12;
const D3DDECLTYPE_UDEC3: u8 = 13;
const D3DDECLTYPE_DEC3N: u8 = 14;
const D3DDECLTYPE_FLOAT16_2: u8 = 15;
const D3DDECLTYPE_FLOAT16_4: u8 = 16;
const D3DDECLTYPE_UNUSED: u8 = 17;

fn invalid(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::invalid(op, why)
}

fn unsupported(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::new(op, why)
}

fn clamp_signed(value: f32, _bits: u32) -> f32 {
    value.max(-1.0)
}

pub(crate) fn element_size(element_type: u8) -> Option<u32> {
    Some(match element_type {
        D3DDECLTYPE_FLOAT1 => 4,
        D3DDECLTYPE_FLOAT2 => 8,
        D3DDECLTYPE_FLOAT3 => 12,
        D3DDECLTYPE_FLOAT4 => 16,
        D3DDECLTYPE_D3DCOLOR | D3DDECLTYPE_UBYTE4 | D3DDECLTYPE_UBYTE4N | D3DDECLTYPE_DEC3N
        | D3DDECLTYPE_UDEC3 => 4,
        D3DDECLTYPE_SHORT2 | D3DDECLTYPE_SHORT2N | D3DDECLTYPE_USHORT2N | D3DDECLTYPE_FLOAT16_2 => {
            4
        }
        D3DDECLTYPE_SHORT4 | D3DDECLTYPE_SHORT4N | D3DDECLTYPE_USHORT4N | D3DDECLTYPE_FLOAT16_4 => {
            8
        }
        _ => return None,
    })
}

fn read_f32(bytes: &[u8]) -> f32 {
    f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_f16(bytes: &[u8]) -> f32 {
    half_to_f32(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = (bits >> 15) as u32;
    let exp = ((bits >> 10) & 0x1f) as u32;
    let frac = (bits & 0x3ff) as u32;
    let value = if exp == 0 {
        if frac == 0 {
            0.0
        } else {
            (frac as f32) * 2.0f32.powi(-24)
        }
    } else if exp == 0x1f {
        if frac == 0 { f32::INFINITY } else { f32::NAN }
    } else {
        (1.0 + frac as f32 / 1024.0) * 2.0f32.powi(exp as i32 - 15)
    };
    if sign != 0 { -value } else { value }
}

pub(crate) fn expand_element(element_type: u8, bytes: &[u8]) -> Result<[f32; 4], RenderError> {
    let mut out = [0.0f32, 0.0, 0.0, 1.0];
    match element_type {
        D3DDECLTYPE_FLOAT1 => out[0] = read_f32(bytes),
        D3DDECLTYPE_FLOAT2 => {
            for i in 0..2 {
                out[i] = read_f32(&bytes[i * 4..]);
            }
        }
        D3DDECLTYPE_FLOAT3 => {
            for i in 0..3 {
                out[i] = read_f32(&bytes[i * 4..]);
            }
        }
        D3DDECLTYPE_FLOAT4 => {
            for (i, v) in out.iter_mut().enumerate() {
                *v = read_f32(&bytes[i * 4..]);
            }
        }
        D3DDECLTYPE_D3DCOLOR => {
            let argb = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            out[0] = ((argb >> 16) & 0xff) as f32 / 255.0;
            out[1] = ((argb >> 8) & 0xff) as f32 / 255.0;
            out[2] = (argb & 0xff) as f32 / 255.0;
            out[3] = ((argb >> 24) & 0xff) as f32 / 255.0;
        }
        D3DDECLTYPE_UBYTE4 | D3DDECLTYPE_UBYTE4N => {
            for i in 0..4 {
                let v = bytes[i] as f32;
                out[i] = if element_type == D3DDECLTYPE_UBYTE4N {
                    v / 255.0
                } else {
                    v
                };
            }
        }
        D3DDECLTYPE_SHORT2 | D3DDECLTYPE_SHORT2N | D3DDECLTYPE_USHORT2N => {
            for i in 0..2 {
                let raw = u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                out[i] = match element_type {
                    D3DDECLTYPE_SHORT2 => raw as i16 as f32,
                    D3DDECLTYPE_SHORT2N => clamp_signed(raw as i16 as f32 / 32767.0, 16),
                    _ => raw as f32 / 65535.0,
                };
            }
        }
        D3DDECLTYPE_SHORT4 | D3DDECLTYPE_SHORT4N | D3DDECLTYPE_USHORT4N => {
            for i in 0..4 {
                let raw = u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                out[i] = match element_type {
                    D3DDECLTYPE_SHORT4 => raw as i16 as f32,
                    D3DDECLTYPE_SHORT4N => clamp_signed(raw as i16 as f32 / 32767.0, 16),
                    _ => raw as f32 / 65535.0,
                };
            }
        }
        D3DDECLTYPE_UDEC3 | D3DDECLTYPE_DEC3N => {
            let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let x = raw & 0x3ff;
            let y = (raw >> 10) & 0x3ff;
            let z = (raw >> 20) & 0x3ff;
            let norm = element_type == D3DDECLTYPE_DEC3N;
            for (i, v) in [x, y, z].into_iter().enumerate() {
                out[i] = if norm {
                    let signed = if v & 0x200 != 0 {
                        v as i32 - 0x400
                    } else {
                        v as i32
                    };
                    clamp_signed(signed as f32 / 511.0, 11)
                } else {
                    v as f32
                };
            }
        }
        D3DDECLTYPE_FLOAT16_2 | D3DDECLTYPE_FLOAT16_4 => {
            let count = if element_type == D3DDECLTYPE_FLOAT16_2 {
                2
            } else {
                4
            };
            for i in 0..count {
                out[i] = read_f16(&bytes[i * 2..]);
            }
        }
        _ => {
            return Err(unsupported(
                "Draw",
                format!("vertex element type {element_type} is not convertible"),
            ));
        }
    }
    Ok(out)
}

pub(crate) type BoundTextures = SmallVec<[BoundTexture; 4]>;
pub(crate) type Streams = SmallVec<[StreamData; 2]>;

#[derive(Clone, PartialEq)]
pub(crate) struct BoundTexture {
    pub stage: u32,
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
    pub depth: bool,
    pub cube: bool,
    pub volume: bool,
    pub format: u32,
    pub lod: u32,
    pub levels: u32,
}

pub(crate) struct StreamData {
    pub stream: u32,
    pub stride: u32,
    pub offset: u32,
    pub bytes: Arc<Vec<u8>>,
}

pub(crate) struct DrawCommand {
    pub primitive: u32,
    pub primitive_count: u32,
    pub start_vertex: u32,
    pub base_vertex: i32,
    pub indexed: bool,
    pub index_format: u32,
    pub index_bytes: Arc<Vec<u8>>,
    pub min_vertex: u32,
    pub num_vertices: u32,
    pub streams: Streams,
}

pub(crate) struct DrawSnapshot {
    pub command: DrawCommand,
    pub target: SurfaceView,
    pub depth: Option<SurfaceView>,
    pub textures: BoundTextures,
}

struct LinkedInput {
    location: u32,
    stream: u16,
    offset: u16,
    element_type: u8,
}

fn link_inputs(
    decl: &[VertexElement],
    program: &wgsl::Program,
    linked: &mut Vec<LinkedInput>,
) -> Result<(), RenderError> {
    linked.clear();
    for input in &program.inputs {
        let element = decl
            .iter()
            .find(|e| e.usage as u32 == input.usage && e.usage_index as u32 == input.usage_index);
        let Some(element) = element else {
            return Err(unsupported(
                "Draw",
                format!(
                    "shader input {}/{} has no matching vertex declaration element",
                    input.usage, input.usage_index
                ),
            ));
        };
        if element.element_type == D3DDECLTYPE_UNUSED {
            return Err(unsupported("Draw", "vertex declaration element is unused"));
        }
        if element_size(element.element_type).is_none() {
            return Err(unsupported("Draw", "unsupported vertex element type"));
        }
        linked.push(LinkedInput {
            location: input.index,
            stream: element.stream,
            offset: element.offset,
            element_type: element.element_type,
        });
    }
    Ok(())
}

pub(crate) fn primitive_vertex_count(primitive: u32, count: u32) -> Result<u32, RenderError> {
    let vertices = match primitive {
        D3DPT_POINTLIST => Some(count),
        D3DPT_LINELIST => count.checked_mul(2),
        D3DPT_LINESTRIP => count.checked_add(1),
        D3DPT_TRIANGLELIST => count.checked_mul(3),
        D3DPT_TRIANGLESTRIP | D3DPT_TRIANGLEFAN => count.checked_add(2),
        _ => {
            return Err(invalid(
                "Draw",
                format!("unknown D3DPRIMITIVETYPE {primitive}"),
            ));
        }
    };
    vertices.ok_or_else(|| invalid("Draw", "primitive vertex count overflow"))
}

fn index_count(index_format: u32, bytes: &[u8]) -> usize {
    let size = if index_format == 101 { 2 } else { 4 };
    bytes.len() / size
}

fn indexed_vertex_index(bytes: &[u8], index_format: u32, index: usize) -> u32 {
    if index_format == 101 {
        let at = index * 2;
        u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u32
    } else {
        let at = index * 4;
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }
}

pub(crate) fn expanded_indices(
    command: &DrawCommand,
    dense_remap: &mut Vec<u32>,
    slots: &mut FastMap<u32, u32>,
) -> Result<(Vec<u32>, Option<Vec<u32>>, u32, wgpu::PrimitiveTopology), RenderError> {
    let count = primitive_vertex_count(command.primitive, command.primitive_count)?;
    let packed_indices = command.indexed && command.primitive == D3DPT_TRIANGLELIST;
    let unique_capacity = if packed_indices {
        count.min(command.num_vertices) as usize
    } else {
        count as usize
    };
    let mut order = Vec::new();
    order
        .try_reserve_exact(unique_capacity)
        .map_err(|_| RenderError::out_of_memory("Draw", "index expansion allocation failed"))?;
    let dense_range = command.num_vertices as usize;
    // Retail D3D9 uses MinVertexIndex/NumVertices as hints; the fetch below is
    // bounded by the actual stream. The dense remap still indexes by the hint
    // range, so it is disabled when the span falls outside it.
    let use_dense_remap = packed_indices
        && dense_range <= 1_048_576
        && dense_range <= (count as usize).saturating_mul(8).max(4096)
        && {
            let start = command.start_vertex as usize;
            let end = start.checked_add(count as usize);
            let available = index_count(command.index_format, &command.index_bytes);
            match end {
                Some(end) if end <= available => (start..end).all(|i| {
                    let raw = i64::from(indexed_vertex_index(
                        &command.index_bytes,
                        command.index_format,
                        i,
                    ));
                    raw >= i64::from(command.min_vertex)
                        && raw < i64::from(command.min_vertex) + i64::from(command.num_vertices)
                }),
                _ => false,
            }
        };
    if use_dense_remap {
        dense_remap
            .try_reserve(dense_range.saturating_sub(dense_remap.len()))
            .map_err(|_| {
                RenderError::out_of_memory("Draw", "dense index remap allocation failed")
            })?;
        dense_remap.resize(dense_range, u32::MAX);
        dense_remap.fill(u32::MAX);
    }
    slots.clear();
    let mut draw_indices = if packed_indices {
        if !use_dense_remap {
            slots.try_reserve(unique_capacity).map_err(|_| {
                RenderError::out_of_memory("Draw", "vertex index map allocation failed")
            })?;
        }
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(count as usize)
            .map_err(|_| RenderError::out_of_memory("Draw", "draw index allocation failed"))?;
        Some(indices)
    } else {
        None
    };
    if command.indexed {
        if !matches!(command.index_format, 101 | 102) {
            return Err(invalid("Draw", "invalid index format"));
        }
        let available = index_count(command.index_format, &command.index_bytes);
        let start = command.start_vertex as usize;
        let end = start
            .checked_add(count as usize)
            .ok_or_else(|| invalid("Draw", "index range overflow"))?;
        if end > available {
            return Err(invalid("Draw", "index range exceeds the index stream"));
        }
        for i in start..end {
            let raw = indexed_vertex_index(&command.index_bytes, command.index_format, i) as i64;
            let absolute = raw + i64::from(command.base_vertex);
            if absolute < 0 || absolute > u32::MAX as i64 {
                return Err(invalid(
                    "Draw",
                    "index plus base vertex is outside the vertex window",
                ));
            }
            let absolute = absolute as u32;
            if let Some(indices) = &mut draw_indices {
                let slot = if use_dense_remap {
                    let slot = &mut dense_remap[(raw as u32 - command.min_vertex) as usize];
                    if *slot == u32::MAX {
                        *slot = u32::try_from(order.len())
                            .map_err(|_| invalid("Draw", "packed vertex count exceeds 32 bits"))?;
                        order.push(absolute);
                    }
                    *slot
                } else {
                    if let Some(&slot) = slots.get(&absolute) {
                        slot
                    } else {
                        let slot = u32::try_from(order.len())
                            .map_err(|_| invalid("Draw", "packed vertex count exceeds 32 bits"))?;
                        order.push(absolute);
                        slots.insert(absolute, slot);
                        slot
                    }
                };
                indices.push(slot);
            } else {
                order.push(absolute);
            }
        }
    } else {
        let start = command.start_vertex;
        for i in 0..count {
            let absolute = u64::from(start) + u64::from(i);
            if absolute > u32::MAX as u64 {
                return Err(invalid("Draw", "vertex range overflow"));
            }
            order.push(absolute as u32);
        }
    }
    let topology = match command.primitive {
        D3DPT_POINTLIST => wgpu::PrimitiveTopology::PointList,
        D3DPT_LINELIST => wgpu::PrimitiveTopology::LineList,
        D3DPT_LINESTRIP => wgpu::PrimitiveTopology::LineStrip,
        D3DPT_TRIANGLELIST => wgpu::PrimitiveTopology::TriangleList,
        D3DPT_TRIANGLESTRIP => wgpu::PrimitiveTopology::TriangleStrip,
        D3DPT_TRIANGLEFAN => {
            let mut triangles = Vec::new();
            if order.len() >= 3 {
                triangles
                    .try_reserve_exact((order.len() - 2) * 3)
                    .map_err(|_| {
                        RenderError::out_of_memory("Draw", "triangle fan allocation failed")
                    })?;
                for i in 1..order.len() - 1 {
                    triangles.extend_from_slice(&[order[0], order[i], order[i + 1]]);
                }
            }
            order = triangles;
            wgpu::PrimitiveTopology::TriangleList
        }
        _ => return Err(invalid("Draw", "unknown D3DPRIMITIVETYPE")),
    };
    Ok((order, draw_indices, count, topology))
}

fn append_f32_le(out: &mut Vec<u8>, values: &[f32]) {
    if cfg!(target_endian = "little") {
        out.extend_from_slice(bytemuck::cast_slice(values));
    } else {
        let start = out.len();
        out.resize(start + values.len() * 4, 0);
        for (destination, value) in out[start..].chunks_exact_mut(4).zip(values) {
            destination.copy_from_slice(&value.to_le_bytes());
        }
    }
}

fn append_u32_le(out: &mut Vec<u8>, values: &[u32]) {
    if cfg!(target_endian = "little") {
        out.extend_from_slice(bytemuck::cast_slice(values));
    } else {
        let start = out.len();
        out.resize(start + values.len() * 4, 0);
        for (destination, value) in out[start..].chunks_exact_mut(4).zip(values) {
            destination.copy_from_slice(&value.to_le_bytes());
        }
    }
}

fn flat_to_u8(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len().saturating_mul(4));
    append_f32_le(&mut out, values);
    out
}

fn multiply_matrices(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    std::array::from_fn(|i| (0..4).map(|k| a[i / 4 * 4 + k] * b[k * 4 + i % 4]).sum())
}

fn transform_point(m: &[f32; 16], v: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|c| (0..4).map(|r| v[r] * m[r * 4 + c]).sum())
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    (0..3).map(|i| a[i] * b[i]).sum()
}
fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let length = dot3(v, v).sqrt();
    if length > 0.0 {
        v.map(|x| x / length)
    } else {
        [0.0; 3]
    }
}
fn color(value: u32) -> [f32; 4] {
    [
        ((value >> 16) & 255) as f32 / 255.0,
        ((value >> 8) & 255) as f32 / 255.0,
        (value & 255) as f32 / 255.0,
        (value >> 24) as f32 / 255.0,
    ]
}
fn rs(state: &D3d9State, id: u32) -> u32 {
    state.render_raw(id)
}
fn tss(state: &D3d9State, stage: u32, id: u32) -> u32 {
    state.texture_stage_raw(stage, id)
}
fn ss(state: &D3d9State, stage: u32, id: u32) -> u32 {
    state.sampler_raw(stage, id)
}
fn inverse3(m: &[f32; 16]) -> Result<[[f32; 3]; 3], RenderError> {
    let (a, b, c, d, e, f, g, h, i) = (m[0], m[1], m[2], m[4], m[5], m[6], m[8], m[9], m[10]);
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det == 0.0 || !det.is_finite() {
        return Err(unsupported(
            "Draw",
            "singular fixed-function normal transform",
        ));
    }
    Ok([
        [e * i - f * h, c * h - b * i, b * f - c * e],
        [f * g - d * i, a * i - c * g, c * d - a * f],
        [d * h - e * g, b * g - a * h, a * e - b * d],
    ]
    .map(|r| r.map(|x| x / det)))
}
struct FfLight {
    kind: u32,
    values: [f32; 26],
    position: [f32; 4],
    direction: [f32; 3],
    spot_inner: f32,
    spot_outer: f32,
}
struct FfFog {
    mode: u32,
    density: f32,
    start: f32,
    end: f32,
}
impl FfFog {
    fn prepare(state: &D3d9State) -> Self {
        Self {
            mode: rs(state, 140),
            density: f32::from_bits(rs(state, 38)),
            start: f32::from_bits(rs(state, 36)),
            end: f32::from_bits(rs(state, 37)),
        }
    }

    fn factor(&self, z: f32) -> f32 {
        match self.mode {
            1 => (-self.density * z).exp(),
            2 => {
                let d = self.density * z;
                (-d * d).exp()
            }
            3 => ((self.end - z) / (self.end - self.start)).clamp(0.0, 1.0),
            _ => 1.0,
        }
    }
}
struct FfDraw {
    lighting: bool,
    local_viewer: bool,
    specular: bool,
    normalize_normals: bool,
    material: [f32; 17],
    diffuse_source: Option<u8>,
    ambient_source: Option<u8>,
    specular_source: Option<u8>,
    emissive_source: Option<u8>,
    global_ambient: [f32; 4],
    fog: FfFog,
}
impl FfDraw {
    fn prepare(state: &D3d9State) -> Self {
        let vertex_color = rs(state, 141) != 0;
        let selector = |id: u32| -> Option<u8> {
            if vertex_color {
                match rs(state, id) {
                    1 => Some(0),
                    2 => Some(1),
                    _ => None,
                }
            } else {
                None
            }
        };
        Self {
            lighting: rs(state, 137) != 0,
            local_viewer: rs(state, 142) != 0,
            specular: rs(state, 29) != 0,
            normalize_normals: rs(state, 143) != 0,
            material: state.material().map(f32::from_bits),
            diffuse_source: selector(145),
            ambient_source: selector(147),
            specular_source: selector(146),
            emissive_source: selector(148),
            global_ambient: color(rs(state, 139)),
            fog: FfFog::prepare(state),
        }
    }

    fn source(
        &self,
        offset: usize,
        selected: Option<u8>,
        vertex: &[Option<[f32; 4]>; 2],
    ) -> [f32; 4] {
        selected
            .and_then(|index| vertex[index as usize])
            .unwrap_or_else(|| {
                self.material[offset..offset + 4]
                    .try_into()
                    .expect("material color")
            })
    }
}
fn ff_colors(
    prep: &FfDraw,
    p: [f32; 4],
    n: [f32; 3],
    vertex: [Option<[f32; 4]>; 2],
    lights: &[FfLight],
) -> Result<[[f32; 4]; 2], RenderError> {
    if !prep.lighting {
        return Ok([vertex[0].unwrap_or([1.0; 4]), vertex[1].unwrap_or([0.0; 4])]);
    }
    let material = &prep.material;
    let diffuse = prep.source(0, prep.diffuse_source, &vertex);
    let ambient = prep.source(4, prep.ambient_source, &vertex);
    let specular = prep.source(8, prep.specular_source, &vertex);
    let emissive = prep.source(12, prep.emissive_source, &vertex);
    let global = prep.global_ambient;
    let mut result: [[f32; 4]; 2] = [
        std::array::from_fn(|i| emissive[i] + ambient[i] * global[i]),
        [0.0; 4],
    ];
    let viewer = if prep.local_viewer {
        normalize3([-p[0], -p[1], -p[2]])
    } else {
        [0.0, 0.0, 1.0]
    };
    for light in lights {
        let (l, mut attenuation) = if light.kind == 3 {
            (light.direction, 1.0)
        } else {
            let delta = [
                light.position[0] - p[0],
                light.position[1] - p[1],
                light.position[2] - p[2],
            ];
            let distance = dot3(delta, delta).sqrt();
            if distance > light.values[19] {
                continue;
            }
            let denominator = light.values[21]
                + light.values[22] * distance
                + light.values[23] * distance * distance;
            if denominator <= 0.0 {
                return Err(unsupported(
                    "Draw",
                    "non-positive light attenuation denominator",
                ));
            }
            (normalize3(delta), 1.0 / denominator)
        };
        if light.kind == 2 {
            let rho = -dot3(l, light.direction);
            let inner = light.spot_inner;
            let outer = light.spot_outer;
            attenuation *= if rho >= inner {
                1.0
            } else if rho <= outer {
                0.0
            } else {
                ((rho - outer) / (inner - outer)).powf(light.values[20])
            };
        }
        let ndotl = dot3(n, l).max(0.0);
        let highlight = if prep.specular && ndotl > 0.0 {
            dot3(n, normalize3(std::array::from_fn(|i| l[i] + viewer[i])))
                .max(0.0)
                .powf(material[16])
        } else {
            0.0
        };
        for i in 0..3 {
            result[0][i] += attenuation
                * (ambient[i] * light.values[9 + i] + diffuse[i] * light.values[1 + i] * ndotl);
            result[1][i] += attenuation * specular[i] * light.values[5 + i] * highlight;
        }
    }
    result[0][3] = diffuse[3];
    Ok(result.map(|v| v.map(|x| x.clamp(0.0, 1.0))))
}

fn ff_vertices(
    command: &DrawCommand,
    decl: &[VertexElement],
    order: &[u32],
    state: &D3d9State,
) -> Result<Vec<u8>, RenderError> {
    for id in [151, 152, 167] {
        if rs(state, id) != 0 {
            return Err(unsupported(
                "Draw",
                format!("fixed-function render state {id} is not implemented"),
            ));
        }
    }
    let find = |usage: u8, index: u8| {
        decl.iter()
            .find(|e| e.usage == usage && e.usage_index == index)
    };
    let position = find(9, 0)
        .or_else(|| find(0, 0))
        .ok_or_else(|| invalid("Draw", "fixed-function declaration has no position"))?;
    let transformed = position.usage == 9;
    let world = state.transform(256)?;
    let view = state.transform(2)?;
    let projection = state.transform(3)?;
    let worldview = multiply_matrices(&world, &view);
    let wvp = multiply_matrices(&worldview, &projection);
    let needs_normal = rs(state, 137) != 0
        || (0..8).any(|stage| matches!(tss(state, stage, 11) & 0xffff0000, 0x10000 | 0x30000));
    let normal_matrix = if !transformed && needs_normal {
        Some(inverse3(&worldview)?)
    } else {
        None
    };
    let mut lights = Vec::new();
    if !transformed && rs(state, 137) != 0 {
        let raw_lights = state.enabled_lights();
        lights
            .try_reserve_exact(raw_lights.len())
            .map_err(|_| RenderError::out_of_memory("Draw", "fixed-function lights"))?;
        for raw in raw_lights {
            let values = raw.map(f32::from_bits);
            let view_direction = transform_point(&view, [values[16], values[17], values[18], 0.0]);
            let position = if raw[0] == 3 {
                [0.0; 4]
            } else {
                transform_point(&view, [values[13], values[14], values[15], 1.0])
            };
            let direction = match raw[0] {
                2 => normalize3([view_direction[0], view_direction[1], view_direction[2]]),
                3 => normalize3([-view_direction[0], -view_direction[1], -view_direction[2]]),
                _ => [0.0; 3],
            };
            let (spot_inner, spot_outer) = if raw[0] == 2 {
                ((values[24] * 0.5).cos(), (values[25] * 0.5).cos())
            } else {
                (0.0, 0.0)
            };
            lights.push(FfLight {
                kind: raw[0],
                values,
                position,
                direction,
                spot_inner,
                spot_outer,
            });
        }
    }
    let mut stage_coords = Vec::new();
    stage_coords
        .try_reserve_exact(8)
        .map_err(|_| RenderError::out_of_memory("Draw", "fixed-function texture stages"))?;
    for stage in 0..8 {
        let coord = tss(state, stage, 11);
        let transform = if !transformed && tss(state, stage, 24) & 255 != 0 {
            Some(state.transform(16 + stage)?)
        } else {
            None
        };
        stage_coords.push((coord, transform));
    }
    let vp = state.viewport();
    let mut out = Vec::new();
    out.try_reserve_exact(
        order
            .len()
            .checked_mul(192)
            .ok_or_else(|| invalid("Draw", "vertex allocation overflow"))?,
    )
    .map_err(|_| RenderError::out_of_memory("Draw", "fixed-function vertices"))?;
    let Some(&max_index) = order.iter().max() else {
        return Ok(out);
    };
    let prepare = |e: &VertexElement| {
        VertexFetch::prepare(command, e.stream, e.offset, e.element_type, max_index)
    };
    let position = prepare(position)?;
    let colors = [
        find(10, 0).map(&prepare).transpose()?,
        find(10, 1).map(&prepare).transpose()?,
    ];
    let normal = if needs_normal {
        find(3, 0).map(&prepare).transpose()?
    } else {
        None
    };
    let mut unique_texcoords: [Option<VertexFetch<'_>>; 8] = std::array::from_fn(|_| None);
    let mut texcoord_sources: [Option<u8>; 8] = [None; 8];
    for (stage, &(coord, _)) in stage_coords.iter().enumerate() {
        if coord & 0xffff0000 != 0 {
            continue;
        }
        let usage = (coord & 0xffff) as u8;
        if unique_texcoords[usage as usize].is_none() {
            match find(5, usage) {
                Some(element) => unique_texcoords[usage as usize] = Some(prepare(element)?),
                None => continue,
            }
        }
        texcoord_sources[stage] = Some(usage);
    }
    let prep = FfDraw::prepare(state);
    for &index in order {
        let fetch = |e: &VertexFetch<'_>| e.fetch(index);
        let p = fetch(&position)?;
        let viewp = if transformed {
            [p[0], p[1], p[2], 1.0]
        } else {
            transform_point(&worldview, p)
        };
        let clip = if transformed {
            if p[3] == 0.0 || !p[3].is_finite() {
                return Err(invalid("Draw", "invalid POSITIONT reciprocal w"));
            }
            let w = 1.0 / p[3];
            let dz = vp.max_z - vp.min_z;
            [
                ((p[0] - vp.x as f32) / vp.width as f32 * 2.0 - 1.0) * w,
                (1.0 - (p[1] - vp.y as f32) / vp.height as f32 * 2.0) * w,
                if dz == 0.0 {
                    0.0
                } else {
                    (p[2] - vp.min_z) / dz * w
                },
                w,
            ]
        } else {
            transform_point(&wvp, p)
        };
        let vertex = [
            colors[0].as_ref().map(&fetch).transpose()?,
            colors[1].as_ref().map(&fetch).transpose()?,
        ];
        let normal = if needs_normal {
            let n = normal.as_ref().map(&fetch).transpose()?.unwrap_or([0.0; 4]);
            let normal = if let Some(inv) = normal_matrix {
                std::array::from_fn(|i| (0..3).map(|j| n[j] * inv[i][j]).sum())
            } else {
                [n[0], n[1], n[2]]
            };
            if !transformed && prep.normalize_normals {
                normalize3(normal)
            } else {
                normal
            }
        } else {
            [0.0; 3]
        };
        let colors = if transformed {
            [vertex[0].unwrap_or([1.0; 4]), vertex[1].unwrap_or([0.0; 4])]
        } else {
            ff_colors(&prep, viewp, normal, vertex, &lights)?
        };
        let mut packed = [0.0f32; 48];
        packed[..4].copy_from_slice(&clip);
        packed[4..8].copy_from_slice(&colors[0]);
        packed[8..12].copy_from_slice(&colors[1]);
        let mut texcoord_cache: [Option<[f32; 4]>; 8] = [None; 8];
        for (stage, &(coord, transform)) in stage_coords.iter().enumerate() {
            let mut uv = match coord & 0xffff0000 {
                0 => match texcoord_sources[stage] {
                    Some(usage) => {
                        let slot = usage as usize;
                        if let Some(value) = texcoord_cache[slot] {
                            value
                        } else {
                            let value = unique_texcoords[slot]
                                .as_ref()
                                .expect("prepared texture coordinate")
                                .fetch(index)?;
                            texcoord_cache[slot] = Some(value);
                            value
                        }
                    }
                    None => [0.0, 0.0, 0.0, 1.0],
                },
                0x10000 => [normal[0], normal[1], normal[2], 1.0],
                0x20000 => viewp,
                0x30000 => {
                    let incident = normalize3([viewp[0], viewp[1], viewp[2]]);
                    let twice_dot = 2.0 * dot3(incident, normal);
                    [
                        incident[0] - twice_dot * normal[0],
                        incident[1] - twice_dot * normal[1],
                        incident[2] - twice_dot * normal[2],
                        1.0,
                    ]
                }
                _ => {
                    return Err(unsupported(
                        "Draw",
                        format!("texture-coordinate generation {coord:#x} is not implemented"),
                    ));
                }
            };
            if let Some(transform) = transform {
                if let Some(usage) = texcoord_sources[stage] {
                    let source = unique_texcoords[usage as usize]
                        .as_ref()
                        .expect("prepared texture coordinate");
                    let count = match source.element_type {
                        D3DDECLTYPE_FLOAT1 => 1,
                        D3DDECLTYPE_FLOAT2
                        | D3DDECLTYPE_SHORT2
                        | D3DDECLTYPE_SHORT2N
                        | D3DDECLTYPE_USHORT2N
                        | D3DDECLTYPE_FLOAT16_2 => 2,
                        D3DDECLTYPE_FLOAT3 | D3DDECLTYPE_UDEC3 | D3DDECLTYPE_DEC3N => 3,
                        _ => 4,
                    };
                    if count < 4 {
                        uv[count..].fill(0.0);
                        uv[count] = 1.0;
                    }
                }
                uv = transform_point(&transform, uv);
            }
            let start = 12 + stage * 4;
            packed[start..start + 4].copy_from_slice(&uv);
        }
        let fog = if transformed {
            vertex[1].map(|v| v[3]).unwrap_or(1.0)
        } else {
            prep.fog.factor(viewp[2].abs())
        };
        packed[44] = fog;
        packed[45] = viewp[2];
        append_f32_le(&mut out, &packed);
    }
    Ok(out)
}
const OUTPUT: &str = include_str!("../shader/raster.wgsl");
fn ff_vertex_source() -> String {
    let mut source = String::from(V2F);
    source += shader_interface::VERTEX_PARAMETERS;
    source += "@group(0) @binding(1) var<uniform> vsp:VSP;\nstruct FfIn {\n";
    for i in 0..12 {
        source += &format!("@location({i}) v{i}:vec4<f32>,\n");
    }
    source += "}\n@vertex fn vs_main(v:FfIn)->V2F {var o:V2F;o.pos=vec4<f32>(v.v0.xy+vsp.halfpix.xy*v.v0.w,v.v0.zw);o.d0=v.v1;o.d1=v.v2;\n";
    for i in 0..8 {
        source += &format!("o.t{i}=v.v{};\n", i + 3);
    }
    source += "o.fog=v.v11.x;o.fogz=v.v11.y;return o;}\n";
    source
}
fn ff_arg(value: u32, stage: u32) -> Result<String, RenderError> {
    let base = match value & 15 {
        0 => "vin.d0".to_string(),
        1 => "current".to_string(),
        2 => "tex".to_string(),
        3 => "tfactor".to_string(),
        4 => "vin.d1".to_string(),
        5 => "temp".to_string(),
        6 => format!("constant{stage}"),
        _ => return Err(unsupported("Draw", format!("D3DTA {value:#x}"))),
    };
    let base = if value & 32 != 0 {
        format!("vec4<f32>(({base}).a)")
    } else {
        base
    };
    Ok(if value & 16 != 0 {
        format!("(vec4<f32>(1.0)-{base})")
    } else {
        base
    })
}
fn ff_op(op: u32, a: &str, b: &str, c: &str) -> Result<String, RenderError> {
    Ok(match op {
        2 => a.into(),
        3 => b.into(),
        4 => format!("({a}*{b})"),
        5 => format!("(2.0*{a}*{b})"),
        6 => format!("(4.0*{a}*{b})"),
        7 => format!("({a}+{b})"),
        8 => format!("({a}+{b}-vec4<f32>(0.5))"),
        9 => format!("(2.0*({a}+{b}-vec4<f32>(0.5)))"),
        10 => format!("({a}-{b})"),
        11 => format!("({a}+{b}*(vec4<f32>(1.0)-{a}))"),
        12 => format!("mix({b},{a},vin.d0.a)"),
        13 => format!("mix({b},{a},tex.a)"),
        14 => format!("mix({b},{a},tfactor.a)"),
        15 => format!("({a}+{b}*(1.0-tex.a))"),
        16 => format!("mix({b},{a},current.a)"),
        18 => format!("({a}+{b}*({a}).a)"),
        19 => format!("({a}*{b}+vec4<f32>(({a}).a))"),
        20 => format!("({a}+{b}*(1.0-({a}).a))"),
        21 => format!("((vec4<f32>(1.0)-{a})*{b}+vec4<f32>(({a}).a))"),
        24 => format!("vec4<f32>(4.0*dot(({a}).rgb-vec3<f32>(0.5),({b}).rgb-vec3<f32>(0.5)))"),
        25 => format!("({a}*{b}+{c})"),
        26 => format!("mix({b},{a},{c})"),
        _ => {
            return Err(unsupported(
                "Draw",
                format!("D3DTOP {op} is not implemented"),
            ));
        }
    })
}
fn ff_fragment_source(state: &D3d9State, textures: &[BoundTexture]) -> Result<String, RenderError> {
    let mut source = String::from(V2F);
    source += &shader_interface::pixel_parameters(false);
    source += "@group(0) @binding(3) var<uniform> psp:PSP;\n";
    let mut body = format!(
        "var current=vin.d0;var temp=vec4<f32>(0.0);let tfactor=vec4<f32>({},{},{},{});\n",
        color(rs(state, 60))[0],
        color(rs(state, 60))[1],
        color(rs(state, 60))[2],
        color(rs(state, 60))[3]
    );
    for stage in 0..8 {
        let op = tss(state, stage, 1);
        if op == 1 {
            break;
        }
        let bound = textures.iter().find(|t| t.stage == stage);
        if bound.is_none() && tss(state, stage, 2) & 15 == 2 {
            break;
        }
        let alpha_op = tss(state, stage, 4);
        if alpha_op == 1 {
            return Err(unsupported(
                "Draw",
                "D3DTSS_ALPHAOP DISABLE with enabled COLOROP is undefined",
            ));
        }
        if matches!(alpha_op, 18..=23) {
            return Err(unsupported(
                "Draw",
                format!("D3DTSS_ALPHAOP {alpha_op} is not implemented"),
            ));
        }
        let needs = |operation: u32, first: u32, second: u32, third: u32| {
            matches!(operation, 13 | 15)
                || match operation {
                    1 => false,
                    2 => first & 15 == 2,
                    3 => second & 15 == 2,
                    25 | 26 => [first, second, third].iter().any(|v| v & 15 == 2),
                    _ => [first, second].iter().any(|v| v & 15 == 2),
                }
        };
        let alpha_arg1 = if bound.is_none() && tss(state, stage, 5) & 15 == 2 {
            tss(state, stage, 5) & !15
        } else {
            tss(state, stage, 5)
        };
        let needs_texture = needs(
            op,
            tss(state, stage, 2),
            tss(state, stage, 3),
            tss(state, stage, 26),
        ) || needs(
            alpha_op,
            alpha_arg1,
            tss(state, stage, 6),
            tss(state, stage, 27),
        );
        let texture = textures
            .iter()
            .find(|t| t.stage == stage)
            .filter(|_| needs_texture);
        if needs_texture && texture.is_none() {
            return Err(unsupported(
                "Draw",
                format!("fixed-function stage {stage} references an unbound texture"),
            ));
        }
        let constant = color(tss(state, stage, 32));
        body += &format!(
            "let constant{stage}=vec4<f32>({},{},{},{});\n",
            constant[0], constant[1], constant[2], constant[3]
        );
        body += "{\n";
        if let Some(t) = texture {
            if t.depth {
                return Err(unsupported(
                    "Draw",
                    "fixed-function depth sampling is not implemented",
                ));
            }
            let dimension = if t.cube {
                "cube"
            } else if t.volume {
                "3d"
            } else {
                "2d"
            };
            source += &format!(
                "@group(0) @binding({}) var tx{stage}:texture_{dimension}<f32>;\n@group(0) @binding({}) var sm{stage}:sampler;\n",
                TEXTURE_BINDING + stage,
                SAMPLER_BINDING + stage
            );
            let flags = tss(state, stage, 24);
            let mut coord = format!("vin.t{stage}");
            if flags & 256 != 0 {
                let component = match flags & 255 {
                    0 if state.shader(false) != 0 => "w",
                    2 => "y",
                    3 => "z",
                    4 => "w",
                    _ => {
                        return Err(unsupported(
                            "Draw",
                            "invalid projected texture coordinate count",
                        ));
                    }
                };
                coord = format!("({coord}/{coord}.{component})");
            }
            let sample = format!(
                "textureSample(tx{stage},sm{stage},({coord}).{})",
                if t.cube || t.volume { "xyz" } else { "xy" }
            );
            body += &format!(
                "let tex={};\n",
                if opaque_format(t.format) {
                    format!("vec4<f32>(({sample}).rgb,1.0)")
                } else {
                    sample
                }
            );
        } else {
            body += "let tex=vec4<f32>(1.0);\n";
        }
        let a = ff_arg(tss(state, stage, 2), stage)?;
        let b = ff_arg(tss(state, stage, 3), stage)?;
        let c = ff_arg(tss(state, stage, 26), stage)?;
        let rgb = ff_op(op, &a, &b, &c)?;
        let a = ff_arg(alpha_arg1, stage)?;
        let b = ff_arg(tss(state, stage, 6), stage)?;
        let c = ff_arg(tss(state, stage, 27), stage)?;
        let alpha = if tss(state, stage, 4) == 1 {
            "current.a".into()
        } else {
            format!("({}).a", ff_op(tss(state, stage, 4), &a, &b, &c)?)
        };
        let result = if tss(state, stage, 28) == 5 {
            "temp"
        } else {
            "current"
        };
        body += &format!(
            "{result}=clamp(vec4<f32>(({rgb}).rgb,{alpha}),vec4<f32>(0.0),vec4<f32>(1.0));\n}}\n"
        );
    }
    if rs(state, 29) != 0 {
        body += "current=vec4<f32>(clamp(current.rgb+vin.d1.rgb,vec3<f32>(0.0),vec3<f32>(1.0)),current.a);\n";
    }
    source += OUTPUT;
    source += &format!(
        "@fragment fn ps_main(vin:V2F)->@location(0) vec4<f32>{{{body}return d9_output(current,vin);}}\n"
    );
    Ok(source)
}

fn compare(value: u32) -> wgpu::CompareFunction {
    match value {
        1 => wgpu::CompareFunction::Never,
        2 => wgpu::CompareFunction::Less,
        3 => wgpu::CompareFunction::Equal,
        4 => wgpu::CompareFunction::LessEqual,
        5 => wgpu::CompareFunction::Greater,
        6 => wgpu::CompareFunction::NotEqual,
        7 => wgpu::CompareFunction::GreaterEqual,
        _ => wgpu::CompareFunction::Always,
    }
}
fn stencil_op(value: u32) -> wgpu::StencilOperation {
    match value {
        1 => wgpu::StencilOperation::Keep,
        2 => wgpu::StencilOperation::Zero,
        3 => wgpu::StencilOperation::Replace,
        4 => wgpu::StencilOperation::IncrementClamp,
        5 => wgpu::StencilOperation::DecrementClamp,
        6 => wgpu::StencilOperation::Invert,
        7 => wgpu::StencilOperation::IncrementWrap,
        _ => wgpu::StencilOperation::DecrementWrap,
    }
}
fn depth_state(
    state: &D3d9State,
    depth: Option<&SurfaceView>,
) -> Result<Option<wgpu::DepthStencilState>, RenderError> {
    let Some(depth) = depth else {
        return Ok(None);
    };
    if rs(state, 7) == 2 {
        return Err(unsupported("Draw", "W-buffer depth is not implemented"));
    }
    if rs(state, 52) != 0 && !super::format::has_stencil(depth.format) {
        return Err(invalid(
            "Draw",
            "stencil enabled on a format without stencil",
        ));
    }
    let face = |fail, zfail, pass, func| wgpu::StencilFaceState {
        compare: compare(rs(state, func)),
        fail_op: stencil_op(rs(state, fail)),
        depth_fail_op: stencil_op(rs(state, zfail)),
        pass_op: stencil_op(rs(state, pass)),
    };
    let stencil = if rs(state, 52) != 0 {
        wgpu::StencilState {
            front: face(53, 54, 55, 56),
            back: if rs(state, 185) != 0 {
                face(186, 187, 188, 189)
            } else {
                face(53, 54, 55, 56)
            },
            read_mask: rs(state, 58),
            write_mask: rs(state, 59),
        }
    } else {
        wgpu::StencilState::default()
    };
    let bias = f32::from_bits(rs(state, 195));
    if bias != 0.0 {
        return Err(unsupported(
            "Draw",
            "constant depth bias conversion is deferred",
        ));
    }
    Ok(Some(wgpu::DepthStencilState {
        format: depth.texture.format(),
        depth_write_enabled: Some(rs(state, 7) != 0 && rs(state, 14) != 0),
        depth_compare: Some(if rs(state, 7) != 0 {
            compare(rs(state, 23))
        } else {
            wgpu::CompareFunction::Always
        }),
        stencil,
        bias: wgpu::DepthBiasState {
            constant: 0,
            slope_scale: f32::from_bits(rs(state, 175)),
            clamp: 0.0,
        },
    }))
}
fn blend_factor(v: u32, alpha: bool) -> Result<wgpu::BlendFactor, RenderError> {
    Ok(match v {
        1 => wgpu::BlendFactor::Zero,
        2 => wgpu::BlendFactor::One,
        3 => {
            if alpha {
                wgpu::BlendFactor::SrcAlpha
            } else {
                wgpu::BlendFactor::Src
            }
        }
        4 => {
            if alpha {
                wgpu::BlendFactor::OneMinusSrcAlpha
            } else {
                wgpu::BlendFactor::OneMinusSrc
            }
        }
        5 => wgpu::BlendFactor::SrcAlpha,
        6 => wgpu::BlendFactor::OneMinusSrcAlpha,
        7 => wgpu::BlendFactor::DstAlpha,
        8 => wgpu::BlendFactor::OneMinusDstAlpha,
        9 => {
            if alpha {
                wgpu::BlendFactor::DstAlpha
            } else {
                wgpu::BlendFactor::Dst
            }
        }
        10 => {
            if alpha {
                wgpu::BlendFactor::OneMinusDstAlpha
            } else {
                wgpu::BlendFactor::OneMinusDst
            }
        }
        11 => {
            if alpha {
                wgpu::BlendFactor::One
            } else {
                wgpu::BlendFactor::SrcAlphaSaturated
            }
        }
        14 => wgpu::BlendFactor::Constant,
        15 => wgpu::BlendFactor::OneMinusConstant,
        _ => {
            return Err(unsupported(
                "Draw",
                format!("D3DBLEND {v} is not implemented"),
            ));
        }
    })
}
fn blend_op(v: u32) -> wgpu::BlendOperation {
    match v {
        1 => wgpu::BlendOperation::Add,
        2 => wgpu::BlendOperation::Subtract,
        3 => wgpu::BlendOperation::ReverseSubtract,
        4 => wgpu::BlendOperation::Min,
        _ => wgpu::BlendOperation::Max,
    }
}
fn blend_state(state: &D3d9State, opaque: bool) -> Result<Option<wgpu::BlendState>, RenderError> {
    if rs(state, 27) == 0 {
        return Ok(None);
    }
    let component = |src, dst, op, alpha| -> Result<wgpu::BlendComponent, RenderError> {
        let (s, d) = match rs(state, src) {
            12 => (5, 6),
            13 => (6, 5),
            v => (v, rs(state, dst)),
        };
        Ok(wgpu::BlendComponent {
            src_factor: if opaque && s == 7 {
                wgpu::BlendFactor::One
            } else if opaque && s == 8 {
                wgpu::BlendFactor::Zero
            } else {
                blend_factor(s, alpha)?
            },
            dst_factor: if opaque && d == 7 {
                wgpu::BlendFactor::One
            } else if opaque && d == 8 {
                wgpu::BlendFactor::Zero
            } else {
                blend_factor(d, alpha)?
            },
            operation: blend_op(rs(state, op)),
        })
    };
    Ok(Some(wgpu::BlendState {
        color: component(19, 20, 171, false)?,
        alpha: if rs(state, 206) != 0 {
            component(207, 208, 209, true)?
        } else {
            component(19, 20, 171, true)?
        },
    }))
}
fn sampler(
    gpu: &D3d9Gpu,
    state: &D3d9State,
    t: &BoundTexture,
) -> Result<(SamplerKey, wgpu::SamplerDescriptor<'static>), RenderError> {
    let address = |id| -> Result<wgpu::AddressMode, RenderError> {
        Ok(match ss(state, t.stage, id) {
            1 => wgpu::AddressMode::Repeat,
            2 => wgpu::AddressMode::MirrorRepeat,
            3 => wgpu::AddressMode::ClampToEdge,
            4 if gpu
                .features()
                .contains(wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER) =>
            {
                wgpu::AddressMode::ClampToBorder
            }
            4 => {
                return Err(unsupported(
                    "Draw",
                    format!(
                        "sampler {} border address mode requires ADDRESS_MODE_CLAMP_TO_BORDER",
                        t.stage
                    ),
                ));
            }
            v => {
                return Err(unsupported(
                    "Draw",
                    format!("sampler {} address mode {v} is not implemented", t.stage),
                ));
            }
        })
    };
    let filter = |id| -> Result<wgpu::FilterMode, RenderError> {
        Ok(match ss(state, t.stage, id) {
            0 | 1 => wgpu::FilterMode::Nearest,
            2 => wgpu::FilterMode::Linear,
            v => {
                return Err(unsupported(
                    "Draw",
                    format!("sampler {} filter {v} is not implemented", t.stage),
                ));
            }
        })
    };
    if ss(state, t.stage, 8) != 0 {
        return Err(unsupported("Draw", "sampler LOD bias is not implemented"));
    }
    if ss(state, t.stage, 11) != 0 {
        return Err(unsupported(
            "Draw",
            "sRGB texture sampling views are not implemented",
        ));
    }
    let lod_min = ss(state, t.stage, 9)
        .max(t.lod)
        .min(t.levels.saturating_sub(1)) as f32;
    let lod_max = if ss(state, t.stage, 7) == 0 {
        lod_min
    } else {
        t.levels.saturating_sub(1) as f32
    };
    let anisotropic = [5, 6].iter().any(|&id| ss(state, t.stage, id) == 3);
    let mipmap_filter = match filter(7)? {
        wgpu::FilterMode::Nearest => wgpu::MipmapFilterMode::Nearest,
        wgpu::FilterMode::Linear => wgpu::MipmapFilterMode::Linear,
    };
    let (mag_filter, min_filter) = if anisotropic {
        (wgpu::FilterMode::Linear, wgpu::FilterMode::Linear)
    } else {
        (filter(5)?, filter(6)?)
    };
    let anisotropy_clamp = if anisotropic && mipmap_filter == wgpu::MipmapFilterMode::Linear {
        ss(state, t.stage, 10).clamp(1, MAX_ANISOTROPY) as u16
    } else {
        1
    };
    let border_color = if [1, 2, 3].iter().any(|&id| ss(state, t.stage, id) == 4) {
        Some(match ss(state, t.stage, 4) {
            0x0000_0000 => wgpu::SamplerBorderColor::TransparentBlack,
            0xff00_0000 => wgpu::SamplerBorderColor::OpaqueBlack,
            0xffff_ffff => wgpu::SamplerBorderColor::OpaqueWhite,
            v => {
                return Err(unsupported(
                    "Draw",
                    format!(
                        "sampler {} border color {v:#010x} is not implemented",
                        t.stage
                    ),
                ));
            }
        })
    } else {
        None
    };
    let key = SamplerKey {
        address: [
            ss(state, t.stage, 1),
            ss(state, t.stage, 2),
            ss(state, t.stage, 3),
        ],
        filters: [
            ss(state, t.stage, 5),
            ss(state, t.stage, 6),
            ss(state, t.stage, 7),
        ],
        anisotropy: anisotropy_clamp,
        border: border_color,
        lod_min: lod_min.to_bits(),
        lod_max: lod_max.to_bits(),
        depth: t.depth,
    };
    Ok((
        key,
        wgpu::SamplerDescriptor {
            label: Some("d3d9-sampler"),
            address_mode_u: address(1)?,
            address_mode_v: address(2)?,
            address_mode_w: address(3)?,
            mag_filter,
            min_filter,
            mipmap_filter,
            lod_min_clamp: lod_min,
            lod_max_clamp: lod_max,
            anisotropy_clamp,
            border_color,
            compare: if t.depth {
                Some(wgpu::CompareFunction::LessEqual)
            } else {
                None
            },
            ..Default::default()
        },
    ))
}
fn make_buffer(
    gpu: &D3d9Gpu,
    label: &'static str,
    bytes: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    gpu.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: if bytes.is_empty() { &[0; 16] } else { bytes },
            usage,
        })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TextureLayout {
    depth: bool,
    filterable: bool,
    dimension: wgpu::TextureViewDimension,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LayoutKey {
    Clear,
    Draw {
        vs: u32,
        ps: u32,
        textures: [Option<TextureLayout>; MAX_SAMPLERS],
    },
}

impl LayoutKey {
    fn entries(&self) -> Vec<wgpu::BindGroupLayoutEntry> {
        let (vs, ps, textures) = match self {
            LayoutKey::Clear => {
                return vec![uniform_entry(0, 32, wgpu::ShaderStages::FRAGMENT, false)];
            }
            LayoutKey::Draw { vs, ps, textures } => (*vs, *ps, textures),
        };
        let mut entries = vec![
            uniform_entry(0, u64::from(vs) * 16, wgpu::ShaderStages::VERTEX, true),
            uniform_entry(
                1,
                shader_interface::VERTEX_PARAMETER_BYTES as u64,
                wgpu::ShaderStages::VERTEX,
                true,
            ),
            uniform_entry(2, u64::from(ps) * 16, wgpu::ShaderStages::FRAGMENT, true),
            uniform_entry(
                3,
                shader_interface::PIXEL_PARAMETER_BYTES as u64,
                wgpu::ShaderStages::FRAGMENT,
                true,
            ),
        ];
        for (stage, texture) in (0u32..).zip(textures) {
            let Some(texture) = texture else {
                continue;
            };
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: TEXTURE_BINDING + stage,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: if texture.depth {
                        wgpu::TextureSampleType::Depth
                    } else {
                        wgpu::TextureSampleType::Float {
                            filterable: texture.filterable,
                        }
                    },
                    view_dimension: texture.dimension,
                    multisampled: false,
                },
                count: None,
            });
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: SAMPLER_BINDING + stage,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(if texture.depth {
                    wgpu::SamplerBindingType::Comparison
                } else {
                    wgpu::SamplerBindingType::Filtering
                }),
                count: None,
            });
        }
        entries
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum KeyWords {
    Inline(u8, [u32; 8]),
    Heap(Vec<u32>),
}

impl KeyWords {
    fn from_slice(values: &[u32]) -> Self {
        if values.len() <= 8 {
            let mut inline = [0; 8];
            inline[..values.len()].copy_from_slice(values);
            Self::Inline(values.len() as u8, inline)
        } else {
            Self::Heap(values.to_vec())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ShaderOrigin {
    Guest(ProgramKey),
    FixedFunction,
    DeclaredFixedFunction,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ShaderVariantKey {
    pixel: bool,
    shader: ShaderOrigin,
    state: KeyWords,
}

#[derive(Clone)]
struct CachedShaderSource {
    source: Arc<str>,
    recipe: ShaderRecipe,
    texture_stages: Vec<u32>,
    module: std::sync::OnceLock<wgpu::ShaderModule>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SamplerKey {
    address: [u32; 3],
    filters: [u32; 3],
    anisotropy: u16,
    border: Option<wgpu::SamplerBorderColor>,
    lod_min: u32,
    lod_max: u32,
    depth: bool,
}

#[derive(Clone)]
struct PendingTexture {
    stage: u32,
    texture: Option<wgpu::Texture>,
    view: wgpu::TextureView,
    sampler: wgpu::Sampler,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct BindGroupKey {
    layout: wgpu::BindGroupLayout,
    uniform_sizes: [u64; 4],
    programs: Option<[wgpu::Buffer; 2]>,
    textures: SmallVec<[(u32, Option<wgpu::Texture>, wgpu::TextureView, wgpu::Sampler); 4]>,
}

struct PendingDraw {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    uniform_offsets: [u32; 4],
    uniform_sizes: [u64; 4],
    programs: Option<[wgpu::Buffer; 2]>,
    textures: Arc<[PendingTexture]>,
    vertex_offset: u64,
    vertex_size: u64,
    has_attributes: bool,
    native_geometry: Option<Box<NativeGeometry>>,
    index_range: Option<(u64, u64, wgpu::IndexFormat)>,
    vertex_count: u32,
    viewport: super::state::Viewport,
    stencil_reference: u32,
    blend_constant: [f32; 4],
    scissor: Option<[u32; 4]>,
}

fn cached_bind_group(
    groups: &mut FastMap<BindGroupKey, wgpu::BindGroup>,
    previous: &mut FastMap<BindGroupKey, wgpu::BindGroup>,
    key: &BindGroupKey,
) -> Option<wgpu::BindGroup> {
    if let Some(group) = groups.get(key) {
        return Some(group.clone());
    }
    let group = previous.remove(key)?;
    if groups.len() >= BIND_GROUP_CACHE_LIMIT {
        *previous = std::mem::take(groups);
    }
    groups.insert(key.clone(), group.clone());
    Some(group)
}

fn store_bind_group(
    groups: &mut FastMap<BindGroupKey, wgpu::BindGroup>,
    previous: &mut FastMap<BindGroupKey, wgpu::BindGroup>,
    key: BindGroupKey,
    group: &wgpu::BindGroup,
) {
    if groups.len() >= BIND_GROUP_CACHE_LIMIT {
        *previous = std::mem::take(groups);
    }
    groups.insert(key, group.clone());
}

fn same_bindings(a: &PendingDraw, b: &PendingDraw) -> bool {
    a.layout == b.layout
        && a.uniform_sizes == b.uniform_sizes
        && a.programs == b.programs
        && a.textures.len() == b.textures.len()
        && a.textures
            .iter()
            .zip(b.textures.iter())
            .all(|(x, y)| x.stage == y.stage && x.view == y.view && x.sampler == y.sampler)
}

#[derive(Default)]
struct DrawBatch {
    draws: Vec<PendingDraw>,
    data: Vec<u8>,
    color_view: Option<wgpu::TextureView>,
    color_key_view: Option<wgpu::TextureView>,
    padded_color: Option<PaddedColor>,
    depth: Option<(wgpu::TextureView, bool)>,
    width: u32,
    height: u32,
    last_uniform: Option<([u32; 4], [u64; 4])>,
}

struct PaddedColor {
    texture: wgpu::Texture,
    source: SurfaceView,
}

const DRAW_MEMOS: usize = 16;
const BIND_GROUP_CACHE_LIMIT: usize = 4096;

struct DrawMemo {
    fingerprint: u64,
    ff_key: Option<[u32; 5]>,
    raw_inputs: u16,
    target: (wgpu::TextureFormat, u32),
    depth: Option<(wgpu::TextureFormat, u32)>,
    textures: BoundTextures,
    bindings: Arc<[PendingTexture]>,
    vs_count: u32,
    ps_count: u32,
    programs: Option<[wgpu::Buffer; 2]>,
    modules: [wgpu::ShaderModule; 2],
    recipes: [ShaderRecipe; 2],
    binding_entries: Arc<[wgpu::BindGroupLayoutEntry]>,
    layout: wgpu::BindGroupLayout,
    trace_interface: Option<trace::Interface>,
    trace_pipeline: Option<(u64, u64)>,
    pipeline: Option<(
        (
            VertexLayouts,
            wgpu::PrimitiveTopology,
            Option<wgpu::IndexFormat>,
        ),
        wgpu::RenderPipeline,
    )>,
}

#[derive(Default)]
pub(crate) struct D3d9DrawState {
    shaders: LruCache<String, wgpu::ShaderModule, 512, { 32 << 20 }>,
    shader_variants: LruCache<ShaderVariantKey, Arc<CachedShaderSource>, 512, { 32 << 20 }>,
    samplers: LruCache<SamplerKey, wgpu::Sampler, 128>,
    layouts: LruCache<LayoutKey, wgpu::BindGroupLayout, 128>,
    pipelines:
        LruCache<PipelineRecipeKey, (CompiledPipeline, Arc<PipelineRecipe>), 512, { 32 << 20 }>,
    batch: DrawBatch,
    index_remap: Vec<u32>,
    geometry: FastMap<usize, CachedGeometry>,
    geometry_seen: FastMap<usize, (Weak<Vec<u8>>, u64)>,
    index_bounds: FastMap<usize, CachedIndexBounds>,
    frame: u64,
    trace_frame_draw: u32,
    trace_total_draw: u64,
    trace_shaders: std::collections::BTreeSet<u32>,
    trace_identity: bool,
    mvp_capture: trace::MvpCapture,
    observed_families: FastMap<trace::Family, u64>,
    inventory_programs: std::collections::BTreeSet<Arc<[u32]>>,
    interpreter_programs: uber::ProgramCache,
    compiler: Option<crate::render::compile::Compiler>,
    specialization: uber::RecipeCache,
    residual_reasons: FastMap<String, u64>,
    interpreter_draws: u64,
    interpreter_frame: uber::FrameCounters,
    measured_draws: u64,
    draw_cpu_ms: f64,
    geometry_cpu_ms: f64,
    specialization_cpu_ms: f64,
    packed_bytes: u64,
    geometry_upload_bytes: u64,
    batch_upload_bytes: u64,
    module_ms: f64,
    pipeline_ms: f64,
    upload: Option<wgpu::Buffer>,
    bind_groups: FastMap<BindGroupKey, wgpu::BindGroup>,
    bind_groups_previous: FastMap<BindGroupKey, wgpu::BindGroup>,
    bind_group_scratch: Vec<wgpu::BindGroup>,
    linked: Vec<LinkedInput>,
    ff_uniforms: Vec<f32>,
    index_slots: FastMap<u32, u32>,
    native_streams: SmallVec<[(u32, VertexAttributes); 2]>,
    native_geometry_pool: Vec<Box<NativeGeometry>>,
    pipeline_memo: Option<(PipelineKey, wgpu::RenderPipeline)>,
    memos: Vec<Box<DrawMemo>>,
}
impl D3d9DrawState {
    fn log_cache_clear(&self, reason: &str) {
        eprintln!(
            "[d3d9-compile] cache-clear frame={} reason={reason} variants={} modules={} layouts={} pipelines={}",
            self.frame,
            self.shader_variants.len(),
            self.shaders.len(),
            self.layouts.len(),
            self.pipelines.len()
        );
    }

    fn clear(&mut self, reason: &str) {
        self.log_cache_clear(reason);
        self.memos.clear();
        self.specialization = uber::RecipeCache::default();
        self.pipeline_memo = None;
        self.pipelines.clear();
        self.layouts.clear();
        self.shaders.clear();
        self.shader_variants.clear();
        self.samplers.clear();
        self.bind_groups.clear();
        self.bind_groups_previous.clear();
        self.geometry.clear();
        self.geometry_seen.clear();
        self.index_bounds.clear();
    }

    pub(crate) fn end_frame(&mut self, gpu: &D3d9Gpu) {
        if uber::hybrid(gpu) {
            if let Some(compiler) = &mut self.compiler {
                compiler.end_frame(self.frame);
            }
            for (reason, draws) in self.residual_reasons.drain() {
                eprintln!(
                    "[d3d9-async] residual frame={} reason={reason:?} draws={draws}",
                    self.frame
                );
            }
        }
        if trace_compilation() {
            eprintln!(
                "[d3d9-compile] frame frame={} draws={} draw_cpu_ms={:.3} geometry_cpu_ms={:.3} packed_bytes={} geometry_upload_bytes={} batch_upload_bytes={} module_ms={:.3} pipeline_ms={:.3} specialization_cpu_ms={:.3} recipe_cache_hits={} recipe_cache_misses={} selection_cache_hits={} recipe_fast_hits={}",
                self.frame,
                self.measured_draws,
                self.draw_cpu_ms,
                self.geometry_cpu_ms,
                self.packed_bytes,
                self.geometry_upload_bytes,
                self.batch_upload_bytes,
                self.module_ms,
                self.pipeline_ms,
                self.specialization_cpu_ms,
                self.specialization.hits,
                self.specialization.misses,
                self.specialization.selection_hits,
                self.specialization.fast_hits
            );
        }
        if trace_compilation() && uber::enabled(gpu) {
            let c = &self.interpreter_frame;
            eprintln!(
                "[d3d9-uber] frame frame={} draws={} interpreted_draws={} total_interpreted_draws={} timing={} cpu_ms={:.3} packing_ms={:.3} packed_bytes={} uniform_bytes={} resident_program_bytes={} program_cache_bytes_estimate={}",
                self.frame,
                self.measured_draws,
                c.draws,
                self.interpreter_draws,
                trace_compilation(),
                c.cpu_ms,
                c.packing_ms,
                c.packed_bytes,
                c.uniform_bytes,
                self.interpreter_programs.len() as u64 * crate::shader::uber::PROGRAM_BYTES,
                self.interpreter_programs.bytes()
            );
        }
        self.measured_draws = 0;
        self.draw_cpu_ms = 0.0;
        self.geometry_cpu_ms = 0.0;
        self.specialization_cpu_ms = 0.0;
        self.specialization.hits = 0;
        self.specialization.misses = 0;
        self.specialization.selection_hits = 0;
        self.specialization.fast_hits = 0;
        self.packed_bytes = 0;
        self.geometry_upload_bytes = 0;
        self.batch_upload_bytes = 0;
        self.module_ms = 0.0;
        self.pipeline_ms = 0.0;
        self.interpreter_frame = uber::FrameCounters::default();
        self.frame += 1;
        self.trace_frame_draw = 0;
        self.mvp_capture.end_frame();
    }

    pub(crate) fn discard_batch(&mut self) {
        for pending in &mut self.batch.draws {
            if let Some(geometry) = pending.native_geometry.take() {
                self.native_geometry_pool.push(geometry);
            }
        }
        self.batch = DrawBatch::default();
    }

    pub(crate) fn reset(&mut self) {
        self.compiler = None;
        self.residual_reasons.clear();
        self.clear("device-reset");
        self.discard_batch();
        self.native_geometry_pool.clear();
        self.interpreter_programs.clear();
        self.upload = None;
    }

    pub fn drop_texture(&mut self, texture: &wgpu::Texture) {
        self.bind_groups
            .retain(|key, _| !key.textures.iter().any(|entry| entry.1.as_ref() == Some(texture)));
        self.bind_groups_previous
            .retain(|key, _| !key.textures.iter().any(|entry| entry.1.as_ref() == Some(texture)));
    }

    fn source(
        &mut self,
        key: ShaderVariantKey,
        prepare: impl FnOnce() -> Result<ShaderRecipe, RenderError>,
    ) -> Result<Arc<CachedShaderSource>, RenderError> {
        if let Some(source) = self.shader_variants.get(&key) {
            return Ok(Arc::clone(source));
        }
        let recipe = prepare()?;
        let source = recipe.render()?;
        if trace_compilation() {
            eprintln!(
                "[d3d9-compile] source frame={} origin={:?} pixel={} variant={:016x} source={:016x} bytes={}",
                self.frame,
                key.shader,
                key.pixel,
                fingerprint(&key),
                fingerprint(&source),
                source.len()
            );
        }
        let texture_stages = (0..16)
            .filter(|stage| source.contains(&format!("var tx{stage}:")))
            .collect();
        let cached = Arc::new(CachedShaderSource {
            source,
            recipe,
            texture_stages,
            module: std::sync::OnceLock::new(),
        });
        let bytes = cached.source.len() + cached.recipe.retained_bytes();
        let evicted = self.shader_variants.insert(key, Arc::clone(&cached), bytes);
        warn_cache_eviction("shader-variants", evicted);
        Ok(cached)
    }

    fn sampler(
        &mut self,
        gpu: &D3d9Gpu,
        key: SamplerKey,
        descriptor: wgpu::SamplerDescriptor<'static>,
    ) -> Result<wgpu::Sampler, RenderError> {
        if let Some(sampler) = self.samplers.get(&key) {
            return Ok(sampler.clone());
        }
        let sampler = gpu.validate("Draw", || Ok(gpu.device.create_sampler(&descriptor)))?;
        self.samplers.insert(key, sampler.clone(), 0);
        Ok(sampler)
    }

    fn variant_shader(
        &mut self,
        gpu: &D3d9Gpu,
        cached: &CachedShaderSource,
    ) -> Result<wgpu::ShaderModule, RenderError> {
        if let Some(module) = cached.module.get() {
            return Ok(module.clone());
        }
        let module = self.shader(gpu, &cached.source)?;
        let _ = cached.module.set(module.clone());
        Ok(module)
    }

    fn shader(&mut self, gpu: &D3d9Gpu, source: &str) -> Result<wgpu::ShaderModule, RenderError> {
        if let Some(shader) = self.shaders.get(source) {
            return Ok(shader.clone());
        }
        let started = Instant::now();
        wgsl::validate(&source)?;
        let shader = gpu.validate("Draw", || {
            Ok(gpu
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("d3d9-cached"),
                    source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(&source)),
                }))
        })?;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.module_ms += ms;
        if trace_compilation() || ms >= 50.0 {
            eprintln!(
                "[d3d9-compile] module frame={} source={:016x} bytes={} ms={ms:.3}",
                self.frame,
                fingerprint(&source),
                source.len()
            );
        }
        let evicted = self
            .shaders
            .insert(source.to_owned(), shader.clone(), source.len());
        warn_cache_eviction("shader-modules", evicted);
        Ok(shader)
    }

    fn layout(
        &mut self,
        gpu: &D3d9Gpu,
        key: LayoutKey,
    ) -> Result<wgpu::BindGroupLayout, RenderError> {
        if let Some(layout) = self.layouts.get(&key) {
            return Ok(layout.clone());
        }
        let entries = key.entries();
        let layout = gpu.validate("Draw", || {
            Ok(gpu
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("d3d9-cached"),
                    entries: &entries,
                }))
        })?;
        self.layouts.insert(key, layout.clone(), 0);
        Ok(layout)
    }

    fn pipeline(
        &mut self,
        gpu: &D3d9Gpu,
        key: PipelineKey,
        recipe: PipelineRecipe,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        if let Some((memo, pipeline)) = &self.pipeline_memo {
            if *memo == key {
                return Ok(pipeline.clone());
            }
        }
        let pipeline = match self.pipelines.get(&recipe.key()) {
            Some((compiled, _)) => compiled.pipeline.clone(),
            None => self.compile_pipeline(gpu, key.clone(), recipe)?,
        };
        self.pipeline_memo = Some((key, pipeline.clone()));
        Ok(pipeline)
    }

    fn compile_pipeline(
        &mut self,
        gpu: &D3d9Gpu,
        key: PipelineKey,
        recipe: PipelineRecipe,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let (recipe_id, sources, layout) = trace::recipe(self, &key, &recipe);
        if trace_compilation() {
            eprintln!(
                "[d3d9-compile] pipeline-begin frame={} recipe={recipe_id:016x} sources={sources:x?} layout={layout:x?} cached={}",
                self.frame,
                self.pipelines.len()
            );
        }
        let started = Instant::now();
        let result = gpu.validate("Draw", || {
            Ok(self.create_pipeline(gpu, key.clone(), recipe))
        });
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.pipeline_ms += ms;
        if trace_compilation() || ms >= 50.0 || result.is_err() {
            eprintln!(
                "[d3d9-compile] pipeline-end frame={} recipe={recipe_id:016x} sources={sources:x?} layout={layout:x?} ms={ms:.3} ok={} cached={} buffers={:016x} vertex_layouts={:?} target={:?} primitive={:?} depth={:?} multisample={:?}",
                self.frame,
                result.is_ok(),
                self.pipelines.len(),
                fingerprint(&key.buffers),
                key.buffers,
                key.target,
                key.primitive,
                key.depth,
                key.multisample
            );
        }
        result
    }

    fn create_pipeline(
        &mut self,
        gpu: &D3d9Gpu,
        key: PipelineKey,
        recipe: PipelineRecipe,
    ) -> wgpu::RenderPipeline {
        let pipeline = recipe
            .prepare(key.modules.clone(), key.layout.clone())
            .create(&gpu.device, "d3d9-cached");
        self.cache_pipeline(
            recipe,
            CompiledPipeline {
                pipeline: pipeline.clone(),
                modules: key.modules,
                layout: key.layout,
            },
        );
        pipeline
    }
    fn cache_pipeline(&mut self, recipe: PipelineRecipe, compiled: CompiledPipeline) {
        let bytes = recipe.retained_bytes() * 2;
        let evicted = self
            .pipelines
            .insert(recipe.key(), (compiled, Arc::new(recipe)), bytes);
        warn_cache_eviction("pipelines", evicted);
    }
}
fn texture_dimension(t: &BoundTexture) -> wgpu::TextureViewDimension {
    if t.cube {
        wgpu::TextureViewDimension::Cube
    } else if t.volume {
        wgpu::TextureViewDimension::D3
    } else {
        wgpu::TextureViewDimension::D2
    }
}
fn opaque_format(format: u32) -> bool {
    matches!(format, 20 | 22 | 23 | 24 | 27 | 30 | 33 | 50)
}
fn translated_settings(
    program: &wgsl::Program,
    state: &D3d9State,
    textures: &[BoundTexture],
    raw_inputs: u16,
) -> Result<ProgramSettings, RenderError> {
    let mut variant = wgsl::Variant::default();
    variant.raw_inputs = raw_inputs;
    for stage in 0..16 {
        if state.texture(stage)? == 0 {
            variant.unbound |= 1 << stage;
        }
    }
    let mut opaque = 0u32;
    for t in textures.iter().filter(|t| program.samples(t.stage)) {
        if t.cube {
            variant.cube |= 1 << t.stage;
        }
        if t.volume {
            variant.volume |= 1 << t.stage;
        }
        if t.depth {
            if t.cube || t.volume {
                return Err(unsupported("Draw", "cube/volume depth sampling"));
            }
            variant.depth |= 1 << t.stage;
        }
        if opaque_format(t.format) {
            opaque |= 1 << t.stage;
        }
        if program.pixel
            && program.major == 1
            && program.minor <= 3
            && t.stage < 8
            && tss(state, t.stage, 24) & 256 != 0
        {
            variant.projected |= 1 << t.stage;
        }
        if let Some(kind) = program.samplers.get(&t.stage) {
            if (*kind == 3) != t.cube || (*kind == 4) != t.volume {
                return Err(invalid(
                    "Draw",
                    format!("sampler {} texture dimension mismatch", t.stage),
                ));
            }
        }
    }
    let sampled = program.sampled_slots();
    let bound = sampled & !variant.unbound;
    variant.unbound &= sampled;
    variant.cube &= bound;
    variant.volume &= bound;
    variant.depth &= bound;
    variant.projected &= bound;
    opaque &= u32::from(bound);
    if program.major >= 2 {
        variant.cube = 0;
        variant.volume = 0;
        variant.projected = 0;
    }
    variant.raw_inputs &= if program.pixel {
        0
    } else {
        program
            .inputs
            .iter()
            .filter(|input| input.ty == crate::shader::program::R_INPUT)
            .fold(0, |mask, input| {
                mask | 1u16.checked_shl(input.index).unwrap_or(0)
            })
    };
    let flat_shading = rs(state, 9) == 1;
    Ok(ProgramSettings {
        variant,
        opaque_slots: opaque as u16,
        flat_shading,
        pixel_output: program.pixel,
    })
}

fn translated_snapshot(
    shader: ShaderOrigin,
    program: &wgsl::Program,
    state: &D3d9State,
    textures: &[BoundTexture],
    raw_inputs: u16,
) -> Result<(ShaderVariantKey, ProgramSettings), RenderError> {
    let settings = translated_settings(program, state, textures, raw_inputs)?;
    let variant = &settings.variant;
    let key = ShaderVariantKey {
        pixel: program.pixel,
        shader,
        state: KeyWords::from_slice(&[
            u32::from(variant.unbound),
            u32::from(variant.cube),
            u32::from(variant.volume),
            u32::from(variant.depth),
            u32::from(variant.projected),
            u32::from(settings.opaque_slots),
            u32::from(settings.flat_shading),
            u32::from(variant.raw_inputs),
        ]),
    };
    Ok((key, settings))
}

fn fixed_function_key(
    pixel: bool,
    state: &D3d9State,
    textures: &[BoundTexture],
) -> ShaderVariantKey {
    let head = [rs(state, 9), rs(state, 29), rs(state, 60)];
    if !pixel {
        return ShaderVariantKey {
            pixel,
            shader: ShaderOrigin::FixedFunction,
            state: KeyWords::from_slice(&head),
        };
    }
    let mut values = Vec::with_capacity(92 + textures.len() * 5);
    values.extend_from_slice(&head);
    values.push(u32::from(state.shader(false) != 0));
    for stage in 0..8 {
        for id in [1, 2, 3, 4, 5, 6, 24, 26, 27, 28, 32] {
            values.push(tss(state, stage, id));
        }
    }
    for texture in textures {
        values.extend_from_slice(&[
            texture.stage,
            texture.format,
            texture.depth as u32,
            texture.cube as u32,
            texture.volume as u32,
        ]);
    }
    ShaderVariantKey {
        pixel,
        shader: ShaderOrigin::FixedFunction,
        state: KeyWords::Heap(values),
    }
}

fn resolve_shading(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    vs: Option<&super::shader::ShaderRecord>,
    ps: Option<&super::shader::ShaderRecord>,
    ff_key: Option<[u32; 5]>,
    ff_uniform_count: u32,
    snapshot: &DrawSnapshot,
    target: (wgpu::TextureFormat, u32),
    depth: Option<(wgpu::TextureFormat, u32)>,
    raw_inputs: u16,
) -> Result<Box<DrawMemo>, RenderError> {
    let (vertex_key, vertex_settings) = if let Some(v) = vs {
        let (key, settings) = translated_snapshot(
            ShaderOrigin::Guest(v.key.clone()),
            &v.program,
            state,
            &snapshot.textures,
            raw_inputs,
        )?;
        (key, Some(settings))
    } else if let Some(key) = &ff_key {
        (
            ShaderVariantKey {
                pixel: false,
                shader: ShaderOrigin::DeclaredFixedFunction,
                state: KeyWords::from_slice(&[
                    key[0],
                    key[1],
                    key[2],
                    key[3],
                    key[4],
                    rs(state, 9),
                ]),
            },
            None,
        )
    } else {
        (fixed_function_key(false, state, &snapshot.textures), None)
    };
    let trace_vertex = trace_compilation().then(|| trace::stage(&vertex_key));
    let vertex_source = draw.source(vertex_key, || {
        if let (Some(v), Some(settings)) = (vs, vertex_settings) {
            return Ok(ShaderRecipe::Program {
                key: v.key.clone(),
                program: Arc::clone(&v.program),
                settings,
            });
        }
        let source = if let Some(key) = &ff_key {
            ffvs::source(key)
        } else {
            ff_vertex_source()
        };
        Ok(ShaderRecipe::Source(
            shading_source(source, rs(state, 9) == 1).into(),
        ))
    })?;
    let (pixel_key, pixel_settings) = if let Some(p) = ps {
        let (key, settings) = translated_snapshot(
            ShaderOrigin::Guest(p.key.clone()),
            &p.program,
            state,
            &snapshot.textures,
            0,
        )?;
        (key, Some(settings))
    } else {
        (fixed_function_key(true, state, &snapshot.textures), None)
    };
    let trace_pixel = trace_compilation().then(|| trace::stage(&pixel_key));
    let pixel_source = draw.source(pixel_key, || {
        if let (Some(p), Some(settings)) = (ps, pixel_settings) {
            return Ok(ShaderRecipe::Program {
                key: p.key.clone(),
                program: Arc::clone(&p.program),
                settings,
            });
        }
        Ok(ShaderRecipe::Source(
            shading_source(
                ff_fragment_source(state, &snapshot.textures)?,
                rs(state, 9) == 1,
            )
            .into(),
        ))
    })?;
    if vs.is_some() && rs(state, 28) != 0 && rs(state, 35) != 0 {
        return Err(unsupported(
            "Draw",
            "D3DRS_FOGTABLEMODE with translated vertex shaders is deferred",
        ));
    }
    if rs(state, 152) != 0 {
        return Err(unsupported(
            "Draw",
            "D3DRS_CLIPPLANEENABLE is not implemented",
        ));
    }
    for &stage in &pixel_source.texture_stages {
        if !snapshot.textures.iter().any(|t| t.stage == stage) {
            return Err(unsupported(
                "Draw",
                format!("shader samples unbound texture stage {stage}"),
            ));
        }
    }
    let textures: Vec<&BoundTexture> = snapshot
        .textures
        .iter()
        .filter(|t| pixel_source.texture_stages.contains(&t.stage))
        .collect();
    let mut bindings = Vec::with_capacity(textures.len());
    for texture in &textures {
        let (key, descriptor) = sampler(gpu, state, texture)?;
        bindings.push(PendingTexture {
            stage: texture.stage,
            texture: Some(texture.texture.clone()),
            view: texture.view.clone(),
            sampler: draw.sampler(gpu, key, descriptor)?,
        });
    }
    let vs_count = match (&vs, ff_key.is_some()) {
        (Some(v), _) => v.program.const_limit(),
        (None, true) => ff_uniform_count,
        (None, false) => 1,
    };
    let ps_count = ps.map(|p| p.program.const_limit()).unwrap_or(1);
    let mut texture_layouts = [None; MAX_SAMPLERS];
    for t in &textures {
        let format = super::format::device_format(t.format, gpu.features())
            .ok_or_else(|| unsupported("Draw", "texture format"))?;
        let filterable = format
            .guaranteed_format_features(gpu.features())
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE);
        if !t.depth
            && !filterable
            && [5, 6, 7]
                .iter()
                .any(|&id| matches!(ss(state, t.stage, id), 2 | 3))
        {
            return Err(unsupported(
                "Draw",
                format!("non-filterable sampler format {}", t.format),
            ));
        }
        texture_layouts[t.stage as usize] = Some(TextureLayout {
            depth: t.depth,
            filterable,
            dimension: texture_dimension(t),
        });
    }
    let layout_key = LayoutKey::Draw {
        vs: vs_count,
        ps: ps_count,
        textures: texture_layouts,
    };
    let layout = draw.layout(gpu, layout_key)?;
    let trace_interface = trace_vertex
        .zip(trace_pixel)
        .map(|(vertex, pixel)| trace::Interface {
            stages: [vertex, pixel],
            layout: layout_key.entries().into(),
            inputs: vs
                .map(|v| {
                    v.program
                        .inputs
                        .iter()
                        .map(|i| (i.usage, i.usage_index, i.ty, i.index))
                        .collect()
                })
                .unwrap_or_default(),
        });
    Ok(Box::new(DrawMemo {
        fingerprint: state.pipeline_fingerprint(),
        ff_key,
        raw_inputs,
        target,
        depth,
        textures: snapshot.textures.clone(),
        bindings: bindings.into(),
        vs_count,
        ps_count,
        programs: None,
        modules: [
            draw.variant_shader(gpu, &vertex_source)?,
            draw.variant_shader(gpu, &pixel_source)?,
        ],
        recipes: [vertex_source.recipe.clone(), pixel_source.recipe.clone()],
        binding_entries: layout_key.entries().into(),
        layout,
        trace_interface,
        trace_pipeline: None,
        pipeline: None,
    }))
}

fn fill_mode(gpu: &D3d9Gpu, state: &D3d9State) -> Result<wgpu::PolygonMode, RenderError> {
    let (mode, feature) = match rs(state, 8) {
        1 => (wgpu::PolygonMode::Point, wgpu::Features::POLYGON_MODE_POINT),
        2 => (wgpu::PolygonMode::Line, wgpu::Features::POLYGON_MODE_LINE),
        3 => return Ok(wgpu::PolygonMode::Fill),
        v => {
            return Err(unsupported(
                "Draw",
                format!("D3DRS_FILLMODE value {v} is not implemented"),
            ));
        }
    };
    if !gpu.features().contains(feature) {
        return Err(unsupported(
            "Draw",
            format!("D3DRS_FILLMODE value {} requires {feature:?}", rs(state, 8)),
        ));
    }
    Ok(mode)
}

fn pipeline_descriptor(
    gpu: &D3d9Gpu,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    topology: wgpu::PrimitiveTopology,
    strip_index_format: Option<wgpu::IndexFormat>,
) -> Result<PipelineDescriptor, RenderError> {
    let cull = match rs(state, 22) {
        1 => None,
        2 => Some(wgpu::Face::Front),
        _ => Some(wgpu::Face::Back),
    };
    let target = wgpu::ColorTargetState {
        format: snapshot.target.texture.format(),
        blend: blend_state(state, opaque_format(snapshot.target.format))?,
        write_mask: wgpu::ColorWrites::from_bits_truncate(rs(state, 168)),
    };
    Ok(PipelineDescriptor {
        entry_points: ["vs_main", "ps_main"],
        target: Some(target),
        primitive: wgpu::PrimitiveState {
            topology,
            strip_index_format,
            front_face: wgpu::FrontFace::Cw,
            cull_mode: cull,
            polygon_mode: fill_mode(gpu, state)?,
            ..Default::default()
        },
        depth: depth_state(state, snapshot.depth.as_ref())?,
        multisample: wgpu::MultisampleState {
            mask: u64::from(rs(state, 162)),
            ..Default::default()
        },
    })
}

pub(crate) fn execute(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    queries: &mut D3d9Queries,
    state: &D3d9State,
    objects: &super::shader::D3d9Objects,
    snapshot: &DrawSnapshot,
) -> Result<(), RenderError> {
    let draw_started = trace_compilation().then(Instant::now);
    if let Some(compiler) = &mut draw.compiler {
        let ready = compiler.publish(draw.frame)?;
        if !ready.is_empty() {
            draw.pipeline_memo = None;
        }
        for (recipe, compiled) in ready {
            draw.cache_pipeline(recipe, compiled);
        }
    }
    if trace_compilation() && !draw.trace_identity {
        eprintln!(
            "[d3d9-compile] identity schema=2 build={} limits={:?} features={:?}",
            env!("RECOMP_RENDERER_BUILD"),
            gpu.limits(),
            gpu.features()
        );
        draw.trace_identity = true;
    }
    if snapshot.command.primitive_count == 0 {
        return Ok(());
    }
    if !matches!(rs(state, 9), 1 | 2) {
        return Err(unsupported(
            "Draw",
            format!("D3DRS_SHADEMODE value {} is not implemented", rs(state, 9)),
        ));
    }
    for (id, default) in [
        (48, 0),
        (136, 1),
        (163, 0),
        (170, 0),
        (178, 1.0f32.to_bits()),
        (179, 1.0f32.to_bits()),
        (180, 0),
        (181, 0),
        (183, 0),
        (184, 0),
    ] {
        if rs(state, id) != default {
            return Err(unsupported(
                "Draw",
                format!(
                    "render state {id} value {} is not implemented",
                    rs(state, id)
                ),
            ));
        }
    }
    for id in (128..136).chain(198..206) {
        if rs(state, id) != 0 {
            return Err(unsupported(
                "Draw",
                format!("texture coordinate wrap render state {id}"),
            ));
        }
    }
    let declaration = objects.declaration(state.declaration())?;
    let vs = if state.shader(false) == 0 {
        None
    } else {
        Some(objects.shader(false, state.shader(false))?)
    };
    let ps = if state.shader(true) == 0 {
        None
    } else {
        Some(objects.shader(true, state.shader(true))?)
    };
    if trace_compilation() {
        for shader in [vs, ps].into_iter().flatten() {
            if !draw.inventory_programs.contains(shader.code.as_ref()) {
                draw.inventory_programs.insert(Arc::clone(&shader.code));
                let p = &shader.program;
                let mut opcodes = std::collections::BTreeMap::<u32, u32>::new();
                let mut reads = std::collections::BTreeMap::<u32, u32>::new();
                let mut writes = std::collections::BTreeMap::<u32, u32>::new();
                let mut modifiers = std::collections::BTreeSet::new();
                let mut shifts = std::collections::BTreeSet::new();
                let mut controls = std::collections::BTreeSet::new();
                for inst in &p.code {
                    *opcodes.entry(inst.op).or_default() += 1;
                    shifts.insert(inst.dst.shift);
                    controls.insert((inst.op, inst.ctrl));
                    for src in &inst.src[..inst.nsrc as usize] {
                        reads
                            .entry(src.ty)
                            .and_modify(|n| *n = (*n).max(src.index))
                            .or_insert(src.index);
                        modifiers.insert(src.md);
                    }
                    writes
                        .entry(inst.dst.ty)
                        .and_modify(|n| *n = (*n).max(inst.dst.index))
                        .or_insert(inst.dst.index);
                }
                eprintln!(
                    "[d3d9-compile] program code={:016x} model={} words={} instructions={} opcodes={opcodes:?} source_max_indices={reads:?} encoded_dst_max_indices={writes:?} modifiers={modifiers:?} shifts={shifts:?} controls={controls:?} saturate={} coissue={} relative={} inputs={:?} samplers={:?} float_bank={} defs={} idefs={} bdefs={}",
                    fingerprint(&shader.code),
                    p.version(),
                    shader.code.len(),
                    p.code.len(),
                    p.code.iter().any(|i| i.dst.saturate),
                    p.code.iter().any(|i| i.coissue),
                    p.relative,
                    p.inputs,
                    p.samplers,
                    p.const_limit(),
                    p.defs.len(),
                    p.idefs.len(),
                    p.bdefs.len()
                );
            }
        }
        eprintln!(
            "[d3d9-compile] draw-programs frame={} draw={} vs={:016x?} ps={:016x?}",
            draw.frame,
            draw.measured_draws,
            vs.map(|s| fingerprint(&s.code)),
            ps.map(|s| fingerprint(&s.code))
        );
    }
    let selection = uber::select(gpu, draw, state, snapshot, vs, ps)?;
    let covered = selection.family.is_some();
    let geometry_started = trace_compilation().then(Instant::now);
    let mut linked_inputs = std::mem::take(&mut draw.linked);
    let mut ff_uniforms = std::mem::take(&mut draw.ff_uniforms);
    let ff_key = if vs.is_none() {
        ffvs::prepare(state, declaration, &mut linked_inputs, &mut ff_uniforms)?
    } else {
        None
    };
    let linked = match vs {
        Some(v) => {
            link_inputs(declaration, &v.program, &mut linked_inputs)?;
            if covered {
                linked_inputs.sort_by_key(|input| input.location);
            }
            Some(linked_inputs.as_slice())
        }
        None => ff_key.is_some().then_some(linked_inputs.as_slice()),
    };
    let hybrid = covered && uber::hybrid(gpu);
    let native_plan = linked
        .filter(|_| !covered || hybrid)
        .map(|linked| plan_native_geometry(gpu, draw, &snapshot.command, linked, vs.is_some()))
        .transpose()?
        .flatten();
    let mut raw_inputs = 0u16;
    if native_plan.is_some() {
        for input in linked.into_iter().flatten() {
            if input.element_type == D3DDECLTYPE_UBYTE4 {
                raw_inputs |= 1u16
                    .checked_shl(input.location)
                    .ok_or_else(|| invalid("Draw", "raw byte vertex input location exceeds 15"))?;
            }
        }
    }
    let specialization_started = trace_compilation().then(Instant::now);
    let ready = if hybrid {
        let (buffers, topology, strip_format) = if let Some(plan) = &native_plan {
            (plan.layouts.clone(), plan.topology, plan.strip_format)
        } else {
            let (attributes, stride) = packed_shader_layout(linked.unwrap(), false)?;
            let buffers = smallvec![(
                stride,
                attributes
                    .iter()
                    .map(LayoutAttribute::new)
                    .collect::<Result<_, _>>()?
            )];
            (buffers, uber::topology(snapshot), None)
        };
        uber::specialize(
            gpu,
            draw,
            state,
            snapshot,
            vs.unwrap(),
            ps.unwrap(),
            buffers,
            topology,
            strip_format,
            raw_inputs,
        )?
    } else {
        None
    };
    let specialization_ms =
        specialization_started.map_or(0.0, |started| started.elapsed().as_secs_f64() * 1000.0);
    draw.specialization_cpu_ms += specialization_ms;
    let interpreted = covered && ready.is_none();
    let canonical = interpreted;
    let direct = match native_plan {
        Some(plan) if !interpreted => {
            Some(upload_native_geometry(gpu, draw, &snapshot.command, plan)?)
        }
        Some(mut plan) => {
            plan.streams.clear();
            draw.native_streams = plan.streams;
            None
        }
        None => None,
    };
    if interpreted {
        raw_inputs = 0;
    }
    let (mut native_geometry, vertices, order, draw_indices, topology) =
        if let Some(geometry) = direct {
            let topology = geometry.topology;
            (
                Some(geometry),
                PackedVertices {
                    bytes: Vec::new(),
                    attributes: Vec::new(),
                    stride: 0,
                },
                Vec::new(),
                None,
                topology,
            )
        } else {
            let (order, draw_indices, _, topology) = expanded_indices(
                &snapshot.command,
                &mut draw.index_remap,
                &mut draw.index_slots,
            )?;
            let vertices = if let Some(linked) = linked {
                pack_shader_vertices(&snapshot.command, linked, &order, canonical)?
            } else {
                PackedVertices {
                    bytes: ff_vertices(&snapshot.command, declaration, &order, state)?,
                    attributes: (0..12)
                        .map(|i| wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: u64::from(i) * 16,
                            shader_location: i,
                        })
                        .collect(),
                    stride: 192,
                }
            };
            (None, vertices, order, draw_indices, topology)
        };
    if let Some(started) = geometry_started {
        let ms = (started.elapsed().as_secs_f64() * 1000.0 - specialization_ms).max(0.0);
        draw.geometry_cpu_ms += ms;
        if interpreted {
            draw.interpreter_frame.packing_ms += ms;
        }
        draw.packed_bytes += vertices.bytes.len() as u64;
    }
    if interpreted {
        draw.interpreter_frame.packed_bytes += vertices.bytes.len() as u64;
    }
    let trace_this = super::trace_enabled()
        && trace_selected(draw.frame, draw.trace_frame_draw, draw.trace_total_draw);
    let count = draw.trace_total_draw;
    if super::trace_enabled() {
        draw.trace_frame_draw = draw.trace_frame_draw.saturating_add(1);
        draw.trace_total_draw = draw.trace_total_draw.saturating_add(1);
    }
    if trace_this {
        let command = &snapshot.command;
        eprintln!(
            "[d3d9-trace] begin frame={} draw={count} frame_draw={} primitive={} primitives={} indexed={} base_vertex={} start_vertex={} min_vertex={} num_vertices={} index_format={} index_bytes={} index_hash={:016x} target={:?} subresource={:?} depth={:?} native={}",
            draw.frame,
            draw.trace_frame_draw - 1,
            command.primitive,
            command.primitive_count,
            command.indexed,
            command.base_vertex,
            command.start_vertex,
            command.min_vertex,
            command.num_vertices,
            command.index_format,
            command.index_bytes.len(),
            fingerprint(&command.index_bytes),
            snapshot.target.view,
            snapshot.target.subresource,
            snapshot
                .depth
                .as_ref()
                .map(|d| (&d.view, d.width, d.height, d.format)),
            native_geometry.is_some()
        );
        for (pixel, shader) in [(false, vs), (true, ps)] {
            if let Some(shader) = shader {
                let id = state.shader(pixel);
                eprintln!(
                    "[d3d9-trace] shader pixel={pixel} id={id} code_hash={:016x} constants={}",
                    fingerprint(&shader.code),
                    shader.program.const_limit()
                );
                if draw.trace_shaders.insert(id) {
                    eprintln!(
                        "[d3d9-trace] shader_code id={id} words={:08x?}\n[d3d9-trace] shader_wgsl id={id}\n{}",
                        shader.code, shader.wgsl
                    );
                }
            }
            let constants = state.constants_f(
                pixel,
                0,
                if pixel {
                    super::state::PS_CONST_F
                } else {
                    super::state::VS_CONST_F
                } as u32,
            )?;
            eprintln!(
                "[d3d9-trace] constants pixel={pixel} hash={:016x}",
                fingerprint(&bytemuck::cast_slice::<f32, u32>(&constants))
            );
            for (register, value) in constants.chunks_exact(4).enumerate() {
                if value.iter().any(|v| v.to_bits() != 0) {
                    eprintln!("[d3d9-trace] constant pixel={pixel} c{register}={value:?}");
                }
            }
        }
        eprintln!("[d3d9-trace] indices={:?}", &order[..order.len().min(12)]);
        if let Some(geometry) = &native_geometry {
            eprintln!(
                "[d3d9-trace] native layouts={:?} topology={:?} index_offset={} index_size={} base_vertex={} count={}",
                geometry.layouts,
                geometry.topology,
                geometry.index_offset,
                geometry.index_size,
                geometry.base_vertex,
                geometry.count
            );
            let samples =
                primitive_vertex_count(command.primitive, command.primitive_count)?.min(6);
            for index in command.start_vertex..command.start_vertex.saturating_add(samples) {
                let raw = indexed_vertex_index(
                    &command.index_bytes,
                    command.index_format,
                    index as usize,
                );
                let absolute = (i64::from(raw) + i64::from(command.base_vertex)) as u32;
                for element in linked.into_iter().flatten() {
                    let stream = stream_for(command, u32::from(element.stream))?;
                    let Some(size) = element_size(element.element_type) else {
                        continue;
                    };
                    let bytes = vertex_element_bytes(
                        &stream.bytes,
                        stream.stream,
                        stream.stride,
                        stream.offset,
                        u64::from(element.offset),
                        size as usize,
                        absolute,
                    )?;
                    eprintln!(
                        "[d3d9-trace] fetched raw_index={raw} vertex={absolute} location={} value={:?}",
                        element.location,
                        expand_element(element.element_type, bytes)?
                    );
                }
            }
        }
        for stream in &snapshot.command.streams {
            eprintln!(
                "[d3d9-trace] stream={} bytes={} hash={:016x}",
                stream.stream,
                stream.bytes.len(),
                fingerprint(&stream.bytes)
            );
            eprintln!(
                "[d3d9-trace] stream={} offset={} stride={} sample={:?}",
                stream.stream,
                stream.offset,
                stream.stride,
                stream
                    .bytes
                    .get(
                        stream.offset as usize..stream.bytes.len().min(stream.offset as usize + 96)
                    )
                    .unwrap_or(&[])
            );
        }
        eprintln!(
            "[d3d9-trace] draw={} target={}x{} format={} viewport={:?} vs={} ps={} declaration={:?}",
            count,
            snapshot.target.width,
            snapshot.target.height,
            snapshot.target.format,
            [
                state.viewport().x as f32,
                state.viewport().y as f32,
                state.viewport().width as f32,
                state.viewport().height as f32,
                state.viewport().min_z,
                state.viewport().max_z
            ],
            state.shader(false),
            state.shader(true),
            declaration
        );
        for id in [7, 14, 15, 19, 20, 22, 23, 24, 25, 27, 137, 162, 168, 174] {
            eprintln!("[d3d9-trace] rs[{id}]={}", rs(state, id));
        }
        for texture in &snapshot.textures {
            if texture.stage >= 8 {
                eprintln!(
                    "[d3d9-trace] sampler={} id={} size={}x{} format={} lod={}",
                    texture.stage,
                    state.texture(texture.stage)?,
                    texture.width,
                    texture.height,
                    texture.format,
                    texture.lod
                );
                continue;
            }
            eprintln!(
                "[d3d9-trace] stage={} id={} size={}x{} format={} lod={} color_op={} color_args={}/{} alpha_op={} alpha_args={}/{}",
                texture.stage,
                state.texture(texture.stage)?,
                texture.width,
                texture.height,
                texture.format,
                texture.lod,
                tss(state, texture.stage, 1),
                tss(state, texture.stage, 2),
                tss(state, texture.stage, 3),
                tss(state, texture.stage, 4),
                tss(state, texture.stage, 5),
                tss(state, texture.stage, 6)
            );
            eprintln!(
                "[d3d9-trace] stage={} coord_index={:#x} transform_flags={:#x} matrix={:?} address={:?}",
                texture.stage,
                tss(state, texture.stage, 11),
                tss(state, texture.stage, 24),
                state.transform(16 + texture.stage)?,
                [
                    ss(state, texture.stage, 1),
                    ss(state, texture.stage, 2),
                    ss(state, texture.stage, 3)
                ]
            );
        }
        eprintln!(
            "[d3d9-trace] vertex_stride={} packed_bytes={}",
            vertices.stride,
            vertices.bytes.len()
        );
        for vertex in vertices
            .bytes
            .chunks((vertices.stride as usize).max(1))
            .take(6)
        {
            let values = if let Some(linked) = &linked {
                linked
                    .iter()
                    .zip(&vertices.attributes)
                    .map(|(input, attribute)| {
                        let element_type =
                            if !canonical && native_vertex_format(input.element_type).is_some() {
                                input.element_type
                            } else {
                                D3DDECLTYPE_FLOAT4
                            };
                        expand_element(element_type, &vertex[attribute.offset as usize..])
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                vertex
                    .chunks_exact(16)
                    .map(|bytes| expand_element(D3DDECLTYPE_FLOAT4, bytes))
                    .collect::<Result<Vec<_>, _>>()?
            };
            eprintln!("[d3d9-trace] vertex={values:?}");
        }
    }
    let target_key = (snapshot.target.texture.format(), snapshot.target.format);
    let depth_key = snapshot
        .depth
        .as_ref()
        .map(|depth| (depth.texture.format(), depth.format));
    let fingerprint = state.pipeline_fingerprint();
    let memo = draw
        .memos
        .iter()
        .rposition(|memo| {
            memo.fingerprint == fingerprint
                && memo.programs.is_some() == interpreted
                && (!interpreted
                    || memo
                        .pipeline
                        .as_ref()
                        .is_some_and(|(shape, _)| shape.1 == topology))
                && ready.as_ref().is_none_or(|(compiled, _)| {
                    memo.pipeline
                        .as_ref()
                        .is_some_and(|(_, pipeline)| *pipeline == compiled.pipeline)
                })
                && memo.ff_key == ff_key
                && memo.raw_inputs == raw_inputs
                && memo.target == target_key
                && memo.depth == depth_key
                && memo.textures == snapshot.textures
        })
        .map(|index| draw.memos.remove(index));
    let mut memo = match memo {
        Some(memo) => memo,
        None if covered => uber::resolve(
            gpu,
            draw,
            state,
            snapshot,
            vs.ok_or_else(|| unsupported("InterpreterDraw", "missing vertex program"))?,
            ps.ok_or_else(|| unsupported("InterpreterDraw", "missing pixel program"))?,
            ready,
            raw_inputs,
        )?,
        None => resolve_shading(
            gpu,
            draw,
            state,
            vs,
            ps,
            ff_key,
            (ff_uniforms.len() / 4) as u32,
            &snapshot,
            target_key,
            depth_key,
            raw_inputs,
        )?,
    };
    if snapshot.command.primitive == 1
        && (rs(state, 156) != 0 || rs(state, 157) != 0 || f32::from_bits(rs(state, 154)) != 1.0)
    {
        return Err(unsupported(
            "Draw",
            "point size/sprite/scaling state is not implemented",
        ));
    }
    let vp = state.viewport();
    if vp.width == 0 || vp.height == 0 || vp.min_z < 0.0 || vp.max_z > 1.0 || vp.min_z > vp.max_z {
        return Err(unsupported(
            "Draw",
            "viewport depth range cannot be represented by wgpu",
        ));
    }
    let max = u64::from(gpu.device.limits().max_texture_dimension_2d);
    if u64::from(vp.width) > max
        || u64::from(vp.height) > max
        || u64::from(vp.x) + u64::from(vp.width) > 2 * max - 1
        || u64::from(vp.y) + u64::from(vp.height) > 2 * max - 1
    {
        return Err(unsupported(
            "Draw",
            format!(
                "viewport {}x{} at ({}, {}) exceeds wgpu limits for max dimension {max}",
                vp.width, vp.height, vp.x, vp.y
            ),
        ));
    }
    let fog_color = color(rs(state, 34));
    let psp_words: [u32; 12] = [
        (rs(state, 24).min(255) as f32 / 255.0).to_bits(),
        if rs(state, 15) != 0 { rs(state, 25) } else { 8 },
        rs(state, 35),
        rs(state, 28),
        fog_color[0].to_bits(),
        fog_color[1].to_bits(),
        fog_color[2].to_bits(),
        fog_color[3].to_bits(),
        rs(state, 36),
        rs(state, 37),
        rs(state, 38),
        (if rs(state, 194) != 0 { 1.0f32 } else { 0.0f32 }).to_bits(),
    ];
    let parameters = DrawParameters::new(
        [1.0 / vp.width as f32, -1.0 / vp.height as f32, 0.0, 0.0],
        [[1.0; 4]; 16],
        psp_words,
    );
    let parameters = if interpreted {
        let (opaque, unbound) = ps
            .unwrap()
            .interpreter
            .as_ref()
            .map_err(|why| unsupported("InterpreterDraw", why.to_string()))?
            .sampler_mapping
            .iter()
            .copied()
            .fold(
                (0, 0),
                |(opaque, unbound), (guest, physical)| match snapshot
                    .textures
                    .iter()
                    .find(|t| t.stage == guest)
                {
                    Some(texture) => (
                        opaque | (u32::from(opaque_format(texture.format)) << physical),
                        unbound,
                    ),
                    None => (opaque, unbound | (1 << physical)),
                },
            );
        parameters.interpreter_flags(opaque, unbound, uber::tint_enabled())
    } else {
        parameters
    };
    let has_attributes = !vertices.attributes.is_empty();
    let buffers = match native_geometry.as_mut() {
        Some(geometry) => std::mem::take(&mut geometry.layouts),
        None if has_attributes => smallvec![(
            vertices.stride,
            vertices
                .attributes
                .iter()
                .map(LayoutAttribute::new)
                .collect::<Result<_, _>>()?
        )],
        None => SmallVec::new(),
    };
    let strip_index_format = native_geometry.as_ref().and_then(|g| g.strip_format);
    let pipeline = match &memo.pipeline {
        Some((shape, pipeline))
            if shape.0 == buffers && shape.1 == topology && shape.2 == strip_index_format =>
        {
            pipeline.clone()
        }
        _ => {
            if covered {
                return Err(unsupported(
                    if interpreted {
                        "InterpreterDraw"
                    } else {
                        "HybridDraw"
                    },
                    "selected pipeline does not match vertex interface",
                ));
            }
            let descriptor =
                pipeline_descriptor(gpu, state, snapshot, topology, strip_index_format)?;
            let key = PipelineKey {
                entry_points: descriptor.entry_points,
                modules: memo.modules.clone(),
                layout: memo.layout.clone(),
                buffers: buffers.clone(),
                target: descriptor.target,
                primitive: descriptor.primitive,
                depth: descriptor.depth,
                multisample: descriptor.multisample,
            };
            let recipe = PipelineRecipe {
                shaders: memo.recipes.clone(),
                interface: InterfaceKey {
                    bindings: Arc::clone(&memo.binding_entries),
                    vertex_buffers: key.buffers.clone(),
                },
                descriptor: key.descriptor(),
            };
            if let Some(interface) = &memo.trace_interface {
                let family = trace::observe(draw, interface, &key);
                memo.trace_pipeline = Some((family, trace::recipe(draw, &key, &recipe).0));
            }
            let pipeline = draw.pipeline(gpu, key, recipe)?;
            memo.pipeline = Some(((buffers, topology, strip_index_format), pipeline.clone()));
            pipeline
        }
    };
    if let Some((family, recipe)) = memo.trace_pipeline {
        let mode = if interpreted {
            "interpreted"
        } else {
            "specialized"
        };
        let eligible = selection
            .eligible
            .map_or("unknown", |value| if value { "true" } else { "false" });
        let prepared_family = selection.family.unwrap_or(0);
        eprintln!(
            "[d3d9-compile] draw-family frame={} draw={} family={family} recipe={recipe:016x} mode={mode} eligible={eligible} prepared_family={prepared_family:016x}",
            draw.frame, draw.measured_draws
        );
    }
    trace::capture_mvp(gpu, draw, state, snapshot, vs, ps, &pipeline);
    if interpreted {
        draw.interpreter_draws += 1;
        draw.interpreter_frame.draws += 1;
        if trace_compilation() || trace_this {
            eprintln!(
                "[d3d9-uber] draw frame={} draw={} interpreted_draw={} family={:016x} vs={:?} ps={:?} target={:?} subresource={:?} texture0={} pipeline={:?}",
                draw.frame,
                draw.measured_draws,
                draw.interpreter_draws,
                gpu.interpreter
                    .get()
                    .and_then(|p| p.for_pipeline(&pipeline))
                    .map_or(0, |family| family.fingerprint),
                vs.map(|s| &s.key),
                ps.map(|s| &s.key),
                snapshot.target.view,
                snapshot.target.subresource,
                state.texture(0)?,
                pipeline
            );
        }
    }
    if draw
        .batch
        .color_key_view
        .as_ref()
        .is_some_and(|view| *view != snapshot.target.view)
        || draw.batch.depth.as_ref().map(|(view, _)| view)
            != snapshot.depth.as_ref().map(|depth| &depth.view)
    {
        flush(gpu, draw, queries)?;
    }
    let uniform_values = parameters.values(
        if ff_key.is_some() {
            bytemuck::cast_slice(&ff_uniforms)
        } else {
            state.constants_f_bytes(false, memo.vs_count)?
        },
        state.constants_f_bytes(true, memo.ps_count)?,
    );
    if interpreted {
        draw.interpreter_frame.uniform_bytes += uniform_values
            .iter()
            .map(|bytes| bytes.len() as u64)
            .sum::<u64>();
    }
    if trace_this {
        eprintln!(
            "[d3d9-trace] submit frame={} draw={count} vs_count={} ps_count={} uniform_hashes={:016x?} depth={:?} blend={:?} packed_vertex_hash={:016x}",
            draw.frame,
            memo.vs_count,
            memo.ps_count,
            uniform_values.map(|v| self::fingerprint(&v)),
            depth_state(state, snapshot.depth.as_ref())?,
            blend_state(state, opaque_format(snapshot.target.format))?,
            self::fingerprint(&vertices.bytes)
        );
    }
    let alignment = gpu.limits().min_uniform_buffer_offset_alignment.max(1) as usize;
    let uniform_sizes = uniform_values.map(|bytes| bytes.len() as u64);
    let shared_for = |batch: &DrawBatch| -> [Option<u32>; 4] {
        let Some((offsets, sizes)) = batch.last_uniform else {
            return [None; 4];
        };
        std::array::from_fn(|index| {
            let start = offsets[index] as usize;
            (sizes[index] == uniform_sizes[index]
                && batch.data.get(start..start + uniform_values[index].len())
                    == Some(uniform_values[index]))
            .then_some(offsets[index])
        })
    };
    let mut shared_uniforms = shared_for(&draw.batch);
    let compact_indices = order.len() <= u16::MAX as usize + 1;
    let index_stride = if compact_indices { 2 } else { 4 };
    let index_size = draw_indices
        .as_ref()
        .map_or(Some(0usize), |indices| {
            indices.len().checked_mul(index_stride)
        })
        .ok_or_else(|| invalid("Draw", "index upload size overflow"))?;
    let streamed = native_geometry.as_ref().map_or(0, |geometry| {
        let mut size = 0;
        for vertex in &geometry.vertices {
            if matches!(vertex.data, GeometryData::Stream(_)) {
                size += vertex.size as usize + 4;
            }
        }
        if matches!(geometry.index, GeometryData::Stream(_)) {
            size += geometry.index_size as usize + 4;
        }
        size
    });
    let vertex_upload_size = vertices.bytes.len() + streamed;
    let aligned = |value: usize, alignment: usize| {
        value
            .checked_add(alignment - 1)
            .map(|value| value / alignment * alignment)
    };
    let estimate_size = |mut size: usize, shared: [Option<u32>; 4]| -> Option<usize> {
        for (bytes, shared) in uniform_values.iter().zip(shared) {
            if shared.is_none() {
                size = aligned(size, alignment)?.checked_add(bytes.len())?;
            }
        }
        size = aligned(size, 4)?.checked_add(vertex_upload_size)?;
        if index_size != 0 {
            size = aligned(aligned(size, 4)?.checked_add(index_size)?, 4)?;
        }
        Some(size)
    };
    let upload_limit = gpu.limits().max_buffer_size.min(u32::MAX as u64) as usize;
    let estimated = estimate_size(draw.batch.data.len(), shared_uniforms)
        .ok_or_else(|| invalid("Draw", "draw upload size overflow"))?;
    if estimated > upload_limit && !draw.batch.draws.is_empty() {
        flush(gpu, draw, queries)?;
        shared_uniforms = [None; 4];
    }
    let estimated = estimate_size(draw.batch.data.len(), shared_uniforms)
        .ok_or_else(|| invalid("Draw", "draw upload size overflow"))?;
    if estimated > upload_limit {
        return Err(RenderError::out_of_memory(
            "Draw",
            "draw upload exceeds the device buffer limit",
        ));
    }
    if draw.batch.draws.is_empty() {
        draw.batch.color_key_view = Some(snapshot.target.view.clone());
        let padded_size = snapshot.depth.as_ref().map(|depth| {
            (
                depth.width.max(snapshot.target.width),
                depth.height.max(snapshot.target.height),
            )
        });
        if padded_size.is_some_and(|(width, height)| {
            width != snapshot.target.width || height != snapshot.target.height
        }) {
            let (width, height) = padded_size.unwrap();
            let format = snapshot.target.texture.format();
            let (texture, view) = gpu.validate("DrawTargetPadding", || {
                let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("d3d9-padded-color-target"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                Ok((texture, view))
            })?;
            let source = snapshot.target.clone();
            gpu.record(|encoder, _| {
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &source.texture,
                        mip_level: source.subresource.1,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: source.subresource.0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: source.width,
                        height: source.height,
                        depth_or_array_layers: 1,
                    },
                );
            });
            draw.batch.color_view = Some(view);
            draw.batch.padded_color = Some(PaddedColor { texture, source });
        } else {
            draw.batch.color_view = Some(snapshot.target.view.clone());
            draw.batch.padded_color = None;
        }
        draw.batch.depth = snapshot
            .depth
            .as_ref()
            .map(|depth| (depth.view.clone(), super::format::has_stencil(depth.format)));
        draw.batch.width = snapshot.target.width;
        draw.batch.height = snapshot.target.height;
    }
    let mut uniform_offsets = [0u32; 4];
    for (index, bytes) in uniform_values.iter().enumerate() {
        uniform_offsets[index] = match shared_uniforms[index] {
            Some(offset) => offset,
            None => {
                let offset = draw.batch.data.len().next_multiple_of(alignment);
                draw.batch.data.resize(offset, 0);
                draw.batch.data.extend_from_slice(bytes);
                u32::try_from(offset)
                    .map_err(|_| invalid("Draw", "uniform upload offset exceeds 32 bits"))?
            }
        };
    }
    draw.batch.last_uniform = Some((uniform_offsets, uniform_sizes));
    if let Some(geometry) = native_geometry.as_mut() {
        let data = &mut draw.batch.data;
        let mut stream = |source: &mut GeometryData, offset: &mut u64, size: u64| {
            if let GeometryData::Stream(bytes) = source {
                let start = data.len().next_multiple_of(4);
                data.resize(start, 0);
                let from = (*offset as usize).min(bytes.len());
                let to = (from + size as usize).min(bytes.len());
                data.extend_from_slice(&bytes[from..to]);
                data.resize(start + size as usize, 0);
                *offset = start as u64;
                *source = GeometryData::Upload;
            }
        };
        for vertex in &mut geometry.vertices {
            stream(&mut vertex.data, &mut vertex.offset, vertex.size);
        }
        stream(
            &mut geometry.index,
            &mut geometry.index_offset,
            geometry.index_size,
        );
    }
    let vertex_size = vertices.bytes.len();
    let vertex_offset = aligned(draw.batch.data.len(), 4)
        .ok_or_else(|| invalid("Draw", "vertex upload offset overflow"))?;
    draw.batch.data.resize(vertex_offset, 0);
    draw.batch.data.extend_from_slice(&vertices.bytes);
    let index_range = draw_indices.as_ref().map(|indices| {
        let offset = draw.batch.data.len().next_multiple_of(4);
        draw.batch.data.resize(offset, 0);
        let start = draw.batch.data.len();
        let format = if compact_indices {
            draw.batch.data.resize(start + index_size, 0);
            for (destination, &index) in draw.batch.data[start..].chunks_exact_mut(2).zip(indices) {
                destination.copy_from_slice(&(index as u16).to_le_bytes());
            }
            wgpu::IndexFormat::Uint16
        } else {
            append_u32_le(&mut draw.batch.data, indices);
            wgpu::IndexFormat::Uint32
        };
        let size = draw.batch.data.len() - start;
        let end = draw.batch.data.len().next_multiple_of(4);
        draw.batch.data.resize(end, 0);
        (start as u64, size as u64, format)
    });
    let scissor = if rs(state, 174) != 0 {
        let s = state.scissor();
        let left = s.left.clamp(0, snapshot.target.width as i32) as u32;
        let top = s.top.clamp(0, snapshot.target.height as i32) as u32;
        let right = s.right.clamp(0, snapshot.target.width as i32) as u32;
        let bottom = s.bottom.clamp(0, snapshot.target.height as i32) as u32;
        Some([
            left,
            top,
            right.saturating_sub(left),
            bottom.saturating_sub(top),
        ])
    } else {
        None
    };
    let vertex_count = native_geometry.as_ref().map_or_else(
        || {
            draw_indices
                .as_ref()
                .map_or(order.len(), |indices| indices.len()) as u32
        },
        |geometry| geometry.count,
    );
    let blend = color(rs(state, 193));
    draw.batch.draws.push(PendingDraw {
        pipeline,
        layout: memo.layout.clone(),
        uniform_offsets,
        uniform_sizes,
        programs: memo.programs.clone(),
        textures: memo.bindings.clone(),
        vertex_offset: vertex_offset as u64,
        vertex_size: vertex_size as u64,
        has_attributes,
        native_geometry,
        index_range,
        vertex_count,
        viewport: vp,
        stencil_reference: rs(state, 57),
        blend_constant: blend,
        scissor,
    });
    if draw.memos.len() >= DRAW_MEMOS {
        draw.memos.remove(0);
    }
    draw.memos.push(memo);
    draw.linked = linked_inputs;
    draw.ff_uniforms = ff_uniforms;
    if draw.batch.data.len() >= 32 << 20 {
        flush(gpu, draw, queries)?;
    }
    if let Some(started) = draw_started {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        draw.draw_cpu_ms += ms;
        if interpreted {
            draw.interpreter_frame.cpu_ms += ms;
        }
    }
    draw.measured_draws += 1;
    if trace_this && trace_readback() {
        flush(gpu, draw, queries)?;
        if matches!(snapshot.target.format, 21 | 22 | 32) {
            let mut pixels =
                vec![0; snapshot.target.width as usize * snapshot.target.height as usize * 4];
            match gpu.read_texture(
                &snapshot.target.texture,
                snapshot.target.format,
                snapshot.target.subresource.1,
                snapshot.target.width,
                snapshot.target.height,
                1,
                snapshot.target.subresource.0,
                false,
                &mut pixels,
            ) {
                Ok(()) => eprintln!(
                    "[d3d9-trace] draw={} target_colored_pixels={}",
                    count,
                    pixels
                        .chunks_exact(4)
                        .filter(|p| p[..3].iter().any(|&b| b != 0))
                        .count()
                ),
                Err(error) => eprintln!("[d3d9-trace] draw={count} readback={error}"),
            }
        }
    }
    Ok(())
}

pub(crate) fn flush(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    queries: &mut D3d9Queries,
) -> Result<(), RenderError> {
    if draw.batch.draws.is_empty() {
        return Ok(());
    }
    match gpu.validate("DrawFlush", || flush_inner(gpu, draw, queries)) {
        Ok(()) => Ok(()),
        Err(error) => {
            draw.discard_batch();
            Err(error)
        }
    }
}

fn flush_inner(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    queries: &mut D3d9Queries,
) -> Result<(), RenderError> {
    if draw.batch.draws.is_empty() {
        return Ok(());
    }
    let area = draw
        .batch
        .draws
        .iter()
        .map(|pending| u64::from(pending.viewport.width) * u64::from(pending.viewport.height))
        .max()
        .unwrap_or(0);
    let segment = queries.open_segment(area)?;
    let needed = draw.batch.data.len().max(4) as u64;
    if needed > gpu.limits().max_buffer_size || needed > u32::MAX as u64 {
        return Err(RenderError::out_of_memory(
            "DrawFlush",
            "draw upload exceeds the device buffer limit",
        ));
    }
    if draw
        .upload
        .as_ref()
        .is_none_or(|buffer| buffer.size() < needed)
    {
        draw.bind_groups.clear();
        draw.bind_groups_previous.clear();
        let size = needed
            .next_power_of_two()
            .max(4096)
            .min(gpu.limits().max_buffer_size)
            .max(needed);
        draw.upload = Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("d3d9-draw-upload"),
            size,
            usage: wgpu::BufferUsages::UNIFORM
                | wgpu::BufferUsages::VERTEX
                | wgpu::BufferUsages::INDEX
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
    }
    let upload = draw
        .upload
        .as_ref()
        .expect("draw upload buffer was created")
        .clone();
    let mut bind_groups = std::mem::take(&mut draw.bind_group_scratch);
    bind_groups.clear();
    let mut bind_group_cache = std::mem::take(&mut draw.bind_groups);
    let mut bind_group_previous = std::mem::take(&mut draw.bind_groups_previous);
    for (index, pending) in draw.batch.draws.iter().enumerate() {
        let bind_group = if index > 0 && same_bindings(&draw.batch.draws[index - 1], pending) {
            bind_groups[index - 1].clone()
        } else {
            let key = BindGroupKey {
                layout: pending.layout.clone(),
                uniform_sizes: pending.uniform_sizes,
                programs: pending.programs.clone(),
                textures: pending
                    .textures
                    .iter()
                    .map(|texture| {
                        (
                            texture.stage,
                            texture.texture.clone(),
                            texture.view.clone(),
                            texture.sampler.clone(),
                        )
                    })
                    .collect(),
            };
            if let Some(bind_group) =
                cached_bind_group(&mut bind_group_cache, &mut bind_group_previous, &key)
            {
                bind_group
            } else {
                let mut entries = Vec::with_capacity(4 + pending.textures.len() * 2);
                for binding in 0..4 {
                    entries.push(wgpu::BindGroupEntry {
                        binding,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &upload,
                            offset: 0,
                            size: wgpu::BufferSize::new(pending.uniform_sizes[binding as usize]),
                        }),
                    });
                }
                if let Some(programs) = &pending.programs {
                    for (binding, buffer) in crate::shader::uber::PROGRAM_BINDINGS
                        .into_iter()
                        .zip(programs)
                    {
                        entries.push(wgpu::BindGroupEntry {
                            binding,
                            resource: buffer.as_entire_binding(),
                        });
                    }
                }
                for texture in pending.textures.iter() {
                    entries.push(wgpu::BindGroupEntry {
                        binding: TEXTURE_BINDING + texture.stage,
                        resource: wgpu::BindingResource::TextureView(&texture.view),
                    });
                    entries.push(wgpu::BindGroupEntry {
                        binding: SAMPLER_BINDING + texture.stage,
                        resource: wgpu::BindingResource::Sampler(&texture.sampler),
                    });
                }
                let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("d3d9-draw-bindings"),
                    layout: &pending.layout,
                    entries: &entries,
                });
                store_bind_group(
                    &mut bind_group_cache,
                    &mut bind_group_previous,
                    key,
                    &bind_group,
                );
                bind_group
            }
        };
        bind_groups.push(bind_group);
    }
    draw.bind_groups = bind_group_cache;
    draw.bind_groups_previous = bind_group_previous;
    let color_view = draw
        .batch
        .color_view
        .as_ref()
        .expect("draw batch has a color target");
    let color_key_view = draw
        .batch
        .color_key_view
        .as_ref()
        .expect("draw batch has a color target key");
    let depth_target = draw.batch.depth.as_ref();
    gpu.record_folding_clear(
        |clear| clear.folds_into(color_key_view, depth_target),
        |encoder, staging, fold| {
            let folded_color = fold
                .as_ref()
                .and_then(|clear| clear.color.as_ref())
                .map_or(wgpu::LoadOp::Load, |(_, value)| wgpu::LoadOp::Clear(*value));
            let folded_depth = fold.as_ref().and_then(|clear| clear.depth.as_ref());
            let colors = [Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: folded_color,
                    store: wgpu::StoreOp::Store,
                },
            })];
            let depth = draw.batch.depth.as_ref().map(|(view, has_stencil)| {
                wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: Some(wgpu::Operations {
                        load: folded_depth.map_or(wgpu::LoadOp::Load, |(_, depth, _)| depth.load),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: has_stencil.then_some(wgpu::Operations {
                        load: folded_depth
                            .and_then(|(_, _, stencil)| stencil.as_ref())
                            .map_or(wgpu::LoadOp::Load, |stencil| stencil.load),
                        store: wgpu::StoreOp::Store,
                    }),
                }
            });
            staging
                .write_buffer(
                    encoder,
                    &upload,
                    0,
                    wgpu::BufferSize::new(needed).expect("draw upload is nonempty"),
                )
                .copy_from_slice(&draw.batch.data);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("d3d9-draw-batch"),
                color_attachments: &colors,
                depth_stencil_attachment: depth,
                timestamp_writes: None,
                occlusion_query_set: segment.and(queries.occlusion_set()),
                multiview_mask: None,
            });
            if let Some(index) = segment {
                pass.begin_occlusion_query(index);
            }
            let mut cached_viewport = None;
            let mut cached_stencil = None;
            let mut cached_blend = None;
            let mut cached_scissor = None;
            let mut cached_pipeline: Option<&wgpu::RenderPipeline> = None;
            let mut cached_bind: Option<(&wgpu::BindGroup, [u32; 4])> = None;
            for (pending, bind_group) in draw.batch.draws.iter().zip(&bind_groups) {
                if cached_pipeline != Some(&pending.pipeline) {
                    pass.set_pipeline(&pending.pipeline);
                    cached_pipeline = Some(&pending.pipeline);
                }
                if cached_bind != Some((bind_group, pending.uniform_offsets)) {
                    pass.set_bind_group(0, bind_group, &pending.uniform_offsets);
                    cached_bind = Some((bind_group, pending.uniform_offsets));
                }
                if let Some(geometry) = &pending.native_geometry {
                    for (slot, vertex) in geometry.vertices.iter().enumerate() {
                        pass.set_vertex_buffer(
                            slot as u32,
                            vertex.data.slice(&upload, vertex.offset, vertex.size),
                        );
                    }
                } else if pending.has_attributes {
                    pass.set_vertex_buffer(
                        0,
                        upload.slice(
                            pending.vertex_offset..pending.vertex_offset + pending.vertex_size,
                        ),
                    );
                }
                let viewport_key = (
                    pending.viewport.x,
                    pending.viewport.y,
                    pending.viewport.width,
                    pending.viewport.height,
                    pending.viewport.min_z.to_bits(),
                    pending.viewport.max_z.to_bits(),
                );
                if cached_viewport != Some(viewport_key) {
                    pass.set_viewport(
                        pending.viewport.x as f32,
                        pending.viewport.y as f32,
                        pending.viewport.width as f32,
                        pending.viewport.height as f32,
                        pending.viewport.min_z,
                        pending.viewport.max_z,
                    );
                    cached_viewport = Some(viewport_key);
                }
                if cached_stencil != Some(pending.stencil_reference) {
                    pass.set_stencil_reference(pending.stencil_reference);
                    cached_stencil = Some(pending.stencil_reference);
                }
                let blend = pending.blend_constant;
                let blend_bits = blend.map(f32::to_bits);
                if cached_blend != Some(blend_bits) {
                    pass.set_blend_constant(wgpu::Color {
                        r: blend[0] as f64,
                        g: blend[1] as f64,
                        b: blend[2] as f64,
                        a: blend[3] as f64,
                    });
                    cached_blend = Some(blend_bits);
                }
                let [left, top, width, height] =
                    pending
                        .scissor
                        .unwrap_or([0, 0, draw.batch.width, draw.batch.height]);
                if cached_scissor != Some([left, top, width, height]) {
                    pass.set_scissor_rect(left, top, width, height);
                    cached_scissor = Some([left, top, width, height]);
                }
                if let Some(geometry) = &pending.native_geometry {
                    pass.set_index_buffer(
                        geometry
                            .index
                            .slice(&upload, geometry.index_offset, geometry.index_size),
                        geometry.index_format,
                    );
                    pass.draw_indexed(0..geometry.count, geometry.base_vertex, 0..1);
                } else if let Some((offset, size, format)) = pending.index_range {
                    pass.set_index_buffer(upload.slice(offset..offset + size), format);
                    pass.draw_indexed(0..pending.vertex_count, 0, 0..1);
                } else {
                    pass.draw(0..pending.vertex_count, 0..1);
                }
            }
            if segment.is_some() {
                pass.end_occlusion_query();
            }
            drop(pass);
            if let Some(padded) = &draw.batch.padded_color {
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &padded.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &padded.source.texture,
                        mip_level: padded.source.subresource.1,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: padded.source.subresource.0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: padded.source.width,
                        height: padded.source.height,
                        depth_or_array_layers: 1,
                    },
                );
            }
        },
    );
    if trace_compilation() {
        draw.batch_upload_bytes += needed;
    }
    bind_groups.clear();
    draw.bind_group_scratch = bind_groups;
    for pending in &mut draw.batch.draws {
        if let Some(geometry) = pending.native_geometry.take() {
            draw.native_geometry_pool.push(geometry);
        }
    }
    draw.batch.draws.clear();
    draw.batch.data.clear();
    draw.batch.color_view = None;
    draw.batch.color_key_view = None;
    draw.batch.padded_color = None;
    draw.batch.depth = None;
    draw.batch.last_uniform = None;
    Ok(())
}

pub(crate) fn clear(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    queries: &mut D3d9Queries,
    target: &SurfaceView,
    depth: Option<&SurfaceView>,
    flags: u32,
    color_value: u32,
    z: f32,
    stencil: u32,
    rects: &[TargetRect],
) -> Result<(), RenderError> {
    if flags == 0 || flags & !7 != 0 {
        return Err(invalid(
            "Clear",
            format!("invalid D3DCLEAR flags {flags:#x}"),
        ));
    }
    if flags & 6 != 0 && depth.is_none() {
        return Err(invalid(
            "Clear",
            "depth/stencil clear without a depth target",
        ));
    }
    if flags & 4 != 0 && !depth.is_some_and(|d| super::format::has_stencil(d.format)) {
        return Err(invalid(
            "Clear",
            "stencil clear on a format without stencil",
        ));
    }
    if flags & 2 != 0 && !(0.0..=1.0).contains(&z) {
        return Err(invalid("Clear", "depth outside 0..1"));
    }
    if rects.is_empty() {
        return Ok(());
    }
    flush(gpu, draw, queries)?;
    let depth = depth.filter(|_| flags & 6 != 0);
    let full_color = flags & 1 == 0 || !target.texture.format().is_srgb();
    let full_rect = rects.len() == 1
        && rects[0].left == 0
        && rects[0].top == 0
        && rects[0].right == target.width as i32
        && rects[0].bottom == target.height as i32
        && target.width != 0
        && target.height != 0;
    let full_depth = flags & 6 == 0
        || depth
            .as_ref()
            .is_some_and(|d| d.width == target.width && d.height == target.height);
    if full_color && full_rect && full_depth {
        let mut clear = color(color_value);
        if opaque_format(target.format) {
            clear[3] = 1.0;
        }
        gpu.set_clear(PendingClear {
            color: (flags & 1 != 0).then(|| {
                (
                    target.view.clone(),
                    wgpu::Color {
                        r: clear[0] as f64,
                        g: clear[1] as f64,
                        b: clear[2] as f64,
                        a: clear[3] as f64,
                    },
                )
            }),
            depth: depth.map(|d| {
                (
                    d.view.clone(),
                    wgpu::Operations {
                        load: if flags & 2 != 0 {
                            wgpu::LoadOp::Clear(z)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    },
                    super::format::has_stencil(d.format).then_some(wgpu::Operations {
                        load: if flags & 4 != 0 {
                            wgpu::LoadOp::Clear(stencil)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: wgpu::StoreOp::Store,
                    }),
                )
            }),
        });
        return Ok(());
    }
    let depth_matches = depth.is_some_and(|d| d.width == target.width && d.height == target.height);
    let pass_depth = depth.filter(|_| depth_matches);
    let separate_depth = depth.filter(|_| !depth_matches);
    let mut source = String::from(
        "struct Params {color:vec4<f32>,depth:vec4<f32>}\n@group(0) @binding(0) var<uniform> params:Params;\n@vertex fn vs_main(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32>{let p=array<vec2<f32>,3>(vec2<f32>(-1.0,-1.0),vec2<f32>(3.0,-1.0),vec2<f32>(-1.0,3.0));return vec4<f32>(p[i],0.0,1.0);}\n",
    );
    if flags & 2 != 0 && pass_depth.is_some() {
        source += "struct Out {@location(0) color:vec4<f32>,@builtin(frag_depth) depth:f32}\n@fragment fn ps_main()->Out{var o:Out;o.color=params.color;o.depth=params.depth.x;return o;}\n";
    } else {
        source += "@fragment fn ps_main()->@location(0) vec4<f32>{return params.color;}\n";
    }
    let shader = draw.shader(gpu, &source)?;
    let layout = draw.layout(gpu, LayoutKey::Clear)?;
    let mut params = color(color_value).to_vec();
    if opaque_format(target.format) {
        params[3] = 1.0;
    }
    params.extend_from_slice(&[z, 0.0, 0.0, 0.0]);
    let buffer = make_buffer(
        gpu,
        "d3d9-clear",
        &flat_to_u8(&params),
        wgpu::BufferUsages::UNIFORM,
    );
    let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("d3d9-clear"),
        layout: &layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Always,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: wgpu::StencilOperation::Replace,
    };
    let depth_state = |d: &SurfaceView| {
        Ok::<_, RenderError>(wgpu::DepthStencilState {
            format: d.texture.format(),
            depth_write_enabled: Some(flags & 2 != 0),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState {
                front: face,
                back: face,
                read_mask: u32::MAX,
                write_mask: if flags & 4 != 0 { u32::MAX } else { 0 },
            },
            bias: Default::default(),
        })
    };
    let clear_source = ShaderRecipe::Source(source.into());
    let clear_bindings: Arc<[wgpu::BindGroupLayoutEntry]> = LayoutKey::Clear.entries().into();
    let clear_pipeline = |draw: &mut D3d9DrawState, key: PipelineKey, source: ShaderRecipe| {
        let recipe = PipelineRecipe {
            shaders: [source.clone(), source],
            interface: InterfaceKey {
                bindings: Arc::clone(&clear_bindings),
                vertex_buffers: key.buffers.clone(),
            },
            descriptor: key.descriptor(),
        };
        draw.pipeline(gpu, key, recipe)
    };
    let color_pipeline = if flags & 1 != 0 || pass_depth.is_some() {
        Some(clear_pipeline(
            draw,
            PipelineKey {
                entry_points: ["vs_main", "ps_main"],
                modules: [shader.clone(), shader.clone()],
                layout: layout.clone(),
                buffers: SmallVec::new(),
                target: Some(wgpu::ColorTargetState {
                    format: target.texture.format(),
                    blend: None,
                    write_mask: if flags & 1 != 0 {
                        wgpu::ColorWrites::ALL
                    } else {
                        wgpu::ColorWrites::empty()
                    },
                }),
                primitive: Default::default(),
                depth: pass_depth.map(&depth_state).transpose()?,
                multisample: Default::default(),
            },
            clear_source.clone(),
        )?)
    } else {
        None
    };
    let depth_pipeline = match separate_depth {
        Some(d) => {
            let source = "struct Params {color:vec4<f32>,depth:vec4<f32>}\n@group(0) @binding(0) var<uniform> params:Params;\n@vertex fn vs_main(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32>{let p=array<vec2<f32>,3>(vec2<f32>(-1.0,-1.0),vec2<f32>(3.0,-1.0),vec2<f32>(-1.0,3.0));return vec4<f32>(p[i],0.0,1.0);}\n@fragment fn ps_main()->@builtin(frag_depth) f32{return params.depth.x;}\n";
            let shader = draw.shader(gpu, source)?;
            Some(clear_pipeline(
                draw,
                PipelineKey {
                    entry_points: ["vs_main", "ps_main"],
                    modules: [shader.clone(), shader],
                    layout: layout.clone(),
                    buffers: SmallVec::new(),
                    target: None,
                    primitive: Default::default(),
                    depth: Some(depth_state(d)?),
                    multisample: Default::default(),
                },
                ShaderRecipe::Source(Arc::from(source)),
            )?)
        }
        None => None,
    };
    gpu.record(|encoder, _| {
        if let Some(pipeline) = &color_pipeline {
            let colors = [Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })];
            let depth = pass_depth.map(|d| wgpu::RenderPassDepthStencilAttachment {
                view: &d.view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: if super::format::has_stencil(d.format) {
                    Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    })
                } else {
                    None
                },
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("d3d9-clear"),
                color_attachments: &colors,
                depth_stencil_attachment: depth,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.set_stencil_reference(stencil);
            for r in rects {
                let l = r.left.clamp(0, target.width as i32) as u32;
                let t = r.top.clamp(0, target.height as i32) as u32;
                let right = r.right.clamp(0, target.width as i32) as u32;
                let bottom = r.bottom.clamp(0, target.height as i32) as u32;
                if right > l && bottom > t {
                    pass.set_scissor_rect(l, t, right - l, bottom - t);
                    pass.draw(0..3, 0..1);
                }
            }
        }
        if let (Some(d), Some(pipeline)) = (separate_depth, &depth_pipeline) {
            let depth = Some(wgpu::RenderPassDepthStencilAttachment {
                view: &d.view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: if super::format::has_stencil(d.format) {
                    Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    })
                } else {
                    None
                },
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("d3d9-clear-depth"),
                color_attachments: &[],
                depth_stencil_attachment: depth,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.set_stencil_reference(stencil);
            for r in rects {
                let l = r.left.clamp(0, d.width as i32) as u32;
                let t = r.top.clamp(0, d.height as i32) as u32;
                let right = r.right.clamp(0, d.width as i32) as u32;
                let bottom = r.bottom.clamp(0, d.height as i32) as u32;
                if right > l && bottom > t {
                    pass.set_scissor_rect(l, t, right - l, bottom - t);
                    pass.draw(0..3, 0..1);
                }
            }
        }
    });
    Ok(())
}
