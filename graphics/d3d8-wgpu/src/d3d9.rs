use crate::RenderError;
use crate::backend::{DepthAttachment, GpuContext, OffscreenTarget};
use crate::bridge::{
    D3d9AdapterIdentifierResult, D3d9Bindings as BridgeBindings, D3d9BindingsResult,
    D3d9BlockBindingsResult, D3d9BufferDescriptor, D3d9BufferDescriptorResult, D3d9BufferKind,
    D3d9BufferLockRequest, D3d9BufferLockResult, D3d9BufferRequest, D3d9BufferUnlockResult,
    D3d9CapsResult, D3d9CheckDepthStencilRequest, D3d9CheckDeviceFormatRequest,
    D3d9CheckDeviceTypeRequest, D3d9CheckFormatConversionRequest, D3d9CheckMultiSampleRequest,
    D3d9CreateResult, D3d9CreationParameters, D3d9CreationParametersResult, D3d9DeclarationResult,
    D3d9DeviceRequest, D3d9DisplayMode, D3d9DisplayModeRequest, D3d9DisplayModeResult,
    D3d9FrameResult, D3d9LevelDescriptor, D3d9LevelDescriptorResult, D3d9LevelLockRequest,
    D3d9LevelLockResult, D3d9LightResult, D3d9PresentParams, D3d9PrivateDataResult,
    D3d9ResetRequest, D3d9ResetResult, D3d9ScissorResult, D3d9StreamResult, D3d9SurfaceDescriptor,
    D3d9SurfaceDescriptorResult, D3d9SurfaceRequest, D3d9TextureCreateResult,
    D3d9TextureDescriptor, D3d9TextureDescriptorResult, D3d9TextureKind, D3d9TextureRequest,
    D3d9VertexElement, FloatValuesResult, Status, StatusCode, ValueResult, ValuesResult, Viewport,
    ViewportResult, status,
};
use crate::d3d9::gpu::{D3d9Gpu, D3d9Loss};
pub use crate::d3d9::resource::D3d9Resource;
use crate::d3d9::resource::StagedStorage;
use crate::fast_map::FastMap;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub mod api;
pub mod copy;
pub mod d3dx;
pub mod d3dx_effect;
pub mod d3dx_math;
pub mod d3dx_shader;
pub mod draw;
pub mod format;
pub mod gpu;
mod present;
pub mod query;
pub mod resource;
pub mod shader;
pub mod state;
mod stats;
pub mod wgsl;

pub use d3dx::*;

fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RECOMP_D3D9_TRACE").is_ok_and(|v| v == "1"))
}

fn trace_draw_limit() -> u32 {
    static LIMIT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("RECOMP_D3D9_TRACE_DRAWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24)
    })
}
pub use d3dx_math::*;
pub use d3dx_shader::*;

const D3DDEVTYPE_HAL: u32 = 1;
const D3DDEVTYPE_REF: u32 = 2;
const D3DDEVTYPE_SW: u32 = 3;
const D3DDEVTYPE_NULLREF: u32 = 4;

const D3DCREATE_FPU_PRESERVE: u32 = 0x0000_0002;
const D3DCREATE_MULTITHREADED: u32 = 0x0000_0004;
const D3DCREATE_PUREDEVICE: u32 = 0x0000_0010;
const D3DCREATE_SOFTWARE_VERTEXPROCESSING: u32 = 0x0000_0020;
const D3DCREATE_HARDWARE_VERTEXPROCESSING: u32 = 0x0000_0040;
const D3DCREATE_MIXED_VERTEXPROCESSING: u32 = 0x0000_0080;
const D3DCREATE_DISABLE_DRIVER_MANAGEMENT: u32 = 0x0000_0100;
const D3DCREATE_ADAPTERGROUP_DEVICE: u32 = 0x0000_0200;
const D3DCREATE_DISABLE_DRIVER_MANAGEMENT_EX: u32 = 0x0000_0400;
const D3DCREATE_NOWINDOWCHANGES: u32 = 0x0000_0800;
const D3DCREATE_DISABLE_PSGP_THREADING: u32 = 0x0000_2000;
const D3DCREATE_ENABLE_PRESENTSTATS: u32 = 0x0000_4000;
const D3DCREATE_DISABLE_PRINTSCREEN: u32 = 0x0000_8000;
const D3DCREATE_SCREENSAVER: u32 = 0x1000_0000;

const D3DCREATE_KNOWN: u32 = D3DCREATE_FPU_PRESERVE
    | D3DCREATE_MULTITHREADED
    | D3DCREATE_PUREDEVICE
    | D3DCREATE_SOFTWARE_VERTEXPROCESSING
    | D3DCREATE_HARDWARE_VERTEXPROCESSING
    | D3DCREATE_MIXED_VERTEXPROCESSING
    | D3DCREATE_DISABLE_DRIVER_MANAGEMENT
    | D3DCREATE_ADAPTERGROUP_DEVICE
    | D3DCREATE_DISABLE_DRIVER_MANAGEMENT_EX
    | D3DCREATE_NOWINDOWCHANGES
    | D3DCREATE_DISABLE_PSGP_THREADING
    | D3DCREATE_ENABLE_PRESENTSTATS
    | D3DCREATE_DISABLE_PRINTSCREEN
    | D3DCREATE_SCREENSAVER;
const D3DCREATE_VERTEXPROCESSING: u32 = D3DCREATE_SOFTWARE_VERTEXPROCESSING
    | D3DCREATE_HARDWARE_VERTEXPROCESSING
    | D3DCREATE_MIXED_VERTEXPROCESSING;
const D3DCREATE_IMPLEMENTED: u32 = D3DCREATE_VERTEXPROCESSING
    | D3DCREATE_MULTITHREADED
    | D3DCREATE_PUREDEVICE
    | D3DCREATE_DISABLE_DRIVER_MANAGEMENT
    | D3DCREATE_DISABLE_DRIVER_MANAGEMENT_EX
    | D3DCREATE_FPU_PRESERVE
    | D3DCREATE_NOWINDOWCHANGES;

const D3DFMT_A8R8G8B8: u32 = 21;
const D3DFMT_X8R8G8B8: u32 = 22;

const D3DSWAPEFFECT_DISCARD: u32 = 1;
const D3DSWAPEFFECT_FLIP: u32 = 2;
const D3DSWAPEFFECT_COPY: u32 = 3;

const D3DPRESENTFLAG_KNOWN: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004 | 0x0000_0010;

const D3DPRESENT_INTERVAL_DEFAULT: u32 = 0x0000_0000;
const D3DPRESENT_INTERVAL_ONE: u32 = 0x0000_0001;
const D3DPRESENT_INTERVAL_TWO: u32 = 0x0000_0002;
const D3DPRESENT_INTERVAL_THREE: u32 = 0x0000_0004;
const D3DPRESENT_INTERVAL_FOUR: u32 = 0x0000_0008;
const D3DPRESENT_INTERVAL_IMMEDIATE: u32 = 0x8000_0000;

const D3DTEXF_LINEAR: u32 = 2;

const D3DCAPS2_CANAUTOGENMIPMAP: u32 = 0x4000_0000;
const D3DDEVCAPS2_STREAMOFFSET: u32 = 0x0000_0001;
const D3DPTEXTURECAPS_ALPHA: u32 = 0x0000_0004;
const D3DPTEXTURECAPS_CUBEMAP: u32 = 0x0000_0800;
const D3DPTEXTURECAPS_VOLUMEMAP: u32 = 0x0000_2000;
const D3DPTEXTURECAPS_MIPMAP: u32 = 0x0000_4000;
const D3DPTEXTURECAPS_MIPVOLUMEMAP: u32 = 0x0000_8000;
const D3DPTEXTURECAPS_MIPCUBEMAP: u32 = 0x0001_0000;
const D3DPTEXTURECAPS_PERSPECTIVE: u32 = 0x0000_0001;
const D3DPTEXTURECAPS_PROJECTED: u32 = 0x0000_0400;
const D3DPTFILTERCAPS_MINFPOINT: u32 = 0x0000_0100;
const D3DPTFILTERCAPS_MINFLINEAR: u32 = 0x0000_0200;
const D3DPTFILTERCAPS_MINFANISOTROPIC: u32 = 0x0000_0400;
const D3DPTFILTERCAPS_MIPFPOINT: u32 = 0x0001_0000;
const D3DPTFILTERCAPS_MIPFLINEAR: u32 = 0x0002_0000;
const D3DPTFILTERCAPS_MAGFPOINT: u32 = 0x0100_0000;
const D3DPTFILTERCAPS_MAGFLINEAR: u32 = 0x0200_0000;
const D3DPTFILTERCAPS_MAGFANISOTROPIC: u32 = 0x0400_0000;
const D3DPTFILTERCAPS_SAMPLING: u32 = D3DPTFILTERCAPS_MINFPOINT
    | D3DPTFILTERCAPS_MINFLINEAR
    | D3DPTFILTERCAPS_MINFANISOTROPIC
    | D3DPTFILTERCAPS_MIPFPOINT
    | D3DPTFILTERCAPS_MIPFLINEAR
    | D3DPTFILTERCAPS_MAGFPOINT
    | D3DPTFILTERCAPS_MAGFLINEAR
    | D3DPTFILTERCAPS_MAGFANISOTROPIC;
const D3DPTADDRESSCAPS_WRAP: u32 = 0x0000_0001;
const D3DPTADDRESSCAPS_MIRROR: u32 = 0x0000_0002;
const D3DPTADDRESSCAPS_CLAMP: u32 = 0x0000_0004;
const D3DPTADDRESSCAPS_BORDER: u32 = 0x0000_0008;
const D3DPTADDRESSCAPS_INDEPENDENTUV: u32 = 0x0000_0010;
const D3DPTADDRESSCAPS_ALL: u32 = D3DPTADDRESSCAPS_WRAP
    | D3DPTADDRESSCAPS_MIRROR
    | D3DPTADDRESSCAPS_CLAMP
    | D3DPTADDRESSCAPS_BORDER
    | D3DPTADDRESSCAPS_INDEPENDENTUV;
const D3DPMISCCAPS_MASKZ: u32 = 0x0000_0002;
const D3DPMISCCAPS_CULLNONE: u32 = 0x0000_0010;
const D3DPMISCCAPS_CULLCW: u32 = 0x0000_0020;
const D3DPMISCCAPS_CULLCCW: u32 = 0x0000_0040;
const D3DPMISCCAPS_COLORWRITEENABLE: u32 = 0x0000_0080;
const D3DPMISCCAPS_TSSARGTEMP: u32 = 0x0000_0400;
const D3DPMISCCAPS_BLENDOP: u32 = 0x0000_0800;
const D3DPMISCCAPS_SEPARATEALPHABLEND: u32 = 0x0002_0000;
const D3DPRASTERCAPS_ZTEST: u32 = 0x0000_0010;
const D3DPRASTERCAPS_FOGVERTEX: u32 = 0x0000_0080;
const D3DPRASTERCAPS_FOGTABLE: u32 = 0x0000_0100;
const D3DPRASTERCAPS_ANISOTROPY: u32 = 0x0002_0000;
const D3DPRASTERCAPS_SLOPESCALEDEPTHBIAS: u32 = 0x0200_0000;
const D3DPCMPCAPS_ALL: u32 = 0x0000_00ff;
const D3DPBLENDCAPS_DEST: u32 = 0x0000_23ff;
const D3DPBLENDCAPS_SRC: u32 = 0x0000_33ff;
const D3DPSHADECAPS_COLORGOURAUDRGB: u32 = 0x0000_0008;
const D3DPSHADECAPS_SPECULARGOURAUDRGB: u32 = 0x0000_0200;
const D3DSTENCILCAPS_ALL: u32 = 0x0000_01ff;
const D3DTEXOPCAPS_IMPLEMENTED: u32 = 0x0380_ffff;
const D3DVTXPCAPS_TEXGEN: u32 = 0x0000_0001;
const D3DVTXPCAPS_MATERIALSOURCE7: u32 = 0x0000_0002;
const D3DVTXPCAPS_DIRECTIONALLIGHTS: u32 = 0x0000_0008;
const D3DVTXPCAPS_POSITIONALLIGHTS: u32 = 0x0000_0010;
const D3DVTXPCAPS_LOCALVIEWER: u32 = 0x0000_0020;
const D3DPS20CAPS_ARBITRARYSWIZZLE: u32 = 0x0000_0001;
const D3DPS20CAPS_NODEPENDENTREADLIMIT: u32 = 0x0000_0008;
const D3DPS20CAPS_NOTEXINSTRUCTIONLIMIT: u32 = 0x0000_0010;
const D3DUSAGE_QUERY_FILTER: u32 = 0x0002_0000;
const D3DUSAGE_QUERY_SRGBWRITE: u32 = 0x0004_0000;
const D3DUSAGE_QUERY_POSTPIXELSHADER_BLENDING: u32 = 0x0008_0000;
const D3DUSAGE_QUERY_WRAPANDMIP: u32 = 0x0020_0000;
const D3DVS_VERSION_2_0: u32 = 0xfffe_0200;
const D3DPS_VERSION_2_0: u32 = 0xffff_0200;
const D3D_MAX_PRIMITIVE_COUNT: u32 = 0x000f_ffff;
const D3D_MAX_VERTEX_INDEX: u32 = 0x000f_ffff;

struct ImplicitTargets {
    target: OffscreenTarget,
    presented: OffscreenTarget,
    pending: OffscreenTarget,
    format: u32,
    depth_format: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone)]
pub(crate) struct SurfaceView {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub cube: bool,
    pub volume: bool,
    pub subresource: (u32, u32, u32),
}

#[derive(Clone)]
pub(crate) enum GpuRegistryEntry {
    Buffer {
        uploaded: Arc<Mutex<Arc<Vec<u8>>>>,
        format: u32,
    },
    Texture {
        texture: wgpu::Texture,
        kind: resource::TextureKind,
        format: u32,
        width: u32,
        height: u32,
        pool: u32,
        usage: u32,
        lod: Arc<AtomicU32>,
        fresh: Arc<AtomicBool>,
    },
    TextureAlias {
        gpu: Option<wgpu::Texture>,
        parent_fresh: Arc<AtomicBool>,
        kind: resource::TextureKind,
        format: u32,
        width: u32,
        height: u32,
        face: u32,
        level: u32,
        volume: bool,
        pool: u32,
        usage: u32,
        lod: Arc<AtomicU32>,
        fresh: Arc<AtomicBool>,
    },
    Surface {
        texture: wgpu::Texture,
        format: u32,
        width: u32,
        height: u32,
        pool: u32,
        usage: u32,
        discard: bool,
        fresh: Arc<AtomicBool>,
    },
}

struct DrawViews {
    key: (u32, u32, u64),
    target: SurfaceView,
    depth: Option<SurfaceView>,
}

struct DrawTextures {
    epoch: u64,
    bindings: [Option<CachedBinding>; state::MAX_SAMPLERS],
}

struct CachedBinding {
    id: u32,
    lod: Option<Arc<AtomicU32>>,
    binding: draw::BoundTexture,
}

struct RestorePlan {
    gpu: Option<Arc<GpuContext>>,
    d3d9_gpu: D3d9Gpu,
    loss: Arc<D3d9Loss>,
    targets: ImplicitTargets,
    present: D3d9PresentParams,
    display_mode: D3d9DisplayMode,
    expected: BTreeMap<u32, u32>,
    staged: BTreeMap<u32, StagedStorage>,
    registry: FastMap<u32, GpuRegistryEntry>,
    committed: usize,
    ready: bool,
}

struct ResourceRegistration {
    resource_type: u32,
    pool: u32,
}

