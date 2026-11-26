//! Fixed-function lighting state and the bounded CPU reference/fallback.
//! GPU draws snapshot prepared light/material uniforms; guest stream bytes
//! are never modified. CPU XYZ/float RGBA/UV feeds the existing raster shaders;
//! the same evaluation also feeds `ProcessVertices`, which bakes lighting and
//! the world/view/projection transform into an XYZRHW destination buffer.
//! Specular lighting is computed only for `ProcessVertices`; a draw that needs
//! the vertex specular input still fails by name.
use super::*;
use crate::d3d8::fixed_function::{d3dcolor_to_rgba, transform_texcoord};

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = dot(v, v).sqrt();
    if length == 0.0 {
        [0.0; 3]
    } else {
        v.map(|x| x / length)
    }
}
fn transform(m: Mat4, v: [f32; 3], w: f32) -> [f32; 3] {
    let v = m.transform([v[0], v[1], v[2], w]);
    [v[0], v[1], v[2]]
}

// Row-vector normal transform n * inverse(world * view)^T. Translation
// does not participate. Preserve normal length unless NORMALIZENORMALS is on.
fn inverse3(m: Mat4) -> Result<[[f32; 3]; 3], RenderError> {
    let [[a, b, c, _], [d, e, f, _], [g, h, i, _], _] = m.rows;
    let cofactors = [
        [e * i - f * h, c * h - b * i, b * f - c * e],
        [f * g - d * i, a * i - c * g, c * d - a * f],
        [d * h - e * g, b * g - a * h, a * e - b * d],
    ];
    let det = a * cofactors[0][0] + b * cofactors[1][0] + c * cofactors[2][0];
    if det == 0.0 || !det.is_finite() {
        return Err(RenderError::new(
            "vertex lighting",
            "singular/nonfinite world-view normal transform",
        ));
    }
    Ok(cofactors.map(|row| row.map(|v| v / det)))
}

/// Guest vertex layout consumed by [`DeviceState::light_vertices`] and
/// [`DeviceState::process_vertices`].
///
/// `light_vertices` always writes the fixed [`LIT_RECORD`]-byte lit layout
/// from [`lit_layout`](super::fixed_function::lit_layout): XYZ at 0, float
/// RGBA diffuse at 12 and texture coordinate sets 0 and 1 at 28 and 36.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LitInput {
    /// Bytes between consecutive input vertices.
    pub stride: usize,
    /// Byte offset of the 3-float normal, or `None` when the FVF has none.
    /// D3D8 then lights with a zero normal: only the ambient and emissive
    /// terms contribute.
    pub normal_offset: Option<usize>,
    /// Byte offset of the `D3DCOLOR` diffuse, or `None` when the FVF has no
    /// COLOR1 and the material supplies it.
    pub diffuse_offset: Option<usize>,
    /// Byte offset of the 2-float texture coordinate.
    pub uv_offset: usize,
    /// Byte offset of texture set 1, or `None` to repeat set 0.
    pub uv1_offset: Option<usize>,
}

/// Bytes per vertex written by [`DeviceState::light_vertices`].
pub const LIT_RECORD: usize = 44;

impl LitInput {
    /// `XYZ | NORMAL | DIFFUSE | TEX1` (0x152): diffuse at 24, UV at 28.
    pub const XYZ_NORMAL_DIFFUSE_TEX1: Self = Self {
        stride: 36,
        normal_offset: Some(12),
        diffuse_offset: Some(24),
        uv_offset: 28,
        uv1_offset: None,
    };

    /// `XYZ | NORMAL | TEX1` (0x112): no per-vertex diffuse, UV at 24.
    pub const XYZ_NORMAL_TEX1: Self = Self {
        stride: 32,
        normal_offset: Some(12),
        diffuse_offset: None,
        uv_offset: 24,
        uv1_offset: None,
    };

    /// `XYZ | NORMAL | TEX2` (0x212): 0x112 with a second UV set at 32.
    pub const XYZ_NORMAL_TEX2: Self = Self {
        stride: 40,
        normal_offset: Some(12),
        diffuse_offset: None,
        uv_offset: 24,
        uv1_offset: Some(32),
    };

    /// The normal-less FVFs a game may draw with `D3DRS_LIGHTING` on:
    /// XYZ | DIFFUSE | TEX1/TEX2 (0x142/0x242) and the same with SPECULAR
    /// (0x1C2/0x2C2), whose texture sets start 4 bytes later.
    pub(crate) fn without_normal(fvf: u32) -> Option<Self> {
        let (stride, uv_offset, uv1_offset) = match fvf {
            0x0142 => (24, 16, None),
            0x0242 => (32, 16, Some(24)),
            0x01C2 => (28, 20, None),
            0x02C2 => (36, 20, Some(28)),
            _ => return None,
        };
        Some(Self {
            stride,
            normal_offset: None,
            diffuse_offset: Some(12),
            uv_offset,
            uv1_offset,
        })
    }

    /// The lit FVFs `ProcessVertices` accepts as a source. Other FVFs would
    /// need a lighting path the engine does not ask for here, so they are
    /// named refusals rather than silently unlit output.
    pub(crate) fn from_fvf(fvf: u32) -> Result<Self, RenderError> {
        match fvf {
            0x0152 => Ok(Self::XYZ_NORMAL_DIFFUSE_TEX1),
            0x0112 => Ok(Self::XYZ_NORMAL_TEX1),
            0x0212 => Ok(Self::XYZ_NORMAL_TEX2),
            other => Err(RenderError::new(
                "ProcessVertices",
                format!("source FVF {other:#06x} is not 0x152, 0x112 or 0x212"),
            )),
        }
    }
}

/// A `ProcessVertices` destination layout. The engine creates the destination
/// buffer with FVF `0x1c4` (`XYZRHW | DIFFUSE | SPECULAR | TEX1`, stride 32) or
/// `0x2c4` (adds `TEX2`, stride 40); [`DeviceState::process_vertices`] writes
/// exactly the components those FVFs name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessDest {
    stride: usize,
    /// Bytes of texture coordinates the destination carries (one or two
    /// 2-float sets).
    texcoord_bytes: usize,
}

impl ProcessDest {
    fn decode(fvf: u32) -> Result<Self, RenderError> {
        match fvf {
            0x01c4 => Ok(Self {
                stride: 32,
                texcoord_bytes: 8,
            }),
            0x02c4 => Ok(Self {
                stride: 40,
                texcoord_bytes: 16,
            }),
            other => Err(RenderError::new(
                "ProcessVertices",
                format!("destination FVF {other:#06x} is not 0x1c4 or 0x2c4"),
            )),
        }
    }
}

