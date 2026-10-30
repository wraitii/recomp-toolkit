//! FVF decoding and bounded fixed-function pipeline construction.
//!
//! XYZ/DIFFUSE FVFs 0x42, 0x142 and 0x242 feed unlit raster shaders.
//! XYZ/NORMAL/TEX1 (0x112) and XYZ/NORMAL/DIFFUSE/TEX1 (0x152) feed GPU
//! diffuse lighting, with CPU comparison/fallback; 0x112 has no
//! per-vertex diffuse, so the material supplies it. Other layouts fail with a
//! named diagnostic.
//!
//! # Matrix convention
//!
//! CPU [`Mat4`] values are row-major and use `v * world * view * projection`
//! (translation in the last row). WGSL's `mat4x4<f32>` treats an array of four
//! `vec4<f32>` values as its **columns**, and the shader computes
//! `matrix * column_vector`. Storing the composed CPU rows contiguously (the
//! natural `bytemuck` cast) therefore yields the WGSL matrix that is the
//! transpose of the CPU matrix, which is exactly what is required:
//!
//! ```text
//! M_wgsl = composed_cpu^T   =>   M_wgsl * v_col = (v_row * composed_cpu)^T
//! ```
//!
//! No explicit transpose is performed.

use crate::RenderError;
use crate::d3d8::math::Mat4;
use bytemuck::{Pod, Zeroable};

/// `D3DFVF_XYZ | D3DFVF_DIFFUSE`; the unlit, untextured vertex layout.
pub const FVF_XYZ_DIFFUSE: u32 = 0x0042;

/// `D3DFVF_XYZ | D3DFVF_DIFFUSE | D3DFVF_TEX1`; adds one 2-float texture
/// coordinate at offset 16. The zero-based set feeds `TEXCOORDINDEX` 0; a stage
/// that selects the missing set 1 samples `(0, 0)` (see [`texcoord_available`]).
pub const FVF_XYZ_DIFFUSE_TEX1: u32 = 0x0142;

/// `D3DFVF_XYZ | D3DFVF_DIFFUSE | D3DFVF_TEX2`; two 2-float texture-coordinate
/// sets at offsets 16 and 24 (stride 32). The survey's second draw class uses
/// this FVF with stage 0 on texcoord set 0 and stage 1 disabled, so the second
/// set is carried by the layout but unused. A stage that selects set 1 reads it
/// directly.
pub const FVF_XYZ_DIFFUSE_TEX2: u32 = 0x0242;

/// `D3DFVF_XYZ | D3DFVF_NORMAL | D3DFVF_TEX1` (0x0112); one 2-float texture
/// coordinate at offset 24 and no per-vertex diffuse. The normal at offset 12
/// feeds the same vertex lighting as `0x152`; with lighting disabled
/// the material supplies the diffuse because there is no COLOR1 component.
pub const FVF_XYZ_NORMAL_TEX1: u32 = 0x0112;

/// `D3DFVF_XYZRHW | D3DFVF_DIFFUSE | D3DFVF_SPECULAR | D3DFVF_TEX1` (0x1C4).
/// The D3D7 front end's logo and fixed-function path submits these already
/// transformed (screen-space `x,y`, depth `z`) vertices, so no world/view/
/// projection transform is applied to them (the pre-transformed entry point).
pub const FVF_XYZRHW_DIFFUSE_SPECULAR_TEX1: u32 = 0x01C4;

/// `D3DFVF_XYZRHW | D3DFVF_DIFFUSE | D3DFVF_SPECULAR | D3DFVF_TEX2` (0x2C4).
/// The D3D7 sprite/text path's two-texture pre-transformed layout.
pub const FVF_XYZRHW_DIFFUSE_SPECULAR_TEX2: u32 = 0x02C4;

/// A decoded FVF: stride plus the matching WGSL vertex attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FvfLayout {
    /// Bytes between consecutive vertices (wgpu `array_stride`).
    pub stride: u64,
    /// Number of texture-coordinate sets the FVF carries (0, 1 or 2).
    pub texcoord_sets: u32,
    /// True for `XYZRHW`: the vertex position is already in screen space, so
    /// the draw uses the pre-transformed vertex entry point instead of the
    /// world/view/projection path.
    pub pre_transformed: bool,
    /// Vertex attributes at their byte offsets.
    pub attributes: Vec<wgpu::VertexAttribute>,
}

impl FvfLayout {
    /// Decode a raw `D3DFVF_*` value into a vertex layout.
    ///
    /// [`FVF_XYZ_DIFFUSE`] (`0x42`) and [`FVF_XYZ_DIFFUSE_TEX1`] (`0x142`) are
    /// accepted; the latter carries an extra 2-float texture coordinate that
    /// the untextured shader leaves unused. Any other value returns
    /// [`RenderError`].
    pub fn decode(raw: u32) -> Result<Self, RenderError> {
        match raw {
            FVF_XYZ_DIFFUSE => Ok(Self {
                stride: 16,
                texcoord_sets: 0,
                pre_transformed: false,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x3,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 12,
                        shader_location: 1,
                    },
                ],
            }),
            FVF_XYZ_DIFFUSE_TEX1 => Ok(Self {
                stride: 24,
                texcoord_sets: 1,
                pre_transformed: false,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x3,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 12,
                        shader_location: 1,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 16,
                        shader_location: 2,
                    },
                    // A single-set FVF exposes set 1 as an alias of set 0 so
                    // the one two-stage shader always has a `location(3)`
                    // input. A stage that selects the missing set 1 samples
                    // `(0, 0)` and ignores this aliased value.
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 16,
                        shader_location: 3,
                    },
                ],
            }),
            FVF_XYZ_DIFFUSE_TEX2 => Ok(Self {
                stride: 32,
                texcoord_sets: 2,
                pre_transformed: false,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x3,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 12,
                        shader_location: 1,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 16,
                        shader_location: 2,
                    },
                    // Texcoord set 1: read by a stage that selects
                    // `TEXCOORDINDEX` 1.
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 24,
                        shader_location: 3,
                    },
                ],
            }),
            FVF_XYZ_NORMAL_TEX1 => Ok(Self {
                stride: 32,
                texcoord_sets: 1,
                pre_transformed: false,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x3,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 24,
                        shader_location: 2,
                    },
                    // Single-set FVF: set 1 aliases set 0, as in 0x142.
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 24,
                        shader_location: 3,
                    },
                ],
            }),
            0x152 => Ok(Self {
                stride: 36,
                texcoord_sets: 1,
                pre_transformed: false,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x3,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 24,
                        shader_location: 1,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 28,
                        shader_location: 2,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 28,
                        shader_location: 3,
                    },
                ],
            }),
            // XYZRHW | DIFFUSE: screen-space color-only vertices. No texture
            // or specular inputs may be read beyond the 20-byte record.
            0x44 => Ok(Self {
                stride: 20,
                texcoord_sets: 0,
                pre_transformed: true,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 16,
                        shader_location: 1,
                    },
                ],
            }),
            // XYZRHW | DIFFUSE | TEX1: no specular word is present. A separate
            // vertex entry supplies zero specular without reading beyond the
            // 28-byte record or aliasing the diffuse/texture fields.
            0x144 => Ok(Self {
                stride: 28,
                texcoord_sets: 1,
                pre_transformed: true,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 16,
                        shader_location: 1,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 20,
                        shader_location: 2,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 20,
                        shader_location: 3,
                    },
                ],
            }),
            // XYZRHW | DIFFUSE | SPECULAR | TEX1. The pre-transformed path
            // passes the 4-float screen-space position and the specular colour
            // to `vs_rhw_main`; the fragment shader adds specular when
            // D3DRS_SPECULARENABLE is set.
            FVF_XYZRHW_DIFFUSE_SPECULAR_TEX1 => Ok(Self {
                stride: 32,
                texcoord_sets: 1,
                pre_transformed: true,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 16,
                        shader_location: 1,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 24,
                        shader_location: 2,
                    },
                    // Set 1 aliases set 0, as in the single-set XYZ layout.
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 24,
                        shader_location: 3,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 20,
                        shader_location: 4,
                    },
                ],
            }),
            FVF_XYZRHW_DIFFUSE_SPECULAR_TEX2 => Ok(Self {
                stride: 40,
                texcoord_sets: 2,
                pre_transformed: true,
                attributes: vec![
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 0,
                        shader_location: 0,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 16,
                        shader_location: 1,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 24,
                        shader_location: 2,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 32,
                        shader_location: 3,
                    },
                    wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Uint32,
                        offset: 20,
                        shader_location: 4,
                    },
                ],
            }),
            _ => Err(RenderError::new(
                "FvfLayout::decode",
                format!(
                    "unsupported FVF 0x{raw:08X}; only D3DFVF_XYZ | D3DFVF_DIFFUSE (0x{FVF_XYZ_DIFFUSE:08X}), D3DFVF_XYZ | D3DFVF_DIFFUSE | D3DFVF_TEX1 (0x{FVF_XYZ_DIFFUSE_TEX1:08X}), D3DFVF_XYZ | D3DFVF_DIFFUSE | D3DFVF_TEX2 (0x{FVF_XYZ_DIFFUSE_TEX2:08X}), XYZ | NORMAL | TEX1 (0x00000112), XYZ | NORMAL | DIFFUSE | TEX1 (0x00000152) and XYZRHW | DIFFUSE | TEX1 (0x00000144) or with SPECULAR | TEX1/2 (0x000001C4/0x000002C4) are implemented"
                ),
            )),
        }
    }

    /// The wgpu vertex-buffer layout for this FVF.
    pub fn vertex_buffer_layout(&self) -> wgpu::VertexBufferLayout<'_> {
        wgpu::VertexBufferLayout {
            array_stride: self.stride,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &self.attributes,
        }
    }
}

/// Decode a D3DCOLOR DWORD to normalized `[r, g, b, a]` floats in `[0, 1]`.
///
/// D3DCOLOR packs alpha in the high byte: `ARGB`. This is the CPU reference for
/// the decode performed by [`UNLIT_WGSL`]'s vertex stage.
pub fn d3dcolor_to_rgba(dword: u32) -> [f32; 4] {
    [
        ((dword >> 16) & 0xff) as f32 / 255.0,
        ((dword >> 8) & 0xff) as f32 / 255.0,
        (dword & 0xff) as f32 / 255.0,
        ((dword >> 24) & 0xff) as f32 / 255.0,
    ]
}