struct D3d9Probe {
    description: String,
    vendor_id: u32,
    device_id: u32,
    max_texture_dimension_2d: u32,
    max_texture_dimension_3d: u32,
    features: wgpu::Features,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PresentParams {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub count: u32,
    pub multisample_type: u32,
    pub multisample_quality: u32,
    pub swap_effect: u32,
    pub windowed: u32,
    pub enable_auto_depth_stencil: u32,
    pub auto_depth_stencil_format: u32,
    pub flags: u32,
    pub fullscreen_refresh_rate: u32,
    pub presentation_interval: u32,
}

impl From<PresentParams> for D3d9PresentParams {
    fn from(value: PresentParams) -> Self {
        Self {
            width: value.width,
            height: value.height,
            format: value.format,
            count: value.count,
            multisample_type: value.multisample_type,
            multisample_quality: value.multisample_quality,
            swap_effect: value.swap_effect,
            windowed: value.windowed,
            enable_auto_depth_stencil: value.enable_auto_depth_stencil,
            auto_depth_stencil_format: value.auto_depth_stencil_format,
            flags: value.flags,
            fullscreen_refresh_rate: value.fullscreen_refresh_rate,
            presentation_interval: value.presentation_interval,
        }
    }
}

impl From<D3d9PresentParams> for PresentParams {
    fn from(value: D3d9PresentParams) -> Self {
        Self {
            width: value.width,
            height: value.height,
            format: value.format,
            count: value.count,
            multisample_type: value.multisample_type,
            multisample_quality: value.multisample_quality,
            swap_effect: value.swap_effect,
            windowed: value.windowed,
            enable_auto_depth_stencil: value.enable_auto_depth_stencil,
            auto_depth_stencil_format: value.auto_depth_stencil_format,
            flags: value.flags,
            fullscreen_refresh_rate: value.fullscreen_refresh_rate,
            presentation_interval: value.presentation_interval,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub format: u32,
}

impl From<DisplayMode> for D3d9DisplayMode {
    fn from(value: DisplayMode) -> Self {
        Self {
            width: value.width,
            height: value.height,
            refresh_rate: value.refresh_rate,
            format: value.format,
        }
    }
}

impl From<D3d9DisplayMode> for DisplayMode {
    fn from(value: D3d9DisplayMode) -> Self {
        Self {
            width: value.width,
            height: value.height,
            refresh_rate: value.refresh_rate,
            format: value.format,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceRequest {
    pub adapter: u32,
    pub device_type: u32,
    pub behavior_flags: u32,
    pub present: PresentParams,
    pub display_mode: DisplayMode,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceConfig {
    pub shader_mode: Option<String>,
    pub shader_mode_error: Option<String>,
    pub tint: bool,
}

impl DeviceConfig {
    fn from_environment() -> Self {
        let (shader_mode, shader_mode_error) = match std::env::var("RECOMP_D3D9_SHADER_MODE") {
            Ok(value) => (Some(value), None),
            Err(std::env::VarError::NotPresent) => (None, None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            shader_mode,
            shader_mode_error,
            tint: std::env::var("RECOMP_D3D9_UBER_TINT").is_ok_and(|value| value == "1"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceCreateErrorKind {
    InvalidArgument,
    Unsupported,
    Backend,
    OutOfMemory,
    InvalidData,
}

#[derive(Debug)]
pub struct DeviceCreateError {
    pub kind: DeviceCreateErrorKind,
    pub message: String,
    pub present: PresentParams,
}

impl core::fmt::Display for DeviceCreateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DeviceCreateError {}

pub struct D3d9Device {
    gpu: Option<Arc<GpuContext>>,
    d3d9_gpu: Option<D3d9Gpu>,
    adapter: u32,
    device_type: u32,
    behavior_flags: u32,
    present: D3d9PresentParams,
    display_mode: D3d9DisplayMode,
    loss: Option<Arc<D3d9Loss>>,
    targets: Option<ImplicitTargets>,
    resources: FastMap<u32, ResourceRegistration>,
    features: wgpu::Features,
    limits: wgpu::Limits,
    replacement: Option<(Arc<GpuContext>, Arc<D3d9Loss>)>,
    supplied_context: bool,
    restore: Option<RestorePlan>,
    state: state::D3d9State,
    objects: shader::D3d9Objects,
    registry: FastMap<u32, GpuRegistryEntry>,
    registry_epoch: u64,
    draw_views: Option<Box<DrawViews>>,
    draw_textures: Option<Box<DrawTextures>>,
    view_cache: RefCell<FastMap<TextureViewKey, wgpu::TextureView>>,
    locks: FastMap<u32, Vec<Arc<AtomicU32>>>,
    draw: draw::D3d9DrawState,
    present_ring: Option<present::PresentRing>,
    queries: query::D3d9Queries,
    render_target_id: u32,
    depth_target_id: u32,
    in_scene: bool,
    implicit_backbuffer_id: u32,
    implicit_depthbuffer_id: u32,
}

impl Drop for D3d9Device {
    fn drop(&mut self) {
        if let Some(gpu) = &self.d3d9_gpu {
            if !gpu.is_physical() {
                gpu.submit_pending();
            }
        }
    }
}

impl D3d9Device {
    pub fn new_owned(request: &DeviceRequest) -> Result<(Self, PresentParams), DeviceCreateError> {
        Self::new_owned_configured(request, &DeviceConfig::default())
    }

    pub fn new_owned_configured(
        request: &DeviceRequest,
        config: &DeviceConfig,
    ) -> Result<(Self, PresentParams), DeviceCreateError> {
        let mut device = Self::default();
        let present = create_device(&mut device, request, None, config)?;
        Ok((device, present))
    }

    pub fn new_supplied(
        request: &DeviceRequest,
        context: Arc<GpuContext>,
    ) -> Result<(Self, PresentParams), DeviceCreateError> {
        Self::new_supplied_configured(request, context, &DeviceConfig::default())
    }

    pub fn new_supplied_configured(
        request: &DeviceRequest,
        context: Arc<GpuContext>,
        config: &DeviceConfig,
    ) -> Result<(Self, PresentParams), DeviceCreateError> {
        let mut device = Self::default();
        let present = create_device(&mut device, request, Some(context), config)?;
        Ok((device, present))
    }

    pub fn supply_replacement(&mut self, context: Arc<GpuContext>) -> Result<(), RenderError> {
        if !self.supplied_context || self.gpu.is_none() {
            return Err(RenderError::invalid(
                "ReplaceContext",
                "device does not use a supplied context",
            ));
        }
        if self.restore.is_some() {
            return Err(RenderError::invalid(
                "ReplaceContext",
                "reset is in progress",
            ));
        }
        if context.physical_loss().is_lost() {
            return Err(RenderError::invalid(
                "ReplaceContext",
                "replacement context is lost",
            ));
        }
        if !context.device.features().contains(self.features)
            || !self.limits.check_limits(&context.device.limits())
        {
            return Err(RenderError::invalid(
                "ReplaceContext",
                "replacement context has incompatible capabilities",
            ));
        }
        let loss = Arc::new(D3d9Loss::from_physical(context.physical_loss()));
        self.replacement = Some((context, loss));
        Ok(())
    }

    fn resource_gpu(&self) -> Result<&D3d9Gpu, RenderError> {
        self.d3d9_gpu
            .as_ref()
            .ok_or_else(|| RenderError::invalid("D3D9", "device is not initialized"))
    }

    pub(crate) fn resource_pool(&self, id: u32) -> Option<u32> {
        self.resources
            .get(&id)
            .map(|registration| registration.pool)
    }
}

fn bridge_error(error: RenderError) -> Status {
    let code = match error.kind {
        crate::RenderErrorKind::InvalidArgument => StatusCode::InvalidArgument,
        crate::RenderErrorKind::InvalidData => StatusCode::InvalidData,
        crate::RenderErrorKind::OutOfMemory => StatusCode::OutOfMemory,
        crate::RenderErrorKind::Unsupported => StatusCode::Unsupported,
    };
    status(code, error.to_string())
}

fn bridge_ok(result: Result<(), RenderError>) -> Status {
    match result {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

fn bridge_gpu_ok(gpu: &D3d9Gpu, result: Result<(), RenderError>) -> Status {
    if gpu.is_physical() {
        status(StatusCode::DeviceLost, gpu.loss().physical_message())
    } else {
        bridge_ok(result)
    }
}

fn live_gpu(resource: &D3d9Resource) -> Result<D3d9Gpu, Status> {
    match resource.gpu_handle() {
        Ok(gpu) if !gpu.is_blocked() => Ok(gpu),
        Ok(_) => Err(status(
            StatusCode::DeviceLost,
            "the wgpu device is not operational",
        )),
        Err(error) => Err(bridge_error(error)),
    }
}

pub fn new_d3d9_resource() -> Box<D3d9Resource> {
    Box::default()
}

pub fn d3d9_create_buffer(
    device: &D3d9Device,
    resource: &mut D3d9Resource,
    request: &D3d9BufferRequest,
) -> Status {
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return bridge_error(error),
    };
    let kind = match request.kind {
        D3d9BufferKind::Vertex => resource::BufferKind::Vertex,
        D3d9BufferKind::Index => resource::BufferKind::Index,
        _ => return status(StatusCode::InvalidArgument, "invalid D3D9 buffer kind"),
    };
    let mapped = resource::BufferRequest {
        kind,
        size: request.size,
        usage: request.usage,
        pool: request.pool,
        fvf: request.fvf,
        format: request.format,
        shared_handle: request.shared_handle,
    };
    bridge_ok(resource::create_buffer(&gpu, resource, &mapped))
}

pub fn d3d9_buffer_descriptor(resource: &D3d9Resource) -> D3d9BufferDescriptorResult {
    match resource.state.as_ref() {
        Some(resource::ResourceState::Buffer(b)) => D3d9BufferDescriptorResult {
            status: status(StatusCode::Ok, ""),
            descriptor: D3d9BufferDescriptor {
                format: b.format,
                resource_type: resource.resource_type().unwrap_or(0),
                usage: b.usage,
                pool: b.pool,
                size: b.size,
                fvf: b.fvf,
            },
        },
        _ => D3d9BufferDescriptorResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 buffer is not initialized",
            ),
            descriptor: D3d9BufferDescriptor {
                format: 0,
                resource_type: 0,
                usage: 0,
                pool: 0,
                size: 0,
                fvf: 0,
            },
        },
    }
}

fn buffer_lock_request(request: &D3d9BufferLockRequest) -> resource::BufferLockRequest {
    resource::BufferLockRequest {
        offset: request.offset,
        size: request.size,
        flags: request.flags,
    }
}

fn buffer_lock_result(
    result: Result<resource::BufferLockRange, RenderError>,
) -> D3d9BufferLockResult {
    match result {
        Ok(range) => D3d9BufferLockResult {
            status: status(StatusCode::Ok, ""),
            offset: range.offset,
            size: range.size,
            total: range.total,
        },
        Err(error) => D3d9BufferLockResult {
            status: buffer_status(Err(error)),
            offset: 0,
            size: 0,
            total: 0,
        },
    }
}

pub fn d3d9_buffer_lock_prepare(
    resource: &mut D3d9Resource,
    request: &D3d9BufferLockRequest,
) -> D3d9BufferLockResult {
    buffer_lock_result(resource.prepare_buffer_lock(&buffer_lock_request(request)))
}

pub fn d3d9_buffer_lock_commit(
    resource: &mut D3d9Resource,
    request: &D3d9BufferLockRequest,
) -> D3d9BufferLockResult {
    let result = resource.commit_buffer_lock(&buffer_lock_request(request));
    if let (Ok(range), Some(resource::ResourceState::Buffer(b))) =
        (&result, resource.state.as_ref())
    {
        if trace_enabled() {
            static COUNT: AtomicU32 = AtomicU32::new(0);
            if COUNT.fetch_add(1, Ordering::Relaxed) < 128 {
                eprintln!(
                    "[d3d9-trace] buffer_lock resource={:p} pool={} format={} usage={} offset={} size={} flags={:#x}",
                    b as *const resource::BufferState,
                    b.pool,
                    b.format,
                    b.usage,
                    range.offset,
                    range.size,
                    request.flags
                );
            }
        }
    }
    buffer_lock_result(result)
}

pub fn d3d9_buffer_bytes(resource: &D3d9Resource) -> &[u8] {
    resource.buffer_bytes()
}

pub fn d3d9_buffer_sync_locked(resource: &mut D3d9Resource, bytes: &[u8]) -> Status {
    buffer_status(resource.sync_locked_buffer(bytes))
}

pub fn d3d9_buffer_unlock(resource: &mut D3d9Resource, bytes: &[u8]) -> D3d9BufferUnlockResult {
    let result = resource.unlock_buffer(bytes);
    D3d9BufferUnlockResult {
        status: buffer_status(result.status),
        closed: result.closed,
    }
}

fn buffer_status(result: Result<(), RenderError>) -> Status {
    match result {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => {
            let mut mapped = bridge_error(error.clone());
            if matches!(error.operation, "LockBuffer" | "UnlockBuffer") {
                mapped.message = error.cause;
            }
            mapped
        }
    }
}

pub fn d3d9_resource_type(resource: &D3d9Resource) -> ValueResult {
    match resource.resource_type() {
        Some(value) => ValueResult {
            status: status(StatusCode::Ok, ""),
            value,
        },
        None => ValueResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 resource is not initialized",
            ),
            value: 0,
        },
    }
}

fn texture_kind(kind: D3d9TextureKind) -> Result<resource::TextureKind, RenderError> {
    match kind {
        D3d9TextureKind::Texture2D => Ok(resource::TextureKind::Texture2D),
        D3d9TextureKind::Cube => Ok(resource::TextureKind::Cube),
        D3d9TextureKind::Volume => Ok(resource::TextureKind::Volume),
        _ => Err(RenderError::invalid(
            "CreateTexture",
            "invalid D3D9 texture kind",
        )),
    }
}

fn texture_no_autogen(resource: &D3d9Resource, usage: u32) -> bool {
    usage & resource::D3DUSAGE_AUTOGENMIPMAP != 0
        && !matches!(resource.state.as_ref(), Some(resource::ResourceState::Texture(t)) if t.autogen)
}

pub fn d3d9_create_texture(
    device: &D3d9Device,
    resource: &mut D3d9Resource,
    request: &D3d9TextureRequest,
) -> D3d9TextureCreateResult {
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => {
            return D3d9TextureCreateResult {
                status: bridge_error(error),
                no_autogen: false,
            };
        }
    };
    let kind = match texture_kind(request.kind) {
        Ok(kind) => kind,
        Err(error) => {
            return D3d9TextureCreateResult {
                status: bridge_error(error),
                no_autogen: false,
            };
        }
    };
    let mapped = resource::TextureRequest {
        kind,
        width: request.width,
        height: request.height,
        depth: request.depth,
        levels: request.levels,
        usage: request.usage,
        format: request.format,
        pool: request.pool,
        shared_handle: request.shared_handle,
    };
    match resource::create_texture(&gpu, resource, &mapped) {
        Ok(()) => {
            let no_autogen = texture_no_autogen(resource, request.usage);
            D3d9TextureCreateResult {
                status: status(StatusCode::Ok, ""),
                no_autogen,
            }
        }
        Err(error) => D3d9TextureCreateResult {
            status: bridge_error(error),
            no_autogen: false,
        },
    }
}

pub fn d3d9_create_surface(
    device: &D3d9Device,
    resource: &mut D3d9Resource,
    request: &D3d9SurfaceRequest,
) -> Status {
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return bridge_error(error),
    };
    let mapped = resource::SurfaceRequest {
        width: request.width,
        height: request.height,
        format: request.format,
        pool: request.pool,
        usage: request.usage,
        lockable: request.lockable,
        discard: request.discard,
        multisample_type: request.multisample_type,
        multisample_quality: request.multisample_quality,
        shared_handle: request.shared_handle,
    };
    bridge_ok(resource::create_surface(&gpu, resource, &mapped))
}

pub fn d3d9_texture_descriptor(resource: &D3d9Resource) -> D3d9TextureDescriptorResult {
    match resource.state.as_ref() {
        Some(resource::ResourceState::Texture(t)) => D3d9TextureDescriptorResult {
            status: status(StatusCode::Ok, ""),
            descriptor: D3d9TextureDescriptor {
                format: t.format,
                resource_type: resource.resource_type().unwrap_or(0),
                usage: t.usage,
                pool: t.pool,
                width: t.width,
                height: t.height,
                depth: t.depth,
                levels: t.levels,
            },
        },
        _ => D3d9TextureDescriptorResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 texture is not initialized",
            ),
            descriptor: D3d9TextureDescriptor {
                format: 0,
                resource_type: 0,
                usage: 0,
                pool: 0,
                width: 0,
                height: 0,
                depth: 0,
                levels: 0,
            },
        },
    }
}

pub fn d3d9_surface_descriptor(resource: &D3d9Resource) -> D3d9SurfaceDescriptorResult {
    match resource.state.as_ref() {
        Some(resource::ResourceState::Surface(s)) => D3d9SurfaceDescriptorResult {
            status: status(StatusCode::Ok, ""),
            descriptor: D3d9SurfaceDescriptor {
                format: s.format,
                resource_type: resource.resource_type().unwrap_or(0),
                usage: s.usage,
                pool: s.pool,
                multisample_type: s.multisample_type,
                multisample_quality: s.multisample_quality,
                width: s.width,
                height: s.height,
            },
        },
        _ => D3d9SurfaceDescriptorResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 surface is not initialized",
            ),
            descriptor: D3d9SurfaceDescriptor {
                format: 0,
                resource_type: 0,
                usage: 0,
                pool: 0,
                multisample_type: 0,
                multisample_quality: 0,
                width: 0,
                height: 0,
            },
        },
    }
}

fn level_descriptor(resource: &D3d9Resource, face: u32, level: u32) -> D3d9LevelDescriptorResult {
    let format = match resource.state.as_ref() {
        Some(resource::ResourceState::Texture(t)) => t.format,
        Some(resource::ResourceState::Surface(s)) => s.format,
        _ => {
            return D3d9LevelDescriptorResult {
                status: status(
                    StatusCode::InvalidArgument,
                    "D3D9 texture is not initialized",
                ),
                descriptor: D3d9LevelDescriptor {
                    format: 0,
                    width: 0,
                    height: 0,
                    depth: 0,
                    levels: 0,
                },
            };
        }
    };
    match resource::level_descriptor(resource, face, level) {
        Ok((width, height, depth, levels)) => D3d9LevelDescriptorResult {
            status: status(StatusCode::Ok, ""),
            descriptor: D3d9LevelDescriptor {
                format,
                width,
                height,
                depth,
                levels,
            },
        },
        Err(error) => D3d9LevelDescriptorResult {
            status: bridge_error(error),
            descriptor: D3d9LevelDescriptor {
                format: 0,
                width: 0,
                height: 0,
                depth: 0,
                levels: 0,
            },
        },
    }
}

pub fn d3d9_texture_level_descriptor(
    resource: &D3d9Resource,
    face: u32,
    level: u32,
) -> D3d9LevelDescriptorResult {
    level_descriptor(resource, face, level)
}

pub fn d3d9_texture_volume_descriptor(
    resource: &D3d9Resource,
    level: u32,
) -> D3d9LevelDescriptorResult {
    level_descriptor(resource, 0, level)
}

fn map_lock_request(request: &D3d9LevelLockRequest) -> resource::LockRequest {
    resource::LockRequest {
        face: request.face,
        level: request.level,
        volume: request.volume,
        full_region: request.full_region,
        left: if request.volume {
            request.volume_box.left as i64
        } else {
            request.rect.left as i64
        },
        top: if request.volume {
            request.volume_box.top as i64
        } else {
            request.rect.top as i64
        },
        front: request.volume_box.front as i64,
        right: if request.volume {
            request.volume_box.right as i64
        } else {
            request.rect.right as i64
        },
        bottom: if request.volume {
            request.volume_box.bottom as i64
        } else {
            request.rect.bottom as i64
        },
        back: request.volume_box.back as i64,
        flags: request.flags,
    }
}

fn lock_result_fail(error: RenderError) -> D3d9LevelLockResult {
    D3d9LevelLockResult {
        status: bridge_error(error),
        pitch: 0,
        slice_pitch: 0,
        offset: 0,
        size: 0,
        total: 0,
    }
}

fn level_lock_prepare(
    resource: &mut D3d9Resource,
    request: &resource::LockRequest,
) -> Result<resource::LockResult, api::ApiError> {
    resource::validate_level_lock(resource, request)?;
    let gpu = resource.gpu_handle()?;
    if request.flags & resource::D3DLOCK_DONOTWAIT != 0
        && request.flags & resource::D3DLOCK_DISCARD == 0
        && !gpu.is_blocked()
        && resource::lock_would_block(resource, request.face, request.level)
    {
        return Err(api::ApiError {
            kind: api::ApiErrorKind::WasStillDrawing,
            message: "the surface is busy and cannot be locked without waiting".into(),
        });
    }
    Ok(resource::level_lock_prepare(&gpu, resource, request)?)
}

pub fn d3d9_level_lock_prepare(
    resource: &mut D3d9Resource,
    request: &D3d9LevelLockRequest,
) -> D3d9LevelLockResult {
    match level_lock_prepare(resource, &map_lock_request(request)) {
        Ok(result) => D3d9LevelLockResult {
            status: status(StatusCode::Ok, ""),
            pitch: result.pitch,
            slice_pitch: result.slice_pitch,
            offset: result.offset,
            size: result.size,
            total: result.total,
        },
        Err(error) => D3d9LevelLockResult {
            status: status(error.kind.status_code(), error.message),
            pitch: 0,
            slice_pitch: 0,
            offset: 0,
            size: 0,
            total: 0,
        },
    }
}

pub fn d3d9_level_lock_commit(
    resource: &mut D3d9Resource,
    request: &D3d9LevelLockRequest,
) -> D3d9LevelLockResult {
    let gpu = match resource.gpu_handle() {
        Ok(gpu) => gpu,
        Err(error) => return lock_result_fail(error),
    };
    match resource::level_lock_commit(&gpu, resource, &map_lock_request(request)) {
        Ok(result) => D3d9LevelLockResult {
            status: status(StatusCode::Ok, ""),
            pitch: result.pitch,
            slice_pitch: result.slice_pitch,
            offset: result.offset,
            size: result.size,
            total: result.total,
        },
        Err(error) => lock_result_fail(error),
    }
}

pub fn d3d9_level_bytes(resource: &D3d9Resource, face: u32, level: u32) -> &[u8] {
    resource::level_bytes(resource, face, level).unwrap_or(&[])
}

fn level_unlock(
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    bytes: &[u8],
) -> (Result<(), RenderError>, bool) {
    let closed = match resource::level_unlock(resource, face, level, bytes) {
        Ok(closed) => closed,
        Err(error) => return (Err(error), false),
    };
    if closed && (resource::is_default_pool(resource) || resource.is_managed()) {
        if let Some(gpu) = resource.gpu_handle().ok().filter(|gpu| !gpu.is_blocked()) {
            let result = resource::upload_level(&gpu, resource, face, level).and_then(|()| {
                if resource::autogen_capable(resource) {
                    resource::generate_mips(&gpu, resource)
                } else {
                    Ok(())
                }
            });
            if let Err(error) = result {
                if !gpu.is_physical() {
                    return (Err(error), true);
                }
            }
        }
    }
    (Ok(()), closed)
}

pub fn d3d9_level_unlock(
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    bytes: &[u8],
) -> D3d9BufferUnlockResult {
    let (result, closed) = level_unlock(resource, face, level, bytes);
    D3d9BufferUnlockResult {
        status: bridge_ok(result),
        closed,
    }
}

pub fn d3d9_set_priority(resource: &mut D3d9Resource, priority: u32) -> ValueResult {
    let old = resource.set_priority(priority);
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: old,
    }
}

pub fn d3d9_priority(resource: &D3d9Resource) -> ValueResult {
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: resource.priority,
    }
}

pub fn d3d9_set_lod(resource: &mut D3d9Resource, lod: u32) -> ValueResult {
    let old = resource.set_lod(lod);
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: old,
    }
}

pub fn d3d9_lod(resource: &D3d9Resource) -> ValueResult {
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: resource.lod.load(Ordering::Acquire),
    }
}

pub fn d3d9_set_private_data(
    resource: &mut D3d9Resource,
    view: u32,
    guid: &[u8],
    data: &[u8],
    flags: u32,
) -> Status {
    if flags & resource::D3DSPD_IUNKNOWN != 0 {
        return status(
            StatusCode::Unsupported,
            "D3DSPD_IUNKNOWN private data is not implemented",
        );
    }
    if flags != 0 {
        return status(StatusCode::InvalidArgument, "unknown private-data flag");
    }
    if guid.len() != 16 {
        return status(
            StatusCode::InvalidArgument,
            "private-data GUID must be 16 bytes",
        );
    }
    let mut key = [0u8; 16];
    key.copy_from_slice(guid);
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(data.len()).is_err() {
        return status(StatusCode::OutOfMemory, "private-data allocation failed");
    }
    bytes.extend_from_slice(data);
    match resource.set_private_data(view, key, bytes) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_get_private_data(
    resource: &D3d9Resource,
    view: u32,
    guid: &[u8],
) -> D3d9PrivateDataResult {
    if guid.len() != 16 {
        return D3d9PrivateDataResult {
            status: status(
                StatusCode::InvalidArgument,
                "private-data GUID must be 16 bytes",
            ),
            found: false,
            data: Vec::new(),
        };
    }
    let mut key = [0u8; 16];
    key.copy_from_slice(guid);
    match resource.get_private_data(view, &key) {
        Some(bytes) => match format::copy_bytes(bytes) {
            Ok(data) => D3d9PrivateDataResult {
                status: status(StatusCode::Ok, ""),
                found: true,
                data,
            },
            Err(error) => D3d9PrivateDataResult {
                status: bridge_error(error),
                found: false,
                data: Vec::new(),
            },
        },
        None => D3d9PrivateDataResult {
            status: status(StatusCode::Ok, ""),
            found: false,
            data: Vec::new(),
        },
    }
}

pub fn d3d9_free_private_data(resource: &mut D3d9Resource, view: u32, guid: &[u8]) -> Status {
    if guid.len() != 16 {
        return status(
            StatusCode::InvalidArgument,
            "private-data GUID must be 16 bytes",
        );
    }
    let mut key = [0u8; 16];
    key.copy_from_slice(guid);
    if resource.free_private_data(view, &key) {
        status(StatusCode::Ok, "")
    } else {
        status(StatusCode::InvalidArgument, "private data is not present")
    }
}

pub fn d3d9_add_dirty_rect(
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    full_region: bool,
    rect: &crate::bridge::D3d9LockRect,
) -> Status {
    bridge_ok(resource::add_dirty_rect(
        resource,
        face,
        level,
        full_region,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
    ))
}

pub fn d3d9_upload_buffer(resource: &mut D3d9Resource) -> Status {
    bridge_ok(resource::upload_buffer(resource))
}

pub fn d3d9_upload_level(resource: &mut D3d9Resource, face: u32, level: u32) -> Status {
    let gpu = match live_gpu(resource) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    bridge_gpu_ok(&gpu, resource::upload_level(&gpu, resource, face, level))
}

pub fn d3d9_readback_level(resource: &mut D3d9Resource, face: u32, level: u32) -> Status {
    let gpu = match live_gpu(resource) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    bridge_gpu_ok(
        &gpu,
        resource::readback_level_explicit(&gpu, resource, face, level),
    )
}

pub fn d3d9_preload(resource: &mut D3d9Resource) -> Status {
    if !resource.initialized() {
        return status(StatusCode::Ok, "");
    }
    let gpu = match live_gpu(resource) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    bridge_gpu_ok(&gpu, resource::preload(&gpu, resource))
}

pub fn d3d9_add_dirty_box(
    resource: &mut D3d9Resource,
    level: u32,
    full_region: bool,
    bounds: &crate::bridge::D3d9LockBox,
) -> Status {
    bridge_ok(resource::add_dirty_box(
        resource,
        level,
        full_region,
        bounds.left,
        bounds.top,
        bounds.right,
        bounds.bottom,
        bounds.front,
        bounds.back,
    ))
}

pub fn d3d9_set_autogen_filter(resource: &mut D3d9Resource, filter: u32) -> Status {
    bridge_ok(resource::set_autogen_filter(resource, filter))
}

pub fn d3d9_get_autogen_filter(resource: &D3d9Resource) -> ValueResult {
    match resource::autogen_filter(resource) {
        Ok(value) => ValueResult {
            status: status(StatusCode::Ok, ""),
            value,
        },
        Err(error) => ValueResult {
            status: bridge_error(error),
            value: 0,
        },
    }
}

pub fn d3d9_generate_mips(resource: &mut D3d9Resource) -> Status {
    let gpu = match live_gpu(resource) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    bridge_gpu_ok(&gpu, resource::generate_mips(&gpu, resource))
}

pub fn d3d9_autogen_capable(resource: &D3d9Resource) -> ValueResult {
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: u32::from(resource::autogen_capable(resource)),
    }
}

pub fn d3d9_format_autogen_capable(format: u32) -> ValueResult {
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: u32::from(format::autogen_capable(format)),
    }
}

impl Default for D3d9Device {
    fn default() -> Self {
        Self {
            gpu: None,
            d3d9_gpu: None,
            adapter: 0,
            device_type: 0,
            behavior_flags: 0,
            loss: None,
            targets: None,
            resources: FastMap::default(),
            features: wgpu::Features::empty(),
            limits: wgpu::Limits::default(),
            replacement: None,
            supplied_context: false,
            restore: None,
            state: state::D3d9State::default(),
            objects: shader::D3d9Objects::default(),
            registry: FastMap::default(),
            registry_epoch: 0,
            draw_views: None,
            draw_textures: None,
            view_cache: RefCell::new(FastMap::default()),
            locks: FastMap::default(),
            draw: draw::D3d9DrawState::default(),
            present_ring: None,
            queries: query::D3d9Queries::new(),
            render_target_id: 0,
            depth_target_id: 0,
            in_scene: false,
            implicit_backbuffer_id: 0,
            implicit_depthbuffer_id: 0,
            present: D3d9PresentParams {
                width: 0,
                height: 0,
                format: 0,
                count: 0,
                multisample_type: 0,
                multisample_quality: 0,
                swap_effect: 0,
                windowed: 0,
                enable_auto_depth_stencil: 0,
                auto_depth_stencil_format: 0,
                flags: 0,
                fullscreen_refresh_rate: 0,
                presentation_interval: 0,
            },
            display_mode: D3d9DisplayMode {
                width: 0,
                height: 0,
                refresh_rate: 0,
                format: 0,
            },
        }
    }
}