/// Quantize a clamped float RGBA to a `D3DCOLOR` (`ARGB`). The raster shader
/// decodes each byte as `value / 255`, so this is the inverse the pre-transformed
/// path sees.
fn rgba_to_d3dcolor(c: [f32; 4]) -> u32 {
    let byte = |v: f32| ((v.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xff;
    (byte(c[3]) << 24) | (byte(c[0]) << 16) | (byte(c[1]) << 8) | byte(c[2])
}

/// Fixed-function lighting output for one vertex.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LitResult {
    diffuse: [f32; 4],
    specular: [f32; 4],
}

/// Precomputed lighting environment shared by every vertex of a pass, so the
/// light list, normal matrix and ambient term are not rebuilt per vertex.
struct LightingSetup<'a> {
    state: &'a DeviceState,
    enabled: Vec<&'a Light>,
    world_view: Mat4,
    normal_matrix: Option<[[f32; 3]; 3]>,
    global_ambient: [f32; 4],
}

impl LightingSetup<'_> {
    /// Evaluate the fixed-function lighting model for one source vertex.
    /// Equations: Microsoft Mathematics of Lighting / Diffuse Lighting /
    /// Attenuation and Spotlight Factor, plus the D3D8 specular term using the
    /// local or infinite viewer selected by `D3DRS_LOCALVIEWER`.
    fn evaluate(&self, vertex: &[u8], layout: LitInput) -> Result<LitResult, RenderError> {
        let s = &self.state.states;
        let material = &self.state.material;
        let float =
            |offset: usize| f32::from_le_bytes(vertex[offset..offset + 4].try_into().unwrap());
        let position = [float(0), float(4), float(8)];
        let color = layout.diffuse_offset.map(|offset| {
            d3dcolor_to_rgba(u32::from_le_bytes(
                vertex[offset..offset + 4].try_into().unwrap(),
            ))
        });
        // COLOR1 with no diffuse component (0x112) and COLOR2 in every lit FVF
        // fall back to the material, matching D3D8's D3DMCS_* material-source
        // rule for a missing vertex colour.
        let material_source = |source, mat, vertex_color: Option<[f32; 4]>| {
            if s.color_vertex && source == D3DMATERIALCOLORSOURCE::Color1 {
                vertex_color.unwrap_or(mat)
            } else {
                mat
            }
        };
        let mut diffuse;
        let mut specular;
        if s.lighting {
            let ambient = material_source(s.ambient_material_source, material.ambient, color);
            let emissive = material_source(s.emissive_material_source, material.emissive, color);
            let source_diffuse =
                material_source(s.diffuse_material_source, material.diffuse, color);
            let source_specular =
                material_source(s.specular_material_source, material.specular, color);
            let normal = match layout.normal_offset {
                Some(at) => {
                    let normal = [float(at), float(at + 4), float(at + 8)];
                    let mut normal = self
                        .normal_matrix
                        .expect("normal matrix prepared with lighting")
                        .map(|row| dot(row, normal));
                    if s.normalize_normals {
                        normal = normalize(normal);
                    }
                    normal
                }
                None => [0.0; 3],
            };
            let view_position = transform(self.world_view, position, 1.0);
            // Camera at the view-space origin. `D3DRS_LOCALVIEWER` selects the
            // per-vertex eye vector; otherwise the viewer is at infinity.
            let eye = if s.local_viewer {
                normalize([-view_position[0], -view_position[1], -view_position[2]])
            } else {
                [0.0, 0.0, -1.0]
            };
            diffuse = [0.0; 4];
            specular = [0.0; 4];
            for c in 0..3 {
                diffuse[c] = emissive[c] + ambient[c] * self.global_ambient[c];
            }
            // Lighting changes RGB; alpha comes from the diffuse source.
            diffuse[3] = source_diffuse[3];
            // A disabled specular input stays fully zero, matching the draw
            // path's specular default; alpha is only carried when enabled.
            specular[3] = if s.specular_enable {
                source_specular[3]
            } else {
                0.0
            };
            for light in &self.enabled {
                let (direction, mut attenuation) = if light.light_type == 3 {
                    (
                        normalize(transform(self.state.view, light.direction, 0.0)).map(|v| -v),
                        1.0,
                    )
                } else {
                    let light_position = transform(self.state.view, light.position, 1.0);
                    let delta = std::array::from_fn(|c| light_position[c] - view_position[c]);
                    let distance = dot(delta, delta).sqrt();
                    // DIVERGENCE(original): a light at a non-finite position
                    // contributes nothing instead of NaN colour.
                    if !(distance <= light.range) {
                        continue;
                    }
                    let denominator = light.attenuation0
                        + light.attenuation1 * distance
                        + light.attenuation2 * distance * distance;
                    if denominator <= 0.0 || !denominator.is_finite() {
                        return Err(RenderError::new(
                            "vertex lighting",
                            format!(
                                "nonpositive/nonfinite point/spot attenuation denominator: \
                                 denominator={denominator:?}, distance={distance:?}, \
                                 position={position:?}, attenuation={:?}",
                                [light.attenuation0, light.attenuation1, light.attenuation2]
                            ),
                        ));
                    }
                    (normalize(delta), 1.0 / denominator)
                };
                if light.light_type == 2 {
                    let spot_direction =
                        normalize(transform(self.state.view, light.direction, 0.0));
                    let rho = -dot(direction, spot_direction);
                    let inner = (light.theta * 0.5).cos();
                    let outer = (light.phi * 0.5).cos();
                    let spot = if rho > inner {
                        1.0
                    } else if rho <= outer {
                        0.0
                    } else {
                        ((rho - outer) / (inner - outer)).powf(light.falloff)
                    };
                    attenuation *= spot;
                }
                let lambert = dot(normal, direction).max(0.0);
                for c in 0..3 {
                    diffuse[c] += attenuation
                        * (ambient[c] * light.ambient[c]
                            + source_diffuse[c] * light.diffuse[c] * lambert);
                }
                if s.specular_enable {
                    let halfway = normalize([
                        direction[0] + eye[0],
                        direction[1] + eye[1],
                        direction[2] + eye[2],
                    ]);
                    let n_dot_h = dot(normal, halfway).max(0.0);
                    let factor = n_dot_h.powf(material.power);
                    for c in 0..3 {
                        specular[c] +=
                            attenuation * source_specular[c] * light.specular[c] * factor;
                    }
                }
            }
        } else {
            diffuse = if let Some(color) = color.filter(|_| s.color_vertex) {
                color
            } else {
                material.diffuse
            };
            specular = [0.0; 4];
        }
        for value in &mut diffuse {
            *value = value.clamp(0.0, 1.0);
        }
        for value in &mut specular {
            *value = value.clamp(0.0, 1.0);
        }
        Ok(LitResult { diffuse, specular })
    }
}

/// Immutable per-draw GPU diffuse-lighting snapshot. Every field is vec4-sized
/// (normal rows are padded) to match WGSL uniform alignment exactly.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuLight {
    position_type: [f32; 4],
    direction: [f32; 4],
    diffuse: [f32; 4],
    ambient: [f32; 4],
    attenuation_range: [f32; 4],
    spot: [f32; 4],
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct LightingUniform {
    world_view: [[f32; 4]; 4],
    normal: [[f32; 4]; 3],
    diffuse: [f32; 4],
    ambient: [f32; 4],
    emissive: [f32; 4],
    global_ambient: [f32; 4],
    flags: [u32; 4],
    lights: [GpuLight; MAX_LIGHTS],
}