/// Group 0 binding 0 uniform: the composed `world * view * projection` matrix.
///
/// `matrix` stores the CPU rows contiguously, which WGSL interprets as columns.
/// See the module-level note on the matrix convention.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct TransformUniform {
    /// Composed transform, CPU rows stored as WGSL columns.
    pub matrix: [[f32; 4]; 4],
    /// Viewport `(x, y, width, height)` in pixels. Only the pre-transformed
    /// (`XYZRHW`) entry point reads it; the transformed path ignores it.
    pub viewport: [f32; 4],
    /// Packed flags in a 16-byte-aligned vec4 so the WGSL uniform layout
    /// matches this Rust struct byte for byte: `rhw[0]` is the pre-transformed
    /// (XYZRHW) flag, `rhw[1]` is `D3DRS_SPECULARENABLE` as 0/1. The rest are
    /// zero.
    pub rhw: [u32; 4],
}

impl TransformUniform {
    /// Compose `world * view * projection` using D3D row-vector semantics and
    /// pack it for the WGSL uniform. `viewport`/`rhw` are set by the draw path
    /// before upload.
    pub fn new(world: Mat4, view: Mat4, projection: Mat4) -> Self {
        Self {
            matrix: world.mul(view).mul(projection).rows,
            viewport: [0.0; 4],
            rhw: [0; 4],
        }
    }
}

/// Unlit XYZ + diffuse WGSL: `vs_main` samples the D3DCOLOR at location 1,
/// `fs_main` returns the decoded color.
///
/// Position input is `vec3<f32>` at location 0; color input is the raw
/// `u32` D3DCOLOR at location 1. The uniform is bound at group 0, binding 0.
pub const UNLIT_WGSL: &str = r#"
struct TransformUniform {
    matrix: mat4x4<f32>,
    viewport: vec4<f32>,
    rhw: vec4<u32>,
};

struct FogUniform {
    enabled: u32,
    mode: u32,
    vertex_fog: u32,
    range_fog: u32,
    start: f32,
    end: f32,
    density: f32,
    color_r: f32,
    color_g: f32,
    color_b: f32,
    color_a: f32,
    pad2: f32,
    world_view: mat4x4<f32>,
};

struct AlphaTestUniform {
    enabled: u32,
    func: u32,
    reference: u32,
    pad0: u32,
};

@group(0) @binding(0)
var<uniform> transform: TransformUniform;
@group(0) @binding(2)
var<uniform> fog: FogUniform;
@group(0) @binding(3)
var<uniform> alpha_test: AlphaTestUniform;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) color: u32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(linear) color: vec4<f32>,
    @location(1) fogdist: f32,
    @location(2) fogfactor: f32,
    @location(4) @interpolate(linear) specular: vec3<f32>,
};

// D3D8 table/pixel fog uses the eye-space depth. A standard D3D perspective
// projection has a fourth row of (0, 0, 1, 0) (or (0, 0, -1, 0)), so clip.w is
// the view-space z; abs() makes it positive for either handedness. It is not
// the normalized z/w depth (see FogUniform's Rust documentation).
@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    // Column vector * transposed CPU matrix equals the row-vector CPU product.
    out.position = transform.matrix * vec4<f32>(in.position, 1.0);
    // Eye distance: the view-space z (clip w), or with D3DRS_RANGEFOGENABLE the
    // true Euclidean distance to the eye. Vertex fog evaluates the factor here
    // and the rasterizer interpolates it; table fog evaluates per pixel.
    var fog_d = abs(out.position.w);
    if (fog.range_fog != 0u) {
        fog_d = length((fog.world_view * vec4<f32>(in.position, 1.0)).xyz);
    }
    out.fogdist = fog_d;
    out.fogfactor = select(1.0, fog_factor(fog_d), fog.vertex_fog != 0u);

    // D3DCOLOR is ARGB; decode to normalized RGBA.
    let a = f32((in.color >> 24u) & 0xffu) / 255.0;
    let r = f32((in.color >> 16u) & 0xffu) / 255.0;
    let g = f32((in.color >> 8u) & 0xffu) / 255.0;
    let b = f32(in.color & 0xffu) / 255.0;
    out.color = vec4<f32>(r, g, b, a);
    out.specular = vec3<f32>(0.0);
    return out;
}

struct VertexInputRhw {
    @location(0) position: vec4<f32>,
    @location(1) color: u32,
    @location(4) specular: u32,
};

// XYZRHW vertices carry a screen-space x,y (pixels) and a depth z that is
// already in the viewport's depth range. The rasterizer maps NDC through the
// viewport (with the half-pixel offset the draw path sets), so invert that
// mapping here and let the rasterizer do the rest. The reciprocal-w field is
// used as `w = 1/rhw` when nonzero; the observed D3D7 logo quad carries
// `rhw = 0.0` and was an orthographic full-screen quad, so that keeps `w = 1`.
fn rhw_vertex(in: VertexInputRhw) -> VertexOutput {
    var out: VertexOutput;
    let vp = transform.viewport;
    let ndc_x = 2.0 * (in.position.x - vp.x) / vp.z - 1.0;
    let ndc_y = 1.0 - 2.0 * (in.position.y - vp.y) / vp.w;
    // rhw != 0: w = 1/rhw, so the rasterizer's perspective division and
    // perspective-correct texture interpolation reproduce D3D's pre-transformed
    // path. rhw == 0 keeps the orthographic w = 1 (the D3D7 logo quad).
    // DIVERGENCE(original): a negative rhw (a vertex behind the eye, as written
    // by `ProcessVertices`, which keeps the sign of 1/clip.w) keeps its sign, so
    // `ndc * w` rebuilds the exact clip-space position and the rasterizer clips
    // triangles that straddle the eye plane against the near plane. Forcing
    // `w = 1` for rhw <= 0 mirrored those vertices to enormous screen positions
    // (terrain "curtains"). The original runtime's behaviour for such triangles
    // is not evidenced; this is the geometric clip the game's terrain needs.
    var w = 1.0;
    if (in.position.w != 0.0) {
        w = 1.0 / in.position.w;
    }
    out.position = vec4<f32>(ndc_x * w, ndc_y * w, in.position.z * w, w);
    out.fogdist = in.position.z;
    out.fogfactor = f32(in.specular >> 24u) / 255.0;
    let a = f32((in.color >> 24u) & 0xffu) / 255.0;
    let r = f32((in.color >> 16u) & 0xffu) / 255.0;
    let g = f32((in.color >> 8u) & 0xffu) / 255.0;
    let b = f32(in.color & 0xffu) / 255.0;
    out.color = vec4<f32>(r, g, b, a);
    out.specular = vec3<f32>(
        f32((in.specular >> 16u) & 0xffu) / 255.0,
        f32((in.specular >> 8u) & 0xffu) / 255.0,
        f32(in.specular & 0xffu) / 255.0,
    );
    return out;
}

@vertex
fn vs_rhw_main(in: VertexInputRhw) -> VertexOutput {
    return rhw_vertex(in);
}

struct VertexInputRhwNoSpecular {
    @location(0) position: vec4<f32>,
    @location(1) color: u32,
};

@vertex
fn vs_rhw_nospec_main(in: VertexInputRhwNoSpecular) -> VertexOutput {
    return rhw_vertex(VertexInputRhw(in.position, in.color, 0u));
}

// D3DFOG_* table fog factor. `d` is the eye-space depth. The result is
// clamped to [0,1]; the fog alpha is left untouched.
fn fog_factor(d: f32) -> f32 {
    var f = 1.0;
    switch fog.mode {
        case 1u: { f = exp(-fog.density * d); }
        case 2u: { f = exp(-(fog.density * d) * (fog.density * d)); }
        case 3u: { f = (fog.end - d) / max(fog.end - fog.start, 1e-6); }
        default: { f = 1.0; }
    }
    return clamp(f, 0.0, 1.0);
}

fn apply_fog(rgb: vec3<f32>, d: f32, vertex_factor: f32) -> vec3<f32> {
    if (fog.enabled == 0u) {
        return rgb;
    }
    var f = vertex_factor;
    if (fog.vertex_fog == 0u) {
        f = fog_factor(d);
    }
    return mix(vec3<f32>(fog.color_r, fog.color_g, fog.color_b), rgb, f);
}

