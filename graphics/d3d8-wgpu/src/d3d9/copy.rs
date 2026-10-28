use super::format;
use super::gpu::D3d9Gpu;
use super::{SurfaceView, TargetRect};
use crate::RenderError;
use wgpu::util::DeviceExt;

const D3DTEXF_NONE: u32 = 0;
const D3DTEXF_POINT: u32 = 1;
const D3DTEXF_LINEAR: u32 = 2;

const BLIT_WGSL: &str = r#"
struct CopyParams {
    src_min: vec2<f32>,
    src_size: vec2<f32>,
    dst_min: vec2<f32>,
    dst_size: vec2<f32>,
    options: vec4<f32>,
};
@group(0) @binding(0) var src_texture: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
@group(0) @binding(2) var<uniform> params: CopyParams;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(positions[index], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = params.src_min + (position.xy - params.dst_min) / params.dst_size * params.src_size;
    let texel = textureSample(src_texture, src_sampler, uv);
    return vec4<f32>(texel.rgb, mix(texel.a, 1.0, params.options.x));
}
"#;

const FILL_WGSL: &str = r#"
struct FillParams {
    color: vec4<f32>,
};
@group(0) @binding(0) var<uniform> params: FillParams;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(positions[index], 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return params.color;
}
"#;

#[derive(Clone, Copy)]
struct Rect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Rect {
    fn width(self) -> u32 {
        self.right - self.left
    }

    fn height(self) -> u32 {
        self.bottom - self.top
    }

