use super::*;

pub(super) fn stream_for<'a>(
    command: &'a DrawCommand,
    stream: u32,
) -> Result<&'a StreamData, RenderError> {
    command
        .streams
        .iter()
        .find(|s| s.stream == stream)
        .ok_or_else(|| unsupported("Draw", format!("vertex stream {stream} is not bound")))
}

pub(super) struct VertexFetch<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) stride: usize,
    pub(super) size: usize,
    pub(super) element_type: u8,
}

pub(crate) fn vertex_element_bytes(
    bytes: &[u8],
    stream: u32,
    stride: u32,
    stream_offset: u32,
    offset: u64,
    size: usize,
    index: u32,
) -> Result<&[u8], RenderError> {
    let base = (index as usize)
        .checked_mul(stride as usize)
        .and_then(|v| v.checked_add(stream_offset as usize))
        .and_then(|v| v.checked_add(offset as usize))
        .ok_or_else(|| invalid("Draw", "vertex offset overflow"))?;
    let end = base
        .checked_add(size)
        .ok_or_else(|| invalid("Draw", "vertex offset overflow"))?;
    if end > bytes.len() {
        return Err(invalid(
            "Draw",
            format!(
                "vertex fetch is outside stream {stream}: stride {stride}, stream offset {stream_offset}, element offset {offset}, element size {size}, index {index}, required end {end}, available {}",
                bytes.len()
            ),
        ));
    }
    Ok(&bytes[base..end])
}

impl<'a> VertexFetch<'a> {
    pub(super) fn prepare(
        command: &'a DrawCommand,
        stream: u16,
        offset: u16,
        element_type: u8,
        max_index: u32,
    ) -> Result<Self, RenderError> {
        let data = stream_for(command, stream as u32)?;
        let size = element_size(element_type)
            .ok_or_else(|| unsupported("Draw", "unsupported vertex element type"))?
            as usize;
        let start = (data.offset as usize)
            .checked_add(offset as usize)
            .ok_or_else(|| invalid("Draw", "vertex offset overflow"))?;
        let end = (max_index as usize)
            .checked_mul(data.stride as usize)
            .and_then(|v| v.checked_add(start))
            .and_then(|v| v.checked_add(size))
            .ok_or_else(|| invalid("Draw", "vertex offset overflow"))?;
        if end > data.bytes.len() {
            return Err(invalid(
                "Draw",
                format!(
                    "vertex fetch is outside stream {stream}: stride {}, stream offset {}, element offset {offset}, element size {size}, max index {max_index}, required end {end}, available {}; primitive {}, primitives {}, indexed {}, base vertex {}, min vertex {}, vertex count {}, start {}, index format {}",
                    data.stride,
                    data.offset,
                    data.bytes.len(),
                    command.primitive,
                    command.primitive_count,
                    command.indexed,
                    command.base_vertex,
                    command.min_vertex,
                    command.num_vertices,
                    command.start_vertex,
                    command.index_format
                ),
            ));
        }
        Ok(Self {
            bytes: &data.bytes[start..],
            stride: data.stride as usize,
            size,
            element_type,
        })
    }

    fn bytes(&self, index: u32) -> &[u8] {
        let base = index as usize * self.stride;
        &self.bytes[base..base + self.size]
    }

    pub(super) fn fetch(&self, index: u32) -> Result<[f32; 4], RenderError> {
        expand_element(self.element_type, self.bytes(index))
    }
}

pub(super) fn native_vertex_format(element_type: u8) -> Option<wgpu::VertexFormat> {
    use wgpu::VertexFormat as F;
    Some(match element_type {
        D3DDECLTYPE_FLOAT1 => F::Float32,
        D3DDECLTYPE_FLOAT2 => F::Float32x2,
        D3DDECLTYPE_FLOAT3 => F::Float32x3,
        D3DDECLTYPE_FLOAT4 => F::Float32x4,
        D3DDECLTYPE_D3DCOLOR => F::Unorm8x4Bgra,
        D3DDECLTYPE_UBYTE4N => F::Unorm8x4,
        D3DDECLTYPE_SHORT2N => F::Snorm16x2,
        D3DDECLTYPE_SHORT4N => F::Snorm16x4,
        D3DDECLTYPE_USHORT2N => F::Unorm16x2,
        D3DDECLTYPE_USHORT4N => F::Unorm16x4,
        D3DDECLTYPE_FLOAT16_2 => F::Float16x2,
        D3DDECLTYPE_FLOAT16_4 => F::Float16x4,
        _ => return None,
    })
}

