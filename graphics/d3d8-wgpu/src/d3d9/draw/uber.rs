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

pub(super) fn enabled(gpu: &D3d9Gpu) -> bool {
    mode(gpu).is_ok_and(|m| m != Mode::Specialized)
}

pub(super) fn tint_enabled() -> bool {
    static TINT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TINT.get_or_init(|| std::env::var("RECOMP_D3D9_UBER_TINT").is_ok_and(|value| value == "1"))
}

fn mode(gpu: &D3d9Gpu) -> Result<Mode, RenderError> {
    static MODE: std::sync::OnceLock<Result<Option<Mode>, RenderError>> =
        std::sync::OnceLock::new();
    MODE.get_or_init(|| match std::env::var("RECOMP_D3D9_SHADER_MODE") {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Ok(value) => match value.as_str() {
            "specialized" => Ok(Some(Mode::Specialized)),
            "compare-family11" => Ok(Some(Mode::CompareFamily11)),
            "hybrid" => Ok(Some(Mode::Hybrid)),
            "interpreter-only" => Ok(Some(Mode::InterpreterOnly)),
            _ => Err(unsupported(
                "InterpreterMode",
                format!("unknown mode {value:?}"),
            )),
        },
        Err(error) => Err(unsupported("InterpreterMode", error.to_string())),
    })
    .clone()
    .map(|mode| {
        mode.unwrap_or(if gpu.backend == wgpu::Backend::Metal {
            Mode::Hybrid
        } else {
            Mode::Specialized
        })
    })
}

pub(crate) fn prepare(gpu: &D3d9Gpu) -> Result<(), RenderError> {
    let mode = mode(gpu)?;
    trace::capture_pair()?;
    if mode == Mode::Hybrid && gpu.backend != wgpu::Backend::Metal {
        return Err(unsupported(
            "HybridPrepare",
            format!("unverified-backend {:?}", gpu.backend),
        ));
    }
    if mode == Mode::Specialized {
        eprintln!("[d3d9-uber] mode={mode:?} backend={:?}", gpu.backend);
        return Ok(());
    }
    if gpu.interpreter.get().is_some() {
        return Ok(());
    }
    let started = Instant::now();
    let prepared = gpu.validate("InterpreterPrepare", || {
        PreparedInterpreter::new(
            &gpu.device,
            mode != Mode::CompareFamily11,
            trace_compilation(),
        )
    })?;
    eprintln!(
        "[d3d9-uber] mode={mode:?} backend={:?} pipelines={} prepare_ms={:.3} tint={}",
        gpu.backend,
        prepared.families.len(),
        started.elapsed().as_secs_f64() * 1000.0,
        tint_enabled()
    );
    if trace_compilation() {
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
    }
    gpu.interpreter
        .set(prepared)
        .map_err(|_| unsupported("InterpreterPrepare", "family already prepared"))
}

#[derive(Default)]
pub(super) struct Selection {
    pub(super) eligible: Option<bool>,
    pub(super) family: Option<u64>,
}

pub(super) fn select(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: Option<&super::super::shader::ShaderRecord>,
    ps: Option<&super::super::shader::ShaderRecord>,
) -> Result<Selection, RenderError> {
    let mode = mode(gpu)?;
    if mode == Mode::Specialized {
        return Ok(Selection::default());
    }
    let coverage = coverage(gpu, state, snapshot, vs, ps);
    let eligible = coverage.as_ref().is_ok_and(|eligible| *eligible);
    let descriptor = pipeline_descriptor(gpu, state, snapshot, topology(snapshot), None)?;
    let family = gpu
        .interpreter
        .get()
        .ok_or_else(|| unsupported("InterpreterDraw", "interpreter not prepared"))?
        .find(&descriptor)
        .filter(|_| eligible)
        .map(|family| fingerprint(&family.key));
    let result = coverage.and_then(|eligible| {
        if eligible && family.is_none() && mode != Mode::CompareFamily11 {
            Err(unsupported("InterpreterDraw", "unprepared-pipeline-state"))
        } else {
            Ok(eligible)
        }
    });
    if mode == Mode::Hybrid {
        let reason = match &result {
            Ok(true) => None,
            Ok(false) => Some("unsupported-instruction-or-interface".to_string()),
            Err(error) => Some(error.cause.clone()),
        };
        if let Some(reason) = reason {
            if trace_compilation() {
                *draw.residual_reasons.entry(reason).or_default() += 1;
            }
        }
    } else {
        result?;
    }
    Ok(Selection {
        eligible: Some(eligible),
        family,
    })
}

