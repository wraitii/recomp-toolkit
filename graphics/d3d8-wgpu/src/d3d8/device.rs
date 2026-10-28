//! Bounded D3D8 host device: full clear, unlit triangle lists, and present.
//! This is an isolated probe implementation, with no COM or guest addresses.
use super::{
    fixed_function::{
        AlphaTestUniform, FogUniform, FvfLayout, StagesUniform, TEXTURED_WGSL, TransformUniform,
        UNLIT_WGSL, gpu_lit_layout, gpu_lit_shader_source, gpu_lit_source, lit_layout,
        lit_shader_source,
    },
    resource::{
        IndexedDraw, VertexBuffer, expand_indexed_into, expand_nonindexed_into, indexed_list_into,
        packed_indexed_list_into,
    },
    shader::{self, Declaration, Program},
    state::{DeviceState, LightingUniform, LitInput, MAX_TEXTURE_STAGES},
    stats, survey,
    texture_cache::{TextureCache, TextureKey},
};
use crate::backend::native_present::{ReadyEvent, create_ready_event, signal_ready};
use crate::{
    RenderError,
    backend::{GpuContext, OffscreenTarget},
    fast_map::FastMap,
};

#[path = "render_targets.rs"]
mod render_targets;
use render_targets::Targets;

/// `Device::scene_boundary` modes.
pub const SCENE_POST_NONE: u32 = 0;
pub const SCENE_POST_FXAA: u32 = 1;

const CLEAR_TARGET: u32 = 0x1;
const CLEAR_ZBUFFER: u32 = 0x2;
const CLEAR_STENCIL: u32 = 0x4;

/// D3D8 rasterizes to pixel centers at integer framebuffer coordinates, while
/// wgpu's viewport maps NDC onto pixel centers at half-integer coordinates.
/// Shifting the viewport origin by half a pixel makes a primitive that D3D8
/// covers land on the same pixels: without it, an edge that falls exactly on a
/// pixel center is dropped by wgpu's right/bottom fill rule (observed as a
/// 1-pixel black right column and bottom row on a full-screen game quad whose
/// projected edges were exactly screen coordinates 639.5 and 479.5).
const D3D8_HALF_PIXEL: f32 = 0.5;

/// Print the survey table roughly every this many presented frames.
const SURVEY_REPORT_INTERVAL: u64 = 300;

/// Upper bound on resident uploaded textures. The kit releases a texture's
/// cache entry when the guest destroys it; this LRU cap is only a safety net
/// against unbounded growth in a very long session.
const MAX_CACHED_TEXTURES: usize = 4096;

/// Map a `D3DBLENDOP` to the wgpu equivalent. `D3DBLENDOP_SUBTRACT` is the
/// source minus the destination (`wgpu::BlendOperation::Subtract`);
/// `D3DBLENDOP_REVSUBTRACT` is the destination minus the source
/// (`wgpu::BlendOperation::ReverseSubtract`).
fn blend_operation(op: crate::d3d8::enums::D3DBLENDOP) -> wgpu::BlendOperation {
    use crate::d3d8::enums::D3DBLENDOP as O;
    match op {
        O::Add => wgpu::BlendOperation::Add,
        O::Subtract => wgpu::BlendOperation::Subtract,
        O::RevSubtract => wgpu::BlendOperation::ReverseSubtract,
        O::Min => wgpu::BlendOperation::Min,
        O::Max => wgpu::BlendOperation::Max,
    }
}

/// Map a `D3DBLEND` factor to the wgpu equivalent. `BOTHSRCALPHA` and
/// `BOTHINVSRCALPHA` are named errors: see [`Device::blend_state`].
fn blend_factor(factor: crate::d3d8::enums::D3DBLEND) -> Result<wgpu::BlendFactor, RenderError> {
    use crate::d3d8::enums::D3DBLEND as B;
    Ok(match factor {
        B::Zero => wgpu::BlendFactor::Zero,
        B::One => wgpu::BlendFactor::One,
        B::SrcColor => wgpu::BlendFactor::Src,
        B::InvSrcColor => wgpu::BlendFactor::OneMinusSrc,
        B::SrcAlpha => wgpu::BlendFactor::SrcAlpha,
        B::InvSrcAlpha => wgpu::BlendFactor::OneMinusSrcAlpha,
        B::DestAlpha => wgpu::BlendFactor::DstAlpha,
        B::InvDestAlpha => wgpu::BlendFactor::OneMinusDstAlpha,
        B::DestColor => wgpu::BlendFactor::Dst,
        B::InvDestColor => wgpu::BlendFactor::OneMinusDst,
        B::SrcAlphaSat => wgpu::BlendFactor::SrcAlphaSaturated,
        B::BothSrcAlpha | B::BothInvSrcAlpha => {
            return Err(RenderError::new(
                "DrawPrimitive",
                "D3DBLEND_BOTHSRCALPHA/D3DBLEND_BOTHINVSRCALPHA are not representable in wgpu",
            ));
        }
    })
}

/// Map D3D8's `D3DRS_COLORWRITEENABLE` 4-bit mask to wgpu's channel write mask.
/// D3D8's bit order (R=1, G=2, B=4, A=8) matches `wgpu::ColorWrites`, so the
/// bits carry over directly. The draw pipeline intersects this with the render
/// target's own channel rule before using it.
fn color_writes_from_mask(mask: u32) -> wgpu::ColorWrites {
    wgpu::ColorWrites::from_bits_truncate(mask & 0xF)
}

/// `RECOMP_D3D8_SKIP_FVF=0x2C4,0x112` drops every draw with one of the listed
/// FVFs. Diagnostic only: it bisects which pass produces an artefact and is
/// never on by default.
fn skipped_fvf(fvf: u32) -> bool {
    static LIST: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var("RECOMP_D3D8_SKIP_FVF")
            .map(|v| {
                v.split(',')
                    .filter_map(|t| u32::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
                    .collect()
            })
            .unwrap_or_default()
    })
    .contains(&fvf)
}

/// D3DPRIMITIVETYPE value for a non-indexed draw, mapped to the number of
/// vertices each primitive consumes.
///
/// Points, line lists and triangle lists map directly to wgpu primitives.
/// Triangle strips/fans are expanded before entering this draw path.
fn topology_vertex_count(topology: u32, primitive_count: u32) -> Result<u32, RenderError> {
    match topology {
        // D3DPT_POINTLIST: one vertex per point.
        1 => Ok(primitive_count),
        // D3DPT_LINELIST: two vertices per independent line.
        2 => primitive_count
            .checked_mul(2)
            .ok_or_else(|| RenderError::new("DrawPrimitive", "vertex range overflow")),
        // D3DPT_TRIANGLELIST: three vertices per triangle.
        4 => primitive_count
            .checked_mul(3)
            .ok_or_else(|| RenderError::new("DrawPrimitive", "vertex range overflow")),
        other => Err(RenderError::new(
            "DrawPrimitive",
            format!(
                "unsupported topology {other}; expected POINTLIST (1), LINELIST (2) or TRIANGLELIST (4)"
            ),
        )),
    }
}

/// Translate a validated D3DPRIMITIVETYPE into its wgpu topology.
fn wgpu_topology(topology: u32) -> wgpu::PrimitiveTopology {
    match topology {
        1 => wgpu::PrimitiveTopology::PointList,
        2 => wgpu::PrimitiveTopology::LineList,
        _ => wgpu::PrimitiveTopology::TriangleList,
    }
}

/// Map `D3DRS_ZBIAS` (D3D8's documented 0..=16 range) onto wgpu's constant
/// depth bias. A guest uses it to separate coplanar overlay layers, so only a
/// small monotonic bias is required.
///
/// D3D8 biases positive values toward the viewer; Wine's wined3d maps it to
/// `glPolygonOffset(0, -zbias)`, the same convention. WebGPU/Vulkan add a
/// positive constant away from the viewer, so the sign is negated. D3DRS_ZBIAS
/// has no slope-scaled component, so `slope_scale` and `clamp` stay zero.
///
/// DIVERGENCE(original): the authored D3D8 ZBIAS step is driver-defined at the
/// hardware level and cannot be recovered from the guest. One wgpu depth unit
/// per ZBIAS step (the reference minimum-resolvable step) was observed to be
/// too small in Railroad Tycoon 3: the CPU-projected (`ProcessVertices`) terrain
/// and the GPU-transformed `0x142` overlay layers differ by more than that in
/// depth rounding, so the overlays z-fight the ground. Measured in the running
/// game (zoomed in, 1600x1200, D24X8): 64 units per step left patches, 256
/// removed them. `ZBIAS_UNITS_PER_STEP` is that empirically chosen step;
/// `RECOMP_D3D8_ZBIAS_SCALE=<n>` overrides it for experiments.
const ZBIAS_UNITS_PER_STEP: i32 = 256;

fn z_bias_state(z_bias: u32) -> wgpu::DepthBiasState {
    static SCALE: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    let scale = *SCALE.get_or_init(|| {
        std::env::var("RECOMP_D3D8_ZBIAS_SCALE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(ZBIAS_UNITS_PER_STEP)
    });
    wgpu::DepthBiasState {
        constant: -(z_bias as i32).saturating_mul(scale),
        slope_scale: 0.0,
        clamp: 0.0,
    }
}

/// DIAGNOSTIC ONLY: opt-in cull override from `RECOMP_D3D8_CULL`. This exists
/// to separate a bridge cull-winding bug from guest-side visibility culling in
/// one windowed run; it deviates from faithful D3D8 behaviour and must not be
/// enabled in a validation run. Unset means faithful. `flip` swaps CW and CCW,
/// `none` forces `D3DCULL_NONE`, and `cw`/`ccw` force one mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CullOverride {
    None,
    Cw,
    Ccw,
    Flip,
}

fn cull_override() -> Option<CullOverride> {
    static OVERRIDE: std::sync::OnceLock<Option<CullOverride>> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        let Ok(raw) = std::env::var("RECOMP_D3D8_CULL") else {
            return None;
        };
        let parsed = match raw.as_str() {
            "none" => CullOverride::None,
            "cw" => CullOverride::Cw,
            "ccw" => CullOverride::Ccw,
            "flip" => CullOverride::Flip,
            other => {
                eprintln!(
                    "[d3d8-cull] DIAGNOSTIC RECOMP_D3D8_CULL={other} is not none|cw|ccw|flip; ignoring"
                );
                return None;
            }
        };
        eprintln!(
            "[d3d8-cull] DIAGNOSTIC cull override active: {raw} (deviation from faithful D3D8 cull state)"
        );
        Some(parsed)
    })
}

/// One-shot diagnostic for the leading draws of a run: prints the topology,
/// FVF, counts, viewport, transforms and the first four vertices so a
/// rasterization or coverage mismatch can be attributed to real inputs.
/// Enabled by `RECOMP_D3D8_TRACE_DRAWS`; the value is the number of draws to
/// print (default 8), and `RECOMP_D3D8_TRACE_DRAWS_START` skips that many
/// draws first (default 0). Unset means no work is done.
fn trace_draw(
    fvf: u32,
    topology: u32,
    layout: &FvfLayout,
    vertices: &VertexBuffer,
    start_vertex: u32,
    primitive_count: u32,
    vertex_count: u32,
    state: &DeviceState,
    textured: bool,
    stage0: Option<&BoundTexture>,
    stage1: Option<&BoundTexture>,
) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);
    static CONFIG: std::sync::OnceLock<Option<(u32, u32, bool)>> = std::sync::OnceLock::new();
    // `RECOMP_D3D8_TRACE_DRAWS_START=<n>` skips the first `n` traced draws so a
    // later draw can be captured; `RECOMP_D3D8_TRACE_DRAWS` still caps how many
    // are printed from that offset. `RECOMP_D3D8_TRACE_STAGE1=1` counts and
    // prints only draws with a texture bound at stage 1 (the world draws), so
    // the cap is not spent on menus.
    let Some((limit, start, stage1_only)) = *CONFIG.get_or_init(|| {
        let raw = std::env::var("RECOMP_D3D8_TRACE_DRAWS").ok()?;
        let start = std::env::var("RECOMP_D3D8_TRACE_DRAWS_START")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Some((
            raw.parse().unwrap_or(8),
            start,
            std::env::var_os("RECOMP_D3D8_TRACE_STAGE1").is_some(),
        ))
    }) else {
        return;
    };
    if stage1_only && stage1.is_none() {
        return;
    }
    static TEXTURE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    if let Some(texture_id) = *TEXTURE.get_or_init(|| {
        let raw = std::env::var("RECOMP_D3D8_TRACE_TEXTURE").ok()?;
        u32::from_str_radix(raw.trim_start_matches("0x"), 16).ok()
    }) {
        if ![stage0, stage1]
            .iter()
            .flatten()
            .any(|texture| texture.texture_id == texture_id)
        {
            return;
        }
    }
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < start || n >= start.saturating_add(limit) {
        return;
    }
    eprintln!(
        "[d3d8-trace] draw {n}: topo={topology} fvf=0x{fvf:06X} stride={} start={start_vertex} prims={primitive_count} textured={textured}",
        layout.stride
    );
    // Full stage/FVF/render-state dump (tss0 and tss1 included) plus whether a
    // real texture is bound at each stage. This is what distinguishes a
    // two-stage world draw whose stage 1 the bridge currently drops from one
    // that is genuinely single-texture; it is diagnostic only.
    eprintln!("[d3d8-trace]   state: {}", state.draw_state_summary());
    for (stage, bound) in [(0u32, stage0), (1u32, stage1)] {
        let Some(t) = bound else {
            eprintln!("[d3d8-trace]   stage{stage} texture unbound (white fallback)");
            continue;
        };
        // Per-level written flags and generations as the renderer recorded
        // them, so a chain that is resident but never marked written (and so
        // clamped to LOD 0) is distinguishable from one with no levels at all.
        let levels: Vec<String> = (0..t.level_count as usize)
            .map(|l| {
                let generation = t.level_generations.get(l).copied().unwrap_or(0);
                let written = t.written.get(l).copied().unwrap_or(false);
                format!("l{l}:g{generation}:{}", if written { "W" } else { "-" })
            })
            .collect();
        eprintln!(
            "[d3d8-trace]   stage{stage} texture id=0x{:08x} {}x{} fmt={:#x} levels={} max_written={} written=[{}]",
            t.texture_id,
            t.width,
            t.height,
            t.format,
            t.level_count,
            t.max_written_level,
            levels.join(",")
        );
        // The clamp the sampler actually used for this stage, from the same
        // helper the draw path calls. `-` when the stage state does not
        // resolve (the draw would already have failed, so this is diagnostic).
        match state.resolve_texture_stage(stage, true) {
            Ok(resolved) => {
                let (lod_min, lod_max, filter) = mip_lod_range(&resolved, t.max_written_level);
                eprintln!(
                    "[d3d8-trace]   stage{stage} sampler lod=({lod_min},{lod_max}) mip_filter={filter:?} max_mip_level={} mip={:#x}",
                    resolved.max_mip_level, resolved.mip_filter
                );
            }
            Err(e) => eprintln!(
                "[d3d8-trace]   stage{stage} sampler unresolved: {}",
                e.cause
            ),
        }
    }
    // Sampler inputs the fixed-function draw depends on. RT3's terrain looked
    // wrong because mip filtering was missing; print the raw guest values so we
    // can see what it sets (defaults: MIN/MAG=POINT, MIP=NONE, MAXMIPLEVEL=0,
    // MIPMAPLODBIAS=0).
    for stage in 0..2u32 {
        use crate::d3d8::enums::D3DTEXTURESTAGESTATETYPE as Ts;
        let get = |s: Ts| match state.texture_stage_state(stage, s.raw()) {
            Some(v) => format!("{v:#x}"),
            None => "-".to_string(),
        };
        eprintln!(
            "[d3d8-trace]   tss{stage} min={} mag={} mip={} maxmip={} lodbias={}",
            get(Ts::MinFilter),
            get(Ts::MagFilter),
            get(Ts::MipFilter),
            get(Ts::MaxMipLevel),
            get(Ts::MipMapLodBias),
        );
    }
    let vp = state.viewport;
    eprintln!(
        "[d3d8-trace]   viewport=({},{}) {}x{} z=[{},{}] z_enable={} z_write={} z_func={:?} z_bias={}",
        vp.x,
        vp.y,
        vp.width,
        vp.height,
        vp.min_z,
        vp.max_z,
        state.z_enable(),
        state.z_write_enable(),
        state.z_func(),
        state.z_bias()
    );
    eprintln!(
        "[d3d8-trace]   cull={:?} blend={} src={:?} dst={:?}",
        state.cull_mode(),
        state.alpha_blend_enable(),
        state.src_blend(),
        state.dest_blend()
    );
    eprintln!("[d3d8-trace]   world={:?}", state.world.rows);
    eprintln!("[d3d8-trace]   view={:?}", state.view.rows);
    eprintln!("[d3d8-trace]   proj={:?}", state.projection.rows);
    // Texture matrices are only meaningful when the stage sets
    // D3DTSS_TEXTURETRANSFORMFLAGS; printing them lets a coordinate mismatch be
    // attributed to the guest's matrix rather than guessed at.
    for stage in 0..2usize {
        eprintln!(
            "[d3d8-trace]   tex_transform[{stage}]={:?}",
            state.texture_transforms[stage].rows
        );
    }
    // Read the vertex fields from the decoded FVF offsets. A hardcoded offset
    // misreads the XYZRHW layout (where position is four floats and diffuse sits
    // at 16), which is exactly the layout the pre-transformed world path uses.
    let attr = |loc: u32| layout.attributes.iter().find(|a| a.shader_location == loc);
    let f32_at =
        |v: &[u8], off: usize| f32::from_le_bytes([v[off], v[off + 1], v[off + 2], v[off + 3]]);
    let u32_at =
        |v: &[u8], off: usize| u32::from_le_bytes([v[off], v[off + 1], v[off + 2], v[off + 3]]);
    let bytes = vertices.bytes();
    let stride = layout.stride as usize;
    for i in 0..4usize {
        let off = (start_vertex as usize + i) * stride;
        if off + stride > bytes.len() {
            break;
        }
        let v = &bytes[off..off + stride];
        let (x, y, z) = match attr(0) {
            Some(p) => {
                let o = p.offset as usize;
                (f32_at(v, o), f32_at(v, o + 4), f32_at(v, o + 8))
            }
            None => (0.0, 0.0, 0.0),
        };
        let mut line = format!("[d3d8-trace]   v{i} pos=({x},{y},{z})");
        if let Some(c) = attr(1) {
            line.push_str(&format!(" color=0x{:08X}", u32_at(v, c.offset as usize)));
        }
        if let Some(s) = attr(4) {
            line.push_str(&format!(" specular=0x{:08X}", u32_at(v, s.offset as usize)));
        }
        if let Some(t) = attr(2) {
            let o = t.offset as usize;
            line.push_str(&format!(" uv=({},{})", f32_at(v, o), f32_at(v, o + 4)));
        }
        eprintln!("{line}");
    }
    // Texture-coordinate range over the whole draw. A constant set (or a
    // single-set FVF aliasing set 1 to set 0) collapses to min == max, which is
    // exactly what a flat single-texel sample looks like. For the world draws
    // set 1 is the large-scale colour texture, so this distinguishes a real
    // per-vertex coordinate from a constant/garbage one.
    let uv_range = |loc: u32| -> Option<([f32; 2], [f32; 2], [f32; 2], u64)> {
        let t = attr(loc)?;
        let o = t.offset as usize;
        if o + 8 > stride {
            return None;
        }
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        let mut sum = [0.0f64; 2];
        let mut n = 0u64;
        // `vertex_count` is the absolute end vertex index (`start + 3*prims`),
        // not a count: scan `start..vertex_count`.
        for index in start_vertex as usize..vertex_count as usize {
            let off = index * stride;
            if off + stride > bytes.len() {
                break;
            }
            let v = &bytes[off..off + stride];
            let pair = [f32_at(v, o), f32_at(v, o + 4)];
            for c in 0..2 {
                min[c] = min[c].min(pair[c]);
                max[c] = max[c].max(pair[c]);
                sum[c] += pair[c] as f64;
            }
            n += 1;
        }
        if n == 0 {
            return None;
        }
        Some((
            min,
            max,
            [(sum[0] / n as f64) as f32, (sum[1] / n as f64) as f32],
            n,
        ))
    };
    for (loc, name) in [(2u32, "uv0"), (3, "uv1")] {
        if let Some((min, max, mean, n)) = uv_range(loc) {
            eprintln!(
                "[d3d8-trace]   {name} n={n} min=({},{}) max=({},{}) mean=({},{})",
                min[0], min[1], max[0], max[1], mean[0], mean[1]
            );
        }
    }
    // Longest triangle edge over the whole draw. A stale ring region, a vertex
    // buffer read with the wrong stride, or a bad index expansion shows up as
    // one triangle edge far longer than the mesh's normal spacing; printing the
    // worst edge per draw attributes a "stretched triangle" to real vertex bytes
    // instead of guessing. Diagnostic only.
    if let Some(p) = attr(0) {
        let po = p.offset as usize;
        let pos_at = |index: usize| -> Option<[f32; 3]> {
            let off = index * stride;
            if off + stride > bytes.len() {
                return None;
            }
            let v = &bytes[off..off + stride];
            Some([f32_at(v, po), f32_at(v, po + 4), f32_at(v, po + 8)])
        };
        let mut worst = 0.0f64;
        let mut worst_tri = start_vertex as usize;
        let mut worst_pair = ([0.0f32; 3], [0.0f32; 3]);
        let mut tri = start_vertex as usize;
        while tri + 2 < vertex_count as usize {
            if let (Some(a), Some(b), Some(c)) = (pos_at(tri), pos_at(tri + 1), pos_at(tri + 2)) {
                for (p, q) in [(a, b), (b, c), (c, a)] {
                    let d = f64::from(p[0] - q[0]).powi(2)
                        + f64::from(p[1] - q[1]).powi(2)
                        + f64::from(p[2] - q[2]).powi(2);
                    if d > worst {
                        worst = d;
                        worst_tri = tri;
                        worst_pair = (p, q);
                    }
                }
            }
            tri += 3;
        }
        eprintln!(
            "[d3d8-trace]   max_edge={:.3} tri_v={} a=({},{},{}) b=({},{},{})",
            worst.sqrt(),
            worst_tri,
            worst_pair.0[0],
            worst_pair.0[1],
            worst_pair.0[2],
            worst_pair.1[0],
            worst_pair.1[1],
            worst_pair.1[2]
        );
    }
}

