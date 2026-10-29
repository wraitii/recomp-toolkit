//! Bounded clear, transformed unlit triangle, readback and presentation checks.
use crate::{
    backend::GpuContext,
    d3d8::{
        device::{Device, TextureLevelUpload},
        math::Mat4,
        resource::VertexBuffer,
    },
};
#[cfg(feature = "windowing")]
use std::sync::Arc;
#[cfg(feature = "windowing")]
use winit::window::Window;

/// One base-level texture upload. The probe only exercises level 0; mip chains
/// are covered by the device unit tests.
fn level0<'a>(
    generation: u64,
    force_upload: bool,
    width: u32,
    height: u32,
    data: &'a [u8],
) -> [TextureLevelUpload<'a>; 1] {
    [TextureLevelUpload {
        level: 0,
        generation,
        force_upload,
        width,
        height,
        data,
    }]
}

/// Options for a probe run.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// If true, render offscreen and read back; if false, also present a window.
    pub headless: bool,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            width: 319,
            height: 241,
            headless: true,
        }
    }
}

/// Outcome of a single probe check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The check ran and passed.
    Passed,
    /// The check ran and failed.
    Failed,
    /// The check could not run on this backend/configuration.
    Unsupported,
}

/// One named probe check and its outcome.
#[derive(Clone, Debug)]
pub struct CheckResult {
    pub name: &'static str,
    pub outcome: Outcome,
    /// Human-readable detail (actual backend/adapter, mismatch, diagnostic).
    pub detail: String,
}

/// Result of a probe run.
#[derive(Clone, Debug)]
pub struct ProbeReport {
    /// Actual wgpu backend used (e.g. `Metal`).
    pub backend: String,
    /// Adapter name reported by wgpu.
    pub adapter: String,
    /// Adapter/device limits relevant to D3D8 emulation.
    pub max_texture_dimension_2d: u32,
    pub checks: Vec<CheckResult>,
    /// Explicit requested checks; absent or duplicate results prevent success.
    pub requested_checks: Vec<&'static str>,
}

impl ProbeReport {
    /// True only for a nonempty report in which every requested check passed.
    /// Unsupported checks prevent success; omit checks outside the selected scope.
    pub fn all_passed(&self) -> bool {
        !self.requested_checks.is_empty()
            && self.checks.len() == self.requested_checks.len()
            && self.requested_checks.iter().all(|name| {
                self.requested_checks.iter().filter(|n| *n == name).count() == 1
                    && self
                        .checks
                        .iter()
                        .filter(|c| c.name == *name && c.outcome == Outcome::Passed)
                        .count()
                        == 1
            })
    }
}

/// Failure that prevents a probe run from completing.
#[derive(Clone, Debug)]
pub enum ProbeError {
    Backend(String),
    Unsupported(String),
    NotImplemented,
}

impl core::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProbeError::Backend(m) => write!(f, "backend error: {m}"),
            ProbeError::Unsupported(m) => write!(f, "unsupported: {m}"),
            ProbeError::NotImplemented => write!(f, "probe not implemented (scaffold)"),
        }
    }
}

impl std::error::Error for ProbeError {}

impl From<crate::RenderError> for ProbeError {
    fn from(error: crate::RenderError) -> Self {
        Self::Backend(error.to_string())
    }
}

/// Run the headless scope. Windowed callers must supply their event-loop window.
pub fn run(options: &ProbeOptions) -> Result<ProbeReport, ProbeError> {
    if !options.headless {
        return Err(ProbeError::Unsupported(
            "windowed mode requires run_windowed with an event-loop-owned window".into(),
        ));
    }
    let gpu = pollster::block_on(GpuContext::new_headless())?;
    run_with_gpu(
        options,
        gpu,
        #[cfg(feature = "windowing")]
        None,
    )
}

#[cfg(feature = "windowing")]
pub fn run_windowed(
    options: &ProbeOptions,
    window: Arc<Window>,
) -> Result<ProbeReport, ProbeError> {
    if options.headless {
        return Err(ProbeError::Unsupported(
            "run_windowed requires headless=false".into(),
        ));
    }
    let gpu = pollster::block_on(GpuContext::new_windowed(Arc::clone(&window)))?;
    run_with_gpu(options, gpu, Some(&window))
}

fn checked_pixels(pixels: &[u8], width: u32, height: u32) -> Result<(), ProbeError> {
    if pixels.len() != width as usize * height as usize * 4 {
        return Err(ProbeError::Backend(
            "readback is not tightly packed RGBA8".into(),
        ));
    }
    Ok(())
}

/// The centre pixel `[R, G, B, A]` of a tightly packed RGBA8 readback.
fn center_pixel(pixels: &[u8], options: &ProbeOptions) -> [u8; 4] {
    let center =
        (options.width as usize / 2) + (options.height as usize / 2) * options.width as usize;
    pixels[center * 4..center * 4 + 4].try_into().unwrap()
}