pub(super) enum GeometryData {
    Buffer(wgpu::Buffer),
    Stream(Arc<Vec<u8>>),
    Upload,
}

impl GeometryData {
    pub(super) fn slice<'a>(
        &'a self,
        upload: &'a wgpu::Buffer,
        offset: u64,
        size: u64,
    ) -> wgpu::BufferSlice<'a> {
        match self {
            GeometryData::Buffer(buffer) => buffer.slice(offset..offset + size),
            _ => upload.slice(offset..offset + size),
        }
    }
}

pub(super) struct NativeStream {
    pub(super) data: GeometryData,
    pub(super) offset: u64,
    pub(super) size: u64,
}

pub(super) type NativeLayoutKey = SmallVec<[(u32, u32, LayoutAttribute); 8]>;

pub(super) struct NativeLayout {
    pub(super) streams: SmallVec<[u32; 2]>,
    pub(super) layouts: VertexLayouts,
}

pub(super) struct NativeGeometryPlan {
    pub(super) layout: Arc<NativeLayout>,
    pub(super) topology: wgpu::PrimitiveTopology,
    pub(super) strip_format: Option<wgpu::IndexFormat>,
    pub(super) first: u64,
    pub(super) span: u64,
    pub(super) index_offset: u64,
    pub(super) index_size: u64,
    pub(super) index_format: wgpu::IndexFormat,
    pub(super) base_vertex: i32,
    pub(super) count: u32,
}

pub(super) struct NativeGeometry {
    pub(super) vertices: SmallVec<[NativeStream; 2]>,
    pub(super) layout: Arc<NativeLayout>,
    pub(super) topology: wgpu::PrimitiveTopology,
    pub(super) strip_format: Option<wgpu::IndexFormat>,
    pub(super) index: GeometryData,
    pub(super) index_offset: u64,
    pub(super) index_size: u64,
    pub(super) index_format: wgpu::IndexFormat,
    pub(super) base_vertex: i32,
    pub(super) count: u32,
}

pub(super) struct CachedGeometry {
    pub(super) bytes: Weak<Vec<u8>>,
    pub(super) buffer: wgpu::Buffer,
}

pub(super) struct CachedIndexBounds {
    pub(super) bytes: Weak<Vec<u8>>,
    pub(super) ranges: FastMap<(u32, u32, u32), (u32, u32)>,
}

pub(super) struct PackedVertices {
    pub(super) bytes: Vec<u8>,
    pub(super) attributes: Vec<wgpu::VertexAttribute>,
    pub(super) stride: u64,
}

pub(super) fn packed_shader_layout(
    linked: &[LinkedInput],
    canonical: bool,
) -> Result<(Vec<wgpu::VertexAttribute>, u64), RenderError> {
    let mut stride = 0;
    let mut attributes = Vec::with_capacity(linked.len());
    for input in linked {
        let format = if canonical {
            None
        } else {
            native_vertex_format(input.element_type)
        }
        .unwrap_or(wgpu::VertexFormat::Float32x4);
        attributes.push(wgpu::VertexAttribute {
            format,
            offset: stride,
            shader_location: input.location,
        });
        stride += format.size();
    }
    if canonical {
        let target = crate::shader::uber::VERTEX_STRIDE;
        if stride > target {
            return Err(invalid("Draw", "canonical vertex input capacity exceeded"));
        }
        while attributes.len() < crate::shader::uber::VERTEX_INPUTS as usize {
            attributes.push(wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: attributes.len() as u64 * 16,
                shader_location: attributes.len() as u32,
            });
        }
        stride = target;
    }
    Ok((attributes, stride))
}