impl DeviceState {
    /// Prepare GPU diffuse lighting without reading any source vertices.
    /// Unsafe positional attenuation stays on the CPU path, which retains its
    /// named per-vertex denominator diagnostic. Specular draws remain rejected
    /// by validate_draw; ProcessVertices always retains the CPU evaluator.
    pub(crate) fn gpu_lighting_uniform(&self) -> Result<Option<LightingUniform>, RenderError> {
        use bytemuck::Zeroable;
        let setup = self.lighting_setup()?;
        let s = &self.states;
        let mut out = LightingUniform::zeroed();
        out.world_view = setup.world_view.rows;
        if let Some(normal) = setup.normal_matrix {
            out.normal = normal.map(|r| [r[0], r[1], r[2], 0.0]);
        }
        out.diffuse = self.material.diffuse;
        out.ambient = self.material.ambient;
        out.emissive = self.material.emissive;
        out.global_ambient = setup.global_ambient;
        let color_source = |src| u32::from(s.color_vertex && src == D3DMATERIALCOLORSOURCE::Color1);
        out.flags = [
            u32::from(s.lighting),
            u32::from(s.normalize_normals),
            color_source(s.diffuse_material_source)
                | color_source(s.ambient_material_source) << 1
                | color_source(s.emissive_material_source) << 2
                | u32::from(s.color_vertex) << 3,
            0,
        ];
        if s.lighting {
            for (i, light) in setup.enabled.iter().enumerate() {
                if light.light_type != 3
                    && (!(light.attenuation0 > 0.0)
                        || light.attenuation1 < 0.0
                        || light.attenuation2 < 0.0)
                {
                    return Ok(None);
                }
                if light.light_type != 3 {
                    let r = light.range.max(0.0);
                    let max_denominator =
                        light.attenuation0 + light.attenuation1 * r + light.attenuation2 * r * r;
                    if !max_denominator.is_finite() {
                        return Ok(None);
                    }
                }
                let position = transform(self.view, light.position, 1.0);
                let direction = normalize(transform(self.view, light.direction, 0.0));
                out.lights[i] = GpuLight {
                    position_type: [
                        position[0],
                        position[1],
                        position[2],
                        light.light_type as f32,
                    ],
                    direction: [direction[0], direction[1], direction[2], 0.0],
                    diffuse: light.diffuse,
                    ambient: light.ambient,
                    attenuation_range: [
                        light.attenuation0,
                        light.attenuation1,
                        light.attenuation2,
                        light.range,
                    ],
                    spot: [
                        (light.theta * 0.5).cos(),
                        (light.phi * 0.5).cos(),
                        light.falloff,
                        0.0,
                    ],
                };
                out.flags[3] += 1;
            }
        }
        // Nonfinite state remains CPU-evaluated, including legacy quirks.
        let floats = bytemuck::cast_slice::<LightingUniform, f32>(std::slice::from_ref(&out));
        if floats.iter().any(|v| !v.is_finite()) {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// Preserve CPU error handling for nonfinite positional inputs. This is a
    /// single transform per source vertex, not a lighting evaluation; indexed
    /// streams have already packed only their referenced vertices. Directional
    /// passes need no position preflight.
    pub(crate) fn gpu_lighting_positions_supported(
        &self,
        bytes: &[u8],
        start: usize,
        end: usize,
        stride: usize,
    ) -> bool {
        if !self.states.lighting
            || !self
                .lights
                .iter()
                .zip(&self.light_enabled)
                .any(|(l, enabled)| *enabled && l.light_type != 3)
        {
            return true;
        }
        let world_view = self.world.mul(self.view);
        (start..end).all(|i| {
            let at = i * stride;
            let pos = std::array::from_fn(|k| {
                f32::from_le_bytes(bytes[at + k * 4..at + k * 4 + 4].try_into().unwrap())
            });
            transform(world_view, pos, 1.0)
                .iter()
                .all(|v| v.is_finite())
        })
    }

    fn lighting_setup(&self) -> Result<LightingSetup<'_>, RenderError> {
        let s = &self.states;
        // DIVERGENCE(original): past MAX_LIGHTS enabled lights, the lowest
        // indices light the draw; drivers differ on which they keep.
        let enabled: Vec<_> = self
            .lights
            .iter()
            .zip(&self.light_enabled)
            .filter_map(|(light, on)| on.then_some(light))
            .take(MAX_LIGHTS)
            .collect();
        if s.lighting {
            for light in &enabled {
                if !(1..=3).contains(&light.light_type) {
                    return Err(RenderError::new(
                        "vertex lighting",
                        format!("unsupported enabled D3DLIGHTTYPE {}", light.light_type),
                    ));
                }
            }
        }
        let world_view = self.world.mul(self.view);
        let normal_matrix = if s.lighting {
            Some(inverse3(world_view)?)
        } else {
            None
        };
        let global_ambient = d3dcolor_to_rgba(s.ambient);
        Ok(LightingSetup {
            state: self,
            enabled,
            world_view,
            normal_matrix,
            global_ambient,
        })
    }

    /// Diagnostic for the first `ProcessVertices` calls. Enabled by
    /// `RECOMP_D3D8_TRACE_PROCESS_VERTICES`; the value is the number of calls
    /// to print (default 8). Unset means no work is done. Prints the source
    /// and destination FVF/stride/count plus each stage's `TEXCOORDINDEX`,
    /// `TEXTURETRANSFORMFLAGS` and `D3DTS_TEXTUREN` matrix, so the guest's
    /// pre-draw transform setup can be compared with the following draw.
    pub(crate) fn trace_process_vertices(
        &self,
        src_fvf: u32,
        dest_fvf: u32,
        src_stride: u32,
        count: u32,
    ) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let Ok(raw) = std::env::var("RECOMP_D3D8_TRACE_PROCESS_VERTICES") else {
            return;
        };
        let limit: u32 = raw.parse().unwrap_or(8);
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        if n >= limit {
            return;
        }
        use crate::d3d8::enums::D3DTEXTURESTAGESTATETYPE as Ts;
        eprintln!(
            "[d3d8-trace] process_vertices {n}: src_fvf=0x{src_fvf:06X} stride={src_stride} dest_fvf=0x{dest_fvf:06X} count={count}"
        );
        for stage in 0..2u32 {
            let get = |s: Ts| match self.texture_stage_state(stage, s.raw()) {
                Some(v) => format!("{v:#x}"),
                None => "-".to_string(),
            };
            eprintln!(
                "[d3d8-trace]   pv stage{stage} tci={} ttf={}",
                get(Ts::TexCoordIndex),
                get(Ts::TextureTransformFlags)
            );
            eprintln!(
                "[d3d8-trace]   pv stage{stage} texture_matrix={:?}",
                self.texture_transforms[stage as usize].rows
            );
        }
    }

