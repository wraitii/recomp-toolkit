use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Interface {
    pub stages: [(u8, KeyWords); 2],
    pub layout: LayoutKey,
    pub inputs: Vec<(u32, u32, u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Family {
    interface: Interface,
    buffers: VertexLayouts,
    target: Option<wgpu::ColorTargetState>,
    primitive: wgpu::PrimitiveState,
    depth: Option<wgpu::DepthStencilState>,
    multisample: wgpu::MultisampleState,
}

pub(super) fn stage(key: &ShaderVariantKey) -> (u8, KeyWords) {
    (
        match key.shader {
            ShaderOrigin::Guest(_) => 0,
            ShaderOrigin::FixedFunction => 1,
            ShaderOrigin::DeclaredFixedFunction => 2,
        },
        key.state.clone(),
    )
}

pub(super) fn observe(draw: &mut D3d9DrawState, interface: &Interface, key: &PipelineKey) -> u64 {
    let family = Family {
        interface: interface.clone(),
        buffers: key.buffers.clone(),
        target: key.target.clone(),
        primitive: key.primitive,
        depth: key.depth.clone(),
        multisample: key.multisample,
    };
    if let Some(id) = draw.observed_families.get(&family) {
        return *id;
    }
    let id = draw.observed_families.len() as u64 + 1;
    eprintln!(
        "[d3d9-compile] observed-family id={id} key={:016x} descriptor={family:?}",
        fingerprint(&family)
    );
    draw.observed_families.insert(family, id);
    id
}

pub(super) fn recipe(
    draw: &D3d9DrawState,
    key: &PipelineKey,
) -> (u64, [Option<(u64, usize)>; 2], Option<u64>) {
    let sources = key.modules.each_ref().map(|module| {
        draw.shaders
            .iter()
            .find(|(_, cached)| *cached == module)
            .map(|(source, _)| (fingerprint(source), source.len()))
    });
    let layout = draw
        .layouts
        .iter()
        .find(|(_, cached)| *cached == &key.layout)
        .map(|(layout, _)| fingerprint(layout));
    let recipe = fingerprint(&(
        sources,
        layout,
        key.entry_points,
        &key.buffers,
        &key.target,
        key.primitive,
        &key.depth,
        key.multisample,
    ));
    (recipe, sources, layout)
}