/// A D3D8 texture bound to a stage. It holds its own texture handle so the
/// default white texture (which has no cache entry) stays alive, and a clone
/// of the view so a cache eviction cannot invalidate a bound stage mid-frame.
struct BoundTexture {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Kit level-surface COM identity; `0` for the white fallback.
    texture_id: u32,
    width: u32,
    height: u32,
    format: u32,
    /// Highest contiguous mip level the guest has written. The sampler clamps
    /// its upper LOD to this so an unwritten level is never sampled. `0` when
    /// only the base level exists.
    max_written_level: u32,
    /// Number of mip slices in the resident wgpu texture.
    level_count: u32,
    /// Content generation per level, as last uploaded (`0` = never written).
    level_generations: Vec<u64>,
    /// Written flag per level (a generation changed or a lock was open).
    written: Vec<bool>,
}

/// One mip level handed to [`Device::set_texture`]. `data` is the level's CPU
/// texel block in the source D3D8 layout.
pub struct TextureLevelUpload<'a> {
    pub level: u32,
    pub generation: u64,
    /// A lock was still open at draw time, so the bytes must not be assumed
    /// unchanged even if the generation matches.
    pub force_upload: bool,
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

/// One GPU-resident mip chain held in [`Device::texture_cache`]. Keyed by the
/// guest texture identity, not by stage, so the same texture bound at two
/// stages or across draws is uploaded once. Each level keeps its own content
/// generation, so re-locking level N re-uploads only N.
struct CachedTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    format: u32,
    level_generations: Vec<u64>,
    written: Vec<bool>,
}

impl CachedTexture {
    /// Highest contiguous written level, starting at 0.
    fn max_written_level(&self) -> u32 {
        contiguous_written_level(&self.written)
    }
}

/// Highest contiguous written mip level starting at 0. Returns 0 when level 0
/// is not written, so the sampler always has a valid upper clamp and an
/// unwritten level is never sampled.
fn contiguous_written_level(written: &[bool]) -> u32 {
    let mut level = 0;
    for (i, written) in written.iter().enumerate() {
        if *written {
            level = i as u32;
        } else {
            break;
        }
    }
    level
}

/// Bytes in the source (pre-decode) level block: the block layout for a
/// compressed format, `width * height * bytes_per_pixel` otherwise.
fn source_level_bytes(
    format: u32,
    color: Option<crate::d3d8::format::ColorFormat>,
    width: u32,
    height: u32,
) -> Result<usize, RenderError> {
    if format == crate::d3d8::format::D3DFMT_V8U8 {
        return (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| RenderError::new("SetTexture", "level size overflow"));
    }
    Ok(match color {
        Some(color) => (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(color.bytes_per_pixel() as usize))
            .ok_or_else(|| RenderError::new("SetTexture", "level size overflow"))?,
        None => crate::d3d8::format::block_level_layout(width, height, format).1 as usize,
    })
}

/// Convert one source level into the reusable RGBA scratch and upload it to its
/// mip slice. Split out so every level uses the same clipped-dimension path;
/// a block-compressed level smaller than 4x4 still decodes from a full block.
#[allow(clippy::too_many_arguments)]
fn upload_texture_level(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    level: u32,
    format: u32,
    color: Option<crate::d3d8::format::ColorFormat>,
    width: u32,
    height: u32,
    data: &[u8],
    scratch: &mut Vec<u8>,
) -> Result<(), RenderError> {
    let expected = source_level_bytes(format, color, width, height)?;
    if data.len() < expected {
        return Err(RenderError::new(
            "SetTexture",
            format!(
                "level {level} has {} bytes but {width}x{height} {format} needs {expected}",
                data.len()
            ),
        ));
    }
    let signed_bump = format == crate::d3d8::format::D3DFMT_V8U8;
    if !signed_bump {
        match color {
            Some(color) => color.to_rgba8_into(&data[..expected], scratch),
            None => crate::d3d8::format::decode_block_into(
                format,
                &data[..expected],
                width,
                height,
                scratch,
            )?,
        }
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        if signed_bump {
            &data[..expected]
        } else {
            scratch
        },
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * if signed_bump { 2 } else { 4 }),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    Ok(())
}

/// Bounded programmable-draw capture. Stage-2 filtering keeps menu/terrain
/// draws from consuming the capture budget. Rendered textures are read from
/// the GPU: their CPU lock storage can be stale after a reflection pass.
fn trace_shader_draw(
    device: &Device,
    uniform: &shader::Uniform,
    samplers: &[bool; 4],
    vertices: &VertexBuffer,
    start: u32,
) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);
    static CONFIG: std::sync::OnceLock<Option<(u32, bool)>> = std::sync::OnceLock::new();
    let Some((limit, stage2_only)) = *CONFIG.get_or_init(|| {
        let limit = std::env::var("RECOMP_D3D8_TRACE_SHADER_DRAWS")
            .ok()?
            .parse::<u32>()
            .ok()?;
        Some((
            limit,
            std::env::var_os("RECOMP_D3D8_TRACE_SHADER_STAGE2").is_some(),
        ))
    }) else {
        return;
    };
    if stage2_only && !samplers[2] {
        return;
    }
    let capture = COUNT.fetch_add(1, Ordering::Relaxed);
    if capture >= limit {
        return;
    }
    eprintln!(
        "[d3d8-shader-draw] capture={capture} draw={} vs={:#x} ps={:#x} target={:?} viewport={:?}",
        device.draw_index,
        device.vertex_shader,
        device.pixel_shader,
        device.targets.current,
        device.state.viewport
    );
    eprintln!(
        "[d3d8-shader-draw] vc={:?} pc={:?} bump={:?} lum={:?}",
        &uniform.vc[..15],
        uniform.pc,
        uniform.bump,
        uniform.lum
    );
    let at = start as usize * vertices.stride as usize;
    let end = (at + vertices.stride as usize * 3).min(vertices.bytes().len());
    let sample: Vec<f32> = vertices.bytes()[at..end]
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    eprintln!(
        "[d3d8-shader-draw] stride={} first_vertices={sample:?}",
        vertices.stride
    );
    for (stage, used) in samplers.iter().enumerate() {
        let Some(texture) = &device.stage_textures[stage] else {
            eprintln!("[d3d8-shader-draw] stage={stage} used={used} unbound");
            continue;
        };
        let rt = device
            .targets
            .textures
            .get(&TextureKey::new(texture.texture_id, 0));
        eprintln!(
            "[d3d8-shader-draw] stage={stage} used={used} id={} {}x{} fmt={:#x} gpu_rendered={} generations={:?}",
            texture.texture_id,
            texture.width,
            texture.height,
            texture.format,
            rt.is_some(),
            texture.level_generations
        );
        if let (Some(rt), Some(dir)) = (rt, std::env::var_os("RECOMP_D3D8_TRACE_SHADER_DIR")) {
            device.flush_draws();
            let dir = std::path::PathBuf::from(dir);
            let result = std::fs::create_dir_all(&dir)
                .map_err(|e| e.to_string())
                .and_then(|_| {
                    device
                        .gpu
                        .read_pixels(&rt.surface)
                        .map_err(|e| e.to_string())
                })
                .and_then(|pixels| {
                    crate::d3d8::dump::write_png(
                        &dir.join(format!("shader{capture}_stage{stage}_rendered.png")),
                        texture.width,
                        texture.height,
                        &pixels,
                    )
                    .map_err(|e| e.to_string())
                });
            if let Err(error) = result {
                eprintln!("[d3d8-shader-draw] capture failed: {error}");
            }
        }
    }
}

/// The `(lod_min, lod_max, mipmap_filter)` a stage's sampler uses for a chain
/// with `max_written_level` written levels. `D3DTSS_MAXMIPLEVEL` selects the
/// base (most detailed) level (WineD3D `mip_base_level`), `MIPFILTER=NONE`
/// collapses to that single level, and POINT/LINEAR span to the last written
/// level. The written clamp keeps a level the guest never filled unsampled.
fn mip_lod_range(
    stage: &crate::d3d8::state::TextureStage,
    max_written_level: u32,
) -> (f32, f32, wgpu::MipmapFilterMode) {
    use crate::d3d8::enums::D3DTEXTUREFILTERTYPE as F;
    let base = stage.max_mip_level.min(max_written_level);
    let last = max_written_level.max(base);
    match F::from_raw(stage.mip_filter).expect("validated mip filter") {
        F::None => (base as f32, base as f32, wgpu::MipmapFilterMode::Nearest),
        F::Point => (base as f32, last as f32, wgpu::MipmapFilterMode::Nearest),
        F::Linear | F::Anisotropic => (base as f32, last as f32, wgpu::MipmapFilterMode::Linear),
        F::PyramidQuad | F::GaussianQuad => {
            unreachable!("validated: only POINT/LINEAR/NONE/ANISOTROPIC are accepted")
        }
    }
}

/// A 1x1 opaque white texture bound as the sampler fallback when a stage is
/// active but no texture is set. It is not what an unbound stage necessarily
/// samples: Wine's `is_invalid_op` (dlls/wined3d/utils.c) rewrites a stage op
/// that reads TEXTURE with no texture to SELECTARG1(CURRENT), which
/// `DeviceState::resolve_texture_stage` mirrors, so the white texel is only
/// read by ops that do not reference TEXTURE or after a real texture is bound.
fn create_white_texture(gpu: &GpuContext) -> BoundTexture {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("D3D8 default white texture"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &[255, 255, 255, 255],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    BoundTexture {
        view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
        _texture: texture,
        texture_id: 0,
        width: 1,
        height: 1,
        format: crate::d3d8::format::D3DFMT_A8R8G8B8,
        max_written_level: 0,
        level_count: 1,
        level_generations: vec![0],
        written: vec![true],
    }
}

/// Build a wgpu sampler from the resolved stage filters/address modes. The
/// validator has already rejected values with no wgpu equivalent, so every
/// `unwrap` here is on a value that passed validation.
fn sampler_descriptor(
    stage: &crate::d3d8::state::TextureStage,
    max_written_level: u32,
) -> wgpu::SamplerDescriptor<'static> {
    use crate::d3d8::enums::{D3DTEXTUREADDRESS as A, D3DTEXTUREFILTERTYPE as F};
    let address = |value: u32| match A::from_raw(value).expect("validated address mode") {
        A::Wrap => wgpu::AddressMode::Repeat,
        A::Mirror => wgpu::AddressMode::MirrorRepeat,
        A::Clamp => wgpu::AddressMode::ClampToEdge,
        A::Border => wgpu::AddressMode::ClampToBorder,
        A::MirrorOnce => unreachable!("validated: MIRRORONCE is refused"),
    };
    let filter = |value: u32| match F::from_raw(value).expect("validated filter") {
        // MaxAnisotropy is 1, so ANISOTROPIC is linear filtering (see
        // DeviceState::resolve_texture_stage).
        F::Linear | F::Anisotropic => wgpu::FilterMode::Linear,
        F::None | F::Point => wgpu::FilterMode::Nearest,
        F::PyramidQuad | F::GaussianQuad => {
            unreachable!("validated: only POINT/LINEAR/NONE/ANISOTROPIC are accepted")
        }
    };
    let border = matches!(A::from_raw(stage.address_u), Ok(A::Border))
        || matches!(A::from_raw(stage.address_v), Ok(A::Border));
    let (lod_min_clamp, lod_max_clamp, mipmap_filter) = mip_lod_range(stage, max_written_level);
    wgpu::SamplerDescriptor {
        label: Some("D3D8 stage sampler"),
        address_mode_u: address(stage.address_u),
        address_mode_v: address(stage.address_v),
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter(stage.mag_filter),
        min_filter: filter(stage.min_filter),
        mipmap_filter,
        lod_min_clamp,
        lod_max_clamp,
        border_color: border.then_some(wgpu::SamplerBorderColor::TransparentBlack),
        ..Default::default()
    }
}

/// A tiny value-keyed cache with hit/miss counters. Values are cloned out so a
/// caller never holds a map borrow while recording GPU work; used for samplers
/// (and uniform buffers). Counters feed `RECOMP_D3D8_DRAW_STATS`.
struct KeyedCache<K, V> {
    map: FastMap<K, V>,
    hits: u64,
    misses: u64,
}

impl<K, V> Default for KeyedCache<K, V> {
    fn default() -> Self {
        Self {
            map: FastMap::default(),
            hits: 0,
            misses: 0,
        }
    }
}

impl<K: Eq + std::hash::Hash, V: Clone> KeyedCache<K, V> {
    fn get_or_insert_with(&mut self, key: K, create: impl FnOnce() -> V) -> V {
        if let Some(value) = self.map.get(&key) {
            self.hits += 1;
            return value.clone();
        }
        self.misses += 1;
        let value = create();
        self.map.insert(key, value.clone());
        value
    }
}

/// Everything that varies a sampler object. `address_mode_w` is always
/// `ClampToEdge` and the remaining fields keep their wgpu defaults, so only
/// these raw D3D8 values distinguish two samplers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct SamplerKey {
    address_u: u32,
    address_v: u32,
    mag_filter: u32,
    min_filter: u32,
    mip_filter: u32,
    /// `D3DTSS_MAXMIPLEVEL` and the bound chain's written-level count fully
    /// determine the clamp; two stages with the same filters but different
    /// chains must not share a sampler.
    max_mip_level: u32,
    max_written_level: u32,
}

impl SamplerKey {
    fn from_stage(stage: &crate::d3d8::state::TextureStage, max_written_level: u32) -> Self {
        Self {
            address_u: stage.address_u,
            address_v: stage.address_v,
            mag_filter: stage.mag_filter,
            min_filter: stage.min_filter,
            mip_filter: stage.mip_filter,
            max_mip_level: stage.max_mip_level,
            max_written_level,
        }
    }
}

/// Byte offsets of the per-draw uniforms inside `Device::draw_buffer`. Each is
/// a multiple of the 256-byte uniform offset alignment; vertices follow.
const DRAW_UB_TRANSFORM: u64 = 0;
const DRAW_UB_FOG: u64 = 256;
const DRAW_UB_ALPHA_TEST: u64 = 512;
const DRAW_UB_STAGES: u64 = 768;
const DRAW_UB_PROGRAM: u64 = 1024;
// Fixed-function lighting reuses the otherwise unused program bank. Only a
// GPU-lit draw with a pixel shader needs both immutable banks simultaneously.
const DRAW_UB_LIGHTING: u64 = DRAW_UB_PROGRAM;
const DRAW_VERTEX_OFFSET: u64 = 3072;
const DRAW_UB_LIGHTING_EXTENDED: u64 = DRAW_VERTEX_OFFSET;
const DRAW_EXTENDED_VERTEX_OFFSET: u64 = 4096;
const _: () = {
    assert!(std::mem::size_of::<TransformUniform>() <= 256);
    assert!(std::mem::size_of::<FogUniform>() <= 256);
    assert!(std::mem::size_of::<AlphaTestUniform>() <= 256);
    assert!(std::mem::size_of::<StagesUniform>() <= 256);
    assert!(std::mem::size_of::<LightingUniform>() <= 1024);
};

/// The raw `MTLTexture*` behind a wgpu texture. Only the Metal backend has one.
#[cfg(target_os = "macos")]
fn native_texture_ptr(texture: &wgpu::Texture) -> Result<*const core::ffi::c_void, RenderError> {
    // SAFETY: the pointer is only borrowed, never released or retained here;
    // the texture outlives every use because the ring owns it.
    unsafe {
        let hal = texture
            .as_hal::<wgpu::hal::api::Metal>()
            .ok_or_else(|| RenderError::new("present_handoff", "texture is not a Metal texture"))?;
        let raw = hal.raw_handle();
        Ok(std::ptr::from_ref(raw).cast())
    }
}

#[cfg(not(target_os = "macos"))]
fn native_texture_ptr(_: &wgpu::Texture) -> Result<*const core::ffi::c_void, RenderError> {
    Err(RenderError::new(
        "present_handoff",
        "native texture handoff is macOS-only",
    ))
}

/// Slots in the present handoff ring. The host's staging blit of a slot runs
/// asynchronously on its own queue; a slot is reused only after the host
/// clears its busy flag, so the next frame never overwrites pixels in flight.
const FRAME_RING_SLOTS: usize = 3;