    /// 0x152: XYZ at 0, NORMAL at 12, D3DCOLOR at 24, float2 UV at 28.
    /// 0x112: XYZ at 0, NORMAL at 12, float2 UV at 24, material diffuse.
    /// Evaluate in camera space and keep floating diffuse until rasterization.
    /// Writes exactly `(end - start) * LIT_RECORD` bytes into `out`, packing the
    /// `start..end` vertex range from output slot zero. The caller-owned scratch
    /// buffer lets a draw loop reuse one allocation; `resize` only zero-fills the first
    /// time a larger draw grows it.
    pub(crate) fn light_vertices(
        &self,
        bytes: &[u8],
        start: usize,
        end: usize,
        layout: LitInput,
        out: &mut Vec<u8>,
    ) -> Result<(), RenderError> {
        let setup = self.lighting_setup()?;
        out.resize((end - start) * LIT_RECORD, 0);
        for index in start..end {
            let begin = index * layout.stride;
            let vertex = &bytes[begin..begin + layout.stride];
            let lit = setup.evaluate(vertex, layout)?;
            let slot = index - start;
            let dst = &mut out[slot * LIT_RECORD..(slot + 1) * LIT_RECORD];
            dst[..12].copy_from_slice(&vertex[..12]);
            dst[12..28].copy_from_slice(bytemuck::bytes_of(&lit.diffuse));
            dst[28..36].copy_from_slice(&vertex[layout.uv_offset..layout.uv_offset + 8]);
            let uv1 = layout.uv1_offset.unwrap_or(layout.uv_offset);
            dst[36..44].copy_from_slice(&vertex[uv1..uv1 + 8]);
        }
        Ok(())
    }

    /// `IDirect3DDevice8::ProcessVertices` fixed-function output. Reads `count`
    /// source vertices starting at `src_start`, applies the current
    /// world/view/projection transform, lighting and viewport, and writes them
    /// at `dest_index` in the destination layout `dest_fvf`. `dest` is the whole
    /// destination buffer; the write range is bounds-checked inside. The
    /// destination buffer's FVF is the output format, as D3D8 defines it.
    pub(crate) fn process_vertices(
        &self,
        src: &[u8],
        src_start: usize,
        count: usize,
        layout: LitInput,
        dest_fvf: u32,
        dest_index: usize,
        dest: &mut [u8],
    ) -> Result<(), RenderError> {
        let out_layout = ProcessDest::decode(dest_fvf)?;
        let src_end = src_start
            .checked_add(count)
            .and_then(|n| n.checked_mul(layout.stride))
            .ok_or_else(|| RenderError::new("ProcessVertices", "source range overflow"))?;
        if src_end > src.len() {
            return Err(RenderError::new(
                "ProcessVertices",
                "source vertex range exceeds the bound stream",
            ));
        }
        let dst_begin = dest_index
            .checked_mul(out_layout.stride)
            .ok_or_else(|| RenderError::new("ProcessVertices", "destination offset overflow"))?;
        let dst_end = count
            .checked_mul(out_layout.stride)
            .and_then(|n| dst_begin.checked_add(n))
            .ok_or_else(|| RenderError::new("ProcessVertices", "destination range overflow"))?;
        if dst_end > dest.len() {
            return Err(RenderError::new(
                "ProcessVertices",
                "destination vertex range exceeds the buffer",
            ));
        }
        let setup = self.lighting_setup()?;
        let world_view_projection = self.world.mul(self.view).mul(self.projection);
        let vp = self.viewport;
        // D3D8's fixed-function `ProcessVertices` generates the destination
        // texture coordinates from the texture stages active at call time:
        // output set N comes from texture stage N, transformed by its
        // `D3DTS_TEXTUREN` matrix when `D3DTSS_TEXTURETRANSFORMFLAGS` is not
        // DISABLE. The application then reprograms `D3DTSS_TEXCOORDINDEX` to
        // point at the generated set and resets the transform before drawing
        // the pre-transformed vertices (see `IDirect3DDevice9::ProcessVertices`
        // and the "Fixed Function Vertex Processing" page: "Texture
        // coordinates are generated when texture transform or texture
        // generation is enabled"). This source FVF carries one coordinate set,
        // so every stage's `TEXCOORDINDEX` selects that set. The later `XYZRHW`
        // draw must not re-apply the transform, or it would be applied twice.
        let texcoord_sets = out_layout.texcoord_bytes / 8;
        let mut stage_transforms = [(0u32, Mat4::IDENTITY); 2];
        for (set, transform) in stage_transforms.iter_mut().enumerate().take(texcoord_sets) {
            let stage = self.resolve_texture_stage(set as u32, true)?;
            // D3D8's ProcessVertices also honours texture-coordinate
            // generation, but this game never selects it here: its
            // ProcessVertices path programs plain vertex coordinate sets
            // (`FUN_00547650`). Refuse CAMERASPACEPOSITION by name rather than
            // feeding the vertex coordinate through the affine transform and
            // silently producing the wrong destination coordinates.
            if stage.tex_coord_gen != crate::d3d8::state::TEXCOORD_GEN_PASSTHRU {
                return Err(RenderError::new(
                    "ProcessVertices",
                    "D3DTSS_TCI_CAMERASPACEPOSITION texture generation is not implemented for ProcessVertices",
                ));
            }
            *transform = (stage.tex_transform_flags, stage.tex_transform);
        }
        let mut pv_trace = ProcessTrace::new(count);
        for i in 0..count {
            let begin = (src_start + i) * layout.stride;
            let vertex = &src[begin..begin + layout.stride];
            let lit = setup.evaluate(vertex, layout)?;
            let float =
                |offset: usize| f32::from_le_bytes(vertex[offset..offset + 4].try_into().unwrap());
            let clip = world_view_projection.transform([float(0), float(4), float(8), 1.0]);
            let w = clip[3];
            pv_trace.record(i, w, [float(0), float(4), float(8)]);
            let rhw = if w != 0.0 && w.is_finite() {
                1.0 / w
            } else {
                0.0
            };
            let ndc = [clip[0] * rhw, clip[1] * rhw, clip[2] * rhw];
            let sx = (ndc[0] + 1.0) * vp.width as f32 * 0.5 + vp.x as f32;
            let sy = (1.0 - ndc[1]) * vp.height as f32 * 0.5 + vp.y as f32;
            let sz = vp.min_z + ndc[2] * (vp.max_z - vp.min_z);
            let dst = &mut dest[dst_begin + i * out_layout.stride..][..out_layout.stride];
            dst[0..4].copy_from_slice(&sx.to_le_bytes());
            dst[4..8].copy_from_slice(&sy.to_le_bytes());
            dst[8..12].copy_from_slice(&sz.to_le_bytes());
            dst[12..16].copy_from_slice(&rhw.to_le_bytes());
            dst[16..20].copy_from_slice(&rgba_to_d3dcolor(lit.diffuse).to_le_bytes());
            dst[20..24].copy_from_slice(&rgba_to_d3dcolor(lit.specular).to_le_bytes());
            let uv = [
                f32::from_le_bytes(
                    vertex[layout.uv_offset..layout.uv_offset + 4]
                        .try_into()
                        .unwrap(),
                ),
                f32::from_le_bytes(
                    vertex[layout.uv_offset + 4..layout.uv_offset + 8]
                        .try_into()
                        .unwrap(),
                ),
            ];
            for (set, (flags, matrix)) in stage_transforms.iter().enumerate().take(texcoord_sets) {
                let transformed = transform_texcoord(uv, matrix, *flags);
                let offset = 24 + set * 8;
                dst[offset..offset + 4].copy_from_slice(&transformed[0].to_le_bytes());
                dst[offset + 4..offset + 8].copy_from_slice(&transformed[1].to_le_bytes());
            }
        }
        pv_trace.finish(src_start, dest_index, dest_fvf);
        trace_source_triangles(src, src_start, count, layout);
        Ok(())
    }
}