pub fn new_d3d9_device() -> Box<D3d9Device> {
    Box::default()
}

fn validate_present_params(
    request: &D3d9PresentParams,
    display_mode: &D3d9DisplayMode,
) -> Result<D3d9PresentParams, RenderError> {
    let mut present = *request;
    if present.count == 0 {
        present.count = 1;
    }
    if present.count > 1 {
        return Err(RenderError::new(
            "D3DPRESENT_PARAMETERS",
            "back buffer count greater than one is not implemented",
        ));
    }
    let fullscreen = present.windowed == 0;
    if present.format == 0 {
        if fullscreen {
            return Err(RenderError::invalid(
                "D3DPRESENT_PARAMETERS",
                "fullscreen device requires a back buffer format",
            ));
        }
        if display_mode.format == 0 {
            return Err(RenderError::new(
                "D3DPRESENT_PARAMETERS",
                "windowed D3DFMT_UNKNOWN needs a desktop format the host cannot provide",
            ));
        }
        present.format = display_mode.format;
    }
    if present.format != D3DFMT_A8R8G8B8 && present.format != D3DFMT_X8R8G8B8 {
        return Err(RenderError::new(
            "D3DPRESENT_PARAMETERS",
            format!(
                "back buffer D3DFORMAT {} is not implemented; only A8R8G8B8 (21) and X8R8G8B8 (22)",
                present.format
            ),
        ));
    }
    if fullscreen {
        if display_mode.width == 0 || display_mode.height == 0 || display_mode.format == 0 {
            return Err(RenderError::new(
                "D3DPRESENT_PARAMETERS",
                "fullscreen device has no enumerated display mode",
            ));
        }
        let format_matches = display_mode.format == D3DFMT_X8R8G8B8
            && (present.format == D3DFMT_X8R8G8B8 || present.format == D3DFMT_A8R8G8B8);
        if !format_matches {
            return Err(RenderError::invalid(
                "D3DPRESENT_PARAMETERS",
                format!(
                    "fullscreen back buffer D3DFORMAT {} does not match enumerated display D3DFORMAT {}",
                    present.format, display_mode.format
                ),
            ));
        }
        if present.width != display_mode.width || present.height != display_mode.height {
            return Err(RenderError::invalid(
                "D3DPRESENT_PARAMETERS",
                format!(
                    "fullscreen back buffer {}x{} is not the enumerated display mode {}x{}",
                    present.width, present.height, display_mode.width, display_mode.height
                ),
            ));
        }
        if present.fullscreen_refresh_rate != 0
            && present.fullscreen_refresh_rate != display_mode.refresh_rate
        {
            return Err(RenderError::invalid(
                "D3DPRESENT_PARAMETERS",
                format!(
                    "fullscreen refresh rate {} is not the enumerated display refresh {}",
                    present.fullscreen_refresh_rate, display_mode.refresh_rate
                ),
            ));
        }
    } else if present.width == 0 || present.height == 0 {
        return Err(RenderError::invalid(
            "D3DPRESENT_PARAMETERS",
            "back buffer dimensions must be nonzero",
        ));
    }
    match present.swap_effect {
        D3DSWAPEFFECT_DISCARD | D3DSWAPEFFECT_FLIP | D3DSWAPEFFECT_COPY => {}
        other => {
            return Err(RenderError::invalid(
                "D3DPRESENT_PARAMETERS",
                format!("unknown D3DSWAPEFFECT {other}"),
            ));
        }
    }
    if present.multisample_type != 0 {
        return Err(RenderError::new(
            "D3DPRESENT_PARAMETERS",
            format!(
                "D3DMULTISAMPLE_TYPE {} is not implemented",
                present.multisample_type
            ),
        ));
    }
    if present.multisample_quality != 0 {
        return Err(RenderError::invalid(
            "D3DPRESENT_PARAMETERS",
            "multisample quality must be zero without multisampling",
        ));
    }
    if !matches!(
        present.presentation_interval,
        D3DPRESENT_INTERVAL_DEFAULT
            | D3DPRESENT_INTERVAL_ONE
            | D3DPRESENT_INTERVAL_TWO
            | D3DPRESENT_INTERVAL_THREE
            | D3DPRESENT_INTERVAL_FOUR
            | D3DPRESENT_INTERVAL_IMMEDIATE
    ) {
        return Err(RenderError::new(
            "D3DPRESENT_PARAMETERS",
            format!(
                "presentation interval {:#x} is not a D3DPRESENT_INTERVAL",
                present.presentation_interval
            ),
        ));
    }
    if !fullscreen && present.fullscreen_refresh_rate != 0 {
        return Err(RenderError::invalid(
            "D3DPRESENT_PARAMETERS",
            "windowed device requires a zero fullscreen refresh rate",
        ));
    }
    let unknown_flags = present.flags & !D3DPRESENTFLAG_KNOWN;
    if unknown_flags != 0 {
        return Err(RenderError::invalid(
            "D3DPRESENT_PARAMETERS",
            format!("unknown D3DPRESENTFLAG(s) {unknown_flags:#x}"),
        ));
    }
    if present.flags != 0 {
        return Err(RenderError::new(
            "D3DPRESENT_PARAMETERS",
            format!("D3DPRESENTFLAG(s) {:#x} are not implemented", present.flags),
        ));
    }
    if present.enable_auto_depth_stencil != 0 {
        if present.auto_depth_stencil_format == 0 {
            return Err(RenderError::invalid(
                "D3DPRESENT_PARAMETERS",
                "automatic depth stencil requires a format",
            ));
        }
        if !format::is_depth(present.auto_depth_stencil_format) {
            return Err(RenderError::new(
                "D3DPRESENT_PARAMETERS",
                format!(
                    "automatic depth stencil format {} is not implemented",
                    present.auto_depth_stencil_format
                ),
            ));
        }
    } else {
        present.auto_depth_stencil_format = 0;
    }
    Ok(present)
}

fn create_d3d9_depth(
    gpu: &GpuContext,
    width: u32,
    height: u32,
    depth_format: u32,
) -> Result<DepthAttachment, RenderError> {
    let wgpu_format = format::d3d_to_wgpu(depth_format).ok_or_else(|| {
        RenderError::new(
            "D3D9DepthTarget",
            format!("D3DFORMAT {depth_format} has no depth mapping"),
        )
    })?;
    let max = gpu.device.limits().max_texture_dimension_2d;
    if width == 0 || height == 0 || width > max || height > max {
        return Err(RenderError::new(
            "D3D9DepthTarget",
            format!("depth target {width}x{height} exceeds max_texture_dimension_2d {max}"),
        ));
    }
    let d3d9_gpu = D3d9Gpu::from_context(gpu);
    d3d9_gpu.validate("D3D9DepthTarget", || {
        let texture = d3d9_gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("d3d9-implicit-depth"),
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
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(DepthAttachment {
            texture,
            view,
            format: wgpu_format,
            has_stencil: depth_format == format::D3DFMT_D24S8,
            d3d_format: depth_format,
        })
    })
}

fn create_targets(
    gpu: &GpuContext,
    present: &D3d9PresentParams,
) -> Result<ImplicitTargets, RenderError> {
    let depth_format = if present.enable_auto_depth_stencil != 0 {
        present.auto_depth_stencil_format
    } else {
        0
    };
    let mut target = gpu.create_target(present.width, present.height, present.format, 0)?;
    if depth_format != 0 {
        target.depth = Some(create_d3d9_depth(
            gpu,
            present.width,
            present.height,
            depth_format,
        )?);
    }
    let presented = gpu.create_target(present.width, present.height, present.format, 0)?;
    let pending = gpu.create_target(present.width, present.height, present.format, 0)?;
    Ok(ImplicitTargets {
        target,
        presented,
        pending,
        format: present.format,
        depth_format,
    })
}

fn attach_loss(gpu: &GpuContext, loss: &Arc<D3d9Loss>) {
    let callback_loss = Arc::clone(loss);
    gpu.device.set_device_lost_callback(move |reason, message| {
        callback_loss.mark_physical(format!("{reason:?}: {message}"));
    });
    let handler_loss = Arc::clone(loss);
    let handler: Arc<dyn wgpu::UncapturedErrorHandler> = Arc::new(move |error: wgpu::Error| {
        if handler_loss.is_physical() {
            log::error!("d3d9-wgpu error during physical device loss: {error}");
        } else {
            panic!("d3d9-wgpu uncaptured wgpu error: {error}");
        }
    });
    gpu.device.on_uncaptured_error(handler);
}

pub fn d3d9_create_device(
    device: &mut D3d9Device,
    request: &D3d9DeviceRequest,
) -> D3d9CreateResult {
    let request = DeviceRequest {
        adapter: request.adapter,
        device_type: request.device_type,
        behavior_flags: request.behavior_flags,
        present: request.present.into(),
        display_mode: request.display_mode.into(),
    };
    match create_owned_device(device, &request) {
        Ok(present) => D3d9CreateResult {
            status: status(StatusCode::Ok, ""),
            present: present.into(),
        },
        Err(error) => {
            let code = match error.kind {
                DeviceCreateErrorKind::InvalidArgument => StatusCode::InvalidArgument,
                DeviceCreateErrorKind::Unsupported => StatusCode::Unsupported,
                DeviceCreateErrorKind::Backend => StatusCode::Backend,
                DeviceCreateErrorKind::OutOfMemory => StatusCode::OutOfMemory,
                DeviceCreateErrorKind::InvalidData => StatusCode::InvalidData,
            };
            D3d9CreateResult {
                status: status(code, error.message),
                present: error.present.into(),
            }
        }
    }
}

fn create_error(
    present: &PresentParams,
    kind: DeviceCreateErrorKind,
    message: impl Into<String>,
) -> DeviceCreateError {
    DeviceCreateError {
        kind,
        message: message.into(),
        present: *present,
    }
}

fn create_render_error(present: PresentParams, error: RenderError) -> DeviceCreateError {
    let kind = match error.kind {
        crate::RenderErrorKind::InvalidArgument => DeviceCreateErrorKind::InvalidArgument,
        crate::RenderErrorKind::Unsupported => DeviceCreateErrorKind::Unsupported,
        crate::RenderErrorKind::OutOfMemory => DeviceCreateErrorKind::OutOfMemory,
        crate::RenderErrorKind::InvalidData => DeviceCreateErrorKind::InvalidData,
    };
    create_error(&present, kind, error.to_string())
}

fn create_owned_device(
    device: &mut D3d9Device,
    request: &DeviceRequest,
) -> Result<PresentParams, DeviceCreateError> {
    create_device(device, request, None, &DeviceConfig::from_environment())
}

fn create_device(
    device: &mut D3d9Device,
    request: &DeviceRequest,
    supplied: Option<Arc<GpuContext>>,
    config: &DeviceConfig,
) -> Result<PresentParams, DeviceCreateError> {
    if device.gpu.is_some() {
        return Err(create_error(
            &request.present,
            DeviceCreateErrorKind::InvalidArgument,
            "D3D9 device is already initialized",
        ));
    }
    if request.adapter != 0 {
        return Err(create_error(
            &request.present,
            DeviceCreateErrorKind::InvalidArgument,
            format!(
                "adapter {} does not exist; only adapter 0 is available",
                request.adapter
            ),
        ));
    }
    if request.device_type != D3DDEVTYPE_HAL {
        return Err(create_error(
            &request.present,
            DeviceCreateErrorKind::Unsupported,
            format!(
                "D3DDEVTYPE {} is not implemented; only D3DDEVTYPE_HAL",
                request.device_type
            ),
        ));
    }
    let unknown = request.behavior_flags & !D3DCREATE_KNOWN;
    if unknown != 0 {
        return Err(create_error(
            &request.present,
            DeviceCreateErrorKind::InvalidArgument,
            format!("unknown D3DCREATE flag(s) {unknown:#x}"),
        ));
    }
    let vertex_processing = request.behavior_flags & D3DCREATE_VERTEXPROCESSING;
    if vertex_processing == 0 || vertex_processing.count_ones() != 1 {
        return Err(create_error(
            &request.present,
            DeviceCreateErrorKind::InvalidArgument,
            "exactly one vertex processing D3DCREATE flag is required",
        ));
    }
    let extra = request.behavior_flags & !D3DCREATE_IMPLEMENTED;
    if extra != 0 {
        return Err(create_error(
            &request.present,
            DeviceCreateErrorKind::Unsupported,
            format!("D3DCREATE flag(s) {extra:#x} are not implemented"),
        ));
    }
    let bridge_present: D3d9PresentParams = request.present.into();
    let bridge_display: D3d9DisplayMode = request.display_mode.into();
    let present = validate_present_params(&bridge_present, &bridge_display)
        .map_err(|error| create_render_error(request.present, error))?;
    let normalized = present.into();
    let is_supplied = supplied.is_some();
    let (gpu, loss) = match supplied {
        Some(gpu) => {
            let loss = Arc::new(D3d9Loss::from_physical(gpu.physical_loss()));
            (gpu, loss)
        }
        None => {
            let gpu = pollster::block_on(GpuContext::new_headless_d3d9()).map_err(|error| {
                create_error(
                    &normalized,
                    DeviceCreateErrorKind::Backend,
                    error.to_string(),
                )
            })?;
            let loss = Arc::new(D3d9Loss::from_physical(gpu.physical_loss()));
            attach_loss(&gpu, &loss);
            (Arc::new(gpu), loss)
        }
    };
    if loss.is_blocked() {
        return Err(create_error(
            &normalized,
            DeviceCreateErrorKind::Backend,
            "supplied device is lost",
        ));
    }
    let targets =
        create_targets(&gpu, &present).map_err(|error| create_render_error(normalized, error))?;
    device.features = gpu.device.features();
    device.limits = gpu.device.limits();
    let mut d3d9_gpu = D3d9Gpu::from_context_with_loss(&gpu, loss.as_ref().clone());
    d3d9_gpu.shader_mode = config.shader_mode.clone();
    d3d9_gpu.shader_mode_error = config.shader_mode_error.clone();
    d3d9_gpu.tint = config.tint;
    draw::uber::prepare(&d3d9_gpu).map_err(|error| create_render_error(normalized, error))?;
    device.d3d9_gpu = Some(d3d9_gpu);
    device.gpu = Some(gpu);
    device.supplied_context = is_supplied;
    device.adapter = request.adapter;
    device.device_type = request.device_type;
    device.behavior_flags = request.behavior_flags;
    device.present = present;
    device.display_mode = bridge_display;
    device.state.set_target_size(present.width, present.height);
    if present.enable_auto_depth_stencil != 0 {
        device.depth_target_id = u32::MAX;
        let _ = device.state.set_render_state(7, 1);
    }
    device.loss = Some(loss);
    device.targets = Some(targets);
    device.registry_epoch += 1;
    Ok(normalized)
}

pub fn d3d9_register_resource(device: &mut D3d9Device, id: u32, resource: &D3d9Resource) -> Status {
    if !matches!((device.d3d9_gpu.as_ref(), resource.gpu.as_ref()),
        (Some(device_gpu), Some(resource_gpu)) if device_gpu.same_owner(resource_gpu))
    {
        return status(
            StatusCode::InvalidArgument,
            "resource belongs to a different device",
        );
    }
    let Some(pool) = resource.pool() else {
        return status(
            StatusCode::Unsupported,
            "resource metadata is not initialized",
        );
    };
    let Some(resource_type) = resource.resource_type() else {
        return status(
            StatusCode::Unsupported,
            "resource metadata is not initialized",
        );
    };
    if device
        .resources
        .insert(
            id,
            ResourceRegistration {
                resource_type,
                pool,
            },
        )
        .is_some()
    {
        return status(
            StatusCode::InvalidArgument,
            "resource id is already registered",
        );
    }
    device.locks.insert(id, resource.lock_handles());
    if let Some(entry) = gpu_entry_from_resource(resource) {
        device.registry.insert(id, entry);
        device.registry_epoch += 1;
    }
    status(StatusCode::Ok, "")
}

fn fresh_or_new(fresh: Option<Arc<AtomicBool>>) -> Arc<AtomicBool> {
    fresh.unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
}

fn gpu_entry_from_staged(
    resource: &D3d9Resource,
    staged: &StagedStorage,
) -> Option<GpuRegistryEntry> {
    if let Some(resource::ResourceState::Buffer(b)) = resource.state.as_ref() {
        return Some(buffer_entry(b));
    }
    let texture = staged.texture.as_ref()?;
    match resource.state.as_ref() {
        Some(resource::ResourceState::Texture(t)) => Some(GpuRegistryEntry::Texture {
            texture: texture.clone(),
            kind: t.kind,
            format: t.format,
            width: t.width,
            height: t.height,
            pool: t.pool,
            usage: t.usage,
            lod: resource.lod.clone(),
            fresh: fresh_or_new(resource.level_freshness(0, 0)),
        }),
        Some(resource::ResourceState::Surface(s)) => Some(GpuRegistryEntry::Surface {
            texture: texture.clone(),
            format: s.format,
            width: s.width,
            height: s.height,
            pool: s.pool,
            usage: s.usage,
            discard: s.discard,
            fresh: fresh_or_new(resource.level_freshness(0, 0)),
        }),
        _ => None,
    }
}

fn registry_texture(entry: &GpuRegistryEntry) -> Option<&wgpu::Texture> {
    match entry {
        GpuRegistryEntry::Texture { texture, .. } | GpuRegistryEntry::Surface { texture, .. } => {
            Some(texture)
        }
        GpuRegistryEntry::TextureAlias { gpu, .. } => gpu.as_ref(),
        GpuRegistryEntry::Buffer { .. } => None,
    }
}

fn buffer_entry(b: &resource::BufferState) -> GpuRegistryEntry {
    GpuRegistryEntry::Buffer {
        uploaded: Arc::clone(&b.uploaded),
        format: b.format,
    }
}

fn gpu_entry_from_resource(resource: &D3d9Resource) -> Option<GpuRegistryEntry> {
    if let Some(resource::ResourceState::Buffer(b)) = resource.state.as_ref() {
        return Some(buffer_entry(b));
    }
    if let Some((texture, kind, format, width, height, _)) = resource.texture_gpu() {
        let (pool, usage) = match resource.state.as_ref() {
            Some(resource::ResourceState::Texture(t)) => (t.pool, t.usage),
            _ => (0, 0),
        };
        return Some(GpuRegistryEntry::Texture {
            texture,
            kind,
            format,
            width,
            height,
            pool,
            usage,
            lod: resource.lod.clone(),
            fresh: fresh_or_new(resource.level_freshness(0, 0)),
        });
    }
    resource.surface_gpu().map(
        |(texture, format, width, height, _)| GpuRegistryEntry::Surface {
            texture,
            format,
            width,
            height,
            pool: resource.pool().unwrap_or(resource::D3DPOOL_DEFAULT),
            usage: match resource.state.as_ref() {
                Some(resource::ResourceState::Surface(s)) => s.usage,
                _ => 0,
            },
            discard: match resource.state.as_ref() {
                Some(resource::ResourceState::Surface(s)) => s.discard,
                _ => false,
            },
            fresh: fresh_or_new(resource.level_freshness(0, 0)),
        },
    )
}

pub fn d3d9_register_resource_view(
    device: &mut D3d9Device,
    id: u32,
    resource: &D3d9Resource,
    face: u32,
    level: u32,
    volume: bool,
) -> Status {
    let Some(resource::ResourceState::Texture(t)) = resource.state.as_ref() else {
        return status(StatusCode::InvalidArgument, "resource is not a texture");
    };
    if level >= t.levels {
        return status(
            StatusCode::InvalidArgument,
            "texture view level is out of range",
        );
    }
    if !volume && matches!(t.kind, resource::TextureKind::Cube) && face >= 6 {
        return status(StatusCode::InvalidArgument, "cube face is out of range");
    }
    let kind = t.kind;
    let format = t.format;
    let pool = t.pool;
    let usage = t.usage;
    let width = (t.width >> level).max(1);
    let height = (t.height >> level).max(1);
    let gpu = resource
        .texture_gpu()
        .map(|(texture, _, _, _, _, _)| texture);
    let lock = match resource.level_lock_handle(face, level) {
        Ok(lock) => lock,
        Err(error) => return bridge_error(error),
    };
    device.locks.insert(id, vec![lock]);
    device.registry.insert(
        id,
        GpuRegistryEntry::TextureAlias {
            gpu,
            parent_fresh: resource.freshness(),
            kind,
            format,
            width,
            height,
            face,
            level,
            volume,
            pool,
            usage,
            lod: resource.lod.clone(),
            fresh: fresh_or_new(resource.level_freshness(face, level)),
        },
    );
    device.registry_epoch += 1;
    status(StatusCode::Ok, "")
}

pub fn d3d9_register_implicit_targets(
    device: &mut D3d9Device,
    backbuffer: u32,
    depth: u32,
) -> Status {
    device.implicit_backbuffer_id = backbuffer;
    device.implicit_depthbuffer_id = depth;
    device.registry_epoch += 1;
    status(StatusCode::Ok, "")
}

pub fn d3d9_unregister_resource(device: &mut D3d9Device, id: u32) -> Status {
    device.resources.remove(&id);
    let released = device.registry.remove(&id);
    if let Some(texture) = released.as_ref().and_then(registry_texture) {
        device.draw.drop_texture(texture);
        if !device
            .registry
            .values()
            .any(|entry| registry_texture(entry) == Some(texture))
        {
            device
                .view_cache
                .borrow_mut()
                .retain(|key, _| &key.texture != texture);
        }
    }
    device.registry_epoch += 1;
    device.locks.remove(&id);
    device.draw_views = None;
    stats::record(|s| {
        if let Some(textures) = &device.draw_textures {
            s.texture_epoch_invalidations += 1;
            s.texture_invalidated_slots +=
                textures.bindings.iter().filter(|b| b.is_some()).count() as u64;
        }
    });
    device.draw_textures = None;
    if device.render_target_id == id {
        device.render_target_id = 0;
    }
    if device.depth_target_id == id {
        device.depth_target_id = 0;
    }
    status(StatusCode::Ok, "")
}

