use super::Region;
use wgpu::util::DeviceExt;

#[derive(Clone)]
pub(super) struct DepthRestore {
    pipeline: wgpu::RenderPipeline,
}
impl DepthRestore {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("host-depth-restore"),
            source: wgpu::ShaderSource::Wgsl(
                r#"
@group(0) @binding(0) var<storage, read> words: array<u32>;
struct Params { offset: u32, pitch: u32, x: u32, y: u32 };
@group(0) @binding(1) var<uniform> params: Params;
@vertex fn vs(@builtin(vertex_index) id: u32) -> @builtin(position) vec4f {
    let p = vec2f(f32((id << 1u) & 2u), f32(id & 2u));
    return vec4f(p * 2.0 - 1.0, 0.0, 1.0);
}
@fragment fn fs(@builtin(position) p: vec4f) -> @builtin(frag_depth) f32 {
    let offset = params.offset + (u32(p.y) - params.y) * params.pitch + (u32(p.x) - params.x) * 4u;
    let shift = (offset & 3u) * 8u;
    var bits = words[offset >> 2u] >> shift;
    if shift != 0u { bits |= words[(offset >> 2u) + 1u] << (32u - shift); }
    return bitcast<f32>(bits);
}
"#
                .into(),
            ),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("host-depth-restore"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline }
    }
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::Buffer,
        offset: u64,
        pitch: u32,
        target: &wgpu::Texture,
        region: Region,
        level: u32,
    ) -> Result<(), &'static str> {
        let offset = u32::try_from(offset).map_err(|_| "depth restore offset exceeds u32")?;
        let params = [offset, pitch, region.x as u32, region.y as u32];
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("host-depth-restore-params"),
            contents: bytemuck::cast_slice(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("host-depth-restore"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: source.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor {
            base_mip_level: level,
            mip_level_count: Some(1),
            ..Default::default()
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("host-depth-restore"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_scissor_rect(
            region.x as u32,
            region.y as u32,
            region.w as u32,
            region.h as u32,
        );
        pass.draw(0..3, 0..1);
        Ok(())
    }
}
