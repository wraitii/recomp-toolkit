use super::interface::{InterfaceKey, LayoutAttribute};
use super::pipeline::{PipelineDescriptor, PreparedPipeline};
use crate::RenderError;
use crate::shader::uber::*;
use std::sync::Arc;

pub(crate) const SOURCE: &str = include_str!(concat!(env!("OUT_DIR"), "/d3d9-interpreter.wgsl"));

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FamilyKey {
    interpreter: u32,
    abi: u32,
    instructions: usize,
    temporaries: u32,
    constants: [u32; 2],
    pub(crate) interface: InterfaceKey,
    pub(crate) descriptor: PipelineDescriptor,
}

pub(crate) struct PreparedInterpreter {
    pub(crate) source: Arc<str>,
    pub(crate) families: Vec<PreparedFamily>,
    pub(crate) module: wgpu::ShaderModule,
    pub(crate) layout: wgpu::BindGroupLayout,
    pub(crate) unused_texture: wgpu::TextureView,
    pub(crate) unused_cube: wgpu::TextureView,
    pub(crate) unused_sampler: wgpu::Sampler,
}

pub(crate) struct PreparedFamily {
    pub(crate) key: FamilyKey,
    pub(crate) pipeline: wgpu::RenderPipeline,
}

fn uniform(
    binding: u32,
    size: u64,
    stage: wgpu::ShaderStages,
    dynamic: bool,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: stage,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: wgpu::BufferSize::new(size),
        },
        count: None,
    }
}

impl FamilyKey {
    pub(crate) fn mvp() -> Self {
        let vertex = wgpu::ShaderStages::VERTEX;
        let fragment = wgpu::ShaderStages::FRAGMENT;
        let mut bindings = vec![
            uniform(0, u64::from(VERTEX_CONSTANTS) * 16, vertex, true),
            uniform(1, 272, vertex, true),
            uniform(2, u64::from(PIXEL_CONSTANTS) * 16, fragment, true),
            uniform(3, 64, fragment, true),
            uniform(PROGRAM_BINDINGS[0], PROGRAM_BYTES, vertex, false),
            uniform(PROGRAM_BINDINGS[1], PROGRAM_BYTES, fragment, false),
        ];
        for slot in 0..TEXTURE_SLOTS {
            bindings.push(wgpu::BindGroupLayoutEntry {
                binding: 4 + slot,
                visibility: fragment,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: if slot < TEXTURES_2D {
                        wgpu::TextureViewDimension::D2
                    } else {
                        wgpu::TextureViewDimension::Cube
                    },
                    multisampled: false,
                },
                count: None,
            });
            bindings.push(wgpu::BindGroupLayoutEntry {
                binding: 20 + slot,
                visibility: fragment,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            });
        }
        let component = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        Self {
            interpreter: INTERPRETER_VERSION,
            abi: ABI_VERSION,
            instructions: MAX_INSTRUCTIONS,
            temporaries: TEMP_REGISTERS,
            constants: [VERTEX_CONSTANTS, PIXEL_CONSTANTS],
            interface: InterfaceKey {
                bindings: bindings.into(),
                vertex_buffers: smallvec::smallvec![(
                    VERTEX_STRIDE,
                    (0..VERTEX_INPUTS)
                        .map(|location| LayoutAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: location * 16,
                            location,
                        })
                        .collect()
                )],
            },
            descriptor: PipelineDescriptor {
                entry_points: ["vs_main", "ps_main"],
                target: Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: Some(wgpu::BlendState {
                        color: component,
                        alpha: component,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: wgpu::FrontFace::Cw,
                    ..Default::default()
                },
                depth: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth24PlusStencil8,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState {
                    mask: u64::from(u32::MAX),
                    ..Default::default()
                },
            },
        }
    }

    pub(crate) fn check_limits(&self, limits: &wgpu::Limits) -> Result<(), RenderError> {
        for (name, available, required) in [
            (
                "uniform-bank-bytes",
                u64::from(limits.max_uniform_buffer_binding_size),
                (u64::from(VERTEX_CONSTANTS) * 16).max(PROGRAM_BYTES),
            ),
            (
                "buffer-bytes",
                limits.max_buffer_size,
                (u64::from(VERTEX_CONSTANTS) * 16).max(PROGRAM_BYTES),
            ),
            (
                "dynamic-uniforms",
                u64::from(limits.max_dynamic_uniform_buffers_per_pipeline_layout),
                4,
            ),
            (
                "uniforms-per-stage",
                u64::from(limits.max_uniform_buffers_per_shader_stage),
                3,
            ),
            (
                "bindings",
                u64::from(limits.max_bindings_per_bind_group),
                38,
            ),
            ("bind-groups", u64::from(limits.max_bind_groups), 1),
            (
                "vertex-attributes",
                u64::from(limits.max_vertex_attributes),
                u64::from(VERTEX_INPUTS),
            ),
            ("vertex-buffers", u64::from(limits.max_vertex_buffers), 1),
            (
                "vertex-stride",
                u64::from(limits.max_vertex_buffer_array_stride),
                VERTEX_STRIDE,
            ),
            (
                "inter-stage-variables",
                u64::from(limits.max_inter_stage_shader_variables),
                u64::from(INTER_STAGE_VARIABLES),
            ),
            (
                "textures",
                u64::from(limits.max_sampled_textures_per_shader_stage),
                u64::from(TEXTURE_SLOTS),
            ),
            (
                "samplers",
                u64::from(limits.max_samplers_per_shader_stage),
                u64::from(TEXTURE_SLOTS),
            ),
        ] {
            if available < required {
                return Err(RenderError::new(
                    "InterpreterPrepare",
                    format!("device-limit {name}: {available} < {required}"),
                ));
            }
        }
        Ok(())
    }
}