pub fn d3d9_registered_resource_ids(device: &D3d9Device) -> ValuesResult {
    if device.gpu.is_none() {
        return ValuesResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device is not initialized",
            ),
            values: Vec::new(),
        };
    }
    ValuesResult {
        status: status(StatusCode::Ok, ""),
        values: {
            let mut ids: Vec<u32> = device.resources.keys().copied().collect();
            ids.sort_unstable();
            ids
        },
    }
}

fn poll_device(gpu: &GpuContext) -> Result<(), Status> {
    gpu.device
        .poll(wgpu::PollType::Poll)
        .map_err(|error| status(StatusCode::Backend, format!("device poll failed: {error}")))?;
    Ok(())
}

fn reset_validation(
    device: &D3d9Device,
    request: &D3d9ResetRequest,
) -> Result<D3d9PresentParams, Status> {
    if !request.window_valid {
        return Err(status(
            StatusCode::InvalidArgument,
            "Reset requires a usable presentation window",
        ));
    }
    let present =
        validate_present_params(&request.present, &request.display_mode).map_err(bridge_error)?;
    if request.implicit_external_refs {
        return Err(status(
            StatusCode::InvalidArgument,
            "Reset requires outstanding implicit surface references to be released first",
        ));
    }
    if device.state.has_live_state_blocks() {
        return Err(status(
            StatusCode::InvalidArgument,
            "Reset requires every live state block to be released first",
        ));
    }
    if device
        .resources
        .values()
        .any(|registration| registration.pool == resource::D3DPOOL_DEFAULT)
    {
        return Err(status(
            StatusCode::InvalidArgument,
            "Reset requires every live D3DPOOL_DEFAULT resource to be released first",
        ));
    }
    Ok(present)
}

fn replacement_ready(device: &mut D3d9Device) -> Result<(), String> {
    if let Some((gpu, loss)) = device.replacement.as_ref() {
        if !loss.is_physical() && poll_device(gpu).is_ok() && !loss.is_physical() {
            return Ok(());
        }
    }
    device.replacement = None;
    if device.supplied_context {
        return Err("the supplied context owner must replace the device".to_string());
    }
    let gpu =
        pollster::block_on(GpuContext::new_headless_d3d9()).map_err(|error| error.to_string())?;
    if !gpu.device.features().contains(device.features)
        || !device.limits.check_limits(&gpu.device.limits())
    {
        return Err(
            "replacement adapter is not compatible with the original device capabilities"
                .to_string(),
        );
    }
    let loss = Arc::new(D3d9Loss::from_physical(gpu.physical_loss()));
    attach_loss(&gpu, &loss);
    poll_device(&gpu).map_err(|error| error.message)?;
    if loss.is_physical() {
        return Err("replacement device was lost during creation".to_string());
    }
    device.replacement = Some((Arc::new(gpu), loss));
    Ok(())
}

pub fn d3d9_reset(device: &mut D3d9Device, request: &D3d9ResetRequest) -> D3d9ResetResult {
    if device.gpu.is_none() {
        return reset_fail(
            request.present,
            status(
                StatusCode::InvalidArgument,
                "D3D9 device is not initialized",
            ),
        );
    }
    if device.restore.is_some() {
        return reset_fail(
            request.present,
            status(
                StatusCode::InvalidArgument,
                "a reset is already in progress",
            ),
        );
    }
    if device.loss.as_ref().is_some_and(|loss| loss.is_blocked()) {
        device.draw.discard_batch();
    } else if let Err(error) = flush_draws(device) {
        return reset_fail(request.present, bridge_error(error));
    }
    if let Ok(gpu) = bridge_gpu(device) {
        gpu.submit_pending();
    }
    if !device.loss.as_ref().is_some_and(|loss| loss.is_physical()) {
        if let Err(error) = poll_device(device.gpu.as_ref().expect("device is initialized")) {
            if !device.loss.as_ref().is_some_and(|loss| loss.is_physical()) {
                if let Some(loss) = device.loss.as_ref() {
                    loss.mark_reset_required();
                }
                return reset_fail(request.present, error);
            }
        }
    }
    let physical = device.loss.as_ref().is_some_and(|loss| loss.is_physical());
    let present = match reset_validation(device, request) {
        Ok(present) => present,
        Err(error) => {
            if let Some(loss) = device.loss.as_ref() {
                loss.mark_reset_required();
            }
            return reset_fail(request.present, error);
        }
    };
    let expected: BTreeMap<u32, u32> = device
        .resources
        .iter()
        .map(|(id, registration)| (*id, registration.pool))
        .collect();
    if physical {
        if let Err(message) = replacement_ready(device) {
            if let Some(loss) = device.loss.as_ref() {
                loss.mark_reset_required();
            }
            return reset_fail(
                present,
                status(
                    StatusCode::DeviceLost,
                    format!("device recovery unavailable: {message}"),
                ),
            );
        }
        let (gpu, loss) = device.replacement.take().expect("replacement is present");
        let targets = match create_targets(&gpu, &present) {
            Ok(targets) => targets,
            Err(error) => {
                device.replacement = Some((gpu, loss));
                return reset_fail(present, status(StatusCode::DeviceLost, error.to_string()));
            }
        };
        let mut d3d9_gpu = D3d9Gpu::from_context_with_loss(&gpu, loss.as_ref().clone());
        if let Some(previous) = device.d3d9_gpu.as_ref() {
            d3d9_gpu.shader_mode = previous.shader_mode.clone();
            d3d9_gpu.shader_mode_error = previous.shader_mode_error.clone();
            d3d9_gpu.tint = previous.tint;
        }
        if let Err(error) = draw::uber::prepare(&d3d9_gpu) {
            device.replacement = Some((gpu, loss));
            return reset_fail(present, bridge_error(error));
        }
        device.restore = Some(RestorePlan {
            gpu: Some(gpu),
            d3d9_gpu,
            loss,
            targets,
            present,
            display_mode: request.display_mode,
            expected,
            staged: BTreeMap::new(),
            registry: device.registry.clone(),
            committed: 0,
            ready: false,
        });
    } else {
        if let Some(loss) = device.loss.as_ref() {
            loss.mark_reset_required();
        }
        let gpu = device.gpu.as_ref().expect("device is initialized");
        let targets = match create_targets(gpu, &present) {
            Ok(targets) => targets,
            Err(error) => {
                let status = match device.loss.as_ref() {
                    Some(loss) if loss.is_physical() => {
                        status(StatusCode::DeviceLost, loss.physical_message())
                    }
                    _ => bridge_error(error),
                };
                return reset_fail(present, status);
            }
        };
        let d3d9_gpu = device
            .d3d9_gpu
            .clone()
            .expect("device GPU context is initialized");
        let loss = Arc::clone(device.loss.as_ref().expect("device loss is initialized"));
        device.restore = Some(RestorePlan {
            gpu: None,
            d3d9_gpu,
            loss,
            targets,
            present,
            display_mode: request.display_mode,
            expected,
            staged: BTreeMap::new(),
            registry: device.registry.clone(),
            committed: 0,
            ready: false,
        });
    }
    D3d9ResetResult {
        status: status(StatusCode::Ok, ""),
        present,
    }
}

pub fn d3d9_prepare_resource_reset(
    device: &mut D3d9Device,
    id: u32,
    resource: &D3d9Resource,
) -> Status {
    let Some(plan) = device.restore.as_mut() else {
        return status(StatusCode::InvalidArgument, "no reset is in progress");
    };
    let Some(expected_pool) = plan.expected.get(&id).copied() else {
        return status(
            StatusCode::InvalidArgument,
            "resource is not part of the reset set",
        );
    };
    if plan.staged.contains_key(&id) {
        return status(StatusCode::InvalidArgument, "resource is already staged");
    }
    if resource.pool() != Some(expected_pool) {
        return status(
            StatusCode::InvalidArgument,
            "resource pool does not match its registration",
        );
    }
    if plan.d3d9_gpu.is_physical() {
        return status(StatusCode::DeviceLost, "the replacement device is lost");
    }
    match resource::stage_restore(&plan.d3d9_gpu, resource) {
        Ok(staged) => {
            let parent_fresh = resource.freshness();
            match gpu_entry_from_staged(resource, &staged) {
                Some(entry) => {
                    plan.registry.insert(id, entry);
                }
                None => {
                    plan.registry.remove(&id);
                }
            }
            for entry in plan.registry.values_mut() {
                if let GpuRegistryEntry::TextureAlias {
                    gpu,
                    parent_fresh: alias_parent,
                    ..
                } = entry
                {
                    if Arc::ptr_eq(alias_parent, &parent_fresh) {
                        *gpu = staged.texture.clone();
                    }
                }
            }
            plan.staged.insert(id, staged);
            status(StatusCode::Ok, "")
        }
        Err(error) => {
            if plan.d3d9_gpu.is_physical() || plan.loss.is_physical() {
                return status(StatusCode::DeviceLost, plan.loss.physical_message());
            }
            bridge_error(error)
        }
    }
}

pub fn d3d9_reset_ready(device: &mut D3d9Device) -> Status {
    let required_features = device.features;
    let required_limits = device.limits.clone();
    let Some(flush_gpu) = device.restore.as_ref().map(|plan| plan.d3d9_gpu.clone()) else {
        return status(StatusCode::InvalidArgument, "no reset is in progress");
    };
    if flush_gpu.is_physical() {
        device.draw.discard_batch();
        return status(StatusCode::DeviceLost, flush_gpu.loss().physical_message());
    }
    if let Err(error) = flush_draws_with_gpu(device, &flush_gpu) {
        if flush_gpu.is_physical() {
            return status(StatusCode::DeviceLost, flush_gpu.loss().physical_message());
        }
        return bridge_error(error);
    }
    let Some(plan) = device.restore.as_mut() else {
        return status(StatusCode::InvalidArgument, "no reset is in progress");
    };
    if plan.staged.len() != plan.expected.len()
        || !plan.expected.keys().all(|id| plan.staged.contains_key(id))
    {
        return status(
            StatusCode::InvalidArgument,
            "reset staged set is incomplete",
        );
    }
    if plan.loss.is_physical() || plan.d3d9_gpu.is_physical() {
        return status(StatusCode::DeviceLost, plan.loss.physical_message());
    }
    if let Some(gpu) = plan.gpu.as_ref() {
        if !gpu.device.features().contains(required_features)
            || !required_limits.check_limits(&gpu.device.limits())
        {
            return status(
                StatusCode::DeviceLost,
                "replacement capabilities changed before commit",
            );
        }
    }
    if let Err(error) = plan.d3d9_gpu.flush() {
        if plan.loss.is_physical() || plan.d3d9_gpu.is_physical() {
            return status(StatusCode::DeviceLost, plan.loss.physical_message());
        }
        return bridge_error(error);
    }
    if plan.loss.is_physical() || plan.d3d9_gpu.is_physical() {
        return status(StatusCode::DeviceLost, plan.loss.physical_message());
    }
    plan.ready = true;
    status(StatusCode::Ok, "")
}

pub fn d3d9_commit_resource_reset(device: &mut D3d9Device, id: u32, resource: &mut D3d9Resource) {
    let plan = device
        .restore
        .as_mut()
        .expect("D3D9 reset commit invariant: no reset is in progress");
    assert!(
        plan.ready,
        "D3D9 reset commit invariant: reset is not ready"
    );
    let staged = plan
        .staged
        .remove(&id)
        .expect("D3D9 reset commit invariant: resource was not staged");
    resource::install_staged(resource, staged, &plan.d3d9_gpu);
    plan.committed += 1;
}

pub fn d3d9_finish_reset(device: &mut D3d9Device) -> D3d9ResetResult {
    let plan = device
        .restore
        .take()
        .expect("D3D9 reset finish invariant: no reset is in progress");
    assert!(
        plan.ready,
        "D3D9 reset finish invariant: reset is not ready"
    );
    assert_eq!(
        plan.committed,
        plan.expected.len(),
        "D3D9 reset finish invariant: reset is not fully committed"
    );
    device.targets = Some(plan.targets);
    device.registry = plan.registry;
    device.registry_epoch += 1;
    device.present = plan.present;
    device.display_mode = plan.display_mode;
    device
        .state
        .set_target_size(plan.present.width, plan.present.height);
    device.state.clear_pipeline();
    device.draw.reset();
    device.view_cache.borrow_mut().clear();
    device.render_target_id = 0;
    device.depth_target_id = 0;
    device.implicit_backbuffer_id = 0;
    device.implicit_depthbuffer_id = 0;
    device.in_scene = false;
    if let Some(gpu) = device.d3d9_gpu.clone() {
        device.queries.reset(&gpu);
    }
    if plan.present.enable_auto_depth_stencil != 0 {
        device.depth_target_id = u32::MAX;
        let _ = device.state.set_render_state(7, 1);
    }
    if let Some(gpu) = plan.gpu {
        device.features = gpu.device.features();
        device.limits = gpu.device.limits();
        device.gpu = Some(gpu);
        device.d3d9_gpu = Some(plan.d3d9_gpu);
        device.loss = Some(plan.loss);
        if let Some(loss) = device.loss.as_ref() {
            loss.clear_reset_required();
        }
    } else {
        plan.loss.clear_reset_required();
    }
    // After ready, commit is infallible; physical loss remains visible to later operations.
    D3d9ResetResult {
        status: status(StatusCode::Ok, ""),
        present: plan.present,
    }
}

pub fn d3d9_abort_reset(device: &mut D3d9Device) -> Status {
    if device.restore.as_ref().is_some_and(|plan| plan.ready) {
        return status(
            StatusCode::InvalidArgument,
            "Reset cannot be aborted after commit is ready",
        );
    }
    device.restore = None;
    device.replacement = None;
    status(StatusCode::Ok, "")
}

pub fn d3d9_test_cooperative_level(device: &mut D3d9Device) -> ValueResult {
    if device.gpu.is_none() {
        return ValueResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device is not initialized",
            ),
            value: 0,
        };
    }
    if let Ok(gpu) = bridge_gpu(device) {
        gpu.submit_pending();
    }
    if let Err(error) = poll_device(device.gpu.as_ref().expect("device is initialized")) {
        device.draw.discard_batch();
        return ValueResult {
            status: error,
            value: 0,
        };
    }
    if device.loss.as_ref().is_some_and(|loss| loss.is_blocked()) {
        device.draw.discard_batch();
    }
    let value = if device.loss.as_ref().is_some_and(|loss| loss.is_physical()) {
        if replacement_ready(device).is_ok() {
            2
        } else {
            1
        }
    } else if device
        .loss
        .as_ref()
        .is_some_and(|loss| loss.is_reset_required())
    {
        2
    } else {
        0
    };
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value,
    }
}

fn frame_fail(status: Status) -> D3d9FrameResult {
    D3d9FrameResult {
        status,
        width: 0,
        height: 0,
        bytes: Vec::new(),
    }
}

fn target_surface(target: &OffscreenTarget, format: u32) -> SurfaceView {
    SurfaceView {
        texture: target.texture.clone(),
        view: target.view.clone(),
        format,
        width: target.width,
        height: target.height,
        cube: false,
        volume: false,
        subresource: (0, 0, 0),
    }
}

fn present_image(
    device: &D3d9Device,
    gpu: &D3d9Gpu,
    src_rect: &[crate::bridge::D3d9Rect],
    dst_rect: &[crate::bridge::D3d9Rect],
    target: &OffscreenTarget,
) -> Result<(), Status> {
    let src = map_rects(src_rect)?.into_iter().next();
    let dst = map_rects(dst_rect)?.into_iter().next();
    let backbuffer = resolve_target_view(device, 0, 0, 0, false).map_err(bridge_error)?;
    let targets = device.targets.as_ref().expect("swap chain is initialized");
    let presented = target_surface(target, targets.format);
    let result = copy::stretch_rect(gpu, &backbuffer, src, &presented, dst, D3DTEXF_LINEAR);
    let result = bridge_gpu_ok(gpu, result);
    if result.code != StatusCode::Ok {
        return Err(result);
    }
    Ok(())
}

fn prepare_present(
    device: &mut D3d9Device,
    src_rect: &[crate::bridge::D3d9Rect],
    dst_rect: &[crate::bridge::D3d9Rect],
) -> Result<(), Status> {
    if device.gpu.is_none() {
        return Err(status(
            StatusCode::InvalidArgument,
            "D3D9 device is not initialized",
        ));
    }
    if let Some(loss) = device.loss.as_ref() {
        if loss.is_physical() {
            return Err(status(StatusCode::DeviceLost, loss.physical_message()));
        }
        if loss.is_reset_required() {
            return Err(status(
                StatusCode::DeviceLost,
                "the device needs a successful Reset before Present",
            ));
        }
    }
    let (pending, presented, format) = match device.targets.as_ref() {
        Some(targets) => (
            targets.pending.clone(),
            targets.presented.clone(),
            targets.format,
        ),
        None => {
            return Err(status(
                StatusCode::InvalidArgument,
                "D3D9 device has no implicit swap chain",
            ));
        }
    };
    let gpu = bridge_gpu(device).map_err(bridge_error)?;
    let result = flush_draws_with_gpu(device, &gpu);
    device.draw.end_frame(&gpu);
    let result = bridge_gpu_ok(&gpu, result);
    if result.code != StatusCode::Ok {
        return Err(result);
    }
    if !src_rect.is_empty() || !dst_rect.is_empty() {
        let from = target_surface(&presented, format);
        let to = target_surface(&pending, format);
        let result = copy::stretch_rect(&gpu, &from, None, &to, None, D3DTEXF_LINEAR);
        let result = bridge_gpu_ok(&gpu, result);
        if result.code != StatusCode::Ok {
            return Err(result);
        }
    }
    present_image(device, &gpu, src_rect, dst_rect, &pending)
}

fn commit_presented(device: &mut D3d9Device) {
    if let Some(targets) = device.targets.as_mut() {
        std::mem::swap(&mut targets.presented, &mut targets.pending);
    }
}

fn read_presented(
    device: &D3d9Device,
    target: &OffscreenTarget,
) -> Result<api::Image, api::ApiError> {
    let gpu = device.gpu.as_ref().ok_or_else(|| api::ApiError {
        kind: api::ApiErrorKind::InvalidArgument,
        message: "D3D9 device is not initialized".into(),
    })?;
    let width = target.width;
    let height = target.height;
    let mut bytes = Vec::new();
    let len = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    bytes.try_reserve_exact(len).map_err(|_| api::ApiError {
        kind: api::ApiErrorKind::OutOfMemory,
        message: "present readback allocation failed".into(),
    })?;
    bytes.resize(len, 0);
    gpu.synchronize_d3d9();
    if let Some(loss) = device.loss.as_ref().filter(|loss| loss.is_physical()) {
        return Err(api::ApiError {
            kind: api::ApiErrorKind::DeviceLost,
            message: loss.physical_message(),
        });
    }
    if let Err(error) = gpu.read_pixels_into(target, &mut bytes) {
        if let Some(loss) = device.loss.as_ref().filter(|loss| loss.is_physical()) {
            return Err(api::ApiError {
                kind: api::ApiErrorKind::DeviceLost,
                message: loss.physical_message(),
            });
        }
        return Err(error.into());
    }
    Ok(api::Image {
        width,
        height,
        bytes,
        format: api::ImageFormat::Rgba8Unorm,
        alpha: api::AlphaInterpretation::Unspecified,
        color: api::ColorInterpretation::UnormNoTransferConversion,
    })
}

fn bridge_frame(result: Result<api::Image, api::ApiError>) -> D3d9FrameResult {
    match result {
        Ok(image) => D3d9FrameResult {
            status: status(StatusCode::Ok, ""),
            width: image.width,
            height: image.height,
            bytes: image.bytes,
        },
        Err(error) => frame_fail(status(error.kind.status_code(), error.message)),
    }
}

pub fn d3d9_present_handoff(
    device: &mut D3d9Device,
    src_rect: &[crate::bridge::D3d9Rect],
    dst_rect: &[crate::bridge::D3d9Rect],
) -> crate::bridge::FrameHandoffResult {
    let result = prepare_present(device, src_rect, dst_rect).and_then(|()| {
        let gpu = bridge_gpu(device).map_err(bridge_error)?;
        let target = device
            .targets
            .as_ref()
            .expect("swap chain is initialized")
            .pending
            .clone();
        let context = device.gpu.as_ref().expect("device is initialized");
        present::handoff(&mut device.present_ring, &gpu, &target, context).map_err(|error| {
            if gpu.is_physical() {
                status(StatusCode::DeviceLost, gpu.loss().physical_message())
            } else {
                bridge_error(error)
            }
        })
    });
    match result {
        Ok(handoff) => crate::bridge::FrameHandoffResult {
            status: status(StatusCode::Ok, ""),
            handoff,
        },
        Err(status) => crate::bridge::FrameHandoffResult {
            status,
            handoff: crate::bridge::FrameHandoff {
                ready_event: std::ptr::null_mut(),
                ready_value: 0,
                texture: std::ptr::null_mut(),
                lease: 0,
                width: 0,
                height: 0,
            },
        },
    }
}

pub fn d3d9_commit_present(device: &mut D3d9Device) {
    commit_presented(device);
}

pub fn d3d9_present_readback(device: &D3d9Device) -> D3d9FrameResult {
    let result = match device.targets.as_ref() {
        Some(targets) => read_presented(device, &targets.presented),
        None => Err(api::ApiError {
            kind: api::ApiErrorKind::InvalidArgument,
            message: "D3D9 device has no implicit swap chain".into(),
        }),
    };
    bridge_frame(result)
}

fn present_gpu_snapshot(
    device: &mut D3d9Device,
) -> Result<(Arc<GpuContext>, wgpu::Texture, u32, u32), api::ApiError> {
    prepare_present(device, &[], &[]).map_err(api::ApiError::from)?;
    let context = Arc::clone(device.gpu.as_ref().expect("device is initialized"));
    let targets = device.targets.as_ref().expect("swap chain is initialized");
    let pending = &targets.pending;
    let gpu = device.resource_gpu()?.clone();
    let lost_or_error = |error: RenderError| {
        if context.physical_loss().is_lost() {
            api::ApiError {
                kind: api::ApiErrorKind::DeviceLost,
                message: "image GPU context is lost".into(),
            }
        } else {
            error.into()
        }
    };
    let texture = gpu
        .create_texture(
            pending.width,
            pending.height,
            1,
            1,
            pending.format,
            wgpu::TextureDimension::D2,
            wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
        )
        .map_err(&lost_or_error)?;
    gpu.validate("PresentGpuImage", || {
        gpu.record(|encoder, _| {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &pending.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: pending.width,
                    height: pending.height,
                    depth_or_array_layers: 1,
                },
            );
        });
        Ok(())
    })
    .map_err(&lost_or_error)?;
    context.synchronize_d3d9();
    if context.physical_loss().is_lost() {
        return Err(api::ApiError {
            kind: api::ApiErrorKind::DeviceLost,
            message: "image GPU context is lost".into(),
        });
    }
    let (width, height) = (pending.width, pending.height);
    commit_presented(device);
    Ok((context, texture, width, height))
}