// D3D8 alpha test after stage blending and before fog/alpha blend. Returns
// true when the pixel passes. `reference` is the 8-bit D3DRS_ALPHAREF.
fn alpha_test_pass(alpha: f32) -> bool {
    if (alpha_test.enabled == 0u) {
        return true;
    }
    let a = i32(round(clamp(alpha, 0.0, 1.0) * 255.0));
    let r = i32(alpha_test.reference);
    switch alpha_test.func {
        case 1u: { return false; }    // NEVER
        case 2u: { return a < r; }    // LESS
        case 3u: { return a == r; }   // EQUAL
        case 4u: { return a <= r; }   // LESSEQUAL
        case 5u: { return a > r; }    // GREATER
        case 6u: { return a != r; }   // NOTEQUAL
        case 7u: { return a >= r; }   // GREATEREQUAL
        default: { return true; }     // ALWAYS
    }
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if (!alpha_test_pass(in.color.a)) {
        discard;
    }
    // D3DRS_SPECULARENABLE adds the vertex specular to the RGB after texture
    // blending and before fog, clamped. Alpha is unaffected.
    var rgb = in.color.rgb;
    if (transform.rhw[1] != 0u) {
        rgb = clamp(rgb + in.specular, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return vec4<f32>(apply_fog(rgb, in.fogdist, in.fogfactor), in.color.a);
}
"#;

/// Group 0 binding 1 uniform: the resolved stage-0 fixed-function state.
///
/// The fields are the raw D3D8 op/argument values; the WGSL fragment stage
/// interprets them with the same bounded subset the CPU validator enforced.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct StageUniform {
    /// `D3DTS_TEXTUREx` applied when `tex_transform_flags != 0`; identity
    /// otherwise. Stored as CPU rows (WGSL columns bound to `matrix`).
    pub tex_transform: [[f32; 4]; 4],
    pub color_op: u32,
    pub color_arg1: u32,
    pub color_arg2: u32,
    pub alpha_op: u32,
    pub alpha_arg1: u32,
    pub alpha_arg2: u32,
    /// `D3DRS_TEXTUREFACTOR` (raw ARGB `D3DCOLOR`) for `D3DTA_TFACTOR`.
    pub texture_factor: u32,
    /// Nonzero when the stage combines; zero keeps `CURRENT`.
    pub active: u32,
    /// `D3DTSS_TEXCOORDINDEX`: 0 or 1.
    pub tex_coord_index: u32,
    /// Resolved coordinate generation: [`crate::d3d8::state::TEXCOORD_GEN_PASSTHRU`]
    /// or [`crate::d3d8::state::TEXCOORD_GEN_CAMERASPACEPOSITION`]. The latter
    /// ignores `tex_coord_index`/`tex_coord_available` and derives the
    /// coordinate from the camera-space vertex position.
    pub tex_coord_gen: u32,
    /// Nonzero when the FVF carries the set `tex_coord_index` selected. Zero
    /// makes the shader sample `(0, 0)`, matching WineD3D and DXVK for an
    /// undeclared set.
    pub tex_coord_available: u32,
    /// `D3DTSS_TEXTURETRANSFORMFLAGS`: 0 disabled, 2 (`D3DTTFF_COUNT2`)
    /// applies `tex_transform` to the selected vertex coordinate, `0x103`
    /// (`D3DTTFF_COUNT3 | D3DTTFF_PROJECTED`) applies it to the generated
    /// camera-space position and divides by the third output component
    /// (`COUNT3 | PROJECTED` divides by the component `COUNT` selects).
    pub tex_transform_flags: u32,
    /// `D3DTSS_MIPMAPLODBIAS` as an `f32`. Applied with `textureSampleBias`
    /// because wgpu 27 samplers have no `lod_bias` field.
    pub lod_bias: f32,
    /// Padding that keeps the struct a 16-byte multiple. The WGSL
    /// `StageUniform` has matching scalar pad fields so the host and shader
    /// layouts agree byte for byte (`wgpu` validates this at pipeline
    /// creation).
    pub pad: [u32; 3],
}

impl StageUniform {
    /// Pack a resolved [`crate::d3d8::state::TextureStage`].
    pub fn new(stage: &crate::d3d8::state::TextureStage) -> Self {
        Self {
            tex_transform: stage.tex_transform.rows,
            color_op: stage.color_op,
            color_arg1: stage.color_arg1,
            color_arg2: stage.color_arg2,
            alpha_op: stage.alpha_op,
            alpha_arg1: stage.alpha_arg1,
            alpha_arg2: stage.alpha_arg2,
            texture_factor: stage.texture_factor,
            active: u32::from(stage.active),
            tex_coord_index: stage.tex_coord_index,
            tex_coord_gen: stage.tex_coord_gen,
            // `Device::draw_primitive` overrides this from the FVF with
            // `StagesUniform::for_fvf`; the standalone default is available.
            tex_coord_available: 1,
            tex_transform_flags: stage.tex_transform_flags,
            lod_bias: stage.lod_bias,
            pad: [0; 3],
        }
    }
}

/// Group 0 binding 1 uniform for the two-stage textured shader: stage 0 then
/// stage 1, in order. `StagesUniform` keeps the array a single 16-byte-aligned
/// binding so the shader indexes it with a constant.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct StagesUniform {
    pub stages: [StageUniform; 2],
}

impl StagesUniform {
    /// Pack both resolved stages (stage 0 first), treating every selected
    /// texture-coordinate set as present. Unit tests and CPU helpers use this;
    /// the draw path uses [`Self::for_fvf`].
    pub fn new(
        stage0: &crate::d3d8::state::TextureStage,
        stage1: &crate::d3d8::state::TextureStage,
    ) -> Self {
        Self {
            stages: [StageUniform::new(stage0), StageUniform::new(stage1)],
        }
    }

    /// Pack both resolved stages for an FVF carrying `texcoord_sets` texture
    /// coordinate sets. A stage that selects a set the FVF does not carry is
    /// marked unavailable, so the fragment shader samples `(0, 0)`.
    ///
    /// Evidence: real D3D8 leaves an undeclared coordinate set to the vertex
    /// shader's default. WineD3D's fixed-function pixel shader (`gen_ffp_pshader`
    /// in `dlls/wined3d/ffp_hlsl.c`) emits `texcoord[i] = 0.0` when the vertex
    /// stage never wrote that coordinate, and DXVK's fixed-function vertex
    /// shader (`loadTexcoord` / `transformTexCoord` in
    /// `src/d3d9/shaders/d3d9_fixed_function_vert.vert`) resolves an undeclared
    /// set to zero. The original runtime's documented behavior is not in the
    /// repository; these are the D3D8/D3D9 implementations this project uses as
    /// its reference, so the bridge follows them rather than failing the draw.
    pub fn for_fvf(
        stage0: &crate::d3d8::state::TextureStage,
        stage1: &crate::d3d8::state::TextureStage,
        texcoord_sets: u32,
    ) -> Self {
        let mut stages = [StageUniform::new(stage0), StageUniform::new(stage1)];
        for (uniform, stage) in stages.iter_mut().zip([stage0, stage1].iter()) {
            uniform.tex_coord_available =
                u32::from(texcoord_available(stage.tex_coord_index, texcoord_sets));
        }
        Self { stages }
    }

    /// Pack both resolved stages for a pre-transformed (`XYZRHW`) draw.
    ///
    /// D3D8 does not transform the texture coordinates of pre-transformed
    /// vertices (`Texture Coordinate Transformations`: "Direct3D does not
    /// modify transformed and lit vertices"); `ProcessVertices` already baked
    /// each stage's transform into the destination coordinate. The shader
    /// therefore sees a disabled transform, so the draw cannot apply it a
    /// second time.
    pub fn for_pre_transformed(
        stage0: &crate::d3d8::state::TextureStage,
        stage1: &crate::d3d8::state::TextureStage,
        texcoord_sets: u32,
    ) -> Self {
        let mut uniform = Self::for_fvf(stage0, stage1, texcoord_sets);
        for stage in &mut uniform.stages {
            stage.tex_transform = Mat4::IDENTITY.rows;
            stage.tex_transform_flags = 0;
            // D3D8 does not generate texture coordinates for pre-transformed
            // vertices, so an XYZRHW draw samples the vertex coordinate even
            // when the stage asked for camera-space generation.
            stage.tex_coord_gen = crate::d3d8::state::TEXCOORD_GEN_PASSTHRU;
        }
        uniform
    }
}

/// True when an FVF carrying `texcoord_sets` coordinate sets contains the set
/// `tex_coord_index` selected by `D3DTSS_TEXCOORDINDEX`.
pub fn texcoord_available(tex_coord_index: u32, texcoord_sets: u32) -> bool {
    tex_coord_index < texcoord_sets
}

/// CPU reference for the WGSL `transform_texcoord` COUNT2 path.
///
/// D3D's fixed-function pipeline expands a COUNT2 texture coordinate to
/// `(u, v, 1, 0)` before applying the texture matrix: the first component not
/// supplied by the vertex is padded to 1, the remaining components to 0. WineD3D
/// documents this in `compute_texture_matrix` (`dlls/wined3d/utils.c`) and
/// `d3d9/tests/visual.c::test_texture_transform_flags` constructs the same
/// `(..., 1, 0)` input for `D3DTTFF_COUNT2`; DXVK's `transformTexCoord`
/// (`src/d3d9/shaders/d3d9_fixed_function_vert.vert`) pads the same component.
/// Under D3D's row-vector convention `v * M` the translation therefore comes
/// from the matrix's third row (`_31`/`_32`), not the fourth (`_41`/`_42`).
///
/// The WGSL function must stay in sync; `count2_texcoord_expands_to_u_v_1_0`
/// checks both the CPU result and the shader source.
pub fn transform_texcoord(uv: [f32; 2], m: &Mat4, flags: u32) -> [f32; 2] {
    if flags == 0 {
        return uv;
    }
    let out = m.transform([uv[0], uv[1], 1.0, 0.0]);
    [out[0], out[1]]
}

/// CPU reference for the WGSL `camera_space_texcoord` path.
///
/// `cam_pos` is the camera-space vertex position `view * world * position`
/// with `w = 1`. D3D8's `D3DTSS_TCI_CAMERASPACEPOSITION` feeds that 4-vector to
/// the stage's `D3DTS_TEXTUREx` matrix; the game uses
/// `D3DTTFF_COUNT3 | D3DTTFF_PROJECTED` (`0x103`). D3D's projected divide uses
/// the last component the count selects, so with COUNT3 the matrix output's
/// third component (`q`) divides the first two; its fourth component is not
/// used. (A COUNT4 projected stage would divide by `w`; that is not resolved.)
/// Applying the matrix to the interpolated position and dividing per pixel is
/// equivalent to D3D's per-vertex transform plus perspective-correct
/// interpolation of the projected coordinates.
///
/// The WGSL function must stay in sync; `camera_space_texcoord_matches_count3_projected`
/// checks both the CPU result and the shader source.
pub fn camera_space_texcoord(cam_pos: [f32; 4], m: &Mat4, flags: u32) -> [f32; 2] {
    debug_assert_eq!(flags, 0x103, "only COUNT3 | PROJECTED is resolved");
    let out = m.transform(cam_pos);
    [out[0] / out[2], out[1] / out[2]]
}

/// Group 0 binding 2 uniform: the resolved D3D8 table/pixel fog state.
///
/// `FOGSTART`/`FOGEND`/`FOGDENSITY` are stored as the raw DWORD bit patterns
/// the guest set (D3D8 render states are `DWORD`s holding a `float`).
///
/// # Fog distance
///
/// Table fog selects its factor from the eye-space depth of the fragment. This
/// bridge takes that depth from `abs(clip.w)`: the guest's projection is a
/// standard D3D perspective matrix whose fourth row is `(0, 0, 1, 0)` (or
/// `(0, 0, -1, 0)` for a right-handed variant), so `clip.w` is the view-space
/// z and `abs(clip.w)` is the eye distance for either handedness. It is not
/// the normalized `z/w` depth: the guest sets FOGSTART=190 and FOGEND=240
/// (survey `win4.log`), values that only make sense as eye-space distances.
/// This matches the project's established D3D9 fixed-function reference
/// (`graphics/d3d8-wgpu/src/host_gpu/shaders.rs`, `d3d_render.cpp`), which
/// computes `v_fogdist = abs(pos.w)` and documents it as the eye distance.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct FogUniform {
    /// `D3DRS_FOGENABLE` as 0/1.
    pub enable: u32,
    /// Raw `D3DFOGMODE`: 0 NONE, 1 EXP, 2 EXP2, 3 LINEAR.
    pub mode: u32,
    /// Nonzero for per-vertex fog (`D3DRS_FOGVERTEXMODE`, used when
    /// `D3DRS_FOGTABLEMODE` is NONE): the factor is computed per vertex and
    /// interpolated. Zero is per-pixel table fog.
    pub vertex_fog: u32,
    /// Nonzero for `D3DRS_RANGEFOGENABLE`: fog distance is the Euclidean eye
    /// distance instead of the view-space depth.
    pub range_fog: u32,
    pub start: f32,
    pub end: f32,
    pub density: f32,
    pub color_r: f32,
    pub color_g: f32,
    pub color_b: f32,
    pub color_a: f32,
    pub pad2: f32,
    /// `world * view` with CPU rows stored as WGSL columns, for range fog.
    pub world_view: [[f32; 4]; 4],
}