/// Camera-independent check of the `ProcessVertices` *source* mesh: treats the
/// stream as a triangle list and reports the triangles with the longest
/// world-space edge (with each vertex's position, normal and uv), so bad mesh
/// data can be told apart from projection artefacts. Same env var and line cap
/// as `ProcessTrace`; one report per call, only when an edge exceeds 1000.
fn trace_source_triangles(src: &[u8], src_start: usize, count: usize, layout: LitInput) {
    static PRINTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let limit = ProcessTrace::limit();
    if limit == 0 || PRINTED.load(std::sync::atomic::Ordering::Relaxed) >= limit {
        return;
    }
    let f = |v: usize, off: usize| {
        let b = (src_start + v) * layout.stride + off;
        f32::from_le_bytes(src[b..b + 4].try_into().unwrap())
    };
    let pos = |v: usize| [f(v, 0), f(v, 4), f(v, 8)];
    let dist = |a: [f32; 3], b: [f32; 3]| {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    };
    let mut worst: Vec<(f32, usize)> = Vec::new();
    let mut over_1000 = 0usize;
    let mut non_finite = 0usize;
    for t in 0..count / 3 {
        let (a, b, c) = (pos(t * 3), pos(t * 3 + 1), pos(t * 3 + 2));
        let e = dist(a, b).max(dist(b, c)).max(dist(c, a));
        if !e.is_finite() {
            non_finite += 1;
        } else if e > 1000.0 {
            over_1000 += 1;
            worst.push((e, t));
        }
    }
    if over_1000 == 0 && non_finite == 0 {
        return;
    }
    PRINTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    eprintln!(
        "[d3d8-trace] src_mesh src_start={src_start} count={count} tris={} edge>1000:{over_1000} non_finite:{non_finite}",
        count / 3
    );
    for (e, t) in worst.iter().take(3) {
        for k in 0..3 {
            let v = t * 3 + k;
            eprintln!(
                "[d3d8-trace]   src_mesh tri={t} edge={e} v{k}(idx {v}) pos={:?} normal=[{},{},{}] uv=[{},{}]",
                pos(v),
                f(v, 12),
                f(v, 16),
                f(v, 20),
                f(v, layout.uv_offset),
                f(v, layout.uv_offset + 4)
            );
        }
    }
}

/// `RECOMP_D3D8_TRACE_PROCESS_VERTICES=<max lines>` summarises each
/// `ProcessVertices` call whose clip-space `w` is not safely positive
/// (`w <= 0`, or `0 < w < 1`), to see whether the XYZRHW draws that later
/// stretch across the screen were fed vertices at or behind the eye.
struct ProcessTrace {
    enabled: bool,
    count: usize,
    non_positive: usize,
    tiny: usize,
    min_w: f32,
    min_at: usize,
    min_pos: [f32; 3],
}