fn present_cpu_image(
    device: &mut D3d9Device,
    src_rect: &[crate::bridge::D3d9Rect],
    dst_rect: &[crate::bridge::D3d9Rect],
) -> Result<api::Image, api::ApiError> {
    prepare_present(device, src_rect, dst_rect).map_err(api::ApiError::from)?;
    let target = device
        .targets
        .as_ref()
        .expect("swap chain is initialized")
        .pending
        .clone();
    let image = read_presented(device, &target)?;
    commit_presented(device);
    Ok(image)
}

pub fn d3d9_present(
    device: &mut D3d9Device,
    src_rect: &[crate::bridge::D3d9Rect],
    dst_rect: &[crate::bridge::D3d9Rect],
) -> D3d9FrameResult {
    bridge_frame(present_cpu_image(device, src_rect, dst_rect))
}

fn empty_surface_descriptor() -> D3d9SurfaceDescriptor {
    D3d9SurfaceDescriptor {
        format: 0,
        resource_type: 0,
        usage: 0,
        pool: 0,
        multisample_type: 0,
        multisample_quality: 0,
        width: 0,
        height: 0,
    }
}

fn implicit_surface_descriptor(device: &D3d9Device, depth: bool) -> D3d9SurfaceDescriptorResult {
    let Some(targets) = device.targets.as_ref() else {
        return D3d9SurfaceDescriptorResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device has no implicit swap chain",
            ),
            descriptor: empty_surface_descriptor(),
        };
    };
    if depth && targets.target.depth.is_none() {
        return D3d9SurfaceDescriptorResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device has no depth buffer",
            ),
            descriptor: empty_surface_descriptor(),
        };
    }
    D3d9SurfaceDescriptorResult {
        status: status(StatusCode::Ok, ""),
        descriptor: D3d9SurfaceDescriptor {
            format: if depth {
                targets.depth_format
            } else {
                targets.format
            },
            resource_type: resource::D3DRTYPE_SURFACE,
            usage: if depth {
                resource::D3DUSAGE_DEPTHSTENCIL
            } else {
                resource::D3DUSAGE_RENDERTARGET
            },
            pool: resource::D3DPOOL_DEFAULT,
            multisample_type: 0,
            multisample_quality: 0,
            width: targets.target.width,
            height: targets.target.height,
        },
    }
}

pub fn d3d9_backbuffer_descriptor(device: &D3d9Device) -> D3d9SurfaceDescriptorResult {
    implicit_surface_descriptor(device, false)
}

pub fn d3d9_depthbuffer_descriptor(device: &D3d9Device) -> D3d9SurfaceDescriptorResult {
    implicit_surface_descriptor(device, true)
}

pub fn d3d9_available_texture_mem(device: &D3d9Device) -> ValueResult {
    const MB: u64 = 1024 * 1024;
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu,
        Err(error) => {
            return ValueResult {
                status: bridge_error(error),
                value: 0,
            };
        }
    };
    if gpu.is_physical() {
        return ValueResult {
            status: status(StatusCode::DeviceLost, gpu.loss().physical_message()),
            value: 0,
        };
    }
    match gpu.available_texture_mem_bytes() {
        Ok(bytes) => {
            let mb = bytes.saturating_add(MB / 2) / MB;
            let mb = mb.min(i32::MAX as u64 / MB);
            ValueResult {
                status: status(StatusCode::Ok, ""),
                value: (mb * MB) as u32,
            }
        }
        Err(error) => ValueResult {
            status: bridge_error(error),
            value: 0,
        },
    }
}

pub fn d3d9_number_of_swap_chains(device: &D3d9Device) -> ValueResult {
    if device.gpu.is_none() {
        return ValueResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device is not initialized",
            ),
            value: 0,
        };
    }
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: 1,
    }
}

pub fn d3d9_display_mode(
    device: &D3d9Device,
    request: &D3d9DisplayModeRequest,
) -> D3d9DisplayModeResult {
    let empty = D3d9DisplayModeResult {
        status: status(StatusCode::Ok, ""),
        width: 0,
        height: 0,
        refresh_rate: 0,
        format: 0,
    };
    if device.gpu.is_none() {
        return D3d9DisplayModeResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device is not initialized",
            ),
            ..empty
        };
    }
    if request.swap_chain != 0 {
        return D3d9DisplayModeResult {
            status: status(
                StatusCode::InvalidArgument,
                format!("swap chain {} does not exist", request.swap_chain),
            ),
            ..empty
        };
    }
    let mode = if device.present.windowed != 0 {
        D3d9DisplayMode {
            width: request.width,
            height: request.height,
            refresh_rate: request.refresh_rate,
            format: request.format,
        }
    } else {
        device.display_mode
    };
    if mode.format == 0 {
        return D3d9DisplayModeResult {
            status: status(
                StatusCode::NotAvailable,
                "the host desktop bit depth has no D3DFORMAT",
            ),
            ..empty
        };
    }
    D3d9DisplayModeResult {
        status: status(StatusCode::Ok, ""),
        width: mode.width,
        height: mode.height,
        refresh_rate: mode.refresh_rate,
        format: mode.format,
    }
}

pub fn d3d9_effective_present(device: &D3d9Device) -> D3d9PresentParams {
    device.present
}

fn adapter_probe() -> Result<&'static D3d9Probe, RenderError> {
    static PROBE: std::sync::OnceLock<Result<D3d9Probe, String>> = std::sync::OnceLock::new();
    let entry = PROBE.get_or_init(|| {
        let gpu = pollster::block_on(GpuContext::new_headless_d3d9())
            .map_err(|error| error.to_string())?;
        let limits = gpu.device.limits();
        Ok(D3d9Probe {
            description: gpu.adapter_info.name.clone(),
            vendor_id: gpu.adapter_info.vendor,
            device_id: gpu.adapter_info.device,
            max_texture_dimension_2d: limits.max_texture_dimension_2d,
            max_texture_dimension_3d: limits.max_texture_dimension_3d,
            features: gpu.device.features(),
        })
    });
    match entry {
        Ok(probe) => Ok(probe),
        Err(message) => Err(RenderError::new("D3D9AdapterProbe", message.clone())),
    }
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn caps_image(probe: &D3d9Probe) -> Vec<u8> {
    let mut caps = vec![0u8; 304];
    put_u32(&mut caps, 0, D3DDEVTYPE_HAL);
    put_u32(&mut caps, 12, D3DCAPS2_CANAUTOGENMIPMAP);
    put_u32(
        &mut caps,
        20,
        D3DPRESENT_INTERVAL_IMMEDIATE
            | D3DPRESENT_INTERVAL_ONE
            | D3DPRESENT_INTERVAL_TWO
            | D3DPRESENT_INTERVAL_THREE
            | D3DPRESENT_INTERVAL_FOUR,
    );
    put_u32(&mut caps, 28, 0x0001_0000 | 0x0008_0000);
    put_u32(
        &mut caps,
        32,
        D3DPMISCCAPS_MASKZ
            | D3DPMISCCAPS_CULLNONE
            | D3DPMISCCAPS_CULLCW
            | D3DPMISCCAPS_CULLCCW
            | D3DPMISCCAPS_COLORWRITEENABLE
            | D3DPMISCCAPS_TSSARGTEMP
            | D3DPMISCCAPS_BLENDOP
            | D3DPMISCCAPS_SEPARATEALPHABLEND,
    );
    put_u32(
        &mut caps,
        36,
        D3DPRASTERCAPS_ZTEST
            | D3DPRASTERCAPS_FOGVERTEX
            | D3DPRASTERCAPS_FOGTABLE
            | D3DPRASTERCAPS_ANISOTROPY
            | D3DPRASTERCAPS_SLOPESCALEDEPTHBIAS,
    );
    put_u32(&mut caps, 40, D3DPCMPCAPS_ALL);
    put_u32(&mut caps, 44, D3DPBLENDCAPS_SRC);
    put_u32(&mut caps, 48, D3DPBLENDCAPS_DEST);
    put_u32(&mut caps, 52, D3DPCMPCAPS_ALL);
    put_u32(
        &mut caps,
        56,
        D3DPSHADECAPS_COLORGOURAUDRGB | D3DPSHADECAPS_SPECULARGOURAUDRGB,
    );
    put_u32(
        &mut caps,
        60,
        D3DPTEXTURECAPS_ALPHA
            | D3DPTEXTURECAPS_PERSPECTIVE
            | D3DPTEXTURECAPS_PROJECTED
            | D3DPTEXTURECAPS_CUBEMAP
            | D3DPTEXTURECAPS_VOLUMEMAP
            | D3DPTEXTURECAPS_MIPMAP
            | D3DPTEXTURECAPS_MIPVOLUMEMAP
            | D3DPTEXTURECAPS_MIPCUBEMAP,
    );
    put_u32(&mut caps, 64, D3DPTFILTERCAPS_SAMPLING);
    put_u32(&mut caps, 68, D3DPTFILTERCAPS_SAMPLING);
    put_u32(&mut caps, 72, D3DPTFILTERCAPS_SAMPLING);
    put_u32(&mut caps, 76, D3DPTADDRESSCAPS_ALL);
    put_u32(&mut caps, 80, D3DPTADDRESSCAPS_ALL);
    put_u32(&mut caps, 88, probe.max_texture_dimension_2d);
    put_u32(&mut caps, 92, probe.max_texture_dimension_2d);
    put_u32(&mut caps, 96, probe.max_texture_dimension_3d);
    put_u32(&mut caps, 100, probe.max_texture_dimension_2d);
    put_u32(&mut caps, 104, probe.max_texture_dimension_2d);
    put_u32(&mut caps, 108, 16);
    put_u32(&mut caps, 136, D3DSTENCILCAPS_ALL);
    put_u32(&mut caps, 140, 8);
    put_u32(&mut caps, 144, D3DTEXOPCAPS_IMPLEMENTED);
    put_u32(&mut caps, 148, 8);
    put_u32(&mut caps, 152, 8);
    put_u32(
        &mut caps,
        156,
        D3DVTXPCAPS_TEXGEN
            | D3DVTXPCAPS_MATERIALSOURCE7
            | D3DVTXPCAPS_DIRECTIONALLIGHTS
            | D3DVTXPCAPS_POSITIONALLIGHTS
            | D3DVTXPCAPS_LOCALVIEWER,
    );
    put_u32(&mut caps, 160, 8);
    put_u32(&mut caps, 180, D3D_MAX_PRIMITIVE_COUNT);
    put_u32(&mut caps, 184, D3D_MAX_VERTEX_INDEX);
    put_u32(&mut caps, 188, 16);
    put_u32(&mut caps, 192, state::MAX_STREAM_STRIDE);
    put_u32(&mut caps, 196, D3DVS_VERSION_2_0);
    put_u32(&mut caps, 200, 256);
    put_u32(&mut caps, 204, D3DPS_VERSION_2_0);
    put_u32(&mut caps, 212, D3DDEVCAPS2_STREAMOFFSET);
    put_u32(&mut caps, 232, 1);
    put_u32(&mut caps, 236, 0x1ffff);
    put_u32(&mut caps, 240, 1);
    put_u32(
        &mut caps,
        244,
        D3DPTFILTERCAPS_MINFPOINT | D3DPTFILTERCAPS_MINFLINEAR,
    );
    put_u32(&mut caps, 256, 32);
    put_u32(
        &mut caps,
        264,
        D3DPS20CAPS_ARBITRARYSWIZZLE
            | D3DPS20CAPS_NODEPENDENTREADLIMIT
            | D3DPS20CAPS_NOTEXINSTRUCTIONLIMIT,
    );
    put_u32(&mut caps, 272, 32);
    put_u32(&mut caps, 280, 512);
    caps
}

pub fn d3d9_adapter_caps(adapter: u32) -> D3d9CapsResult {
    if adapter != 0 {
        return D3d9CapsResult {
            status: status(
                StatusCode::InvalidArgument,
                format!("adapter {adapter} does not exist"),
            ),
            caps: Vec::new(),
        };
    }
    match adapter_probe() {
        Ok(probe) => D3d9CapsResult {
            status: status(StatusCode::Ok, ""),
            caps: caps_image(probe),
        },
        Err(error) => D3d9CapsResult {
            status: bridge_error(error),
            caps: Vec::new(),
        },
    }
}

pub fn d3d9_device_caps(device: &D3d9Device) -> D3d9CapsResult {
    if device.gpu.is_none() {
        return D3d9CapsResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device is not initialized",
            ),
            caps: Vec::new(),
        };
    }
    d3d9_adapter_caps(device.adapter)
}

pub fn d3d9_adapter_identifier(adapter: u32) -> D3d9AdapterIdentifierResult {
    if adapter != 0 {
        return D3d9AdapterIdentifierResult {
            status: status(
                StatusCode::InvalidArgument,
                format!("adapter {adapter} does not exist"),
            ),
            description: String::new(),
            vendor_id: 0,
            device_id: 0,
            sub_sys_id: 0,
            revision: 0,
            whql_level: 0,
            driver_version_low: 0,
            driver_version_high: 0,
        };
    }
    match adapter_probe() {
        Ok(probe) => D3d9AdapterIdentifierResult {
            status: status(StatusCode::Ok, ""),
            description: probe.description.clone(),
            vendor_id: probe.vendor_id,
            device_id: probe.device_id,
            sub_sys_id: 0,
            revision: 0,
            whql_level: 0,
            driver_version_low: 0,
            driver_version_high: 0,
        },
        Err(error) => D3d9AdapterIdentifierResult {
            status: bridge_error(error),
            description: String::new(),
            vendor_id: 0,
            device_id: 0,
            sub_sys_id: 0,
            revision: 0,
            whql_level: 0,
            driver_version_low: 0,
            driver_version_high: 0,
        },
    }
}

fn check_buffer_format(request: &D3d9CheckDeviceFormatRequest) -> Status {
    let unknown = request.usage & !resource::BUFFER_USAGE_MASK;
    if unknown != 0 {
        return status(
            StatusCode::NotAvailable,
            format!("unknown D3DUSAGE bits {unknown:#x}"),
        );
    }
    let ok = if request.resource_type == resource::D3DRTYPE_VERTEXBUFFER {
        request.check_format == format::D3DFMT_VERTEXDATA
    } else {
        request.check_format == format::D3DFMT_INDEX16
            || request.check_format == format::D3DFMT_INDEX32
    };
    if !ok {
        return status(
            StatusCode::NotAvailable,
            format!("buffer D3DFORMAT {} is not available", request.check_format),
        );
    }
    status(StatusCode::Ok, "")
}

fn check_adapter(adapter: u32, device_type: u32) -> Result<(), Status> {
    if adapter != 0 {
        return Err(status(
            StatusCode::InvalidArgument,
            format!("adapter {adapter} does not exist"),
        ));
    }
    match device_type {
        D3DDEVTYPE_HAL => Ok(()),
        D3DDEVTYPE_REF | D3DDEVTYPE_SW | D3DDEVTYPE_NULLREF => Err(status(
            StatusCode::NotAvailable,
            format!("D3DDEVTYPE {device_type} is not available"),
        )),
        _ => Err(status(
            StatusCode::InvalidArgument,
            format!("D3DDEVTYPE {device_type} is invalid"),
        )),
    }
}

fn adapter_format_supported(adapter_format: u32) -> bool {
    adapter_format == D3DFMT_A8R8G8B8 || adapter_format == D3DFMT_X8R8G8B8
}

fn format_allows_render_target(probe: &D3d9Probe, fmt: u32) -> bool {
    if format::is_depth(fmt) || format::is_block(fmt) {
        return false;
    }
    let Some(wgpu_format) = format::d3d_to_wgpu(fmt) else {
        return false;
    };
    wgpu_format
        .guaranteed_format_features(probe.features)
        .allowed_usages
        .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
}

fn query_usage_supported(probe: &D3d9Probe, fmt: u32, resource_type: u32, query: u32) -> bool {
    let Some(wgpu_format) = format::d3d_to_wgpu(fmt) else {
        return false;
    };
    let flags = wgpu_format.guaranteed_format_features(probe.features).flags;
    let render_target = format_allows_render_target(probe, fmt);
    let answers = [
        (
            D3DUSAGE_QUERY_FILTER,
            flags.contains(wgpu::TextureFormatFeatureFlags::FILTERABLE),
        ),
        (D3DUSAGE_QUERY_SRGBWRITE, render_target),
        (
            D3DUSAGE_QUERY_POSTPIXELSHADER_BLENDING,
            render_target && flags.contains(wgpu::TextureFormatFeatureFlags::BLENDABLE),
        ),
        (
            D3DUSAGE_QUERY_WRAPANDMIP,
            !format::is_depth(fmt)
                && matches!(
                    resource_type,
                    resource::D3DRTYPE_TEXTURE
                        | resource::D3DRTYPE_CUBETEXTURE
                        | resource::D3DRTYPE_VOLUMETEXTURE
                ),
        ),
    ];
    let answered = answers.iter().fold(0, |mask, &(bit, _)| mask | bit);
    (query & !answered) == 0 && answers.iter().all(|&(bit, ok)| query & bit == 0 || ok)
}

pub fn d3d9_check_device_type(request: &D3d9CheckDeviceTypeRequest) -> Status {
    if let Err(status) = check_adapter(request.adapter, request.device_type) {
        return status;
    }
    if !adapter_format_supported(request.adapter_format) {
        return status(
            StatusCode::NotAvailable,
            format!("adapter format {} is not available", request.adapter_format),
        );
    }
    if request.windowed == 0 {
        let format_ok = request.adapter_format == D3DFMT_X8R8G8B8
            && (request.backbuffer_format == D3DFMT_X8R8G8B8
                || request.backbuffer_format == D3DFMT_A8R8G8B8);
        if !format_ok {
            return status(
                StatusCode::NotAvailable,
                format!(
                    "fullscreen back buffer D3DFORMAT {} is not compatible with display D3DFORMAT {}",
                    request.backbuffer_format, request.adapter_format
                ),
            );
        }
        return status(StatusCode::Ok, "");
    }
    if request.backbuffer_format == 0 {
        return status(StatusCode::Ok, "");
    }
    if !adapter_format_supported(request.backbuffer_format) {
        return status(
            StatusCode::NotAvailable,
            format!(
                "back buffer D3DFORMAT {} is not available",
                request.backbuffer_format
            ),
        );
    }
    status(StatusCode::Ok, "")
}

pub fn d3d9_check_device_format(request: &D3d9CheckDeviceFormatRequest) -> Status {
    if let Err(status) = check_adapter(request.adapter, request.device_type) {
        return status;
    }
    if !adapter_format_supported(request.adapter_format) {
        return status(
            StatusCode::NotAvailable,
            format!("adapter format {} is not available", request.adapter_format),
        );
    }
    let probe = match adapter_probe() {
        Ok(probe) => probe,
        Err(error) => return bridge_error(error),
    };
    let fmt = request.check_format;
    if matches!(
        request.resource_type,
        resource::D3DRTYPE_VERTEXBUFFER | resource::D3DRTYPE_INDEXBUFFER
    ) {
        return check_buffer_format(request);
    }
    if !format::is_supported(fmt) {
        return status(
            StatusCode::NotAvailable,
            format!("D3DFORMAT {fmt} is not available"),
        );
    }
    let usage_mask = match request.resource_type {
        resource::D3DRTYPE_VERTEXBUFFER | resource::D3DRTYPE_INDEXBUFFER => {
            resource::BUFFER_USAGE_MASK
        }
        resource::D3DRTYPE_TEXTURE
        | resource::D3DRTYPE_CUBETEXTURE
        | resource::D3DRTYPE_VOLUMETEXTURE
        | resource::D3DRTYPE_VOLUME => resource::TEXTURE_USAGE_MASK,
        resource::D3DRTYPE_SURFACE => resource::SURFACE_USAGE_MASK,
        _ => {
            return status(
                StatusCode::NotAvailable,
                format!("unknown resource type {}", request.resource_type),
            );
        }
    };
    let unknown = request.usage & !(usage_mask | resource::D3DUSAGE_QUERY_MASK);
    if unknown != 0 {
        return status(
            StatusCode::NotAvailable,
            format!("unknown D3DUSAGE bits {unknown:#x}"),
        );
    }
    let query = request.usage & resource::D3DUSAGE_QUERY_MASK;
    if !query_usage_supported(probe, fmt, request.resource_type, query) {
        return status(
            StatusCode::NotAvailable,
            format!("D3DUSAGE query bits {query:#x} are not available for D3DFORMAT {fmt}"),
        );
    }
    let depth = format::is_depth(fmt);
    let render_target = request.usage & resource::D3DUSAGE_RENDERTARGET != 0;
    let depth_stencil = request.usage & resource::D3DUSAGE_DEPTHSTENCIL != 0;
    let autogen = request.usage & resource::D3DUSAGE_AUTOGENMIPMAP != 0;
    if let Some(feature) = format::required_feature(fmt) {
        if !probe.features.contains(feature) {
            return status(
                StatusCode::NotAvailable,
                format!("D3DFORMAT {fmt} needs wgpu feature {feature:?}"),
            );
        }
    }
    match request.resource_type {
        resource::D3DRTYPE_TEXTURE | resource::D3DRTYPE_CUBETEXTURE => {
            if request.resource_type == resource::D3DRTYPE_CUBETEXTURE && depth {
                return status(
                    StatusCode::NotAvailable,
                    "cube textures cannot be depth stencils",
                );
            }
            if depth {
                if render_target {
                    return status(
                        StatusCode::NotAvailable,
                        "depth formats are not render targets",
                    );
                }
                if !depth_stencil {
                    return status(
                        StatusCode::NotAvailable,
                        "depth formats need D3DUSAGE_DEPTHSTENCIL",
                    );
                }
            } else if depth_stencil {
                return status(
                    StatusCode::NotAvailable,
                    "D3DUSAGE_DEPTHSTENCIL needs a depth format",
                );
            }
            if render_target && !format_allows_render_target(probe, fmt) {
                return status(
                    StatusCode::NotAvailable,
                    format!("D3DFORMAT {fmt} is not a render target"),
                );
            }
            if autogen
                && (!format::autogen_capable(fmt) || !format_allows_render_target(probe, fmt))
            {
                return status(StatusCode::NoAutogen, "");
            }
        }
        resource::D3DRTYPE_VOLUMETEXTURE | resource::D3DRTYPE_VOLUME => {
            if depth || depth_stencil {
                return status(StatusCode::NotAvailable, "volumes cannot be depth stencils");
            }
            if render_target {
                return status(
                    StatusCode::NotAvailable,
                    "volume textures cannot be render targets",
                );
            }
            if autogen {
                return status(
                    StatusCode::NotAvailable,
                    "volume textures cannot use autogen mipmaps",
                );
            }
            if format::is_block(fmt)
                && !probe
                    .features
                    .contains(wgpu::Features::TEXTURE_COMPRESSION_BC_SLICED_3D)
            {
                return status(
                    StatusCode::NotAvailable,
                    "compressed volumes need TEXTURE_COMPRESSION_BC_SLICED_3D",
                );
            }
        }
        resource::D3DRTYPE_SURFACE => {
            if autogen {
                return status(
                    StatusCode::NotAvailable,
                    "surfaces cannot use autogen mipmaps",
                );
            }
            if depth {
                if render_target {
                    return status(
                        StatusCode::NotAvailable,
                        "depth formats are not render targets",
                    );
                }
                if !depth_stencil {
                    return status(
                        StatusCode::NotAvailable,
                        "depth formats need D3DUSAGE_DEPTHSTENCIL",
                    );
                }
            } else if depth_stencil {
                return status(
                    StatusCode::NotAvailable,
                    "D3DUSAGE_DEPTHSTENCIL needs a depth format",
                );
            }
            if render_target && !format_allows_render_target(probe, fmt) {
                return status(
                    StatusCode::NotAvailable,
                    format!("D3DFORMAT {fmt} is not a render target"),
                );
            }
        }
        _ => unreachable!(),
    }
    status(StatusCode::Ok, "")
}