impl FogUniform {
    /// Pack the current device fog state. `color` is the already-adjusted
    /// `[r, g, b, a]` fog colour (D3D8 forces it to black for additive
    /// blending); the device owns that decision.
    pub fn new(
        enable: bool,
        mode: u32,
        vertex_fog: bool,
        range_fog: bool,
        start: u32,
        end: u32,
        density: u32,
        color: [f32; 4],
        world_view: Mat4,
    ) -> Self {
        Self {
            enable: u32::from(enable),
            mode,
            vertex_fog: u32::from(vertex_fog),
            range_fog: u32::from(range_fog),
            start: f32::from_bits(start),
            end: f32::from_bits(end),
            density: f32::from_bits(density),
            color_r: color[0],
            color_g: color[1],
            color_b: color[2],
            color_a: color[3],
            pad2: 0.0,
            world_view: world_view.rows,
        }
    }
}

/// Group 0 binding 3 uniform: the resolved D3D8 alpha-test state.
///
/// D3D8's `D3DRS_ALPHAREF` is an 8-bit reference (0..255); `D3DRS_ALPHAFUNC`
/// is a [`crate::d3d8::enums::D3DCMPFUNC`]. The fragment shader compares the
/// final stage-blended alpha against the reference and discards failing pixels.
/// Fixed-function ordering puts the alpha test after texture blending and
/// before fog/alpha blending, so the shader discards before it applies fog.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct AlphaTestUniform {
    /// `D3DRS_ALPHATESTENABLE` as 0/1.
    pub enable: u32,
    /// Raw `D3DCMPFUNC` (1 NEVER .. 8 ALWAYS).
    pub func: u32,
    /// Raw `D3DRS_ALPHAREF`; validated to 0..=255 before the draw.
    pub reference: u32,
    pub pad0: u32,
}

impl AlphaTestUniform {
    /// Pack the current alpha-test state.
    pub fn new(enable: bool, func: u32, reference: u32) -> Self {
        Self {
            enable: u32::from(enable),
            func,
            reference,
            pad0: 0,
        }
    }
}

/// CPU reference for the WGSL alpha-test predicate. `alpha` is the final
/// stage-blended alpha; it is quantised to the 8-bit value D3D8 compares,
/// then tested against `reference` (0..255) with `func` (raw `D3DCMPFUNC`).
///
/// Returns true when the pixel passes (is kept); false means discard.
/// `NEVER` always fails, `ALWAYS` always passes, and an out-of-range `func`
/// keeps the pixel (the validator rejects it before a draw).
pub fn alpha_test_pass(func: u32, reference: u32, alpha: f32) -> bool {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as i32;
    let r = reference as i32;
    match func {
        1 => false,  // D3DCMP_NEVER
        2 => a < r,  // D3DCMP_LESS
        3 => a == r, // D3DCMP_EQUAL
        4 => a <= r, // D3DCMP_LESSEQUAL
        5 => a > r,  // D3DCMP_GREATER
        6 => a != r, // D3DCMP_NOTEQUAL
        7 => a >= r, // D3DCMP_GREATEREQUAL
        8 => true,   // D3DCMP_ALWAYS
        _ => true,
    }
}

/// Textured XYZ + diffuse + TEX1 WGSL. `vs_main` decodes the D3DCOLOR and
/// passes the texture coordinate through; `fs_main` samples stage 0 and
/// applies the resolved COLOROP/ALPHAOP subset. The sampler and 2D texture
/// are group 1 bindings 0 and 1; the unbound case binds a 1x1 white texture as
/// a fallback, but an unbound stage does not simply sample white. Wine's
/// `is_invalid_op` (dlls/wined3d/utils.c) rewrites a stage op that reads
/// TEXTURE with no texture to SELECTARG1(CURRENT), which
/// `DeviceState::resolve_texture_stage` mirrors; the white fallback is only
/// read by stages whose op does not reference TEXTURE.
pub const TEXTURED_WGSL: &str = r#"
struct TransformUniform {
    matrix: mat4x4<f32>,
    viewport: vec4<f32>,
    rhw: vec4<u32>,
};

struct StageUniform {
    tex_transform: mat4x4<f32>,
    color_op: u32,
    color_arg1: u32,
    color_arg2: u32,
    alpha_op: u32,
    alpha_arg1: u32,
    alpha_arg2: u32,
    texture_factor: u32,
    stage_active: u32,
    tex_coord_index: u32,
    tex_coord_gen: u32,
    tex_coord_available: u32,
    tex_transform_flags: u32,
    lod_bias: f32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
};

struct StagesUniform {
    stages: array<StageUniform, 2>,
};

struct FogUniform {
    enabled: u32,
    mode: u32,
    vertex_fog: u32,
    range_fog: u32,
    start: f32,
    end: f32,
    density: f32,
    color_r: f32,
    color_g: f32,
    color_b: f32,
    color_a: f32,
    pad2: f32,
    world_view: mat4x4<f32>,
};

struct AlphaTestUniform {
    enabled: u32,
    func: u32,
    reference: u32,
    pad0: u32,
};

@group(0) @binding(0)
var<uniform> transform: TransformUniform;
@group(0) @binding(1)
var<uniform> stages: StagesUniform;
@group(0) @binding(2)
var<uniform> fog: FogUniform;
@group(0) @binding(3)
var<uniform> alpha_test: AlphaTestUniform;
@group(1) @binding(0)
var stage0_tex: texture_2d<f32>;
@group(1) @binding(1)
var stage0_sampler: sampler;
@group(1) @binding(2)
var stage1_tex: texture_2d<f32>;
@group(1) @binding(3)
var stage1_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) color: u32,
    @location(2) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(linear) color: vec4<f32>,
    @location(1) uv0: vec2<f32>,
    @location(2) uv1: vec2<f32>,
    @location(3) fogdist: f32,
    @location(4) fogfactor: f32,
    @location(5) @interpolate(linear) specular: vec3<f32>,
    // Camera-space position (`view * world * position`, w = 1) used by
    // D3DTSS_TCI_CAMERASPACEPOSITION stages. Interpolated perspective-correct
    // so the fragment stage can apply the projective texture matrix there.
    @location(6) cam_pos: vec4<f32>,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.position = transform.matrix * vec4<f32>(in.position, 1.0);
    // Eye distance: the view-space z (clip w), or with D3DRS_RANGEFOGENABLE the
    // true Euclidean distance to the eye. Vertex fog evaluates the factor here
    // and the rasterizer interpolates it; table fog evaluates per pixel.
    var fog_d = abs(out.position.w);
    if (fog.range_fog != 0u) {
        fog_d = length((fog.world_view * vec4<f32>(in.position, 1.0)).xyz);
    }
    out.fogdist = fog_d;
    out.fogfactor = select(1.0, fog_factor(fog_d), fog.vertex_fog != 0u);
    // Camera-space position for D3DTSS_TCI_CAMERASPACEPOSITION texgen;
    // `fog.world_view` is `world * view`, so this is `view * world * pos`.
    out.cam_pos = fog.world_view * vec4<f32>(in.position, 1.0);
    let a = f32((in.color >> 24u) & 0xffu) / 255.0;
    let r = f32((in.color >> 16u) & 0xffu) / 255.0;
    let g = f32((in.color >> 8u) & 0xffu) / 255.0;
    let b = f32(in.color & 0xffu) / 255.0;
    out.color = vec4<f32>(r, g, b, a);
    out.uv0 = in.uv0;
    out.uv1 = in.uv1;
    out.specular = vec3<f32>(0.0);
    return out;
}

struct VertexInputRhw {
    @location(0) position: vec4<f32>,
    @location(1) color: u32,
    @location(2) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
    @location(4) specular: u32,
};

// XYZRHW: see the unlit shader for the screen-space mapping. The texture
// coordinates are passed through unchanged.
fn rhw_vertex(in: VertexInputRhw) -> VertexOutput {
    var out: VertexOutput;
    let vp = transform.viewport;
    let ndc_x = 2.0 * (in.position.x - vp.x) / vp.z - 1.0;
    let ndc_y = 1.0 - 2.0 * (in.position.y - vp.y) / vp.w;
    // rhw != 0: w = 1/rhw, so the rasterizer's perspective division and
    // perspective-correct texture interpolation reproduce D3D's pre-transformed
    // path. rhw == 0 keeps the orthographic w = 1 (the D3D7 logo quad).
    // DIVERGENCE(original): a negative rhw (a vertex behind the eye, as written
    // by `ProcessVertices`, which keeps the sign of 1/clip.w) keeps its sign, so
    // `ndc * w` rebuilds the exact clip-space position and the rasterizer clips
    // triangles that straddle the eye plane against the near plane. Forcing
    // `w = 1` for rhw <= 0 mirrored those vertices to enormous screen positions
    // (terrain "curtains"). The original runtime's behaviour for such triangles
    // is not evidenced; this is the geometric clip the game's terrain needs.
    var w = 1.0;
    if (in.position.w != 0.0) {
        w = 1.0 / in.position.w;
    }
    out.position = vec4<f32>(ndc_x * w, ndc_y * w, in.position.z * w, w);
    out.fogdist = in.position.z;
    out.fogfactor = f32(in.specular >> 24u) / 255.0;
    // Pre-transformed draws never generate texture coordinates; the entry
    // point is shared, so leave an unused value.
    out.cam_pos = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let a = f32((in.color >> 24u) & 0xffu) / 255.0;
    let r = f32((in.color >> 16u) & 0xffu) / 255.0;
    let g = f32((in.color >> 8u) & 0xffu) / 255.0;
    let b = f32(in.color & 0xffu) / 255.0;
    out.color = vec4<f32>(r, g, b, a);
    out.uv0 = in.uv0;
    out.uv1 = in.uv1;
    out.specular = vec3<f32>(
        f32((in.specular >> 16u) & 0xffu) / 255.0,
        f32((in.specular >> 8u) & 0xffu) / 255.0,
        f32(in.specular & 0xffu) / 255.0,
    );
    return out;
}

@vertex
fn vs_rhw_main(in: VertexInputRhw) -> VertexOutput {
    return rhw_vertex(in);
}

struct VertexInputRhwNoSpecular {
    @location(0) position: vec4<f32>,
    @location(1) color: u32,
    @location(2) uv0: vec2<f32>,
    @location(3) uv1: vec2<f32>,
};