pub(super) fn pack_shader_vertices(
    command: &DrawCommand,
    linked: &[LinkedInput],
    order: &[u32],
    canonical: bool,
) -> Result<PackedVertices, RenderError> {
    struct Copy<'a> {
        fetch: VertexFetch<'a>,
        native: bool,
        stream: u16,
        offset: usize,
    }
    let mut copies: Vec<Copy<'_>> = Vec::with_capacity(linked.len());
    let (attributes, stride) = packed_shader_layout(linked, canonical)?;
    let mut copied_stride = 0u64;
    let max_index = order.iter().copied().max();
    for input in linked {
        let native = (!canonical)
            .then(|| native_vertex_format(input.element_type))
            .flatten();
        copied_stride += native.unwrap_or(wgpu::VertexFormat::Float32x4).size();
        if let Some(max_index) = max_index {
            let fetch = VertexFetch::prepare(
                command,
                input.stream,
                input.offset,
                input.element_type,
                max_index,
            )?;
            if let Some(last) = copies.last_mut() {
                if native.is_some()
                    && last.native
                    && last.stream == input.stream
                    && last.offset + last.fetch.size == input.offset as usize
                {
                    last.fetch.size += fetch.size;
                    continue;
                }
            }
            copies.push(Copy {
                fetch,
                native: native.is_some(),
                stream: input.stream,
                offset: input.offset as usize,
            });
        }
    }
    let padding = (stride - copied_stride) as usize;
    let total = order
        .len()
        .checked_mul(stride as usize)
        .ok_or_else(|| invalid("Draw", "vertex buffer size overflow"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| RenderError::out_of_memory("Draw", "vertex repack allocation failed"))?;
    if let [copy] = copies.as_slice() {
        if copy.native
            && copy.fetch.stride == copy.fetch.size
            && order
                .windows(2)
                .all(|pair| u64::from(pair[0]) + 1 == u64::from(pair[1]))
        {
            if let Some(&first) = order.first() {
                let start = first as usize * copy.fetch.stride;
                bytes.extend_from_slice(&copy.fetch.bytes[start..start + total]);
            }
            return Ok(PackedVertices {
                bytes,
                attributes,
                stride,
            });
        }
    }
    for &index in order {
        for copy in &copies {
            if copy.native {
                bytes.extend_from_slice(copy.fetch.bytes(index));
            } else {
                append_f32_le(&mut bytes, &copy.fetch.fetch(index)?);
            }
        }
        bytes.resize(bytes.len() + padding, 0);
    }
    Ok(PackedVertices {
        bytes,
        attributes,
        stride,
    })
}

