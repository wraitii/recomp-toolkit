//! wgpu backend: instance, adapter, device/queue, offscreen targets, readback
//! and windowed presentation.
//!
//! [`GpuContext`] owns the real GPU objects and records the actual backend,
//! adapter name and identifiers so the probe can report them. Other modules
//! (format mapping, fixed-function description) also use wgpu types; this module
//! is where the wgpu instance/device/surface actually live.
//!
//! Design rules:
//! * Offscreen targets are always **linear UNORM** (`Rgba8Unorm`). There is no
//!   silent sRGB selection, so exact clear/readback checks see raw bytes.
//! * [`GpuContext::read_pixels`] returns tightly packed RGBA8 rows; the 256-byte
//!   GPU copy alignment padding is stripped.
//! * [`GpuContext::present`] always copies the supplied target's contents with a
//!   fullscreen `textureLoad` blit, even when the surface format differs. The
//!   blit preserves top-left orientation and explicitly maps the target size
//!   onto the surface size (stretch + clamp), so a window resize cannot read
//!   outside the target.
//! * `X8R8G8B8` alpha policy: clear writes alpha 1.0, readback forces 255, and
//!   [`OffscreenTarget::color_write_mask`] returns an RGB-only write mask so a
//!   draw pipeline leaves alpha untouched.
//! * The windowed entry point takes an `Arc<winit::window::Window>` owned by the
//!   caller's event loop. There is no hidden event loop here.
//! * wgpu validation errors from `create_target`, `clear`, `ensure_blit` and
//!   `present` are captured with operation-scoped error scopes and returned as
//!   [`RenderError`]. An uncaptured-error handler stays fail-loud for anything
//!   unexpected (internal/OOM, or errors outside a scope).
//!
//! Format support is intentionally narrow; see [`crate::d3d8::format`].

use std::sync::Arc;

pub(crate) mod native_completion;
pub(crate) mod native_present;
mod postprocess;

use crate::RenderError;
use crate::d3d8::format;

/// A created offscreen render target.
///
/// The struct keeps the originating `D3DFORMAT` in a private field so
/// [`GpuContext::clear`] and [`GpuContext::read_pixels`] can apply the
/// `X8R8G8B8` opaque-alpha rule. The raw D3D value is available through
/// [`Self::d3d_format`].
#[derive(Clone)]
pub struct OffscreenTarget {
    /// The backing texture.
    pub texture: wgpu::Texture,
    /// The default view used for rendering and binding.
    pub view: wgpu::TextureView,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// wgpu format of the texture (always linear `Rgba8Unorm` today).
    pub format: wgpu::TextureFormat,
    /// Originating raw `D3DFORMAT`.
    d3d_format: u32,
    /// Depth/stencil attachment, or `None` when the device was created without
    /// one. Its format is the honest wgpu mapping documented in
    /// [`crate::d3d8::format::DepthFormat`].
    pub depth: Option<DepthAttachment>,
}

/// A depth (and possibly stencil) render attachment owned by an offscreen
/// target. Kept alive by the target; the wgpu view is what a clear or draw
/// pass binds.
#[derive(Clone)]
pub struct DepthAttachment {
    /// The backing texture.
    pub texture: wgpu::Texture,
    /// The default view used as a render attachment.
    pub view: wgpu::TextureView,
    /// wgpu depth (and stencil) format.
    pub format: wgpu::TextureFormat,
    /// True when the format's wgpu aspect includes stencil.
    pub has_stencil: bool,
    /// Originating raw `D3DFORMAT`.
    pub d3d_format: u32,
}

impl OffscreenTarget {
    /// Originating raw `D3DFORMAT` (21 or 22).
    pub const fn d3d_format(&self) -> u32 {
        self.d3d_format
    }

    /// True when alpha must be forced to 255 (the `X8R8G8B8` rule).
    pub const fn forces_opaque_alpha(&self) -> bool {
        self.d3d_format == format::D3DFMT_X8R8G8B8
    }

    /// Recommended color-write mask for a draw pipeline targeting this surface.
    ///
    /// `X8R8G8B8` has no alpha, so RGB-only writes keep the forced clear alpha
    /// (1.0) in place. `A8R8G8B8` writes all channels.
    pub const fn color_write_mask(&self) -> wgpu::ColorWrites {
        if self.forces_opaque_alpha() {
            wgpu::ColorWrites::COLOR
        } else {
            wgpu::ColorWrites::ALL
        }
    }

    /// Bytes per pixel of the tightly packed readback.
    pub const fn bytes_per_pixel(&self) -> u32 {
        4
    }

    /// Unpadded row length in bytes.
    pub const fn row_bytes(&self) -> u32 {
        self.width * self.bytes_per_pixel()
    }
}