@vertex
fn vs_rhw_nospec_main(in: VertexInputRhwNoSpecular) -> VertexOutput {
    return rhw_vertex(VertexInputRhw(in.position, in.color, in.uv0, in.uv1, 0u));
}

struct VertexInputRhwColor {
    @location(0) position: vec4<f32>,
    @location(1) color: u32,
};

// This layout has no guest coordinate or specular fields. Bound textures and
// color-combine stages still run with the missing coordinates supplied as zero.
@vertex
fn vs_rhw_color_main(in: VertexInputRhwColor) -> VertexOutput {
    return rhw_vertex(VertexInputRhw(in.position, in.color, vec2<f32>(0.0), vec2<f32>(0.0), 0u));
}

// D3DTA_*: low nibble 0 DIFFUSE, 1 CURRENT, 2 TEXTURE, 3 TFACTOR; bit 0x10 is
// D3DTA_COMPLEMENT and bit 0x20 is D3DTA_ALPHAREPLICATE.
fn stage_arg(
    selector: u32,
    diffuse: vec4<f32>,
    current: vec4<f32>,
    texel: vec4<f32>,
    tfactor: vec4<f32>,
) -> vec4<f32> {
    var v: vec4<f32>;
    switch selector & 0xfu {
        case 0u: { v = diffuse; }
        case 1u: { v = current; }
        case 2u: { v = texel; }
        case 3u: { v = tfactor; }
        default: { v = vec4<f32>(1.0, 1.0, 1.0, 1.0); }
    }
    if ((selector & 0x10u) != 0u) { v = vec4<f32>(1.0, 1.0, 1.0, 1.0) - v; }
    if ((selector & 0x20u) != 0u) { v = vec4<f32>(v.a, v.a, v.a, v.a); }
    return v;
}

fn stage_arg_a(
    selector: u32,
    diffuse: vec4<f32>,
    current: vec4<f32>,
    texel: vec4<f32>,
    tfactor: vec4<f32>,
) -> f32 {
    var v: f32;
    switch selector & 0xfu {
        case 0u: { v = diffuse.a; }
        case 1u: { v = current.a; }
        case 2u: { v = texel.a; }
        case 3u: { v = tfactor.a; }
        default: { v = 1.0; }
    }
    if ((selector & 0x10u) != 0u) { v = 1.0 - v; }
    return v;
}

// WineD3D shader_glsl_ffp_fragment_op: signed components are 2*x-1.
// Alpha components never participate in the three-component dot product.
fn dotproduct3(a: vec4<f32>, b: vec4<f32>) -> f32 {
    return clamp(4.0 * dot(a.rgb - vec3<f32>(0.5), b.rgb - vec3<f32>(0.5)), 0.0, 1.0);
}

// D3DTOP color result on RGB. The blend ops take their factor alpha from the
// source D3D8 names.
fn color_op(
    op: u32,
    a: vec4<f32>,
    b: vec4<f32>,
    current: vec4<f32>,
    diffuse: vec4<f32>,
    texel: vec4<f32>,
    tfactor: vec4<f32>,
) -> vec3<f32> {
    var r: vec3<f32>;
    switch op {
        case 1u: { r = current.rgb; }                 // DISABLE
        case 2u: { r = a.rgb; }                       // SELECTARG1
        case 3u: { r = b.rgb; }                       // SELECTARG2
        case 4u: { r = a.rgb * b.rgb; }               // MODULATE
        case 5u: { r = a.rgb * b.rgb * 2.0; }         // MODULATE2X
        case 6u: { r = a.rgb * b.rgb * 4.0; }         // MODULATE4X
        case 7u: { r = a.rgb + b.rgb; }               // ADD
        case 8u: { r = a.rgb + b.rgb - 0.5; }         // ADDSIGNED
        case 9u: { r = (a.rgb + b.rgb - 0.5) * 2.0; } // ADDSIGNED2X
        case 10u: { r = a.rgb - b.rgb; }              // SUBTRACT
        // BLEND*ALPHA: Arg1 * alpha + Arg2 * (1 - alpha), i.e. mix(Arg2, Arg1, alpha)
        // (Wine glsl_shader.c shader_glsl_ffp_fragment_op).
        case 12u: { r = mix(b.rgb, a.rgb, vec3<f32>(diffuse.a)); }
        case 13u: { r = mix(b.rgb, a.rgb, vec3<f32>(texel.a)); }
        case 14u: { r = mix(b.rgb, a.rgb, vec3<f32>(tfactor.a)); }
        case 16u: { r = mix(b.rgb, a.rgb, vec3<f32>(current.a)); }
        case 24u: { r = vec3<f32>(dotproduct3(a, b)); } // DOTPRODUCT3
        default: { r = current.rgb; }
    }
    return r;
}

fn alpha_op(
    op: u32,
    a: f32,
    b: f32,
    current: vec4<f32>,
    diffuse: vec4<f32>,
    texel: vec4<f32>,
    tfactor: vec4<f32>,
) -> f32 {
    var r: f32;
    switch op {
        case 1u: { r = current.a; }
        case 2u: { r = a; }
        case 3u: { r = b; }
        case 4u: { r = a * b; }
        case 5u: { r = a * b * 2.0; }
        case 6u: { r = a * b * 4.0; }
        case 7u: { r = a + b; }
        case 8u: { r = a + b - 0.5; }
        case 9u: { r = (a + b - 0.5) * 2.0; }
        case 10u: { r = a - b; }
        case 12u: { r = mix(b, a, diffuse.a); }
        case 13u: { r = mix(b, a, texel.a); }
        case 14u: { r = mix(b, a, tfactor.a); }
        case 16u: { r = mix(b, a, current.a); }
        default: { r = current.a; }
    }
    return r;
}

// D3DTTFF_COUNT2: the fixed-function pipeline expands the selected 2D
// coordinate to `(u, v, 1, 0)` before the texture matrix, then keeps the first
// two components. The `1` sits in the first component the vertex does not
// supply, so under the row-vector convention `v * M` the translation comes from
// the matrix's third row (`_31`/`_32`), not the fourth (`_41`/`_42`). See the
// CPU `transform_texcoord` below for the WineD3D/DXVK/D3D-test evidence. The
// shader matrix is the CPU row-major matrix uploaded as WGSL columns, so
// `mat * v` is `v * M`.
fn transform_texcoord(uv: vec2<f32>, m: mat4x4<f32>, flags: u32) -> vec2<f32> {
    if (flags == 0u) {
        return uv;
    }
    return (m * vec4<f32>(uv, 1.0, 0.0)).xy;
}

// D3DTSS_TCI_CAMERASPACEPOSITION with D3DTTFF_COUNT3 | D3DTTFF_PROJECTED: the
// stage input is the camera-space position `(x, y, z, 1)`. The texture matrix
// is applied to the perspective-correct interpolated position and the third
// output component (`q`, the last one COUNT3 selects) divides the first two;
// D3D's projected divide uses the last component the count selects, not `w`.
// Applying the matrix after interpolation is equivalent to D3D's per-vertex
// transform followed by perspective-correct interpolation, and dividing at the
// pixel is the projective texture mapping the fixed-function rasterizer
// performs. See the CPU `camera_space_texcoord` below.
fn camera_space_texcoord(pos: vec4<f32>, m: mat4x4<f32>) -> vec2<f32> {
    let out = m * pos;
    return out.xy / out.z;
}

// Evaluate one stage. `current` is the input current colour (diffuse for
// stage 0, the stage-0 result for stage 1). The result is clamped to [0,1]
// before it feeds the next stage, as D3D8 does.
fn eval_stage(
    s: StageUniform,
    current: vec4<f32>,
    diffuse: vec4<f32>,
    uv0: vec2<f32>,
    uv1: vec2<f32>,
    cam_pos: vec4<f32>,
    tex: texture_2d<f32>,
    samp: sampler,
) -> vec4<f32> {
    if (s.stage_active == 0u) {
        return current;
    }
    var uv = uv0;
    if (s.tex_coord_gen == 1u) {
        uv = camera_space_texcoord(cam_pos, s.tex_transform);
    } else {
        // An FVF that does not carry the selected set leaves the coordinate
        // uninitialized; WineD3D and DXVK both resolve that to (0, 0).
        if (s.tex_coord_index == 1u) {
            uv = select(uv1, vec2<f32>(0.0, 0.0), s.tex_coord_available == 0u);
        }
        uv = transform_texcoord(uv, s.tex_transform, s.tex_transform_flags);
    }
    // `textureSampleBias` requires uniform control flow; this call site is
    // unconditional in `fs_main` and every branch above depends only on the
    // uniform-buffer stage state, so naga's uniformity analysis accepts it.
    // A zero bias is identical to `textureSample`.
    let texel = textureSampleBias(tex, samp, uv, s.lod_bias);
    let tfactor = vec4<f32>(
        f32((s.texture_factor >> 16u) & 0xffu) / 255.0,
        f32((s.texture_factor >> 8u) & 0xffu) / 255.0,
        f32(s.texture_factor & 0xffu) / 255.0,
        f32((s.texture_factor >> 24u) & 0xffu) / 255.0,
    );
    let c1 = stage_arg(s.color_arg1, diffuse, current, texel, tfactor);
    let c2 = stage_arg(s.color_arg2, diffuse, current, texel, tfactor);
    let rgb = color_op(s.color_op, c1, c2, current, diffuse, texel, tfactor);
    let a1 = stage_arg_a(s.alpha_arg1, diffuse, current, texel, tfactor);
    let a2 = stage_arg_a(s.alpha_arg2, diffuse, current, texel, tfactor);
    var alpha = alpha_op(s.alpha_op, a1, a2, current, diffuse, texel, tfactor);
    if (s.color_op == 24u) {
        // WineD3D wined3d_ffp_get_fs_settings: COLOROP DOTPRODUCT3 overrides
        // ALPHAOP and its arguments, replicating the color result to alpha.
        alpha = rgb.x;
    } else if (s.alpha_op == 24u) {
        alpha = dotproduct3(
            stage_arg(s.alpha_arg1, diffuse, current, texel, tfactor),
            stage_arg(s.alpha_arg2, diffuse, current, texel, tfactor));
    }
    return clamp(vec4<f32>(rgb, alpha), vec4<f32>(0.0), vec4<f32>(1.0));
}