pub(super) fn plan_native_geometry(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    command: &DrawCommand,
    linked: &[LinkedInput],
    allow_raw_inputs: bool,
) -> Result<Option<NativeGeometryPlan>, RenderError> {
    let topology = match command.primitive {
        D3DPT_POINTLIST => wgpu::PrimitiveTopology::PointList,
        D3DPT_LINELIST => wgpu::PrimitiveTopology::LineList,
        D3DPT_LINESTRIP => wgpu::PrimitiveTopology::LineStrip,
        D3DPT_TRIANGLELIST => wgpu::PrimitiveTopology::TriangleList,
        D3DPT_TRIANGLESTRIP => wgpu::PrimitiveTopology::TriangleStrip,
        _ => return Ok(None),
    };
    if !command.indexed || linked.is_empty() {
        return Ok(None);
    }
    let mut key = NativeLayoutKey::new();
    for input in linked {
        let format = if allow_raw_inputs && input.element_type == D3DDECLTYPE_UBYTE4 {
            Some(wgpu::VertexFormat::Uint8x4)
        } else {
            native_vertex_format(input.element_type)
        };
        let Some(format) = format else {
            return Ok(None);
        };
        let stream_id = u32::from(input.stream);
        let stream = stream_for(command, stream_id)?;
        if stream.stride == 0
            || stream.stride % 4 != 0
            || stream.offset % 4 != 0
            || stream.stride > gpu.limits().max_vertex_buffer_array_stride
        {
            return Ok(None);
        }
        let stride = stream.stride;
        if u64::from(input.offset) % format.size().min(4) != 0
            || u64::from(input.offset) + format.size() > u64::from(stride)
        {
            return Ok(None);
        }
        key.push((
            stream_id,
            stride,
            LayoutAttribute {
                format,
                offset: u32::from(input.offset),
                location: input.location,
            },
        ));
    }
    let layout = match draw.native_layouts.get(&key) {
        Some(layout) => {
            stats::record(|s| s.native_layout_hits += 1);
            Arc::clone(layout)
        }
        None => {
            stats::record(|s| s.native_layout_misses += 1);
            let mut streams = SmallVec::new();
            let mut layouts = VertexLayouts::new();
            for &(stream, stride, attribute) in &key {
                let slot = match streams.iter().position(|id| *id == stream) {
                    Some(slot) => slot,
                    None => {
                        streams.push(stream);
                        layouts.push((u64::from(stride), VertexAttributes::new()));
                        streams.len() - 1
                    }
                };
                layouts[slot].1.push(attribute);
            }
            if streams.len() as u32 > gpu.limits().max_vertex_buffers {
                return Ok(None);
            }
            let layout = Arc::new(NativeLayout { streams, layouts });
            let evicted = draw.native_layouts.insert(key, Arc::clone(&layout), 0);
            stats::record(|s| s.native_layout_evictions += evicted as u64);
            layout
        }
    };
    if !matches!(command.index_format, 101 | 102) {
        return Err(invalid("Draw", "invalid index format"));
    }
    let count = primitive_vertex_count(command.primitive, command.primitive_count)?;
    if count == 0 {
        return Ok(None);
    }
    let start = command.start_vertex as usize;
    let end = start
        .checked_add(count as usize)
        .ok_or_else(|| invalid("Draw", "index range overflow"))?;
    if end > index_count(command.index_format, &command.index_bytes) {
        return Err(invalid("Draw", "index range exceeds the index stream"));
    }
    let (min_raw, max_raw) = draw.index_bounds(command, count);
    let max_index = i64::from(max_raw) + i64::from(command.base_vertex);
    if i64::from(min_raw) + i64::from(command.base_vertex) < 0 || max_index > i64::from(u32::MAX) {
        return Err(invalid(
            "Draw",
            "index plus base vertex is outside the vertex window",
        ));
    }
    let max_index = max_index as u32;
    for input in linked {
        VertexFetch::prepare(
            command,
            input.stream,
            input.offset,
            input.element_type,
            max_index,
        )?;
    }
    let strip = matches!(
        topology,
        wgpu::PrimitiveTopology::TriangleStrip | wgpu::PrimitiveTopology::LineStrip
    );
    let (index_stride, index_format, restart) = if command.index_format == 101 {
        (2, wgpu::IndexFormat::Uint16, u32::from(u16::MAX))
    } else {
        (4, wgpu::IndexFormat::Uint32, u32::MAX)
    };
    if (strip && max_raw >= restart) || max_raw > i32::MAX as u32 {
        return Ok(None);
    }
    let first = (i64::from(command.base_vertex) + i64::from(min_raw)) as u64;
    let span = u64::from(max_raw - min_raw + 1);
    Ok(Some(NativeGeometryPlan {
        layout,
        topology,
        strip_format: strip.then_some(index_format),
        first,
        span,
        index_offset: start as u64 * index_stride,
        index_size: u64::from(count) * index_stride,
        index_format,
        base_vertex: -(min_raw as i32),
        count,
    }))
}

pub(super) fn upload_native_geometry(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    command: &DrawCommand,
    plan: NativeGeometryPlan,
) -> Result<Box<NativeGeometry>, RenderError> {
    let index = draw.geometry_source(gpu, &command.index_bytes)?;
    let mut vertices = SmallVec::new();
    for &stream_id in &plan.layout.streams {
        let stream = stream_for(command, stream_id)?;
        let stride = u64::from(stream.stride);
        let offset = u64::from(stream.offset) + plan.first * stride;
        let data = draw.geometry_source(gpu, &stream.bytes)?;
        let size = if matches!(data, GeometryData::Stream(_)) {
            plan.span * stride
        } else {
            stream.bytes.len() as u64 - offset
        };
        vertices.push(NativeStream { data, offset, size });
    }
    let geometry = NativeGeometry {
        vertices,
        layout: plan.layout,
        topology: plan.topology,
        strip_format: plan.strip_format,
        index,
        index_offset: plan.index_offset,
        index_size: plan.index_size,
        index_format: plan.index_format,
        base_vertex: plan.base_vertex,
        count: plan.count,
    };
    Ok(match draw.native_geometry_pool.pop() {
        Some(mut slot) => {
            *slot = geometry;
            slot
        }
        None => Box::new(geometry),
    })
}

