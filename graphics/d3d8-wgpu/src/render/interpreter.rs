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
    pub(crate) key: FamilyKey,
    pub(crate) module: wgpu::ShaderModule,
    pub(crate) layout: wgpu::BindGroupLayout,
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
        let bindings: Arc<[_]> = vec![
            uniform(0, u64::from(VERTEX_CONSTANTS) * 16, vertex, true),
            uniform(1, 272, vertex, true),
            uniform(2, u64::from(PIXEL_CONSTANTS) * 16, fragment, true),
            uniform(3, 64, fragment, true),
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: fragment,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 20,
                visibility: fragment,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            uniform(PROGRAM_BINDINGS[0], PROGRAM_BYTES, vertex, false),
            uniform(PROGRAM_BINDINGS[1], PROGRAM_BYTES, fragment, false),
        ]
        .into();
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
                bindings,
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
                    depth_write_enabled: false,
                    depth_compare: wgpu::CompareFunction::Less,
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
                u64::from(VERTEX_CONSTANTS) * 16,
            ),
            (
                "buffer-bytes",
                limits.max_buffer_size,
                u64::from(VERTEX_CONSTANTS) * 16,
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
                "inter-stage-components",
                u64::from(limits.max_inter_stage_shader_components),
                u64::from(INTER_STAGE_COMPONENTS),
            ),
            (
                "textures",
                u64::from(limits.max_sampled_textures_per_shader_stage),
                1,
            ),
            (
                "samplers",
                u64::from(limits.max_samplers_per_shader_stage),
                1,
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
    pub(crate) fn new(device: &wgpu::Device) -> Result<Self, RenderError> {
        let key = FamilyKey::mvp();
        key.check_limits(&device.limits())?;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("d3d9-interpreter"),
            entries: &key.interface.bindings,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("d3d9-interpreter"),
            source: wgpu::ShaderSource::Wgsl(SOURCE.into()),
        });
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
        Ok(Self {
            key,
            module,
            layout,
            pipeline,
            source: SOURCE.into(),
        })
    }
}
