use super::interface::{InterfaceKey, VertexLayouts};
use crate::RenderError;
use crate::fast_map::FastMap;
use crate::shader::recipe::{ShaderRecipe, ShaderRecipeKey};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PipelineRecipeKey {
    fingerprint: u64,
    shaders: [ShaderRecipeKey; 2],
    interface: InterfaceKey,
    descriptor: PipelineDescriptor,
}

impl PipelineRecipeKey {
    pub(crate) fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

impl Hash for PipelineRecipeKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.fingerprint);
    }
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

#[derive(Default)]
pub(crate) struct CompilationCache {
    modules: FastMap<ShaderRecipeKey, wgpu::ShaderModule>,
    layouts: FastMap<Arc<[wgpu::BindGroupLayoutEntry]>, wgpu::BindGroupLayout>,
}

impl CompilationCache {
    fn retain(&mut self, recipe: &PipelineRecipe, compiled: &CompiledPipeline) {
        for (shader, module) in recipe.shaders.iter().zip(&compiled.modules) {
            let key = shader.key();
            if self.modules.contains_key(&key) {
                continue;
            }
            if self.modules.len() >= 512 {
                if let Some(key) = self.modules.keys().next().cloned() {
                    self.modules.remove(&key);
                    eprintln!(
                        "[d3d9-cache] WARNING eviction cache=worker-shader-modules evicted=1 limit_entries=512; may require recompilation"
                    );
                }
            }
            self.modules.insert(key, module.clone());
        }
        if !self.layouts.contains_key(&recipe.interface.bindings) {
            if self.layouts.len() >= 128 {
                if let Some(key) = self.layouts.keys().next().cloned() {
                    self.layouts.remove(&key);
                }
            }
            self.layouts.insert(
                Arc::clone(&recipe.interface.bindings),
                compiled.layout.clone(),
            );
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

    pub(crate) fn compile(
        &self,
        device: &wgpu::Device,
        cache: &mut CompilationCache,
    ) -> Result<CompiledPipeline, RenderError> {
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let result = (|| {
            let mut modules: Vec<wgpu::ShaderModule> = Vec::with_capacity(2);
            for shader in &self.shaders {
                let key = shader.key();
                let module = if let Some(module) = cache.modules.get(&key) {
                    module.clone()
                } else if let Some((_, module)) = self
                    .shaders
                    .iter()
                    .zip(&modules)
                    .find(|(previous, _)| previous.key() == key)
                {
                    module.clone()
                } else {
                    let source = shader.render()?;
                    crate::shader::specialize::validate(&source)?;
                    device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("specialization-worker"),
                        source: wgpu::ShaderSource::Wgsl(source.as_ref().into()),
                    })
                };
                modules.push(module);
            }
            let modules: [wgpu::ShaderModule; 2] = modules.try_into().unwrap();
            let layout = cache
                .layouts
                .get(&self.interface.bindings)
                .cloned()
                .unwrap_or_else(|| {
                    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                        label: Some("specialization-worker"),
                        entries: &self.interface.bindings,
                    })
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
        if let Ok(compiled) = &result {
            cache.retain(self, compiled);
        }
        result
    }

    pub(crate) fn key(&self) -> PipelineRecipeKey {
        let shaders = self.shaders.each_ref().map(ShaderRecipe::key);
        let mut hasher = crate::fast_map::FastHasher::default();
        shaders.hash(&mut hasher);
        self.interface.hash(&mut hasher);
        self.descriptor.hash(&mut hasher);
        PipelineRecipeKey {
            fingerprint: hasher.finish(),
            shaders,
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
