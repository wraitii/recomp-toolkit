use super::interface::{InterfaceKey, VertexLayouts};
use crate::RenderError;
use crate::shader::recipe::{ShaderRecipe, ShaderRecipeKey};

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct PreparedPipeline {
    pub(crate) entry_points: [&'static str; 2],
    pub(crate) modules: [wgpu::ShaderModule; 2],
    pub(crate) layout: wgpu::BindGroupLayout,
    pub(crate) buffers: VertexLayouts,
    pub(crate) target: Option<wgpu::ColorTargetState>,
    pub(crate) primitive: wgpu::PrimitiveState,
    pub(crate) depth: Option<wgpu::DepthStencilState>,
    pub(crate) multisample: wgpu::MultisampleState,
}

impl PreparedPipeline {
    pub(crate) fn create(&self, device: &wgpu::Device, label: &str) -> wgpu::RenderPipeline {
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(label),
            bind_group_layouts: &[Some(&self.layout)],
            immediate_size: 0,
        });
        let attributes: Vec<Vec<_>> = self
            .buffers
            .iter()
            .map(|(_, attributes)| attributes.iter().map(|a| a.wgpu()).collect())
            .collect();
        let buffers: Vec<_> = self
            .buffers
            .iter()
            .zip(&attributes)
            .map(|((stride, _), attributes)| {
                Some(wgpu::VertexBufferLayout {
                    array_stride: *stride,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes,
                })
            })
            .collect();
        let targets = self.target.clone().map(|target| [Some(target)]);
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &self.modules[0],
                entry_point: Some(self.entry_points[0]),
                buffers: &buffers,
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.modules[1],
                entry_point: Some(self.entry_points[1]),
                targets: targets.as_ref().map_or(&[], |targets| targets.as_slice()),
                compilation_options: Default::default(),
            }),
            primitive: self.primitive,
            depth_stencil: self.depth.clone(),
            multisample: self.multisample,
            multiview_mask: None,
            cache: None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PipelineDescriptor {
    pub(crate) entry_points: [&'static str; 2],
    pub(crate) target: Option<wgpu::ColorTargetState>,
    pub(crate) primitive: wgpu::PrimitiveState,
    pub(crate) depth: Option<wgpu::DepthStencilState>,
    pub(crate) multisample: wgpu::MultisampleState,
}

#[derive(Clone)]
pub(crate) struct PipelineRecipe {
    pub(crate) shaders: [ShaderRecipe; 2],
    pub(crate) interface: InterfaceKey,
    pub(crate) descriptor: PipelineDescriptor,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PipelineRecipeKey {
    shaders: [ShaderRecipeKey; 2],
    interface: InterfaceKey,
    descriptor: PipelineDescriptor,
}

impl PreparedPipeline {
    pub(crate) fn descriptor(&self) -> PipelineDescriptor {
        PipelineDescriptor {
            entry_points: self.entry_points,
            target: self.target.clone(),
            primitive: self.primitive,
            depth: self.depth.clone(),
            multisample: self.multisample,
        }
    }
}

impl PipelineRecipe {
    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .shaders
                .iter()
                .map(ShaderRecipe::retained_bytes)
                .sum::<usize>()
            + std::mem::size_of_val(self.interface.bindings.as_ref())
            + self
                .interface
                .vertex_buffers
                .iter()
                .map(|(_, attributes)| std::mem::size_of_val(attributes.as_slice()))
                .sum::<usize>()
    }

    pub(crate) fn compile(&self, device: &wgpu::Device) -> Result<CompiledPipeline, RenderError> {
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let result = (|| {
            let sources = [self.shaders[0].render()?, self.shaders[1].render()?];
            for source in &sources {
                crate::shader::specialize::validate(source)?;
            }
            let modules = sources.each_ref().map(|source| {
                device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("specialization-worker"),
                    source: wgpu::ShaderSource::Wgsl(source.as_ref().into()),
                })
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("specialization-worker"),
                entries: &self.interface.bindings,
            });
            let pipeline = self
                .prepare(modules.clone(), layout.clone())
                .create(device, "specialization-worker");
            Ok(CompiledPipeline {
                pipeline,
                modules,
                layout,
            })
        })();
        let oom = pollster::block_on(oom.pop());
        let validation = pollster::block_on(validation.pop());
        if let Some(error) = oom {
            return Err(RenderError::out_of_memory(
                "SpecializationCompile",
                error.to_string(),
            ));
        }
        if let Some(error) = validation {
            return Err(RenderError::new("SpecializationCompile", error.to_string()));
        }
        result
    }

    pub(crate) fn key(&self) -> PipelineRecipeKey {
        PipelineRecipeKey {
            shaders: self.shaders.each_ref().map(ShaderRecipe::key),
            interface: self.interface.clone(),
            descriptor: self.descriptor.clone(),
        }
    }

    pub(crate) fn prepare(
        &self,
        modules: [wgpu::ShaderModule; 2],
        layout: wgpu::BindGroupLayout,
    ) -> PreparedPipeline {
        PreparedPipeline {
            entry_points: self.descriptor.entry_points,
            modules,
            layout,
            buffers: self.interface.vertex_buffers.clone(),
            target: self.descriptor.target.clone(),
            primitive: self.descriptor.primitive,
            depth: self.descriptor.depth.clone(),
            multisample: self.descriptor.multisample,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CompiledPipeline {
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) modules: [wgpu::ShaderModule; 2],
    pub(crate) layout: wgpu::BindGroupLayout,
}