impl ProcessTrace {
    fn limit() -> usize {
        static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *LIMIT.get_or_init(|| {
            std::env::var("RECOMP_D3D8_TRACE_PROCESS_VERTICES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        })
    }

    fn new(count: usize) -> Self {
        Self {
            enabled: Self::limit() > 0,
            count,
            non_positive: 0,
            tiny: 0,
            min_w: f32::INFINITY,
            min_at: 0,
            min_pos: [0.0; 3],
        }
    }

    fn record(&mut self, index: usize, w: f32, pos: [f32; 3]) {
        if !self.enabled {
            return;
        }
        if w <= 0.0 || !w.is_finite() {
            self.non_positive += 1;
        } else if w < 1.0 {
            self.tiny += 1;
        }
        if w < self.min_w {
            self.min_w = w;
            self.min_at = index;
            self.min_pos = pos;
        }
    }

    fn finish(&self, src_start: usize, dest_index: usize, dest_fvf: u32) {
        static PRINTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        if !self.enabled || (self.non_positive == 0 && self.tiny == 0) {
            return;
        }
        if PRINTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= Self::limit() {
            return;
        }
        eprintln!(
            "[d3d8-trace] process_vertices src_start={src_start} dest_index={dest_index} \
             dest_fvf=0x{dest_fvf:X} count={} w<=0:{} 0<w<1:{} min_w={} at={} src_pos={:?}",
            self.count, self.non_positive, self.tiny, self.min_w, self.min_at, self.min_pos
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vertex(normal: [f32; 3], color: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in [0.0f32, 0.0, 0.5].into_iter().chain(normal) {
            bytes.extend(value.to_le_bytes());
        }
        bytes.extend(color.to_le_bytes());
        bytes.extend([0; 8]);
        bytes
    }
    fn lit_color(state: &DeviceState, normal: [f32; 3], color: u32) -> [f32; 4] {
        let mut out = Vec::new();
        state
            .light_vertices(
                &vertex(normal, color),
                0,
                1,
                LitInput::XYZ_NORMAL_DIFFUSE_TEX1,
                &mut out,
            )
            .unwrap();
        std::array::from_fn(|c| f32::from_le_bytes(out[12 + c * 4..16 + c * 4].try_into().unwrap()))
    }

    fn vertex_xyz_normal_tex1(normal: [f32; 3], uv: [f32; 2]) -> Vec<u8> {
        vertex_at_xyz_normal_tex1([0.0, 0.0, 0.5], normal, uv)
    }
    fn vertex_at_xyz_normal_tex1(position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in position.into_iter().chain(normal).chain(uv) {
            bytes.extend(value.to_le_bytes());
        }
        bytes
    }
    fn directional() -> DeviceState {
        let mut state = DeviceState::new(32, 32);
        state
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
        state.light_enable(0, true).unwrap();
        state
    }
    #[test]
    fn normal_less_vertices_light_with_a_zero_normal() {
        // XYZ | DIFFUSE | TEX2 (0x242): the directional light adds nothing,
        // so the vertex colour reaches the output only through the ambient
        // term (AMBIENTMATERIALSOURCE = COLOR1, global ambient white).
        let mut state = directional();
        state.set_render_state(139, 0xffff_ffff).unwrap(); // D3DRS_AMBIENT
        state.set_render_state(147, 1).unwrap(); // AMBIENTMATERIALSOURCE COLOR1
        let mut vertex = Vec::new();
        for v in [1.0f32, 2.0, 3.0] {
            vertex.extend(v.to_le_bytes());
        }
        vertex.extend(0x80ff_8000u32.to_le_bytes());
        for v in [0.25f32, 0.5, 4.0, 8.0] {
            vertex.extend(v.to_le_bytes());
        }
        let layout = LitInput::without_normal(0x242).unwrap();
        let mut out = Vec::new();
        state
            .light_vertices(&vertex, 0, 1, layout, &mut out)
            .unwrap();
        assert_eq!(out.len(), LIT_RECORD);
        let f = |at: usize| f32::from_le_bytes(out[at..at + 4].try_into().unwrap());
        assert_eq!([f(12), f(16), f(20)], [1.0, 128.0 / 255.0, 0.0]);
        assert_eq!([f(28), f(32), f(36), f(40)], [0.25, 0.5, 4.0, 8.0]);
        // Without lighting these FVFs keep the unlit path.
        assert!(LitInput::without_normal(0x112).is_none());
    }

    #[test]
    fn gpu_uniform_layout_and_cpu_fallback_keep_diagnostics() {
        assert_eq!(std::mem::size_of::<GpuLight>(), 96);
        assert_eq!(std::mem::size_of::<LightingUniform>(), 960);
        assert_eq!(std::mem::offset_of!(LightingUniform, flags), 176);
        assert_eq!(std::mem::offset_of!(LightingUniform, lights), 192);
        let mut state = directional();
        assert!(state.gpu_lighting_uniform().unwrap().is_some());
        for attenuation0 in [0.0, -1.0, f32::INFINITY, f32::NAN] {
            state
                .set_light(
                    0,
                    Light {
                        light_type: 1,
                        position: [0.0, 0.0, 1.0],
                        range: 10.0,
                        attenuation0,
                        ..Light::default()
                    },
                )
                .unwrap();
            assert!(state.gpu_lighting_uniform().unwrap().is_none());
            assert!(
                state
                    .light_vertices(
                        &vertex([0.0, 0.0, 1.0], u32::MAX),
                        0,
                        1,
                        LitInput::XYZ_NORMAL_DIFFUSE_TEX1,
                        &mut Vec::new()
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("attenuation denominator")
            );
        }
        state
            .set_light(
                0,
                Light {
                    light_type: 1,
                    position: [0.0, 0.0, 1.0],
                    range: 10.0,
                    attenuation0: 1.0,
                    ..Light::default()
                },
            )
            .unwrap();
        let valid = vertex([0.0, 0.0, 1.0], u32::MAX);
        let mut bad = valid.clone();
        bad[..4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(state.gpu_lighting_positions_supported(&valid, 0, 1, 36));
        assert!(!state.gpu_lighting_positions_supported(&bad, 0, 1, 36));
        state
            .set_light(
                0,
                Light {
                    light_type: 99,
                    ..Light::default()
                },
            )
            .unwrap();
        assert!(state.gpu_lighting_uniform().is_err());
        state.light_enable(0, false).unwrap();
        state.world = Mat4::scale(0.0, 1.0, 1.0);
        assert!(state.gpu_lighting_uniform().is_err());
    }

    #[test]
    fn packed_indexed_lighting_matches_expanded_bytes_and_skips_invalid_gaps() {
        use crate::d3d8::resource::{IndexedDraw, expand_indexed_into, packed_indexed_list_into};
        let state = directional();
        for layout in [LitInput::XYZ_NORMAL_TEX1, LitInput::XYZ_NORMAL_DIFFUSE_TEX1] {
            let mut vertices = vec![0; 70 * layout.stride];
            for chunk in vertices.chunks_exact_mut(4) {
                chunk.copy_from_slice(&f32::NAN.to_le_bytes());
            }
            for (slot, normal, color) in [
                (5, [0.0, 0.0, 1.0], 0xff408020),
                (33, [0.0, 0.6, 0.8], 0xff204080),
                (64, [0.0, 0.0, -1.0], 0xff804020),
            ] {
                let mut v = vertex(normal, color);
                if layout.diffuse_offset.is_none() {
                    v.drain(24..28);
                }
                vertices[slot * layout.stride..(slot + 1) * layout.stride].copy_from_slice(&v);
            }
            for format in [101, 102] {
                let indices: Vec<u8> = [999u32, 62, 3, 31, 3, 62, 31]
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
                    stride: layout.stride as u32,
                    base_vertex: 2,
                    min_index: u32::MAX,
                    num_vertices: 0,
                    start_index: 1,
                    primitive_count: 2,
                };
                let (mut packed, mut remapped, mut expanded) = (Vec::new(), Vec::new(), Vec::new());
                packed_indexed_list_into(&mut packed, &mut remapped, &vertices, &indices, draw)
                    .unwrap();
                assert_eq!(remapped, [0, 1, 2, 1, 0, 2]);
                assert_eq!(packed.len(), 3 * layout.stride);
                expand_indexed_into(&mut expanded, &vertices, &indices, draw).unwrap();
                let (mut lit, mut reference) = (Vec::new(), Vec::new());
                state
                    .light_vertices(&packed, 0, 3, layout, &mut lit)
                    .unwrap();
                state
                    .light_vertices(&expanded, 0, 6, layout, &mut reference)
                    .unwrap();
                let unpacked: Vec<u8> = remapped
                    .iter()
                    .flat_map(|i| {
                        lit[*i as usize * LIT_RECORD..(*i as usize + 1) * LIT_RECORD]
                            .iter()
                            .copied()
                    })
                    .collect();
                assert_eq!(unpacked, reference);
                // Invalid reads must reject before overwriting packed bytes.
                let old = packed.clone();
                for bad in [
                    IndexedDraw {
                        base_vertex: u32::MAX,
                        ..draw
                    },
                    IndexedDraw {
                        start_index: u32::MAX,
                        ..draw
                    },
                    IndexedDraw {
                        index_format: 0,
                        ..draw
                    },
                    IndexedDraw { stride: 0, ..draw },
                ] {
                    assert!(
                        packed_indexed_list_into(
                            &mut packed,
                            &mut remapped,
                            &vertices,
                            &indices,
                            bad
                        )
                        .is_err()
                    );
                    assert_eq!(packed, old);
                }
                assert!(
                    packed_indexed_list_into(
                        &mut packed,
                        &mut remapped,
                        &vertices,
                        &indices[..1],
                        draw
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn reused_scratch_truncates_and_matches_a_fresh_lighting() {
        let state = directional();
        let mut scratch = Vec::new();
        // Fill the scratch with two vertices first, then light one vertex into
        // the same buffer. The result must equal a fresh one-vertex pass and
        // must not expose the previous second vertex.
        let two = [
            vertex([0.0, 0.0, 1.0], 0x80402010),
            vertex([0.0, 0.0, -1.0], 0xff00ff00),
        ]
        .concat();
        state
            .light_vertices(&two, 0, 2, LitInput::XYZ_NORMAL_DIFFUSE_TEX1, &mut scratch)
            .unwrap();
        assert_eq!(scratch.len(), 2 * LIT_RECORD);
        let one = vertex([0.0, 0.0, -1.0], 0xff00ff00);
        state
            .light_vertices(&one, 0, 1, LitInput::XYZ_NORMAL_DIFFUSE_TEX1, &mut scratch)
            .unwrap();
        let mut fresh = Vec::new();
        state
            .light_vertices(&one, 0, 1, LitInput::XYZ_NORMAL_DIFFUSE_TEX1, &mut fresh)
            .unwrap();
        assert_eq!(scratch, fresh);
        assert_eq!(scratch.len(), LIT_RECORD);
    }

    #[test]
    fn nonzero_start_packs_only_requested_vertices() {
        let state = directional();
        let selected = vertex([0.0, 0.0, 1.0], 0x80402010);
        // An invalid normal in the unused prefix must never be evaluated.
        let bytes = [vertex([f32::NAN; 3], 0), selected.clone()].concat();
        let mut packed = vec![0xff; 108];
        state
            .light_vertices(&bytes, 1, 2, LitInput::XYZ_NORMAL_DIFFUSE_TEX1, &mut packed)
            .unwrap();
        let mut fresh = Vec::new();
        state
            .light_vertices(
                &selected,
                0,
                1,
                LitInput::XYZ_NORMAL_DIFFUSE_TEX1,
                &mut fresh,
            )
            .unwrap();
        assert_eq!(packed, fresh);
        assert_eq!(packed.len(), LIT_RECORD);
    }

    #[test]
    fn xyz_normal_tex1_uses_material_and_copies_uv() {
        // 0x112 has no COLOR1, so its diffuse must equal the material diffuse
        // (default opaque white), which the 0x152 helper can express with a
        // white vertex colour.
        let state = directional();
        let mut out152 = Vec::new();
        state
            .light_vertices(
                &vertex([0.0, 0.0, 1.0], 0xffff_ffff),
                0,
                1,
                LitInput::XYZ_NORMAL_DIFFUSE_TEX1,
                &mut out152,
            )
            .unwrap();
        let mut out112 = Vec::new();
        state
            .light_vertices(
                &vertex_xyz_normal_tex1([0.0, 0.0, 1.0], [0.25, 0.5]),
                0,
                1,
                LitInput::XYZ_NORMAL_TEX1,
                &mut out112,
            )
            .unwrap();
        assert_eq!(out112.len(), LIT_RECORD);
        assert_eq!(&out112[..12], &out152[..12]);
        assert_eq!(&out112[12..28], &out152[12..28]);
        let uv = [0.25f32, 0.5]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(&out112[28..36], uv.as_slice());
    }

    #[test]
    fn directional_front_back_and_diffuse_alpha() {
        let state = directional();
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0x80402010),
            d3dcolor_to_rgba(0x80402010)
        );
        assert_eq!(
            lit_color(&state, [0.0, 0.0, -1.0], 0x80402010),
            [0.0, 0.0, 0.0, 128.0 / 255.0]
        );
    }
    #[test]
    fn normal_inverse_transpose_and_normalization() {
        let mut state = directional();
        state.world = Mat4::scale(1.0, 1.0, 4.0);
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0xffffffff),
            [0.25, 0.25, 0.25, 1.0]
        );
        state.set_render_state(143, 1).unwrap();
        assert_eq!(lit_color(&state, [0.0, 0.0, 1.0], 0xffffffff), [1.0; 4]);
        state.view = Mat4::translation(10.0, 20.0, 30.0);
        assert_eq!(lit_color(&state, [0.0, 0.0, 1.0], 0xffffffff), [1.0; 4]);
    }
    #[test]
    fn material_sources_ambient_emissive_and_disabled_lights() {
        let mut state = directional();
        state.light_enable(0, false).unwrap();
        state.material.ambient = [0.5; 4];
        state.material.emissive = [0.125; 4];
        state.states.ambient = 0xffffffff;
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0x80ffffff),
            [0.625, 0.625, 0.625, 128.0 / 255.0]
        );
        state.states.color_vertex = false;
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0),
            [0.625, 0.625, 0.625, 1.0]
        );
        state.states.emissive_material_source = D3DMATERIALCOLORSOURCE::Color2;
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0),
            [0.625, 0.625, 0.625, 1.0]
        );
    }
    #[test]
    fn point_attenuation_range_and_spot_cone() {
        let mut state = directional();
        state.lights[0] = Light {
            light_type: 1,
            position: [0.0, 0.0, 2.5],
            range: 3.0,
            attenuation1: 1.0,
            diffuse: [1.0; 4],
            ..Light::default()
        };
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0xffffffff),
            [0.5, 0.5, 0.5, 1.0]
        );
        state.lights[0].range = 1.0;
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0xffffffff),
            [0.0, 0.0, 0.0, 1.0]
        );
        state.lights[0].range = 3.0;
        state.lights[0].light_type = 2;
        state.lights[0].direction = [0.0, 0.0, -1.0];
        state.lights[0].theta = 0.5;
        state.lights[0].phi = 1.0;
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0xffffffff),
            [0.5, 0.5, 0.5, 1.0]
        );
        state.lights[0].direction = [0.0, 0.0, 1.0];
        assert_eq!(
            lit_color(&state, [0.0, 0.0, 1.0], 0xffffffff),
            [0.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn process_vertices_writes_specular_only_when_enabled() {
        let mut state = directional();
        state.material.specular = [1.0, 1.0, 1.0, 1.0];
        state.material.power = 1.0;
        state.lights[0].specular = [1.0; 4];
        // Off-axis vertex: the eye is not opposite the light, so the halfway
        // vector is not the normal and the specular term is nonzero.
        let source = vertex_at_xyz_normal_tex1([3.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
        let mut dest = vec![0u8; 32];
        state
            .process_vertices(
                &source,
                0,
                1,
                LitInput::XYZ_NORMAL_TEX1,
                0x01c4,
                0,
                &mut dest,
            )
            .unwrap();
        assert_eq!(dest[20..24], [0, 0, 0, 0]);
        state.set_render_state(29, 1).unwrap(); // D3DRS_SPECULARENABLE
        state
            .process_vertices(
                &source,
                0,
                1,
                LitInput::XYZ_NORMAL_TEX1,
                0x01c4,
                0,
                &mut dest,
            )
            .unwrap();
        assert!(dest[20] > 0, "specular bytes {:?}", &dest[20..24]);
    }

    #[test]
    fn process_vertices_transforms_and_packs_xyzrhw() {
        let mut state = directional();
        // Identity world/view/projection and a full viewport with a nonzero
        // origin exercises the screen mapping and rhw without a projection.
        state.viewport = Viewport {
            x: 10,
            y: 20,
            width: 200,
            height: 100,
            min_z: 0.0,
            max_z: 1.0,
        };
        let source = vertex_xyz_normal_tex1([0.0, 0.0, 1.0], [0.25, 0.5]);
        let mut dest = vec![0u8; 32];
        state
            .process_vertices(
                &source,
                0,
                1,
                LitInput::XYZ_NORMAL_TEX1,
                0x01c4,
                0,
                &mut dest,
            )
            .unwrap();
        let f = |o: usize| f32::from_le_bytes(dest[o..o + 4].try_into().unwrap());
        // NDC (0,0,0) maps to the viewport centre.
        assert_eq!(f(0), 110.0);
        assert_eq!(f(4), 70.0);
        assert_eq!(f(8), 0.5);
        assert_eq!(f(12), 1.0);
        // Diffuse from the directional light, specular disabled -> 0.
        assert_eq!(
            dest[16..20],
            rgba_to_d3dcolor([1.0, 1.0, 1.0, 1.0]).to_le_bytes()
        );
        assert_eq!(dest[20..24], [0, 0, 0, 0]);
        assert_eq!(dest[24..32], {
            let mut uv = Vec::new();
            for v in [0.25f32, 0.5] {
                uv.extend(v.to_le_bytes());
            }
            uv
        });
    }

    #[test]
    fn process_vertices_divides_by_clip_w() {
        let mut state = directional();
        // A perspective-style projection: row 3 carries no w contribution and
        // row 2's last column feeds clip.w, so clip.w is the view-space z.
        let mut projection = Mat4::IDENTITY;
        projection.rows[2] = [0.0, 0.0, 1.0, 1.0];
        projection.rows[3] = [0.0, 0.0, 0.0, 0.0];
        state.projection = projection;
        state.viewport = Viewport {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            min_z: 0.0,
            max_z: 1.0,
        };
        let source = vertex_at_xyz_normal_tex1([0.0, 0.0, 2.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
        let mut dest = vec![0u8; 32];
        state
            .process_vertices(
                &source,
                0,
                1,
                LitInput::XYZ_NORMAL_TEX1,
                0x01c4,
                0,
                &mut dest,
            )
            .unwrap();
        let f = |o: usize| f32::from_le_bytes(dest[o..o + 4].try_into().unwrap());
        // clip = (0, 0, 2, 2), rhw = 0.5, ndc = (0, 0, 1).
        assert_eq!(f(0), 50.0);
        assert_eq!(f(4), 50.0);
        assert_eq!(f(8), 1.0);
        assert_eq!(f(12), 0.5);
    }

    #[test]
    fn process_vertices_keeps_sign_of_rhw_behind_the_eye() {
        let mut state = directional();
        let mut projection = Mat4::IDENTITY;
        projection.rows[2] = [0.0, 0.0, 1.0, 1.0];
        projection.rows[3] = [0.0, 0.0, 0.0, 0.0];
        state.projection = projection;
        state.viewport = Viewport {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            min_z: 0.0,
            max_z: 1.0,
        };
        // Vertex at view z = -2: clip = (2, 0, -2, -2), so rhw = -0.5 and
        // ndc = (-1, 0, 1). The XYZRHW shader rebuilds clip = ndc * (1/rhw).
        let source = vertex_at_xyz_normal_tex1([2.0, 0.0, -2.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
        let mut dest = vec![0u8; 32];
        state
            .process_vertices(
                &source,
                0,
                1,
                LitInput::XYZ_NORMAL_TEX1,
                0x01c4,
                0,
                &mut dest,
            )
            .unwrap();
        let f = |o: usize| f32::from_le_bytes(dest[o..o + 4].try_into().unwrap());
        let (sx, sy, sz, rhw) = (f(0), f(4), f(8), f(12));
        assert_eq!(rhw, -0.5);
        let w = 1.0 / rhw;
        let ndc_x = 2.0 * sx / 100.0 - 1.0;
        let ndc_y = 1.0 - 2.0 * sy / 100.0;
        assert_eq!((ndc_x * w, ndc_y * w, sz * w, w), (2.0, 0.0, -2.0, -2.0));
    }

    #[test]
    fn process_vertices_rejects_unknown_formats_and_ranges() {
        let state = directional();
        let source = vertex_xyz_normal_tex1([0.0, 0.0, 1.0], [0.0, 0.0]);
        let mut dest = vec![0u8; 32];
        assert!(
            state
                .process_vertices(
                    &source,
                    0,
                    1,
                    LitInput::XYZ_NORMAL_TEX1,
                    0x0042,
                    0,
                    &mut dest
                )
                .is_err()
        );
        assert!(LitInput::from_fvf(0x0042).is_err());
        // One vertex does not fit in a destination smaller than its stride.
        assert!(
            state
                .process_vertices(
                    &source,
                    0,
                    1,
                    LitInput::XYZ_NORMAL_TEX1,
                    0x01c4,
                    0,
                    &mut vec![0u8; 16],
                )
                .is_err()
        );
        // The source range must stay inside the passed stream.
        assert!(
            state
                .process_vertices(
                    &source,
                    1,
                    1,
                    LitInput::XYZ_NORMAL_TEX1,
                    0x01c4,
                    0,
                    &mut dest,
                )
                .is_err()
        );
    }

    #[test]
    fn process_vertices_bakes_stage_texture_transforms_into_destination_sets() {
        use crate::d3d8::enums::D3DTEXTURESTAGESTATETYPE as Ts;
        let mut state = directional();
        // Stage 0: COUNT2 with a per-axis scale.
        let mut scale0 = Mat4::IDENTITY;
        scale0.rows[0][0] = 2.0;
        scale0.rows[1][1] = 4.0;
        state
            .set_texture_stage_state(0, Ts::TextureTransformFlags.raw(), 2)
            .unwrap();
        state.set_transform(16, scale0).unwrap(); // D3DTS_TEXTURE0
        // Stage 1: the guest's detail tiling, COUNT2 with a 46x uniform scale.
        let mut scale1 = Mat4::IDENTITY;
        scale1.rows[0][0] = 46.0;
        scale1.rows[1][1] = 46.0;
        state
            .set_texture_stage_state(1, Ts::TextureTransformFlags.raw(), 2)
            .unwrap();
        state
            .set_texture_stage_state(1, Ts::TexCoordIndex.raw(), 0)
            .unwrap();
        state.set_transform(17, scale1).unwrap(); // D3DTS_TEXTURE1
        let source = vertex_xyz_normal_tex1([0.0, 0.0, 1.0], [0.25, 0.5]);
        let mut dest = vec![0u8; 40];
        state
            .process_vertices(
                &source,
                0,
                1,
                LitInput::XYZ_NORMAL_TEX1,
                0x02c4,
                0,
                &mut dest,
            )
            .unwrap();
        let f = |o: usize| f32::from_le_bytes(dest[o..o + 4].try_into().unwrap());
        // Destination set N is texture stage N, transformed at process time.
        assert_eq!([f(24), f(28)], [0.5, 2.0]);
        assert_eq!([f(32), f(36)], [11.5, 23.0]);
        // The matrices are consumed by ProcessVertices, so they must remain
        // in device state for the reprocessing case; the draw path drops them
        // only for the pre-transformed draw itself (tested in fixed_function).
        assert_eq!(state.texture_transforms[1].rows[0][0], 46.0);
    }
}
