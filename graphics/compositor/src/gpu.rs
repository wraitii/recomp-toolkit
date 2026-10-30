use crate::{Blend, Draw, Source};
use recomp_gpu::{wgpu, SharedContext};
use std::sync::Arc;
use wgpu::util::DeviceExt;

pub const COMPOSITOR_SHADER: &str = include_str!("compositor.wgsl");

pub struct GpuCompositor {
    context: Arc<SharedContext>,
    layout: wgpu::BindGroupLayout,
    pipelines: [wgpu::RenderPipeline; 3],
    samplers: [wgpu::Sampler; 2],
}

impl GpuCompositor {
    pub fn new(
        context: Arc<SharedContext>,
        format: wgpu::TextureFormat,
    ) -> Result<Self, &'static str> {
        if context.loss.is_lost() {
            return Err("compositor: physical device lost");
        }
        let device = &context.device;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("compositor"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 16,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 24,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("compositor"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("compositor"),
            source: wgpu::ShaderSource::Wgsl(COMPOSITOR_SHADER.into()),
        });
        let pipelines = [Blend::Opaque, Blend::Straight, Blend::Premultiplied].map(|blend| {
            let blend = match blend {
                Blend::Opaque => None,
                Blend::Straight | Blend::Premultiplied => Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: if matches!(blend, Blend::Straight) {
                            wgpu::BlendFactor::SrcAlpha
                        } else {
                            wgpu::BlendFactor::One
                        },
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
            };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("compositor"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        });
        let samplers = [wgpu::FilterMode::Nearest, wgpu::FilterMode::Linear].map(|filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("compositor"),
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        });
        Ok(Self {
            context,
            layout,
            pipelines,
            samplers,
        })
    }

    pub fn context(&self) -> &Arc<SharedContext> {
        &self.context
    }

    pub fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        drawable: [u32; 2],
        draws: &[Draw],
        mut resolve: impl FnMut(&Draw) -> Option<(wgpu::TextureView, wgpu::FilterMode)>,
    ) -> Result<(), &'static str> {
        if self.context.loss.is_lost() {
            return Err("compositor: physical device lost");
        }
        if drawable[0] == 0 || drawable[1] == 0 {
            return Err("compositor: zero drawable dimensions");
        }
        let alignment = self
            .context
            .device
            .limits()
            .min_storage_buffer_offset_alignment
            .max(1) as usize;
        let stride = 48usize.div_ceil(alignment) * alignment;
        let size = stride
            .checked_mul(draws.len())
            .ok_or("compositor: quad buffer too large")?;
        if size as u64 > self.context.device.limits().max_buffer_size {
            return Err("compositor: quad buffer exceeds device limit");
        }
        let mut bytes = vec![0u8; size];
        let mut sources = Vec::with_capacity(draws.len());
        for (index, draw) in draws.iter().enumerate() {
            let (view, filter) = resolve(draw).ok_or("compositor: missing draw source")?;
            let values = [
                draw.dst.x as f32,
                draw.dst.y as f32,
                draw.dst.w as f32,
                draw.dst.h as f32,
                draw.uv.x as f32,
                draw.uv.y as f32,
                draw.uv.w as f32,
                draw.uv.h as f32,
                drawable[0] as f32,
                drawable[1] as f32,
            ];
            if !values.iter().all(|v| v.is_finite()) {
                return Err("compositor: nonfinite draw geometry");
            }
            let start = index * stride;
            for (i, value) in values.iter().enumerate() {
                bytes[start + i * 4..start + i * 4 + 4].copy_from_slice(&value.to_ne_bytes());
            }
            bytes[start + 40..start + 44]
                .copy_from_slice(&u32::from(matches!(draw.blend, Blend::Opaque)).to_ne_bytes());
            let sampler = usize::from(
                !matches!(draw.source, Source::Settings) && filter == wgpu::FilterMode::Linear,
            );
            sources.push((draw.blend as usize, view, sampler, start as u64));
        }
        let buffer = if bytes.is_empty() {
            None
        } else {
            Some(
                self.context
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("compositor quads"),
                        contents: &bytes,
                        usage: wgpu::BufferUsages::STORAGE,
                    }),
            )
        };
        let mut prepared = Vec::with_capacity(sources.len());
        for (blend, view, sampler, offset) in &sources {
            let binding = wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: buffer.as_ref().expect("compositor quad buffer"),
                offset: *offset,
                size: wgpu::BufferSize::new(48),
            });
            let group = self
                .context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("compositor"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: binding.clone(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: binding,
                        },
                        wgpu::BindGroupEntry {
                            binding: 16,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 24,
                            resource: wgpu::BindingResource::Sampler(&self.samplers[*sampler]),
                        },
                    ],
                });
            prepared.push((*blend, group));
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("compositor"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        for (blend, group) in &prepared {
            pass.set_pipeline(&self.pipelines[*blend]);
            pass.set_bind_group(0, group, &[]);
            pass.draw(0..4, 0..1);
        }
        Ok(())
    }
}