pub(super) fn topology(snapshot: &DrawSnapshot) -> wgpu::PrimitiveTopology {
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
    let mode = mode(gpu)?;
    if mode == Mode::Specialized {
        return Ok(false);
    }
    let exclusion = if !matches!(
        snapshot.command.primitive,
        D3DPT_TRIANGLELIST | D3DPT_TRIANGLESTRIP
    ) {
        Some("unsupported-primitive")
    } else if rs(state, 9) != 2 {
        Some("unsupported-shading-mode")
    } else if !vs.is_some_and(|s| program::interface_candidate(&s.program)) {
        Some("unsupported-vertex-interface")
    } else if !ps.is_some_and(|s| program::interface_candidate(&s.program)) {
        Some("unsupported-pixel-interface")
    } else {
        None
    };
    if mode == Mode::CompareFamily11
        && (exclusion.is_some()
            || vs.is_some_and(|s| s.interpreter.is_err())
            || ps.is_some_and(|s| s.interpreter.is_err()))
    {
        return Ok(false);
    }
    if let Some(reason) = exclusion {
        return Err(unsupported("InterpreterDraw", reason));
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
    for (&stage, &kind) in &ps.unwrap().program.samplers {
        let Some(texture) = snapshot.textures.iter().find(|t| t.stage == stage) else {
            continue;
        };
        if texture.volume
            || texture.depth
            || texture.cube != (kind == crate::shader::program::S_CUBE)
        {
            return Err(unsupported(
                "InterpreterDraw",
                format!("unsupported-texture-type s{stage}"),
            ));
        }
        let format = super::super::format::device_format(texture.format, gpu.features())
            .ok_or_else(|| {
                unsupported(
                    "InterpreterDraw",
                    format!("unsupported-texture-format s{stage}"),
                )
            })?;
        if !format
            .guaranteed_format_features(gpu.features())
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
        {
            return Err(unsupported(
                "InterpreterDraw",
                format!("nonfilterable-texture s{stage}"),
            ));
        }
    }
    Ok(true)
}

pub(super) fn hybrid(gpu: &D3d9Gpu) -> bool {
    mode(gpu).is_ok_and(|mode| mode == Mode::Hybrid)
}

pub(super) fn specialize(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: &super::super::shader::ShaderRecord,
    ps: &super::super::shader::ShaderRecord,
    vertex_buffers: VertexLayouts,
    topology: wgpu::PrimitiveTopology,
    strip_format: Option<wgpu::IndexFormat>,
    raw_inputs: u16,
) -> Result<Option<(CompiledPipeline, PipelineRecipe)>, RenderError> {
    let descriptor = pipeline_descriptor(gpu, state, snapshot, topology, strip_format)?;
    let shaders = [vs, ps].map(|shader| {
        let (_, settings) = translated_snapshot(
            ShaderOrigin::Guest(shader.key.clone()),
            &shader.program,
            state,
            &snapshot.textures,
            raw_inputs,
        )?;
        Ok(ShaderRecipe::Program {
            key: shader.key.clone(),
            program: Arc::clone(&shader.program),
            settings,
        })
    });
    let [vertex, pixel] = shaders;
    let mut textures = [None; MAX_SAMPLERS];
    for texture in snapshot
        .textures
        .iter()
        .filter(|t| ps.program.samplers.contains_key(&t.stage))
    {
        let stage = texture.stage;
        textures[stage as usize] = Some(TextureLayout {
            depth: false,
            filterable: true,
            dimension: texture_dimension(texture),
        });
    }
    let layout_key = LayoutKey::Draw {
        vs: vs.program.const_limit(),
        ps: ps.program.const_limit(),
        textures,
    };
    let recipe = PipelineRecipe {
        shaders: [vertex?, pixel?],
        interface: InterfaceKey {
            bindings: layout_key.entries().into(),
            vertex_buffers,
        },
        descriptor,
    };
    let key = recipe.key();
    if trace_compilation() {
        eprintln!(
            "[d3d9-compile] draw-specialization frame={} draw={} recipe={:016x}",
            draw.frame,
            draw.measured_draws,
            key.fingerprint()
        );
    }
    if let Some((compiled, _)) = draw.pipelines.get(&key) {
        return Ok(Some((compiled.clone(), recipe)));
    }
    if draw.compiler.is_none() {
        draw.compiler = Some(crate::render::compile::Compiler::new(
            gpu.device.clone(),
            crate::render::compile::Limits {
                queued: 32,
                recipe_bytes: 8 << 20,
            },
            trace_compilation(),
        )?);
    }
    let compiler = draw.compiler.as_mut().unwrap();
    compiler.request(recipe, draw.frame)?;
    Ok(None)
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
    if trace_compilation() {
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
    }
    Ok(buffer)
}

pub(super) fn resolve(
    gpu: &D3d9Gpu,
    draw: &mut D3d9DrawState,
    state: &D3d9State,
    snapshot: &DrawSnapshot,
    vs: &super::super::shader::ShaderRecord,
    ps: &super::super::shader::ShaderRecord,
    specialized: Option<(CompiledPipeline, PipelineRecipe)>,
    raw_inputs: u16,
) -> Result<Box<DrawMemo>, RenderError> {
    let interpreted = specialized.is_none();
    let prepared = gpu
        .interpreter
        .get()
        .ok_or_else(|| unsupported("InterpreterDraw", "interpreter not prepared"))?;
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
    let mapping = &ps
        .interpreter
        .as_ref()
        .map_err(|why| unsupported("InterpreterDraw", why.to_string()))?
        .sampler_mapping;
    let mut bindings = Vec::new();
    if interpreted {
        for physical in 0..program::TEXTURE_SLOTS {
            let bound = mapping
                .iter()
                .find(|&&(_, slot)| slot == physical)
                .and_then(|&(guest, _)| snapshot.textures.iter().find(|t| t.stage == guest));
            let binding = if let Some(texture) = bound {
                let (key, descriptor) = sampler(gpu, state, texture)?;
                PendingTexture {
                    stage: physical,
                    view: texture.view.clone(),
                    sampler: draw.sampler(gpu, key, descriptor)?,
                }
            } else {
                PendingTexture {
                    stage: physical,
                    view: if physical < program::TEXTURES_2D {
                        prepared.unused_texture.clone()
                    } else {
                        prepared.unused_cube.clone()
                    },
                    sampler: prepared.unused_sampler.clone(),
                }
            };
            bindings.push(binding);
        }
    } else {
        for &(guest, _) in mapping {
            let Some(texture) = snapshot.textures.iter().find(|t| t.stage == guest) else {
                continue;
            };
            let (key, descriptor) = sampler(gpu, state, texture)?;
            bindings.push(PendingTexture {
                stage: guest,
                view: texture.view.clone(),
                sampler: draw.sampler(gpu, key, descriptor)?,
            });
        }
    }
    let bindings = bindings.into();
    let selected_descriptor = specialized
        .as_ref()
        .map(|(_, recipe)| recipe.descriptor.clone())
        .unwrap_or_else(|| family.key.descriptor.clone());
    let shape = specialized
        .as_ref()
        .map(|(_, recipe)| {
            (
                recipe.interface.vertex_buffers.clone(),
                recipe.descriptor.primitive.topology,
                recipe.descriptor.primitive.strip_index_format,
            )
        })
        .unwrap_or_else(|| {
            (
                family.key.interface.vertex_buffers.clone(),
                family.key.descriptor.primitive.topology,
                None,
            )
        });
    let (modules, layout, binding_entries, recipes, vs_count, ps_count, pipeline) =
        if let Some((compiled, recipe)) = specialized {
            (
                compiled.modules,
                compiled.layout,
                recipe.interface.bindings,
                recipe.shaders,
                vs.program.const_limit(),
                ps.program.const_limit(),
                compiled.pipeline,
            )
        } else {
            (
                [prepared.module.clone(), prepared.module.clone()],
                prepared.layout.clone(),
                Arc::clone(&family.key.interface.bindings),
                [
                    ShaderRecipe::Source(Arc::clone(&prepared.source)),
                    ShaderRecipe::Source(Arc::clone(&prepared.source)),
                ],
                program::VERTEX_CONSTANTS,
                program::PIXEL_CONSTANTS,
                family.pipeline.clone(),
            )
        };
    let trace_pipeline = if trace_compilation() {
        let mut stages = Vec::with_capacity(2);
        for shader in [vs, ps] {
            let (key, _) = translated_snapshot(
                ShaderOrigin::Guest(shader.key.clone()),
                &shader.program,
                state,
                &snapshot.textures,
                raw_inputs,
            )?;
            stages.push(trace::stage(&key));
        }
        let interface = trace::Interface {
            stages: stages.try_into().unwrap(),
            layout: Arc::clone(&binding_entries),
            inputs: vs
                .program
                .inputs
                .iter()
                .map(|i| (i.usage, i.usage_index, i.ty, i.index))
                .collect(),
        };
        let recipe = PipelineRecipe {
            shaders: recipes.clone(),
            interface: InterfaceKey {
                bindings: Arc::clone(&binding_entries),
                vertex_buffers: shape.0.clone(),
            },
            descriptor: selected_descriptor,
        };
        let key = recipe.prepare(modules.clone(), layout.clone());
        Some((
            trace::observe(draw, &interface, &key),
            fingerprint(&recipe.key()),
        ))
    } else {
        None
    };
    Ok(Box::new(DrawMemo {
        fingerprint: state.pipeline_fingerprint(),
        ff_key: None,
        raw_inputs,
        target: (snapshot.target.texture.format(), snapshot.target.format),
        depth: snapshot
            .depth
            .as_ref()
            .map(|d| (d.texture.format(), d.format)),
        textures: snapshot.textures.clone(),
        bindings,
        vs_count,
        ps_count,
        modules,
        recipes,
        binding_entries,
        layout,
        programs,
        trace_interface: None,
        trace_pipeline,
        pipeline: Some((shape, pipeline)),
    }))
}