pub fn d3d9_check_device_multisample_type(request: &D3d9CheckMultiSampleRequest) -> ValueResult {
    if let Err(status) = check_adapter(request.adapter, request.device_type) {
        return ValueResult { status, value: 0 };
    }
    if !format::is_supported(request.surface_format) {
        return ValueResult {
            status: status(
                StatusCode::NotAvailable,
                format!("D3DFORMAT {} is not available", request.surface_format),
            ),
            value: 0,
        };
    }
    let probe = match adapter_probe() {
        Ok(probe) => probe,
        Err(error) => {
            return ValueResult {
                status: bridge_error(error),
                value: 0,
            };
        }
    };
    if let Some(feature) = format::required_feature(request.surface_format) {
        if !probe.features.contains(feature) {
            return ValueResult {
                status: status(
                    StatusCode::NotAvailable,
                    format!(
                        "D3DFORMAT {} needs wgpu feature {feature:?}",
                        request.surface_format
                    ),
                ),
                value: 0,
            };
        }
    }
    if request.multisample_type == 0 {
        if !format::is_depth(request.surface_format)
            && !format_allows_render_target(probe, request.surface_format)
        {
            return ValueResult {
                status: status(
                    StatusCode::NotAvailable,
                    format!(
                        "D3DFORMAT {} is not a render target",
                        request.surface_format
                    ),
                ),
                value: 0,
            };
        }
        return ValueResult {
            status: status(StatusCode::Ok, ""),
            value: 1,
        };
    }
    ValueResult {
        status: status(
            StatusCode::NotAvailable,
            format!(
                "D3DMULTISAMPLE_TYPE {} is not available",
                request.multisample_type
            ),
        ),
        value: 0,
    }
}

pub fn d3d9_check_depth_stencil_match(request: &D3d9CheckDepthStencilRequest) -> Status {
    if let Err(status) = check_adapter(request.adapter, request.device_type) {
        return status;
    }
    let probe = match adapter_probe() {
        Ok(probe) => probe,
        Err(error) => return bridge_error(error),
    };
    if !format::is_supported(request.render_target_format)
        || format::is_depth(request.render_target_format)
        || !format_allows_render_target(probe, request.render_target_format)
    {
        return status(
            StatusCode::NotAvailable,
            format!(
                "render target D3DFORMAT {} is not available",
                request.render_target_format
            ),
        );
    }
    if !format::is_supported(request.depth_stencil_format)
        || !format::is_depth(request.depth_stencil_format)
    {
        return status(
            StatusCode::NotAvailable,
            format!(
                "depth stencil D3DFORMAT {} is not available",
                request.depth_stencil_format
            ),
        );
    }
    if request.adapter_format != D3DFMT_A8R8G8B8 && request.adapter_format != D3DFMT_X8R8G8B8 {
        return status(
            StatusCode::NotAvailable,
            format!("adapter format {} is not available", request.adapter_format),
        );
    }
    status(StatusCode::Ok, "")
}

pub fn d3d9_check_device_format_conversion(request: &D3d9CheckFormatConversionRequest) -> Status {
    if let Err(status) = check_adapter(request.adapter, request.device_type) {
        return status;
    }
    if copy::supports_format_conversion(request.source_format, request.destination_format)
        || (request.source_format == request.destination_format
            && format::is_supported(request.source_format)
            && !format::is_block(request.source_format))
    {
        return status(StatusCode::Ok, "");
    }
    status(
        StatusCode::NotAvailable,
        "format conversion is not available",
    )
}

fn reset_output_present(mut present: D3d9PresentParams) -> D3d9PresentParams {
    present.width = 0;
    present.height = 0;
    if present.windowed != 0 {
        present.format = 0;
    }
    present.count = 0;
    present
}

fn reset_fail(present: D3d9PresentParams, status: Status) -> D3d9ResetResult {
    D3d9ResetResult {
        status,
        present: reset_output_present(present),
    }
}

pub fn d3d9_creation_parameters(device: &D3d9Device) -> D3d9CreationParametersResult {
    if device.gpu.is_none() {
        return D3d9CreationParametersResult {
            status: status(
                StatusCode::InvalidArgument,
                "D3D9 device is not initialized",
            ),
            parameters: D3d9CreationParameters {
                adapter: 0,
                device_type: 0,
                behavior_flags: 0,
            },
        };
    }
    D3d9CreationParametersResult {
        status: status(StatusCode::Ok, ""),
        parameters: D3d9CreationParameters {
            adapter: device.adapter,
            device_type: device.device_type,
            behavior_flags: device.behavior_flags,
        },
    }
}

fn ok_value(value: u32) -> ValueResult {
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value,
    }
}

fn err_value(error: RenderError) -> ValueResult {
    ValueResult {
        status: bridge_error(error),
        value: 0,
    }
}

fn ok_values(values: Vec<u32>) -> ValuesResult {
    ValuesResult {
        status: status(StatusCode::Ok, ""),
        values,
    }
}

fn err_values(error: RenderError) -> ValuesResult {
    ValuesResult {
        status: bridge_error(error),
        values: Vec::new(),
    }
}

fn ok_floats(values: Vec<f32>) -> FloatValuesResult {
    FloatValuesResult {
        status: status(StatusCode::Ok, ""),
        values,
    }
}

fn err_floats(error: RenderError) -> FloatValuesResult {
    FloatValuesResult {
        status: bridge_error(error),
        values: Vec::new(),
    }
}

fn validate_binding(device: &D3d9Device, id: u32, allowed: &[u32]) -> Result<(), RenderError> {
    if id == 0 {
        return Ok(());
    }
    let Some(registration) = device.resources.get(&id) else {
        return Err(RenderError::invalid(
            "D3D9",
            "resource id is not registered with this device",
        ));
    };
    if !allowed.contains(&registration.resource_type) {
        return Err(RenderError::invalid(
            "D3D9",
            "resource kind cannot be bound to this slot",
        ));
    }
    if registration.pool == resource::D3DPOOL_SYSTEMMEM
        || registration.pool == resource::D3DPOOL_SCRATCH
    {
        return Err(RenderError::invalid(
            "D3D9",
            "SYSTEMMEM and SCRATCH resources cannot be bound",
        ));
    }
    Ok(())
}

fn bridge_bindings(bindings: state::D3d9Bindings) -> BridgeBindings {
    BridgeBindings {
        textures: bindings.textures,
        streams: bindings.streams,
        indices: bindings.indices,
        vertex_shader: bindings.vertex_shader,
        pixel_shader: bindings.pixel_shader,
        declaration: bindings.declaration,
    }
}

fn empty_bridge_bindings() -> BridgeBindings {
    BridgeBindings {
        textures: [0; state::MAX_SAMPLERS],
        streams: [0; state::MAX_STREAMS],
        indices: 0,
        vertex_shader: 0,
        pixel_shader: 0,
        declaration: 0,
    }
}

pub fn d3d9_set_render_state(device: &mut D3d9Device, state: u32, value: u32) -> Status {
    bridge_ok(device.state.set_render_state(state, value))
}

