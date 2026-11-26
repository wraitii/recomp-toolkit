//! CPU resource storage and draw preparation, independent of adapters and GPUs.
//! The bridge owns guest staging; these bytes retain their native D3D layout.
use crate::RenderError;
use crate::fast_map::FastMap;
use std::borrow::Cow;

/// Stable storage. Allocation is fallible; its address never changes before drop.
pub struct CpuStorage {
    pub bytes: Vec<u8>,
}

impl CpuStorage {
    pub fn new(size: u32) -> Result<Self, RenderError> {
        if size == 0 {
            return Err(RenderError::invalid("CreateResource", "zero size"));
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size as usize).map_err(|_| {
            RenderError::out_of_memory("CreateResource", "CPU storage allocation failed")
        })?;
        bytes.resize(size as usize, 0);
        Ok(Self { bytes })
    }
}

/// Supported CPU texel layouts, separate from GPU sampling support.
/// Palette and compressed formats remain unsupported by this storage path.
pub fn format_bytes(format: u32) -> u32 {
    match format {
        21 | 22 | 31..=35 => 4,
        23..=26 | 29 | 30 | 51 | 60 => 2,
        27 | 28 | 50 | 52 => 1,
        36 => 8,
        _ => 0,
    }
}

/// True for a block-compressed format. These have no per-texel byte count:
/// their levels are laid out in 4x4 blocks by [`crate::d3d8::format`].
pub fn is_block_format(format: u32) -> bool {
    crate::d3d8::format::is_block_format(format)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct D3d8LevelLayout {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub size: u32,
    pub levels: u32,
}

/// Preserve the bridge's bounded mip policy while centralizing checked sizes.
pub fn level_layout(
    width: u32,
    height: u32,
    levels: u32,
    level: u32,
    format: u32,
) -> Result<D3d8LevelLayout, RenderError> {
    let bpp = format_bytes(format);
    let block = crate::d3d8::format::block_bytes(format);
    if width == 0 || height == 0 || width > 16384 || height > 16384 || (bpp == 0 && block == 0) {
        return Err(RenderError::invalid(
            "CreateTexture",
            "invalid dimensions or CPU format",
        ));
    }
    let levels = if levels == 0 {
        u32::BITS - width.max(height).leading_zeros()
    } else {
        levels.min(16)
    };
    if level >= levels {
        return Err(RenderError::invalid(
            "GetLevelDesc",
            "mip level out of range",
        ));
    }
    let width = (width >> level).max(1);
    let height = (height >> level).max(1);
    let (pitch, size) = if block != 0 {
        // A block format's pitch is a block-row pitch and its extent is in
        // blocks; the last row/column stores a full block (D3D8 LockRect pitch).
        crate::d3d8::format::block_level_layout(width, height, format)
    } else {
        let pitch = width
            .checked_mul(bpp)
            .ok_or_else(|| RenderError::invalid("CreateTexture", "pitch overflow"))?;
        let size = pitch
            .checked_mul(height)
            .ok_or_else(|| RenderError::invalid("CreateTexture", "size overflow"))?;
        (pitch, size)
    };
    Ok(D3d8LevelLayout {
        width,
        height,
        pitch,
        size,
        levels,
    })
}

/// Owned probe vertices or a borrowed ABI upload. No extra copy for guest draws.
pub struct VertexBuffer<'a> {
    bytes: Cow<'a, [u8]>,
    pub stride: u32,
}

impl VertexBuffer<'static> {
    pub fn new(bytes: &[u8], stride: u32) -> Result<Self, RenderError> {
        VertexBuffer::borrowed(bytes, stride)?;
        let mut storage = CpuStorage::new(u32::try_from(bytes.len()).map_err(|_| {
            RenderError::invalid("CreateVertexBuffer", "size exceeds 32-bit resource limit")
        })?)?;
        storage.bytes.copy_from_slice(bytes);
        Ok(Self {
            bytes: Cow::Owned(storage.bytes),
            stride,
        })
    }
}

