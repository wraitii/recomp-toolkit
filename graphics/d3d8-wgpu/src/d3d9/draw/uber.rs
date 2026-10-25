use super::*;
use crate::render::interpreter::PreparedInterpreter;
use crate::shader::uber::{self as program, EncodedKey, EncodedProgram};
use wgpu::util::DeviceExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Specialized,
    CompareFamily11,
    Hybrid,
    InterpreterOnly,
}

#[derive(Default)]
pub(super) struct FrameCounters {
    pub(super) draws: u64,
    pub(super) cpu_ms: f64,
    pub(super) packing_ms: f64,
    pub(super) packed_bytes: u64,
    pub(super) uniform_bytes: u64,
}

pub(super) fn enabled() -> bool {
    mode().is_ok_and(|m| m != Mode::Specialized)
}

pub(super) fn tint_enabled() -> bool {
    static TINT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TINT.get_or_init(|| std::env::var("RECOMP_D3D9_UBER_TINT").is_ok_and(|value| value == "1"))
}

fn mode() -> Result<Mode, RenderError> {
    static MODE: std::sync::OnceLock<Result<Mode, RenderError>> = std::sync::OnceLock::new();
    MODE.get_or_init(|| match std::env::var("RECOMP_D3D9_SHADER_MODE") {
        Err(std::env::VarError::NotPresent) => Ok(Mode::Specialized),
        Ok(value) => match value.as_str() {
            "specialized" => Ok(Mode::Specialized),
            "compare-family11" => Ok(Mode::CompareFamily11),
            "hybrid" => Ok(Mode::Hybrid),
            "interpreter-only" => Ok(Mode::InterpreterOnly),
            _ => Err(unsupported(
                "InterpreterMode",
                format!("unknown mode {value:?}"),
            )),
        },
        Err(error) => Err(unsupported("InterpreterMode", error.to_string())),
    })
    .clone()
}

pub(crate) fn prepare(gpu: &D3d9Gpu) -> Result<(), RenderError> {
    let mode = mode()?;
    trace::capture_pair()?;
    if mode == Mode::Hybrid && gpu.backend != wgpu::Backend::Metal {
        return Err(unsupported(
            "HybridPrepare",
            format!("unverified-backend {:?}", gpu.backend),
        ));
    }
    if mode == Mode::Specialized || gpu.interpreter.get().is_some() {
        return Ok(());
    }
    let started = Instant::now();
    let prepared = gpu.validate("InterpreterPrepare", || {
        PreparedInterpreter::new(&gpu.device, mode != Mode::CompareFamily11)
    })?;
    eprintln!(
        "[d3d9-uber] prepare mode={mode:?} abi={} tint={} modules=1 pipelines={} ms={:.3} source_bytes={} program_bytes={} limits={:?}",
        program::ABI_VERSION,
        tint_enabled(),
        prepared.families.len(),
        started.elapsed().as_secs_f64() * 1000.0,
        prepared.source.len(),
        program::PROGRAM_BYTES,
        gpu.limits()
    );
    for family in &prepared.families {
        eprintln!(
            "[d3d9-uber] prepare-family family={:016x} manifest={:?}",
            fingerprint(&family.key),
            family.key
        );
    }
    gpu.interpreter
        .set(prepared)
        .map_err(|_| unsupported("InterpreterPrepare", "family already prepared"))
}

pub(super) fn select(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: Option<&super::super::shader::ShaderRecord>,
    ps: Option<&super::super::shader::ShaderRecord>,
) -> Result<bool, RenderError> {
    let result = coverage(gpu, state, snapshot, vs, ps);
    if hybrid() {
        let reason = match &result {
            Ok(true) => None,
            Ok(false) => Some("unsupported-instruction-or-interface".to_string()),
            Err(error) => Some(error.cause.clone()),
        };
        if let Some(reason) = reason {
            *draw.residual_reasons.entry(reason).or_default() += 1;
            return Ok(false);
        }
    }
    result
}

fn topology(snapshot: &DrawSnapshot) -> wgpu::PrimitiveTopology {
    if snapshot.command.primitive == D3DPT_TRIANGLESTRIP {
        wgpu::PrimitiveTopology::TriangleStrip
    } else {
        wgpu::PrimitiveTopology::TriangleList
    }
}