/// GPU copies of the backbuffer handed to the host presenter as raw native
/// textures, so a present moves no pixels through the CPU.
struct FrameRing {
    ready_event: ReadyEvent,
    ready_value: u64,
    width: u32,
    height: u32,
    textures: Vec<wgpu::Texture>,
    /// 1 while the host still reads the slot. Leaked: the host clears it from a
    /// GPU completion callback that may outlive the device.
    busy: Vec<&'static std::sync::atomic::AtomicU32>,
    next: usize,
}

/// One frame handed to the host. `texture` is a borrowed native texture
/// (`MTLTexture*`), valid while the device lives; the host sets `*busy` to 0
/// when it no longer reads it.
pub struct FrameHandoff {
    pub ready_event: *mut u8,
    pub ready_value: u64,
    pub texture: *const core::ffi::c_void,
    pub busy: &'static std::sync::atomic::AtomicU32,
    pub width: u32,
    pub height: u32,
}

/// Flush a recorded batch once its CPU-side block data reaches this size.
const BATCH_FLUSH_BYTES: usize = 8 << 20;

type TextureGroupKey = [(wgpu::TextureView, wgpu::Sampler); 4];
const MAX_TEXTURE_GROUPS: usize = 512;

/// Retained bindings for all four shader-model 1.1 texture registers.
struct StageBindings {
    pairs: TextureGroupKey,
}

/// One recorded draw with immutable uniform and vertex offsets into `DrawBatch::data`.
struct PendingDraw {
    pipeline: wgpu::RenderPipeline,
    uniform_offsets: [u32; 6],
    vertex_base: u64,
    vertex_len: u64,
    stages: bool,
    textures: Option<StageBindings>,
    viewport: crate::d3d8::state::Viewport,
    count: u32,
    indices: Option<(u64, u64)>,
}

/// Consecutive draws into one render target, replayed as a single render pass
/// and submit by `Device::flush_draws`. The pass uses Load/Store ops, so
/// replaying draws in order is equivalent to one pass per draw. Anything that
/// reads or writes GPU state outside a draw must flush first (clear, texture
/// writes, target changes, readback, present, end of scene).
#[derive(Default)]
struct DrawBatch {
    draws: Vec<PendingDraw>,
    data: Vec<u8>,
    last_uniforms: [Option<(usize, usize)>; 6],
    color_view: Option<wgpu::TextureView>,
    depth: Option<(wgpu::TextureView, bool)>,
}

/// Explicit bind group layouts for the draw pipelines. Group 0 binds the
/// per-draw uniform blocks with dynamic offsets into the shared draw buffer, so
/// one bind group serves every draw; group 1 (textured only) holds the stage
/// textures and samplers.
struct DrawLayouts {
    /// Pipeline layout for unlit draws: group 0 = transform, fog, alpha test.
    unlit: wgpu::PipelineLayout,
    /// Pipeline layout for textured draws: group 0 adds the stages block.
    textured: wgpu::PipelineLayout,
    bgl_unlit: wgpu::BindGroupLayout,
    bgl_textured: wgpu::BindGroupLayout,
    bgl_stage_textures: wgpu::BindGroupLayout,
}

impl DrawLayouts {
    fn new(gpu: &wgpu::Device) -> Self {
        let uniform = |binding: u32, size: usize| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(size as u64),
            },
            count: None,
        };
        let transform = uniform(0, std::mem::size_of::<TransformUniform>());
        let stages = uniform(1, std::mem::size_of::<StagesUniform>());
        let fog = uniform(2, std::mem::size_of::<FogUniform>());
        let alpha = uniform(3, std::mem::size_of::<AlphaTestUniform>());
        let program = uniform(4, std::mem::size_of::<shader::Uniform>());
        let lighting = uniform(5, std::mem::size_of::<LightingUniform>());
        let bgl_unlit = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("D3D8 unlit uniforms"),
            entries: &[transform, fog, alpha, lighting],
        });
        let bgl_textured = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("D3D8 textured uniforms"),
            entries: &[transform, stages, fog, alpha, program, lighting],
        });
        let tex = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let bgl_stage_textures = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("D3D8 stage textures"),
            entries: &[
                tex(0),
                samp(1),
                tex(2),
                samp(3),
                tex(4),
                samp(5),
                tex(6),
                samp(7),
            ],
        });
        let unlit = gpu.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("D3D8 unlit pipeline layout"),
            bind_group_layouts: &[Some(&bgl_unlit)],
            immediate_size: 0,
        });
        let textured = gpu.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("D3D8 textured pipeline layout"),
            bind_group_layouts: &[Some(&bgl_textured), Some(&bgl_stage_textures)],
            immediate_size: 0,
        });
        Self {
            unlit,
            textured,
            bgl_unlit,
            bgl_textured,
            bgl_stage_textures,
        }
    }
}

/// `RECOMP_D3D8_DRAW_STATS=1` per-frame counters. Deltas are reset after each
/// `Present`, so a line is one frame's draw-path cost; `sampler_hits`/`misses`
/// are cumulative cache counters.
#[derive(Default)]
struct DrawStats {
    draws: u64,
    buffers_created: u64,
    uniform_writes: u64,
    submits: u64,
}

/// Everything that varies a cached draw pipeline. The attachment formats belong
/// to the bound render target, which changes on every `SetRenderTarget`, so they
/// are part of the key; the effective color write mask combines the target's
/// channel rule with `D3DRS_COLORWRITEENABLE`. (Clearing the whole cache on a
/// target switch recompiled every pipeline each frame once shadows rendered to
/// texture targets.)
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DrawPipelineKey {
    gpu_lighting: bool,
    topology: wgpu::PrimitiveTopology,
    color_format: wgpu::TextureFormat,
    color_write_mask: wgpu::ColorWrites,
    depth_format: Option<wgpu::TextureFormat>,
    fvf: u32,
    vs: u32,
    ps: u32,
    stride: u64,
    textured: bool,
    z_enable: bool,
    z_write: bool,
    z_func: u32,
    z_bias: u32,
    blend: u32,
    cull: u32,
}

pub struct Device {
    pub shaders: std::collections::HashMap<u32, (Option<Declaration>, Program)>,
    pub vertex_shader: u32,
    pub pixel_shader: u32,
    pub shader_uniform: shader::Uniform,
    programmable_modules: FastMap<(u32, u32, u32, bool), wgpu::ShaderModule>,
    shader: Option<wgpu::ShaderModule>,
    textured_shader: Option<wgpu::ShaderModule>,
    lit_shader: Option<wgpu::ShaderModule>,
    lit_textured_shader: Option<wgpu::ShaderModule>,
    gpu_lit_shaders: std::collections::HashMap<(u32, bool), wgpu::ShaderModule>,
    /// Comparison switch; ProcessVertices and unsupported states remain CPU-lit.
    cpu_lighting: bool,
    pipelines: FastMap<DrawPipelineKey, wgpu::RenderPipeline>,
    pub state: DeviceState,
    pub gpu: GpuContext,
    target: OffscreenTarget,
    targets: Targets,
    /// Bound texture per stage; `None` means the default white texture.
    stage_textures: [Option<BoundTexture>; MAX_TEXTURE_STAGES],
    /// Identity-keyed cache of uploaded texture levels.
    texture_cache: TextureCache<CachedTexture>,
    /// Reused destination for `ColorFormat::to_rgba8_into`, so a genuine upload
    /// does not allocate a fresh RGBA buffer per bind.
    scratch_rgba: Vec<u8>,
    /// Descriptor-keyed sampler cache; D3D8 stage sampling state rarely changes.
    samplers: KeyedCache<SamplerKey, wgpu::Sampler>,
    /// Reused index-expansion output (triangle-list expansion of a guest index
    /// buffer); sized and fully overwritten by [`Device::draw_indexed_primitive`].
    index_scratch: Vec<u8>,
    /// Reused rebased GPU indices; batch data owns the queued snapshot.
    index_rebased: Vec<u32>,
    /// Reused software-lighting output for the normal-bearing FVFs (0x112 and
    /// 0x152); sized and fully overwritten by `light_vertices`.
    lit_scratch: Vec<u8>,
    /// Persistent upload buffer for immutable batched uniforms, vertices and
    /// indices. Each flush uploads and submits in draw order.
    draw_buffer: std::cell::RefCell<Option<wgpu::Buffer>>,
    draw_staging: std::cell::RefCell<wgpu::util::StagingBelt>,
    draw_layouts: DrawLayouts,
    /// Group-0 bind groups over `draw_buffer` (unlit, textured); the real block
    /// offsets are dynamic. Dropped when the buffer is recreated.
    uniform_groups: std::cell::RefCell<[Option<wgpu::BindGroup>; 2]>,
    /// Stage texture/sampler groups, keyed by the bound objects. Cleared when
    /// it grows past `MAX_TEXTURE_GROUPS` so it cannot pin textures forever.
    texture_groups: std::cell::RefCell<FastMap<TextureGroupKey, wgpu::BindGroup>>,
    batch: std::cell::RefCell<DrawBatch>,
    /// Present handoff ring (see [`FrameRing`]); created on first use.
    frame_ring: std::cell::RefCell<Option<FrameRing>>,
    rgb565_handoff: std::cell::RefCell<Option<wgpu::ComputePipeline>>,
    /// The batch's target needs `publish_target` after it is flushed.
    publish_pending: std::cell::Cell<bool>,
    submits: std::cell::Cell<u64>,
    buffers_created_at_flush: std::cell::Cell<u64>,
    /// `RECOMP_D3D8_DRAW_STATS=1` draw-path counters, reported per frame.
    draw_stats_enabled: bool,
    frame_stats: DrawStats,
    white: BoundTexture,
    scene: bool,
    /// `RECOMP_D3D8_SURVEY=1`: record and skip unsupported draws.
    survey: bool,
    /// `RECOMP_D3D8_STRICT_SCOPES=1`: restore one validation error scope per
    /// draw instead of one per frame. Debug aid only.
    strict_scopes: bool,
    /// The managed per-frame validation error scope.
    frame_scope: Option<wgpu::ErrorScopeGuard>,
    frame_index: u64,
    draw_index: u64,
}

impl Device {
    pub fn new(
        gpu: GpuContext,
        width: u32,
        height: u32,
        format: u32,
        depth_format: u32,
    ) -> Result<Self, RenderError> {
        let target = gpu.create_target(width, height, format, depth_format)?;
        let white = create_white_texture(&gpu);
        let draw_layouts = DrawLayouts::new(&gpu.device);
        let survey = survey::enabled();
        if survey {
            survey::register_exit_report();
        }
        let strict_scopes = std::env::var("RECOMP_D3D8_STRICT_SCOPES").as_deref() == Ok("1");
        let draw_stats_enabled = std::env::var("RECOMP_D3D8_DRAW_STATS").as_deref() == Ok("1");
        stats::register_exit_report();
        Ok(Self {
            shaders: Default::default(),
            vertex_shader: 0,
            pixel_shader: 0,
            shader_uniform: Default::default(),
            programmable_modules: Default::default(),
            shader: None,
            textured_shader: None,
            lit_shader: None,
            lit_textured_shader: None,
            gpu_lit_shaders: Default::default(),
            cpu_lighting: std::env::var_os("RECOMP_D3D8_CPU_LIGHTING").is_some(),
            pipelines: FastMap::default(),
            state: DeviceState::new(width, height),
            stage_textures: std::array::from_fn(|_| None),
            texture_cache: TextureCache::new(MAX_CACHED_TEXTURES),
            scratch_rgba: Vec::new(),
            samplers: KeyedCache::default(),
            index_scratch: Vec::new(),
            index_rebased: Vec::new(),
            lit_scratch: Vec::new(),
            draw_buffer: Default::default(),
            draw_staging: std::cell::RefCell::new(wgpu::util::StagingBelt::new(
                gpu.device.clone(),
                1 << 20,
            )),
            draw_layouts,
            uniform_groups: Default::default(),
            texture_groups: Default::default(),
            batch: Default::default(),
            frame_ring: Default::default(),
            rgb565_handoff: Default::default(),
            publish_pending: Default::default(),
            submits: Default::default(),
            buffers_created_at_flush: Default::default(),
            draw_stats_enabled,
            frame_stats: DrawStats::default(),
            white,
            gpu,
            targets: Targets::new(target.clone()),
            target,
            scene: false,
            survey,
            strict_scopes,
            frame_scope: None,
            frame_index: 0,
            draw_index: 0,
        })
    }

    /// Clear explicit screen rectangles clipped to the viewport, or the whole
    /// viewport for an empty slice. Independent clear passes preserve draw order.
    pub fn clear(
        &mut self,
        rects: &[crate::d3d8::state::Rect],
        flags: u32,
        argb: u32,
        z: f32,
        stencil: u32,
    ) -> Result<(), RenderError> {
        if flags & !(CLEAR_TARGET | CLEAR_ZBUFFER | CLEAR_STENCIL) != 0 || flags == 0 {
            return Err(RenderError::new(
                "Clear",
                format!("unsupported D3DCLEAR flags {flags:#x}"),
            ));
        }
        if !z.is_finite() || !(0.0..=1.0).contains(&z) {
            return Err(RenderError::new(
                "Clear",
                format!("depth clear value {z} is outside [0,1]"),
            ));
        }
        self.flush_draws();
        let v = self.state.viewport;
        // Validate attachments even when all rectangles clip to empty.
        if flags & (CLEAR_ZBUFFER | CLEAR_STENCIL) != 0 && self.target.depth.is_none() {
            return Err(RenderError::new(
                "Clear",
                "depth/stencil requested without attachment",
            ));
        }
        if flags & CLEAR_STENCIL != 0 && !self.target.depth.as_ref().is_some_and(|d| d.has_stencil)
        {
            return Err(RenderError::new(
                "Clear",
                "stencil requested without stencil aspect",
            ));
        }
        if rects.is_empty() {
            self.gpu.clear_region(
                &self.target,
                flags,
                argb,
                z,
                stencil,
                v.x,
                v.y,
                v.width,
                v.height,
            )?;
        } else {
            for r in rects {
                let x1 = i64::from(r.x1).max(i64::from(v.x));
                let y1 = i64::from(r.y1).max(i64::from(v.y));
                let x2 = i64::from(r.x2).min(i64::from(v.x) + i64::from(v.width));
                let y2 = i64::from(r.y2).min(i64::from(v.y) + i64::from(v.height));
                if x2 > x1 && y2 > y1 {
                    self.gpu.clear_region(
                        &self.target,
                        flags,
                        argb,
                        z,
                        stencil,
                        x1 as u32,
                        y1 as u32,
                        (x2 - x1) as u32,
                        (y2 - y1) as u32,
                    )?;
                }
            }
        }
        self.publish_target();
        Ok(())
    }