    fn is_empty(self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

fn clamp_rect(
    rect: Option<TargetRect>,
    width: u32,
    height: u32,
    op: &'static str,
) -> Result<Rect, RenderError> {
    let (left, top, right, bottom) = match rect {
        Some(rect) => (
            rect.left as i64,
            rect.top as i64,
            rect.right as i64,
            rect.bottom as i64,
        ),
        None => (0, 0, width as i64, height as i64),
    };
    if right < left || bottom < top {
        return Err(RenderError::invalid(op, "inverted rectangle"));
    }
    let clamp = |value: i64, limit: u32| value.clamp(0, limit as i64) as u32;
    Ok(Rect {
        left: clamp(left, width),
        top: clamp(top, height),
        right: clamp(right, width),
        bottom: clamp(bottom, height),
    })
}

fn fit_rect(
    rect: Option<TargetRect>,
    width: u32,
    height: u32,
    op: &'static str,
) -> Result<Rect, RenderError> {
    let (left, top, right, bottom) = match rect {
        Some(rect) => (
            rect.left as i64,
            rect.top as i64,
            rect.right as i64,
            rect.bottom as i64,
        ),
        None => (0, 0, width as i64, height as i64),
    };
    if left < 0
        || top < 0
        || right < left
        || bottom < top
        || right > width as i64
        || bottom > height as i64
    {
        return Err(RenderError::invalid(op, "rectangle is outside the surface"));
    }
    Ok(Rect {
        left: left as u32,
        top: top as u32,
        right: right as u32,
        bottom: bottom as u32,
    })
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn same_subresource(a: &SurfaceView, b: &SurfaceView) -> bool {
    a.texture == b.texture && a.subresource == b.subresource
}

fn wgpu_format(view: &SurfaceView, op: &'static str) -> Result<wgpu::TextureFormat, RenderError> {
    format::d3d_to_wgpu(view.format)
        .map(|_| view.texture.format())
        .ok_or_else(|| {
            RenderError::new(
                op,
                format!("D3D format {} has no wgpu mapping", view.format),
            )
        })
}

fn opaque_format(format_value: u32) -> bool {
    matches!(
        format::info(format_value),
        Some(format::FormatInfo {
            conversion: format::Conversion::BgraOpaque | format::Conversion::RgbaOpaque,
            ..
        })
    )
}

pub(crate) fn supports_format_conversion(src: u32, dst: u32) -> bool {
    matches!(src, format::D3DFMT_A8R8G8B8 | format::D3DFMT_X8R8G8B8)
        && matches!(dst, format::D3DFMT_A8R8G8B8 | format::D3DFMT_X8R8G8B8)
}

fn reject_depth(src: &SurfaceView, dst: &SurfaceView, op: &'static str) -> Result<(), RenderError> {
    if format::is_depth(src.format) || format::is_depth(dst.format) {
        return Err(RenderError::new(
            op,
            "depth/stencil surfaces are not valid copy operands",
        ));
    }
    Ok(())
}

fn reject_volume(
    src: &SurfaceView,
    dst: Option<&SurfaceView>,
    op: &'static str,
) -> Result<(), RenderError> {
    if src.volume || dst.is_some_and(|view| view.volume) {
        return Err(RenderError::new(
            op,
            "volume texture subresources are not supported for copies",
        ));
    }
    Ok(())
}

fn reject_block(src: &SurfaceView, dst: &SurfaceView, op: &'static str) -> Result<(), RenderError> {
    if format::is_block(src.format) || format::is_block(dst.format) {
        return Err(RenderError::new(
            op,
            "block-compressed formats are not valid copy operands",
        ));
    }
    Ok(())
}

fn subresource_view(
    source: &SurfaceView,
    op: &'static str,
    label: &'static str,
) -> Result<wgpu::TextureView, RenderError> {
    if source.volume {
        return Err(RenderError::new(
            op,
            "volume texture subresources are not supported for copies",
        ));
    }
    let (face, level, _slice) = source.subresource;
    Ok(source.texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some(label),
        format: None,
        dimension: Some(wgpu::TextureViewDimension::D2),
        aspect: wgpu::TextureAspect::All,
        base_mip_level: level,
        mip_level_count: Some(1),
        base_array_layer: if source.cube { face } else { 0 },
        array_layer_count: Some(1),
        usage: None,
    }))
}

fn raw_copy(
    gpu: &D3d9Gpu,
    src: &SurfaceView,
    src_rect: Rect,
    dst: &SurfaceView,
    dst_rect: Rect,
    op: &'static str,
) -> Result<(), RenderError> {
    let src_format = wgpu_format(src, op)?;
    let dst_format = wgpu_format(dst, op)?;
    if src_format != dst_format {
        return Err(RenderError::new(
            op,
            "raw copy requires identical wgpu formats",
        ));
    }
    if !src.texture.usage().contains(wgpu::TextureUsages::COPY_SRC)
        || !dst.texture.usage().contains(wgpu::TextureUsages::COPY_DST)
    {
        return Err(RenderError::new(op, "texture lacks copy usage"));
    }
    let (src_face, src_level, src_slice) = src.subresource;
    let (dst_face, dst_level, dst_slice) = dst.subresource;
    let src_z = if src.volume { src_slice } else { src_face };
    let dst_z = if dst.volume { dst_slice } else { dst_face };
    gpu.validate(op, || {
        gpu.record(|encoder, _| {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &src.texture,
                    mip_level: src_level,
                    origin: wgpu::Origin3d {
                        x: src_rect.left,
                        y: src_rect.top,
                        z: src_z,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &dst.texture,
                    mip_level: dst_level,
                    origin: wgpu::Origin3d {
                        x: dst_rect.left,
                        y: dst_rect.top,
                        z: dst_z,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: src_rect.width(),
                    height: src_rect.height(),
                    depth_or_array_layers: 1,
                },
            );
        });
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn blit(
    gpu: &D3d9Gpu,
    src: &SurfaceView,
    src_rect: Rect,
    dst: &SurfaceView,
    dst_rect: Rect,
    filter: u32,
    op: &'static str,
) -> Result<(), RenderError> {
    let src_format = wgpu_format(src, op)?;
    let dst_format = wgpu_format(dst, op)?;
    let src_features = src_format.guaranteed_format_features(gpu.features());
    if !src_features
        .flags
        .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
    {
        return Err(RenderError::new(op, "source format is not filterable"));
    }
    let dst_features = dst_format.guaranteed_format_features(gpu.features());
    if !dst_features
        .allowed_usages
        .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
    {
        return Err(RenderError::new(
            op,
            "destination format is not a valid render attachment",
        ));
    }
    if !dst
        .texture
        .usage()
        .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
    {
        return Err(RenderError::new(
            op,
            "destination texture lacks render-attachment usage",
        ));
    }
    let src_view = subresource_view(src, op, "d3d9-copy-source")?;
    let dst_view = subresource_view(dst, op, "d3d9-copy-target")?;
    let params = [
        src_rect.left as f32 / src.width as f32,
        src_rect.top as f32 / src.height as f32,
        src_rect.width() as f32 / src.width as f32,
        src_rect.height() as f32 / src.height as f32,
        dst_rect.left as f32,
        dst_rect.top as f32,
        dst_rect.width() as f32,
        dst_rect.height() as f32,
        if opaque_format(src.format) || opaque_format(dst.format) {
            1.0
        } else {
            0.0
        },
        0.0,
        0.0,
        0.0,
    ];
    let filter_mode = if filter == D3DTEXF_LINEAR {
        wgpu::FilterMode::Linear
    } else {
        wgpu::FilterMode::Nearest
    };
    gpu.validate(op, || {
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("d3d9-copy"),
                source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
            });
        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("d3d9-copy"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let uniform = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("d3d9-copy-params"),
                contents: bytemuck::cast_slice(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("d3d9-copy-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: filter_mode,
            min_filter: filter_mode,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            lod_min_clamp: 0.0,
            lod_max_clamp: 0.0,
            ..Default::default()
        });
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("d3d9-copy"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("d3d9-copy"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("d3d9-copy"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: dst_format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        gpu.record(|encoder, _| {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("d3d9-copy"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.set_scissor_rect(
                dst_rect.left,
                dst_rect.top,
                dst_rect.width(),
                dst_rect.height(),
            );
            pass.draw(0..3, 0..1);
        });
        Ok(())
    })
}

pub(crate) fn stretch_rect(
    gpu: &D3d9Gpu,
    src: &SurfaceView,
    src_rect: Option<TargetRect>,
    dst: &SurfaceView,
    dst_rect: Option<TargetRect>,
    filter: u32,
) -> Result<(), RenderError> {
    const OP: &str = "StretchRect";
    if !matches!(filter, D3DTEXF_NONE | D3DTEXF_POINT | D3DTEXF_LINEAR) {
        return Err(RenderError::new(
            OP,
            format!("D3DTEXF filter {filter} is not supported by StretchRect"),
        ));
    }
    if format::is_depth(src.format) || format::is_depth(dst.format) {
        if src.format != dst.format
            || src.width != dst.width
            || src.height != dst.height
            || filter != D3DTEXF_NONE
        {
            return Err(RenderError::invalid(
                OP,
                "depth copy requires matching whole surfaces and D3DTEXF_NONE",
            ));
        }
        let full = |r: Option<TargetRect>, w: u32, h: u32| {
            r.is_none_or(|r| {
                r.left == 0 && r.top == 0 && r.right == w as i32 && r.bottom == h as i32
            })
        };
        if !full(src_rect, src.width, src.height) || !full(dst_rect, dst.width, dst.height) {
            return Err(RenderError::invalid(
                OP,
                "depth subrectangle copies are invalid",
            ));
        }
        if src.texture == dst.texture {
            return Err(RenderError::invalid(
                OP,
                "source and destination depth surfaces must differ",
            ));
        }
        let region = Rect {
            left: 0,
            top: 0,
            right: src.width,
            bottom: src.height,
        };
        return raw_copy(gpu, src, region, dst, region, OP);
    }
    reject_depth(src, dst, OP)?;
    reject_volume(src, Some(dst), OP)?;
    reject_block(src, dst, OP)?;
    let src_rect = clamp_rect(src_rect, src.width, src.height, OP)?;
    let dst_rect = clamp_rect(dst_rect, dst.width, dst.height, OP)?;
    if src_rect.is_empty() || dst_rect.is_empty() {
        return Ok(());
    }
    if same_subresource(src, dst) && overlaps(src_rect, dst_rect) {
        return Err(RenderError::invalid(
            OP,
            "source and destination rectangles overlap on the same surface",
        ));
    }
    let same_format = src.format == dst.format;
    let same_extent =
        src_rect.width() == dst_rect.width() && src_rect.height() == dst_rect.height();
    if same_format && same_extent {
        return raw_copy(gpu, src, src_rect, dst, dst_rect, OP);
    }
    if !same_format && !supports_format_conversion(src.format, dst.format) {
        return Err(RenderError::new(
            OP,
            format!(
                "format conversion from D3DFORMAT {} to {} is not supported",
                src.format, dst.format
            ),
        ));
    }
    if src.texture == dst.texture {
        return Err(RenderError::new(
            OP,
            "same-surface scaling is not supported",
        ));
    }
    blit(gpu, src, src_rect, dst, dst_rect, filter, OP)
}

pub(crate) fn update_surface(
    gpu: &D3d9Gpu,
    src: &SurfaceView,
    src_rect: Option<TargetRect>,
    dst: &SurfaceView,
    dst_point: (i32, i32),
) -> Result<(), RenderError> {
    const OP: &str = "UpdateSurface";
    reject_depth(src, dst, OP)?;
    reject_volume(src, Some(dst), OP)?;
    reject_block(src, dst, OP)?;
    if src.format != dst.format {
        return Err(RenderError::new(
            OP,
            "format conversion is not supported by UpdateSurface",
        ));
    }
    let src_rect = fit_rect(src_rect, src.width, src.height, OP)?;
    if src_rect.is_empty() {
        return Ok(());
    }
    let (x, y) = dst_point;
    if x < 0 || y < 0 {
        return Err(RenderError::invalid(OP, "negative destination point"));
    }
    let dst_rect = Rect {
        left: x as u32,
        top: y as u32,
        right: x as u32 + src_rect.width(),
        bottom: y as u32 + src_rect.height(),
    };
    if dst_rect.right > dst.width || dst_rect.bottom > dst.height {
        return Err(RenderError::invalid(
            OP,
            "destination rectangle is outside the surface",
        ));
    }
    if same_subresource(src, dst) && overlaps(src_rect, dst_rect) {
        return Err(RenderError::invalid(
            OP,
            "source and destination rectangles overlap on the same surface",
        ));
    }
    raw_copy(gpu, src, src_rect, dst, dst_rect, OP)
}

pub(crate) fn copy_to_cpu(
    gpu: &D3d9Gpu,
    src: &SurfaceView,
    rect: Option<TargetRect>,
    out: &mut [u8],
) -> Result<(), RenderError> {
    const OP: &str = "GetRenderTargetData";
    reject_volume(src, None, OP)?;
    if format::is_depth(src.format) && !format::depth_readback_supported(src.format) {
        return Err(RenderError::new(OP, "depth format cannot be read back"));
    }
    let (face, level, slice) = src.subresource;
    let layer = if src.volume { slice } else { face };
    let physical = format::physical_total(src.format, src.width, src.height, 1)
        .ok_or_else(|| RenderError::new(OP, "source layout overflow"))?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(physical as usize)
        .map_err(|_| RenderError::out_of_memory(OP, "readback allocation failed"))?;
    raw.resize(physical as usize, 0);
    gpu.read_texture(
        &src.texture,
        src.format,
        level,
        src.width,
        src.height,
        1,
        layer,
        src.volume,
        &mut raw,
    )?;
    let packed = format::readback_bytes(src.format, src.width, src.height, 1, &raw)?;
    match rect {
        None => {
            if out.len() < packed.len() {
                return Err(RenderError::invalid(
                    OP,
                    "destination is smaller than the source level",
                ));
            }
            out[..packed.len()].copy_from_slice(&packed);
            Ok(())
        }
        Some(rect) => {
            if format::is_block(src.format) {
                return Err(RenderError::new(
                    OP,
                    "rectangle readback of block-compressed formats is not supported",
                ));
            }
            let rect = clamp_rect(Some(rect), src.width, src.height, OP)?;
            if rect.is_empty() {
                return Ok(());
            }
            let src_pitch = format::row_pitch(src.format, src.width)
                .ok_or_else(|| RenderError::new(OP, "invalid source pitch"))?
                as usize;
            let row_bytes = format::row_pitch(src.format, rect.width())
                .ok_or_else(|| RenderError::new(OP, "invalid rectangle pitch"))?
                as usize;
            let needed = row_bytes
                .checked_mul(rect.height() as usize)
                .ok_or_else(|| RenderError::new(OP, "readback size overflow"))?;
            if out.len() < needed {
                return Err(RenderError::invalid(
                    OP,
                    "destination is smaller than the rectangle",
                ));
            }
            let bpp = format::bpp(src.format) as usize;
            for row in 0..rect.height() as usize {
                let src_at = (rect.top as usize + row) * src_pitch + rect.left as usize * bpp;
                let dst_at = row * row_bytes;
                out[dst_at..dst_at + row_bytes]
                    .copy_from_slice(&packed[src_at..src_at + row_bytes]);
            }
            Ok(())
        }
    }
}

pub(crate) fn color_fill(
    gpu: &D3d9Gpu,
    dst: &SurfaceView,
    rect: Option<TargetRect>,
    color: u32,
) -> Result<(), RenderError> {
    const OP: &str = "ColorFill";
    reject_volume(dst, None, OP)?;
    if format::is_depth(dst.format) {
        return Err(RenderError::new(
            OP,
            "depth/stencil surfaces cannot be color filled",
        ));
    }
    if format::is_block(dst.format) {
        return Err(RenderError::new(
            OP,
            "block-compressed surfaces cannot be color filled",
        ));
    }
    let rect = clamp_rect(rect, dst.width, dst.height, OP)?;
    if rect.is_empty() {
        return Ok(());
    }
    let dst_format = wgpu_format(dst, OP)?;
    let features = dst_format.guaranteed_format_features(gpu.features());
    if !features
        .allowed_usages
        .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
    {
        return Err(RenderError::new(
            OP,
            "destination format is not a valid render attachment",
        ));
    }
    if !dst
        .texture
        .usage()
        .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
    {
        return Err(RenderError::new(
            OP,
            "destination texture lacks render-attachment usage",
        ));
    }
    let dst_view = subresource_view(dst, OP, "d3d9-fill-target")?;
    let alpha = if opaque_format(dst.format) {
        1.0
    } else {
        ((color >> 24) & 0xff) as f32 / 255.0
    };
    let params = [
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0,
        alpha,
    ];
    gpu.validate(OP, || {
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("d3d9-fill"),
                source: wgpu::ShaderSource::Wgsl(FILL_WGSL.into()),
            });
        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("d3d9-fill"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });
        let uniform = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("d3d9-fill-params"),
                contents: bytemuck::cast_slice(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("d3d9-fill"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("d3d9-fill"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("d3d9-fill"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: dst_format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        gpu.record(|encoder, _| {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("d3d9-fill"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.set_scissor_rect(rect.left, rect.top, rect.width(), rect.height());
            pass.draw(0..3, 0..1);
        });
        Ok(())
    })
}