/// Owns the wgpu instance/device/queue and, for windowed use, the surface.
pub struct GpuContext {
    pub(crate) d3d9_pending:
        std::sync::OnceLock<Arc<std::sync::Mutex<crate::d3d9::gpu::PendingWork>>>,
    pub(crate) d3d9_physical_loss: crate::d3d9::gpu::PhysicalLoss,
    /// The wgpu logical device.
    pub device: wgpu::Device,
    /// The queue used to submit work.
    pub queue: wgpu::Queue,
    /// Actual adapter details reported by wgpu.
    pub adapter_info: wgpu::AdapterInfo,
    /// Kept for surface-capability queries during presentation.
    adapter: wgpu::Adapter,
    /// Present surface, or `None` for a headless context.
    surface: Option<wgpu::Surface<'static>>,
    /// Window backing `surface`, kept so `present` can size the swapchain to the
    /// physical window rather than the offscreen target.
    #[cfg(feature = "windowing")]
    window: Option<Arc<winit::window::Window>>,
    /// Last configuration applied to `surface`.
    surface_config: Option<wgpu::SurfaceConfiguration>,
    /// Cached present blit pipeline for the current surface format.
    blit: Option<BlitState>,
    /// Viewport-clear shader and pipelines, built once per distinct target
    /// formats and clear flags (rebuilding them per clear dominated the
    /// projected-shadow pass).
    viewport_clear: std::sync::Mutex<ViewportClearCache>,
    // Serializes mapping and reuses staging storage across backbuffer/texture reads.
    readback: std::sync::Mutex<Option<wgpu::Buffer>>,
    /// Optional FXAA pipeline and scratch target; see `postprocess`.
    fxaa: std::sync::Mutex<Option<postprocess::FxaaState>>,
}

#[derive(Default)]
struct ViewportClearCache {
    shader: Option<wgpu::ShaderModule>,
    pipelines: std::collections::HashMap<ViewportClearKey, wgpu::RenderPipeline>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ViewportClearKey {
    color_format: wgpu::TextureFormat,
    color_write_mask: wgpu::ColorWrites,
    depth_format: Option<wgpu::TextureFormat>,
    depth_write: bool,
    stencil_write: bool,
}

/// Cached present-blit pipeline, keyed by the surface format.
struct BlitState {
    format: wgpu::TextureFormat,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
}

/// Fullscreen `textureLoad` blit used by [`GpuContext::present`].
///
/// A single oversized triangle covers the surface. The fragment shader maps the
/// top-left-origin framebuffer coordinate through `dst`/`src` sizes, so the
/// target is stretched onto a differently sized surface without flipping.
/// Texels outside the target are clamped to the edge. This mirrors
/// [`blit_source_coord`], which is unit-tested on the CPU.
const BLIT_WGSL: &str = r#"
struct BlitParams {
    src_size: vec2<f32>,
    dst_size: vec2<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> params: BlitParams;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    return vec4<f32>(pos[idx], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let src = max(params.src_size, vec2<f32>(1.0, 1.0));
    let dst = max(params.dst_size, vec2<f32>(1.0, 1.0));
    let coord = vec2<i32>(frag.xy / dst * src);
    let max_coord = vec2<i32>(src - vec2<f32>(1.0, 1.0));
    return textureLoad(src_tex, clamp(coord, vec2<i32>(0, 0), max_coord), 0);
}
"#;

impl GpuContext {
    /// Submit all queued D3D9 work before the owner submits directly to the queue.
    pub fn synchronize_d3d9(&self) {
        if let Some(pending) = self.d3d9_pending.get() {
            pending.lock().unwrap().synchronize();
        }
    }

    pub fn physical_loss(&self) -> crate::d3d9::gpu::PhysicalLoss {
        self.d3d9_physical_loss.clone()
    }
    /// Wrap a caller-owned headless device and queue. The caller guarantees that
    /// both came from this adapter and imports each device only once; wgpu does
    /// not expose a check for that provenance. Share the returned context with
    /// every client. The caller owns error/loss handling and must synchronize
    /// direct queue access with all clients.
    pub fn from_supplied(
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Result<Self, RenderError> {
        if !adapter.features().contains(device.features())
            || !device.limits().check_limits(&adapter.limits())
        {
            return Err(RenderError::invalid(
                "SuppliedContext",
                "device capabilities exceed the supplied adapter capabilities",
            ));
        }
        let adapter_info = adapter.get_info();
        Ok(Self {
            d3d9_pending: Default::default(),
            d3d9_physical_loss: Default::default(),
            device,
            queue,
            adapter_info,
            adapter,
            surface: None,
            #[cfg(feature = "windowing")]
            window: None,
            surface_config: None,
            viewport_clear: Default::default(),
            fxaa: Default::default(),
            readback: Default::default(),
            blit: None,
        })
    }

    /// Create a headless context with the default backend set.
    pub async fn new_headless() -> Result<Self, RenderError> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map_err(|e| RenderError::new("new_headless::request_adapter", e.to_string()))?;
        Self::from_adapter(
            adapter,
            None,
            #[cfg(feature = "windowing")]
            None,
            wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER,
        )
        .await
    }

