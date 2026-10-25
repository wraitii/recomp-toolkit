use super::*;
use crate::render::interpreter::PreparedInterpreter;
use crate::shader::uber::{self as program, EncodedKey, EncodedProgram};
use wgpu::util::DeviceExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Specialized,
    CompareFamily11,
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
    if mode == Mode::Specialized || gpu.interpreter.get().is_some() {
        return Ok(());
    }
    let started = Instant::now();
    let prepared = gpu.validate("InterpreterPrepare", || {
        PreparedInterpreter::new(&gpu.device)
    })?;
    eprintln!(
        "[d3d9-uber] prepare mode={mode:?} family={:016x} abi={} tint={} modules=1 pipelines=1 ms={:.3} source_bytes={} program_bytes={} limits={:?} manifest={:?}",
        fingerprint(&prepared.key),
        program::ABI_VERSION,
        tint_enabled(),
        started.elapsed().as_secs_f64() * 1000.0,
        prepared.source.len(),
        program::PROGRAM_BYTES,
        gpu.limits(),
        prepared.key
    );
    gpu.interpreter
        .set(prepared)
        .map_err(|_| unsupported("InterpreterPrepare", "family already prepared"))
}

pub(super) fn select(
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
    let descriptor = pipeline_descriptor(
        gpu,
        state,
        snapshot,
        wgpu::PrimitiveTopology::TriangleList,
        None,
    )?;
    let prepared = gpu
        .interpreter
        .get()
        .ok_or_else(|| unsupported("InterpreterDraw", "unprepared-family family11"))?;
    let candidate = snapshot.command.primitive == D3DPT_TRIANGLELIST
        && rs(state, 9) == 2
        && descriptor == prepared.key.descriptor
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
) -> Result<Box<DrawMemo>, RenderError> {
    let prepared = gpu
        .interpreter
        .get()
        .ok_or_else(|| unsupported("InterpreterDraw", "unprepared-family family11"))?;
    let mut buffers = Vec::with_capacity(2);
    for shader in [vs, ps] {
        let program = shader
            .interpreter
            .as_ref()
            .map_err(|why| unsupported("InterpreterDraw", why.to_string()))?;
        buffers.push(upload(gpu, &mut draw.interpreter_programs, program)?);
    }
    let programs = Some([buffers[0].clone(), buffers[1].clone()]);
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
        binding_entries: Arc::clone(&prepared.key.interface.bindings),
        layout: prepared.layout.clone(),
        programs,
        trace_interface: None,
        trace_pipeline: None,
        pipeline: Some((
            (
                prepared.key.interface.vertex_buffers.clone(),
                wgpu::PrimitiveTopology::TriangleList,
                None,
            ),
            prepared.pipeline.clone(),
        )),
    }))
}