fn coverage(
    gpu: &D3d9Gpu,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: Option<&super::super::shader::ShaderRecord>,
    ps: Option<&super::super::shader::ShaderRecord>,
) -> Result<bool, RenderError> {
    let mode = mode()?;
    if mode == Mode::Specialized {
        return Ok(false);
    }
    let descriptor = pipeline_descriptor(gpu, state, snapshot, topology(snapshot), None)?;
    let prepared = gpu
        .interpreter
        .get()
        .ok_or_else(|| unsupported("InterpreterDraw", "unprepared-family family11"))?;
    let candidate = matches!(
        snapshot.command.primitive,
        D3DPT_TRIANGLELIST | D3DPT_TRIANGLESTRIP
    ) && rs(state, 9) == 2
        && prepared.find(&descriptor).is_some()
        && vs.is_some_and(|s| program::interface_candidate(&s.program))
        && ps.is_some_and(|s| program::interface_candidate(&s.program));
    if mode == Mode::CompareFamily11
        && (!candidate
            || vs.is_some_and(|s| s.interpreter.is_err())
            || ps.is_some_and(|s| s.interpreter.is_err()))
    {
        return Ok(false);
    }
    if !candidate {
        return Err(unsupported(
            "InterpreterDraw",
            "unsupported-interface family11",
        ));
    }
    for (stage, shader) in [("vertex", vs), ("pixel", ps)] {
        let shader = shader.ok_or_else(|| {
            unsupported("InterpreterDraw", format!("{stage} fixed-function-stage"))
        })?;
        shader
            .interpreter
            .as_ref()
            .map_err(|why| unsupported("InterpreterDraw", format!("{stage} {why}")))?;
    }
    if rs(state, 152) != 0 || (rs(state, 28) != 0 && rs(state, 35) != 0) {
        return Err(unsupported(
            "InterpreterDraw",
            "unsupported-clip-or-table-fog",
        ));
    }
    if snapshot.target.texture.sample_count() != 1
        || snapshot
            .depth
            .as_ref()
            .is_some_and(|d| d.texture.sample_count() != 1)
    {
        return Err(unsupported("InterpreterDraw", "unsupported-multisample"));
    }
    let texture = snapshot
        .textures
        .iter()
        .find(|t| t.stage == 0)
        .ok_or_else(|| unsupported("InterpreterDraw", "unbound-texture s0"))?;
    if texture.cube || texture.volume || texture.depth {
        return Err(unsupported(
            "InterpreterDraw",
            "unsupported-texture-type s0",
        ));
    }
    let format = super::super::format::device_format(texture.format, gpu.features())
        .ok_or_else(|| unsupported("InterpreterDraw", "unsupported-texture-format s0"))?;
    if !format
        .guaranteed_format_features(gpu.features())
        .flags
        .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
    {
        return Err(unsupported("InterpreterDraw", "nonfilterable-texture s0"));
    }
    Ok(true)
}

pub(super) fn hybrid() -> bool {
    mode().is_ok_and(|mode| mode == Mode::Hybrid)
}

pub(super) fn specialize(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: &super::super::shader::ShaderRecord,
    ps: &super::super::shader::ShaderRecord,
) -> Result<Option<Box<DrawMemo>>, RenderError> {
    let prepared = gpu
        .interpreter
        .get()
        .ok_or_else(|| unsupported("HybridDraw", "unprepared-family family11"))?;
    let descriptor = pipeline_descriptor(gpu, state, snapshot, topology(snapshot), None)?;
    let family = prepared
        .find(&descriptor)
        .ok_or_else(|| unsupported("HybridDraw", "unprepared-family"))?;
    let shaders = [vs, ps].map(|shader| {
        let (_, settings) = translated_snapshot(
            ShaderOrigin::Guest(shader.key.clone()),
            &shader.program,
            state,
            &snapshot.textures,
            0,
        )?;
        Ok(ShaderRecipe::Program {
            key: shader.key.clone(),
            program: Arc::clone(&shader.program),
            settings,
        })
    });
    let [vertex, pixel] = shaders;
    let mut textures = [None; MAX_SAMPLERS];
    textures[0] = Some(TextureLayout {
        depth: false,
        filterable: true,
        dimension: wgpu::TextureViewDimension::D2,
    });
    let layout_key = LayoutKey::Draw {
        vs: vs.program.const_limit(),
        ps: ps.program.const_limit(),
        textures,
    };
    let recipe = PipelineRecipe {
        shaders: [vertex?, pixel?],
        interface: InterfaceKey {
            bindings: layout_key.entries().into(),
            vertex_buffers: family.key.interface.vertex_buffers.clone(),
        },
        descriptor: family.key.descriptor.clone(),
    };
    if draw.compiler.is_none() {
        draw.compiler = Some(crate::render::compile::Compiler::new(
            gpu.device.clone(),
            crate::render::compile::Limits {
                queued: 32,
                recipe_bytes: 8 << 20,
                ready: 128,
            },
        )?);
    }
    let compiler = draw.compiler.as_mut().unwrap();
    let Some(compiled) = compiler.request(recipe.clone(), draw.frame)? else {
        return Ok(None);
    };
    let mut memo = resolve(gpu, draw, state, snapshot, vs, ps, false)?;
    memo.modules = compiled.modules;
    memo.layout = compiled.layout;
    memo.binding_entries = recipe.interface.bindings;
    memo.recipes = recipe.shaders;
    memo.vs_count = vs.program.const_limit();
    memo.ps_count = ps.program.const_limit();
    memo.pipeline = Some((
        (
            family.key.interface.vertex_buffers.clone(),
            family.key.descriptor.primitive.topology,
            None,
        ),
        compiled.pipeline,
    ));
    Ok(Some(memo))
}