    /// Create a headless context for the D3D9 path.
    ///
    /// D3D9 needs block-compressed storage and optional 16-bit normalized
    /// formats. Features are intersected with the adapter so a device that
    /// lacks them still opens; the format is then refused by name.
    pub async fn new_headless_d3d9() -> Result<Self, RenderError> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map_err(|e| RenderError::new("new_headless_d3d9::request_adapter", e.to_string()))?;
        let requested = wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER
            | wgpu::Features::TEXTURE_COMPRESSION_BC
            | wgpu::Features::TEXTURE_COMPRESSION_BC_SLICED_3D
            | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
            | wgpu::Features::POLYGON_MODE_LINE
            | wgpu::Features::POLYGON_MODE_POINT;
        Self::from_adapter(
            adapter,
            None,
            #[cfg(feature = "windowing")]
            None,
            requested,
        )
        .await
    }

    /// True when this context owns a window surface, i.e. [`GpuContext::present`]
    /// will blit to a swapchain. A headless context returns false and presents
    /// by doing nothing: the frame is already complete in the offscreen target,
    /// and the bridge's readback is the presentation for that host.
    pub fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// Create a context that presents to `window`.
    ///
    /// The caller owns the event loop and the window lifetime; the `Arc` keeps
    /// the window alive through the surface. No event loop is created here.
    #[cfg(feature = "windowing")]
    pub async fn new_windowed(window: Arc<winit::window::Window>) -> Result<Self, RenderError> {
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| RenderError::new("new_windowed::create_surface", e.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::new("new_windowed::request_adapter", e.to_string()))?;
        Self::from_adapter(
            adapter,
            Some(surface),
            Some(window),
            wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER,
        )
        .await
    }

    async fn from_adapter(
        adapter: wgpu::Adapter,
        surface: Option<wgpu::Surface<'static>>,
        #[cfg(feature = "windowing")] window: Option<Arc<winit::window::Window>>,
        requested_features: wgpu::Features,
    ) -> Result<Self, RenderError> {
        let adapter_info = adapter.get_info();
        // Optional features are intersected with the adapter. A guest that
        // selects a feature the adapter lacks fails with a named error later
        // instead of stopping the whole device from opening.
        let required_features = adapter.features() & requested_features;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("d3d8-wgpu"),
                required_features,
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::new("request_device", e.to_string()))?;

        // Validation errors are reported synchronously through this handler.
        // Panicking here keeps them loud and attributed instead of swallowed.
        let uncaptured: Arc<dyn wgpu::UncapturedErrorHandler> = Arc::new(|error: wgpu::Error| {
            log::error!("d3d8-wgpu uncaptured wgpu error: {error}");
            panic!("d3d8-wgpu uncaptured wgpu error: {error}");
        });
        device.on_uncaptured_error(uncaptured);
        let physical_loss = crate::d3d9::gpu::PhysicalLoss::default();
        let callback_loss = physical_loss.clone();
        device.set_device_lost_callback(move |reason, message| {
            callback_loss.mark(format!("{reason:?}: {message}"));
        });

        log::info!(
            "d3d8-wgpu adapter: backend={:?} name={:?} vendor=0x{:04x} device=0x{:04x}",
            adapter_info.backend,
            adapter_info.name,
            adapter_info.vendor,
            adapter_info.device
        );

        Ok(Self {
            d3d9_pending: Default::default(),
            d3d9_physical_loss: physical_loss,
            device,
            queue,
            adapter_info,
            adapter,
            surface,
            #[cfg(feature = "windowing")]
            window,
            surface_config: None,
            viewport_clear: Default::default(),
            fxaa: Default::default(),
            readback: Default::default(),
            blit: None,
        })
    }