/// Draw a full-screen quad with FVF `0x242` (`XYZ|DIFFUSE|TEX2`, stride 32)
/// under two resolved texture stages and return the centre pixel. `stage0` and
/// `stage1` are `[color_op, color_arg1, color_arg2, alpha_op, alpha_arg1,
/// alpha_arg2]`; `uv0`/`uv1` are the two vertex coordinate sets and `tci1`
/// selects which set stage 1 samples.
fn draw_two_stage_quad(
    device: &mut Device,
    stage0: &[u32; 6],
    stage1: &[u32; 6],
    tci1: u32,
    diffuse: u32,
    uv0: [f32; 2],
    uv1: [f32; 2],
    width: u32,
    height: u32,
) -> Result<[u8; 4], ProbeError> {
    for (stage, state) in [(0u32, stage0), (1u32, stage1)] {
        for (offset, value) in state.iter().enumerate() {
            // state index 1..=6: COLORARG1..ALPHAARG2.
            device
                .state
                .set_texture_stage_state(stage, offset as u32 + 1, *value)?;
        }
    }
    device.state.set_texture_stage_state(1, 11, tci1)?;
    let corners = [
        [-1.0f32, -1.0, 0.5],
        [1.0, -1.0, 0.5],
        [1.0, 1.0, 0.5],
        [-1.0, -1.0, 0.5],
        [1.0, 1.0, 0.5],
        [-1.0, 1.0, 0.5],
    ];
    let mut bytes = Vec::new();
    for position in corners {
        for component in position {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
        bytes.extend_from_slice(&diffuse.to_le_bytes());
        for component in uv0 {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
        for component in uv1 {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
    }
    device.clear(&[], 1 | 2, 0x00000000, 1.0, 0)?;
    let vb = VertexBuffer::new(&bytes, 32)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x242, &vb, 0, 2)?;
    device.end_scene()?;
    let pixels = device.read_pixels()?;
    checked_pixels(&pixels, width, height)?;
    let center = (width as usize / 2) + (height as usize / 2) * width as usize;
    Ok(pixels[center * 4..center * 4 + 4].try_into().unwrap())
}

fn run_with_gpu(
    options: &ProbeOptions,
    gpu: GpuContext,
    #[cfg(feature = "windowing")] window: Option<&Window>,
) -> Result<ProbeReport, ProbeError> {
    if options.width < 32 || options.height < 32 {
        return Err(ProbeError::Unsupported(
            "probe requires dimensions >= 32 pixels".into(),
        ));
    }
    let mut report = ProbeReport {
        backend: format!("{:?}", gpu.adapter_info.backend),
        adapter: gpu.adapter_info.name.clone(),
        max_texture_dimension_2d: gpu.device.limits().max_texture_dimension_2d,
        checks: Vec::new(),
        requested_checks: vec![
            "adapter initialization",
            "X8 clear/readback",
            "reused staging/direct readback",
            "direct readback output bounds",
            "clear/readback",
            "transformed triangle/readback",
            "point list/readback",
            "depth test/readback",
            "table fog/readback",
            "cull winding/readback",
            "alpha test/readback",
            "texture factor/readback",
            "unbound stage/readback",
            "two-stage op family/readback",
            "texture upload cache/readback",
            "vertex lighting/readback",
            "render texture/depth/restoration/readback",
        ],
    };
    if !options.headless {
        report.requested_checks.push("present");
    }
    report.checks.push(CheckResult {
        name: "adapter initialization",
        outcome: Outcome::Passed,
        detail: format!(
            "device max_texture_dimension_2d={}",
            report.max_texture_dimension_2d
        ),
    });
    let opaque_target = gpu.create_target(17, 3, 22, 0)?;
    gpu.clear(&opaque_target, 1, 0x00123456, 1.0, 0)?;
    let opaque_pixels = gpu.read_pixels(&opaque_target)?;
    checked_pixels(&opaque_pixels, 17, 3)?;
    let opaque_ok = opaque_pixels
        .chunks_exact(4)
        .all(|p| p == [0x12, 0x34, 0x56, 0xff]);
    report.checks.push(CheckResult {
        name: "X8 clear/readback",
        outcome: if opaque_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: "17x3 RGBA8; X8 alpha forced to 255".into(),
    });
    // Reuse one staging allocation across clears, then change its padded size.
    // Reject wrong output sizes before recording any GPU copy.
    let mut direct_ok = true;
    for (width, height, format, alpha) in [(17, 3, 22, 255), (64, 2, 21, 0x7f), (65, 4, 21, 0x7f)] {
        let target = gpu.create_target(width, height, format, 0)?;
        let mut direct = vec![0xaa; width as usize * height as usize * 4];
        for color in [0x7f123456, 0x7fabcdef] {
            gpu.clear(&target, 1, color, 1.0, 0)?;
            gpu.read_pixels_into(&target, &mut direct)?;
            let expected = [(color >> 16) as u8, (color >> 8) as u8, color as u8, alpha];
            direct_ok &= direct.chunks_exact(4).all(|p| p == expected);
        }
        direct_ok &= gpu.read_pixels_into(&target, &mut direct[..4]).is_err();
    }
    report.checks.push(CheckResult {
        name: "reused staging/direct readback",
        outcome: if direct_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: "repeated clears; padded/aligned/resized targets; A8/X8 alpha; short output".into(),
    });
    // 80 is D3DFMT_D16: every later draw pipeline must match this depth
    // attachment format, which is what the depth test below proves.
    let mut device = Device::new(gpu, options.width, options.height, 21, 80)?;
    const CLEAR: [u8; 4] = [0x12, 0x34, 0x56, 0x7f];
    device.clear(&[], 1 | 2, 0x7f123456, 1.0, 0)?;
    let pixels = device.read_pixels()?;
    checked_pixels(&pixels, options.width, options.height)?;
    let mut readback = vec![0xaa; pixels.len()];
    let short_ok =
        device.read_pixels_into(&mut readback[..1]).is_err() && readback.iter().all(|&p| p == 0xaa);
    let readback_ok =
        short_ok && device.read_pixels_into(&mut readback).is_ok() && readback == pixels;
    report.checks.push(CheckResult {
        name: "direct readback output bounds",
        outcome: if readback_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: "short output unchanged; exact pixels".into(),
    });

    let mismatches = pixels.chunks_exact(4).filter(|p| *p != CLEAR).count();
    report.checks.push(CheckResult {
        name: "clear/readback",
        outcome: if mismatches == 0 {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!(
            "{}x{} RGBA8, {mismatches} mismatched pixels",
            options.width, options.height
        ),
    });
    device.state.set_render_state(7, 0)?;
    device.state.set_render_state(137, 0)?;
    device.state.set_render_state(22, 1)?;
    let mut world = Mat4::IDENTITY;
    world.rows[0][0] = 0.75;
    world.rows[1][0] = 0.1;
    device.state.world = world;
    device.state.view.rows[3][0] = 0.35;
    device.state.projection.rows[1][1] = 0.8;
    // D3D little-endian XYZ floats followed by a D3DCOLOR DWORD.
    let mut bytes = Vec::new();
    for position in [[-0.6f32, -0.6, 0.5], [0.6, -0.6, 0.5], [0.0, 0.6, 0.5]] {
        for component in position {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
        bytes.extend_from_slice(&0xffd05020u32.to_le_bytes());
    }
    // A large unused prefix catches uploading/drawing the wrong interval.
    let mut prefixed = vec![0; 4096 * 16];
    prefixed.extend_from_slice(&bytes);
    let vertices = VertexBuffer::new(&prefixed, 16)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x42, &vertices, 4096, 1)?;
    device.end_scene()?;
    let pixels = device.read_pixels()?;
    checked_pixels(&pixels, options.width, options.height)?;
    let sample = |nx: f32, ny: f32| -> [u8; 4] {
        let x = ((nx + 1.0) * 0.5 * options.width as f32) as usize;
        let y = ((1.0 - ny) * 0.5 * options.height as f32) as usize;
        let offset = (y * options.width as usize + x) * 4;
        pixels[offset..offset + 4].try_into().unwrap()
    };
    let interior = sample(0.35, 0.0);
    let old_location = sample(-0.2, 0.0);
    let background = sample(-0.9, 0.9);
    let order_sensitive = sample(0.075, 0.0);
    let passed = interior == [0xd0, 0x50, 0x20, 0xff]
        && old_location == CLEAR
        && background == CLEAR
        && order_sensitive == CLEAR;
    report.checks.push(CheckResult { name: "transformed triangle/readback", outcome: if passed { Outcome::Passed } else { Outcome::Failed }, detail: format!("translated interior={interior:?}, vacated location={old_location:?}, background={background:?}, order-sensitive background={order_sensitive:?}") });

    // POINTLIST: D3D8's default point state (POINTSIZE 1.0, no scale, no
    // sprite) rasterizes a single pixel. wgpu `PointList` has the same fixed
    // one-pixel coverage, so a point at NDC centre must land on the centre
    // pixel with the vertex diffuse colour. This is the GPU-side validation of
    // the topology the guest's star field (`FUN_00545ca0`) draws.
    device.clear(&[], 1 | 2, 0x00000000, 1.0, 0)?;
    // The preceding triangle check left translated matrices in place; a point
    // at NDC centre must be drawn with the identity transform to land on the
    // centre pixel.
    device.state.world = Mat4::IDENTITY;
    device.state.view = Mat4::IDENTITY;
    device.state.projection = Mat4::IDENTITY;
    let mut point_bytes = Vec::new();
    for component in [0.0f32, 0.0, 0.5] {
        point_bytes.extend_from_slice(&component.to_le_bytes());
    }
    point_bytes.extend_from_slice(&0xff20d0f0u32.to_le_bytes());
    let point = VertexBuffer::new(&point_bytes, 16)?;
    device.begin_scene()?;
    device.draw_primitive(1, 0x42, &point, 0, 1)?;
    device.end_scene()?;
    let point_pixels = device.read_pixels()?;
    checked_pixels(&point_pixels, options.width, options.height)?;
    let point_center =
        ((options.width as usize / 2) + (options.height as usize / 2) * options.width as usize) * 4;
    let point_sample: [u8; 4] = point_pixels[point_center..point_center + 4]
        .try_into()
        .unwrap();
    let point_ok = point_sample == [0x20, 0xd0, 0xf0, 0xff];
    report.checks.push(CheckResult {
        name: "point list/readback",
        outcome: if point_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("centre pixel={point_sample:?}, expected [0x20, 0xd0, 0xf0, 0xff]"),
    });

    // Depth test: with ZENABLE on, a nearer triangle drawn first must not be
    // overwritten by a farther one drawn second. A pipeline/pass attachment
    // mismatch, an ignored depth buffer, or painter's-order output all show
    // the far colour at the overlap instead of the near colour.
    device.state.world = Mat4::IDENTITY;
    device.state.view = Mat4::IDENTITY;
    device.state.projection = Mat4::IDENTITY;
    device.state.set_render_state(7, 1)?; // D3DRS_ZENABLE = TRUE
    device.clear(&[], 1 | 2, 0x00000000, 1.0, 0)?;
    let triangle = |z: f32, color: u32| -> Vec<u8> {
        let mut bytes = Vec::new();
        for position in [[-0.8f32, -0.8, z], [0.8, -0.8, z], [0.0, 0.8, z]] {
            for component in position {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
            bytes.extend_from_slice(&color.to_le_bytes());
        }
        bytes
    };
    let near = VertexBuffer::new(&triangle(0.1, 0xff00ff00), 16)?;
    let far = VertexBuffer::new(&triangle(0.9, 0xffff0000), 16)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x42, &near, 0, 1)?;
    device.draw_primitive(4, 0x42, &far, 0, 1)?;
    device.end_scene()?;
    let depth_pixels = device.read_pixels()?;
    checked_pixels(&depth_pixels, options.width, options.height)?;
    let center =
        ((options.width as usize / 2) + (options.height as usize / 2) * options.width as usize) * 4;
    let overlap = &depth_pixels[center..center + 4];
    let depth_ok = overlap == [0x00, 0xff, 0x00, 0xff];
    report.checks.push(CheckResult {
        name: "depth test/readback",
        outcome: if depth_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!(
            "near-first overlap centre={overlap:?}, expected near green [0, 255, 0, 255]"
        ),
    });
    // Table fog: LINEAR over the eye-space depth. A projection whose fourth
    // row is (0, 0, 1, 0) makes clip.w == z, so each triangle's z is its fog
    // distance. FOGSTART=1, FOGEND=3: z=0.5 is unfogged, z=2 is half-fogged,
    // z=4 is fully fogged.
    device.state.set_render_state(7, 0)?; // D3DRS_ZENABLE = FALSE
    device.state.world = Mat4::IDENTITY;
    device.state.view = Mat4::IDENTITY;
    let mut fog_projection = Mat4::IDENTITY;
    fog_projection.rows[2][3] = 1.0; // clip.w = z
    fog_projection.rows[3][3] = 0.0;
    device.state.projection = fog_projection;
    device.state.set_render_state(28, 1)?; // FOGENABLE
    device.state.set_render_state(35, 3)?; // FOGTABLEMODE = LINEAR
    device.state.set_render_state(34, 0x0000_00ff)?; // FOGCOLOR = blue
    device.state.set_render_state(36, 1.0f32.to_bits())?; // FOGSTART
    device.state.set_render_state(37, 3.0f32.to_bits())?; // FOGEND
    device.state.set_render_state(38, 1.0f32.to_bits())?; // FOGDENSITY
    let fog_triangle = |z: f32| -> Vec<u8> {
        let mut bytes = Vec::new();
        // x and y are pre-divided by z so the NDC edges stay at +-1.
        for (x, y) in [(-z, -z), (z, -z), (0.0, z)] {
            for component in [x, y, z] {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
            bytes.extend_from_slice(&0xffff_0000u32.to_le_bytes()); // red
        }
        bytes
    };
    let mut fog_samples = Vec::new();
    for z in [0.5f32, 2.0, 4.0] {
        device.clear(&[], 1 | 2, 0x0000_0000, 1.0, 0)?;
        let vb = VertexBuffer::new(&fog_triangle(z), 16)?;
        device.begin_scene()?;
        device.draw_primitive(4, 0x42, &vb, 0, 1)?;
        device.end_scene()?;
        let pixels = device.read_pixels()?;
        checked_pixels(&pixels, options.width, options.height)?;
        let centre = ((options.width as usize / 2)
            + (options.height as usize / 2) * options.width as usize)
            * 4;
        fog_samples.push([
            pixels[centre],
            pixels[centre + 1],
            pixels[centre + 2],
            pixels[centre + 3],
        ]);
    }
    // z=0.5: factor 1, pure red. z=2: factor 0.5, half red/half blue. z=4:
    // factor 0, pure blue.
    let mid = fog_samples[1];
    let fog_ok = fog_samples[0] == [0xff, 0x00, 0x00, 0xff]
        && fog_samples[2] == [0x00, 0x00, 0xff, 0xff]
        && (i32::from(mid[0]) - 0x80).abs() <= 2
        && (i32::from(mid[2]) - 0x80).abs() <= 2;
    report.checks.push(CheckResult {
        name: "table fog/readback",
        outcome: if fog_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("linear fog near/mid/far samples = {fog_samples:?}"),
    });
    // Cull winding: D3D8 defines front faces by their screen-space winding.
    // D3DCULL_CCW (the D3D8 default) culls counter-clockwise faces, D3DCULL_CW
    // culls clockwise ones, and D3DCULL_NONE culls neither. The bridge emits
    // clip space and wgpu applies the same y-down viewport flip D3D8 does, so
    // the rasterizer sees the D3D8 screen winding directly.
    device.state.world = Mat4::IDENTITY;
    device.state.view = Mat4::IDENTITY;
    device.state.projection = Mat4::IDENTITY;
    device.state.set_render_state(7, 0)?; // ZENABLE off
    device.state.set_render_state(28, 0)?; // FOGENABLE off
    device.state.set_render_state(27, 0)?; // ALPHABLENDENABLE off
    device.state.set_render_state(15, 0)?; // ALPHATESTENABLE off
    let tri = |order: [usize; 3], color: u32| -> Vec<u8> {
        let pts = [[-0.5f32, -0.5, 0.5], [0.5, -0.5, 0.5], [0.0, 0.5, 0.5]];
        let mut bytes = Vec::new();
        for i in order {
            for component in pts[i] {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
            bytes.extend_from_slice(&color.to_le_bytes());
        }
        bytes
    };
    // Forward order (v0,v1,v2) is counter-clockwise on screen; reversing the
    // last two vertices makes it clockwise.
    let ccw_red = tri([0, 1, 2], 0xffff0000);
    let cw_green = tri([0, 2, 1], 0xff00ff00);
    let cull_center =
        ((options.width as usize / 2) + (options.height as usize / 2) * options.width as usize) * 4;
    let clear = [0x00, 0x00, 0x00, 0x00];
    let mut cull_ok = true;
    let mut cull_detail = Vec::new();
    for (mode, expect) in [
        (1u32, [true, true]),  // D3DCULL_NONE
        (2u32, [true, false]), // D3DCULL_CW culls the clockwise (green) triangle
        (3u32, [false, true]), // D3DCULL_CCW culls the counter-clockwise (red) triangle
    ] {
        device.state.set_render_state(22, mode)?;
        let mut visible = [false; 2];
        let mut seen = [[0u8; 4]; 2];
        for (i, bytes) in [&ccw_red, &cw_green].into_iter().enumerate() {
            device.clear(&[], 1 | 2, 0x00000000, 1.0, 0)?;
            let vb = VertexBuffer::new(bytes, 16)?;
            device.begin_scene()?;
            device.draw_primitive(4, 0x42, &vb, 0, 1)?;
            device.end_scene()?;
            let pixels = device.read_pixels()?;
            checked_pixels(&pixels, options.width, options.height)?;
            seen[i] = pixels[cull_center..cull_center + 4].try_into().unwrap();
            visible[i] = seen[i] != clear;
        }
        if visible != expect {
            cull_ok = false;
        }
        cull_detail.push((mode, visible, seen));
    }
    report.checks.push(CheckResult {
        name: "cull winding/readback",
        outcome: if cull_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!(
            "[red_ccw, green_cw] visible by mode (1 none, 2 cw, 3 ccw) = {cull_detail:?}"
        ),
    });
    device.state.set_render_state(22, 3)?; // restore D3D8 default D3DCULL_CCW

    // Alpha test: the fragment shader compares the quantised source alpha
    // against D3DRS_ALPHAREF after stage blending. Diffuse alpha 0x40 is 64/255
    // (0.25); it fails GREATEREQUAL against 0x80 and passes against 0x40.
    device.state.set_render_state(15, 1)?; // ALPHATESTENABLE
    device.state.set_render_state(22, 1)?; // D3DCULL_NONE: isolate the alpha test
    device.state.set_render_state(25, 7)?; // ALPHAFUNC = D3DCMP_GREATEREQUAL
    let alpha_triangle = |color: u32| -> Vec<u8> {
        let mut bytes = Vec::new();
        for position in [[-0.5f32, -0.5, 0.5], [0.5, -0.5, 0.5], [0.0, 0.5, 0.5]] {
            for component in position {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
            bytes.extend_from_slice(&color.to_le_bytes());
        }
        bytes
    };
    let alpha_bytes = alpha_triangle(0x40ff0000);
    let alpha_center =
        ((options.width as usize / 2) + (options.height as usize / 2) * options.width as usize) * 4;
    let mut alpha_samples = Vec::new();
    for (func, reference, expect_visible) in [
        (7u32, 0x80u32, false), // GREATEREQUAL: 64 >= 128 fails -> discard
        (2u32, 0x80u32, true),  // LESS: 64 < 128 passes
        (7u32, 0x40u32, true),  // GREATEREQUAL: 64 >= 64 passes
    ] {
        device.state.set_render_state(25, func)?;
        device.state.set_render_state(24, reference)?;
        device.clear(&[], 1 | 2, 0x00000000, 1.0, 0)?;
        let vb = VertexBuffer::new(&alpha_bytes, 16)?;
        device.begin_scene()?;
        device.draw_primitive(4, 0x42, &vb, 0, 1)?;
        device.end_scene()?;
        let pixels = device.read_pixels()?;
        checked_pixels(&pixels, options.width, options.height)?;
        let visible = pixels[alpha_center..alpha_center + 4] != clear;
        alpha_samples.push((func, reference, visible, expect_visible));
    }
    device.state.set_render_state(15, 0)?; // ALPHATESTENABLE off
    let alpha_ok = alpha_samples
        .iter()
        .all(|(_, _, visible, expect)| visible == expect);
    report.checks.push(CheckResult {
        name: "alpha test/readback",
        outcome: if alpha_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("(func, ref, visible, expected) = {alpha_samples:?}"),
    });

    // Texture factor (D3DRS_TEXTUREFACTOR) as D3DTA_TFACTOR: COLOROP=MODULATE
    // of TFACTOR and a bound texture yields TFACTOR*texel RGB, and
    // ALPHAOP=SELECTARG2 with ALPHAARG2=TFACTOR yields its alpha. The factor is
    // ARGB 0x4080c020 -> RGB [0x80,0xc0,0x20], alpha 0x40. The draw needs the
    // textured path (0x142); a white 1x1 texture makes the modulate a no-op so
    // the check isolates the TFACTOR path with a real binding.
    device.state.set_render_state(60, 0x4080_c020)?; // D3DRS_TEXTUREFACTOR
    device.state.set_render_state(27, 0)?; // ALPHABLENDENABLE off
    device.state.set_texture_stage_state(0, 1, 4)?; // COLOROP = MODULATE
    device.state.set_texture_stage_state(0, 2, 3)?; // COLORARG1 = TFACTOR
    device.state.set_texture_stage_state(0, 3, 2)?; // COLORARG2 = TEXTURE
    device.state.set_texture_stage_state(0, 4, 3)?; // ALPHAOP = SELECTARG2
    device.state.set_texture_stage_state(0, 5, 2)?; // ALPHAARG1 = TEXTURE
    device.state.set_texture_stage_state(0, 6, 3)?; // ALPHAARG2 = TFACTOR
    let mut tf_bytes = Vec::new();
    for (position, uv) in [
        ([-0.5f32, -0.5, 0.5], [0.0f32, 0.0]),
        ([0.5, -0.5, 0.5], [1.0, 0.0]),
        ([0.0, 0.5, 0.5], [0.5, 1.0]),
    ] {
        for component in position {
            tf_bytes.extend_from_slice(&component.to_le_bytes());
        }
        tf_bytes.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // white diffuse
        for component in uv {
            tf_bytes.extend_from_slice(&component.to_le_bytes());
        }
    }
    device.clear(&[], 1 | 2, 0x00000000, 1.0, 0)?;
    let tf_vb = VertexBuffer::new(&tf_bytes, 24)?;
    const TF_TEX_ID: u32 = 88;
    // Source texels are D3D8 B,G,R,A; opaque white. MODULATE(TFACTOR, white) is
    // the factor, so this isolates TFACTOR with a real bound texture.
    let tf_white = [0xffu8, 0xff, 0xff, 0xff];
    device.set_texture(0, TF_TEX_ID, 21, &level0(1, false, 1, 1, &tf_white))?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x142, &tf_vb, 0, 1)?;
    device.end_scene()?;
    let tf_pixels = device.read_pixels()?;
    checked_pixels(&tf_pixels, options.width, options.height)?;
    let tf_seen = center_pixel(&tf_pixels, options);
    let tf_ok = tf_seen == [0x80, 0xc0, 0x20, 0x40];
    report.checks.push(CheckResult {
        name: "texture factor/readback",
        outcome: if tf_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("center={tf_seen:?}, expected [128, 192, 32, 64]"),
    });

    // Unbound-texture stage: with no texture set, Wine's `is_invalid_op`
    // rewrites MODULATE(TFACTOR, TEXTURE) to SELECTARG1(CURRENT)
    // (dlls/wined3d/utils.c at Wine commit
    // 455e3509b98a6919fd4ad1def4803e08c41c03b2). COLOROP reads TEXTURE, so it
    // passes CURRENT through (the white vertex diffuse); ALPHAOP=SELECTARG2
    // (TFACTOR) does not read TEXTURE, so alpha stays the TFACTOR alpha. The
    // renderer must not sample the white fallback for the RGB.
    device.release_texture(TF_TEX_ID);
    device.clear(&[], 1 | 2, 0x00000000, 1.0, 0)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x142, &tf_vb, 0, 1)?;
    device.end_scene()?;
    let unbound_pixels = device.read_pixels()?;
    checked_pixels(&unbound_pixels, options.width, options.height)?;
    let unbound_seen = center_pixel(&unbound_pixels, options);
    // Diffuse is opaque white, so CURRENT RGB is [255,255,255] and alpha is the
    // TFACTOR alpha 0x40.
    let unbound_ok = unbound_seen == [0xff, 0xff, 0xff, 0x40];
    report.checks.push(CheckResult {
        name: "unbound stage/readback",
        outcome: if unbound_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("center={unbound_seen:?}, expected [255, 255, 255, 64]"),
    });

    // Upload-cache wiring: an unchanged bind reuses the resident texture, a
    // bumped generation re-uploads, and release drops it. The same 1x1 source
    // is sampled through a full-screen triangle after each bind; a broken hit
    // path would leave the previous colour or panic.
    device.state.set_texture_stage_state(0, 1, 2)?; // COLOROP = SELECTARG1
    device.state.set_texture_stage_state(0, 2, 2)?; // COLORARG1 = TEXTURE
    device.state.set_texture_stage_state(0, 4, 2)?; // ALPHAOP = SELECTARG1
    device.state.set_texture_stage_state(0, 5, 2)?; // ALPHAARG1 = TEXTURE
    device.state.set_texture_stage_state(1, 1, 1)?; // stage1 COLOROP = DISABLE
    device.state.set_texture_stage_state(1, 4, 1)?; // stage1 ALPHAOP = DISABLE
    // Source texels are D3D8 B,G,R,A; red is [0,0,255,255], green [0,255,0,255].
    let cache_red = [0u8, 0, 255, 255];
    let cache_green = [0u8, 255, 0, 255];
    let sample_cached = |device: &mut Device| -> Result<[u8; 4], ProbeError> {
        device.clear(&[], 1 | 2, 0x0000_0000, 1.0, 0)?;
        device.begin_scene()?;
        device.draw_primitive(4, 0x142, &tf_vb, 0, 1)?;
        device.end_scene()?;
        let pixels = device.read_pixels()?;
        let center = ((options.width as usize / 2)
            + (options.height as usize / 2) * options.width as usize)
            * 4;
        Ok(pixels[center..center + 4].try_into().unwrap())
    };
    let first = {
        device.set_texture(0, 77, 21, &level0(1, false, 1, 1, &cache_red))?;
        sample_cached(&mut device)?
    };
    // Same identity, generation and bytes: must hit the cache.
    device.set_texture(0, 77, 21, &level0(1, false, 1, 1, &cache_red))?;
    let hit = sample_cached(&mut device)?;
    // Bumped generation: must re-upload the new green content.
    device.set_texture(0, 77, 21, &level0(2, false, 1, 1, &cache_green))?;
    let bumped = sample_cached(&mut device)?;
    // Released identity: the cache entry is gone, so this is a fresh upload.
    device.release_texture(77);
    device.set_texture(0, 77, 21, &level0(2, false, 1, 1, &cache_green))?;
    let released = sample_cached(&mut device)?;
    let cache_ok = first == [255, 0, 0, 255]
        && hit == first
        && bumped == [0, 255, 0, 255]
        && released == bumped;
    report.checks.push(CheckResult {
        name: "texture upload cache/readback",
        outcome: if cache_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("first={first:?}, hit={hit:?}, bumped={bumped:?}, released={released:?}"),
    });

    // Two-stage texture composition. The guest's world draws bind stages 0 and
    // 1 (`FUN_007c23e0` pass field `+0xf0==2`) and combine them with the ops
    // emitted by `FUN_007c2940`/`FUN_007c2c70`/`FUN_007c2f80`. Each row here
    // exercises a distinct op family with a full-screen `0x242` quad; the
    // blend/TCI rows use their own textures.
    device.state.set_render_state(7, 0)?; // ZENABLE off
    device.state.set_render_state(22, 1)?; // CULL_NONE
    device.state.set_render_state(27, 0)?; // ALPHABLENDENABLE off
    device.state.world = Mat4::IDENTITY;
    device.state.view = Mat4::IDENTITY;
    device.state.projection = Mat4::IDENTITY;
    device.state.set_render_state(60, 0xffff_ffff)?; // TEXTUREFACTOR white

    // [color_op, color_arg1, color_arg2, alpha_op, alpha_arg1, alpha_arg2].
    let select_tex0 = [2u32, 2, 1, 2, 2, 1]; // SELECTARG1(TEXTURE0); alpha texture
    let select_tex1 = [2u32, 2, 1, 2, 2, 1]; // SELECTARG1(TEXTURE1)
    let tex0 = [200u8, 100, 50, 255];
    let tex1 = [40u8, 80, 160, 255];
    let cases: [(
        &str,
        [u32; 6],
        [u32; 6],
        u32,
        [u8; 4],
        [u8; 4],
        [u8; 4],
        i32,
    ); 8] = [
        // stage-1 SELECTARG1: final colour is texture 1, stage 0 discarded.
        (
            "select",
            select_tex0,
            select_tex1,
            white_u32(),
            tex0,
            tex1,
            [40, 80, 160, 255],
            0,
        ),
        // ADD(CURRENT, DIFFUSE).
        (
            "add",
            select_tex0,
            [7, 1, 0, 2, 2, 1],
            0xff0a_141e,
            tex0,
            tex1,
            [210, 120, 80, 255],
            1,
        ),
        // MODULATE4X(CURRENT, DIFFUSE) with white diffuse: CURRENT * 4, clamped.
        (
            "mod4x",
            select_tex0,
            [6, 1, 0, 2, 2, 1],
            white_u32(),
            tex0,
            tex1,
            [255, 255, 200, 255],
            1,
        ),
        // ADDSIGNED(CURRENT, DIFFUSE) with grey 0x40: CURRENT + 0.25 - 0.5.
        (
            "addsigned",
            select_tex0,
            [8, 1, 0, 2, 2, 1],
            0xff40_4040,
            tex0,
            tex1,
            [136, 36, 0, 255],
            1,
        ),
        // SUBTRACT(CURRENT, DIFFUSE) with grey 0x80.
        (
            "subtract",
            select_tex0,
            [10, 1, 0, 2, 2, 1],
            0xff80_8080,
            tex0,
            tex1,
            [72, 0, 0, 255],
            1,
        ),
        // BLENDTEXTUREALPHA uses texture-1 alpha 0x80: mix(tex0, diffuse, 0.5).
        (
            "blendtexalpha",
            select_tex0,
            [13, 1, 0, 2, 2, 1],
            0xff0a_141e,
            tex0,
            [40, 80, 160, 128],
            [105, 60, 40, 128],
            1,
        ),
        // BLENDTEXTUREALPHA is Arg1 * alpha + Arg2 * (1 - alpha) (Wine
        // `mix(arg2, arg1, alpha)`): alpha 255 keeps Arg1 (CURRENT, tex0) and alpha 0
        // keeps Arg2 (diffuse). A symmetric 0.5 alpha cannot tell the operand order apart.
        (
            "blendtexalpha a=255",
            select_tex0,
            [13, 1, 0, 2, 2, 1],
            0xff0a_141e,
            tex0,
            [40, 80, 160, 255],
            [200, 100, 50, 255],
            1,
        ),
        (
            "blendtexalpha a=0",
            select_tex0,
            [13, 1, 0, 2, 2, 1],
            0xff0a_141e,
            tex0,
            [40, 80, 160, 0],
            [10, 20, 30, 0],
            1,
        ),
    ];
    // A 2x1 texture for the TEXCOORDINDEX-1 row: set 0 samples the left texel,
    // set 1 the right one.
    let tex1_two = [[40u8, 80, 160, 255], [222, 111, 33, 255]];
    fn white_u32() -> u32 {
        0xffff_ffff
    }
    let mut two_stage_results = Vec::new();
    let mut two_stage_ok = true;
    for (name, s0, s1, diffuse, t0, t1, expected, tol) in cases {
        let d0 = [t0[2], t0[1], t0[0], t0[3]];
        let d1 = [t1[2], t1[1], t1[0], t1[3]];
        device.set_texture(0, 1, 21, &level0(0, true, 1, 1, &d0))?;
        device.set_texture(1, 2, 21, &level0(0, true, 1, 1, &d1))?;
        let seen = draw_two_stage_quad(
            &mut device,
            &s0,
            &s1,
            0,
            diffuse,
            [0.5, 0.5],
            [0.5, 0.5],
            options.width,
            options.height,
        )?;
        let ok = seen
            .iter()
            .zip(expected.iter())
            .all(|(a, b)| (*a as i32 - *b as i32).abs() <= tol);
        two_stage_ok &= ok;
        two_stage_results.push((name, seen, expected, ok));
    }
    {
        let mut d = [0u8; 8];
        for (i, t) in tex1_two.iter().enumerate() {
            d[i * 4] = t[2];
            d[i * 4 + 1] = t[1];
            d[i * 4 + 2] = t[0];
            d[i * 4 + 3] = t[3];
        }
        let d0 = [tex0[2], tex0[1], tex0[0], tex0[3]];
        device.set_texture(0, 1, 21, &level0(0, true, 1, 1, &d0))?;
        device.set_texture(1, 3, 21, &level0(0, true, 2, 1, &d))?;
        let seen = draw_two_stage_quad(
            &mut device,
            &select_tex0,
            &select_tex1,
            1,
            white_u32(),
            [0.25, 0.5],
            [0.75, 0.5],
            options.width,
            options.height,
        )?;
        let expected = [222, 111, 33, 255];
        let ok = seen == expected;
        two_stage_ok &= ok;
        two_stage_results.push(("tci1", seen, expected, ok));
    }
    report.checks.push(CheckResult {
        name: "two-stage op family/readback",
        outcome: if two_stage_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("{two_stage_results:?}"),
    });

    // Exercise the actual 0x152 stream through both float-diffuse shaders,
    // including composition, alpha discard, fog and a depth attachment.
    use crate::d3d8::state::{DeviceState, Light};
    let mut lit_results = Vec::new();
    let mut lit_ok = true;
    for (name, textured, back, fog, alpha_test, expected) in [
        ("untextured", false, false, false, false, [128, 64, 32, 128]),
        ("textured", true, false, false, false, [128, 64, 32, 128]),
        ("back normal", true, true, false, false, [0, 0, 0, 128]),
        ("table fog", true, false, true, false, [64, 32, 144, 128]),
        ("alpha discard", true, false, false, true, [0, 0, 0, 0]),
    ] {
        device.state = DeviceState::new(options.width, options.height);
        device.state.set_render_state(22, 1)?;
        device.state.set_light(
            0,
            Light {
                light_type: 3,
                direction: [0.0, 0.0, -1.0],
                diffuse: [0.5, 0.25, 0.125, 0.0],
                ..Light::default()
            },
        )?;
        device.state.light_enable(0, true)?;
        device.state.set_texture_stage_state(1, 1, 1)?;
        device.state.set_texture_stage_state(1, 4, 1)?;
        device.state.set_texture_stage_state(0, 4, 1)?;
        device
            .state
            .set_texture_stage_state(0, 1, if textured { 4 } else { 1 })?;
        if textured {
            device.set_texture(0, 999, 21, &level0(1, false, 1, 1, &[255; 4]))?;
            device.state.set_texture_stage_state(0, 2, 2)?;
            device.state.set_texture_stage_state(0, 3, 0)?;
            device.state.set_texture_stage_state(0, 4, 4)?;
            device.state.set_texture_stage_state(0, 5, 2)?;
            device.state.set_texture_stage_state(0, 6, 0)?;
        }
        if fog {
            device.state.set_render_state(28, 1)?;
            device.state.set_render_state(35, 3)?;
            device.state.set_render_state(34, 0xff0000ff)?;
            device.state.set_render_state(36, 0.0f32.to_bits())?;
            device.state.set_render_state(37, 2.0f32.to_bits())?;
        }
        if alpha_test {
            device.state.set_render_state(15, 1)?;
            device.state.set_render_state(24, 200)?;
            device.state.set_render_state(25, 5)?;
        }
        let mut bytes = Vec::new();
        for position in [[-0.8f32, -0.8, 0.5], [0.8, -0.8, 0.5], [0.0, 0.8, 0.5]] {
            for value in position
                .into_iter()
                .chain([0.0, 0.0, if back { -1.0 } else { 1.0 }])
            {
                bytes.extend(value.to_le_bytes());
            }
            bytes.extend(0x80ffffffu32.to_le_bytes());
            bytes.extend([0; 8]);
        }
        let mut prefixed = vec![0; 4096 * 36];
        prefixed.extend_from_slice(&bytes);
        let vertices = VertexBuffer::new(&prefixed, 36)?;
        device.clear(&[], 3, 0, 1.0, 0)?;
        device.begin_scene()?;
        device.draw_primitive(4, 0x152, &vertices, 4096, 1)?;
        device.end_scene()?;
        let pixels = device.read_pixels()?;
        let seen: [u8; 4] = pixels[center..center + 4].try_into().unwrap();
        // UNORM conversion at a half-byte permits the adjacent integer.
        let passed = seen.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 1);
        lit_ok &= passed;
        lit_results.push((name, seen, expected, passed));
    }
    report.checks.push(CheckResult {
        name: "vertex lighting/readback",
        outcome: if lit_ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        detail: format!("{lit_results:?}"),
    });

    // Reproduce the evidenced small-color/larger-shared-depth pairing. Depth
    // writes and clears must touch only the viewport, texture sampling must see
    // GPU output despite unchanged CPU bytes/cache eviction, and restoring the
    // backbuffer must preserve both its color and the original depth storage.
    device.reset(options.width, options.height, 21, 71)?;
    device.state.set_render_state(137, 0)?; // unlit fixture
    device.state.set_render_state(22, 1)?; // no culling
    device.state.set_render_state(7, 1)?; // ZENABLE
    device.state.set_render_state(14, 1)?; // ZWRITEENABLE
    device.state.set_render_state(23, 4)?; // LESSEQUAL
    device.clear(&[], 3, 0xff00_00ff, 0.25, 0)?;
    let rt_width = 32;
    let rt_height = 16;
    let stale_cpu = vec![0u8; rt_width as usize * rt_height as usize * 4];
    let make_quad = |argb: u32, depth: f32| -> Result<VertexBuffer, ProbeError> {
        let mut bytes = Vec::new();
        for p in [
            [-1.0f32, -1.0],
            [1.0, -1.0],
            [1.0, 1.0],
            [-1.0, -1.0],
            [1.0, 1.0],
            [-1.0, 1.0],
        ] {
            for component in [p[0], p[1], depth] {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
            bytes.extend_from_slice(&argb.to_le_bytes());
            for component in [0.5f32, 0.5] {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
        }
        Ok(VertexBuffer::new(&bytes, 24)?)
    };
    let red_quad = make_quad(0xffff_0000, 0.5)?;
    let green_quad = make_quad(0xff00_ff00, 0.5)?;
    device.set_render_target(901, 0, 7, 21, rt_width, rt_height, &stale_cpu, true, true)?;
    let rt_viewport = device.state.viewport;
    device.clear(&[], 3, 0xffff_ffff, 0.75, 0)?;
    device.state.set_texture_stage_state(0, 1, 1)?;
    device.state.set_texture_stage_state(0, 4, 1)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x142, &red_quad, 0, 2)?;
    let cpu_newer = device.read_texture_if_current(901, 0, 8)?.is_none();
    let gpu_current = device.read_texture_if_current(901, 0, 7)?.is_some();
    let rendered = device.read_texture(901, 0)?;
    let rt_center = ((rt_height as usize / 2) * rt_width as usize + rt_width as usize / 2) * 4;
    let rt_pixel: [u8; 4] = rendered[rt_center..rt_center + 4].try_into().unwrap(); // BGRA
    let during = device.read_pixels()?;
    let untouched = during.chunks_exact(4).all(|p| p == [0, 0, 255, 255]);
    device.set_render_target(0, 0, 0, 21, 0, 0, &[], true, true)?;
    let restored_viewport = device.state.viewport; // same open scene across the switch
    device.draw_primitive(4, 0x142, &green_quad, 0, 2)?;
    device.end_scene()?;
    let depth_pixels = device.read_pixels()?;
    let inside = (8 * options.width as usize + 8) * 4;
    let outside =
        ((options.height as usize / 2) * options.width as usize + options.width as usize / 2) * 4;
    let shared_inside: [u8; 4] = depth_pixels[inside..inside + 4].try_into().unwrap();
    let shared_outside: [u8; 4] = depth_pixels[outside..outside + 4].try_into().unwrap();
    // Null depth really detaches; color/viewport restoration remains independent.
    device.set_render_target(0, 0, 0, 21, 0, 0, &[], false, true)?;
    let detached = !device.has_depth();
    device.state.set_render_state(7, 0)?;
    device.state.set_texture_stage_state(0, 1, 2)?;
    device.state.set_texture_stage_state(0, 2, 2)?;
    device.state.set_texture_stage_state(0, 4, 2)?;
    device.state.set_texture_stage_state(0, 5, 2)?;
    device.state.set_texture_stage_state(1, 1, 1)?;
    device.state.set_texture_stage_state(1, 4, 1)?;
    for i in 0..4098u32 {
        device.set_texture(1, 10000 + i, 21, &level0(1, false, 1, 1, &[255, 0, 0, 255]))?;
    }
    device.set_texture(1, 0, 0, &[])?;
    device.set_texture(
        0,
        901,
        21,
        &level0(7, false, rt_width, rt_height, &stale_cpu),
    )?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x142, &green_quad, 0, 2)?;
    device.end_scene()?;
    let sampled = device.read_pixels()?;
    let sample: [u8; 4] = sampled[outside..outside + 4].try_into().unwrap();
    // Explicit CPU generation change must update the GPU surface, too.
    let new_cpu = [0u8, 255, 0, 255].repeat(rt_width as usize * rt_height as usize);
    device.set_texture(0, 901, 21, &level0(8, false, rt_width, rt_height, &new_cpu))?;
    device.begin_scene()?;
    device.draw_primitive(4, 0x142, &red_quad, 0, 2)?;
    device.end_scene()?;
    let updated = device.read_pixels()?;
    let update: [u8; 4] = updated[outside..outside + 4].try_into().unwrap();
    let rt_ok = cpu_newer
        && gpu_current
        && rt_viewport.width == rt_width
        && rt_viewport.height == rt_height
        && restored_viewport.width == options.width
        && restored_viewport.height == options.height
        && rt_pixel == [0, 0, 255, 255]
        && untouched
        && detached
        && shared_inside == [0, 255, 0, 255]
        && shared_outside == [0, 0, 255, 255]
        && sample == [255, 0, 0, 255]
        && update == [0, 255, 0, 255];
    report.checks.push(CheckResult {
        name: "render texture/depth/restoration/readback",
        outcome: if rt_ok { Outcome::Passed } else { Outcome::Failed },
        detail: format!("RT BGRA={rt_pixel:?} backbuffer_preserved={untouched} depth_inside={shared_inside:?} depth_outside={shared_outside:?} sampled_after_eviction={sample:?} CPU_update={update:?} depth_detached={detached}"),
    });

    if !options.headless {
        #[cfg(feature = "windowing")]
        if let Some(window) = window {
            window.pre_present_notify();
        }
        device.present()?;
        report.checks.push(CheckResult { name: "present", outcome: Outcome::Passed, detail: "same offscreen target submitted to the window surface and presented; no compositor screenshot comparison".into() });
    }
    Ok(report)
}

pub fn print_report(report: &ProbeReport) {
    println!("backend: {}", report.backend);
    println!("adapter: {}", report.adapter);
    println!(
        "max_texture_dimension_2d: {}",
        report.max_texture_dimension_2d
    );
    for check in &report.checks {
        println!("{:?}: {} ({})", check.outcome, check.name, check.detail);
    }
}
