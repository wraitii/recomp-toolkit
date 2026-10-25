use super::*;

#[derive(Default)]
pub(super) struct MvpCapture {
    frame_draw: u32,
    programs: std::collections::BTreeSet<Arc<[u32]>>,
    families: std::collections::BTreeSet<u64>,
}

impl MvpCapture {
    pub(super) fn end_frame(&mut self) {
        self.frame_draw = 0;
    }
}

pub(super) fn capture_pair() -> Result<[u64; 2], RenderError> {
    static PAIR: std::sync::OnceLock<Result<[u64; 2], RenderError>> = std::sync::OnceLock::new();
    PAIR.get_or_init(|| {
        let value = match std::env::var("RECOMP_D3D9_UBER_CAPTURE_PAIR") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => {
                return Ok([0xdc32dcdca4f64c33, 0xc49b28329a8ab277]);
            }
            Err(error) => return Err(invalid("InterpreterCapture", error.to_string())),
        };
        let fail = || {
            invalid(
                "InterpreterCapture",
                "RECOMP_D3D9_UBER_CAPTURE_PAIR must be VS_HEX:PS_HEX or all",
            )
        };
        if value == "all" {
            return Ok([0, 0]);
        }
        let (vs, ps) = value.split_once(':').ok_or_else(fail)?;
        Ok([
            u64::from_str_radix(vs, 16).map_err(|_| fail())?,
            u64::from_str_radix(ps, 16).map_err(|_| fail())?,
        ])
    })
    .clone()
}

pub(super) fn capture_mvp(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: Option<&super::super::shader::ShaderRecord>,
    ps: Option<&super::super::shader::ShaderRecord>,
    pipeline: &wgpu::RenderPipeline,
) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("RECOMP_D3D9_UBER_CAPTURE").is_ok_and(|v| v == "1")) {
        return;
    }
    let frame_draw = draw.mvp_capture.frame_draw;
    draw.mvp_capture.frame_draw = frame_draw.saturating_add(1);
    let (Some(vs), Some(ps)) = (vs, ps) else {
        return;
    };
    let pair = match capture_pair() {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("[d3d9-uber] capture-error {error}");
            return;
        }
    };
    let interpreted = gpu
        .interpreter
        .get()
        .is_some_and(|p| p.for_pipeline(pipeline).is_some());
    if !interpreted
        && pair != [0, 0]
        && (fingerprint(&vs.code) != pair[0] || fingerprint(&ps.code) != pair[1])
    {
        return;
    }
    for shader in [vs, ps] {
        if draw.mvp_capture.programs.insert(Arc::clone(&shader.code)) {
            let code = fingerprint(&shader.code);
            eprintln!(
                "[d3d9-uber] capture-program code={code:016x} model={} build={} abi={} words={:08x?}",
                shader.program.version(),
                env!("RECOMP_RENDERER_BUILD"),
                crate::shader::uber::ABI_VERSION,
                shader.code
            );
            match &shader.interpreter {
                Ok(program) => eprintln!(
                    "[d3d9-uber] lowered-program code={code:016x} identity={:?} requirements={:?} bytes={} words={:08x?}",
                    program.key,
                    program.requirements,
                    program.words.len() * 4,
                    program.words
                ),
                Err(reason) => eprintln!(
                    "[d3d9-uber] excluded-program code={code:016x} reason={} detail={reason}",
                    reason.reason()
                ),
            }
        }
    }
    let description = draw
        .pipelines
        .values()
        .find(|(p, _)| p == pipeline)
        .map(|(_, recipe)| {
            (
                fingerprint(&recipe.key()),
                &recipe.interface,
                &recipe.descriptor,
            )
        })
        .or_else(|| {
            gpu.interpreter
                .get()
                .and_then(|p| p.for_pipeline(pipeline))
                .map(|family| {
                    (
                        fingerprint(&family.key),
                        &family.key.interface,
                        &family.key.descriptor,
                    )
                })
        });
    let Some((recipe_id, interface, descriptor)) = description else {
        eprintln!(
            "[d3d9-uber] capture-missing-recipe frame={} draw={frame_draw}",
            draw.frame
        );
        return;
    };
    if draw.mvp_capture.families.insert(recipe_id) {
        eprintln!(
            "[d3d9-uber] capture-family recipe={recipe_id:016x} interface={:?} descriptor={:?}",
            interface, descriptor
        );
    }
    eprintln!(
        "[d3d9-uber] capture-draw frame={} draw={frame_draw} recipe={recipe_id:016x} target={:?} subresource={:?} size={}x{} viewport={:?} primitives={} indexed={} texture0={} texture_details={:?}",
        draw.frame,
        snapshot.target.view,
        snapshot.target.subresource,
        snapshot.target.width,
        snapshot.target.height,
        state.viewport(),
        snapshot.command.primitive_count,
        snapshot.command.indexed,
        state.texture(0).unwrap_or(0),
        snapshot
            .textures
            .iter()
            .map(|t| (t.stage, t.width, t.height, t.format, t.lod))
            .collect::<Vec<_>>()
    );
}

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
    recipe: &PipelineRecipe,
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
    let recipe = fingerprint(&recipe.key());
    (recipe, sources, layout)
}