    /// Run `f` inside a wgpu validation error scope, converting any validation
    /// error into a named [`RenderError`].
    ///
    /// Expected validation errors (bad state, unsupported use) become an
    /// `Err` instead of panicking. Errors outside a scope, and non-validation
    /// errors, still reach the fail-loud uncaptured handler.
    fn scope_validation<T>(
        &self,
        operation: &'static str,
        f: impl FnOnce() -> Result<T, RenderError>,
    ) -> Result<T, RenderError> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let value = f();
        match pollster::block_on(scope.pop()) {
            Some(error) => Err(RenderError::new(operation, error.to_string())),
            None => value,
        }
    }

    /// Allocate an offscreen render target for a raw `D3DFORMAT`.
    ///
    /// The five color formats `ColorFormat` decodes are accepted; every other
    /// value is a named error. The returned target is always a linear
    /// `Rgba8Unorm` texture and readback re-encodes into the source layout.
    pub fn create_target(
        &self,
        width: u32,
        height: u32,
        d3d_format: u32,
        depth_format: u32,
    ) -> Result<OffscreenTarget, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::new(
                "create_target",
                format!("target dimensions {width}x{height} must be nonzero"),
            ));
        }
        let color = format::ColorFormat::from_d3dformat(d3d_format)?;
        if color == format::ColorFormat::A8 {
            return Err(RenderError::new(
                "create_target",
                "A8 is supported for texture sampling only",
            ));
        }
        let wgpu_format = color.wgpu_format();
        let depth = format::DepthFormat::from_d3dformat(depth_format)?;
        let max = self.device.limits().max_texture_dimension_2d;
        if width > max || height > max {
            return Err(RenderError::new(
                "create_target",
                format!("target {width}x{height} exceeds max_texture_dimension_2d {max}"),
            ));
        }

        self.scope_validation("create_target", || {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("d3d8-offscreen"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            let depth = depth.map(|depth| self.create_depth(width, height, depth));

            Ok(OffscreenTarget {
                texture,
                view,
                width,
                height,
                format: wgpu_format,
                d3d_format,
                depth,
            })
        })
    }

    pub fn create_depth_surface(
        &self,
        width: u32,
        height: u32,
        depth_format: u32,
    ) -> Result<DepthAttachment, RenderError> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Err(RenderError::new(
                "CreateDepthStencilSurface",
                "invalid dimensions",
            ));
        }
        let depth = format::DepthFormat::from_d3dformat(depth_format)?
            .ok_or_else(|| RenderError::new("CreateDepthStencilSurface", "missing depth format"))?;
        self.scope_validation("CreateDepthStencilSurface", || {
            Ok(self.create_depth(width, height, depth))
        })
    }

    fn create_depth(&self, width: u32, height: u32, depth: format::DepthFormat) -> DepthAttachment {
        let depth_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("d3d8-offscreen-depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: depth.wgpu_format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth_texture.create_view(&wgpu::TextureViewDescriptor::default());
        DepthAttachment {
            texture: depth_texture,
            view: depth_view,
            format: depth.wgpu_format(),
            has_stencil: depth.has_stencil(),
            d3d_format: depth.d3dformat(),
        }
    }

    /// Clear target color and/or depth/stencil, exactly as far as `flags` asks.
    ///
    /// `flags` is the D3D8 `D3DCLEAR_*` mask: bit 0 target, bit 1 ZBUFFER, bit 2
    /// STENCIL. `z` is the depth clear value (D3D8 passes a float) and
    /// `stencil` the stencil byte. A request for a depth or stencil clear on a
    /// target with no depth attachment is a named error, never a silent no-op.
    ///
    /// For `X8R8G8B8` targets the alpha byte is forced to 255. Depth and
    /// stencil are stored for the following draws in the same frame.
    pub fn clear(
        &self,
        target: &OffscreenTarget,
        flags: u32,
        argb: u32,
        z: f32,
        stencil: u32,
    ) -> Result<(), RenderError> {
        const CLEAR_TARGET: u32 = 0x1;
        const CLEAR_ZBUFFER: u32 = 0x2;
        const CLEAR_STENCIL: u32 = 0x4;
        let color = format::d3dcolor_to_wgpu(argb, target.forces_opaque_alpha());

        let color_attachment = if flags & CLEAR_TARGET != 0 {
            Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(color),
                    store: wgpu::StoreOp::Store,
                },
            })
        } else {
            None
        };

        let wants_depth = flags & CLEAR_ZBUFFER != 0;
        let wants_stencil = flags & CLEAR_STENCIL != 0;
        let depth_attachment = match &target.depth {
            None if wants_depth || wants_stencil => {
                return Err(RenderError::new(
                    "clear",
                    "depth/stencil clear requested but the device has no depth attachment",
                ));
            }
            None => None,
            Some(depth) => {
                if wants_stencil && !depth.has_stencil {
                    return Err(RenderError::new(
                        "clear",
                        format!(
                            "stencil clear requested but depth format 0x{:x} has no stencil",
                            depth.d3d_format
                        ),
                    ));
                }
                Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth.view,
                    depth_ops: if wants_depth {
                        Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(z),
                            store: wgpu::StoreOp::Store,
                        })
                    } else {
                        Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        })
                    },
                    stencil_ops: if depth.has_stencil {
                        Some(wgpu::Operations {
                            load: if wants_stencil {
                                wgpu::LoadOp::Clear(stencil)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        })
                    } else {
                        None
                    },
                })
            }
        };

        if color_attachment.is_none() && depth_attachment.is_none() {
            return Err(RenderError::new(
                "clear",
                "clear flags selected nothing to clear",
            ));
        }

        self.scope_validation("clear", || {
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("d3d8-clear"),
                });
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("d3d8-clear"),
                    color_attachments: &[color_attachment],
                    depth_stencil_attachment: depth_attachment,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            self.queue.submit(std::iter::once(encoder.finish()));
            Ok(())
        })
    }

    /// D3D8 Clear without explicit rectangles clears the current viewport.
    /// LoadOp::Clear is valid only for a full physical attachment. A scissored
    /// replacement pass preserves shared depth/stencil outside a smaller RT.
    #[allow(clippy::too_many_arguments)]
    pub fn clear_region(
        &self,
        target: &OffscreenTarget,
        flags: u32,
        argb: u32,
        z: f32,
        stencil: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        if x.checked_add(width).is_none_or(|n| n > target.width)
            || y.checked_add(height).is_none_or(|n| n > target.height)
        {
            return Err(RenderError::invalid(
                "Clear",
                format!(
                    "region ({x}, {y} {width}x{height}) outside logical target {}x{}",
                    target.width, target.height
                ),
            ));
        }
        if flags & 6 != 0 && target.depth.is_none() {
            return Err(RenderError::new(
                "Clear",
                "depth/stencil requested without attachment",
            ));
        }
        if flags & 4 != 0 && !target.depth.as_ref().is_some_and(|d| d.has_stencil) {
            return Err(RenderError::new(
                "Clear",
                "stencil requested without stencil aspect",
            ));
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        if x == 0 && y == 0 && width == target.texture.width() && height == target.texture.height()
        {
            return self.clear(target, flags, argb, z, stencil);
        }
        self.scope_validation("Clear viewport", || {
            use wgpu::util::DeviceExt;
            let c = format::d3dcolor_to_wgpu(argb, target.forces_opaque_alpha());
            let values: [f32; 8] = [
                c.r as f32, c.g as f32, c.b as f32, c.a as f32, z, 0.0, 0.0, 0.0,
            ];
            let uniform = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("D3D8 viewport clear values"),
                    contents: bytemuck::cast_slice(&values),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            let key = ViewportClearKey {
                color_format: target.format,
                color_write_mask: if flags & 1 != 0 {
                    target.color_write_mask()
                } else {
                    wgpu::ColorWrites::empty()
                },
                depth_format: target.depth.as_ref().map(|d| d.format),
                depth_write: flags & 2 != 0,
                stencil_write: flags & 4 != 0,
            };
            let mut cache = self.viewport_clear.lock().unwrap();
            let cache = &mut *cache;
            let shader = cache.shader.get_or_insert_with(|| {
                self.device
                    .create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("D3D8 viewport clear"),
                        source: wgpu::ShaderSource::Wgsl(
                            r#"
struct Values { color: vec4<f32>, depth: vec4<f32> }
@group(0) @binding(0) var<uniform> values: Values;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0,-1.0),vec2<f32>(3.0,-1.0),vec2<f32>(-1.0,3.0));
    return vec4<f32>(p[i], values.depth.x, 1.0);
}
@fragment fn fs() -> @location(0) vec4<f32> { return values.color; }
"#
                            .into(),
                        ),
                    })
            });
            let stencil_face = wgpu::StencilFaceState {
                compare: wgpu::CompareFunction::Always,
                fail_op: wgpu::StencilOperation::Keep,
                depth_fail_op: wgpu::StencilOperation::Keep,
                pass_op: if flags & 4 != 0 {
                    wgpu::StencilOperation::Replace
                } else {
                    wgpu::StencilOperation::Keep
                },
            };
            let pipeline = cache.pipelines.entry(key).or_insert_with(|| {
                self.device
                    .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                        label: Some("D3D8 viewport clear"),
                        layout: None,
                        vertex: wgpu::VertexState {
                            module: shader,
                            entry_point: Some("vs"),
                            compilation_options: Default::default(),
                            buffers: &[],
                        },
                        primitive: Default::default(),
                        depth_stencil: target.depth.as_ref().map(|d| wgpu::DepthStencilState {
                            format: d.format,
                            depth_write_enabled: Some(flags & 2 != 0),
                            depth_compare: Some(wgpu::CompareFunction::Always),
                            stencil: wgpu::StencilState {
                                front: stencil_face,
                                back: stencil_face,
                                read_mask: 0xff,
                                write_mask: if flags & 4 != 0 { 0xff } else { 0 },
                            },
                            bias: Default::default(),
                        }),
                        multisample: Default::default(),
                        fragment: Some(wgpu::FragmentState {
                            module: shader,
                            entry_point: Some("fs"),
                            compilation_options: Default::default(),
                            targets: &[Some(wgpu::ColorTargetState {
                                format: target.format,
                                blend: None,
                                write_mask: if flags & 1 != 0 {
                                    target.color_write_mask()
                                } else {
                                    wgpu::ColorWrites::empty()
                                },
                            })],
                        }),
                        multiview_mask: None,
                        cache: None,
                    })
            });
            let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("D3D8 viewport clear"),
                layout: &pipeline.get_bind_group_layout(0),
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("D3D8 viewport clear"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("D3D8 viewport clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: target.depth.as_ref().map(|d| {
                        wgpu::RenderPassDepthStencilAttachment {
                            view: &d.view,
                            depth_ops: Some(wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            }),
                            stencil_ops: d.has_stencil.then_some(wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            }),
                        }
                    }),
                    ..Default::default()
                });
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.set_scissor_rect(x, y, width, height);
                pass.set_stencil_reference(stencil);
                pass.draw(0..3, 0..1);
            }
            self.queue.submit([encoder.finish()]);
            Ok(())
        })
    }

    /// Read the whole target back as tightly packed RGBA8.
    ///
    /// GPU `COPY_BYTES_PER_ROW_ALIGNMENT` (256-byte) padding is removed, so the
    /// returned buffer is exactly `width * height * 4` bytes. For `X8R8G8B8`
    /// targets the alpha byte of every pixel is forced to 255.
    pub fn read_pixels(&self, target: &OffscreenTarget) -> Result<Vec<u8>, RenderError> {
        let mut out = vec![0; target.row_bytes() as usize * target.height as usize];
        self.read_pixels_into(target, &mut out)?;
        Ok(out)
    }

    /// Copy mapped rows directly into caller-owned, tightly packed RGBA8 storage.
    /// The staging buffer remains locked until unmapped; no mapped pointer escapes.
    pub fn read_pixels_into(
        &self,
        target: &OffscreenTarget,
        out: &mut [u8],
    ) -> Result<(), RenderError> {
        let needed = target.row_bytes() as usize * target.height as usize;
        if out.len() != needed {
            return Err(RenderError::new(
                "read_pixels",
                "output size does not match target",
            ));
        }
        let unpadded_row = target.row_bytes() as u64;
        let padded_row = align_up(unpadded_row, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64);
        let padded_row_u32 = u32::try_from(padded_row).map_err(|_| {
            RenderError::new(
                "read_pixels",
                format!("padded row {padded_row} exceeds u32"),
            )
        })?;
        let buffer_size = padded_row * target.height as u64;
        let max_buffer_size = self.device.limits().max_buffer_size;
        if buffer_size > max_buffer_size {
            return Err(RenderError::new(
                "read_pixels",
                format!(
                    "readback buffer {buffer_size} bytes exceeds max_buffer_size {max_buffer_size}"
                ),
            ));
        }

        let mut staging = self.readback.lock().expect("readback mutex poisoned");
        if staging
            .as_ref()
            .is_none_or(|buffer| buffer.size() != buffer_size)
        {
            *staging = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("d3d8-readback"),
                size: buffer_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
        }
        let buffer = staging.as_ref().expect("readback buffer allocated");

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("d3d8-readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_u32),
                    rows_per_image: Some(target.height),
                },
            },
            wgpu::Extent3d {
                width: target.width,
                height: target.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        let mapped_result = (|| {
            self.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| RenderError::new("read_pixels::poll", e.to_string()))?;
            rx.recv()
                .map_err(|e| RenderError::new("read_pixels::map_callback", e.to_string()))?
                .map_err(|e| RenderError::new("read_pixels::map_async", e.to_string()))
        })();
        if let Err(error) = mapped_result {
            // Cancel a pending map too: the cached buffer must be reusable after
            // a reported backend failure, just as a fresh buffer would be.
            buffer.unmap();
            return Err(error);
        }

        let mapped = slice
            .get_mapped_range()
            .map_err(|error| RenderError::new("read_pixels::mapped_range", error.to_string()))?;
        copy_readback_rows(
            &mapped,
            out,
            unpadded_row as usize,
            padded_row_u32 as usize,
            target.height,
            target.forces_opaque_alpha(),
        );
        drop(mapped);
        buffer.unmap();

        Ok(())
    }

    /// Present the contents of `target` to the window surface.
    ///
    /// The offscreen texture is bound and copied with a fullscreen `textureLoad`
    /// blit, so a surface format that differs from the offscreen format (for
    /// example a BGRA or sRGB surface) still shows the target's contents. A
    /// linear (non-sRGB) surface format is preferred; if none is offered the
    /// first advertised format is used and wgpu applies the required conversion
    /// on write. A headless context has no surface to blit to, so it returns
    /// success immediately; the caller's own readback is the presentation.
    pub fn present(&mut self, target: &OffscreenTarget) -> Result<(), RenderError> {
        if self.surface.is_none() {
            return Ok(());
        }
        let (format, present_mode, alpha_mode) = {
            let surface = self.surface.as_ref().ok_or_else(|| {
                RenderError::new(
                    "present",
                    "GpuContext has no window surface (headless context)",
                )
            })?;
            let caps = surface.get_capabilities(&self.adapter);
            if caps.formats.is_empty() {
                return Err(RenderError::new(
                    "present",
                    "surface has no formats compatible with the requested adapter",
                ));
            }
            let format = caps
                .formats
                .iter()
                .copied()
                .find(|f| !f.is_srgb())
                .unwrap_or(caps.formats[0]);
            let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
                wgpu::PresentMode::Fifo
            } else {
                caps.present_modes[0]
            };
            let alpha_mode = caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto);
            (format, present_mode, alpha_mode)
        };

        self.ensure_blit(format)?;

        // The swapchain is sized to the physical window, not to the offscreen
        // target; the blit below maps target texels onto it explicitly.
        #[cfg(feature = "windowing")]
        let (surface_width, surface_height) = self
            .window
            .as_ref()
            .map(|window| {
                let size = window.inner_size();
                (size.width.max(1), size.height.max(1))
            })
            .unwrap_or((target.width.max(1), target.height.max(1)));
        #[cfg(not(feature = "windowing"))]
        let (surface_width, surface_height) = (target.width.max(1), target.height.max(1));

        let config = wgpu::SurfaceConfiguration {
            color_space: wgpu::SurfaceColorSpace::Auto,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: surface_width,
            height: surface_height,
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        let needs_config = self.surface_config.as_ref().map_or(true, |current| {
            current.width != config.width
                || current.height != config.height
                || current.format != config.format
                || current.present_mode != config.present_mode
                || current.alpha_mode != config.alpha_mode
        });

        // Configure inside its own borrow scope, then persist the config so the
        // surface borrow used by `get_current_texture` cannot alias the field write.
        if needs_config {
            {
                let surface = self.surface.as_ref().ok_or_else(|| {
                    RenderError::new(
                        "present",
                        "GpuContext has no window surface (headless context)",
                    )
                })?;
                self.scope_validation("present::configure", || {
                    surface.configure(&self.device, &config);
                    Ok(())
                })?;
            }
            self.surface_config = Some(config);
        }

        let surface = self.surface.as_ref().ok_or_else(|| {
            RenderError::new(
                "present",
                "GpuContext has no window surface (headless context)",
            )
        })?;
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            status => {
                return Err(RenderError::new(
                    "present::get_current_texture",
                    format!("{status:?}"),
                ));
            }
        };
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let dst_width = frame.texture.width().max(1);
        let dst_height = frame.texture.height().max(1);

        self.scope_validation("present", || {
            let blit = self.blit.as_ref().ok_or_else(|| {
                RenderError::new(
                    "present",
                    "internal error: blit pipeline was not initialized",
                )
            })?;

            // src/dst sizes drive the orientation-preserving mapping in the
            // blit shader (`blit_source_coord`).
            let params: [f32; 4] = [
                target.width.max(1) as f32,
                target.height.max(1) as f32,
                dst_width as f32,
                dst_height as f32,
            ];
            let params_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("d3d8-present-params"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue
                .write_buffer(&params_buffer, 0, bytemuck::cast_slice(&params));

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("d3d8-present-blit"),
                layout: &blit.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&target.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: params_buffer.as_entire_binding(),
                    },
                ],
            });

            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("d3d8-present"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("d3d8-present-blit"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &frame_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&blit.pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            self.queue.submit(std::iter::once(encoder.finish()));
            Ok(())
        })?;

        self.queue.present(frame);
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| RenderError::new("present::poll", error.to_string()))?;
        Ok(())
    }

    fn ensure_blit(&mut self, format: wgpu::TextureFormat) -> Result<(), RenderError> {
        if self.blit.as_ref().is_some_and(|blit| blit.format == format) {
            return Ok(());
        }

        let (bind_group_layout, pipeline) =
            self.scope_validation("present::ensure_blit", || {
                let shader = self
                    .device
                    .create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("d3d8-present-blit"),
                        source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
                    });
                let bind_group_layout =
                    self.device
                        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                            label: Some("d3d8-present-blit"),
                            entries: &[
                                wgpu::BindGroupLayoutEntry {
                                    binding: 0,
                                    visibility: wgpu::ShaderStages::FRAGMENT,
                                    ty: wgpu::BindingType::Texture {
                                        sample_type: wgpu::TextureSampleType::Float {
                                            filterable: true,
                                        },
                                        view_dimension: wgpu::TextureViewDimension::D2,
                                        multisampled: false,
                                    },
                                    count: None,
                                },
                                wgpu::BindGroupLayoutEntry {
                                    binding: 1,
                                    visibility: wgpu::ShaderStages::FRAGMENT,
                                    ty: wgpu::BindingType::Buffer {
                                        ty: wgpu::BufferBindingType::Uniform,
                                        has_dynamic_offset: false,
                                        min_binding_size: None,
                                    },
                                    count: None,
                                },
                            ],
                        });
                let pipeline_layout =
                    self.device
                        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                            label: Some("d3d8-present-blit"),
                            bind_group_layouts: &[Some(&bind_group_layout)],
                            immediate_size: 0,
                        });
                let pipeline =
                    self.device
                        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                            label: Some("d3d8-present-blit"),
                            layout: Some(&pipeline_layout),
                            vertex: wgpu::VertexState {
                                module: &shader,
                                entry_point: Some("vs_main"),
                                compilation_options: Default::default(),
                                buffers: &[],
                            },
                            primitive: wgpu::PrimitiveState {
                                topology: wgpu::PrimitiveTopology::TriangleList,
                                ..Default::default()
                            },
                            depth_stencil: None,
                            multisample: wgpu::MultisampleState::default(),
                            fragment: Some(wgpu::FragmentState {
                                module: &shader,
                                entry_point: Some("fs_main"),
                                compilation_options: Default::default(),
                                targets: &[Some(wgpu::ColorTargetState {
                                    format,
                                    blend: None,
                                    write_mask: wgpu::ColorWrites::ALL,
                                })],
                            }),
                            multiview_mask: None,
                            cache: None,
                        });
                Ok((bind_group_layout, pipeline))
            })?;

        self.blit = Some(BlitState {
            format,
            bind_group_layout,
            pipeline,
        });
        Ok(())
    }
}

