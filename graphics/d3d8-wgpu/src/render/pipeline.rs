use super::interface::VertexLayouts;

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
            bind_group_layouts: &[&self.layout],
            push_constant_ranges: &[],
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
            .map(|((stride, _), attributes)| wgpu::VertexBufferLayout {
                array_stride: *stride,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes,
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
            multiview: None,
            cache: None,
        })
    }
}