// D3DFOG_* table fog factor, using the eye-space depth `d` (clip w).
fn fog_factor(d: f32) -> f32 {
    var f = 1.0;
    switch fog.mode {
        case 1u: { f = exp(-fog.density * d); }
        case 2u: { f = exp(-(fog.density * d) * (fog.density * d)); }
        case 3u: { f = (fog.end - d) / max(fog.end - fog.start, 1e-6); }
        default: { f = 1.0; }
    }
    return clamp(f, 0.0, 1.0);
}

fn apply_fog(rgb: vec3<f32>, d: f32, vertex_factor: f32) -> vec3<f32> {
    if (fog.enabled == 0u) {
        return rgb;
    }
    var f = vertex_factor;
    if (fog.vertex_fog == 0u) {
        f = fog_factor(d);
    }
    return mix(vec3<f32>(fog.color_r, fog.color_g, fog.color_b), rgb, f);
}

// D3D8 alpha test after stage blending and before fog/alpha blend. Returns
// true when the pixel passes. `reference` is the 8-bit D3DRS_ALPHAREF.
fn alpha_test_pass(alpha: f32) -> bool {
    if (alpha_test.enabled == 0u) {
        return true;
    }
    let a = i32(round(clamp(alpha, 0.0, 1.0) * 255.0));
    let r = i32(alpha_test.reference);
    switch alpha_test.func {
        case 1u: { return false; }    // NEVER
        case 2u: { return a < r; }    // LESS
        case 3u: { return a == r; }   // EQUAL
        case 4u: { return a <= r; }   // LESSEQUAL
        case 5u: { return a > r; }    // GREATER
        case 6u: { return a != r; }   // NOTEQUAL
        case 7u: { return a >= r; }   // GREATEREQUAL
        default: { return true; }     // ALWAYS
    }
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let diffuse = in.color;
    let r0 = eval_stage(stages.stages[0], diffuse, diffuse, in.uv0, in.uv1, in.cam_pos, stage0_tex, stage0_sampler);
    let r1 = eval_stage(stages.stages[1], r0, diffuse, in.uv0, in.uv1, in.cam_pos, stage1_tex, stage1_sampler);
    if (!alpha_test_pass(r1.a)) {
        discard;
    }
    // D3DRS_SPECULARENABLE adds the vertex specular to the RGB after texture
    // blending and before fog, clamped. Alpha is unaffected.
    var rgb = r1.rgb;
    if (transform.rhw[1] != 0u) {
        rgb = clamp(rgb + in.specular, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return vec4<f32>(apply_fog(rgb, in.fogdist, in.fogfactor), r1.a);
}
"#;

/// CPU reference for the WGSL `fog_factor` used by both shaders.
///
/// `mode` is a raw `D3DFOGMODE` (0 NONE, 1 EXP, 2 EXP2, 3 LINEAR) and `d` the
/// eye-space fog distance (clip `w`). The factor is clamped to `[0, 1]`: at or
/// beyond FOGEND it is 0 (pure fog colour) and at or before FOGSTART it is 1
/// (unfogged). Mirroring the WGSL here keeps the factor math unit-tested on the
/// CPU while the shader does the per-pixel work.
pub fn fog_factor(mode: u32, start: f32, end: f32, density: f32, d: f32) -> f32 {
    let f = match mode {
        1 => (-density * d).exp(),
        2 => {
            let q = density * d;
            (-(q * q)).exp()
        }
        3 => (end - d) / (end - start).max(1e-6),
        _ => 1.0,
    };
    f.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: [f32; 4], b: [f32; 4]) {
        for i in 0..4 {
            assert!((a[i] - b[i]).abs() < 1e-6, "component {i}: {a:?} != {b:?}");
        }
    }

    /// Treat `m[i]` as the `i`th column (WGSL `mat4x4` memory order) and
    /// compute `m * v`.
    fn wgsl_column_mul(m: &[[f32; 4]; 4], v: [f32; 4]) -> [f32; 4] {
        let mut out = [0.0f32; 4];
        for i in 0..4 {
            for j in 0..4 {
                out[j] += v[i] * m[i][j];
            }
        }
        out
    }

    #[test]
    fn decode_xyz_diffuse_stride_and_offsets() {
        let layout = FvfLayout::decode(FVF_XYZ_DIFFUSE).expect("0x42 must decode");
        assert_eq!(layout.stride, 16);
        assert_eq!(layout.attributes.len(), 2);

        let pos = &layout.attributes[0];
        assert_eq!(pos.format, wgpu::VertexFormat::Float32x3);
        assert_eq!(pos.offset, 0);
        assert_eq!(pos.shader_location, 0);

        let color = &layout.attributes[1];
        assert_eq!(color.format, wgpu::VertexFormat::Uint32);
        assert_eq!(color.offset, 12);
        assert_eq!(color.shader_location, 1);

        let vb = layout.vertex_buffer_layout();
        assert_eq!(vb.array_stride, 16);
        assert_eq!(vb.step_mode, wgpu::VertexStepMode::Vertex);
        assert_eq!(vb.attributes.len(), 2);
    }

    #[test]
    fn decode_xyzrhw_diffuse_tex1_has_no_specular_input() {
        let layout = FvfLayout::decode(0x144).unwrap();
        assert_eq!(layout.stride, 28);
        assert!(layout.pre_transformed);
        assert_eq!(layout.texcoord_sets, 1);
        assert_eq!(
            layout
                .attributes
                .iter()
                .map(|a| (a.shader_location, a.offset))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 16), (2, 20), (3, 20)]
        );
    }

    #[test]
    fn decode_xyzrhw_diffuse_specular_tex1_stride_and_offsets() {
        let layout =
            FvfLayout::decode(FVF_XYZRHW_DIFFUSE_SPECULAR_TEX1).expect("0x1C4 must decode");
        assert_eq!(layout.stride, 32);
        assert_eq!(layout.texcoord_sets, 1);
        assert!(layout.pre_transformed);
        assert_eq!(layout.attributes[0].format, wgpu::VertexFormat::Float32x4);
        assert_eq!(layout.attributes[0].offset, 0);
        assert_eq!(layout.attributes[1].format, wgpu::VertexFormat::Uint32);
        assert_eq!(layout.attributes[1].offset, 16);
        assert_eq!(layout.attributes[2].offset, 24);
        assert_eq!(layout.attributes[3].offset, 24);
        // The specular colour feeds the shader's specular add.
        assert_eq!(layout.attributes[4].format, wgpu::VertexFormat::Uint32);
        assert_eq!(layout.attributes[4].offset, 20);
        assert_eq!(layout.attributes[4].shader_location, 4);
    }

    #[test]
    fn decode_xyzrhw_diffuse_specular_tex2_stride_and_offsets() {
        let layout =
            FvfLayout::decode(FVF_XYZRHW_DIFFUSE_SPECULAR_TEX2).expect("0x2C4 must decode");
        assert_eq!(layout.stride, 40);
        assert_eq!(layout.texcoord_sets, 2);
        assert!(layout.pre_transformed);
        assert_eq!(layout.attributes[0].format, wgpu::VertexFormat::Float32x4);
        assert_eq!(layout.attributes[2].offset, 24);
        assert_eq!(layout.attributes[3].offset, 32);
        assert_eq!(layout.attributes[4].offset, 20);
        assert_eq!(layout.attributes[4].shader_location, 4);
    }

    #[test]
    fn all_shader_sources_parse_and_validate() {
        // `lit_shader_source` rewrites the D3DCOLOR vertex input to a float
        // diffuse; this catches a rewrite that corrupts the pre-transformed
        // entry point or the specular additions, neither of which the runtime
        // logo path exercises yet. naga is the same parser wgpu uses.
        use naga::valid::{Capabilities, ValidationFlags, Validator};
        let validate = |name: &str, source: &str| {
            let module = naga::front::wgsl::parse_str(source)
                .unwrap_or_else(|e| panic!("{name} WGSL parse failed: {e}"));
            let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
            validator
                .validate(&module)
                .unwrap_or_else(|e| panic!("{name} WGSL validation failed: {e:?}"));
        };
        validate("unlit", UNLIT_WGSL);
        validate("textured", TEXTURED_WGSL);
        validate("lit-untextured", &lit_shader_source(false));
        validate("lit-textured", &lit_shader_source(true));
        for fvf in [0x112, 0x152] {
            for textured in [false, true] {
                validate("GPU-lit", &gpu_lit_shader_source(textured, fvf));
            }
        }
    }

    #[test]
    fn decode_rejects_all_unsupported_fvf_values() {
        let unsupported = [
            0x0000,          // nothing
            0x0002,          // XYZ only
            0x0040,          // DIFFUSE only
            0x0052,          // XYZ | NORMAL | DIFFUSE
            0x0082,          // XYZ | SPECULAR
            0x0042 | 0x0020, // + PSIZE
            0x0042 | 0x0010, // + NORMAL
            0x0F42,          // + 15 texcoord-size bits / trailing fields
            0x0042 | 0x1000, // + LASTBETA_UBYTE4
            0x0042 | 0x2000, // reserved2 bit
            0x0001,          // reserved0
            0xFFFF_FFFF,
        ];
        for raw in unsupported {
            assert!(
                FvfLayout::decode(raw).is_err(),
                "FVF 0x{raw:08X} must be rejected"
            );
        }
    }

    #[test]
    fn decode_xyz_diffuse_tex1_stride_and_offsets() {
        let layout = FvfLayout::decode(FVF_XYZ_DIFFUSE_TEX1).expect("0x142 must decode");
        assert_eq!(layout.stride, 24);
        assert_eq!(layout.texcoord_sets, 1);
        assert_eq!(layout.attributes.len(), 4);
        assert_eq!(layout.attributes[2].format, wgpu::VertexFormat::Float32x2);
        assert_eq!(layout.attributes[2].offset, 16);
        assert_eq!(layout.attributes[2].shader_location, 2);
        // Set 1 is exposed as an alias of set 0 so the shared two-stage shader
        // always has a location(3) input.
        assert_eq!(layout.attributes[3].format, wgpu::VertexFormat::Float32x2);
        assert_eq!(layout.attributes[3].offset, 16);
        assert_eq!(layout.attributes[3].shader_location, 3);
    }

    #[test]
    fn decode_xyz_diffuse_tex2_stride_and_offsets() {
        let layout = FvfLayout::decode(FVF_XYZ_DIFFUSE_TEX2).expect("0x242 must decode");
        assert_eq!(layout.stride, 32);
        assert_eq!(layout.texcoord_sets, 2);
        assert_eq!(layout.attributes.len(), 4);
        assert_eq!(layout.attributes[2].format, wgpu::VertexFormat::Float32x2);
        assert_eq!(layout.attributes[2].offset, 16);
        assert_eq!(layout.attributes[2].shader_location, 2);
        // Second texcoord set is carried exactly and read by a stage that
        // selects `TEXCOORDINDEX` 1.
        assert_eq!(layout.attributes[3].format, wgpu::VertexFormat::Float32x2);
        assert_eq!(layout.attributes[3].offset, 24);
        assert_eq!(layout.attributes[3].shader_location, 3);
        assert_eq!(layout.vertex_buffer_layout().array_stride, 32);
    }

    #[test]
    fn decode_xyz_normal_tex1_stride_and_offsets() {
        let layout = FvfLayout::decode(FVF_XYZ_NORMAL_TEX1).expect("0x112 must decode");
        assert_eq!(layout.stride, 32);
        assert_eq!(layout.texcoord_sets, 1);
        assert!(!layout.pre_transformed);
        assert_eq!(layout.attributes.len(), 3);
        assert_eq!(layout.attributes[0].format, wgpu::VertexFormat::Float32x3);
        assert_eq!(layout.attributes[0].offset, 0);
        // No diffuse attribute: lighting synthesises the float diffuse.
        assert_eq!(layout.attributes[1].shader_location, 2);
        assert_eq!(layout.attributes[1].format, wgpu::VertexFormat::Float32x2);
        assert_eq!(layout.attributes[1].offset, 24);
        // Set 1 aliases set 0 for the shared two-stage shader.
        assert_eq!(layout.attributes[2].shader_location, 3);
        assert_eq!(layout.attributes[2].offset, 24);
        assert_eq!(layout.vertex_buffer_layout().array_stride, 32);
    }

    #[test]
    fn alpha_test_compares_eight_bit_alpha_against_reference() {
        // 0.25 * 255 = 63.75 -> 64; reference 0x80 = 128.
        assert!(!alpha_test_pass(7, 0x80, 0.25)); // GREATEREQUAL fails
        assert!(alpha_test_pass(2, 0x80, 0.25)); // LESS passes
        assert!(alpha_test_pass(3, 64, 0.25)); // EQUAL passes on quantised alpha
        assert!(alpha_test_pass(4, 64, 0.25)); // LESSEQUAL passes
        assert!(!alpha_test_pass(5, 64, 0.25)); // GREATER fails
        assert!(!alpha_test_pass(6, 64, 0.25)); // NOTEQUAL fails
        assert!(alpha_test_pass(7, 64, 0.25)); // GREATEREQUAL passes
        assert!(!alpha_test_pass(1, 0, 1.0)); // NEVER always fails
        assert!(alpha_test_pass(8, 255, 0.0)); // ALWAYS always passes
        assert!(alpha_test_pass(8, 0, 0.0)); // ALWAYS ignores the reference
        // Alpha is clamped before quantisation.
        assert!(alpha_test_pass(7, 255, 2.0));
        assert!(!alpha_test_pass(5, 0, -1.0));
    }

    #[test]
    fn d3dcolor_channels_are_argb() {
        // 0xAARRGGBB: alpha high, then red, green, blue.
        let rgba = d3dcolor_to_rgba(0xFF_11_22_33);
        assert_close(
            rgba,
            [
                0x11 as f32 / 255.0,
                0x22 as f32 / 255.0,
                0x33 as f32 / 255.0,
                1.0,
            ],
        );
        assert_close(d3dcolor_to_rgba(0x00_00_00_00), [0.0, 0.0, 0.0, 0.0]);
        assert_close(
            d3dcolor_to_rgba(0x80_FF_00_01),
            [1.0, 0.0, 1.0 / 255.0, 0x80 as f32 / 255.0],
        );
    }

    #[test]
    fn composed_uniform_matches_row_vector_semantics() {
        // Non-symmetric cyclic 3x3 with translation in the last row.
        let world = Mat4 {
            rows: [
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                [5.0, 6.0, 7.0, 1.0],
            ],
        };
        let view = Mat4 {
            rows: [
                [2.0, 0.0, 0.0, 0.0],
                [0.0, 3.0, 0.0, 0.0],
                [0.0, 0.0, 4.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
        };
        let projection = Mat4::IDENTITY;

        // Build the composition from copies so the test does not depend on
        // `Mat4: Copy` (only `[[f32; 4]; 4]` is copied here).
        let composed = Mat4 { rows: world.rows }
            .mul(Mat4 { rows: view.rows })
            .mul(Mat4 {
                rows: projection.rows,
            });
        let uniform = TransformUniform::new(world, view, projection);

        for p in [
            [1.0f32, 2.0, 3.0, 1.0],
            [0.0, 0.0, 0.0, 1.0],
            [-1.0, 0.5, 4.0, 1.0],
        ] {
            let expected = Mat4 {
                rows: composed.rows,
            }
            .transform(p);
            let got = wgsl_column_mul(&uniform.matrix, p);
            assert_close(got, expected);
        }

        // The uniform is the matrix (4 vec4) plus viewport and rhw (2 vec4).
        assert_eq!(bytemuck::bytes_of(&uniform).len(), 96);
    }

    #[test]
    fn transform_order_is_world_then_view_then_projection() {
        // v * world * view: (1,0,0,1) scaled by 2 then translated by 1 => 3.
        let world = Mat4 {
            rows: [
                [2.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
        };
        let view = Mat4 {
            rows: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0, 1.0],
            ],
        };
        let composed = Mat4 { rows: world.rows }.mul(Mat4 { rows: view.rows });
        let p = [1.0f32, 0.0, 0.0, 1.0];
        assert_eq!(
            Mat4 {
                rows: composed.rows
            }
            .transform(p)[0],
            3.0
        );

        let uniform = TransformUniform::new(world, view, Mat4::IDENTITY);
        assert_eq!(wgsl_column_mul(&uniform.matrix, p)[0], 3.0);
    }

    #[test]
    fn linear_fog_factor_is_one_before_start_zero_after_end() {
        // start 190, end 240 matches the guest's training-map fog.
        assert_eq!(fog_factor(3, 190.0, 240.0, 1.0, 100.0), 1.0);
        assert_eq!(fog_factor(3, 190.0, 240.0, 1.0, 190.0), 1.0);
        assert_eq!(fog_factor(3, 190.0, 240.0, 1.0, 240.0), 0.0);
        assert_eq!(fog_factor(3, 190.0, 240.0, 1.0, 300.0), 0.0);
        // Halfway is a half mix.
        assert!((fog_factor(3, 190.0, 240.0, 1.0, 215.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn exp_fog_factors_match_the_reference_formula() {
        let d = 2.0f32;
        let density = 0.5f32;
        assert!((fog_factor(1, 0.0, 1.0, density, d) - (-1.0f32).exp()).abs() < 1e-6);
        assert!((fog_factor(2, 0.0, 1.0, density, d) - (-1.0f32).exp()).abs() < 1e-6);
        // A zero density is no fog at all for both exponential modes.
        assert_eq!(fog_factor(1, 0.0, 1.0, 0.0, d), 1.0);
        assert_eq!(fog_factor(2, 0.0, 1.0, 0.0, d), 1.0);
        // FOG_NONE is a no-op even when a distance is supplied.
        assert_eq!(fog_factor(0, 0.0, 1.0, 1.0, d), 1.0);
    }

    #[test]
    fn fog_uniform_converts_raw_float_bits() {
        let u = FogUniform::new(
            true,
            3,
            true,
            true,
            190.0f32.to_bits(),
            240.0f32.to_bits(),
            1.0f32.to_bits(),
            [0.7, 0.7, 0.7, 1.0],
            Mat4::IDENTITY,
        );
        assert_eq!(u.enable, 1);
        assert_eq!((u.vertex_fog, u.range_fog), (1, 1));
        assert_eq!(u.mode, 3);
        assert_eq!(u.start, 190.0);
        assert_eq!(u.end, 240.0);
        assert_eq!(u.density, 1.0);
        assert_eq!(u.color_r, 0.7);
        assert_eq!(bytemuck::bytes_of(&u).len(), 112);
    }

    #[test]
    fn stage_uniform_carries_texture_factor() {
        let stage = crate::d3d8::state::TextureStage {
            active: true,
            color_op: 4,   // MODULATE
            color_arg1: 3, // TFACTOR
            color_arg2: 2, // TEXTURE
            alpha_op: 3,   // SELECTARG2
            alpha_arg1: 2, // TEXTURE
            alpha_arg2: 3, // TFACTOR
            texture_factor: 0x4080_c020,
            tex_coord_index: 0,
            tex_coord_gen: 0,
            tex_transform_flags: 0,
            tex_transform: crate::d3d8::math::Mat4::IDENTITY,
            min_filter: 2,
            mag_filter: 2,
            mip_filter: 0,
            max_mip_level: 0,
            lod_bias: 0.8,
            address_u: 1,
            address_v: 1,
        };
        let u = StageUniform::new(&stage);
        assert_eq!(u.texture_factor, 0x4080_c020);
        assert_eq!(u.color_arg1, 3);
        assert_eq!(u.color_arg2, 2);
        assert_eq!(u.alpha_arg2, 3);
        assert_eq!(u.active, 1);
        assert_eq!(u.lod_bias, 0.8);
        // Texture matrix, six op/arg words, texture factor, active,
        // coordinate index/generation/availability, transform flags, the LOD
        // bias and the trailing padding.
        assert_eq!(bytemuck::bytes_of(&u).len(), 128);
        // std140-style layout: the bias follows the transform flags. wgpu
        // checks this host layout against the WGSL struct at pipeline creation.
        assert_eq!(&bytemuck::bytes_of(&u)[112..116], &0.8f32.to_le_bytes());
        let both = StagesUniform::new(&stage, &stage);
        assert_eq!(bytemuck::bytes_of(&both).len(), 256);
        // The shader must sample with the bias, not plain `textureSample`.
        assert!(TEXTURED_WGSL.contains("textureSampleBias(tex, samp, uv, s.lod_bias)"));
    }

    #[test]
    fn count2_texcoord_expands_to_u_v_1_0() {
        // Translation in the fourth row (`_41`/`_42`, where `Mat4::translation`
        // and D3DXMatrixTranslation put it) must not shift a COUNT2 coordinate:
        // D3D pads the input to `(u, v, 1, 0)`, so the fourth row is unused.
        let mut translation = Mat4::IDENTITY;
        translation.rows[3][0] = 10.0;
        translation.rows[3][1] = 20.0;
        assert_eq!(
            transform_texcoord([0.25, 0.5], &translation, 2),
            [0.25, 0.5]
        );

        // The third row (`_31`/`_32`) is where the COUNT2 translation lands.
        let mut third = Mat4::IDENTITY;
        third.rows[2][0] = 10.0;
        third.rows[2][1] = 20.0;
        assert_eq!(transform_texcoord([0.25, 0.5], &third, 2), [10.25, 20.5]);

        // Scale still applies from the diagonal, and a disabled transform is a
        // pass-through even when a matrix is stored.
        let mut scale = Mat4::IDENTITY;
        scale.rows[0][0] = 2.0;
        scale.rows[1][1] = 4.0;
        assert_eq!(transform_texcoord([0.25, 0.5], &scale, 2), [0.5, 2.0]);
        assert_eq!(transform_texcoord([0.25, 0.5], &third, 0), [0.25, 0.5]);

        // Guard against the WGSL mirror drifting back to the shader-style
        // `(u, v, 0, 1)` padding.
        assert!(
            TEXTURED_WGSL.contains("vec4<f32>(uv, 1.0, 0.0)"),
            "COUNT2 must pad with (..., 1, 0)"
        );
        assert!(!TEXTURED_WGSL.contains("vec4<f32>(uv, 0.0, 1.0)"));
    }

    #[test]
    fn missing_texcoord_set_is_marked_unavailable_for_zero_sampling() {
        let stage = |tex_coord_index| crate::d3d8::state::TextureStage {
            active: true,
            color_op: 2,   // SELECTARG1
            color_arg1: 2, // TEXTURE
            color_arg2: 1, // CURRENT
            alpha_op: 1,   // DISABLE
            alpha_arg1: 2, // TEXTURE
            alpha_arg2: 1, // CURRENT
            texture_factor: 0,
            tex_coord_index,
            tex_coord_gen: 0,
            tex_transform_flags: 0,
            tex_transform: crate::d3d8::math::Mat4::IDENTITY,
            min_filter: 2,
            mag_filter: 2,
            mip_filter: 0,
            max_mip_level: 0,
            lod_bias: 0.0,
            address_u: 1,
            address_v: 1,
        };
        assert!(!texcoord_available(0, 0));
        assert!(texcoord_available(0, 1));
        assert!(!texcoord_available(1, 1));
        assert!(texcoord_available(1, 2));
        // FVF 0x142 carries only set 0: a stage 1 that selects set 1 is
        // unavailable and samples (0, 0) rather than aborting or aliasing set 0.
        let stages = StagesUniform::for_fvf(&stage(0), &stage(1), 1);
        assert_eq!(stages.stages[0].tex_coord_available, 1);
        assert_eq!(stages.stages[1].tex_coord_index, 1);
        assert_eq!(stages.stages[1].tex_coord_available, 0);
        // FVF 0x242 carries set 1, so the same stage state is available.
        let stages = StagesUniform::for_fvf(&stage(0), &stage(1), 2);
        assert_eq!(stages.stages[1].tex_coord_available, 1);
        assert_eq!(bytemuck::bytes_of(&stages).len(), 256);
    }

    #[test]
    fn pre_transformed_draw_disables_texture_transforms() {
        // A COUNT2 matrix that a normal draw would apply to a vertex coordinate.
        let mut matrix = Mat4::IDENTITY;
        matrix.rows[0][0] = 46.0;
        matrix.rows[1][1] = 46.0;
        let stage = crate::d3d8::state::TextureStage {
            active: true,
            color_op: 8,   // ADDSIGNED
            color_arg1: 2, // TEXTURE
            color_arg2: 1, // CURRENT
            alpha_op: 1,   // DISABLE
            alpha_arg1: 2, // TEXTURE
            alpha_arg2: 1, // CURRENT
            texture_factor: 0,
            tex_coord_index: 1,
            tex_coord_gen: crate::d3d8::state::TEXCOORD_GEN_PASSTHRU,
            tex_transform_flags: 2,
            tex_transform: matrix,
            min_filter: 2,
            mag_filter: 2,
            mip_filter: 2,
            max_mip_level: 0,
            lod_bias: 0.0,
            address_u: 1,
            address_v: 1,
        };
        // A world (non-RHW) draw keeps the matrix so the shader applies it.
        let world = StagesUniform::for_fvf(&stage, &stage, 2);
        assert_eq!(world.stages[0].tex_transform_flags, 2);
        assert_eq!(world.stages[0].tex_transform[0][0], 46.0);
        // A pre-transformed (XYZRHW) draw drops it: ProcessVertices already
        // baked the coordinate.
        let rhw = StagesUniform::for_pre_transformed(&stage, &stage, 2);
        for uniform in &rhw.stages {
            assert_eq!(uniform.tex_transform_flags, 0);
            assert_eq!(uniform.tex_transform, Mat4::IDENTITY.rows);
            assert_eq!(
                uniform.tex_coord_gen,
                crate::d3d8::state::TEXCOORD_GEN_PASSTHRU
            );
        }
        // A camera-space stage is disabled the same way: D3D8 does not generate
        // coordinates for pre-transformed vertices, so the vertex coordinate is
        // sampled instead of the overlay projection.
        let generated = crate::d3d8::state::TextureStage {
            tex_coord_gen: crate::d3d8::state::TEXCOORD_GEN_CAMERASPACEPOSITION,
            tex_transform_flags: 0x103,
            ..stage
        };
        let rhw = StagesUniform::for_pre_transformed(&generated, &generated, 2);
        for uniform in &rhw.stages {
            assert_eq!(
                uniform.tex_coord_gen,
                crate::d3d8::state::TEXCOORD_GEN_PASSTHRU
            );
            assert_eq!(uniform.tex_transform_flags, 0);
        }
    }

    #[test]
    fn camera_space_texcoord_matches_count3_projected() {
        use crate::d3d8::state::{TEXCOORD_GEN_CAMERASPACEPOSITION, TEXCOORD_GEN_PASSTHRU};
        // Camera space for an object at the origin under a view that scales by
        // 2 and translates by z = 4. `world_view * pos` maps (1, 2, 3, 1) to
        // (2, 4, 10, 1).
        let world = Mat4::translation(0.0, 0.0, 0.0);
        let view = Mat4::scale(2.0, 2.0, 2.0).mul(Mat4::translation(0.0, 0.0, 4.0));
        let world_view = world.mul(view);
        let cam = world_view.transform([1.0, 2.0, 3.0, 1.0]);
        assert_eq!(cam, [2.0, 4.0, 10.0, 1.0]);
        // The terrain overlay's matrix: scale x by 0.435, y by 0.85, pass z
        // through (it is the COUNT3 divisor q) and offset y by the fourth row
        // (camera-space w = 1). The fourth output (0.5 * z + 1) is unused.
        let mut m = Mat4::IDENTITY;
        m.rows[0][0] = 0.435;
        m.rows[1][1] = 0.85;
        m.rows[2][2] = 1.0;
        m.rows[2][3] = 0.5;
        m.rows[3][1] = 0.25;
        let out = camera_space_texcoord(cam, &m, 0x103);
        // (0.435 * 2, 0.85 * 4 + 0.25) / q, with q = z = 10.
        assert!((out[0] - (0.435 * 2.0) / 10.0).abs() < 1e-6);
        assert!((out[1] - (0.85 * 4.0 + 0.25) / 10.0).abs() < 1e-6);
        // The passthru CPU helper must not be used for generated coordinates;
        // the shader takes the camera-space branch on the generation word.
        assert_eq!(TEXCOORD_GEN_CAMERASPACEPOSITION, 1);
        assert_eq!(TEXCOORD_GEN_PASSTHRU, 0);
        assert!(TEXTURED_WGSL.contains("fn camera_space_texcoord(pos: vec4<f32>, m: mat4x4<f32>)"));
        assert!(TEXTURED_WGSL.contains("if (s.tex_coord_gen == 1u)"));
        assert!(TEXTURED_WGSL.contains(
            "let r0 = eval_stage(stages.stages[0], diffuse, diffuse, in.uv0, in.uv1, in.cam_pos"
        ));
        assert!(
            TEXTURED_WGSL.contains("out.cam_pos = fog.world_view * vec4<f32>(in.position, 1.0)")
        );
    }
}

/// Float diffuse produced by the bounded software lighting stage, without an
/// intermediate D3DCOLOR quantization. Position and UV retain guest values.
pub fn lit_layout() -> FvfLayout {
    let mut layout = FvfLayout::decode(0x152).unwrap();
    layout.attributes[1].format = wgpu::VertexFormat::Float32x4;
    layout.attributes[1].offset = 12;
    layout
}

pub fn lit_shader_source(textured: bool) -> String {
    let source = if textured { TEXTURED_WGSL } else { UNLIT_WGSL };
    let start = source.find("    let a = f32((in.color >> 24u)").unwrap();
    let end = source[start..]
        .find("    out.color = vec4<f32>(r, g, b, a);")
        .unwrap()
        + start
        + "    out.color = vec4<f32>(r, g, b, a);".len();
    let mut source = source.to_owned();
    source.replace_range(start..end, "    out.color = in.color;");
    // Only the transformed `VertexInput` becomes a float diffuse; the
    // pre-transformed `VertexInputRhw` keeps its D3DCOLOR and is unused by the
    // lit entry point.
    source.replacen(
        "@location(1) color: u32",
        "@location(1) color: vec4<f32>",
        1,
    )
}

/// Guest XYZ/NORMAL stream layout for diffuse lighting in the vertex shader.
pub fn gpu_lit_layout(fvf: u32) -> FvfLayout {
    let mut layout = FvfLayout::decode(fvf).unwrap();
    layout.attributes.push(wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 12,
        shader_location: 4,
    });
    layout
}

/// Retain the existing raster/texture/fog stages, replacing only diffuse
/// evaluation and adding the guest normal attribute. No guest bytes change.
pub fn gpu_lit_shader_source(textured: bool, fvf: u32) -> String {
    gpu_lit_source(lit_shader_source(textured), fvf)
}

/// Also used for fixed-function vertices paired with a programmable pixel stage.
pub fn gpu_lit_source(mut source: String, fvf: u32) -> String {
    source = source.replacen(
        "struct VertexInput {",
        "struct VertexInput {\n    @location(4) normal: vec3<f32>,",
        1,
    );
    let color = if fvf == 0x152 {
        source = source.replacen(
            "@location(1) color: vec4<f32>",
            "@location(1) color: u32",
            1,
        );
        "lighting_color(in.color), true"
    } else {
        source = source.replacen("    @location(1) color: vec4<f32>,", "", 1);
        "vec4<f32>(0.0), false"
    };
    source = source.replacen(
        "    out.color = in.color;",
        &format!("    out.color = evaluate_lighting(in.position, in.normal, {color});"),
        1,
    );
    source.push_str(include_str!("gpu_lighting.wgsl"));
    source
}