impl<'a> VertexBuffer<'a> {
    pub fn borrowed(bytes: &'a [u8], stride: u32) -> Result<Self, RenderError> {
        if stride == 0 || bytes.is_empty() {
            return Err(RenderError::invalid(
                "DrawPrimitive",
                "nonempty vertices and nonzero stride required",
            ));
        }
        // D3D buffers may have trailing bytes that aren't part of a vertex.
        Ok(Self {
            bytes: Cow::Borrowed(bytes),
            stride,
        })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Pack a nonindexed line strip into a line list, or a triangle strip/fan into
/// independent triangles, retaining complete vertex records and alternating
/// strip winding. Bounds and allocation are checked before any source copy;
/// the caller retains/reuses the scratch bytes.
pub fn expand_nonindexed_into(
    output: &mut Vec<u8>,
    vertices: &VertexBuffer,
    topology: u32,
    start_vertex: u32,
    primitive_count: u32,
) -> Result<(), RenderError> {
    let invalid = |why| RenderError::invalid("DrawPrimitive", why);
    if !matches!(topology, 3 | 5 | 6) {
        return Err(RenderError::new(
            "DrawPrimitive",
            "expected LINESTRIP (3), TRIANGLESTRIP (5) or TRIANGLEFAN (6)",
        ));
    }
    if primitive_count == 0 {
        output.clear();
        return Ok(());
    }
    let (extra, per) = if topology == 3 { (1, 2) } else { (2, 3) };
    let end = start_vertex
        .checked_add(primitive_count)
        .and_then(|n| n.checked_add(extra))
        .ok_or_else(|| invalid("vertex range overflow"))?;
    let stride = vertices.stride;
    if u64::from(end) * u64::from(stride) > vertices.bytes().len() as u64 {
        return Err(invalid("vertex range exceeds buffer"));
    }
    let size = primitive_count
        .checked_mul(per)
        .and_then(|n| n.checked_mul(stride))
        .ok_or_else(|| invalid("expanded vertex size overflow"))? as usize;
    output.clear();
    output.try_reserve(size).map_err(|_| {
        RenderError::out_of_memory("DrawPrimitive", "triangle expansion allocation failed")
    })?;
    for triangle in 0..primitive_count {
        if topology == 3 {
            for index in [triangle, triangle + 1] {
                let begin = (u64::from(start_vertex + index) * u64::from(stride)) as usize;
                output.extend_from_slice(&vertices.bytes()[begin..begin + stride as usize]);
            }
            continue;
        }
        let indices = if topology == 6 {
            [0, triangle + 1, triangle + 2]
        } else if triangle & 1 == 0 {
            [triangle, triangle + 1, triangle + 2]
        } else {
            [triangle + 1, triangle, triangle + 2]
        };
        for index in indices {
            let begin = (u64::from(start_vertex + index) * u64::from(stride)) as usize;
            output.extend_from_slice(&vertices.bytes()[begin..begin + stride as usize]);
        }
    }
    Ok(())
}

/// Arguments retain D3D8's distinction between the raw indices and the base
/// vertex applied when accessing the stream. `min_index`/`num_vertices` are the
/// API's hints: Wine only passes `num_vertices` to the sysmem vertex-buffer
/// upload and never uses either to bound the draw's index reads
/// (`d3d8_device_DrawIndexedPrimitive`, dlls/d3d8/device.c at Wine commit
/// 455e3509b98a6919fd4ad1def4803e08c41c03b2). Expansion preserves ordering.
#[derive(Clone, Copy, Debug)]
pub struct IndexedDraw {
    pub topology: u32,
    pub index_format: u32,
    pub stride: u32,
    pub base_vertex: u32,
    pub min_index: u32,
    pub num_vertices: u32,
    pub start_index: u32,
    pub primitive_count: u32,
}

pub fn expand_indexed(
    vertices: &[u8],
    indices: &[u8],
    draw: IndexedDraw,
) -> Result<Vec<u8>, RenderError> {
    let mut output = Vec::new();
    expand_indexed_into(&mut output, vertices, indices, draw)?;
    Ok(output)
}

/// [`expand_indexed`] writing into a caller-owned buffer so a draw loop can
/// reuse one allocation. Validation happens before `output` is touched, and
/// `output` is left exactly `count * stride` bytes with every byte written.
/// `resize` only zero-fills the first time a larger draw grows it; in steady
/// state the length already matches and nothing is initialised.
pub fn expand_indexed_into(
    output: &mut Vec<u8>,
    vertices: &[u8],
    indices: &[u8],
    draw: IndexedDraw,
) -> Result<(), RenderError> {
    let invalid = |cause| RenderError::invalid("DrawIndexedPrimitive", cause);
    // D3D7/8 index buffers name vertices; the renderer draws triangle lists.
    // A list selects `3 * primitives` indices, a strip or fan selects
    // `2 + primitives` and is expanded into the same list below. The strip
    // expansion keeps each triangle's original vertex order, so the cull test
    // sees the same winding the strip would have produced.
    let (index_count, triangle_count) = match draw.topology {
        4 => (
            draw.primitive_count
                .checked_mul(3)
                .ok_or_else(|| invalid("index count overflow"))?,
            draw.primitive_count,
        ),
        5 | 6 => (
            draw.primitive_count
                .checked_add(2)
                .ok_or_else(|| invalid("index count overflow"))?,
            draw.primitive_count,
        ),
        _ => {
            return Err(RenderError::new(
                "DrawIndexedPrimitive",
                "unsupported topology; expected TRIANGLELIST (4), TRIANGLESTRIP (5) or TRIANGLEFAN (6)",
            ));
        }
    };
    let index_size = match draw.index_format {
        101 => 2usize,
        102 => 4,
        _ => return Err(invalid("invalid index format")),
    };
    // Wine's d3d8_device_DrawIndexedPrimitive does not reject a zero primitive
    // count and passes NumVertices only to the sysmem vertex-buffer upload; the
    // draw itself uses the indices and BaseVertexIndex
    // (dlls/d3d8/device.c at Wine commit 455e3509b98a6919fd4ad1def4803e08c41c03b2).
    // A zero count expands to no indices and is a no-op; NumVertices=0 is a
    // valid hint. Only a zero stride is a draw error.
    if draw.stride == 0 {
        return Err(invalid("zero stride"));
    }
    if draw.primitive_count == 0 {
        output.clear();
        return Ok(());
    }
    let end = draw
        .start_index
        .checked_add(index_count)
        .ok_or_else(|| invalid("index range overflow"))?;
    if u64::from(end) * index_size as u64 > indices.len() as u64 {
        return Err(invalid("index range exceeds buffer"));
    }
    // Validate all indices before allocating or reading a vertex. Every index
    // is bounds-checked as `(index + base_vertex) * stride` against the real
    // vertex buffer; MinIndex/NumVertices are not a range (Wine only uses
    // NumVertices for the upload, never to bound the draw's index reads).
    let selected = &indices[draw.start_index as usize * index_size..end as usize * index_size];
    let read = |bytes: &[u8]| {
        if index_size == 2 {
            u32::from(u16::from_le_bytes(bytes.try_into().unwrap()))
        } else {
            u32::from_le_bytes(bytes.try_into().unwrap())
        }
    };
    for bytes in selected.chunks_exact(index_size) {
        let vertex = u64::from(read(bytes)) + u64::from(draw.base_vertex);
        // Divide before comparing: (index + base + 1) * stride can exceed
        // u64 even though each guest argument is only 32 bits.
        if vertex >= vertices.len() as u64 / u64::from(draw.stride) {
            return Err(invalid("index outside vertex buffer"));
        }
    }
    let list_indices = triangle_count
        .checked_mul(3)
        .ok_or_else(|| invalid("triangle count overflow"))?;
    let size = list_indices
        .checked_mul(draw.stride)
        .ok_or_else(|| invalid("expanded size overflow"))?;
    // The source position of output triangle vertex `t`: a list is in order, a
    // strip is windowed (i, i+1, i+2) with odd triangles as (i+1, i, i+2) so
    // every triangle keeps the strip's winding, and a fan pins vertex 0.
    let source = |t: usize| -> usize {
        match draw.topology {
            4 => t,
            5 => {
                let i = t / 3;
                match (i & 1, t % 3) {
                    (1, 0) => i + 1,
                    (1, 1) => i,
                    (_, k) => i + k,
                }
            }
            _ => {
                let i = t / 3;
                if t % 3 == 0 { 0 } else { i + t % 3 }
            }
        }
    };
    trace_drawn_triangles(
        vertices,
        selected,
        index_size,
        &draw,
        triangle_count as usize,
    );
    output.resize(size as usize, 0);
    for (t, dst) in output.chunks_exact_mut(draw.stride as usize).enumerate() {
        let at = source(t) * index_size;
        let bytes = &selected[at..at + index_size];
        let offset =
            (u64::from(read(bytes)) + u64::from(draw.base_vertex)) * u64::from(draw.stride);
        dst.copy_from_slice(&vertices[offset as usize..offset as usize + draw.stride as usize]);
    }
    Ok(())
}

/// Validate a triangle-list draw and rebase its indices to the smallest
/// contiguous vertex interval. MinIndex/NumVertices remain upload hints.
/// Returns None when that interval would upload more vertices than expansion.
pub fn indexed_list_into(
    output: &mut Vec<u32>,
    vertices: &[u8],
    indices: &[u8],
    draw: IndexedDraw,
) -> Result<Option<(usize, usize)>, RenderError> {
    indexed_list_bounds(output, vertices, indices, draw, true)
}

/// Pack only distinct referenced vertices in first-use order. In particular,
/// sparse gaps are never copied or evaluated by fixed-function lighting.
/// All source reads are validated before the packed output is modified.
pub fn packed_indexed_list_into(
    output: &mut Vec<u8>,
    remapped: &mut Vec<u32>,
    vertices: &[u8],
    indices: &[u8],
    draw: IndexedDraw,
) -> Result<(), RenderError> {
    let Some((begin, _)) = indexed_list_bounds(remapped, vertices, indices, draw, false)? else {
        output.clear();
        return Ok(());
    };
    output.clear();
    let mut slots: FastMap<u32, u32> = FastMap::default();
    let stride = draw.stride as usize;
    for index in remapped.iter_mut() {
        let next = slots.len() as u32;
        let slot = *slots.entry(*index).or_insert_with(|| {
            let at = begin + *index as usize * stride;
            output.extend_from_slice(&vertices[at..at + stride]);
            next
        });
        *index = slot;
    }
    Ok(())
}

fn indexed_list_bounds(
    output: &mut Vec<u32>,
    vertices: &[u8],
    indices: &[u8],
    draw: IndexedDraw,
    compact_only: bool,
) -> Result<Option<(usize, usize)>, RenderError> {
    let invalid = |cause| RenderError::invalid("DrawIndexedPrimitive", cause);
    let count = draw
        .primitive_count
        .checked_mul(3)
        .ok_or_else(|| invalid("index count overflow"))?;
    let size = match draw.index_format {
        101 => 2,
        102 => 4,
        _ => return Err(invalid("invalid index format")),
    };
    if draw.stride == 0 {
        return Err(invalid("zero stride"));
    }
    let end = draw
        .start_index
        .checked_add(count)
        .ok_or_else(|| invalid("index range overflow"))?;
    if u64::from(end) * size as u64 > indices.len() as u64 {
        return Err(invalid("index range exceeds buffer"));
    }
    let selected = &indices[draw.start_index as usize * size..end as usize * size];
    let read = |b: &[u8]| {
        if size == 2 {
            u32::from(u16::from_le_bytes(b.try_into().unwrap()))
        } else {
            u32::from_le_bytes(b.try_into().unwrap())
        }
    };
    let mut min = u32::MAX;
    let mut max = 0;
    for b in selected.chunks_exact(size) {
        let index = read(b);
        let vertex = u64::from(index) + u64::from(draw.base_vertex);
        if vertex >= vertices.len() as u64 / u64::from(draw.stride) {
            return Err(invalid("index outside vertex buffer"));
        }
        min = min.min(index);
        max = max.max(index);
    }
    if count == 0 {
        output.clear();
        return Ok(None);
    }
    let span = u64::from(max) - u64::from(min) + 1;
    if compact_only && span > u64::from(count) {
        return Ok(None);
    }
    output.clear();
    output.extend(selected.chunks_exact(size).map(|b| read(b) - min));
    let begin = (u64::from(draw.base_vertex) + u64::from(min)) * u64::from(draw.stride);
    let end = begin + span * u64::from(draw.stride);
    Ok(Some((begin as usize, end as usize)))
}

/// `RECOMP_D3D8_TRACE_DRAWN_TRIS=<max reports>`: for indexed triangle-list
/// draws, reports triangles whose longest edge over the first three floats of
/// each vertex exceeds 5000 (world units for XYZ streams, pixels for XYZRHW),
/// with the three raw index values and each vertex's first six floats, so a bad
/// index can be told from a bad vertex. Indices are pre-`base_vertex`.
fn trace_drawn_triangles(
    vertices: &[u8],
    selected: &[u8],
    index_size: usize,
    draw: &IndexedDraw,
    triangle_count: usize,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    static PRINTED: AtomicUsize = AtomicUsize::new(0);
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("RECOMP_D3D8_TRACE_DRAWN_TRIS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    });
    if limit == 0 || draw.topology != 4 || PRINTED.load(Ordering::Relaxed) >= limit {
        return;
    }
    let stride = draw.stride as usize;
    let index = |i: usize| -> u32 {
        let b = &selected[i * index_size..(i + 1) * index_size];
        if index_size == 2 {
            u32::from(u16::from_le_bytes(b.try_into().unwrap()))
        } else {
            u32::from_le_bytes(b.try_into().unwrap())
        }
    };
    let floats = |idx: u32, n: usize| -> Vec<f32> {
        let at = (idx as usize + draw.base_vertex as usize) * stride;
        (0..n.min(stride / 4))
            .map(|k| f32::from_le_bytes(vertices[at + k * 4..at + k * 4 + 4].try_into().unwrap()))
            .collect()
    };
    let dist = |a: &[f32], b: &[f32]| {
        (0..3.min(a.len()))
            .map(|k| (a[k] - b[k]).powi(2))
            .sum::<f32>()
            .sqrt()
    };
    let mut bad = Vec::new();
    for t in 0..triangle_count {
        let ids = [index(t * 3), index(t * 3 + 1), index(t * 3 + 2)];
        let p = [floats(ids[0], 3), floats(ids[1], 3), floats(ids[2], 3)];
        let e = dist(&p[0], &p[1])
            .max(dist(&p[1], &p[2]))
            .max(dist(&p[2], &p[0]));
        if !(e <= 5000.0) {
            bad.push((e, t, ids));
        }
    }
    if bad.is_empty() {
        return;
    }
    PRINTED.fetch_add(1, Ordering::Relaxed);
    bad.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!(
        "[d3d8-trace] drawn_tris stride={stride} start_index={} min_index={} num_vertices={} base_vertex={} tris={triangle_count} edge>5000:{}",
        draw.start_index,
        draw.min_index,
        draw.num_vertices,
        draw.base_vertex,
        bad.len()
    );
    for (e, t, ids) in bad.iter().take(3) {
        eprintln!(
            "[d3d8-trace]   drawn_tri {t} edge={e} idx={ids:?} v0={:?} v1={:?} v2={:?}",
            floats(ids[0], 6),
            floats(ids[1], 6),
            floats(ids[2], 6)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draw() -> IndexedDraw {
        IndexedDraw {
            topology: 4,
            index_format: 101,
            stride: 1,
            base_vertex: 2,
            min_index: 1,
            num_vertices: 3,
            start_index: 1,
            primitive_count: 1,
        }
    }
    #[test]
    fn extreme_index_base_and_stride_reject_without_arithmetic_overflow() {
        let indices = [u32::MAX; 3]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let d = IndexedDraw {
            topology: 4,
            index_format: 102,
            stride: u32::MAX,
            base_vertex: u32::MAX,
            min_index: 0,
            num_vertices: 0,
            start_index: 0,
            primitive_count: 1,
        };
        let mut packed = vec![7];
        let mut remapped = Vec::new();
        assert!(expand_indexed_into(&mut packed, &[0; 8], &indices, d).is_err());
        assert!(indexed_list_into(&mut remapped, &[0; 8], &indices, d).is_err());
        assert!(
            packed_indexed_list_into(&mut packed, &mut remapped, &[0; 8], &indices, d).is_err()
        );
        assert_eq!(packed, [7]);
    }

    #[test]
    fn native_indices_rebase_and_match_expansion() {
        for format in [101, 102] {
            let raw = [99u32, 3, 1, 2];
            let indices: Vec<u8> = raw
                .into_iter()
                .flat_map(|i| {
                    if format == 101 {
                        (i as u16).to_le_bytes().to_vec()
                    } else {
                        i.to_le_bytes().to_vec()
                    }
                })
                .collect();
            let vertices = [10, 11, 12, 13, 14, 15];
            let d = IndexedDraw {
                index_format: format,
                min_index: u32::MAX,
                num_vertices: 0,
                ..draw()
            };
            let mut rebased = Vec::new();
            let (begin, end) = indexed_list_into(&mut rebased, &vertices, &indices, d)
                .unwrap()
                .unwrap();
            assert_eq!((begin, end), (3, 6));
            assert_eq!(rebased, [2, 0, 1]);
            assert_eq!(
                rebased
                    .iter()
                    .map(|i| vertices[begin + *i as usize])
                    .collect::<Vec<_>>(),
                expand_indexed(&vertices, &indices, d).unwrap()
            );
            assert!(indexed_list_into(&mut rebased, &vertices[..5], &indices, d).is_err());
            assert!(indexed_list_into(&mut rebased, &vertices, &indices[..1], d).is_err());
        }
        let indices = [0u16, 0, 7]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert!(
            indexed_list_into(
                &mut Vec::new(),
                &[0; 16],
                &indices,
                IndexedDraw {
                    start_index: 0,
                    ..draw()
                }
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn indexed_interval_is_relative_before_base_vertex() {
        let indices = [99u16, 3, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            expand_indexed(&[10, 11, 12, 13, 14, 15], &indices, draw()).unwrap(),
            [15, 13, 14]
        );
        let indices = [99u32, 3, 1, 2]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            expand_indexed(
                &[10, 11, 12, 13, 14, 15],
                &indices,
                IndexedDraw {
                    index_format: 102,
                    ..draw()
                }
            )
            .unwrap(),
            [15, 13, 14]
        );
    }
    #[test]
    fn indexed_strip_and_fan_expand_to_triangle_lists() {
        let vertices = [10u8, 11, 12, 13, 14];
        let indices = [1u16, 2, 3, 4]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let strip = IndexedDraw {
            topology: 5,
            index_format: 101,
            stride: 1,
            base_vertex: 0,
            min_index: 1,
            num_vertices: 4,
            start_index: 0,
            primitive_count: 2,
        };
        assert_eq!(
            expand_indexed(&vertices, &indices, strip).unwrap(),
            [11, 12, 13, 13, 12, 14]
        );
        let fan = IndexedDraw {
            topology: 6,
            ..strip
        };
        assert_eq!(
            expand_indexed(&vertices, &indices, fan).unwrap(),
            [11, 12, 13, 11, 13, 14]
        );
    }

    #[test]
    fn indexed_overflow_and_bounds_fail_before_allocation() {
        let indices = [1u16, 2, 3]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        for d in [
            IndexedDraw {
                primitive_count: u32::MAX,
                ..draw()
            },
            IndexedDraw {
                start_index: u32::MAX,
                ..draw()
            },
            IndexedDraw {
                base_vertex: u32::MAX,
                ..draw()
            },
            IndexedDraw {
                stride: 0,
                ..draw()
            },
        ] {
            assert!(expand_indexed(&[0; 6], &indices, d).is_err());
        }
        // An index that reads past the real vertex buffer is a named error even
        // though it lies inside the declared MinIndex/NumVertices interval.
        let err = expand_indexed(
            &[0; 4],
            &[0u16, 1, 2]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
            IndexedDraw {
                base_vertex: 2,
                min_index: 0,
                num_vertices: 0,
                start_index: 0,
                ..draw()
            },
        )
        .unwrap_err();
        assert!(
            err.cause.contains("index outside vertex buffer"),
            "{}",
            err.cause
        );
        // A short index buffer still fails the index-range check (start_index 1
        // with one listed triangle needs 8 bytes).
        assert!(
            expand_indexed(
                &[0; 6],
                &[0, 0, 0, 0, 0, 0],
                IndexedDraw {
                    min_index: 0,
                    ..draw()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn num_vertices_is_only_a_hint_and_zero_draws_nothing() {
        // Wine passes NumVertices to the sysmem vertex-buffer upload only; the
        // draw uses the indices and BaseVertexIndex. NumVertices=0 must still
        // expand, with each index bounds-checked against the real buffer.
        let indices = [3u16, 1, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let vertices = [10u8, 11, 12, 13, 14, 15];
        let expanded = expand_indexed(
            &vertices,
            &indices,
            IndexedDraw {
                base_vertex: 1,
                min_index: 0,
                num_vertices: 0,
                start_index: 0,
                primitive_count: 1,
                ..draw()
            },
        )
        .unwrap();
        assert_eq!(expanded, [14, 12, 13]);
        // A zero primitive count is a D3D_OK no-op: the caller's scratch is
        // emptied rather than filled from stale data.
        let mut output = vec![0xAA; 8];
        expand_indexed_into(
            &mut output,
            &vertices,
            &indices,
            IndexedDraw {
                primitive_count: 0,
                ..draw()
            },
        )
        .unwrap();
        assert!(output.is_empty());
    }
    #[test]
    fn mip_layout_and_cpu_formats() {
        assert_eq!(
            level_layout(8, 4, 0, 3, 23).unwrap(),
            D3d8LevelLayout {
                width: 1,
                height: 1,
                pitch: 2,
                size: 2,
                levels: 4
            }
        );
        assert!(level_layout(8, 4, 0, 4, 23).is_err());
        assert_eq!(level_layout(1, 1, 99, 15, 21).unwrap().levels, 16);
        assert!(level_layout(u32::MAX, 1, 1, 0, 21).is_err());
        assert_eq!(format_bytes(41), 0); // P8 requires palette support.
        assert_eq!(format_bytes(36), 8);
        assert_eq!(format_bytes(60), 2);
        let bump = level_layout(128, 128, 5, 4, 60).unwrap();
        assert_eq!(
            (bump.width, bump.height, bump.pitch, bump.size),
            (8, 8, 16, 128)
        );
    }
    #[test]
    fn borrowed_vertices_keep_original_storage_and_trailing_bytes() {
        let bytes = [0; 17];
        let view = VertexBuffer::borrowed(&bytes, 16).unwrap();
        assert_eq!(view.bytes().as_ptr(), bytes.as_ptr());
        assert_eq!(view.bytes().len(), 17);
    }
    #[test]
    fn reused_expansion_truncates_and_overwrites_every_byte() {
        // One wide draw fills the scratch, then a narrow one reuses it. The
        // result must be exactly the narrow expansion: a shorter draw must not
        // expose bytes the wide draw left behind.
        let vertices = [10u8, 11, 12, 13, 14, 15, 16];
        let wide_indices = [0u16, 1, 2, 3, 4, 5]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let narrow_indices = [2u16, 0, 1]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let wide = IndexedDraw {
            topology: 4,
            index_format: 101,
            stride: 1,
            base_vertex: 0,
            min_index: 0,
            num_vertices: 6,
            start_index: 0,
            primitive_count: 2,
        };
        let narrow = IndexedDraw {
            primitive_count: 1,
            num_vertices: 3,
            ..wide
        };
        let mut scratch = Vec::new();
        expand_indexed_into(&mut scratch, &vertices, &wide_indices, wide).unwrap();
        assert_eq!(scratch, [10, 11, 12, 13, 14, 15]);
        expand_indexed_into(&mut scratch, &vertices, &narrow_indices, narrow).unwrap();
        assert_eq!(scratch, [12, 10, 11]);
        // And the reverse order: the narrow draw first must not shrink the
        // reused buffer's capacity so the wide draw still comes out complete.
        expand_indexed_into(&mut scratch, &vertices, &narrow_indices, narrow).unwrap();
        expand_indexed_into(&mut scratch, &vertices, &wide_indices, wide).unwrap();
        assert_eq!(scratch, [10, 11, 12, 13, 14, 15]);
    }
}

#[cfg(test)]
mod nonindexed_tests {
    use super::{VertexBuffer, expand_nonindexed_into};

    #[test]
    fn strips_and_fans_preserve_records_start_and_winding() {
        let bytes = [90, 91, 0, 10, 1, 11, 2, 12, 3, 13, 4, 14];
        let vb = VertexBuffer::borrowed(&bytes, 2).unwrap();
        let mut packed = Vec::new();
        expand_nonindexed_into(&mut packed, &vb, 5, 1, 3).unwrap();
        assert_eq!(
            packed,
            [
                0, 10, 1, 11, 2, 12, 2, 12, 1, 11, 3, 13, 2, 12, 3, 13, 4, 14
            ]
        );
        expand_nonindexed_into(&mut packed, &vb, 6, 1, 3).unwrap();
        assert_eq!(
            packed,
            [
                0, 10, 1, 11, 2, 12, 0, 10, 2, 12, 3, 13, 0, 10, 3, 13, 4, 14
            ]
        );
        expand_nonindexed_into(&mut packed, &vb, 6, u32::MAX, 0).unwrap();
        assert!(packed.is_empty());
    }

    #[test]
    fn expansion_rejects_short_ranges_and_wrapping_counts() {
        let vb = VertexBuffer::borrowed(&[0; 8], 2).unwrap();
        for (start, count) in [(0, 3), (3, 1), (u32::MAX, 1), (0, u32::MAX)] {
            assert!(expand_nonindexed_into(&mut Vec::new(), &vb, 6, start, count).is_err());
        }
        assert!(expand_nonindexed_into(&mut Vec::new(), &vb, 2, 0, 1).is_err());
    }
}