/// Map a surface framebuffer coordinate to a source texel coordinate for the
/// present blit.
///
/// Mirrors the WGSL in [`BLIT_WGSL`]: both framebuffer and texture coordinates
/// have a top-left origin, so `x` grows right and `y` grows down (no flip). The
/// source is stretched to the destination size and out-of-range coordinates are
/// clamped to the nearest texel, so a surface larger than the target cannot read
/// outside it.
#[cfg(test)]
fn blit_source_coord(
    frag_x: f32,
    frag_y: f32,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> (i32, i32) {
    let src_w = src_w.max(1) as f32;
    let src_h = src_h.max(1) as f32;
    let dst_w = dst_w.max(1) as f32;
    let dst_h = dst_h.max(1) as f32;
    let x = (frag_x / dst_w * src_w) as i32;
    let y = (frag_y / dst_h * src_h) as i32;
    (x.clamp(0, src_w as i32 - 1), y.clamp(0, src_h as i32 - 1))
}

/// Round `value` up to the next multiple of `alignment`.
fn align_up(value: u64, alignment: u64) -> u64 {
    debug_assert!(alignment > 0);
    value.div_ceil(alignment) * alignment
}

/// Copy rows once, keeping the D3D format's readback alpha rule separate from
/// the host's opaque presentation rule. Padding never reaches caller storage.
fn copy_readback_rows(
    data: &[u8],
    out: &mut [u8],
    unpadded: usize,
    padded: usize,
    rows: u32,
    opaque: bool,
) {
    debug_assert!(padded >= unpadded);
    debug_assert_eq!(out.len(), unpadded * rows as usize);
    if !opaque && padded == unpadded {
        out.copy_from_slice(&data[..out.len()]);
        return;
    }
    for (src, dst) in data
        .chunks(padded)
        .zip(out.chunks_exact_mut(unpadded))
        .take(rows as usize)
    {
        dst.copy_from_slice(&src[..unpadded]);
        if opaque {
            for pixel in dst.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_matches_copy_alignment() {
        assert_eq!(align_up(0, 256), 0);
        assert_eq!(align_up(1, 256), 256);
        assert_eq!(align_up(256, 256), 256);
        assert_eq!(align_up(257, 256), 512);
        // A 320px-wide row is already 256-byte aligned.
        assert_eq!(align_up(320 * 4, 256), 320 * 4);
        // 321px is not.
        assert_eq!(align_up(321 * 4, 256), 1536);
    }

    #[test]
    fn strip_row_padding_removes_256_byte_stride() {
        // Two rows: row 0 = 1,2,3,4; row 1 = 5,6,7,8; padded to 8 bytes.
        let data = [
            1u8, 2, 3, 4, 0xAA, 0xBB, 0xCC, 0xDD, 5, 6, 7, 8, 0xEE, 0xFF, 0x11, 0x22,
        ];
        let mut out = [0; 8];
        copy_readback_rows(&data, &mut out, 4, 8, 2, false);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        copy_readback_rows(&data, &mut out, 4, 8, 2, true);
        assert_eq!(out, [1, 2, 3, 255, 5, 6, 7, 255]);
        copy_readback_rows(&data[..8], &mut out, 4, 4, 2, false);
        assert_eq!(out, data[..8]);
    }

    #[test]
    fn format_errors_are_named() {
        // D3DFMT_X1R5G5B5 = 24 is deliberately unsupported.
        let err = crate::d3d8::format::d3d_to_wgpu_color(24).unwrap_err();
        assert_eq!(err.operation, "format::from_d3dformat");
    }

    #[test]
    fn blit_mapping_preserves_orientation() {
        // Same size: top-left framebuffer maps to top-left texel, and x grows
        // right / y grows down.
        assert_eq!(blit_source_coord(0.5, 0.5, 4, 4, 4, 4), (0, 0));
        assert_eq!(blit_source_coord(1.5, 0.5, 4, 4, 4, 4), (1, 0));
        assert_eq!(blit_source_coord(0.5, 1.5, 4, 4, 4, 4), (0, 1));
        assert_eq!(blit_source_coord(3.5, 3.5, 4, 4, 4, 4), (3, 3));
    }

    #[test]
    fn blit_mapping_stretches_and_clamps_differing_sizes() {
        // 4x4 target on an 8x8 surface: bottom-right surface pixel still points
        // at the last target texel, not out of range.
        assert_eq!(blit_source_coord(7.5, 7.5, 4, 4, 8, 8), (3, 3));
        assert_eq!(blit_source_coord(3.5, 3.5, 4, 4, 8, 8), (1, 1));
        // A surface exactly twice as wide keeps the top edge on the top row.
        assert_eq!(blit_source_coord(11.5, 0.5, 6, 3, 12, 3), (5, 0));
        // Coordinates beyond the surface are clamped to the edge.
        assert_eq!(blit_source_coord(100.0, 100.0, 4, 4, 8, 8), (3, 3));
    }
}