impl D3d9DrawState {
    fn index_bounds(&mut self, command: &DrawCommand, count: u32) -> (u32, u32) {
        let key = Arc::as_ptr(&command.index_bytes) as usize;
        let range = (command.start_vertex, count, command.index_format);
        if let Some(&bounds) = self
            .index_bounds
            .get(&key)
            .and_then(|cached| cached.ranges.get(&range))
        {
            stats::record(|s| s.index_hits += 1);
            return bounds;
        }
        stats::record(|s| {
            s.index_misses += 1;
            s.index_scanned += u64::from(count);
            s.index_new_buffers += u64::from(!self.index_bounds.contains_key(&key));
        });
        let mut min = u32::MAX;
        let mut max = 0;
        let start = command.start_vertex as usize;
        for index in start..start + count as usize {
            let raw = indexed_vertex_index(&command.index_bytes, command.index_format, index);
            min = min.min(raw);
            max = max.max(raw);
        }
        if !self.index_bounds.contains_key(&key) {
            if self.index_bounds.len() >= 4096 {
                let mut removed = 0;
                let mut evicted = 0;
                for _ in 0..256 {
                    let oldest = self.index_bounds_order.pop_front().unwrap();
                    let cached = self.index_bounds.remove(&oldest).unwrap();
                    if cached.bytes.strong_count() == 0 {
                        removed += 1;
                    } else {
                        evicted += 1;
                    }
                }
                stats::record(|s| {
                    s.index_cleanup_scans += 1;
                    s.index_cleanup_entries += 256;
                    s.index_cleanup_removed += removed;
                    s.index_buffer_evictions += evicted;
                });
            }
            self.index_bounds_order.push_back(key);
        }
        let cached = self
            .index_bounds
            .entry(key)
            .or_insert_with(|| CachedIndexBounds {
                bytes: Arc::downgrade(&command.index_bytes),
                ranges: FastMap::default(),
            });
        if cached.ranges.len() >= 256 {
            stats::record(|s| s.index_range_evictions += 1);
            evict_one(&mut cached.ranges);
        }
        cached.ranges.insert(range, (min, max));
        (min, max)
    }

    pub(super) fn geometry_source(
        &mut self,
        gpu: &D3d9Gpu,
        bytes: &Arc<Vec<u8>>,
    ) -> Result<GeometryData, RenderError> {
        let key = Arc::as_ptr(bytes) as usize;
        if let Some(cached) = self.geometry.get(&key) {
            return Ok(GeometryData::Buffer(cached.buffer.clone()));
        }
        match self.geometry_seen.get(&key) {
            Some((seen, frame)) if seen.strong_count() != 0 => {
                if *frame == self.frame {
                    return Ok(GeometryData::Stream(Arc::clone(bytes)));
                }
                self.geometry_seen.remove(&key);
                return self.geometry_buffer(gpu, bytes).map(GeometryData::Buffer);
            }
            _ => {}
        }
        if self.geometry_seen.len() >= 4096 {
            self.geometry_seen
                .retain(|_, (seen, _)| seen.strong_count() != 0);
        }
        self.geometry_seen
            .insert(key, (Arc::downgrade(bytes), self.frame));
        Ok(GeometryData::Stream(Arc::clone(bytes)))
    }

    fn geometry_buffer(
        &mut self,
        gpu: &D3d9Gpu,
        bytes: &Arc<Vec<u8>>,
    ) -> Result<wgpu::Buffer, RenderError> {
        let key = Arc::as_ptr(bytes) as usize;
        if let Some(cached) = self.geometry.get(&key) {
            return Ok(cached.buffer.clone());
        }
        if bytes.len() as u64 > gpu.limits().max_buffer_size {
            return Err(RenderError::out_of_memory(
                "Draw",
                "geometry exceeds the device buffer limit",
            ));
        }
        self.geometry
            .retain(|_, cached| cached.bytes.strong_count() != 0);
        let total: u64 = self
            .geometry
            .values()
            .map(|cached| cached.buffer.size())
            .sum();
        if total.saturating_add(bytes.len() as u64) > 128 << 20 {
            self.geometry.clear();
        }
        let buffer = gpu.validate("Draw", || {
            Ok(make_buffer(
                gpu,
                "d3d9-geometry",
                bytes,
                wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::INDEX,
            ))
        })?;
        if trace_compilation() {
            self.geometry_upload_bytes += bytes.len() as u64;
        }
        self.geometry.insert(
            key,
            CachedGeometry {
                bytes: Arc::downgrade(bytes),
                buffer: buffer.clone(),
            },
        );
        Ok(buffer)
    }
}