    /// `IDirect3DDevice8::SetTexture`. Each entry of `levels` is one mip slice
    /// (`level`, `generation`, the current lock state and the CPU texel block
    /// in the source D3D8 layout). An empty slice or `texture_id == 0` unbinds;
    /// the sampler keeps the white fallback, but a stage op that reads TEXTURE
    /// is rewritten to SELECTARG1(CURRENT) by `resolve_texture_stage`.
    ///
    /// The wgpu texture holds the whole chain with `mip_level_count = N`. Every
    /// level tracks its own content generation, so re-locking level N re-uploads
    /// only N, and a base-dimension change rebuilds the chain. Content is
    /// converted to linear UNORM `Rgba8Unorm`, except V8U8 which retains its
    /// signed bytes in `Rg8Snorm`; a block-compressed level decodes
    /// from its full block layout but keeps the clipped `width`/`height` (a 1x1
    /// DXT level still occupies one 4x4 block in the source).
    pub fn set_texture(
        &mut self,
        stage: u32,
        texture_id: u32,
        format: u32,
        levels: &[TextureLevelUpload<'_>],
    ) -> Result<(), RenderError> {
        use crate::d3d8::format::ColorFormat;
        if stage as usize >= MAX_TEXTURE_STAGES {
            return Err(RenderError::new(
                "SetTexture",
                format!("stage {stage} is beyond D3D8's {MAX_TEXTURE_STAGES} texture stages"),
            ));
        }
        if texture_id == 0 || levels.is_empty() {
            self.stage_textures[stage as usize] = None;
            return Ok(());
        }
        let base_width = levels[0].width;
        let base_height = levels[0].height;
        if base_width == 0 || base_height == 0 {
            return Err(RenderError::new(
                "SetTexture",
                "bound texture dimensions must be nonzero",
            ));
        }
        // Block-compressed levels are sized by their block layout; the rest by
        // their per-texel layout. V8U8 uploads signed bytes directly; color
        // and block-compressed formats decode to RGBA.
        let block = crate::d3d8::format::block_bytes(format);
        let signed_bump = format == crate::d3d8::format::D3DFMT_V8U8;
        let color = if block == 0 && !signed_bump {
            Some(ColorFormat::from_d3dformat(format)?)
        } else {
            None
        };
        let level_count = levels
            .iter()
            .map(|l| l.level)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| RenderError::new("SetTexture", "mip level overflow"))?;
        if level_count > 16 {
            return Err(RenderError::new(
                "SetTexture",
                format!("bound texture has {level_count} mip levels; D3D8 caps at 16"),
            ));
        }
        let max = self.gpu.device.limits().max_texture_dimension_2d;
        if base_width > max || base_height > max {
            return Err(RenderError::new(
                "SetTexture",
                format!(
                    "bound texture {base_width}x{base_height} exceeds max_texture_dimension_2d {max}"
                ),
            ));
        }

        // Temporary diagnostic (`RECOMP_D3D8_DUMP_TEXTURES`): decode the same
        // bytes a real upload would convert and write each level as a PNG.
        if crate::d3d8::dump::texture_enabled() {
            for l in levels {
                let Ok(expected) = source_level_bytes(format, color, l.width, l.height) else {
                    continue;
                };
                if l.data.len() < expected {
                    continue;
                }
                let mut rgba = Vec::new();
                // PNG diagnostics visualize signed U,V biased to [0,255].
                // The GPU upload retains the original signed bytes.
                let decoded = if signed_bump {
                    for uv in l.data[..expected].chunks_exact(2) {
                        rgba.extend_from_slice(&[uv[0] ^ 0x80, uv[1] ^ 0x80, 0, 255]);
                    }
                    Ok(())
                } else {
                    match color {
                        Some(color) => {
                            color.to_rgba8_into(&l.data[..expected], &mut rgba);
                            Ok(())
                        }
                        None => crate::d3d8::format::decode_block_into(
                            format,
                            &l.data[..expected],
                            l.width,
                            l.height,
                            &mut rgba,
                        ),
                    }
                };
                if decoded.is_ok() {
                    crate::d3d8::dump::texture(
                        stage,
                        texture_id,
                        l.level,
                        l.generation,
                        format,
                        l.width,
                        l.height,
                        &rgba,
                    );
                }
            }
        }

        stats::record_bind();
        let key = TextureKey::new(texture_id, 0);
        // A render-target-backed level keeps its existing path; its sampling
        // chain is still level 0 only (mip render targets are a separate path).
        if self.targets.textures.contains_key(&key) {
            let base = &levels[0];
            self.sync_target_cpu(
                key,
                base.generation,
                format,
                base_width,
                base_height,
                base.data,
                base.force_upload,
            )?;
            let rt = &self.targets.textures[&key];
            self.stage_textures[stage as usize] = Some(BoundTexture {
                _texture: rt.surface.texture.clone(),
                view: rt.surface.view.clone(),
                texture_id,
                width: base_width,
                height: base_height,
                format,
                max_written_level: 0,
                level_count: 1,
                level_generations: vec![base.generation],
                written: vec![true],
            });
            return Ok(());
        }

        let existing_shape = self.texture_cache.resource(key).map(|c| {
            (
                c.width,
                c.height,
                c.format,
                c.level_generations.len() as u32,
            )
        });
        let needs_new = existing_shape != Some((base_width, base_height, format, level_count));

        if needs_new {
            // Replacing the chain may rewrite a texture recorded draws sample.
            self.flush_draws();
            let scope = self
                .gpu
                .device
                .push_error_scope(wgpu::ErrorFilter::Validation);
            let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("D3D8 stage texture"),
                size: wgpu::Extent3d {
                    width: base_width,
                    height: base_height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: level_count,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: if signed_bump {
                    wgpu::TextureFormat::Rg8Snorm
                } else {
                    wgpu::TextureFormat::Rgba8Unorm
                },
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            if let Some(err) = pollster::block_on(scope.pop()) {
                return Err(RenderError::new("SetTexture", err.to_string()));
            }
            let mut level_generations = vec![0u64; level_count as usize];
            let mut written = vec![false; level_count as usize];
            for l in levels {
                if l.level as usize >= level_count as usize {
                    continue;
                }
                upload_texture_level(
                    &self.gpu.queue,
                    &texture,
                    l.level,
                    format,
                    color,
                    l.width,
                    l.height,
                    l.data,
                    &mut self.scratch_rgba,
                )?;
                level_generations[l.level as usize] = l.generation;
                written[l.level as usize] = l.generation != 0 || l.force_upload;
                stats::record_upload();
            }
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.texture_cache.insert(
                key,
                CachedTexture {
                    texture,
                    view,
                    width: base_width,
                    height: base_height,
                    format,
                    level_generations,
                    written,
                },
                0,
                base_width,
                base_height,
                format,
            );
        } else {
            // Same shape: re-upload only the levels whose generation changed
            // (or whose lock is still open).
            let changed: Vec<usize> = levels
                .iter()
                .enumerate()
                .filter(|(_, l)| {
                    self.texture_cache
                        .resource(key)
                        .and_then(|c| c.level_generations.get(l.level as usize))
                        != Some(&l.generation)
                        || l.force_upload
                })
                .map(|(i, _)| i)
                .collect();
            if !changed.is_empty() {
                self.flush_draws();
                for i in changed {
                    let l = &levels[i];
                    let cached = self.texture_cache.resource(key).expect("chain present");
                    upload_texture_level(
                        &self.gpu.queue,
                        &cached.texture,
                        l.level,
                        format,
                        color,
                        l.width,
                        l.height,
                        l.data,
                        &mut self.scratch_rgba,
                    )?;
                    stats::record_upload();
                    if let Some(cached) = self.texture_cache.resource_mut(key) {
                        if let Some(slot) = cached.level_generations.get_mut(l.level as usize) {
                            *slot = l.generation;
                        }
                        if let Some(slot) = cached.written.get_mut(l.level as usize) {
                            *slot = true;
                        }
                    }
                }
            }
        }
        let cached = self.texture_cache.resource(key).expect("just stored");
        let max_written_level = cached.max_written_level();
        self.stage_textures[stage as usize] = Some(BoundTexture {
            _texture: cached.texture.clone(),
            view: cached.view.clone(),
            texture_id,
            width: base_width,
            height: base_height,
            format,
            max_written_level,
            level_count: cached.level_generations.len() as u32,
            level_generations: cached.level_generations.clone(),
            written: cached.written.clone(),
        });
        Ok(())
    }

    /// Drop every resident level of a guest texture. Called from the kit when
    /// the texture's refcount reaches zero, so GPU textures do not outlive the
    /// guest object. The identity is never reused, so no stale entry can be
    /// read by a later texture.
    pub fn release_texture(&mut self, texture_id: u32) {
        if texture_id == 0 {
            return;
        }
        self.targets.depths.remove(&texture_id);
        self.texture_cache.remove_texture(texture_id);
        self.targets
            .textures
            .retain(|key, _| key.texture_id != texture_id);
        for slot in &mut self.stage_textures {
            if slot.as_ref().is_some_and(|t| t.texture_id == texture_id) {
                *slot = None;
            }
        }
    }

    /// True when a depth attachment was created for the device.
    pub fn has_depth(&self) -> bool {
        self.target.depth.is_some()
    }

    /// The wgpu depth-stencil state a draw pipeline needs for the current
    /// D3D8 render state. `None` when depth testing is disabled (or the device
    /// has no depth attachment); otherwise it carries `D3DRS_ZWRITEENABLE` and
    /// `D3DRS_ZFUNC` and the attachment format. Stencil is not part of the
    /// unlit slice, so its wgpu state is all-off/keep.
    ///
    /// wgpu requires a pipeline's depth-stencil format to match the render
    /// pass attachment, so whenever the device owns a depth attachment the
    /// pipeline declares it. `D3DRS_ZENABLE` then selects behaviour: with
    /// depth off the pipeline compares `Always` and writes nothing, which is
    /// what D3D8 does (the attachment stays bound but does not affect output).
    fn depth_stencil_state(&self) -> Option<wgpu::DepthStencilState> {
        use crate::d3d8::enums::D3DCMPFUNC;
        let depth = self.target.depth.as_ref()?;
        let (compare, write) = if self.state.z_enable() {
            let compare = match self.state.z_func() {
                D3DCMPFUNC::Never => wgpu::CompareFunction::Never,
                D3DCMPFUNC::Less => wgpu::CompareFunction::Less,
                D3DCMPFUNC::Equal => wgpu::CompareFunction::Equal,
                D3DCMPFUNC::LessEqual => wgpu::CompareFunction::LessEqual,
                D3DCMPFUNC::Greater => wgpu::CompareFunction::Greater,
                D3DCMPFUNC::NotEqual => wgpu::CompareFunction::NotEqual,
                D3DCMPFUNC::GreaterEqual => wgpu::CompareFunction::GreaterEqual,
                D3DCMPFUNC::Always => wgpu::CompareFunction::Always,
            };
            (compare, self.state.z_write_enable())
        } else {
            (wgpu::CompareFunction::Always, false)
        };
        let bias = z_bias_state(self.state.z_bias());
        Some(wgpu::DepthStencilState {
            format: depth.format,
            depth_write_enabled: Some(write),
            depth_compare: Some(compare),
            stencil: wgpu::StencilState::default(),
            bias,
        })
    }

    /// Map the current D3D8 alpha-blend state onto a wgpu pipeline blend state.
    ///
    /// `None` when `D3DRS_ALPHABLENDENABLE` is FALSE (opaque). D3D8 has no
    /// separate alpha blend, so the same source/destination factors apply to
    /// the alpha channel. The two D3D8 "both" factors are rejected by name:
    /// their documented meaning (source alpha for both factors) does not map
    /// onto wgpu's independent colour/alpha components without an unproven
    /// reading, so the draw fails loudly instead of approximating.
    fn blend_state(&self) -> Result<Option<wgpu::BlendState>, RenderError> {
        if !self.state.alpha_blend_enable() {
            return Ok(None);
        }
        let src = blend_factor(self.state.src_blend())?;
        let dst = blend_factor(self.state.dest_blend())?;
        let op = blend_operation(self.state.blend_op());
        Ok(Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: src,
                dst_factor: dst,
                operation: op,
            },
            alpha: wgpu::BlendComponent {
                src_factor: src,
                dst_factor: dst,
                operation: op,
            },
        }))
    }

    /// Stable key for the blend part of the pipeline cache key. Zero when
    /// blending is off.
    fn blend_key(&self) -> u32 {
        if !self.state.alpha_blend_enable() {
            return 0;
        }
        (self.state.blend_op().raw() & 0xff)
            | ((self.state.src_blend().raw() & 0xff) << 8)
            | ((self.state.dest_blend().raw() & 0xff) << 16)
    }

    /// Map `D3DRS_CULLMODE` onto wgpu's cull face and front-face winding.
    ///
    /// D3D8 and WebGPU both define front-face winding in framebuffer/screen
    /// space (y down), so the cull sense carries over directly: `D3DCULL_CW`
    /// culls clockwise faces, keeping counter-clockwise as front, and
    /// `D3DCULL_CCW` the opposite. The readback test
    /// (`probe` "cull winding/readback") exercises all three modes; see the
    /// 2026-10 cull verdict in the report.
    fn cull_state(&self) -> (Option<wgpu::Face>, wgpu::FrontFace) {
        Self::cull_mode_to_wgpu(self.effective_cull_mode())
    }

    /// Pure `D3DCULL` -> wgpu cull/front-face mapping, split out so the winding
    /// convention is unit-testable without a device. D3D8 culls the named winding
    /// (so `D3DCULL_CCW` keeps clockwise as front) and wgpu's framebuffer-space
    /// `FrontFace` uses the same y-down convention.
    fn cull_mode_to_wgpu(
        mode: crate::d3d8::enums::D3DCULL,
    ) -> (Option<wgpu::Face>, wgpu::FrontFace) {
        use crate::d3d8::enums::D3DCULL;
        match mode {
            D3DCULL::None => (None, wgpu::FrontFace::Ccw),
            D3DCULL::Cw => (Some(wgpu::Face::Back), wgpu::FrontFace::Ccw),
            D3DCULL::Ccw => (Some(wgpu::Face::Back), wgpu::FrontFace::Cw),
        }
    }

    /// The cull mode the pipeline will actually use, after the diagnostic
    /// override in [`cull_override`]. Faithful D3D8 behaviour when the
    /// override is unset.
    fn effective_cull_mode(&self) -> crate::d3d8::enums::D3DCULL {
        use crate::d3d8::enums::D3DCULL;
        let mode = self.state.cull_mode();
        match cull_override() {
            None => mode,
            Some(CullOverride::None) => D3DCULL::None,
            Some(CullOverride::Cw) => D3DCULL::Cw,
            Some(CullOverride::Ccw) => D3DCULL::Ccw,
            Some(CullOverride::Flip) => match mode {
                D3DCULL::Cw => D3DCULL::Ccw,
                D3DCULL::Ccw => D3DCULL::Cw,
                D3DCULL::None => D3DCULL::None,
            },
        }
    }

    /// Resolve the D3D8 table/pixel fog uniform for the current state.
    ///
    /// The D3D8 rule for additive blending adjusts the fog colour to black;
    /// `validate_unlit` has already refused the blend combinations whose
    /// adjustment is not implemented, so the cases handled here (opaque, the
    /// standard INVSRCALPHA destination, and DESTBLEND=ONE) are the only ones a
    /// validated draw can hold.
    fn fog_uniform(&self) -> FogUniform {
        use crate::d3d8::enums::D3DBLEND;
        let enable = self.state.fog_enable();
        let mut color = crate::d3d8::fixed_function::d3dcolor_to_rgba(self.state.fog_color());
        if enable && self.state.alpha_blend_enable() && self.state.dest_blend() == D3DBLEND::One {
            color = [0.0, 0.0, 0.0, 1.0];
        }
        // D3D8: a non-NONE FOGTABLEMODE selects per-pixel table fog; otherwise
        // FOGVERTEXMODE selects per-vertex fog, the only kind range-based
        // distance applies to (`validate_*` refuses range with table fog).
        let table = self.state.fog_table_mode();
        let vertex_fog = table == 0;
        FogUniform::new(
            enable,
            if vertex_fog {
                self.state.fog_vertex_mode()
            } else {
                table
            },
            vertex_fog,
            vertex_fog && self.state.range_fog_enable(),
            self.state.fog_start(),
            self.state.fog_end(),
            self.state.fog_density(),
            color,
            self.state.world.mul(self.state.view),
        )
    }

    /// Release device cache entries for a deleted shader. Already queued
    /// draws retain cloned pipelines, so retirement does not alter them.
    pub fn retire_shader(&mut self, handle: u32) {
        self.shaders.remove(&handle);
        self.programmable_modules
            .retain(|&(vs, ps, _, _), _| vs != handle && ps != handle);
        self.pipelines
            .retain(|key, _| key.vs != handle && key.ps != handle);
    }

    pub fn begin_scene(&mut self) -> Result<(), RenderError> {
        if self.scene {
            return Err(RenderError::new("BeginScene", "scene already open"));
        }
        self.scene = true;
        Ok(())
    }

    pub fn end_scene(&mut self) -> Result<(), RenderError> {
        if !self.scene {
            return Err(RenderError::new("EndScene", "no open scene"));
        }
        self.scene = false;
        self.flush_draws();
        self.finish_frame_scope()?;
        Ok(())
    }

    pub fn draw_primitive(
        &mut self,
        topology: u32,
        fvf: u32,
        vertices: &VertexBuffer,
        start_vertex: u32,
        primitive_count: u32,
    ) -> Result<(), RenderError> {
        if matches!(topology, 5 | 6) {
            if primitive_count == 0 {
                return Ok(());
            }
            let mut scratch = std::mem::take(&mut self.index_scratch);
            let result = (|| {
                expand_nonindexed_into(
                    &mut scratch,
                    vertices,
                    topology,
                    start_vertex,
                    primitive_count,
                )?;
                let view = VertexBuffer::borrowed(&scratch, vertices.stride)?;
                self.draw_stream(4, fvf, &view, 0, primitive_count, None)
            })();
            self.index_scratch = scratch;
            return result;
        }
        self.draw_stream(topology, fvf, vertices, start_vertex, primitive_count, None)
    }

    // Indexed streams use the same state validation and immutable batch data
    // as ordinary draws; their consumed vertex count comes from the range.
    fn draw_stream(
        &mut self,
        topology: u32,
        fvf: u32,
        vertices: &VertexBuffer,
        start_vertex: u32,
        primitive_count: u32,
        indices: Option<&[u32]>,
    ) -> Result<(), RenderError> {
        if !self.scene {
            return Err(RenderError::new(
                "DrawPrimitive",
                "requires an open scene (BeginScene)",
            ));
        }
        // A zero-primitive draw is a legal no-op in D3D, not an error. The
        // degenerate tail of a strip or fan (two or fewer vertices) lands here.
        if primitive_count == 0 {
            return Ok(());
        }
        self.draw_index += 1;
        self.frame_stats.draws += 1;
        if skipped_fvf(fvf) {
            return Ok(());
        }
        // Survey mode records and skips a state/FVF/TSS rejection so one run
        // can enumerate every unsupported draw. Argument/range errors below
        // deliberately bypass this and stay hard failures.
        macro_rules! survey_or_skip {
            ($expr:expr) => {
                match $expr {
                    Ok(value) => value,
                    Err(error) => {
                        if self.note_draw_rejection(&error, topology, fvf, vertices.stride) {
                            return Ok(());
                        }
                        return Err(error);
                    }
                }
            };
        }
        let vertex_count = match topology_vertex_count(topology, primitive_count) {
            Ok(count) => count,
            Err(error) => {
                if self.note_draw_rejection(&error, topology, fvf, vertices.stride) {
                    return Ok(());
                }
                return Err(error);
            }
        };
        let bound_vs = self.shaders.get(&self.vertex_shader);
        let fvf = if let Some((Some(d), p)) = bound_vs {
            if p.declaration_only {
                d.fixed_fvf()?
            } else {
                fvf
            }
        } else {
            fvf
        };
        let vs = bound_vs.filter(|(_, p)| !p.declaration_only);
        let ps = self.shaders.get(&self.pixel_shader).map(|s| &s.1);
        if let Some(ps) = ps {
            ps.validate_draw()?;
        }
        let programmable = vs.is_some() || ps.is_some();
        if vs.is_some() {
            survey_or_skip!(self.state.validate_shader_draw());
        } else {
            survey_or_skip!(self.state.validate_draw(fvf));
        }
        if topology == 1 {
            // wgpu's PointList is fixed at one pixel; only D3D8's default point
            // configuration is faithful (see `validate_point_draw`).
            survey_or_skip!(self.state.validate_point_draw());
        }
        if self.state.z_enable() && !self.has_depth() {
            let error = RenderError::new(
                "DrawPrimitive",
                "D3DRS_ZENABLE is on but the device has no depth attachment",
            );
            if self.note_draw_rejection(&error, topology, fvf, vertices.stride) {
                return Ok(());
            }
            return Err(error);
        }
        let mut layout = if let Some((Some(decl), _)) = vs {
            decl.layout.clone()
        } else {
            survey_or_skip!(FvfLayout::decode(fvf))
        };
        if vs.is_some() && u64::from(vertices.stride) >= layout.stride {
            layout.stride = u64::from(vertices.stride);
        }
        if u64::from(vertices.stride) != layout.stride {
            let error = RenderError::new("SetStreamSource", "stride does not match FVF");
            if self.note_draw_rejection(&error, topology, fvf, vertices.stride) {
                return Ok(());
            }
            return Err(error);
        }
        let vertex_count = if indices.is_some() {
            (vertices.bytes().len() / vertices.stride as usize) as u32
        } else {
            vertex_count
        };
        let count = start_vertex
            .checked_add(vertex_count)
            .ok_or_else(|| RenderError::new("DrawPrimitive", "vertex range overflow"))?;
        if count as u64 * layout.stride > vertices.bytes().len() as u64 {
            return Err(RenderError::new(
                "DrawPrimitive",
                "vertex range exceeds buffer",
            ));
        }
        // Resolve both implemented stages. A stage with no explicit op and no
        // bound texture stays inactive and passes CURRENT through, preserving
        // the established untextured path. Stage >= 2 that is active fails by
        // name inside the resolver and is recorded by the survey.
        let inactive_sampler = DeviceState::inactive_texture_stage()?;
        let mut stages = [inactive_sampler; 4];
        for stage in 0..4 {
            let resolved = if let Some(ps) = ps {
                if ps.samplers[stage] {
                    survey_or_skip!(self.state.resolve_shader_sampler(stage))
                } else {
                    inactive_sampler
                }
            } else if stage < 2 && vs.is_some() {
                survey_or_skip!(self.state.resolve_shader_fixed_pixel_stage(
                    stage as u32,
                    self.stage_textures[stage].is_some()
                ))
            } else if stage < 2 {
                survey_or_skip!(
                    self.state
                        .resolve_texture_stage(stage as u32, self.stage_textures[stage].is_some())
                )
            } else {
                inactive_sampler
            };
            stages[stage] = resolved;
        }
        let stage0 = stages[0];
        let stage1 = stages[1];
        let textured = programmable || stage0.active || stage1.active;
        if let Some(key) = self.targets.current {
            for (stage, resolved) in stages.iter().enumerate() {
                let active = resolved.active;
                if active
                    && self.stage_textures[stage]
                        .as_ref()
                        .is_some_and(|t| t.texture_id == key.texture_id)
                {
                    return Err(RenderError::new(
                        "DrawPrimitive",
                        "sampling the current color render target is unsupported",
                    ));
                }
            }
        }
        // Color-only RHW vertices carry no texture coordinates even when a
        // texture remains bound. The RHW color entry point supplies zero UVs,
        // and for_pre_transformed marks both coordinate sets unavailable,
        // following the missing-set behavior documented in for_fvf. Keep the
        // stage operations and bound textures rather than rejecting the draw.
        let color_only_rhw = fvf == 0x44;
        if textured
            && vs.is_none()
            && !layout.attributes.iter().any(|a| a.shader_location == 2)
            && !color_only_rhw
        {
            let error = RenderError::new(
                "DrawPrimitive",
                "an active texture stage requires D3DFVF_TEX1 texture coordinates",
            );
            if self.note_draw_rejection(&error, topology, fvf, vertices.stride) {
                return Ok(());
            }
            return Err(error);
        }
        // Only the two-set FVF carries texcoord set 1. A single-set FVF aliases
        // set 1 to set 0 for the shared shader, but a stage that selects the
        // missing set is not an error and is not aliased: D3D8 fixed-function
        // leaves an undeclared coordinate uninitialized, and the reference
        // implementations resolve that to `(0, 0)`. `StagesUniform::for_fvf`
        // records which selected sets the FVF carries so the shader substitutes
        // the zero coordinate, matching WineD3D/DXVK rather than failing the
        // draw.
        trace_draw(
            fvf,
            topology,
            &layout,
            vertices,
            start_vertex,
            primitive_count,
            count,
            &self.state,
            textured,
            self.stage_textures[0].as_ref(),
            self.stage_textures[1].as_ref(),
        );
        let v = &self.state.viewport;
        if v.width == 0
            || v.height == 0
            || v.x
                .checked_add(v.width)
                .is_none_or(|n| n > self.target.width)
            || v.y
                .checked_add(v.height)
                .is_none_or(|n| n > self.target.height)
            || !v.min_z.is_finite()
            || !v.max_z.is_finite()
            || v.min_z < 0.0
            || v.max_z > 1.0
            || v.min_z > v.max_z
        {
            return Err(RenderError::new(
                "SetViewport",
                "viewport outside target or invalid depth interval",
            ));
        }
        // Target formats are fixed for this device. The supported slice only
        // varies vertex layout and effective depth/blend pipeline state.
        let depth_state = self.depth_stencil_state();
        let blend = survey_or_skip!(self.blend_state());
        let (cull_mode, front_face) = if matches!(topology, 1 | 2) {
            // Culling does not apply to points or lines; keep the pipeline independent
            // of the triangle cull state.
            (None, wgpu::FrontFace::Ccw)
        } else {
            self.cull_state()
        };
        let lighting_uniform = if !self.cpu_lighting && vs.is_none() && matches!(fvf, 0x112 | 0x152)
        {
            if self.state.gpu_lighting_positions_supported(
                vertices.bytes(),
                start_vertex as usize,
                count as usize,
                vertices.stride as usize,
            ) {
                survey_or_skip!(self.state.gpu_lighting_uniform())
            } else {
                None
            }
        } else {
            None
        };
        let gpu_lighting = lighting_uniform.is_some();
        let extended_lighting = gpu_lighting && ps.is_some();
        let lighting_offset = if extended_lighting {
            DRAW_UB_LIGHTING_EXTENDED
        } else {
            DRAW_UB_LIGHTING
        };
        let key = DrawPipelineKey {
            gpu_lighting,
            topology: wgpu_topology(topology),
            color_format: self.target.format,
            color_write_mask: color_writes_from_mask(self.state.color_write_mask())
                & self.target.color_write_mask(),
            depth_format: self.target.depth.as_ref().map(|d| d.format),
            fvf,
            vs: self.vertex_shader,
            ps: self.pixel_shader,
            stride: layout.stride,
            textured,
            z_enable: self.state.z_enable(),
            z_write: self.state.z_enable() && self.state.z_write_enable(),
            z_func: if self.state.z_enable() {
                self.state.z_func().raw()
            } else {
                8
            },
            z_bias: self.state.z_bias(),
            blend: self.blend_key(),
            cull: self.effective_cull_mode().raw(),
        };
        // Resolve fog and alpha-test state before the shader borrow because
        // these methods read several state fields (a call borrows all of `self`).
        let mut fog_uniform = self.fog_uniform();
        if vs.is_some() && self.state.fog_table_mode() == 0 {
            fog_uniform.vertex_fog = 1;
        }
        let mut program_uniform = self.shader_uniform;
        for stage in 0..4 {
            program_uniform.bump[stage] = std::array::from_fn(|i| {
                f32::from_bits(
                    self.state
                        .texture_stage_state(stage as u32, 7 + i as u32)
                        .unwrap_or(0),
                )
            });
            program_uniform.lum[stage][0] = f32::from_bits(
                self.state
                    .texture_stage_state(stage as u32, 22)
                    .unwrap_or(0),
            );
            program_uniform.lum[stage][1] = f32::from_bits(
                self.state
                    .texture_stage_state(stage as u32, 23)
                    .unwrap_or(0),
            );
            program_uniform.lod[stage][0] = stages[stage].lod_bias;
            if ps.is_some() && vs.is_none() && stages[stage].active {
                let coord = self
                    .state
                    .texture_stage_state(stage as u32, 11)
                    .unwrap_or(stage as u32);
                let flags = self
                    .state
                    .texture_stage_state(stage as u32, 24)
                    .unwrap_or(0);
                if coord & !0xffff != 0 || flags != 0 {
                    return Err(RenderError::new(
                        "DrawPrimitive",
                        "fixed-function vertex texgen/texture transforms with a pixel shader are unsupported",
                    ));
                }
                program_uniform.lod[stage][1] = if coord < layout.texcoord_sets {
                    coord as f32
                } else {
                    8.0
                };
            }
        }
        if let Some(ps) = ps {
            trace_shader_draw(self, &program_uniform, &ps.samplers, vertices, start_vertex);
        }
        let alpha_test_uniform = AlphaTestUniform::new(
            self.state.alpha_test_enable(),
            self.state.alpha_func().raw(),
            self.state.alpha_ref(),
        );
        // GPU lighting consumes raw guest attributes and the immutable light
        // snapshot. CPU fallback packs only the requested vertex interval;
        // move its reusable scratch out while this method borrows self.
        let lit_input = if vs.is_some() {
            None
        } else {
            match fvf {
                0x0152 => Some(LitInput::XYZ_NORMAL_DIFFUSE_TEX1),
                0x0112 => Some(LitInput::XYZ_NORMAL_TEX1),
                _ => None,
            }
        };
        let mut lit_scratch = lit_input
            .filter(|_| !gpu_lighting)
            .map(|_| std::mem::take(&mut self.lit_scratch));
        if let Some(out) = lit_scratch.as_mut() {
            if let Err(error) = self.state.light_vertices(
                vertices.bytes(),
                start_vertex as usize,
                count as usize,
                lit_input.unwrap(),
                out,
            ) {
                self.lit_scratch = lit_scratch.take().unwrap();
                if self.note_draw_rejection(&error, topology, fvf, vertices.stride) {
                    return Ok(());
                }
                return Err(error);
            }
        }
        let lit_bytes = lit_scratch.as_deref();
        let upload_layout = if gpu_lighting {
            gpu_lit_layout(fvf)
        } else if lit_bytes.is_some() {
            lit_layout()
        } else {
            layout.clone()
        };
        let gpu = &self.gpu.device;
        let draw_scope = if self.strict_scopes {
            Some(gpu.push_error_scope(wgpu::ErrorFilter::Validation))
        } else {
            if self.frame_scope.is_none() {
                self.frame_scope = Some(gpu.push_error_scope(wgpu::ErrorFilter::Validation));
            }
            None
        };
        let shader = if gpu_lighting && !programmable {
            self.gpu_lit_shaders
                .entry((fvf, textured))
                .or_insert_with(|| {
                    gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("D3D8 GPU diffuse lighting"),
                        source: wgpu::ShaderSource::Wgsl(
                            gpu_lit_shader_source(textured, fvf).into(),
                        ),
                    })
                })
        } else if programmable {
            self.programmable_modules
                .entry((self.vertex_shader, self.pixel_shader, fvf, gpu_lighting))
                .or_insert_with(|| {
                    let mut source = shader::compose(
                        vs.and_then(|(d, p)| d.as_ref().map(|d| (d, p))),
                        ps,
                        lit_input.is_some(),
                    );
                    if gpu_lighting {
                        source = gpu_lit_source(source, fvf);
                    }
                    gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("D3D8 shader-model 1.1"),
                        source: wgpu::ShaderSource::Wgsl(source.into()),
                    })
                })
        } else if lit_input.is_some() {
            let slot = if textured {
                &mut self.lit_textured_shader
            } else {
                &mut self.lit_shader
            };
            slot.get_or_insert_with(|| {
                gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("D3D8 software-lit float diffuse"),
                    source: wgpu::ShaderSource::Wgsl(lit_shader_source(textured).into()),
                })
            })
        } else if textured {
            self.textured_shader.get_or_insert_with(|| {
                gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("D3D8 textured FVF"),
                    source: wgpu::ShaderSource::Wgsl(TEXTURED_WGSL.into()),
                })
            })
        } else {
            self.shader.get_or_insert_with(|| {
                gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("D3D8 unlit FVF"),
                    source: wgpu::ShaderSource::Wgsl(UNLIT_WGSL.into()),
                })
            })
        };
        let mut uniform =
            TransformUniform::new(self.state.world, self.state.view, self.state.projection);
        if layout.pre_transformed {
            // XYZRHW: the vertex entry point maps screen pixels to NDC through
            // the same viewport the rasterizer uses. `rhw[0]` marks it; the
            // reciprocal-w field becomes clip w when nonzero (else 1.0).
            uniform.viewport = [v.x as f32, v.y as f32, v.width as f32, v.height as f32];
            uniform.rhw[0] = 1;
        }
        // D3DRS_SPECULARENABLE gates the shader's specular add.
        uniform.rhw[1] = u32::from(self.state.specular_enable());
        let stages_uniform = textured.then(|| {
            if layout.pre_transformed {
                StagesUniform::for_pre_transformed(&stage0, &stage1, layout.texcoord_sets)
            } else {
                StagesUniform::for_fvf(&stage0, &stage1, layout.texcoord_sets)
            }
        });
        // Pack only the requested interval: uploading 0..count repeats the
        // unused prefix on every draw into a shared guest vertex buffer. D3D8
        // shader-model 1.1 has no vertex-index input, so rebasing the host draw
        // to zero preserves every shader input. Guest indices and traces keep
        // their original offsets; queued bytes remain an immutable snapshot.
        let (vertex_source, vertex_len) = if let Some(lit) = lit_bytes {
            (lit, lit.len())
        } else {
            let begin = start_vertex as usize * vertices.stride as usize;
            let end = count as usize * vertices.stride as usize;
            (&vertices.bytes()[begin..end], end - begin)
        };
        let (uniform_offsets, vertex_base, index_range) = {
            let mut batch = self.batch.borrow_mut();
            if batch.draws.is_empty() {
                batch.color_view = Some(self.target.view.clone());
                batch.depth = self
                    .target
                    .depth
                    .as_ref()
                    .map(|d| (d.view.clone(), d.has_stencil));
            }
            let mut ub_storage = [0u8; DRAW_EXTENDED_VERTEX_OFFSET as usize];
            let ub_len = if extended_lighting {
                DRAW_EXTENDED_VERTEX_OFFSET
            } else {
                DRAW_VERTEX_OFFSET
            } as usize;
            let ub = &mut ub_storage[..ub_len];
            let put = |region: &mut [u8], offset: u64, bytes: &[u8]| {
                region[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
            };
            put(ub, DRAW_UB_PROGRAM, bytemuck::bytes_of(&program_uniform));
            if let Some(lighting) = &lighting_uniform {
                put(ub, lighting_offset, bytemuck::bytes_of(lighting));
            }
            put(ub, DRAW_UB_TRANSFORM, bytemuck::bytes_of(&uniform));
            put(ub, DRAW_UB_FOG, bytemuck::bytes_of(&fog_uniform));
            put(
                ub,
                DRAW_UB_ALPHA_TEST,
                bytemuck::bytes_of(&alpha_test_uniform),
            );
            if let Some(stages) = &stages_uniform {
                put(ub, DRAW_UB_STAGES, bytemuck::bytes_of(stages));
            }
            let banks = [
                (DRAW_UB_TRANSFORM, std::mem::size_of::<TransformUniform>()),
                (DRAW_UB_STAGES, std::mem::size_of::<StagesUniform>()),
                (DRAW_UB_FOG, std::mem::size_of::<FogUniform>()),
                (DRAW_UB_ALPHA_TEST, std::mem::size_of::<AlphaTestUniform>()),
                (DRAW_UB_PROGRAM, std::mem::size_of_val(&program_uniform)),
                (lighting_offset, std::mem::size_of::<LightingUniform>()),
            ];
            let mut uniform_offsets = [0; 6];
            for (index, (offset, size)) in banks.into_iter().enumerate() {
                let bytes = &ub[offset as usize..offset as usize + size];
                let shared = batch.last_uniforms[index]
                    .filter(|&(at, len)| len == size && batch.data[at..at + len] == *bytes)
                    .map(|(at, _)| at);
                let at = shared.unwrap_or_else(|| {
                    let at = batch.data.len().next_multiple_of(256);
                    batch.data.resize(at, 0);
                    batch.data.extend_from_slice(bytes);
                    batch.last_uniforms[index] = Some((at, size));
                    at
                });
                uniform_offsets[index] = at as u32;
            }
            // Vertex buffer offsets need 4-byte alignment only.
            let vertex_base = batch.data.len().next_multiple_of(4);
            batch.data.resize(vertex_base, 0);
            batch.data.extend_from_slice(vertex_source);
            // `write_buffer` needs a multiple of COPY_BUFFER_ALIGNMENT.
            let padded = batch.data.len().next_multiple_of(4);
            batch.data.resize(padded, 0);
            let index_range = indices.map(|indices| {
                let at = batch.data.len();
                let bytes = bytemuck::cast_slice(indices);
                batch.data.extend_from_slice(bytes);
                (at as u64, bytes.len() as u64)
            });
            (uniform_offsets, vertex_base as u64, index_range)
        };
        self.frame_stats.uniform_writes += 1;
        if let Some(scratch) = lit_scratch.take() {
            self.lit_scratch = scratch;
        }
        let pipeline = self.pipelines.entry(key).or_insert_with(|| {
            gpu.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(if textured {
                    "D3D8 textured triangle"
                } else {
                    "D3D8 unlit triangle"
                }),
                layout: Some(if textured {
                    &self.draw_layouts.textured
                } else {
                    &self.draw_layouts.unlit
                }),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some(if layout.pre_transformed {
                        if textured && fvf == 0x44 {
                            "vs_rhw_color_main"
                        } else if layout.attributes.iter().any(|a| a.shader_location == 4) {
                            "vs_rhw_main"
                        } else {
                            "vs_rhw_nospec_main"
                        }
                    } else {
                        "vs_main"
                    }),
                    compilation_options: Default::default(),
                    buffers: &[Some(upload_layout.vertex_buffer_layout())],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu_topology(topology),
                    cull_mode,
                    front_face,
                    ..Default::default()
                },
                depth_stencil: depth_state,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.target.format,
                        blend,
                        write_mask: color_writes_from_mask(self.state.color_write_mask())
                            & self.target.color_write_mask(),
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        });
        let pipeline = pipeline.clone();
        let textures = if textured {
            let pairs = std::array::from_fn(|stage| {
                let texture = if stages[stage].active {
                    self.stage_textures[stage].as_ref()
                } else {
                    None
                };
                let view = texture.map(|t| &t.view).unwrap_or(&self.white.view).clone();
                let max_written = texture.map_or(0, |t| t.max_written_level);
                let sampler = self.samplers.get_or_insert_with(
                    SamplerKey::from_stage(&stages[stage], max_written),
                    || gpu.create_sampler(&sampler_descriptor(&stages[stage], max_written)),
                );
                (view, sampler)
            });
            Some(StageBindings { pairs })
        } else {
            None
        };
        self.batch.borrow_mut().draws.push(PendingDraw {
            pipeline,
            uniform_offsets,
            vertex_base,
            vertex_len: vertex_len as u64,
            stages: stages_uniform.is_some(),
            textures,
            viewport: *v,
            count: indices.map_or(vertex_count, |i| i.len() as u32),
            indices: index_range,
        });
        self.publish_pending.set(true);
        if self.strict_scopes || self.batch.borrow().data.len() >= BATCH_FLUSH_BYTES {
            self.flush_draws();
        }
        if let Some(scope) = draw_scope {
            if let Some(err) = pollster::block_on(scope.pop()) {
                self.pipelines.remove(&key);
                self.shader = None;
                self.textured_shader = None;
                return Err(RenderError::new("DrawPrimitive", err.to_string()));
            }
        }
        Ok(())
    }

    /// Replay every recorded draw as one render pass and one submit, then
    /// publish the target if a draw wrote it. See [`DrawBatch`].
    pub(super) fn flush_draws(&self) {
        let mut batch = self.batch.borrow_mut();
        if batch.draws.is_empty() {
            return;
        }
        let gpu = &self.gpu.device;
        let mut slot = self.draw_buffer.borrow_mut();
        let needed = batch.data.len() as u64;
        if slot.as_ref().map_or(true, |b| b.size() < needed) {
            *slot = Some(gpu.create_buffer(&wgpu::BufferDescriptor {
                label: Some("D3D8 draw uniforms+vertices"),
                size: needed.next_power_of_two().max(4096),
                usage: wgpu::BufferUsages::UNIFORM
                    | wgpu::BufferUsages::VERTEX
                    | wgpu::BufferUsages::INDEX
                    | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.buffers_created_at_flush
                .set(self.buffers_created_at_flush.get() + 1);
            *self.uniform_groups.borrow_mut() = [None, None];
        }
        let db = slot.as_ref().unwrap();
        {
            let mut groups = self.uniform_groups.borrow_mut();
            let entry = |binding: u32, size: usize| wgpu::BindGroupEntry {
                binding,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: db,
                    offset: 0,
                    size: wgpu::BufferSize::new(size as u64),
                }),
            };
            let t = entry(0, std::mem::size_of::<TransformUniform>());
            let st = entry(1, std::mem::size_of::<StagesUniform>());
            let f = entry(2, std::mem::size_of::<FogUniform>());
            let a = entry(3, std::mem::size_of::<AlphaTestUniform>());
            let p = entry(4, std::mem::size_of::<shader::Uniform>());
            let l = entry(5, std::mem::size_of::<LightingUniform>());
            if groups[0].is_none() {
                groups[0] = Some(gpu.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("D3D8 unlit uniforms binding"),
                    layout: &self.draw_layouts.bgl_unlit,
                    entries: &[t.clone(), f.clone(), a.clone(), l.clone()],
                }));
            }
            if groups[1].is_none() {
                groups[1] = Some(gpu.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("D3D8 textured uniforms binding"),
                    layout: &self.draw_layouts.bgl_textured,
                    entries: &[t, st, f, a, p, l],
                }));
            }
        }
        let uniform_groups = self.uniform_groups.borrow();
        let mut texture_groups = self.texture_groups.borrow_mut();
        let color_view = batch.color_view.take().expect("batch has a color target");
        let depth = batch.depth.take();
        let mut encoder = gpu.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("D3D8 draw batch"),
        });
        let mut staging = self.draw_staging.borrow_mut();
        staging
            .write_buffer(
                &mut encoder,
                db,
                0,
                wgpu::BufferSize::new(needed).expect("draw batch upload is nonempty"),
            )
            .copy_from_slice(&batch.data);
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("D3D8 draw batch"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                // The pass must attach the same depth format the pipelines
                // declare; stencil is only present for a stencil format.
                depth_stencil_attachment: depth.as_ref().map(|(view, has_stencil)| {
                    wgpu::RenderPassDepthStencilAttachment {
                        view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: has_stencil.then_some(wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        }),
                    }
                }),
                ..Default::default()
            });
            let mut cached_pipeline = None;
            let mut cached_uniforms = None;
            let mut cached_viewport = None;
            for d in &batch.draws {
                // Uniform blocks are addressed by dynamic offset into the one
                // draw buffer, so a single group per layout serves every draw.
                let kind = usize::from(d.stages);
                let group = uniform_groups[kind].as_ref().unwrap();
                let offsets = d.uniform_offsets;
                if cached_pipeline != Some(&d.pipeline) {
                    pass.set_pipeline(&d.pipeline);
                    cached_pipeline = Some(&d.pipeline);
                }
                // Binding order: transform, [stages], fog, alpha, [program], lighting.
                if cached_uniforms != Some((kind, offsets)) {
                    if d.stages {
                        pass.set_bind_group(0, group, &offsets);
                    } else {
                        pass.set_bind_group(
                            0,
                            group,
                            &[offsets[0], offsets[2], offsets[3], offsets[5]],
                        );
                    }
                    cached_uniforms = Some((kind, offsets));
                }
                if let Some(t) = &d.textures {
                    if texture_groups.len() >= MAX_TEXTURE_GROUPS {
                        // Groups already recorded in this pass stay alive in it.
                        texture_groups.clear();
                    }
                    let texture_group: &wgpu::BindGroup = match texture_groups.get(&t.pairs) {
                        Some(group) => group,
                        None => texture_groups.entry(t.pairs.clone()).or_insert_with(|| {
                            let entries: Vec<_> = t
                                .pairs
                                .iter()
                                .enumerate()
                                .flat_map(|(stage, (view, sampler))| {
                                    [
                                        wgpu::BindGroupEntry {
                                            binding: stage as u32 * 2,
                                            resource: wgpu::BindingResource::TextureView(view),
                                        },
                                        wgpu::BindGroupEntry {
                                            binding: stage as u32 * 2 + 1,
                                            resource: wgpu::BindingResource::Sampler(sampler),
                                        },
                                    ]
                                })
                                .collect();
                            gpu.create_bind_group(&wgpu::BindGroupDescriptor {
                                label: Some("D3D8 stage textures binding"),
                                layout: &self.draw_layouts.bgl_stage_textures,
                                entries: &entries,
                            })
                        }),
                    };
                    pass.set_bind_group(1, texture_group, &[]);
                }
                let vstart = d.vertex_base;
                pass.set_vertex_buffer(0, db.slice(vstart..vstart + d.vertex_len));
                let v = &d.viewport;
                let viewport_key = (
                    v.x,
                    v.y,
                    v.width,
                    v.height,
                    v.min_z.to_bits(),
                    v.max_z.to_bits(),
                );
                if cached_viewport != Some(viewport_key) {
                    pass.set_scissor_rect(v.x, v.y, v.width, v.height);
                    pass.set_viewport(
                        v.x as f32 + D3D8_HALF_PIXEL,
                        v.y as f32 + D3D8_HALF_PIXEL,
                        v.width as f32,
                        v.height as f32,
                        v.min_z,
                        v.max_z,
                    );
                    cached_viewport = Some(viewport_key);
                }
                if let Some((at, len)) = d.indices {
                    pass.set_index_buffer(db.slice(at..at + len), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..d.count, 0, 0..1);
                } else {
                    pass.draw(0..d.count, 0..1);
                }
            }
        }
        staging.finish();
        self.gpu.queue.submit([encoder.finish()]);
        staging.recall();
        self.submits.set(self.submits.get() + 1);
        batch.draws.clear();
        batch.data.clear();
        batch.last_uniforms = [None; 6];
        drop(batch);
        drop(slot);
        if self.publish_pending.replace(false) {
            self.publish_target();
        }
    }

    /// Close the managed per-frame validation error scope, failing loudly when
    /// any draw in the frame produced a wgpu validation error. The diagnostic
    /// names the frame and its draw count, since the report is deferred from
    /// the offending draw to the frame end. A no-op under `RECOMP_D3D8_STRICT_SCOPES`,
    /// where each draw popped its own scope.
    fn finish_frame_scope(&mut self) -> Result<(), RenderError> {
        let Some(scope) = self.frame_scope.take() else {
            return Ok(());
        };
        if let Some(err) = pollster::block_on(scope.pop()) {
            self.pipelines.clear();
            self.shader = None;
            self.textured_shader = None;
            self.lit_shader = None;
            self.lit_textured_shader = None;
            self.gpu_lit_shaders.clear();
            return Err(RenderError::new(
                "DrawPrimitive",
                format!(
                    "frame {} ({} draws): {err}",
                    self.frame_index, self.draw_index
                ),
            ));
        }
        Ok(())
    }

    /// `DrawIndexedPrimitive`: snapshot compact triangle lists as GPU-indexed
    /// draws, including declaration-derived programmable layouts. Lit lists
    /// pack distinct referenced vertices for GPU lighting or CPU fallback; unlit sparse
    /// ranges and other topologies retain expansion.
    /// Both paths validate actual index reads rather than the upload hints.
    pub fn draw_indexed_primitive(
        &mut self,
        topology: u32,
        fvf: u32,
        vertices: &[u8],
        indices: &[u8],
        draw: IndexedDraw,
    ) -> Result<(), RenderError> {
        if draw.primitive_count == 0 {
            return Ok(());
        }
        // Preserve expansion for strips/fans and diagnostic traces.
        // The switch also permits before/after profiling of the same scene.
        static EXPAND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let expand = *EXPAND.get_or_init(|| {
            [
                "RECOMP_D3D8_EXPAND_INDICES",
                "RECOMP_D3D8_TRACE_DRAWS",
                "RECOMP_D3D8_TRACE_DRAWN_TRIS",
                "RECOMP_D3D8_TRACE_SHADER_DRAWS",
            ]
            .iter()
            .any(|key| std::env::var_os(key).is_some())
        });
        let effective_fvf = match self.shaders.get(&self.vertex_shader) {
            Some((Some(decl), program)) if program.declaration_only => decl.fixed_fvf()?,
            Some((_, program)) if !program.declaration_only => 0,
            _ => fvf,
        };
        if !expand && draw.topology == 4 && topology == 4 {
            // Pack only referenced sources: GPU lighting consumes their raw
            // attributes, CPU fallback evaluates each source once. Sparse gaps
            // are never evaluated; queued snapshots own all required bytes.
            if matches!(effective_fvf, 0x0152 | 0x0112) {
                let mut packed = std::mem::take(&mut self.index_scratch);
                let mut remapped = std::mem::take(&mut self.index_rebased);
                let result = (|| {
                    packed_indexed_list_into(&mut packed, &mut remapped, vertices, indices, draw)?;
                    let view = VertexBuffer::borrowed(&packed, draw.stride)?;
                    self.draw_stream(4, fvf, &view, 0, draw.primitive_count, Some(&remapped))
                })();
                self.index_scratch = packed;
                self.index_rebased = remapped;
                return result;
            }
            let mut rebased = std::mem::take(&mut self.index_rebased);
            let result = (|| match indexed_list_into(&mut rebased, vertices, indices, draw)? {
                Some((begin, end)) => {
                    let view = VertexBuffer::borrowed(&vertices[begin..end], draw.stride)?;
                    self.draw_stream(4, fvf, &view, 0, draw.primitive_count, Some(&rebased))
                        .map(Some)
                }
                None => Ok(None),
            })();
            self.index_rebased = rebased;
            if result?.is_some() {
                return Ok(());
            }
        }
        let mut scratch = std::mem::take(&mut self.index_scratch);
        let result = (|| {
            expand_indexed_into(&mut scratch, vertices, indices, draw)?;
            let view = VertexBuffer::borrowed(&scratch, draw.stride)?;
            // Expansion always produces a triangle list, whatever the source
            // topology was, so the list draw path is the one to run.
            let _ = topology;
            self.draw_primitive(4, fvf, &view, 0, draw.primitive_count)
        })();
        self.index_scratch = scratch;
        result
    }

    /// `IDirect3DDevice8::Reset` for the single implicit swap chain.
    ///
    /// The new target is created first, so an invalid or oversized request
    /// leaves the device untouched. On success the device state is reset to
    /// D3D8 defaults, then the post-Reset adjustments Wine's `d3d8` applies
    /// (`d3d8_device_Reset`): `D3DRS_ZENABLE` follows `EnableAutoDepthStencil`
    /// and the viewport returns to the full target. Cached pipelines are
    /// dropped because a backbuffer format change alters the color target
    /// state they were built with. The scene/clear frame bookkeeping is
    /// cleared, matching a fresh device.
    ///
    /// TODO(decomp): D3D8 returns D3DERR_DEVICELOST while any referenced
    /// D3DPOOL_DEFAULT resource exists (Wine `reset_enum_callback`); this
    /// bridge does not track pool references yet and rebuilds unconditionally.
    pub fn reset(
        &mut self,
        width: u32,
        height: u32,
        format: u32,
        depth_format: u32,
    ) -> Result<(), RenderError> {
        self.flush_draws();
        if let Some(scope) = self.frame_scope.take() {
            // Drop the managed frame scope's pending diagnostics: the target
            // and its pipelines are rebuilt below.
            let _ = pollster::block_on(scope.pop());
        }
        let target = self
            .gpu
            .create_target(width, height, format, depth_format)?;
        self.targets = Targets::new(target.clone());
        self.target = target;
        self.state = DeviceState::new(width, height);
        let z_enable = u32::from(depth_format != 0);
        self.state.set_render_state(
            crate::d3d8::enums::D3DRENDERSTATETYPE::ZEnable as u32,
            z_enable,
        )?;
        self.pipelines.clear();
        self.vertex_shader = 0;
        self.pixel_shader = 0;
        self.shader_uniform = Default::default();
        self.stage_textures = std::array::from_fn(|_| None);
        self.texture_cache.clear();
        self.scratch_rgba.clear();
        self.scene = false;
        Ok(())
    }

    /// Tightly packed RGBA8, regardless of the D3D target format's channel order.
    pub fn read_pixels(&self) -> Result<Vec<u8>, RenderError> {
        self.flush_draws();
        self.gpu.read_pixels(&self.targets.backbuffer)
    }

    pub fn present_handoff(&self) -> Result<FrameHandoff, RenderError> {
        self.present_surface_handoff(32)
    }

    pub fn present_surface_handoff(&self, bpp: u32) -> Result<FrameHandoff, RenderError> {
        use std::sync::atomic::Ordering;
        if bpp != 16 && bpp != 32 {
            return Err(RenderError::invalid(
                "present_handoff",
                "unsupported surface bit depth",
            ));
        }
        self.flush_draws();
        let b = &self.targets.backbuffer;
        let mut ring = self.frame_ring.borrow_mut();
        if ring
            .as_ref()
            .is_some_and(|r| r.width != b.width || r.height != b.height)
        {
            // Resized: the old slots may still be read by the host.
            let old = ring.take().unwrap();
            for busy in &old.busy {
                self.wait_slot(busy)?;
            }
        }
        if ring.is_none() {
            *ring = Some(FrameRing {
                ready_event: create_ready_event(&self.gpu.device)?,
                ready_value: 0,
                width: b.width,
                height: b.height,
                textures: (0..FRAME_RING_SLOTS)
                    .map(|_| {
                        self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("d3d8-present-ring"),
                            size: wgpu::Extent3d {
                                width: b.width,
                                height: b.height,
                                depth_or_array_layers: 1,
                            },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: b.format,
                            usage: wgpu::TextureUsages::COPY_DST
                                | wgpu::TextureUsages::TEXTURE_BINDING
                                | wgpu::TextureUsages::STORAGE_BINDING,
                            view_formats: &[],
                        })
                    })
                    .collect(),
                busy: (0..FRAME_RING_SLOTS)
                    .map(|_| &*Box::leak(Box::new(std::sync::atomic::AtomicU32::new(0))))
                    .collect(),
                next: 0,
            });
        }
        let ring = ring.as_mut().expect("present ring is initialized");
        let slot = ring.next;
        let native = native_texture_ptr(&ring.textures[slot])?;
        self.wait_slot(ring.busy[slot])?;
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("d3d8-present-copy"),
            });
        if bpp == 16 {
            self.encode_rgb565_handoff(&mut encoder, &b.view, &ring.textures[slot]);
        } else {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &b.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &ring.textures[slot],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: b.width,
                    height: b.height,
                    depth_or_array_layers: 1,
                },
            );
        }
        let ready_value = ring.ready_value.checked_add(1).ok_or_else(|| {
            RenderError::new(
                "present_handoff",
                "native GPU synchronization value overflow",
            )
        })?;
        self.gpu.queue.submit([encoder.finish()]);
        let ready_event = signal_ready(&self.gpu.queue, &ring.ready_event, ready_value)?;
        ring.ready_value = ready_value;
        self.gpu
            .device
            .poll(wgpu::PollType::Poll)
            .map_err(|e| RenderError::new("present_handoff::poll", e.to_string()))?;
        ring.busy[slot].store(1, Ordering::Release);
        ring.next = (slot + 1) % FRAME_RING_SLOTS;
        Ok(FrameHandoff {
            ready_event,
            ready_value,
            texture: native,
            busy: ring.busy[slot],
            width: b.width,
            height: b.height,
        })
    }

    fn encode_rgb565_handoff(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
        destination: &wgpu::Texture,
    ) {
        let mut pipeline = self.rgb565_handoff.borrow_mut();
        let pipeline = pipeline.get_or_insert_with(|| {
            let shader = self
                .gpu
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("d3d8-rgb565-handoff"),
                    source: wgpu::ShaderSource::Wgsl(
                        r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;
@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(dst);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let p = vec2<i32>(id.xy);
    let bytes = vec3<u32>(textureLoad(src, p, 0).rgb * 255.0 + vec3<f32>(0.5));
    let reduced = bytes >> vec3<u32>(3, 2, 3);
    let expanded = reduced * vec3<u32>(255) / vec3<u32>(31, 63, 31);
    textureStore(dst, p, vec4<f32>(vec3<f32>(expanded) / 255.0, 1.0));
}
"#
                        .into(),
                    ),
                });
            self.gpu
                .device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("d3d8-rgb565-handoff"),
                    layout: None,
                    module: &shader,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    cache: None,
                })
        });
        let view = destination.create_view(&wgpu::TextureViewDescriptor::default());
        let group = self
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("d3d8-rgb565-handoff"),
                layout: &pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(source),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                ],
            });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("d3d8-rgb565-handoff"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(
            destination.width().div_ceil(8),
            destination.height().div_ceil(8),
            1,
        );
    }

    fn wait_slot(&self, busy: &std::sync::atomic::AtomicU32) -> Result<(), RenderError> {
        let start = std::time::Instant::now();
        while busy.load(std::sync::atomic::Ordering::Acquire) != 0 {
            if start.elapsed() > std::time::Duration::from_secs(2) {
                return Err(RenderError::new(
                    "present_handoff",
                    "host never released a presented frame slot",
                ));
            }
            std::thread::yield_now();
        }
        Ok(())
    }

    /// Required tightly packed backbuffer readback size, independent of viewport.
    pub fn read_pixels_len(&self) -> u64 {
        u64::from(self.targets.backbuffer.width) * u64::from(self.targets.backbuffer.height) * 4
    }

    /// Read into caller storage without an intermediate full-frame allocation.
    pub fn read_pixels_into(&self, out: &mut [u8]) -> Result<(), RenderError> {
        self.flush_draws();
        self.gpu.read_pixels_into(&self.targets.backbuffer, out)
    }

    pub fn write_pixels(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        let backbuffer = &self.targets.backbuffer;
        if width == 0
            || height == 0
            || u64::from(x) + u64::from(width) > u64::from(backbuffer.width)
            || u64::from(y) + u64::from(height) > u64::from(backbuffer.height)
            || rgba.len() as u64 != u64::from(width) * u64::from(height) * 4
        {
            return Err(RenderError::invalid(
                "write_pixels",
                "invalid backbuffer region or byte count",
            ));
        }
        self.flush_draws();
        for target in std::iter::once(backbuffer).chain(self.targets.backbuffer_padded.iter()) {
            self.gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &target.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        Ok(())
    }

    /// True when this device records unsupported draw rejections
    /// (`RECOMP_D3D8_SURVEY=1`).
    pub fn survey_enabled(&self) -> bool {
        self.survey
    }

    /// Record one state/FVF/TSS draw rejection in survey mode. Returns true
    /// when the caller must skip the draw and return success so the guest
    /// continues; false means the error is a hard failure and must propagate.
    ///
    /// The key combines the named error with a compact dump of the draw state
    /// (topology, FVF/vertex-shader handle, stride, fog/lighting/blend and the
    /// first two texture stages).
    pub fn note_draw_rejection(
        &self,
        error: &RenderError,
        topology: u32,
        fvf: u32,
        stride: u32,
    ) -> bool {
        if !self.survey {
            return false;
        }
        let state = format!(
            "topology={topology} fvf=0x{fvf:08X} stride={stride} {}",
            self.state.draw_state_summary()
        );
        survey::record(error, &state, self.frame_index, self.draw_index);
        true
    }

    /// Survey-mode counterpart for a rejection raised before
    /// [`Device::draw_primitive`] (the indexed expansion path). Counts the
    /// draw attempt, then records like [`Device::note_draw_rejection`].
    pub fn note_indexed_rejection(
        &mut self,
        error: &RenderError,
        topology: u32,
        fvf: u32,
        stride: u32,
    ) -> bool {
        self.draw_index += 1;
        self.note_draw_rejection(error, topology, fvf, stride)
    }

    /// The guest's world/UI boundary: everything drawn so far is the 3D scene,
    /// everything drawn next is overlay. `mode` selects an optional host
    /// post-process of the scene ([`SCENE_POST_NONE`] does nothing). Draws are
    /// flushed first, so the filter sees exactly the scene. Only the implicit
    /// backbuffer is filtered; an unknown mode, or a render-target texture
    /// bound at the boundary, is a named error rather than a silent skip.
    ///
    /// `RECOMP_D3D8_TRACE_DRAWS` prints a marker with the running draw count so
    /// a trace shows which draws precede the boundary.
    pub fn scene_boundary(&mut self, mode: u32) -> Result<(), RenderError> {
        if std::env::var_os("RECOMP_D3D8_TRACE_DRAWS").is_some() {
            eprintln!(
                "[d3d8-trace] scene-boundary frame={} draws={} mode={mode}",
                self.frame_index, self.draw_index
            );
        }
        match mode {
            SCENE_POST_NONE => Ok(()),
            SCENE_POST_FXAA => {
                if self.targets.current.is_some() {
                    return Err(RenderError::new(
                        "SceneBoundary",
                        "FXAA requested while a render-target texture is bound",
                    ));
                }
                // A partial viewport at the boundary is a sub-view (for
                // example a preview) drawn over an overlay that already
                // exists, so filtering the whole target would hit the overlay.
                let v = &self.state.viewport;
                let b = &self.targets.backbuffer;
                if (v.x, v.y, v.width, v.height) != (0, 0, b.width, b.height) {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "[d3d8] scene boundary: FXAA skipped for partial viewport {}x{}+{}+{}",
                            v.width, v.height, v.x, v.y
                        );
                    }
                    return Ok(());
                }
                self.flush_draws();
                self.gpu.fxaa_in_place(&self.targets.backbuffer)
            }
            other => Err(RenderError::new(
                "SceneBoundary",
                format!("unknown scene post-process mode {other}"),
            )),
        }
    }

    pub fn present(&mut self) -> Result<(), RenderError> {
        if self.scene {
            return Err(RenderError::new(
                "Present",
                "requires an ended frame (EndScene)",
            ));
        }
        self.flush_draws();
        self.gpu.present(&self.targets.backbuffer)?;
        self.frame_index += 1;
        if self.draw_stats_enabled {
            let mut stats = std::mem::take(&mut self.frame_stats);
            stats.submits = self.submits.take();
            stats.buffers_created += self.buffers_created_at_flush.take();
            eprintln!(
                "[d3d8-draw] frame={} draws={} buffers_created={} uniform_writes={} sampler_hits={} sampler_misses={} submits={}",
                self.frame_index,
                stats.draws,
                stats.buffers_created,
                stats.uniform_writes,
                self.samplers.hits,
                self.samplers.misses,
                stats.submits,
            );
        }
        if self.survey && self.frame_index % SURVEY_REPORT_INTERVAL == 0 {
            survey::report_to_stderr();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::blend_factor;
    use crate::d3d8::enums::D3DBLEND;

    #[test]
    fn dotproduct3_signed_rgb_clamps_and_routes_alpha_on_gpu() {
        use super::Device;
        use crate::{backend::GpuContext, d3d8::resource::VertexBuffer};
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("GPU adapter");
        let mut device = Device::new(gpu, 8, 8, 21, 0).unwrap();
        device.state.set_render_state(7, 0).unwrap();
        device.state.set_render_state(137, 0).unwrap();
        device.state.set_render_state(22, 1).unwrap();
        device.state.set_texture_stage_state(0, 2, 0).unwrap(); // COLORARG1 DIFFUSE
        device.state.set_texture_stage_state(0, 3, 3).unwrap(); // COLORARG2 TFACTOR
        device.state.set_texture_stage_state(0, 5, 0).unwrap(); // ALPHAARG1 DIFFUSE
        device.state.set_texture_stage_state(0, 6, 3).unwrap(); // ALPHAARG2 TFACTOR
        // Expected values derive from 4*sum((a.rgb-.5)*(b.rgb-.5)).
        // Opposing components clamp to zero, aligned components saturate,
        // and asymmetric RGB proves this is a dot rather than a scalar multiply.
        for (diffuse, factor, expected) in [
            (0x00ffffffu32, 0xff000000u32, 0u8),
            (0x00000000, 0xff000000, 255),
            (0x00ffffff, 0xffffffff, 255),
            (0x00ff8080, 0xffbf8080, 127),
            (0x00ff00ff, 0xffbfbfbf, 127),
        ] {
            device.state.set_render_state(60, factor).unwrap();
            let mut bytes = Vec::new();
            for pos in [[-1.0f32, -1.0, 0.5], [3.0, -1.0, 0.5], [-1.0, 3.0, 0.5]] {
                for f in pos {
                    bytes.extend(f.to_le_bytes());
                }
                bytes.extend(diffuse.to_le_bytes());
                bytes.extend([0; 8]);
            }
            let vb = VertexBuffer::borrowed(&bytes, 24).unwrap();
            for alpha_only in [false, true] {
                device
                    .state
                    .set_texture_stage_state(0, 1, if alpha_only { 2 } else { 24 })
                    .unwrap();
                device
                    .state
                    .set_texture_stage_state(0, 4, if alpha_only { 24 } else { 2 })
                    .unwrap();
                device.clear(&[], 1, 0, 1.0, 0).unwrap();
                device.begin_scene().unwrap();
                device.draw_primitive(4, 0x142, &vb, 0, 1).unwrap();
                device.end_scene().unwrap();
                let pixels = device.read_pixels().unwrap();
                let pixel = &pixels[(4 * 8 + 4) * 4..(4 * 8 + 4) * 4 + 4];
                assert!(
                    (i16::from(pixel[3]) - i16::from(expected)).abs() <= 1,
                    "alpha: {diffuse:#x} {factor:#x}: {pixel:?}"
                );
                if !alpha_only {
                    for c in &pixel[..3] {
                        assert!((i16::from(*c) - i16::from(expected)).abs() <= 1);
                    }
                } else {
                    assert_eq!(
                        &pixel[..3],
                        &[(diffuse >> 16) as u8, (diffuse >> 8) as u8, diffuse as u8]
                    );
                }
            }
        }
    }

    #[test]
    fn a8_texture_upload_and_sampling_preserve_alpha_on_gpu() {
        use super::{Device, TextureLevelUpload};
        use crate::{backend::GpuContext, d3d8::resource::VertexBuffer};
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("GPU adapter");
        let mut device = Device::new(gpu, 8, 8, 21, 0).unwrap();
        device.state.set_render_state(7, 0).unwrap();
        device.state.set_render_state(137, 0).unwrap();
        device.state.set_render_state(22, 1).unwrap();
        device.state.set_texture_stage_state(0, 1, 2).unwrap(); // SELECTARG1 TEXTURE
        let mut bytes = Vec::new();
        for pos in [[-1.0f32, -1.0, 0.5], [3.0, -1.0, 0.5], [-1.0, 3.0, 0.5]] {
            for f in pos {
                bytes.extend(f.to_le_bytes());
            }
            bytes.extend(u32::MAX.to_le_bytes());
            bytes.extend([0; 8]);
        }
        let vb = VertexBuffer::borrowed(&bytes, 24).unwrap();
        for (generation, a) in [0u8, 1, 64, 128, 254, 255].into_iter().enumerate() {
            device
                .set_texture(
                    0,
                    999,
                    28,
                    &[TextureLevelUpload {
                        level: 0,
                        generation: generation as u64,
                        force_upload: false,
                        width: 1,
                        height: 1,
                        data: &[a],
                    }],
                )
                .unwrap();
            device.begin_scene().unwrap();
            device.draw_primitive(4, 0x142, &vb, 0, 1).unwrap();
            device.end_scene().unwrap();
            let pixels = device.read_pixels().unwrap();
            assert_eq!(&pixels[(4 * 8 + 4) * 4..(4 * 8 + 4) * 4 + 4], &[0, 0, 0, a]);
        }
    }

    #[test]
    fn unchanged_render_texture_bind_keeps_batch_and_rewrite_orders_old_draws() {
        use super::{Device, TextureLevelUpload};
        use crate::{backend::GpuContext, d3d8::resource::VertexBuffer};
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("Metal adapter");
        let mut device = Device::new(gpu, 64, 32, 21, 0).unwrap();
        device.state.set_render_state(7, 0).unwrap();
        device.state.set_render_state(137, 0).unwrap();
        device.state.set_render_state(22, 1).unwrap();
        // A8R8G8B8 CPU bytes are BGRA; texture begins red.
        device
            .set_render_target(901, 0, 1, 21, 1, 1, &[0, 0, 255, 255], false, true)
            .unwrap();
        device
            .set_render_target(0, 0, 0, 21, 0, 0, &[], false, true)
            .unwrap();
        let upload = |generation, force_upload, data| TextureLevelUpload {
            level: 0,
            generation,
            force_upload,
            width: 1,
            height: 1,
            data,
        };
        device
            .set_texture(0, 901, 21, &[upload(1, false, &[0, 0, 255, 255])])
            .unwrap();
        let mut bytes = Vec::new();
        for pos in [[-1.0f32, -1.0, 0.5], [3.0, -1.0, 0.5], [-1.0, 3.0, 0.5]] {
            for f in pos {
                bytes.extend(f.to_le_bytes());
            }
            bytes.extend(u32::MAX.to_le_bytes());
            bytes.extend([0; 8]);
        }
        let vb = VertexBuffer::borrowed(&bytes, 24).unwrap();
        device.begin_scene().unwrap();
        device.state.viewport.width = 32;
        device.draw_primitive(4, 0x142, &vb, 0, 1).unwrap();
        device
            .set_texture(0, 901, 21, &[upload(1, false, &[0, 0, 255, 255])])
            .unwrap();
        assert_eq!(device.batch.borrow().draws.len(), 1);
        assert!(
            device
                .set_texture(0, 901, 21, &[upload(1, true, &[0, 0, 255, 255])])
                .is_err()
        );
        assert_eq!(device.batch.borrow().draws.len(), 1);
        // A new CPU generation must flush the red draw before uploading green.
        device
            .set_texture(0, 901, 21, &[upload(2, false, &[0, 255, 0, 255])])
            .unwrap();
        assert!(device.batch.borrow().draws.is_empty());
        device.state.viewport.x = 32;
        device.draw_primitive(4, 0x142, &vb, 0, 1).unwrap();
        device.end_scene().unwrap();
        let pixels = device.read_pixels().unwrap();
        assert_eq!(
            &pixels[(16 * 64 + 16) * 4..(16 * 64 + 16) * 4 + 4],
            &[255, 0, 0, 255]
        );
        assert_eq!(
            &pixels[(16 * 64 + 48) * 4..(16 * 64 + 48) * 4 + 4],
            &[0, 255, 0, 255]
        );
    }

    #[test]
    fn native_indexed_draw_matches_expansion_and_snapshots_bytes() {
        use super::*;
        use crate::backend::GpuContext;
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("Metal adapter");
        let mut device = Device::new(gpu, 64, 32, 21, 0).unwrap();
        device.state.set_render_state(7, 0).unwrap();
        device.state.set_render_state(137, 0).unwrap();
        device.state.set_render_state(22, 1).unwrap();
        let mut vertices = vec![0; 4 * 16];
        for pos in [[-1.0f32, -1.0, 0.5], [3.0, -1.0, 0.5], [-1.0, 3.0, 0.5]] {
            for f in pos {
                vertices.extend(f.to_le_bytes());
            }
            vertices.extend(0xff00ff00u32.to_le_bytes());
        }
        for format in [101, 102] {
            device.clear(&[], 1, 0, 1.0, 0).unwrap();
            let mut indices: Vec<u8> = [99u32, 2, 0, 1, 2, 0, 1]
                .into_iter()
                .flat_map(|i| {
                    if format == 101 {
                        (i as u16).to_le_bytes().to_vec()
                    } else {
                        i.to_le_bytes().to_vec()
                    }
                })
                .collect();
            let draw = IndexedDraw {
                topology: 4,
                index_format: format,
                stride: 16,
                base_vertex: 4,
                min_index: u32::MAX,
                num_vertices: 0,
                start_index: 1,
                primitive_count: 2,
            };
            device.begin_scene().unwrap();
            device.state.viewport.x = 0;
            device.state.viewport.width = 32;
            device
                .draw_indexed_primitive(4, 0x42, &vertices, &indices, draw)
                .unwrap();
            {
                let batch = device.batch.borrow();
                let queued = batch.draws.last().unwrap();
                assert!(queued.indices.is_some());
                assert_eq!(queued.vertex_len, 48);
                assert_eq!(queued.count, 6);
            }
            let expanded = expand_indexed_into;
            let mut bytes = Vec::new();
            expanded(&mut bytes, &vertices, &indices, draw).unwrap();
            indices.fill(0); // queued draw must own its index snapshot
            device.state.viewport.x = 32;
            device
                .draw_primitive(4, 0x42, &VertexBuffer::borrowed(&bytes, 16).unwrap(), 0, 2)
                .unwrap();
            device.end_scene().unwrap();
            let pixels = device.read_pixels().unwrap();
            for x in [16, 48] {
                assert_eq!(
                    &pixels[(16 * 64 + x) * 4..(16 * 64 + x) * 4 + 4],
                    &[0, 255, 0, 255]
                );
            }
        }
    }

    #[test]
    fn indexed_shader_and_sparse_lighting_match_expansion() {
        use super::*;
        use crate::{backend::GpuContext, d3d8::state::Light};
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("Metal adapter");
        let mut device = Device::new(gpu, 64, 32, 21, 0).unwrap();
        device.cpu_lighting = true;
        device.state.set_render_state(7, 0).unwrap();
        device.state.set_render_state(22, 1).unwrap();
        // Input registers differ from fixed-function attribute locations.
        let decl = Declaration::parse(&[0x20000000, 0x40030002, 0x40030005, u32::MAX]).unwrap();
        let program = Program::parse(
            &[
                0xfffe0101, 1, 0xc00f0000, 0x90e40002, 5, 0xd00f0000, 0x90e40005, 0xa0e40000,
                0xffff,
            ],
            false,
        )
        .unwrap(); // mov oPos,v2; mul oD0,v5,c0
        device.shaders.insert(0x10000, (Some(decl), program));
        device
            .state
            .set_light(
                0,
                Light {
                    light_type: 3,
                    direction: [0.0, 0.0, -1.0],
                    diffuse: [1.0; 4],
                    ..Light::default()
                },
            )
            .unwrap();
        device.state.light_enable(0, true).unwrap();
        for (fvf, stride, sparse) in [
            (0x10000, 32, false),
            (0x10000, 48, false),
            (0x112, 32, true),
            (0x152, 36, true),
        ] {
            device.vertex_shader = if fvf == 0x10000 { fvf } else { 0 };
            device
                .state
                .set_render_state(137, u32::from(sparse))
                .unwrap();
            for format in [101, 102] {
                let slots = if sparse { [2usize, 17, 41] } else { [2, 3, 4] };
                // Every unused vertex has a NaN normal/position. Sparse gaps
                // must never be evaluated or included in the lighting output.
                let mut vertices = vec![0; (slots[2] + 5) * stride];
                for chunk in vertices.chunks_exact_mut(4) {
                    chunk.copy_from_slice(&f32::NAN.to_le_bytes());
                }
                for (n, pos) in [[-0.8f32, -0.8, 0.5], [0.8, -0.8, 0.5], [0.0, 0.8, 0.5]]
                    .into_iter()
                    .enumerate()
                {
                    let at = (slots[n] + 3) * stride;
                    let dst = &mut vertices[at..at + stride];
                    dst.fill(0);
                    dst[..12].copy_from_slice(bytemuck::cast_slice(&pos));
                    if sparse {
                        dst[20..24].copy_from_slice(&1.0f32.to_le_bytes());
                        if fvf == 0x152 {
                            dst[24..28].copy_from_slice(
                                &[0xff0000ffu32, 0xff00ff00, 0xffff0000][n].to_le_bytes(),
                            );
                        }
                    } else {
                        dst[12..16].copy_from_slice(&1.0f32.to_le_bytes());
                        let color = [
                            [1.0f32, 0.2, 0.1, 1.0],
                            [0.1, 1.0, 0.2, 1.0],
                            [0.2, 0.1, 1.0, 1.0],
                        ][n];
                        dst[16..32].copy_from_slice(bytemuck::cast_slice(&color));
                    }
                }
                let raw = [
                    999u32,
                    slots[2] as u32,
                    slots[0] as u32,
                    slots[1] as u32,
                    slots[2] as u32,
                    slots[0] as u32,
                    slots[1] as u32,
                ];
                let mut indices: Vec<u8> = raw
                    .into_iter()
                    .flat_map(|i| {
                        if format == 101 {
                            (i as u16).to_le_bytes().to_vec()
                        } else {
                            i.to_le_bytes().to_vec()
                        }
                    })
                    .collect();
                let draw = IndexedDraw {
                    topology: 4,
                    index_format: format,
                    stride: stride as u32,
                    base_vertex: 3,
                    min_index: u32::MAX,
                    num_vertices: 0,
                    start_index: 1,
                    primitive_count: 2,
                };
                let mut expanded = Vec::new();
                expand_indexed_into(&mut expanded, &vertices, &indices, draw).unwrap();
                device.shader_uniform.vc[0] = [0.5, 0.75, 1.0, 1.0];
                device.clear(&[], 1, 0xff000000, 1.0, 0).unwrap();
                device.begin_scene().unwrap();
                device.state.viewport.x = 0;
                device.state.viewport.width = 32;
                device
                    .draw_indexed_primitive(4, fvf, &vertices, &indices, draw)
                    .unwrap();
                {
                    let batch = device.batch.borrow();
                    let queued = batch.draws.last().unwrap();
                    assert!(queued.indices.is_some());
                    assert_eq!(
                        queued.vertex_len,
                        3 * if sparse { 36 } else { stride } as u64
                    );
                    assert_eq!(queued.count, 6);
                }
                // Neither mutable guest storage nor subsequent constants may
                // change the previously queued indexed draw.
                vertices.fill(0);
                indices.fill(0);
                device.shader_uniform.vc[0] = [0.0; 4];
                device.shader_uniform.vc[0] = [0.5, 0.75, 1.0, 1.0];
                device.state.viewport.x = 32;
                device
                    .draw_primitive(
                        4,
                        fvf,
                        &VertexBuffer::borrowed(&expanded, stride as u32).unwrap(),
                        0,
                        2,
                    )
                    .unwrap();
                device.shader_uniform.vc[0] = [0.0; 4];
                device.end_scene().unwrap();
                let pixels = device.read_pixels().unwrap();
                let mut colored = 0;
                for y in 0..32 {
                    for x in 0..32 {
                        let left = &pixels[(y * 64 + x) * 4..(y * 64 + x + 1) * 4];
                        let right = &pixels[(y * 64 + x + 32) * 4..(y * 64 + x + 33) * 4];
                        assert_eq!(
                            left, right,
                            "fvf={fvf:#x} stride={stride} format={format} pixel={x},{y}"
                        );
                        colored += usize::from(left[..3] != [0, 0, 0]);
                    }
                }
                assert!(
                    colored > 100,
                    "test must render visible pixels: fvf={fvf:#x} stride={stride} format={format}"
                );
            }
        }
    }

    #[test]
    fn gpu_lighting_matches_cpu_for_draw_types_and_snapshots() {
        use super::*;
        use crate::{
            backend::GpuContext,
            d3d8::{
                math::Mat4,
                state::{Light, Material},
            },
        };
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("Metal adapter");
        let mut device = Device::new(gpu, 64, 32, 21, 0).unwrap();
        device.state.set_render_state(7, 0).unwrap();
        device.state.set_render_state(22, 1).unwrap();
        let light = |kind| Light {
            light_type: kind,
            position: [0.3, 0.1, 2.0],
            direction: [0.0, 0.0, -1.0],
            diffuse: [0.6, 0.4, 0.2, 1.0],
            ambient: [0.05, 0.07, 0.09, 1.0],
            range: 20.0,
            attenuation0: 1.0,
            attenuation1: 0.1,
            attenuation2: 0.03,
            theta: 0.3,
            phi: 1.5,
            falloff: 2.3,
            ..Light::default()
        };
        device.state.world = Mat4::scale(0.8, 0.7, 1.3).mul(Mat4::translation(0.05, -0.05, 0.0));
        device.state.view = Mat4::translation(0.03, 0.02, 0.0);
        device.shaders.insert(
            0x10001,
            (
                None,
                Program::parse(&[0xffff0101, 1, 0x800f0000, 0x90e40000, 0xffff], true).unwrap(),
            ),
        ); // mov r0,v0: fixed vertices + pixel shader
        for fvf in [0x112, 0x152] {
            let stride = if fvf == 0x112 { 32usize } else { 36 };
            let mut source = vec![0; 50 * stride];
            for chunk in source.chunks_exact_mut(4) {
                chunk.copy_from_slice(&f32::NAN.to_le_bytes());
            }
            let verts: Vec<Vec<u8>> = [[-0.8f32, -0.8, 0.4], [0.8, -0.8, 0.4], [0.0, 0.8, 0.4]]
                .into_iter()
                .enumerate()
                .map(|(n, pos)| {
                    let mut v = Vec::new();
                    for f in pos
                        .into_iter()
                        .chain([[0.0, 0.0, 0.0], [0.3, 0.0, 0.7], [0.0, 0.2, 1.8]][n])
                    {
                        v.extend(f.to_le_bytes());
                    }
                    if fvf == 0x152 {
                        v.extend([0x80402080u32, 0xc0804020, 0xff208040][n].to_le_bytes());
                    }
                    v.extend([0.2f32, 0.7].into_iter().flat_map(f32::to_le_bytes));
                    v
                })
                .collect();
            for (n, v) in verts.iter().enumerate() {
                source[(7 + n) * stride..(8 + n) * stride].copy_from_slice(v);
                source[[5, 22, 43][n] * stride..([5, 22, 43][n] + 1) * stride].copy_from_slice(v);
            }
            for case in 0..10 {
                for slot in 0..8 {
                    device.state.light_enable(slot, false).unwrap();
                }
                let kinds: &[u32] = match case {
                    0 => &[3],
                    1 => &[1],
                    2 => &[2],
                    _ => &[3, 1, 2],
                };
                for (slot, kind) in kinds.iter().enumerate() {
                    device.state.set_light(slot as u32, light(*kind)).unwrap();
                    device.state.light_enable(slot as u32, true).unwrap();
                }
                device
                    .state
                    .set_render_state(137, u32::from(case != 4))
                    .unwrap();
                device
                    .state
                    .set_render_state(143, u32::from(case % 2 == 1))
                    .unwrap(); // NORMALIZENORMALS
                device
                    .state
                    .set_render_state(141, u32::from(case != 5))
                    .unwrap(); // COLORVERTEX
                for rs in [145, 146, 147] {
                    // diffuse/specular/ambient sources
                    device
                        .state
                        .set_render_state(rs, if case == 6 { 0 } else { 1 })
                        .unwrap();
                }
                device
                    .state
                    .set_render_state(148, if case == 6 { 1 } else { 0 })
                    .unwrap(); // emissive source
                device.state.set_render_state(139, 0xff304050).unwrap(); // AMBIENT
                let material = Material {
                    diffuse: [0.4, 0.6, 0.8, 0.7],
                    ambient: [0.2, 0.1, 0.3, 1.0],
                    emissive: [0.03, 0.01, 0.04, 1.0],
                    ..Material::d3d_default()
                };
                device.state.set_material(material);
                device.pixel_shader = if case == 7 { 0x10001 } else { 0 };
                if case == 8 {
                    device
                        .set_texture(
                            0,
                            987,
                            21,
                            &[TextureLevelUpload {
                                level: 0,
                                generation: 1,
                                force_upload: false,
                                width: 1,
                                height: 1,
                                data: &[96, 160, 224, 192],
                            }],
                        )
                        .unwrap();
                } else {
                    device.set_texture(0, 0, 0, &[]).unwrap();
                }
                device
                    .state
                    .set_render_state(28, u32::from(case == 9))
                    .unwrap(); // FOGENABLE
                device.state.set_render_state(35, 3).unwrap(); // linear table fog
                device.state.set_render_state(36, 0.0f32.to_bits()).unwrap();
                device.state.set_render_state(37, 2.0f32.to_bits()).unwrap();
                device.state.set_render_state(34, 0xff204080).unwrap();
                device
                    .state
                    .set_render_state(15, u32::from(case == 9))
                    .unwrap(); // ALPHATESTENABLE
                device.state.set_render_state(24, 32).unwrap(); // ALPHAREF
                device.state.set_render_state(25, 7).unwrap(); // GREATEREQUAL

                for format in [0, 101, 102] {
                    let mut bytes = source.clone();
                    let mut indices: Vec<u8> = [999u32, 40, 2, 19, 40, 2, 19]
                        .into_iter()
                        .flat_map(|i| {
                            if format == 101 {
                                (i as u16).to_le_bytes().to_vec()
                            } else {
                                i.to_le_bytes().to_vec()
                            }
                        })
                        .collect();
                    device.clear(&[], 1, 0xff000000, 1.0, 0).unwrap();
                    device.begin_scene().unwrap();
                    device.state.viewport.width = 32;
                    device.state.viewport.x = 0;
                    device.cpu_lighting = false;
                    if format == 0 {
                        device
                            .draw_primitive(
                                4,
                                fvf,
                                &VertexBuffer::borrowed(&bytes, stride as u32).unwrap(),
                                7,
                                1,
                            )
                            .unwrap();
                    } else {
                        device
                            .draw_indexed_primitive(
                                4,
                                fvf,
                                &bytes,
                                &indices,
                                IndexedDraw {
                                    topology: 4,
                                    index_format: format,
                                    stride: stride as u32,
                                    base_vertex: 3,
                                    min_index: u32::MAX,
                                    num_vertices: 0,
                                    start_index: 1,
                                    primitive_count: 2,
                                },
                            )
                            .unwrap();
                    }
                    {
                        let batch = device.batch.borrow();
                        assert_eq!(batch.draws.last().unwrap().vertex_len, (3 * stride) as u64);
                        assert!(
                            device
                                .pipelines
                                .keys()
                                .any(|k| k.gpu_lighting && k.fvf == fvf)
                        );
                    }
                    bytes.fill(0);
                    indices.fill(0); // immutable queued inputs
                    device.state.set_material(Material {
                        diffuse: [0.0; 4],
                        ..material
                    });
                    device.state.set_material(material);
                    device.state.viewport.x = 32;
                    device.cpu_lighting = true;
                    let cpu_vertices = if format == 0 {
                        verts.concat()
                    } else {
                        [
                            verts[2].clone(),
                            verts[0].clone(),
                            verts[1].clone(),
                            verts[2].clone(),
                            verts[0].clone(),
                            verts[1].clone(),
                        ]
                        .concat()
                    };
                    device
                        .draw_primitive(
                            4,
                            fvf,
                            &VertexBuffer::borrowed(&cpu_vertices, stride as u32).unwrap(),
                            0,
                            if format == 0 { 1 } else { 2 },
                        )
                        .unwrap();
                    // Change all lights/material after queuing both draws.
                    device.state.set_material(Material {
                        diffuse: [0.0; 4],
                        ..material
                    });
                    for slot in 0..8 {
                        device.state.light_enable(slot, false).unwrap();
                    }
                    device.end_scene().unwrap();
                    let pixels = device.read_pixels().unwrap();
                    let mut visible = 0;
                    for y in 0..32 {
                        for x in 0..32 {
                            let left = &pixels[(y * 64 + x) * 4..(y * 64 + x + 1) * 4];
                            let right = &pixels[(y * 64 + x + 32) * 4..(y * 64 + x + 33) * 4];
                            for c in 0..4 {
                                assert!(
                                    left[c].abs_diff(right[c]) <= 2,
                                    "fvf={fvf:#x} case={case} format={format} pixel={x},{y}: GPU={left:?} CPU={right:?}"
                                );
                            }
                            visible += usize::from(left[..3] != [0, 0, 0]);
                        }
                    }
                    assert!(visible > 100);
                    device.state.set_material(material);
                    for (slot, kind) in kinds.iter().enumerate() {
                        device.state.set_light(slot as u32, light(*kind)).unwrap();
                        device.state.light_enable(slot as u32, true).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn queued_draws_exclude_unused_vertex_prefixes() {
        use super::Device;
        use crate::{backend::GpuContext, d3d8::resource::VertexBuffer};
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("Metal adapter");
        let mut device = Device::new(gpu, 16, 16, 21, 0).unwrap();
        device.state.set_render_state(7, 0).unwrap();
        device.state.set_render_state(22, 1).unwrap();
        device.begin_scene().unwrap();
        // Large offsets must neither trigger the 8 MiB flush nor contribute
        // unused bytes to the queued GPU upload. Exercise lit and unlit paths.
        for (fvf, stride) in [(0x42, 16), (0x152, 36)] {
            device
                .state
                .set_render_state(137, u32::from(fvf == 0x152))
                .unwrap();
            let mut source = vec![0; (65536 + 3) * stride];
            for slot in 65536..65539 {
                source[slot * stride + 8..slot * stride + 12]
                    .copy_from_slice(&0.5f32.to_le_bytes());
                if fvf == 0x152 {
                    source[slot * stride + 20..slot * stride + 24]
                        .copy_from_slice(&1.0f32.to_le_bytes());
                }
            }
            let vertices = VertexBuffer::borrowed(&source, stride as u32).unwrap();
            device.draw_primitive(4, fvf, &vertices, 65536, 1).unwrap();
            let batch = device.batch.borrow();
            let draw = batch.draws.last().unwrap();
            assert_eq!(draw.count, 3);
            assert_eq!(draw.vertex_len, 3 * stride as u64);
            assert!(batch.data.len() < 8192);
        }
        assert_eq!(device.batch.borrow().draws.len(), 2);
        device.end_scene().unwrap();
    }

    #[test]
    fn supported_topologies_consume_the_right_vertex_count() {
        use super::{topology_vertex_count, wgpu_topology};
        // POINTLIST: one vertex per point.
        assert_eq!(topology_vertex_count(1, 7).unwrap(), 7);
        assert_eq!(wgpu_topology(1), wgpu::PrimitiveTopology::PointList);
        // LINELIST: two vertices per independent line.
        assert_eq!(topology_vertex_count(2, 7).unwrap(), 14);
        assert_eq!(wgpu_topology(2), wgpu::PrimitiveTopology::LineList);
        assert!(topology_vertex_count(2, u32::MAX).is_err());
        // TRIANGLELIST: three vertices per triangle.
        assert_eq!(topology_vertex_count(4, 7).unwrap(), 21);
        assert_eq!(wgpu_topology(4), wgpu::PrimitiveTopology::TriangleList);
        // Overflow and the unimplemented strip/fan topologies fail by name.
        assert!(topology_vertex_count(4, u32::MAX).is_err());
        for topology in [0, 3, 5, 6, 7] {
            let err = topology_vertex_count(topology, 1).unwrap_err();
            assert!(
                err.cause
                    .contains("expected POINTLIST (1), LINELIST (2) or TRIANGLELIST (4)")
            );
        }
    }

    #[test]
    fn cull_modes_map_to_the_d3d8_winding() {
        use super::Device;
        use crate::d3d8::enums::D3DCULL;
        // D3DCULL_CCW (the D3D8 default) culls counter-clockwise faces, so
        // clockwise is front; D3DCULL_CW is the opposite; NONE culls nothing.
        assert_eq!(
            Device::cull_mode_to_wgpu(D3DCULL::None),
            (None, wgpu::FrontFace::Ccw)
        );
        assert_eq!(
            Device::cull_mode_to_wgpu(D3DCULL::Cw),
            (Some(wgpu::Face::Back), wgpu::FrontFace::Ccw)
        );
        assert_eq!(
            Device::cull_mode_to_wgpu(D3DCULL::Ccw),
            (Some(wgpu::Face::Back), wgpu::FrontFace::Cw)
        );
    }

    #[test]
    fn blend_factors_map_to_wgpu() {
        assert_eq!(
            blend_factor(D3DBLEND::Zero).unwrap(),
            wgpu::BlendFactor::Zero
        );
        assert_eq!(blend_factor(D3DBLEND::One).unwrap(), wgpu::BlendFactor::One);
        assert_eq!(
            blend_factor(D3DBLEND::SrcAlpha).unwrap(),
            wgpu::BlendFactor::SrcAlpha
        );
        assert_eq!(
            blend_factor(D3DBLEND::InvSrcAlpha).unwrap(),
            wgpu::BlendFactor::OneMinusSrcAlpha
        );
        assert_eq!(
            blend_factor(D3DBLEND::DestColor).unwrap(),
            wgpu::BlendFactor::Dst
        );
        assert_eq!(
            blend_factor(D3DBLEND::SrcAlphaSat).unwrap(),
            wgpu::BlendFactor::SrcAlphaSaturated
        );
    }

    #[test]
    fn both_alpha_blend_factors_fail_by_name() {
        let err = blend_factor(D3DBLEND::BothSrcAlpha).unwrap_err();
        assert!(err.cause.contains("BOTHSRCALPHA"), "{}", err.cause);
        let err = blend_factor(D3DBLEND::BothInvSrcAlpha).unwrap_err();
        assert!(err.cause.contains("BOTHINVSRCALPHA"), "{}", err.cause);
    }

    #[test]
    fn color_write_masks_map_to_the_wgpu_channel_bits() {
        use super::color_writes_from_mask;
        // D3D8 R/G/B/A and wgpu RED/GREEN/BLUE/ALPHA share the same bit order.
        assert_eq!(color_writes_from_mask(0x0), wgpu::ColorWrites::empty());
        assert_eq!(color_writes_from_mask(0xF), wgpu::ColorWrites::ALL);
        assert_eq!(color_writes_from_mask(0x1), wgpu::ColorWrites::RED);
        assert_eq!(color_writes_from_mask(0x8), wgpu::ColorWrites::ALPHA);
        // Bits above the 4-bit mask are not representable and are dropped; the
        // state setter refuses them before they reach the draw path.
        assert_eq!(color_writes_from_mask(0x30), wgpu::ColorWrites::empty());
    }

    #[test]
    fn sampler_keys_dedupe_by_sampling_state() {
        use super::{SamplerKey, SamplerKey as K};
        use crate::d3d8::state::TextureStage;
        let stage = TextureStage {
            active: true,
            color_op: 0,
            color_arg1: 0,
            color_arg2: 0,
            alpha_op: 0,
            alpha_arg1: 0,
            alpha_arg2: 0,
            texture_factor: 0,
            tex_coord_index: 0,
            tex_coord_gen: 0,
            tex_transform_flags: 0,
            tex_transform: crate::d3d8::math::Mat4::IDENTITY,
            min_filter: 2,
            mag_filter: 2,
            mip_filter: 1,
            max_mip_level: 0,
            lod_bias: 0.0,
            address_u: 1,
            address_v: 1,
        };
        let same = TextureStage {
            color_op: 7,
            texture_factor: 0x11223344,
            ..stage
        };
        let other = TextureStage {
            address_v: 3,
            ..stage
        };
        // Combine state that does not affect the sampler must not split the
        // cache; a sampling-mode difference must. The chain's written-level
        // count and MAXMIPLEVEL are sampler inputs too.
        assert_eq!(K::from_stage(&stage, 3), K::from_stage(&same, 3));
        assert_ne!(K::from_stage(&stage, 3), K::from_stage(&other, 3));
        assert_ne!(K::from_stage(&stage, 3), K::from_stage(&stage, 2));
        let shifted = TextureStage {
            max_mip_level: 1,
            ..stage
        };
        assert_ne!(K::from_stage(&stage, 3), K::from_stage(&shifted, 3));
        let _: fn(&TextureStage, u32) -> SamplerKey = K::from_stage;
    }

    fn stage_with_mips(mip_filter: u32, max_mip_level: u32) -> crate::d3d8::state::TextureStage {
        crate::d3d8::state::TextureStage {
            active: true,
            color_op: 4,
            color_arg1: 2,
            color_arg2: 1,
            alpha_op: 1,
            alpha_arg1: 2,
            alpha_arg2: 1,
            texture_factor: 0,
            tex_coord_index: 0,
            tex_coord_gen: 0,
            tex_transform_flags: 0,
            tex_transform: crate::d3d8::math::Mat4::IDENTITY,
            min_filter: 2,
            mag_filter: 2,
            mip_filter,
            max_mip_level,
            lod_bias: 0.0,
            address_u: 1,
            address_v: 1,
        }
    }

    #[test]
    fn mip_lod_range_maps_d3d_filters_and_clamps_to_written_levels() {
        use super::mip_lod_range;
        // MIPFILTER=NONE collapses to a single level even with more written.
        assert_eq!(
            mip_lod_range(&stage_with_mips(0, 0), 3),
            (0.0, 0.0, wgpu::FilterMode::Nearest)
        );
        // POINT spans the written chain with nearest mip selection.
        assert_eq!(
            mip_lod_range(&stage_with_mips(1, 0), 3),
            (0.0, 3.0, wgpu::FilterMode::Nearest)
        );
        // LINEAR is trilinear across the written chain.
        assert_eq!(
            mip_lod_range(&stage_with_mips(2, 0), 3),
            (0.0, 3.0, wgpu::FilterMode::Linear)
        );
        // MAXMIPLEVEL shifts the base; it is clamped to the written chain.
        assert_eq!(
            mip_lod_range(&stage_with_mips(2, 2), 3),
            (2.0, 3.0, wgpu::FilterMode::Linear)
        );
        assert_eq!(
            mip_lod_range(&stage_with_mips(2, 9), 3),
            (3.0, 3.0, wgpu::FilterMode::Linear)
        );
        // Only level 0 written: no filter may reach an unwritten level.
        assert_eq!(
            mip_lod_range(&stage_with_mips(1, 0), 0),
            (0.0, 0.0, wgpu::FilterMode::Nearest)
        );
    }

    #[test]
    fn contiguous_written_level_stops_at_the_first_gap() {
        use super::contiguous_written_level as c;
        assert_eq!(c(&[]), 0);
        assert_eq!(c(&[false, true, true]), 0);
        assert_eq!(c(&[true, false, true]), 0);
        assert_eq!(c(&[true, true, false]), 1);
        assert_eq!(c(&[true, true, true]), 2);
    }

    #[test]
    fn keyed_cache_clones_on_hit_and_counts() {
        use super::KeyedCache;
        let mut cache: KeyedCache<u32, u32> = KeyedCache::default();
        let mut created = 0;
        for _ in 0..3 {
            let value = cache.get_or_insert_with(7, || {
                created += 1;
                42
            });
            assert_eq!(value, 42);
        }
        let value = cache.get_or_insert_with(8, || {
            created += 1;
            99
        });
        assert_eq!(value, 99);
        assert_eq!(created, 2);
        assert_eq!(cache.hits, 2);
        assert_eq!(cache.misses, 2);
    }

    #[test]
    fn z_bias_maps_d3d8_steps_to_a_viewer_side_constant() {
        use super::{ZBIAS_UNITS_PER_STEP, z_bias_state};
        assert_eq!(z_bias_state(0), wgpu::DepthBiasState::default());
        assert_eq!(
            z_bias_state(1),
            wgpu::DepthBiasState {
                constant: -ZBIAS_UNITS_PER_STEP,
                slope_scale: 0.0,
                clamp: 0.0,
            }
        );
        assert_eq!(
            z_bias_state(16),
            wgpu::DepthBiasState {
                constant: -16 * ZBIAS_UNITS_PER_STEP,
                slope_scale: 0.0,
                clamp: 0.0,
            }
        );
    }

    #[test]
    fn blend_operations_map_to_wgpu() {
        use super::blend_operation;
        use crate::d3d8::enums::D3DBLENDOP as O;
        assert_eq!(blend_operation(O::Add), wgpu::BlendOperation::Add);
        assert_eq!(blend_operation(O::Subtract), wgpu::BlendOperation::Subtract);
        assert_eq!(
            blend_operation(O::RevSubtract),
            wgpu::BlendOperation::ReverseSubtract
        );
        assert_eq!(blend_operation(O::Min), wgpu::BlendOperation::Min);
        assert_eq!(blend_operation(O::Max), wgpu::BlendOperation::Max);
    }
}