impl PreparedInterpreter {
    pub(crate) fn find(&self, descriptor: &PipelineDescriptor) -> Option<&PreparedFamily> {
        self.families
            .iter()
            .find(|family| family.key.descriptor == *descriptor)
    }

    pub(crate) fn for_pipeline(&self, pipeline: &wgpu::RenderPipeline) -> Option<&PreparedFamily> {
        self.families
            .iter()
            .find(|family| family.pipeline == *pipeline)
    }

    pub(crate) fn new(
        device: &wgpu::Device,
        expanded: bool,
        trace: bool,
    ) -> Result<Self, RenderError> {
        let key = FamilyKey::mvp();
        key.check_limits(&device.limits())?;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("d3d9-interpreter"),
            entries: &key.interface.bindings,
        });
        let module_started = std::time::Instant::now();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("d3d9-interpreter"),
            source: wgpu::ShaderSource::Wgsl(SOURCE.into()),
        });
        if trace {
            eprintln!(
                "[d3d9-uber] prepare-module ms={:.3} source_bytes={}",
                module_started.elapsed().as_secs_f64() * 1000.0,
                SOURCE.len()
            );
        }
        let mut keys = vec![key];
        if expanded {
            for compare in [wgpu::CompareFunction::Less, wgpu::CompareFunction::Always] {
                let mut key = keys[0].clone();
                let blend = key
                    .descriptor
                    .target
                    .as_mut()
                    .unwrap()
                    .blend
                    .as_mut()
                    .unwrap();
                blend.color.dst_factor = wgpu::BlendFactor::One;
                blend.alpha.dst_factor = wgpu::BlendFactor::One;
                key.descriptor.depth.as_mut().unwrap().depth_compare = Some(compare);
                keys.push(key);
            }
            let mut key = keys[0].clone();
            key.descriptor.target.as_mut().unwrap().format = wgpu::TextureFormat::Rgba8Unorm;
            key.descriptor.primitive.topology = wgpu::PrimitiveTopology::TriangleStrip;
            key.descriptor.primitive.strip_index_format = None;
            key.descriptor.depth.as_mut().unwrap().depth_write_enabled = Some(true);
            keys.push(key);
            let mut key = keys[0].clone();
            key.descriptor.target.as_mut().unwrap().format = wgpu::TextureFormat::Rgba8Unorm;
            key.descriptor.depth.as_mut().unwrap().depth_compare =
                Some(wgpu::CompareFunction::Always);
            keys.push(key);
            for (format, cull_mode, blend, depth, topology) in [
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    None,
                    None,
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    None,
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    None,
                    Some((true, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((wgpu::BlendFactor::One, wgpu::BlendFactor::One)),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((wgpu::BlendFactor::One, wgpu::BlendFactor::One)),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((wgpu::BlendFactor::SrcAlpha, wgpu::BlendFactor::One)),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((true, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    None,
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    None,
                    Some((true, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    None,
                    Some((true, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Back),
                    None,
                    Some((true, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    None,
                    None,
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    None,
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((wgpu::BlendFactor::SrcAlpha, wgpu::BlendFactor::One)),
                    None,
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    None,
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    None,
                    wgpu::PrimitiveTopology::TriangleStrip,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Back),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    None,
                    None,
                    wgpu::PrimitiveTopology::TriangleStrip,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Back),
                    Some((wgpu::BlendFactor::SrcAlpha, wgpu::BlendFactor::One)),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((true, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    None,
                    Some((true, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((true, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    Some((wgpu::BlendFactor::SrcAlpha, wgpu::BlendFactor::One)),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((true, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    None,
                    Some((true, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Back),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    Some((wgpu::BlendFactor::SrcAlpha, wgpu::BlendFactor::One)),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    Some((wgpu::BlendFactor::SrcAlpha, wgpu::BlendFactor::One)),
                    Some((false, wgpu::CompareFunction::Always)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((true, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    Some(wgpu::Face::Front),
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Rgba8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::LessEqual)),
                    wgpu::PrimitiveTopology::TriangleList,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    None,
                    Some((
                        wgpu::BlendFactor::SrcAlpha,
                        wgpu::BlendFactor::OneMinusSrcAlpha,
                    )),
                    Some((false, wgpu::CompareFunction::Less)),
                    wgpu::PrimitiveTopology::TriangleStrip,
                ),
                (
                    wgpu::TextureFormat::Bgra8Unorm,
                    Some(wgpu::Face::Front),
                    Some((wgpu::BlendFactor::SrcAlpha, wgpu::BlendFactor::Zero)),
                    None,
                    wgpu::PrimitiveTopology::TriangleList,
                ),
            ] {
                let mut key = keys[0].clone();
                let target = key.descriptor.target.as_mut().unwrap();
                target.format = format;
                target.blend = blend.map(|(src_factor, dst_factor)| {
                    let component = wgpu::BlendComponent {
                        src_factor,
                        dst_factor,
                        operation: wgpu::BlendOperation::Add,
                    };
                    wgpu::BlendState {
                        color: component,
                        alpha: component,
                    }
                });
                key.descriptor.primitive.cull_mode = cull_mode;
                key.descriptor.primitive.topology = topology;
                key.descriptor.primitive.strip_index_format = None;
                key.descriptor.depth = depth.map(|(write, compare)| {
                    let mut state = keys[0].descriptor.depth.clone().unwrap();
                    state.depth_write_enabled = Some(write);
                    state.depth_compare = Some(compare);
                    state
                });
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        let families = keys
            .into_iter()
            .enumerate()
            .map(|(index, key)| {
                let started = std::time::Instant::now();
                let d = &key.descriptor;
                let pipeline = PreparedPipeline {
                    entry_points: d.entry_points,
                    modules: [module.clone(), module.clone()],
                    layout: layout.clone(),
                    buffers: key.interface.vertex_buffers.clone(),
                    target: d.target.clone(),
                    primitive: d.primitive,
                    depth: d.depth.clone(),
                    multisample: d.multisample,
                }
                .create(device, "d3d9-interpreter");
                if trace {
                    eprintln!(
                        "[d3d9-uber] prepare-pipeline index={index} ms={:.3} descriptor={d:?}",
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                }
                PreparedFamily { key, pipeline }
            })
            .collect();
        let unused_texture = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("d3d9-interpreter-unused-texture"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let unused_cube = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("d3d9-interpreter-unused-cube"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 6,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::Cube),
                ..Default::default()
            });
        let unused_sampler = device.create_sampler(&Default::default());
        Ok(Self {
            unused_texture,
            unused_cube,
            unused_sampler,
            families,
            module,
            layout,
            source: SOURCE.into(),
        })
    }
}