pub fn d3d9_get_render_state(device: &D3d9Device, state: u32) -> ValueResult {
    match device.state.render_state(state) {
        Ok(value) => ok_value(value),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_set_texture_stage_state(
    device: &mut D3d9Device,
    stage: u32,
    state: u32,
    value: u32,
) -> Status {
    bridge_ok(device.state.set_texture_stage_state(stage, state, value))
}

pub fn d3d9_get_texture_stage_state(device: &D3d9Device, stage: u32, state: u32) -> ValueResult {
    match device.state.texture_stage_state(stage, state) {
        Ok(value) => ok_value(value),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_set_sampler_state(
    device: &mut D3d9Device,
    sampler: u32,
    state: u32,
    value: u32,
) -> Status {
    bridge_ok(device.state.set_sampler_state(sampler, state, value))
}

pub fn d3d9_get_sampler_state(device: &D3d9Device, sampler: u32, state: u32) -> ValueResult {
    match device.state.sampler_state(sampler, state) {
        Ok(value) => ok_value(value),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_set_transform(device: &mut D3d9Device, state: u32, values: &[f32]) -> Status {
    let Ok(matrix) = <[f32; 16]>::try_from(values) else {
        return bridge_error(RenderError::invalid(
            "SetTransform",
            "a transform requires exactly 16 floats",
        ));
    };
    bridge_ok(device.state.set_transform(state, matrix))
}

pub fn d3d9_get_transform(device: &D3d9Device, state: u32) -> FloatValuesResult {
    match device.state.transform(state) {
        Ok(matrix) => ok_floats(matrix.to_vec()),
        Err(error) => err_floats(error),
    }
}

pub fn d3d9_multiply_transform(device: &mut D3d9Device, state: u32, values: &[f32]) -> Status {
    let Ok(matrix) = <[f32; 16]>::try_from(values) else {
        return bridge_error(RenderError::invalid(
            "MultiplyTransform",
            "a transform requires exactly 16 floats",
        ));
    };
    bridge_ok(device.state.multiply_transform(state, matrix))
}

pub fn d3d9_set_viewport(device: &mut D3d9Device, viewport: &Viewport) -> Status {
    bridge_ok(device.state.set_viewport(state::Viewport {
        x: viewport.x,
        y: viewport.y,
        width: viewport.width,
        height: viewport.height,
        min_z: viewport.min_z,
        max_z: viewport.max_z,
    }))
}

pub fn d3d9_get_viewport(device: &D3d9Device) -> ViewportResult {
    let viewport = device.state.viewport();
    ViewportResult {
        status: status(StatusCode::Ok, ""),
        viewport: Viewport {
            x: viewport.x,
            y: viewport.y,
            width: viewport.width,
            height: viewport.height,
            min_z: viewport.min_z,
            max_z: viewport.max_z,
        },
    }
}

pub fn d3d9_set_scissor(
    device: &mut D3d9Device,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
) -> Status {
    bridge_ok(device.state.set_scissor(state::Scissor {
        left,
        top,
        right,
        bottom,
    }))
}

pub fn d3d9_get_scissor(device: &D3d9Device) -> D3d9ScissorResult {
    let scissor = device.state.scissor();
    D3d9ScissorResult {
        status: status(StatusCode::Ok, ""),
        left: scissor.left,
        top: scissor.top,
        right: scissor.right,
        bottom: scissor.bottom,
    }
}

pub fn d3d9_set_material(device: &mut D3d9Device, values: &[f32]) -> Status {
    if values.len() != 17 {
        return bridge_error(RenderError::invalid(
            "SetMaterial",
            "a material requires exactly 17 floats",
        ));
    }
    let mut material = [0u32; 17];
    for (slot, value) in material.iter_mut().zip(values) {
        *slot = value.to_bits();
    }
    bridge_ok(device.state.set_material(material))
}

pub fn d3d9_get_material(device: &D3d9Device) -> FloatValuesResult {
    ok_floats(
        device
            .state
            .material()
            .iter()
            .map(|b| f32::from_bits(*b))
            .collect(),
    )
}

pub fn d3d9_set_light(
    device: &mut D3d9Device,
    index: u32,
    light_type: u32,
    values: &[f32],
) -> Status {
    if values.len() != 25 {
        return bridge_error(RenderError::invalid(
            "SetLight",
            "a light requires exactly 25 floats",
        ));
    }
    let mut light = [0u32; 26];
    light[0] = light_type;
    for (slot, value) in light[1..].iter_mut().zip(values) {
        *slot = value.to_bits();
    }
    bridge_ok(device.state.set_light(index, light))
}

pub fn d3d9_get_light(device: &D3d9Device, index: u32) -> D3d9LightResult {
    match device.state.light(index) {
        Ok(light) => D3d9LightResult {
            status: status(StatusCode::Ok, ""),
            light_type: light[0],
            values: light[1..].iter().map(|b| f32::from_bits(*b)).collect(),
        },
        Err(error) => D3d9LightResult {
            status: bridge_error(error),
            light_type: 0,
            values: Vec::new(),
        },
    }
}

pub fn d3d9_set_light_enable(device: &mut D3d9Device, index: u32, enable: bool) -> Status {
    bridge_ok(device.state.set_light_enable(index, enable))
}

pub fn d3d9_get_light_enable(device: &D3d9Device, index: u32) -> ValueResult {
    match device.state.light_enabled(index) {
        Ok(enabled) => ok_value(u32::from(enabled)),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_set_clip_plane(device: &mut D3d9Device, index: u32, values: &[f32]) -> Status {
    let Ok(plane) = <[f32; 4]>::try_from(values) else {
        return bridge_error(RenderError::invalid(
            "SetClipPlane",
            "a clip plane requires exactly 4 floats",
        ));
    };
    bridge_ok(device.state.set_clip_plane(index, plane))
}

pub fn d3d9_get_clip_plane(device: &D3d9Device, index: u32) -> FloatValuesResult {
    if index as usize >= state::MAX_CLIP_PLANES {
        return err_floats(RenderError::invalid(
            "GetClipPlane",
            format!("clip plane {index} is out of range"),
        ));
    }
    ok_floats(device.state.clip_plane(index).to_vec())
}

fn set_texture(device: &mut D3d9Device, stage: u32, id: u32) -> Result<(), RenderError> {
    validate_binding(
        device,
        id,
        &[
            resource::D3DRTYPE_TEXTURE,
            resource::D3DRTYPE_CUBETEXTURE,
            resource::D3DRTYPE_VOLUMETEXTURE,
        ],
    )?;
    device.state.set_texture(stage, id)
}

pub fn d3d9_set_texture(device: &mut D3d9Device, stage: u32, id: u32) -> Status {
    bridge_ok(set_texture(device, stage, id))
}

pub fn d3d9_get_texture(device: &D3d9Device, stage: u32) -> ValueResult {
    match device.state.texture(stage) {
        Ok(value) => ok_value(value),
        Err(error) => err_value(error),
    }
}

fn set_stream_source(
    device: &mut D3d9Device,
    stream: u32,
    buffer: u32,
    offset: u32,
    stride: u32,
) -> Result<(), RenderError> {
    validate_binding(device, buffer, &[resource::D3DRTYPE_VERTEXBUFFER])?;
    device.state.set_stream_source(
        stream,
        state::StreamSource {
            buffer,
            offset,
            stride,
        },
    )
}

pub fn d3d9_set_stream_source(
    device: &mut D3d9Device,
    stream: u32,
    buffer: u32,
    offset: u32,
    stride: u32,
) -> Status {
    bridge_ok(set_stream_source(device, stream, buffer, offset, stride))
}

pub fn d3d9_get_stream_source(device: &D3d9Device, stream: u32) -> D3d9StreamResult {
    match device.state.stream_source(stream) {
        Ok(source) => D3d9StreamResult {
            status: status(StatusCode::Ok, ""),
            buffer: source.buffer,
            offset: source.offset,
            stride: source.stride,
        },
        Err(error) => D3d9StreamResult {
            status: bridge_error(error),
            buffer: 0,
            offset: 0,
            stride: 0,
        },
    }
}

pub fn d3d9_set_stream_freq(device: &mut D3d9Device, stream: u32, divider: u32) -> Status {
    bridge_ok(device.state.set_stream_freq(stream, divider))
}

pub fn d3d9_get_stream_freq(device: &D3d9Device, stream: u32) -> ValueResult {
    match device.state.stream_freq(stream) {
        Ok(value) => ok_value(value),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_set_indices(device: &mut D3d9Device, id: u32) -> Status {
    if let Err(error) = validate_binding(device, id, &[resource::D3DRTYPE_INDEXBUFFER]) {
        return bridge_error(error);
    }
    device.state.set_indices(id);
    status(StatusCode::Ok, "")
}

pub fn d3d9_get_indices(device: &D3d9Device) -> ValueResult {
    ok_value(device.state.bindings().indices)
}

fn create_fvf_declaration(device: &mut D3d9Device, fvf: u32) -> Result<u32, RenderError> {
    let elements = shader::fvf_declaration(fvf)?;
    device.objects.add_fvf_declaration(fvf, &elements)
}

pub fn d3d9_create_fvf_declaration(device: &mut D3d9Device, fvf: u32) -> ValueResult {
    match create_fvf_declaration(device, fvf) {
        Ok(handle) => ok_value(handle),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_set_fvf_binding(device: &mut D3d9Device, handle: u32, fvf: u32) -> Status {
    device.state.set_declaration(handle, fvf);
    status(StatusCode::Ok, "")
}

pub fn d3d9_get_fvf(device: &D3d9Device) -> ValueResult {
    ok_value(device.state.fvf())
}

pub fn d3d9_create_vertex_declaration(
    device: &mut D3d9Device,
    elements: &[D3d9VertexElement],
) -> ValueResult {
    let mapped: Vec<shader::VertexElement> = elements
        .iter()
        .map(|element| shader::VertexElement {
            stream: element.stream,
            offset: element.offset,
            element_type: element.element_type,
            method: element.method,
            usage: element.usage,
            usage_index: element.usage_index,
        })
        .collect();
    match device.objects.add_declaration(&mapped) {
        Ok(handle) => ok_value(handle),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_destroy_declaration(device: &mut D3d9Device, handle: u32) -> Status {
    bridge_ok(device.objects.destroy_declaration(handle))
}

pub fn d3d9_declaration_elements(device: &D3d9Device, handle: u32) -> D3d9DeclarationResult {
    match device.objects.declaration(handle) {
        Ok(elements) => D3d9DeclarationResult {
            status: status(StatusCode::Ok, ""),
            elements: elements
                .iter()
                .map(|element| D3d9VertexElement {
                    stream: element.stream,
                    offset: element.offset,
                    element_type: element.element_type,
                    method: element.method,
                    usage: element.usage,
                    usage_index: element.usage_index,
                })
                .collect(),
        },
        Err(error) => D3d9DeclarationResult {
            status: bridge_error(error),
            elements: Vec::new(),
        },
    }
}

pub fn d3d9_set_vertex_declaration(device: &mut D3d9Device, handle: u32) -> Status {
    if handle != 0 && device.objects.declaration(handle).is_err() {
        return bridge_error(RenderError::invalid(
            "SetVertexDeclaration",
            "invalid vertex declaration handle",
        ));
    }
    device.state.set_declaration(handle, 0);
    status(StatusCode::Ok, "")
}

pub fn d3d9_get_vertex_declaration(device: &D3d9Device) -> ValueResult {
    ok_value(device.state.declaration())
}

pub fn d3d9_create_shader(device: &mut D3d9Device, pixel: bool, code: &[u32]) -> ValueResult {
    match device.objects.add_shader(pixel, code) {
        Ok(handle) => ok_value(handle),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_destroy_shader(device: &mut D3d9Device, pixel: bool, handle: u32) -> Status {
    let status_value = bridge_ok(device.objects.destroy_shader(pixel, handle));
    if status_value.code == StatusCode::Ok && device.state.shader(pixel) == handle {
        device.state.set_shader(pixel, 0);
    }
    status_value
}

pub fn d3d9_set_shader(device: &mut D3d9Device, pixel: bool, handle: u32) -> Status {
    if handle != 0 && device.objects.shader(pixel, handle).is_err() {
        return bridge_error(RenderError::invalid(
            "SetShader",
            "invalid shader handle for the shader stage",
        ));
    }
    device.state.set_shader(pixel, handle);
    status(StatusCode::Ok, "")
}

pub fn d3d9_get_shader(device: &D3d9Device, pixel: bool) -> ValueResult {
    ok_value(device.state.shader(pixel))
}

pub fn d3d9_shader_function(device: &D3d9Device, pixel: bool, handle: u32) -> ValuesResult {
    match device.objects.shader(pixel, handle) {
        Ok(record) => ok_values(record.code.to_vec()),
        Err(error) => err_values(error),
    }
}

pub fn d3d9_shader_size(device: &D3d9Device, pixel: bool, handle: u32) -> ValueResult {
    match device.objects.shader(pixel, handle) {
        Ok(record) => ok_value(record.code.len() as u32 * 4),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_set_shader_constants_f(
    device: &mut D3d9Device,
    pixel: bool,
    start: u32,
    values: &[f32],
) -> Status {
    bridge_ok(device.state.set_constants_f(pixel, start, values))
}

pub fn d3d9_get_shader_constants_f(
    device: &D3d9Device,
    pixel: bool,
    start: u32,
    count: u32,
) -> FloatValuesResult {
    match device.state.constants_f(pixel, start, count) {
        Ok(values) => ok_floats(values),
        Err(error) => err_floats(error),
    }
}

pub fn d3d9_set_shader_constants_i(
    device: &mut D3d9Device,
    pixel: bool,
    start: u32,
    values: &[i32],
) -> Status {
    bridge_ok(device.state.set_constants_i(pixel, start, values))
}

pub fn d3d9_get_shader_constants_i(
    device: &D3d9Device,
    pixel: bool,
    start: u32,
    count: u32,
) -> ValuesResult {
    match device.state.constants_i(pixel, start, count) {
        Ok(values) => ok_values(values.into_iter().map(|value| value as u32).collect()),
        Err(error) => err_values(error),
    }
}

pub fn d3d9_set_shader_constants_b(
    device: &mut D3d9Device,
    pixel: bool,
    start: u32,
    values: &[u32],
) -> Status {
    bridge_ok(device.state.set_constants_b(pixel, start, values))
}

pub fn d3d9_get_shader_constants_b(
    device: &D3d9Device,
    pixel: bool,
    start: u32,
    count: u32,
) -> ValuesResult {
    match device.state.constants_b(pixel, start, count) {
        Ok(values) => ok_values(values),
        Err(error) => err_values(error),
    }
}

pub fn d3d9_create_state_block(device: &mut D3d9Device, block_type: u32) -> ValueResult {
    match device.state.create_block(block_type) {
        Ok(handle) => ok_value(handle),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_begin_state_block(device: &mut D3d9Device) -> Status {
    bridge_ok(device.state.begin_block())
}

pub fn d3d9_end_state_block(device: &mut D3d9Device) -> ValueResult {
    match device.state.end_block() {
        Ok(handle) => ok_value(handle),
        Err(error) => err_value(error),
    }
}

pub fn d3d9_capture_state_block(device: &mut D3d9Device, handle: u32) -> Status {
    bridge_ok(device.state.capture_block(handle))
}

pub fn d3d9_apply_state_block(device: &mut D3d9Device, handle: u32) -> Status {
    bridge_ok(device.state.apply_block(handle))
}

pub fn d3d9_destroy_state_block(device: &mut D3d9Device, handle: u32) -> Status {
    bridge_ok(device.state.destroy_block(handle))
}

pub fn d3d9_state_block_bindings(device: &D3d9Device, handle: u32) -> D3d9BlockBindingsResult {
    match device.state.block_bindings(handle) {
        Ok(bindings) => D3d9BlockBindingsResult {
            status: status(StatusCode::Ok, ""),
            bindings: bridge_bindings(bindings),
        },
        Err(error) => D3d9BlockBindingsResult {
            status: bridge_error(error),
            bindings: empty_bridge_bindings(),
        },
    }
}

pub fn d3d9_current_bindings(device: &D3d9Device) -> D3d9BindingsResult {
    let (recording, recording_active) = device.state.recording_bindings();
    D3d9BindingsResult {
        status: status(StatusCode::Ok, ""),
        live: bridge_bindings(device.state.bindings()),
        recording: bridge_bindings(recording),
        recording_active,
    }
}

fn bridge_gpu(device: &D3d9Device) -> Result<D3d9Gpu, RenderError> {
    device.resource_gpu().cloned()
}

fn flush_draws(device: &mut D3d9Device) -> Result<(), RenderError> {
    let gpu = bridge_gpu(device)?;
    flush_draws_with_gpu(device, &gpu)
}

fn flush_draws_with_gpu(device: &mut D3d9Device, gpu: &D3d9Gpu) -> Result<(), RenderError> {
    draw::flush(gpu, &mut device.draw, &mut device.queries)
}

fn device_gpu(device: &D3d9Device) -> Result<D3d9Gpu, Status> {
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return Err(bridge_error(error)),
    };
    if gpu.is_physical() {
        return Err(status(
            StatusCode::DeviceLost,
            gpu.loss().physical_message(),
        ));
    }
    if gpu.loss().is_reset_required() {
        return Err(status(
            StatusCode::DeviceLost,
            "the device must be reset before this call",
        ));
    }
    Ok(gpu)
}

fn invalid(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::invalid(op, why)
}

fn unsupported(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::new(op, why)
}

fn alloc_bytes(size: u32) -> Result<Vec<u8>, RenderError> {
    let mut out = Vec::new();
    out.try_reserve_exact(size as usize)
        .map_err(|_| RenderError::out_of_memory("D3D9", "allocation failed"))?;
    out.resize(size as usize, 0);
    Ok(out)
}

fn resource_storage_format(resource: &D3d9Resource) -> Option<u32> {
    match resource.state.as_ref() {
        Some(resource::ResourceState::Texture(t)) => Some(t.format),
        Some(resource::ResourceState::Surface(s)) => Some(s.format),
        _ => None,
    }
}

fn map_rects(rects: &[crate::bridge::D3d9Rect]) -> Result<Vec<TargetRect>, Status> {
    if rects.len() > 1 {
        return Err(status(
            StatusCode::InvalidArgument,
            "a D3D9 call accepts at most one rectangle",
        ));
    }
    Ok(rects
        .iter()
        .map(|r| TargetRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        })
        .collect())
}

fn clear_region(
    rect: Option<TargetRect>,
    viewport: &state::Viewport,
    width: u32,
    height: u32,
) -> Option<TargetRect> {
    let vp_left = viewport.x.min(i32::MAX as u32) as i32;
    let vp_top = viewport.y.min(i32::MAX as u32) as i32;
    let vp_right = viewport
        .x
        .saturating_add(viewport.width)
        .min(i32::MAX as u32) as i32;
    let vp_bottom = viewport
        .y
        .saturating_add(viewport.height)
        .min(i32::MAX as u32) as i32;
    let (left, top, right, bottom) = match rect {
        Some(rect) => (rect.left, rect.top, rect.right, rect.bottom),
        None => (vp_left, vp_top, vp_right, vp_bottom),
    };
    let left = left.max(0).max(vp_left);
    let top = top.max(0).max(vp_top);
    let right = right.min(width.min(i32::MAX as u32) as i32).min(vp_right);
    let bottom = bottom
        .min(height.min(i32::MAX as u32) as i32)
        .min(vp_bottom);
    if right <= left || bottom <= top {
        return None;
    }
    Some(TargetRect {
        left,
        top,
        right,
        bottom,
    })
}

fn index_element_size(index_format: u32) -> Result<u32, RenderError> {
    match index_format {
        101 => Ok(2),
        102 => Ok(4),
        _ => Err(invalid(
            "Draw",
            format!("unknown D3DFORMAT index type {index_format}"),
        )),
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct TextureViewKey {
    texture: wgpu::Texture,
    format: Option<wgpu::TextureFormat>,
    dimension: Option<wgpu::TextureViewDimension>,
    aspect: wgpu::TextureAspect,
    base_mip_level: u32,
    mip_level_count: Option<u32>,
    base_array_layer: u32,
    array_layer_count: Option<u32>,
    usage: Option<wgpu::TextureUsages>,
}

fn cached_texture_view(
    cache: &RefCell<FastMap<TextureViewKey, wgpu::TextureView>>,
    texture: &wgpu::Texture,
    descriptor: wgpu::TextureViewDescriptor<'_>,
) -> wgpu::TextureView {
    let key = TextureViewKey {
        texture: texture.clone(),
        format: descriptor.format,
        dimension: descriptor.dimension,
        aspect: descriptor.aspect,
        base_mip_level: descriptor.base_mip_level,
        mip_level_count: descriptor.mip_level_count,
        base_array_layer: descriptor.base_array_layer,
        array_layer_count: descriptor.array_layer_count,
        usage: descriptor.usage,
    };
    let mut views = cache.borrow_mut();
    if let Some(view) = views.get(&key) {
        return view.clone();
    }
    let view = texture.create_view(&descriptor);
    views.insert(key, view.clone());
    view
}

fn texture_view(
    cache: &RefCell<FastMap<TextureViewKey, wgpu::TextureView>>,
    texture: &wgpu::Texture,
    face: u32,
    level: u32,
    volume: bool,
    cube: bool,
) -> wgpu::TextureView {
    let dimension = if volume {
        wgpu::TextureViewDimension::D3
    } else {
        wgpu::TextureViewDimension::D2
    };
    let array_layer = if cube && !volume { face } else { 0 };
    cached_texture_view(
        cache,
        texture,
        wgpu::TextureViewDescriptor {
            dimension: Some(dimension),
            base_mip_level: level,
            mip_level_count: Some(1),
            base_array_layer: array_layer,
            array_layer_count: Some(1),
            ..Default::default()
        },
    )
}

fn sampling_view(
    cache: &RefCell<FastMap<TextureViewKey, wgpu::TextureView>>,
    texture: &wgpu::Texture,
    kind: resource::TextureKind,
    depth: bool,
) -> wgpu::TextureView {
    let aspect = if depth {
        wgpu::TextureAspect::DepthOnly
    } else {
        wgpu::TextureAspect::All
    };
    match kind {
        resource::TextureKind::Cube => cached_texture_view(
            cache,
            texture,
            wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::Cube),
                aspect,
                base_array_layer: 0,
                array_layer_count: Some(6),
                ..Default::default()
            },
        ),
        resource::TextureKind::Volume => cached_texture_view(
            cache,
            texture,
            wgpu::TextureViewDescriptor {
                dimension: Some(wgpu::TextureViewDimension::D3),
                aspect,
                ..Default::default()
            },
        ),
        resource::TextureKind::Texture2D => cached_texture_view(
            cache,
            texture,
            wgpu::TextureViewDescriptor {
                aspect,
                ..Default::default()
            },
        ),
    }
}

fn require_unlocked(
    device: &D3d9Device,
    id: u32,
    operation: &'static str,
) -> Result<(), RenderError> {
    if device
        .locks
        .get(&id)
        .is_some_and(|locks| locks.iter().any(|l| l.load(Ordering::Acquire) != 0))
    {
        Err(invalid(operation, "resource has an outstanding lock"))
    } else {
        Ok(())
    }
}

fn resolve_target_view(
    device: &D3d9Device,
    id: u32,
    face: u32,
    level: u32,
    volume: bool,
) -> Result<SurfaceView, RenderError> {
    require_unlocked(device, id, "D3D9 surface")?;
    if id == 0 || id == device.implicit_backbuffer_id {
        let targets = device
            .targets
            .as_ref()
            .ok_or_else(|| invalid("D3D9", "device has no implicit back buffer"))?;
        return Ok(SurfaceView {
            texture: targets.target.texture.clone(),
            view: targets.target.view.clone(),
            format: targets.format,
            width: targets.target.width,
            height: targets.target.height,
            cube: false,
            volume: false,
            subresource: (0, 0, 0),
        });
    }
    if id == u32::MAX || id == device.implicit_depthbuffer_id {
        let targets = device
            .targets
            .as_ref()
            .ok_or_else(|| invalid("D3D9", "device has no implicit depth buffer"))?;
        let depth = targets
            .target
            .depth
            .as_ref()
            .ok_or_else(|| invalid("D3D9", "device has no implicit depth buffer"))?;
        return Ok(SurfaceView {
            texture: depth.texture.clone(),
            view: depth.view.clone(),
            format: targets.depth_format,
            width: targets.target.width,
            height: targets.target.height,
            cube: false,
            volume: false,
            subresource: (0, 0, 0),
        });
    }
    match device.registry.get(&id) {
        Some(GpuRegistryEntry::Texture {
            texture,
            kind,
            format,
            width,
            height,
            ..
        }) => Ok(SurfaceView {
            texture: texture.clone(),
            view: texture_view(
                &device.view_cache,
                texture,
                face,
                level,
                *kind == resource::TextureKind::Volume,
                *kind == resource::TextureKind::Cube,
            ),
            format: *format,
            width: (*width >> level).max(1),
            height: (*height >> level).max(1),
            cube: *kind == resource::TextureKind::Cube,
            volume: *kind == resource::TextureKind::Volume,
            subresource: (face, level, 0),
        }),
        Some(GpuRegistryEntry::TextureAlias {
            gpu,
            kind,
            format,
            width,
            height,
            face: alias_face,
            level: alias_level,
            volume: alias_volume,
            ..
        }) => {
            let texture = gpu
                .as_ref()
                .ok_or_else(|| unsupported("D3D9", "texture view has no GPU storage"))?;
            let view = texture_view(
                &device.view_cache,
                texture,
                *alias_face,
                *alias_level,
                *alias_volume,
                *kind == resource::TextureKind::Cube,
            );
            let _ = (face, level, volume);
            Ok(SurfaceView {
                texture: texture.clone(),
                view,
                format: *format,
                width: *width,
                height: *height,
                cube: *kind == resource::TextureKind::Cube,
                volume: *alias_volume,
                subresource: (*alias_face, *alias_level, 0),
            })
        }
        Some(GpuRegistryEntry::Surface {
            texture,
            format,
            width,
            height,
            ..
        }) => Ok(SurfaceView {
            texture: texture.clone(),
            view: cached_texture_view(
                &device.view_cache,
                texture,
                wgpu::TextureViewDescriptor::default(),
            ),
            format: *format,
            width: *width,
            height: *height,
            cube: false,
            volume: false,
            subresource: (0, 0, 0),
        }),
        Some(GpuRegistryEntry::Buffer { .. }) => {
            Err(invalid("D3D9", "a buffer cannot be a render target"))
        }
        None => Err(unsupported("D3D9", "target id is not registered")),
    }
}

fn resolve_binding(device: &D3d9Device, id: u32, stage: u32) -> Result<CachedBinding, RenderError> {
    let (texture, kind, format, width, height, lod) = match device.registry.get(&id) {
        Some(GpuRegistryEntry::Texture {
            texture,
            kind,
            format,
            width,
            height,
            lod,
            ..
        }) => (
            texture.clone(),
            *kind,
            *format,
            *width,
            *height,
            Some(Arc::clone(lod)),
        ),
        Some(GpuRegistryEntry::TextureAlias {
            gpu: Some(texture),
            kind,
            format,
            width,
            height,
            lod,
            ..
        }) => (
            texture.clone(),
            *kind,
            *format,
            *width,
            *height,
            Some(Arc::clone(lod)),
        ),
        Some(GpuRegistryEntry::Surface {
            texture,
            format,
            width,
            height,
            ..
        }) => (
            texture.clone(),
            resource::TextureKind::Texture2D,
            *format,
            *width,
            *height,
            None,
        ),
        _ => {
            return Err(unsupported(
                "Draw",
                format!("sampler stage {stage} has no GPU texture"),
            ));
        }
    };
    let is_depth = format::is_depth(format);
    let binding = draw::BoundTexture {
        stage,
        texture: texture.clone(),
        view: sampling_view(&device.view_cache, &texture, kind, is_depth),
        width,
        height,
        depth: is_depth,
        cube: kind == resource::TextureKind::Cube,
        volume: kind == resource::TextureKind::Volume,
        format,
        lod: lod.as_ref().map_or(0, |lod| lod.load(Ordering::Acquire)),
        levels: texture.mip_level_count(),
    };
    Ok(CachedBinding { id, lod, binding })
}

fn bound_textures_cached(
    device: &D3d9Device,
    cache: &mut [Option<CachedBinding>; state::MAX_SAMPLERS],
) -> Result<draw::BoundTextures, RenderError> {
    let mut out = draw::BoundTextures::new();
    for stage in 0..state::MAX_SAMPLERS as u32 {
        let id = device.state.texture(stage)?;
        if id == 0 {
            continue;
        }
        require_unlocked(device, id, "Draw texture")?;
        let cached = match cache[stage as usize].take() {
            Some(mut cached) if cached.id == id => {
                stats::record(|s| s.texture_hits += 1);
                if let Some(lod) = &cached.lod {
                    cached.binding.lod = lod.load(Ordering::Acquire);
                }
                cached
            }
            previous => {
                stats::record(|s| {
                    if previous.is_some() {
                        s.texture_changed_misses += 1;
                    } else {
                        s.texture_cold_misses += 1;
                    }
                });
                resolve_binding(device, id, stage)?
            }
        };
        out.push(cached.binding.clone());
        cache[stage as usize] = Some(cached);
    }
    Ok(out)
}

fn collect_streams(
    device: &D3d9Device,
    state: &state::D3d9State,
) -> Result<draw::Streams, RenderError> {
    let mut out = draw::Streams::new();
    for stream in 0..state::MAX_STREAMS as u32 {
        let source = state.stream_source(stream)?;
        if source.buffer == 0 {
            continue;
        }
        let Some(GpuRegistryEntry::Buffer { uploaded, .. }) = device.registry.get(&source.buffer)
        else {
            return Err(unsupported(
                "Draw",
                format!("vertex stream {stream} has no GPU buffer"),
            ));
        };
        let bytes = Arc::clone(&uploaded.lock().unwrap());
        out.push(draw::StreamData {
            stream,
            stride: source.stride,
            offset: source.offset,
            bytes,
        });
    }
    Ok(out)
}

fn index_buffer(device: &D3d9Device, id: u32) -> Result<(Arc<Vec<u8>>, u32), RenderError> {
    match device.registry.get(&id) {
        Some(GpuRegistryEntry::Buffer { uploaded, format }) => {
            Ok((Arc::clone(&uploaded.lock().unwrap()), *format))
        }
        _ => Err(unsupported("Draw", "index buffer has no GPU storage")),
    }
}

fn mark_target_fresh(device: &D3d9Device, id: u32) {
    if id == 0 || id == u32::MAX {
        return;
    }
    let fresh = match device.registry.get(&id) {
        Some(GpuRegistryEntry::Texture { fresh, .. })
        | Some(GpuRegistryEntry::TextureAlias { fresh, .. })
        | Some(GpuRegistryEntry::Surface { fresh, .. }) => Some(Arc::clone(fresh)),
        _ => None,
    };
    if let Some(fresh) = fresh {
        fresh.store(true, Ordering::Release);
    }
}

fn mark_clear_fresh(device: &D3d9Device, flags: u32) {
    if flags & 1 != 0 {
        mark_target_fresh(device, device.render_target_id);
    }
    if flags & 6 != 0 {
        mark_target_fresh(device, device.depth_target_id);
    }
}

fn mark_draw_fresh(device: &D3d9Device) {
    if device.state.render_state(168).unwrap_or(0) != 0 {
        mark_target_fresh(device, device.render_target_id);
    }
    let depth_write = device.state.render_state(7).unwrap_or(0) != 0
        && device.state.render_state(14).unwrap_or(0) != 0;
    let stencil_write = device.state.render_state(52).unwrap_or(0) != 0
        && device.state.render_state(59).unwrap_or(0xffff_ffff) != 0
        && [53, 54, 55]
            .iter()
            .any(|&state| device.state.render_state(state).unwrap_or(1) != 1);
    if depth_write || stencil_write {
        mark_target_fresh(device, device.depth_target_id);
    }
}

fn resolve_draw_views(
    device: &D3d9Device,
    key: (u32, u32, u64),
) -> Result<Box<DrawViews>, RenderError> {
    let target = resolve_target_view(device, device.render_target_id, 0, 0, false)?;
    let depth = if device.depth_target_id == 0 {
        None
    } else {
        Some(resolve_target_view(
            device,
            device.depth_target_id,
            0,
            0,
            false,
        )?)
    };
    if depth
        .as_ref()
        .is_some_and(|d| d.width < target.width || d.height < target.height)
    {
        return Err(invalid(
            "Draw",
            "depth surface is smaller than the render target",
        ));
    }
    Ok(Box::new(DrawViews { key, target, depth }))
}

fn dispatch_draw(device: &mut D3d9Device, command: draw::DrawCommand) -> Result<(), RenderError> {
    stats::record(|s| s.draws += 1);
    let gpu = bridge_gpu(device)?;
    require_unlocked(device, device.render_target_id, "D3D9 surface")?;
    if device.depth_target_id != 0 {
        require_unlocked(device, device.depth_target_id, "D3D9 surface")?;
    }
    let key = (
        device.render_target_id,
        device.depth_target_id,
        device.registry_epoch,
    );
    let views = match device.draw_views.take() {
        Some(views) if views.key == key => views,
        _ => resolve_draw_views(device, key)?,
    };
    let mut textures = match device.draw_textures.take() {
        Some(textures) if textures.epoch == device.registry_epoch => textures,
        previous => {
            stats::record(|s| {
                if let Some(textures) = &previous {
                    s.texture_epoch_invalidations += 1;
                    s.texture_invalidated_slots +=
                        textures.bindings.iter().filter(|b| b.is_some()).count() as u64;
                }
            });
            Box::new(DrawTextures {
                epoch: device.registry_epoch,
                bindings: Default::default(),
            })
        }
    };
    let bindings = bound_textures_cached(device, &mut textures.bindings)?;
    let snapshot = draw::DrawSnapshot {
        command,
        target: views.target.clone(),
        depth: views.depth.clone(),
        textures: bindings,
    };
    device.draw_views = Some(views);
    device.draw_textures = Some(textures);
    if gpu.is_physical() {
        return Err(RenderError::new("Draw", gpu.loss().physical_message()));
    }
    draw::execute(
        &gpu,
        &mut device.draw,
        &mut device.queries,
        &device.state,
        &device.objects,
        &snapshot,
    )?;
    mark_draw_fresh(device);
    Ok(())
}

fn target_metadata(device: &D3d9Device, id: u32) -> Option<(u32, u32, u32)> {
    match device.registry.get(&id)? {
        GpuRegistryEntry::Texture {
            format,
            pool,
            usage,
            ..
        }
        | GpuRegistryEntry::TextureAlias {
            format,
            pool,
            usage,
            ..
        }
        | GpuRegistryEntry::Surface {
            format,
            pool,
            usage,
            ..
        } => Some((*format, *pool, *usage)),
        GpuRegistryEntry::Buffer { .. } => None,
    }
}

fn validate_color_target(device: &D3d9Device, id: u32) -> Result<(), RenderError> {
    if id == u32::MAX {
        return Err(invalid(
            "SetRenderTarget",
            "the implicit depth surface is not a color target",
        ));
    }
    let (format, pool, usage) = target_metadata(device, id)
        .ok_or_else(|| invalid("SetRenderTarget", "target id is not a color surface"))?;
    if pool != resource::D3DPOOL_DEFAULT {
        return Err(invalid(
            "SetRenderTarget",
            "render targets must be in D3DPOOL_DEFAULT",
        ));
    }
    if usage & resource::D3DUSAGE_RENDERTARGET == 0 {
        return Err(invalid(
            "SetRenderTarget",
            "surface lacks D3DUSAGE_RENDERTARGET",
        ));
    }
    if format::is_depth(format) {
        return Err(invalid(
            "SetRenderTarget",
            "a depth/stencil surface is not a color target",
        ));
    }
    Ok(())
}

fn validate_depth_target(device: &D3d9Device, id: u32) -> Result<(), RenderError> {
    let (format, pool, usage) = target_metadata(device, id)
        .ok_or_else(|| invalid("SetDepthStencilSurface", "target id is not a depth surface"))?;
    if pool != resource::D3DPOOL_DEFAULT {
        return Err(invalid(
            "SetDepthStencilSurface",
            "depth targets must be in D3DPOOL_DEFAULT",
        ));
    }
    if usage & resource::D3DUSAGE_DEPTHSTENCIL == 0 {
        return Err(invalid(
            "SetDepthStencilSurface",
            "surface lacks D3DUSAGE_DEPTHSTENCIL",
        ));
    }
    if !format::is_depth(format) {
        return Err(invalid(
            "SetDepthStencilSurface",
            "a color surface is not a depth target",
        ));
    }
    Ok(())
}

fn begin_scene(device: &mut D3d9Device) -> Result<(), RenderError> {
    if device.gpu.is_none() {
        return Err(RenderError::invalid(
            "BeginScene",
            "D3D9 device is not initialized",
        ));
    }
    if device.in_scene {
        return Err(RenderError::invalid(
            "BeginScene",
            "BeginScene is already active",
        ));
    }
    flush_draws(device)?;
    device.in_scene = true;
    Ok(())
}

fn end_scene(device: &mut D3d9Device) -> Result<(), RenderError> {
    if !device.in_scene {
        return Err(RenderError::invalid(
            "EndScene",
            "EndScene without BeginScene",
        ));
    }
    flush_draws(device)?;
    device.in_scene = false;
    Ok(())
}

pub fn d3d9_begin_scene(device: &mut D3d9Device) -> Status {
    scene_status(begin_scene(device))
}

pub fn d3d9_end_scene(device: &mut D3d9Device) -> Status {
    scene_status(end_scene(device))
}

fn scene_status(result: Result<(), RenderError>) -> Status {
    match result {
        Err(error) if matches!(error.operation, "BeginScene" | "EndScene") => {
            status(StatusCode::InvalidArgument, error.cause)
        }
        result => bridge_ok(result),
    }
}

pub fn d3d9_flush(device: &mut D3d9Device) -> Status {
    match flush_draws(device) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_set_render_target(
    device: &mut D3d9Device,
    index: u32,
    id: u32,
    face: u32,
    level: u32,
    volume: bool,
) -> Status {
    if index != 0 {
        return status(
            StatusCode::Unsupported,
            "multiple render targets are not implemented",
        );
    }
    if id != 0 && id != device.implicit_backbuffer_id {
        if let Err(error) = validate_color_target(device, id) {
            return bridge_error(error);
        }
    }
    if let Err(error) = flush_draws(device) {
        return bridge_error(error);
    }
    match resolve_target_view(device, id, face, level, volume) {
        Ok(view) => {
            device.render_target_id = if id == device.implicit_backbuffer_id {
                0
            } else {
                id
            };
            device.state.set_target_size(view.width, view.height);
            device.state.reset_viewport();
            status(StatusCode::Ok, "")
        }
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_set_depth_stencil(
    device: &mut D3d9Device,
    id: u32,
    face: u32,
    level: u32,
    volume: bool,
) -> Status {
    if let Err(error) = flush_draws(device) {
        return bridge_error(error);
    }
    if id == 0 {
        device.depth_target_id = 0;
        return status(StatusCode::Ok, "");
    }
    if id != u32::MAX && id != device.implicit_depthbuffer_id {
        if let Err(error) = validate_depth_target(device, id) {
            return bridge_error(error);
        }
    }
    match resolve_target_view(device, id, face, level, volume) {
        Ok(_) => {
            device.depth_target_id = if id == device.implicit_depthbuffer_id {
                u32::MAX
            } else {
                id
            };
            status(StatusCode::Ok, "")
        }
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_get_render_target(device: &D3d9Device) -> ValueResult {
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value: device.render_target_id,
    }
}

pub fn d3d9_get_depth_stencil(device: &D3d9Device) -> ValueResult {
    let value = if device.depth_target_id == 0 {
        0
    } else if device.depth_target_id == device.implicit_depthbuffer_id {
        u32::MAX
    } else {
        device.depth_target_id
    };
    ValueResult {
        status: status(StatusCode::Ok, ""),
        value,
    }
}

fn clear(
    device: &mut D3d9Device,
    flags: u32,
    color: u32,
    z: f32,
    stencil: u32,
    rects: &[TargetRect],
) -> Result<(), api::ApiError> {
    let gpu = device_gpu(device).map_err(api::ApiError::from)?;
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return Err(bridge_gpu_ok(&gpu, Err(error)).into());
    }
    let target = match resolve_target_view(device, device.render_target_id, 0, 0, false) {
        Ok(view) => view,
        Err(error) => return Err(error.into()),
    };
    let depth = match if device.depth_target_id == 0 {
        None
    } else {
        Some(resolve_target_view(
            device,
            device.depth_target_id,
            0,
            0,
            false,
        ))
    } {
        None => None,
        Some(Ok(view)) => Some(view),
        Some(Err(error)) => return Err(error.into()),
    };
    let viewport = device.state.viewport();
    let mut regions = Vec::new();
    if rects.is_empty() {
        if let Some(region) = clear_region(None, viewport, target.width, target.height) {
            regions.push(region);
        }
    } else {
        for rect in rects {
            if let Some(region) = clear_region(Some(*rect), viewport, target.width, target.height) {
                regions.push(region);
            }
        }
    }
    match gpu.validate("Clear", || {
        draw::clear(
            &gpu,
            &mut device.draw,
            &mut device.queries,
            &target,
            depth.as_ref(),
            flags,
            color,
            z,
            stencil,
            &regions,
        )
    }) {
        Ok(()) => {
            mark_clear_fresh(device, flags);
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub fn d3d9_clear(
    device: &mut D3d9Device,
    flags: u32,
    color: u32,
    z: f32,
    stencil: u32,
    rects: &[crate::bridge::D3d9Rect],
) -> Status {
    let rects: Vec<_> = rects
        .iter()
        .map(|rect| TargetRect {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        })
        .collect();
    match clear(device, flags, color, z, stencil, &rects) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => status(error.kind.status_code(), error.message),
    }
}

fn draw_primitive(
    device: &mut D3d9Device,
    primitive: u32,
    start_vertex: u32,
    primitive_count: u32,
) -> Result<(), api::ApiError> {
    if !device.in_scene {
        return Err(api::ApiError {
            kind: api::ApiErrorKind::InvalidArgument,
            message: "DrawPrimitive requires BeginScene".into(),
        });
    }
    device_gpu(device).map_err(api::ApiError::from)?;
    let streams = collect_streams(device, &device.state)?;
    let command = draw::DrawCommand {
        primitive,
        primitive_count,
        start_vertex,
        base_vertex: 0,
        indexed: false,
        index_format: 0,
        index_bytes: Arc::default(),
        min_vertex: 0,
        num_vertices: 0,
        streams,
    };
    dispatch_draw(device, command)?;
    Ok(())
}

pub fn d3d9_draw_primitive(
    device: &mut D3d9Device,
    primitive: u32,
    start_vertex: u32,
    primitive_count: u32,
) -> Status {
    match draw_primitive(device, primitive, start_vertex, primitive_count) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => status(error.kind.status_code(), error.message),
    }
}

pub fn d3d9_draw_indexed_primitive(
    device: &mut D3d9Device,
    primitive: u32,
    base_vertex: i32,
    min_index: u32,
    num_vertices: u32,
    start_index: u32,
    primitive_count: u32,
) -> Status {
    if !device.in_scene {
        return status(
            StatusCode::InvalidArgument,
            "DrawIndexedPrimitive requires BeginScene",
        );
    }
    if let Err(status) = device_gpu(device) {
        return status;
    }
    let index_id = device.state.bindings().indices;
    if index_id == 0 {
        return status(StatusCode::InvalidArgument, "no index buffer is bound");
    }
    if min_index.checked_add(num_vertices).is_none() {
        return status(StatusCode::InvalidArgument, "vertex hint range overflows");
    }
    let (index_bytes, index_format) = match index_buffer(device, index_id) {
        Ok(value) => value,
        Err(error) => return bridge_error(error),
    };
    let streams = match collect_streams(device, &device.state) {
        Ok(streams) => streams,
        Err(error) => return bridge_error(error),
    };
    let command = draw::DrawCommand {
        primitive,
        primitive_count,
        start_vertex: start_index,
        base_vertex,
        indexed: true,
        index_format,
        index_bytes,
        min_vertex: min_index,
        num_vertices,
        streams,
    };
    match dispatch_draw(device, command) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_draw_primitive_up(
    device: &mut D3d9Device,
    primitive: u32,
    primitive_count: u32,
    vertices: &[u8],
    stride: u32,
) -> Status {
    if !device.in_scene {
        return status(
            StatusCode::InvalidArgument,
            "DrawPrimitiveUP requires BeginScene",
        );
    }
    if let Err(status) = device_gpu(device) {
        device.state.clear_up_stream();
        return status;
    }
    let required = match draw::primitive_vertex_count(primitive, primitive_count) {
        Ok(count) => count,
        Err(error) => return bridge_error(error),
    };
    if (required as u64)
        .checked_mul(stride as u64)
        .is_none_or(|bytes| bytes > vertices.len() as u64)
    {
        return status(
            StatusCode::InvalidArgument,
            "DrawPrimitiveUP vertex data is shorter than the primitive span",
        );
    }
    let streams = smallvec::smallvec![draw::StreamData {
        stream: 0,
        stride,
        offset: 0,
        bytes: Arc::new(vertices.to_vec()),
    }];
    let command = draw::DrawCommand {
        primitive,
        primitive_count,
        start_vertex: 0,
        base_vertex: 0,
        indexed: false,
        index_format: 0,
        index_bytes: Arc::default(),
        min_vertex: 0,
        num_vertices: 0,
        streams,
    };
    let status = match dispatch_draw(device, command) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    };
    device.state.clear_up_stream();
    status
}

pub fn d3d9_draw_indexed_primitive_up(
    device: &mut D3d9Device,
    primitive: u32,
    min_vertex: u32,
    num_vertices: u32,
    primitive_count: u32,
    indices: &[u8],
    index_format: u32,
    vertices: &[u8],
    stride: u32,
) -> Status {
    if !device.in_scene {
        return status(
            StatusCode::InvalidArgument,
            "DrawIndexedPrimitiveUP requires BeginScene",
        );
    }
    if let Err(status) = device_gpu(device) {
        device.state.clear_up_indexed();
        return status;
    }
    let index_size = match index_element_size(index_format) {
        Ok(size) => size,
        Err(error) => return bridge_error(error),
    };
    let required_indices = match draw::primitive_vertex_count(primitive, primitive_count) {
        Ok(count) => count,
        Err(error) => return bridge_error(error),
    };
    if (required_indices as u64)
        .checked_mul(index_size as u64)
        .is_none_or(|bytes| bytes > indices.len() as u64)
    {
        return status(
            StatusCode::InvalidArgument,
            "DrawIndexedPrimitiveUP index data is shorter than the primitive span",
        );
    }
    let hint_vertices = match min_vertex.checked_add(num_vertices) {
        Some(count) => count,
        None => return status(StatusCode::InvalidArgument, "vertex hint range overflows"),
    };
    if (hint_vertices as u64)
        .checked_mul(stride as u64)
        .is_none_or(|bytes| bytes > vertices.len() as u64)
    {
        return status(
            StatusCode::InvalidArgument,
            "DrawIndexedPrimitiveUP vertex data is shorter than MinVertexIndex + NumVertices",
        );
    }
    let streams = smallvec::smallvec![draw::StreamData {
        stream: 0,
        stride,
        offset: 0,
        bytes: Arc::new(vertices.to_vec()),
    }];
    let command = draw::DrawCommand {
        primitive,
        primitive_count,
        start_vertex: 0,
        base_vertex: 0,
        indexed: true,
        index_format,
        index_bytes: Arc::new(indices.to_vec()),
        min_vertex,
        num_vertices,
        streams,
    };
    let status = match dispatch_draw(device, command) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    };
    device.state.clear_up_indexed();
    status
}

fn depth_surface_meta(device: &D3d9Device, id: u32) -> Option<(u32, bool)> {
    if id == u32::MAX || id == device.implicit_depthbuffer_id {
        return Some((resource::D3DPOOL_DEFAULT, false));
    }
    match device.registry.get(&id) {
        Some(GpuRegistryEntry::Surface { pool, discard, .. }) => Some((*pool, *discard)),
        _ => None,
    }
}

fn validate_depth_stretch(
    device: &D3d9Device,
    src: u32,
    src_view: &SurfaceView,
    src_rect: Option<TargetRect>,
    dst: u32,
    dst_view: &SurfaceView,
    dst_rect: Option<TargetRect>,
    filter: u32,
) -> Result<(), RenderError> {
    if !format::is_depth(src_view.format) && !format::is_depth(dst_view.format) {
        return Ok(());
    }
    if device.in_scene {
        return Err(invalid(
            "StretchRect",
            "depth StretchRect is not allowed inside BeginScene/EndScene",
        ));
    }
    if src_view.format != dst_view.format {
        return Err(invalid(
            "StretchRect",
            "depth StretchRect requires matching formats",
        ));
    }
    let full = |rect: Option<TargetRect>, view: &SurfaceView| {
        rect.is_none_or(|r| {
            r.left == 0
                && r.top == 0
                && r.right == view.width as i32
                && r.bottom == view.height as i32
        })
    };
    if !full(src_rect, src_view) || !full(dst_rect, dst_view) {
        return Err(unsupported(
            "StretchRect",
            "depth StretchRect only supports full-surface copies",
        ));
    }
    if filter != 0 {
        return Err(unsupported(
            "StretchRect",
            "depth StretchRect requires D3DTEXF_NONE",
        ));
    }
    for (id, which) in [(src, "source"), (dst, "destination")] {
        let Some((pool, discard)) = depth_surface_meta(device, id) else {
            return Err(unsupported(
                "StretchRect",
                format!("depth StretchRect {which} must be a standalone surface"),
            ));
        };
        if pool != resource::D3DPOOL_DEFAULT {
            return Err(invalid(
                "StretchRect",
                format!("depth StretchRect {which} must be D3DPOOL_DEFAULT"),
            ));
        }
        if discard {
            return Err(invalid(
                "StretchRect",
                format!("depth StretchRect {which} must not be discardable"),
            ));
        }
    }
    Ok(())
}

pub fn d3d9_stretch_rect(
    device: &mut D3d9Device,
    src: u32,
    src_rect: &[crate::bridge::D3d9Rect],
    dst: u32,
    dst_rect: &[crate::bridge::D3d9Rect],
    filter: u32,
) -> Status {
    let gpu = match device_gpu(device) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    let source = match resolve_target_view(device, src, 0, 0, false) {
        Ok(view) => view,
        Err(error) => return bridge_error(error),
    };
    let destination = match resolve_target_view(device, dst, 0, 0, false) {
        Ok(view) => view,
        Err(error) => return bridge_error(error),
    };
    let src_rect = match map_rects(src_rect) {
        Ok(rects) => rects.into_iter().next(),
        Err(status) => return status,
    };
    let dst_rect = match map_rects(dst_rect) {
        Ok(rects) => rects.into_iter().next(),
        Err(status) => return status,
    };
    if let Err(error) = validate_depth_stretch(
        device,
        src,
        &source,
        src_rect,
        dst,
        &destination,
        dst_rect,
        filter,
    ) {
        return bridge_error(error);
    }
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return bridge_gpu_ok(&gpu, Err(error));
    }
    match copy::stretch_rect(&gpu, &source, src_rect, &destination, dst_rect, filter) {
        Ok(()) => {
            mark_target_fresh(device, dst);
            status(StatusCode::Ok, "")
        }
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_update_surface(
    device: &mut D3d9Device,
    source: &mut D3d9Resource,
    src_face: u32,
    src_level: u32,
    src_rect: &[crate::bridge::D3d9Rect],
    destination: &mut D3d9Resource,
    dst_face: u32,
    dst_level: u32,
    dst_x: i32,
    dst_y: i32,
) -> Status {
    let gpu = match bridge_gpu(device) {
        Ok(gpu) => gpu,
        Err(error) => return bridge_error(error),
    };
    let src_rect = match map_rects(src_rect) {
        Ok(rects) => rects.into_iter().next(),
        Err(status) => return status,
    };
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return bridge_gpu_ok(&gpu, Err(error));
    }
    match resource::update_surface(
        &gpu,
        source,
        src_face,
        src_level,
        src_rect,
        destination,
        dst_face,
        dst_level,
        (dst_x, dst_y),
    ) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_update_texture(
    device: &mut D3d9Device,
    source: &mut D3d9Resource,
    destination: &mut D3d9Resource,
) -> Status {
    let gpu = match bridge_gpu(device) {
        Ok(gpu) => gpu,
        Err(error) => return bridge_error(error),
    };
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return bridge_gpu_ok(&gpu, Err(error));
    }
    match resource::update_texture(&gpu, source, destination) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

fn validate_readback_source(
    device: &D3d9Device,
    src: u32,
    source: &SurfaceView,
) -> Result<(), RenderError> {
    if src == u32::MAX || src == device.implicit_depthbuffer_id {
        return Err(invalid(
            "GetRenderTargetData",
            "the depth/stencil target cannot be read back",
        ));
    }
    if format::is_depth(source.format) {
        return Err(invalid(
            "GetRenderTargetData",
            "depth/stencil surfaces cannot be read back",
        ));
    }
    if src == 0 || src == device.implicit_backbuffer_id {
        return Ok(());
    }
    let (_, pool, usage) = target_metadata(device, src).ok_or_else(|| {
        invalid(
            "GetRenderTargetData",
            "source is not a registered render target",
        )
    })?;
    if pool != resource::D3DPOOL_DEFAULT {
        return Err(invalid(
            "GetRenderTargetData",
            "source render target must be in D3DPOOL_DEFAULT",
        ));
    }
    if usage & resource::D3DUSAGE_RENDERTARGET == 0 {
        return Err(invalid(
            "GetRenderTargetData",
            "source lacks D3DUSAGE_RENDERTARGET",
        ));
    }
    Ok(())
}

pub fn d3d9_get_render_target_data(
    device: &mut D3d9Device,
    src: u32,
    destination: &mut D3d9Resource,
    dst_face: u32,
    dst_level: u32,
) -> Status {
    let gpu = match device_gpu(device) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    let source = match resolve_target_view(device, src, 0, 0, false) {
        Ok(view) => view,
        Err(error) => return bridge_error(error),
    };
    if let Err(error) = validate_readback_source(device, src, &source) {
        return bridge_error(error);
    }
    let (width, height, depth, _) =
        match resource::level_descriptor(destination, dst_face, dst_level) {
            Ok(values) => values,
            Err(error) => return bridge_error(error),
        };
    if depth != 1 {
        return status(
            StatusCode::InvalidArgument,
            "GetRenderTargetData destination must be a single-slice surface",
        );
    }
    let fmt = match resource_storage_format(destination) {
        Some(fmt) => fmt,
        None => {
            return status(
                StatusCode::InvalidArgument,
                "GetRenderTargetData destination is not a texture or surface",
            );
        }
    };
    if source.format != fmt || source.width != width || source.height != height {
        return status(
            StatusCode::InvalidArgument,
            "GetRenderTargetData source and destination must match in format and size",
        );
    }
    let size = match format::level_size(fmt, width, height) {
        Some(size) => size,
        None => {
            return bridge_error(invalid(
                "GetRenderTargetData",
                "unsupported destination format",
            ));
        }
    };
    let mut bytes = match alloc_bytes(size) {
        Ok(bytes) => bytes,
        Err(error) => return bridge_error(error),
    };
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return bridge_gpu_ok(&gpu, Err(error));
    }
    if let Err(error) = copy::copy_to_cpu(&gpu, &source, None, &mut bytes) {
        return bridge_error(error);
    }
    match resource::store_readback(destination, dst_face, dst_level, &bytes, fmt, width, height) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_color_fill(
    device: &mut D3d9Device,
    dst: u32,
    rect: &[crate::bridge::D3d9Rect],
    color: u32,
) -> Status {
    let gpu = match device_gpu(device) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    let destination = match resolve_target_view(device, dst, 0, 0, false) {
        Ok(view) => view,
        Err(error) => return bridge_error(error),
    };
    let rect = match map_rects(rect) {
        Ok(rects) => rects.into_iter().next(),
        Err(status) => return status,
    };
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return bridge_gpu_ok(&gpu, Err(error));
    }
    match copy::color_fill(&gpu, &destination, rect, color) {
        Ok(()) => {
            mark_target_fresh(device, dst);
            status(StatusCode::Ok, "")
        }
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_create_query(device: &mut D3d9Device, kind: u32) -> ValueResult {
    let gpu = match device_gpu(device) {
        Ok(gpu) => gpu,
        Err(status) => return ValueResult { status, value: 0 },
    };
    match device.queries.create(&gpu, kind) {
        Ok(handle) => ValueResult {
            status: status(StatusCode::Ok, ""),
            value: handle,
        },
        Err(error) => ValueResult {
            status: if kind == 9 {
                status(StatusCode::Unsupported, error.to_string())
            } else {
                bridge_error(error)
            },
            value: 0,
        },
    }
}

pub fn d3d9_destroy_query(device: &mut D3d9Device, handle: u32) -> Status {
    match device.queries.destroy(handle) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_query_type(device: &D3d9Device, handle: u32) -> ValueResult {
    match device.queries.query_type(handle) {
        Ok(kind) => ValueResult {
            status: status(StatusCode::Ok, ""),
            value: kind,
        },
        Err(error) => ValueResult {
            status: bridge_error(error),
            value: 0,
        },
    }
}

pub fn d3d9_query_data_size(kind: u32) -> ValueResult {
    match crate::d3d9::query::D3d9Queries::data_size(kind) {
        Ok(size) => ValueResult {
            status: status(StatusCode::Ok, ""),
            value: size,
        },
        Err(error) => ValueResult {
            status: bridge_error(error),
            value: 0,
        },
    }
}

pub fn d3d9_query_issue(device: &mut D3d9Device, handle: u32, flags: u32) -> Status {
    if flags != 1 && flags != 2 {
        return status(
            StatusCode::InvalidArgument,
            "D3DISSUE requires exactly one of BEGIN or END",
        );
    }
    let gpu = match device_gpu(device) {
        Ok(gpu) => gpu,
        Err(status) => return status,
    };
    let begin = flags == 2;
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return bridge_gpu_ok(&gpu, Err(error));
    }
    match device.queries.issue(&gpu, handle, begin) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => bridge_error(error),
    }
}

pub fn d3d9_query_get_data(
    device: &mut D3d9Device,
    handle: u32,
    data: &mut [u8],
    size: u32,
    flags: u32,
) -> ValueResult {
    if size != 0 && size != 4 {
        return ValueResult {
            status: status(
                StatusCode::InvalidArgument,
                "query data size must be 0 or 4",
            ),
            value: 0,
        };
    }
    if flags & !1 != 0 {
        return ValueResult {
            status: status(
                StatusCode::InvalidArgument,
                "unknown D3DGETDATA flag for query readback",
            ),
            value: 0,
        };
    }
    if size == 4 && data.len() < 4 {
        return ValueResult {
            status: status(
                StatusCode::InvalidArgument,
                "query output buffer is too small",
            ),
            value: 0,
        };
    }
    let gpu = match device_gpu(device) {
        Ok(gpu) => gpu,
        Err(status) => {
            return ValueResult { status, value: 0 };
        }
    };
    if let Err(error) = flush_draws_with_gpu(device, &gpu) {
        return ValueResult {
            status: bridge_gpu_ok(&gpu, Err(error)),
            value: 0,
        };
    }
    match device.queries.get_data(&gpu, handle, flags & 1 != 0) {
        Ok(crate::d3d9::query::QueryPoll::Ready(value)) => {
            if size == 4 {
                data[..4].copy_from_slice(&value.to_le_bytes());
            }
            ValueResult {
                status: status(StatusCode::Ok, ""),
                value: 1,
            }
        }
        Ok(crate::d3d9::query::QueryPoll::Pending) => ValueResult {
            status: status(StatusCode::Ok, ""),
            value: 0,
        },
        Err(error) => ValueResult {
            status: bridge_error(error),
            value: 0,
        },
    }
}