fn upload(
    gpu: &D3d9Gpu,
    programs: &mut FastMap<EncodedKey, wgpu::Buffer>,
    program: &EncodedProgram,
) -> Result<wgpu::Buffer, RenderError> {
    if let Some(buffer) = programs.get(&program.key) {
        return Ok(buffer.clone());
    }
    let bytes: Vec<_> = program
        .words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect();
    let buffer = gpu.validate("InterpreterProgramUpload", || {
        Ok(gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("d3d9-interpreter-program"),
                contents: &bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            }))
    })?;
    programs.insert(program.key.clone(), buffer.clone());
    eprintln!(
        "[d3d9-uber] program-upload identity={:?} bytes={} resident_programs={} resident_program_bytes={} retained_bytecode_bytes={}",
        program.key,
        bytes.len(),
        programs.len(),
        programs.len() as u64 * program::PROGRAM_BYTES,
        programs
            .keys()
            .map(EncodedKey::retained_bytecode_bytes)
            .sum::<usize>()
    );
    Ok(buffer)
}

pub(super) fn resolve(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: &super::super::shader::ShaderRecord,
    ps: &super::super::shader::ShaderRecord,
    interpreted: bool,
) -> Result<Box<DrawMemo>, RenderError> {
    let prepared = gpu
        .interpreter
        .get()
        .ok_or_else(|| unsupported("InterpreterDraw", "unprepared-family family11"))?;
    let descriptor = pipeline_descriptor(gpu, state, snapshot, topology(snapshot), None)?;
    let family = prepared
        .find(&descriptor)
        .ok_or_else(|| unsupported("InterpreterDraw", "unprepared-family"))?;
    let mut buffers = Vec::with_capacity(2);
    for shader in [vs, ps].into_iter().filter(|_| interpreted) {
        let program = shader
            .interpreter
            .as_ref()
            .map_err(|why| unsupported("InterpreterDraw", why.to_string()))?;
        buffers.push(upload(gpu, &mut draw.interpreter_programs, program)?);
    }
    let programs = interpreted.then(|| [buffers[0].clone(), buffers[1].clone()]);
    let texture = snapshot
        .textures
        .iter()
        .find(|t| t.stage == 0)
        .ok_or_else(|| unsupported("InterpreterDraw", "unbound-texture s0"))?;
    let (key, descriptor) = sampler(gpu, state, texture)?;
    let bindings = vec![PendingTexture {
        stage: 0,
        view: texture.view.clone(),
        sampler: draw.sampler(gpu, key, descriptor)?,
    }]
    .into();
    let recipes = [
        ShaderRecipe::Source(Arc::clone(&prepared.source)),
        ShaderRecipe::Source(Arc::clone(&prepared.source)),
    ];
    Ok(Box::new(DrawMemo {
        fingerprint: state.pipeline_fingerprint(),
        ff_key: None,
        raw_inputs: 0,
        target: (snapshot.target.texture.format(), snapshot.target.format),
        depth: snapshot
            .depth
            .as_ref()
            .map(|d| (d.texture.format(), d.format)),
        textures: snapshot.textures.clone(),
        bindings,
        vs_count: program::VERTEX_CONSTANTS,
        ps_count: program::PIXEL_CONSTANTS,
        modules: [prepared.module.clone(), prepared.module.clone()],
        recipes,
        binding_entries: Arc::clone(&family.key.interface.bindings),
        layout: prepared.layout.clone(),
        programs,
        trace_interface: None,
        trace_pipeline: None,
        pipeline: Some((
            (
                family.key.interface.vertex_buffers.clone(),
                family.key.descriptor.primitive.topology,
                None,
            ),
            family.pipeline.clone(),
        )),
    }))
}
