use super::TargetRect;
use super::format;
use super::gpu::D3d9Gpu;
use crate::RenderError;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const D3DPOOL_DEFAULT: u32 = 0;
pub const D3DPOOL_MANAGED: u32 = 1;
pub const D3DPOOL_SYSTEMMEM: u32 = 2;
pub const D3DPOOL_SCRATCH: u32 = 3;

pub const D3DUSAGE_RENDERTARGET: u32 = 0x0000_0001;
pub const D3DUSAGE_DEPTHSTENCIL: u32 = 0x0000_0002;
pub const D3DUSAGE_WRITEONLY: u32 = 0x0000_0008;
pub const D3DUSAGE_SOFTWAREPROCESSING: u32 = 0x0000_0010;
pub const D3DUSAGE_DONOTCLIP: u32 = 0x0000_0020;
pub const D3DUSAGE_POINTS: u32 = 0x0000_0040;
pub const D3DUSAGE_RTPATCHES: u32 = 0x0000_0080;
pub const D3DUSAGE_NPATCHES: u32 = 0x0000_0100;
pub const D3DUSAGE_DYNAMIC: u32 = 0x0000_0200;
pub const D3DUSAGE_AUTOGENMIPMAP: u32 = 0x0000_0400;
pub const D3DUSAGE_DMAP: u32 = 0x0000_4000;
pub const D3DUSAGE_QUERY_MASK: u32 = 0x003F_8000;
pub const D3DUSAGE_KNOWN: u32 = D3DUSAGE_RENDERTARGET
    | D3DUSAGE_DEPTHSTENCIL
    | D3DUSAGE_WRITEONLY
    | D3DUSAGE_SOFTWAREPROCESSING
    | D3DUSAGE_DONOTCLIP
    | D3DUSAGE_POINTS
    | D3DUSAGE_RTPATCHES
    | D3DUSAGE_NPATCHES
    | D3DUSAGE_DYNAMIC
    | D3DUSAGE_AUTOGENMIPMAP
    | D3DUSAGE_DMAP
    | D3DUSAGE_QUERY_MASK;

pub const BUFFER_USAGE_MASK: u32 = D3DUSAGE_DYNAMIC
    | D3DUSAGE_WRITEONLY
    | D3DUSAGE_SOFTWAREPROCESSING
    | D3DUSAGE_DONOTCLIP
    | D3DUSAGE_POINTS
    | D3DUSAGE_RTPATCHES
    | D3DUSAGE_NPATCHES;
pub const TEXTURE_USAGE_MASK: u32 =
    D3DUSAGE_RENDERTARGET | D3DUSAGE_DEPTHSTENCIL | D3DUSAGE_DYNAMIC | D3DUSAGE_AUTOGENMIPMAP;
pub const SURFACE_USAGE_MASK: u32 =
    D3DUSAGE_RENDERTARGET | D3DUSAGE_DEPTHSTENCIL | D3DUSAGE_DYNAMIC;

pub const D3DLOCK_READONLY: u32 = 0x0000_0010;
pub const D3DLOCK_NOSYSLOCK: u32 = 0x0000_0800;
pub const D3DLOCK_NOOVERWRITE: u32 = 0x0000_1000;
pub const D3DLOCK_DISCARD: u32 = 0x0000_2000;
pub const D3DLOCK_DONOTWAIT: u32 = 0x0000_4000;
pub const D3DLOCK_NO_DIRTY_UPDATE: u32 = 0x0000_8000;
pub const D3DLOCK_KNOWN: u32 = D3DLOCK_READONLY
    | D3DLOCK_NOSYSLOCK
    | D3DLOCK_NOOVERWRITE
    | D3DLOCK_DISCARD
    | D3DLOCK_DONOTWAIT
    | D3DLOCK_NO_DIRTY_UPDATE;
pub const D3DLOCK_DYNAMIC_ONLY: u32 = D3DLOCK_NOOVERWRITE | D3DLOCK_DISCARD;

pub const D3DSPD_IUNKNOWN: u32 = 0x0000_0001;

pub const D3DRTYPE_SURFACE: u32 = 1;
pub const D3DRTYPE_VOLUME: u32 = 2;
pub const D3DRTYPE_TEXTURE: u32 = 3;
pub const D3DRTYPE_VOLUMETEXTURE: u32 = 4;
pub const D3DRTYPE_CUBETEXTURE: u32 = 5;
pub const D3DRTYPE_VERTEXBUFFER: u32 = 6;
pub const D3DRTYPE_INDEXBUFFER: u32 = 7;

pub const AUTOGEN_FILTER_NONE: u32 = 0;
pub const AUTOGEN_FILTER_POINT: u32 = 1;
pub const AUTOGEN_FILTER_LINEAR: u32 = 2;
pub const AUTOGEN_FILTER_ANISOTROPIC: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferKind {
    Vertex,
    Index,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextureKind {
    Texture2D,
    Cube,
    Volume,
}

fn invalid(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::invalid(op, why)
}

fn unsupported(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::new(op, why)
}

fn oom(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::out_of_memory(op, why)
}

fn alloc_bytes(size: u32) -> Result<Vec<u8>, RenderError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size as usize)
        .map_err(|_| oom("CreateResource", "resource allocation failed"))?;
    bytes.resize(size as usize, 0);
    Ok(bytes)
}

fn bytes_to_f32(bytes: &[u8]) -> Result<Vec<f32>, RenderError> {
    if bytes.len() % 4 != 0 {
        return Err(invalid("UploadTexture", "depth bytes are not whole floats"));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len() / 4)
        .map_err(|_| oom("UploadTexture", "depth float conversion failed"))?;
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
enum Region {
    Full,
    Rect {
        left: u32,
        top: u32,
        front: u32,
        right: u32,
        bottom: u32,
        back: u32,
    },
}

#[derive(Clone)]
pub struct LevelState {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    depth: u32,
    lock_count: Arc<AtomicU32>,
    writable: Vec<Region>,
    dirty: bool,
    dirty_lo: u32,
    dirty_hi: u32,
    gpu_newer: Arc<AtomicBool>,
    generation: u64,
    gpu_generation: u64,
    writable_dirty: bool,
}

impl LevelState {
    fn new(bytes: Vec<u8>, width: u32, height: u32, depth: u32) -> Self {
        Self {
            bytes,
            width,
            height,
            depth,
            lock_count: Arc::new(AtomicU32::new(0)),
            writable: Vec::new(),
            dirty: false,
            dirty_lo: 0,
            dirty_hi: 0,
            gpu_newer: Arc::new(AtomicBool::new(false)),
            generation: 0,
            gpu_generation: 0,
            writable_dirty: false,
        }
    }

    fn mark_managed_initial(&mut self) {
        self.dirty = true;
        self.dirty_lo = 0;
        self.dirty_hi = self.bytes.len() as u32;
    }

    fn mark_dirty(&mut self, lo: u32, hi: u32) {
        if self.dirty {
            self.dirty_lo = self.dirty_lo.min(lo);
            self.dirty_hi = self.dirty_hi.max(hi);
        } else {
            self.dirty_lo = lo;
            self.dirty_hi = hi;
        }
        self.dirty = true;
    }
}

pub struct BufferState {
    pub kind: BufferKind,
    pub size: u32,
    pub usage: u32,
    pub pool: u32,
    pub fvf: u32,
    pub format: u32,
    pub bytes: Vec<u8>,
    pub uploaded: Arc<Mutex<Arc<Vec<u8>>>>,
    pub lock_count: Arc<AtomicU32>,
    pub writable: Vec<(u32, u32)>,
    pub writable_dirty: bool,
    pub dirty_lo: u32,
    pub dirty_hi: u32,
    pub generation: u64,
}

pub struct TextureState {
    pub kind: TextureKind,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub levels: u32,
    pub gpu_levels: u32,
    pub usage: u32,
    pub pool: u32,
    pub lockable: bool,
    pub autogen: bool,
    pub autogen_filter: u32,
    pub subresources: Vec<LevelState>,
    pub gpu: Option<wgpu::Texture>,
}

pub struct SurfaceState {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub usage: u32,
    pub pool: u32,
    pub multisample_type: u32,
    pub multisample_quality: u32,
    pub lockable: bool,
    pub depth: bool,
    pub discard: bool,
    pub level: LevelState,
    pub gpu: Option<wgpu::Texture>,
}

pub enum ResourceState {
    Buffer(BufferState),
    Texture(TextureState),
    Surface(SurfaceState),
}

pub struct PrivateData {
    pub view: u32,
    pub guid: [u8; 16],
    pub bytes: Vec<u8>,
}

pub struct D3d9Resource {
    pub(crate) gpu: Option<D3d9Gpu>,
    pub state: Option<ResourceState>,
    pub priority: u32,
    pub lod: Arc<AtomicU32>,
    pub private: Vec<PrivateData>,
    pub gpu_newer: Arc<AtomicBool>,
}

impl Default for D3d9Resource {
    fn default() -> Self {
        Self {
            gpu: None,
            state: None,
            priority: 0,
            lod: Arc::new(AtomicU32::new(0)),
            private: Vec::new(),
            gpu_newer: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl D3d9Resource {
    pub fn lock_handles(&self) -> Vec<Arc<AtomicU32>> {
        match self.state.as_ref() {
            Some(ResourceState::Buffer(b)) => vec![Arc::clone(&b.lock_count)],
            Some(ResourceState::Surface(s)) => vec![Arc::clone(&s.level.lock_count)],
            Some(ResourceState::Texture(t)) => t
                .subresources
                .iter()
                .map(|l| Arc::clone(&l.lock_count))
                .collect(),
            None => Vec::new(),
        }
    }
    pub fn level_lock_handle(&self, face: u32, level: u32) -> Result<Arc<AtomicU32>, RenderError> {
        self.level(face, level).map(|l| Arc::clone(&l.lock_count))
    }

    pub fn initialized(&self) -> bool {
        self.state.is_some()
    }

    pub fn resource_type(&self) -> Option<u32> {
        match self.state.as_ref()? {
            ResourceState::Buffer(b) => Some(match b.kind {
                BufferKind::Vertex => D3DRTYPE_VERTEXBUFFER,
                BufferKind::Index => D3DRTYPE_INDEXBUFFER,
            }),
            ResourceState::Texture(t) => Some(match t.kind {
                TextureKind::Texture2D => D3DRTYPE_TEXTURE,
                TextureKind::Cube => D3DRTYPE_CUBETEXTURE,
                TextureKind::Volume => D3DRTYPE_VOLUMETEXTURE,
            }),
            ResourceState::Surface(_) => Some(D3DRTYPE_SURFACE),
        }
    }

    pub fn pool(&self) -> Option<u32> {
        match self.state.as_ref()? {
            ResourceState::Buffer(b) => Some(b.pool),
            ResourceState::Texture(t) => Some(t.pool),
            ResourceState::Surface(s) => Some(s.pool),
        }
    }

    pub fn gpu(&self) -> Result<&D3d9Gpu, RenderError> {
        self.gpu
            .as_ref()
            .ok_or_else(|| unsupported("D3D9", "resource has no GPU context"))
    }

    pub fn gpu_handle(&self) -> Result<D3d9Gpu, RenderError> {
        self.gpu
            .clone()
            .ok_or_else(|| unsupported("D3D9", "resource has no GPU context"))
    }

    pub fn gpu_texture(&self) -> Option<&wgpu::Texture> {
        match self.state.as_ref()? {
            ResourceState::Texture(t) => t.gpu.as_ref(),
            ResourceState::Surface(s) => s.gpu.as_ref(),
            _ => None,
        }
    }

    pub fn texture_shape(&self) -> Option<(TextureKind, u32, u32, u32, u32, u32)> {
        match self.state.as_ref()? {
            ResourceState::Texture(t) => {
                Some((t.kind, t.format, t.width, t.height, t.depth, t.levels))
            }
            _ => None,
        }
    }

    pub fn surface_shape(&self) -> Option<(u32, u32, u32, bool, u32)> {
        match self.state.as_ref()? {
            ResourceState::Surface(s) => {
                Some((s.format, s.width, s.height, s.depth, s.multisample_type))
            }
            _ => None,
        }
    }

    pub fn subresource_layout(
        &self,
        face: u32,
        level: u32,
    ) -> Result<(u32, u32, u32, u32), RenderError> {
        let l = self.level(face, level)?;
        let format = match self.state.as_ref() {
            Some(ResourceState::Texture(t)) => t.format,
            Some(ResourceState::Surface(s)) => s.format,
            _ => 0,
        };
        Ok((format, l.width, l.height, l.depth))
    }

    pub fn write_subresource(
        &mut self,
        face: u32,
        level: u32,
        bytes: &[u8],
    ) -> Result<(), RenderError> {
        let l = self.level_mut(face, level)?;
        if l.lock_count.load(Ordering::Acquire) != 0 {
            return Err(invalid("D3DX", "resource is locked"));
        }
        if bytes.len() != l.bytes.len() {
            return Err(invalid(
                "D3DX",
                "decoded subresource size does not match the level",
            ));
        }
        l.bytes.copy_from_slice(bytes);
        l.generation = l.generation.wrapping_add(1);
        let size = l.bytes.len() as u32;
        l.mark_dirty(0, size);
        l.gpu_newer.store(false, Ordering::Release);
        Ok(())
    }

    pub fn freshness(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.gpu_newer)
    }

    pub fn level_freshness(&self, face: u32, level: u32) -> Option<Arc<AtomicBool>> {
        match self.state.as_ref()? {
            ResourceState::Surface(s) => Some(Arc::clone(&s.level.gpu_newer)),
            ResourceState::Texture(t) => texture_index(t.kind, face, level, t.levels)
                .and_then(|index| t.subresources.get(index))
                .map(|l| Arc::clone(&l.gpu_newer)),
            _ => None,
        }
    }

    pub fn texture_gpu(&self) -> Option<(wgpu::Texture, TextureKind, u32, u32, u32, u32)> {
        match self.state.as_ref()? {
            ResourceState::Texture(t) => t
                .gpu
                .as_ref()
                .map(|gpu| (gpu.clone(), t.kind, t.format, t.width, t.height, t.levels)),
            _ => None,
        }
    }

    pub fn surface_gpu(&self) -> Option<(wgpu::Texture, u32, u32, u32, bool)> {
        match self.state.as_ref()? {
            ResourceState::Surface(s) => s
                .gpu
                .as_ref()
                .map(|gpu| (gpu.clone(), s.format, s.width, s.height, s.depth)),
            _ => None,
        }
    }

    fn level(&self, face: u32, level: u32) -> Result<&LevelState, RenderError> {
        match self.state.as_ref() {
            Some(ResourceState::Texture(t)) => {
                let index = texture_index(t.kind, face, level, t.levels)
                    .ok_or_else(|| invalid("GetDesc", "texture subresource out of range"))?;
                t.subresources
                    .get(index)
                    .ok_or_else(|| invalid("GetDesc", "texture subresource missing"))
            }
            Some(ResourceState::Surface(s)) => {
                if face != 0 || level != 0 {
                    return Err(invalid("GetDesc", "surface has one subresource"));
                }
                Ok(&s.level)
            }
            _ => Err(invalid("GetDesc", "resource is not a texture")),
        }
    }

    fn level_mut(&mut self, face: u32, level: u32) -> Result<&mut LevelState, RenderError> {
        match self.state.as_mut() {
            Some(ResourceState::Texture(t)) => {
                let index = texture_index(t.kind, face, level, t.levels)
                    .ok_or_else(|| invalid("LockRect", "texture subresource out of range"))?;
                t.subresources
                    .get_mut(index)
                    .ok_or_else(|| invalid("LockRect", "texture subresource missing"))
            }
            Some(ResourceState::Surface(s)) => {
                if face != 0 || level != 0 {
                    return Err(invalid("LockRect", "surface has one subresource"));
                }
                Ok(&mut s.level)
            }
            _ => Err(invalid("LockRect", "resource is not a texture")),
        }
    }

    pub fn is_lockable(&self) -> bool {
        match self.state.as_ref() {
            Some(ResourceState::Buffer(_)) => true,
            Some(ResourceState::Texture(t)) => t.lockable,
            Some(ResourceState::Surface(s)) => s.lockable,
            None => false,
        }
    }

    pub fn is_managed(&self) -> bool {
        match self.state.as_ref() {
            Some(ResourceState::Buffer(b)) => b.pool == D3DPOOL_MANAGED,
            Some(ResourceState::Texture(t)) => t.pool == D3DPOOL_MANAGED,
            Some(ResourceState::Surface(s)) => s.pool == D3DPOOL_MANAGED,
            None => false,
        }
    }

    fn max_lod_level(&self) -> u32 {
        match self.state.as_ref() {
            Some(ResourceState::Texture(t)) => t.levels.saturating_sub(1),
            _ => 0,
        }
    }

    pub fn set_priority(&mut self, priority: u32) -> u32 {
        if !self.is_managed() {
            return 0;
        }
        let old = self.priority;
        self.priority = priority;
        old
    }

    pub fn set_lod(&mut self, lod: u32) -> u32 {
        if !self.is_managed() || !matches!(self.state, Some(ResourceState::Texture(_))) {
            return 0;
        }
        let old = self
            .lod
            .swap(lod.min(self.max_lod_level()), Ordering::AcqRel);
        old
    }

    pub fn set_private_data(
        &mut self,
        view: u32,
        guid: [u8; 16],
        data: Vec<u8>,
    ) -> Result<(), RenderError> {
        if let Some(entry) = self
            .private
            .iter_mut()
            .find(|e| e.view == view && e.guid == guid)
        {
            entry.bytes = data;
            return Ok(());
        }
        self.private
            .try_reserve(1)
            .map_err(|_| oom("SetPrivateData", "private-data table allocation failed"))?;
        self.private.push(PrivateData {
            view,
            guid,
            bytes: data,
        });
        Ok(())
    }

    pub fn get_private_data(&self, view: u32, guid: &[u8; 16]) -> Option<&[u8]> {
        self.private
            .iter()
            .find(|e| e.view == view && &e.guid == guid)
            .map(|e| e.bytes.as_slice())
    }

    pub fn free_private_data(&mut self, view: u32, guid: &[u8; 16]) -> bool {
        let before = self.private.len();
        self.private
            .retain(|e| !(e.view == view && &e.guid == guid));
        self.private.len() != before
    }
}

pub fn texture_index(kind: TextureKind, face: u32, level: u32, levels: u32) -> Option<usize> {
    if level >= levels {
        return None;
    }
    let index = match kind {
        TextureKind::Texture2D | TextureKind::Volume => level as usize,
        TextureKind::Cube => {
            if face >= 6 {
                return None;
            }
            (level * 6 + face) as usize
        }
    };
    Some(index)
}

pub fn face_count(kind: TextureKind) -> u32 {
    match kind {
        TextureKind::Cube => 6,
        _ => 1,
    }
}

pub struct TextureRequest {
    pub kind: TextureKind,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub levels: u32,
    pub usage: u32,
    pub format: u32,
    pub pool: u32,
    pub shared_handle: bool,
}

pub struct SurfaceRequest {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub pool: u32,
    pub usage: u32,
    pub lockable: bool,
    pub discard: bool,
    pub multisample_type: u32,
    pub multisample_quality: u32,
    pub shared_handle: bool,
}

pub struct BufferRequest {
    pub kind: BufferKind,
    pub size: u32,
    pub usage: u32,
    pub pool: u32,
    pub fvf: u32,
    pub format: u32,
    pub shared_handle: bool,
}

fn valid_pool(pool: u32) -> bool {
    matches!(
        pool,
        D3DPOOL_DEFAULT | D3DPOOL_MANAGED | D3DPOOL_SYSTEMMEM | D3DPOOL_SCRATCH
    )
}

fn check_dynamic(pool: u32, usage: u32) -> Result<(), RenderError> {
    if usage & D3DUSAGE_DYNAMIC == 0 {
        return Ok(());
    }
    match pool {
        D3DPOOL_DEFAULT => Ok(()),
        D3DPOOL_MANAGED => Err(invalid(
            "CreateResource",
            "D3DUSAGE_DYNAMIC cannot be combined with D3DPOOL_MANAGED",
        )),
        D3DPOOL_SYSTEMMEM => Err(invalid(
            "CreateResource",
            "D3DUSAGE_DYNAMIC is not valid for D3DPOOL_SYSTEMMEM resources",
        )),
        _ => Err(invalid(
            "CreateResource",
            "D3DUSAGE_DYNAMIC requires D3DPOOL_DEFAULT",
        )),
    }
}

fn check_target_pool(pool: u32, usage: u32) -> Result<(), RenderError> {
    let target = usage & (D3DUSAGE_RENDERTARGET | D3DUSAGE_DEPTHSTENCIL);
    if target != 0 && pool != D3DPOOL_DEFAULT {
        return Err(invalid(
            "CreateResource",
            "render-target and depth-stencil resources require D3DPOOL_DEFAULT",
        ));
    }
    if usage & D3DUSAGE_RENDERTARGET != 0 && usage & D3DUSAGE_DEPTHSTENCIL != 0 {
        return Err(invalid(
            "CreateResource",
            "a resource cannot be both a render target and a depth-stencil buffer",
        ));
    }
    Ok(())
}

fn check_usage_mask(op: &'static str, usage: u32, mask: u32) -> Result<(), RenderError> {
    let unknown = usage & !D3DUSAGE_KNOWN;
    if unknown != 0 {
        return Err(invalid(
            op,
            format!("unknown D3DUSAGE flag(s) {unknown:#x}"),
        ));
    }
    let illegal = usage & !mask;
    if illegal != 0 {
        return Err(invalid(
            op,
            format!("D3DUSAGE flag(s) {illegal:#x} are not valid for this resource"),
        ));
    }
    Ok(())
}

const D3DFVF_POSITION_MASK: u32 = 0x400E;
const D3DFVF_XYZW: u32 = 0x4002;
const D3DFVF_XYZRHW: u32 = 0x004;
const D3DFVF_NORMAL: u32 = 0x010;
const D3DFVF_PSIZE: u32 = 0x020;
const D3DFVF_DIFFUSE: u32 = 0x040;
const D3DFVF_SPECULAR: u32 = 0x080;
const D3DFVF_TEXCOUNT_MASK: u32 = 0x0000_0F00;
const D3DFVF_LASTBETA_UBYTE4: u32 = 0x0000_1000;
const D3DFVF_LASTBETA_D3DCOLOR: u32 = 0x0000_8000;

fn validate_fvf(fvf: u32) -> Result<(), RenderError> {
    if fvf == 0 {
        return Ok(());
    }
    let position = fvf & D3DFVF_POSITION_MASK;
    let xyz = position == 0x002;
    let xyzrhw = position == D3DFVF_XYZRHW;
    let xyzw = position == D3DFVF_XYZW;
    let blend = matches!(position, 0x006 | 0x008 | 0x00A | 0x00C | 0x00E);
    if !(xyz || xyzrhw || xyzw || blend) {
        return Err(invalid(
            "CreateVertexBuffer",
            "FVF has no valid position type",
        ));
    }
    if xyzrhw && fvf & D3DFVF_NORMAL != 0 {
        return Err(invalid(
            "CreateVertexBuffer",
            "D3DFVF_XYZRHW cannot be combined with D3DFVF_NORMAL",
        ));
    }
    let last_beta = fvf & (D3DFVF_LASTBETA_UBYTE4 | D3DFVF_LASTBETA_D3DCOLOR);
    if last_beta != 0 && !blend {
        return Err(invalid(
            "CreateVertexBuffer",
            "last-beta FVF flags require an XYZB position type",
        ));
    }
    let tex_count = (fvf & D3DFVF_TEXCOUNT_MASK) >> 8;
    if tex_count > 8 {
        return Err(invalid(
            "CreateVertexBuffer",
            "FVF declares too many texture coordinates",
        ));
    }
    let coord_bits = if tex_count == 0 {
        0
    } else {
        ((1u32 << (tex_count * 2)) - 1) << 16
    };
    let known = D3DFVF_POSITION_MASK
        | D3DFVF_NORMAL
        | D3DFVF_PSIZE
        | D3DFVF_DIFFUSE
        | D3DFVF_SPECULAR
        | D3DFVF_TEXCOUNT_MASK
        | D3DFVF_LASTBETA_UBYTE4
        | D3DFVF_LASTBETA_D3DCOLOR
        | coord_bits;
    let unknown = fvf & !known;
    if unknown != 0 {
        return Err(invalid(
            "CreateVertexBuffer",
            format!("FVF has unknown or unused bits {unknown:#x}"),
        ));
    }
    Ok(())
}

fn has_gpu_storage(pool: u32) -> bool {
    matches!(pool, D3DPOOL_DEFAULT | D3DPOOL_MANAGED)
}

fn allocates_gpu(gpu: &D3d9Gpu, pool: u32) -> bool {
    has_gpu_storage(pool) && !gpu.is_blocked()
}

fn texture_wgpu_usage(format: u32, usage: u32, autogen: bool) -> wgpu::TextureUsages {
    let depth_format = format::is_depth(format);
    let mut out = wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC;
    if !depth_format || format::sampleable_depth(format) {
        out |= wgpu::TextureUsages::TEXTURE_BINDING;
    }
    if usage & D3DUSAGE_RENDERTARGET != 0 || depth_format || autogen {
        out |= wgpu::TextureUsages::RENDER_ATTACHMENT;
    }
    out
}

fn texture_layer_count(kind: TextureKind, depth: u32) -> u32 {
    match kind {
        TextureKind::Cube => 6,
        TextureKind::Volume => depth,
        TextureKind::Texture2D => 1,
    }
}

fn surface_wgpu_usage(format: u32) -> wgpu::TextureUsages {
    let depth_format = format::is_depth(format);
    let mut out = wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC;
    if !depth_format {
        out |= wgpu::TextureUsages::TEXTURE_BINDING;
    }
    if !format::is_block(format) {
        out |= wgpu::TextureUsages::RENDER_ATTACHMENT;
    }
    out
}

pub fn create_buffer(
    gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    request: &BufferRequest,
) -> Result<(), RenderError> {
    if resource.initialized() {
        return Err(invalid(
            "CreateVertexBuffer",
            "resource is already initialized",
        ));
    }
    if request.shared_handle {
        return Err(unsupported(
            "CreateResource",
            "shared handles are not implemented",
        ));
    }
    if request.size == 0 {
        return Err(invalid("CreateResource", "zero-length buffers are invalid"));
    }
    if !valid_pool(request.pool) {
        return Err(invalid(
            "CreateResource",
            format!("invalid D3DPOOL {}", request.pool),
        ));
    }
    check_usage_mask("CreateResource", request.usage, BUFFER_USAGE_MASK)?;
    check_dynamic(request.pool, request.usage)?;
    match request.kind {
        BufferKind::Vertex => {
            if request.pool == D3DPOOL_SCRATCH {
                return Err(invalid(
                    "CreateVertexBuffer",
                    "D3DPOOL_SCRATCH is invalid for vertex buffers",
                ));
            }
            if request.format != format::D3DFMT_VERTEXDATA {
                return Err(invalid(
                    "CreateVertexBuffer",
                    "invalid vertex buffer format",
                ));
            }
            validate_fvf(request.fvf)?;
        }
        BufferKind::Index => {
            if request.fvf != 0 {
                return Err(invalid(
                    "CreateIndexBuffer",
                    "index buffers do not have an FVF",
                ));
            }
            if request.format != format::D3DFMT_INDEX16 && request.format != format::D3DFMT_INDEX32
            {
                return Err(invalid("CreateIndexBuffer", "invalid index buffer format"));
            }
        }
    }
    let bytes = alloc_bytes(request.size)?;
    let initial_dirty = request.pool == D3DPOOL_MANAGED;
    resource.gpu = Some(gpu.clone());
    resource.state = Some(ResourceState::Buffer(BufferState {
        kind: request.kind,
        size: request.size,
        usage: request.usage,
        pool: request.pool,
        fvf: request.fvf,
        format: request.format,
        uploaded: Arc::new(Mutex::new(Arc::new(alloc_bytes(request.size)?))),
        bytes,
        lock_count: Arc::new(AtomicU32::new(0)),
        writable: Vec::new(),
        writable_dirty: false,
        dirty_lo: 0,
        dirty_hi: if initial_dirty { request.size } else { 0 },
        generation: 0,
    }));
    Ok(())
}

fn texture_lockable(kind: TextureKind, pool: u32, usage: u32, format: u32, depth: bool) -> bool {
    if depth {
        return format::depth_lockable(format);
    }
    match pool {
        D3DPOOL_SYSTEMMEM | D3DPOOL_MANAGED | D3DPOOL_SCRATCH => true,
        D3DPOOL_DEFAULT => {
            usage & D3DUSAGE_DYNAMIC != 0 || format::is_block(format) || kind == TextureKind::Volume
        }
        _ => false,
    }
}

pub fn create_texture(
    gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    request: &TextureRequest,
) -> Result<(), RenderError> {
    if resource.initialized() {
        return Err(invalid("CreateTexture", "resource is already initialized"));
    }
    if request.shared_handle {
        return Err(unsupported(
            "CreateTexture",
            "shared handles are not implemented",
        ));
    }
    if !valid_pool(request.pool) {
        return Err(invalid(
            "CreateTexture",
            format!("invalid D3DPOOL {}", request.pool),
        ));
    }
    if request.width == 0 || request.height == 0 || request.depth == 0 {
        return Err(invalid("CreateTexture", "zero texture dimension"));
    }
    check_usage_mask("CreateTexture", request.usage, TEXTURE_USAGE_MASK)?;
    check_dynamic(request.pool, request.usage)?;
    check_target_pool(request.pool, request.usage)?;
    if !format::is_supported(request.format) {
        return Err(unsupported(
            "CreateTexture",
            format!("unsupported texture D3DFORMAT {}", request.format),
        ));
    }
    let depth_format = format::is_depth(request.format);
    let depth_stencil = request.usage & D3DUSAGE_DEPTHSTENCIL != 0;
    if depth_format != depth_stencil {
        return Err(invalid(
            "CreateTexture",
            "D3DUSAGE_DEPTHSTENCIL and a depth D3DFORMAT must agree",
        ));
    }
    match request.kind {
        TextureKind::Texture2D => {
            if request.depth != 1 {
                return Err(invalid("CreateTexture", "2D textures have depth 1"));
            }
        }
        TextureKind::Cube => {
            if request.depth != 1 || request.width != request.height {
                return Err(invalid(
                    "CreateTexture",
                    "cube textures need equal width and height and depth 1",
                ));
            }
            if depth_format || depth_stencil {
                return Err(invalid(
                    "CreateTexture",
                    "cube textures cannot be depth-stencil",
                ));
            }
        }
        TextureKind::Volume => {
            if depth_format || depth_stencil {
                return Err(invalid(
                    "CreateTexture",
                    "volume textures cannot be depth-stencil",
                ));
            }
            if request.usage & D3DUSAGE_RENDERTARGET != 0 {
                return Err(invalid(
                    "CreateTexture",
                    "volume textures cannot be render targets",
                ));
            }
            if request.usage & D3DUSAGE_AUTOGENMIPMAP != 0 {
                return Err(invalid(
                    "CreateTexture",
                    "volume textures cannot use automatic mipmaps",
                ));
            }
        }
    }
    if depth_stencil && request.kind != TextureKind::Texture2D {
        return Err(invalid(
            "CreateTexture",
            "depth-stencil textures must be 2D",
        ));
    }
    if depth_stencil && request.pool != D3DPOOL_DEFAULT {
        return Err(invalid(
            "CreateTexture",
            "depth-stencil textures require D3DPOOL_DEFAULT",
        ));
    }
    if depth_stencil && request.levels > 1 {
        return Err(invalid(
            "CreateTexture",
            "depth-stencil textures have one level",
        ));
    }
    let autogen_requested = request.usage & D3DUSAGE_AUTOGENMIPMAP != 0;
    if autogen_requested && request.pool == D3DPOOL_SYSTEMMEM {
        return Err(invalid(
            "CreateTexture",
            "D3DUSAGE_AUTOGENMIPMAP is invalid for D3DPOOL_SYSTEMMEM textures",
        ));
    }
    let device_pool = has_gpu_storage(request.pool);
    let autogen_format_ok = format::autogen_capable(request.format);
    let mut autogen = autogen_requested && autogen_format_ok && device_pool;
    let mut autogen_filter = AUTOGEN_FILTER_LINEAR;
    if autogen {
        let wgpu_format = format::d3d_to_wgpu(request.format)
            .ok_or_else(|| unsupported("CreateTexture", "unsupported texture format"))?;
        let features = wgpu_format.guaranteed_format_features(gpu.features());
        if !features
            .allowed_usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            autogen = false;
        } else if !features
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
        {
            autogen_filter = AUTOGEN_FILTER_POINT;
        }
    }
    let mut visible_levels = if autogen_requested {
        1
    } else {
        format::level_count(request.width, request.height, request.depth, request.levels)?
    };
    if depth_stencil {
        visible_levels = 1;
    }
    let full_levels = format::full_level_count(request.width, request.height, request.depth);
    let gpu_levels = if autogen {
        full_levels.max(1)
    } else {
        visible_levels
    };
    if device_pool {
        format::level_fits_device(
            request.format,
            request.width,
            request.height,
            request.depth,
            &gpu.device.limits(),
            request.kind == TextureKind::Volume,
        )?;
        if let Some(feature) = format::required_feature(request.format) {
            if !gpu.features().contains(feature) {
                return Err(unsupported(
                    "CreateTexture",
                    format!(
                        "D3DFORMAT {} needs wgpu feature {feature:?}",
                        request.format
                    ),
                ));
            }
        }
        if format::is_block(request.format)
            && request.kind == TextureKind::Volume
            && !gpu
                .features()
                .contains(wgpu::Features::TEXTURE_COMPRESSION_BC_SLICED_3D)
            && !format::needs_bc_decode(request.format, gpu.features())
        {
            return Err(unsupported(
                "CreateTexture",
                "compressed volume textures need TEXTURE_COMPRESSION_BC_SLICED_3D",
            ));
        }
        let wgpu_format = format::device_format(request.format, gpu.features())
            .ok_or_else(|| unsupported("CreateTexture", "unsupported texture format"))?;
        let features = wgpu_format.guaranteed_format_features(gpu.features());
        if request.usage & D3DUSAGE_RENDERTARGET != 0 || depth_stencil || autogen {
            if !features
                .allowed_usages
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
            {
                return Err(unsupported(
                    "CreateTexture",
                    "texture format cannot be used as a render attachment",
                ));
            }
        }
    }
    let wgpu_format = format::device_format(request.format, gpu.features())
        .ok_or_else(|| unsupported("CreateTexture", "unsupported texture format"))?;
    let texture = if allocates_gpu(gpu, request.pool) {
        let layers = texture_layer_count(request.kind, request.depth);
        Some(gpu.create_texture(
            request.width,
            request.height,
            layers,
            gpu_levels,
            wgpu_format,
            if request.kind == TextureKind::Volume {
                wgpu::TextureDimension::D3
            } else {
                wgpu::TextureDimension::D2
            },
            texture_wgpu_usage(request.format, request.usage, autogen),
        )?)
    } else {
        None
    };
    let per_face = if request.kind == TextureKind::Cube {
        6
    } else {
        1
    };
    let total_levels = (visible_levels as usize)
        .checked_mul(per_face as usize)
        .ok_or_else(|| invalid("CreateTexture", "texture subresource count overflow"))?;
    let mut subresources = Vec::new();
    subresources.try_reserve_exact(total_levels).map_err(|_| {
        oom(
            "CreateTexture",
            "texture subresource table allocation failed",
        )
    })?;
    let initial_upload = request.pool == D3DPOOL_MANAGED
        || (device_pool && format::needs_initial_upload(request.format));
    for level in 0..visible_levels {
        let (w, h, d) =
            format::level_dimensions(request.width, request.height, request.depth, level);
        let size = if request.kind == TextureKind::Volume {
            format::volume_size(request.format, w, h, d).map(|(_, _, total)| total)
        } else {
            format::level_size(request.format, w, h)
        }
        .ok_or_else(|| invalid("CreateTexture", "texture level layout overflow"))?;
        for _ in 0..per_face {
            let mut state = LevelState::new(alloc_bytes(size)?, w, h, d);
            if initial_upload {
                state.mark_managed_initial();
            }
            subresources.push(state);
        }
    }
    let requested_usage = if autogen_requested && !autogen {
        request.usage & !D3DUSAGE_AUTOGENMIPMAP
    } else {
        request.usage
    };
    resource.gpu = Some(gpu.clone());
    resource.state = Some(ResourceState::Texture(TextureState {
        kind: request.kind,
        format: request.format,
        width: request.width,
        height: request.height,
        depth: request.depth,
        levels: visible_levels,
        gpu_levels,
        usage: requested_usage,
        pool: request.pool,
        lockable: texture_lockable(
            request.kind,
            request.pool,
            request.usage,
            request.format,
            depth_format,
        ),
        autogen,
        autogen_filter,
        subresources,
        gpu: texture,
    }));
    Ok(())
}

pub fn create_surface(
    gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    request: &SurfaceRequest,
) -> Result<(), RenderError> {
    if resource.initialized() {
        return Err(invalid("CreateSurface", "resource is already initialized"));
    }
    if request.shared_handle {
        return Err(unsupported(
            "CreateSurface",
            "shared handles are not implemented",
        ));
    }
    if !valid_pool(request.pool) {
        return Err(invalid(
            "CreateSurface",
            format!("invalid D3DPOOL {}", request.pool),
        ));
    }
    if request.width == 0 || request.height == 0 {
        return Err(invalid("CreateSurface", "zero surface dimension"));
    }
    check_usage_mask("CreateSurface", request.usage, SURFACE_USAGE_MASK)?;
    check_dynamic(request.pool, request.usage)?;
    check_target_pool(request.pool, request.usage)?;
    if !format::is_supported(request.format) {
        return Err(unsupported(
            "CreateSurface",
            format!("unsupported surface D3DFORMAT {}", request.format),
        ));
    }
    if request.multisample_type != 0 {
        return Err(unsupported(
            "CreateSurface",
            format!(
                "D3DMULTISAMPLE_TYPE {} is not implemented",
                request.multisample_type
            ),
        ));
    }
    if request.multisample_quality != 0 {
        return Err(invalid(
            "CreateSurface",
            "multisample quality must be zero without multisampling",
        ));
    }
    let depth_format = format::is_depth(request.format);
    let render_target = request.usage & D3DUSAGE_RENDERTARGET != 0;
    let depth_stencil = request.usage & D3DUSAGE_DEPTHSTENCIL != 0;
    if render_target && (depth_format || depth_stencil) {
        return Err(invalid(
            "CreateSurface",
            "render targets need a color D3DFORMAT",
        ));
    }
    if depth_stencil && !depth_format {
        return Err(invalid(
            "CreateSurface",
            "depth-stencil surfaces need a depth D3DFORMAT",
        ));
    }
    if !render_target && !depth_stencil && request.pool == D3DPOOL_MANAGED {
        return Err(invalid(
            "CreateSurface",
            "offscreen plain surfaces cannot be D3DPOOL_MANAGED",
        ));
    }
    let lockable = if render_target {
        request.lockable
    } else if depth_stencil {
        format::depth_lockable(request.format)
    } else {
        true
    };
    let size = format::level_size(request.format, request.width, request.height)
        .ok_or_else(|| invalid("CreateSurface", "surface layout overflow"))?;
    let device_pool = has_gpu_storage(request.pool);
    let mut level = LevelState::new(alloc_bytes(size)?, request.width, request.height, 1);
    if request.pool == D3DPOOL_MANAGED
        || (device_pool && format::needs_initial_upload(request.format))
    {
        level.mark_managed_initial();
    }
    if device_pool {
        format::level_fits_device(
            request.format,
            request.width,
            request.height,
            1,
            &gpu.limits(),
            false,
        )?;
    }
    let texture = if allocates_gpu(gpu, request.pool) {
        if format::needs_bc_decode(request.format, gpu.features()) {
            return Err(unsupported(
                "CreateSurface",
                "compressed surfaces need TEXTURE_COMPRESSION_BC",
            ));
        }
        let wgpu_format = format::d3d_to_wgpu(request.format)
            .ok_or_else(|| unsupported("CreateSurface", "unsupported surface format"))?;
        Some(gpu.create_texture(
            request.width,
            request.height,
            1,
            1,
            wgpu_format,
            wgpu::TextureDimension::D2,
            surface_wgpu_usage(request.format),
        )?)
    } else {
        None
    };
    resource.gpu = Some(gpu.clone());
    resource.state = Some(ResourceState::Surface(SurfaceState {
        format: request.format,
        width: request.width,
        height: request.height,
        usage: request.usage,
        pool: request.pool,
        multisample_type: request.multisample_type,
        multisample_quality: request.multisample_quality,
        lockable,
        depth: depth_format,
        discard: request.discard,
        level,
        gpu: texture,
    }));
    Ok(())
}

pub fn upload_buffer(resource: &mut D3d9Resource) -> Result<(), RenderError> {
    let Some(ResourceState::Buffer(b)) = resource.state.as_mut() else {
        return Err(invalid("PreLoad", "resource is not a buffer"));
    };
    if b.lock_count.load(Ordering::Acquire) != 0 {
        return Err(invalid("PreLoad", "buffer is locked"));
    }
    if b.dirty_hi <= b.dirty_lo {
        return Ok(());
    }
    let hi = b.dirty_hi.min(b.size);
    let lo = b.dirty_lo.min(hi);
    if hi as usize > b.bytes.len() {
        return Err(invalid("PreLoad", "buffer dirty range is outside storage"));
    }
    if super::trace_enabled() {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        if COUNT.fetch_add(1, Ordering::Relaxed) < 128 {
            eprintln!(
                "[d3d9-trace] buffer_upload resource={:p} pool={} format={} size={} range={}-{} sample={:?}",
                b as *const BufferState,
                b.pool,
                b.format,
                b.size,
                lo,
                hi,
                &b.bytes[lo as usize..(hi as usize).min(lo as usize + 64)]
            );
        }
    }
    let align = wgpu::COPY_BUFFER_ALIGNMENT as u32;
    let copy_lo = (lo - lo % align) as usize;
    let copy_hi = (hi.next_multiple_of(align) as usize).min(b.bytes.len());
    {
        let mut uploaded = b.uploaded.lock().unwrap();
        super::stats::record(|s| {
            s.buffer_uploads += 1;
            s.buffer_upload_bytes += (copy_hi - copy_lo) as u64;
            let strong = Arc::strong_count(&uploaded);
            let weak_detach = strong == 1 && Arc::weak_count(&uploaded) != 0;
            s.buffer_snapshot_copies += u64::from(strong > 1);
            s.buffer_weak_detaches += u64::from(weak_detach);
            if matches!(b.format, 101 | 102) {
                s.index_buffer_uploads += 1;
                s.index_buffer_snapshot_copies += u64::from(strong > 1);
                s.index_buffer_weak_detaches += u64::from(weak_detach);
            }
        });
        Arc::make_mut(&mut uploaded)[copy_lo..copy_hi].copy_from_slice(&b.bytes[copy_lo..copy_hi]);
    }
    b.dirty_lo = 0;
    b.dirty_hi = 0;
    Ok(())
}

pub fn level_descriptor(
    resource: &D3d9Resource,
    face: u32,
    level: u32,
) -> Result<(u32, u32, u32, u32), RenderError> {
    let levels = match resource.state.as_ref() {
        Some(ResourceState::Texture(t)) => t.levels,
        Some(ResourceState::Surface(_)) => 1,
        _ => return Err(invalid("GetDesc", "resource is not a texture")),
    };
    let l = resource.level(face, level)?;
    Ok((l.width, l.height, l.depth, levels))
}

fn upload_texels(
    gpu: &D3d9Gpu,
    fmt: u32,
    width: u32,
    height: u32,
    depth: u32,
    src: &[u8],
) -> Result<(u32, Vec<u8>), RenderError> {
    if format::needs_bc_decode(fmt, gpu.features()) {
        let decoded = format::decode_bc_level(fmt, width, height, depth, src)?;
        Ok((format::D3DFMT_A8B8G8R8, decoded))
    } else {
        Ok((fmt, format::upload_bytes(fmt, width, height, depth, src)?))
    }
}

pub fn upload_level(
    gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
) -> Result<(), RenderError> {
    if matches!(resource.state, Some(ResourceState::Surface(_))) {
        let ResourceState::Surface(s) = resource.state.as_mut().expect("surface state") else {
            unreachable!()
        };
        if s.level.lock_count.load(Ordering::Acquire) != 0 {
            return Err(invalid("PreLoad", "surface is locked"));
        }
        let Some(texture) = s.gpu.as_ref() else {
            return Ok(());
        };
        if s.level.dirty {
            let depth_format = format::is_depth(s.format);
            if depth_format && s.format == format::D3DFMT_D32F_LOCKABLE {
                let floats = bytes_to_f32(&s.level.bytes)?;
                gpu.write_depth32(texture, s.width, s.height, &floats)?;
            } else {
                let converted =
                    format::upload_bytes(s.format, s.width, s.height, 1, &s.level.bytes)?;
                gpu.write_texture(
                    texture, s.format, 0, s.width, s.height, 1, 0, false, &converted,
                )?;
            }
            s.level.dirty = false;
            s.level.gpu_generation = s.level.generation;
            s.level.gpu_newer.store(false, Ordering::Release);
        }
        return Ok(());
    }
    let Some(ResourceState::Texture(t)) = resource.state.as_mut() else {
        return Err(invalid("PreLoad", "resource is not a texture"));
    };
    let index = texture_index(t.kind, face, level, t.levels)
        .ok_or_else(|| invalid("PreLoad", "texture subresource out of range"))?;
    if t.subresources[index].lock_count.load(Ordering::Acquire) != 0 {
        return Err(invalid("PreLoad", "texture is locked"));
    }
    let Some(texture) = t.gpu.as_ref() else {
        return Ok(());
    };
    let is_3d = t.kind == TextureKind::Volume;
    let layer = if t.kind == TextureKind::Cube { face } else { 0 };
    let (format, width, height, depth, dirty) = {
        let l = &t.subresources[index];
        (t.format, l.width, l.height, l.depth, l.dirty)
    };
    if dirty {
        if super::trace_enabled() && level == 0 {
            static COUNT: AtomicU32 = AtomicU32::new(0);
            if COUNT.fetch_add(1, Ordering::Relaxed) < 128 {
                let bytes = &t.subresources[index].bytes;
                eprintln!(
                    "[d3d9-trace] upload resource={:p} pool={} format={} size={}x{}x{} nonzero={} sample={:?}",
                    t as *const TextureState,
                    t.pool,
                    format,
                    width,
                    height,
                    depth,
                    bytes.iter().filter(|&&b| b != 0).count(),
                    &bytes[..bytes.len().min(32)]
                );
            }
        }
        if format::is_depth(format) && format == format::D3DFMT_D32F_LOCKABLE {
            let bytes = {
                let l = &t.subresources[index];
                bytes_to_f32(&l.bytes)?
            };
            gpu.write_depth32(texture, width, height, &bytes)?;
        } else {
            let (target, converted) = {
                let l = &t.subresources[index];
                upload_texels(gpu, format, width, height, depth, &l.bytes)?
            };
            gpu.write_texture(
                texture, target, level, width, height, depth, layer, is_3d, &converted,
            )?;
        }
        let l = &mut t.subresources[index];
        l.dirty = false;
        l.gpu_generation = l.generation;
        l.gpu_newer.store(false, Ordering::Release);
    }
    Ok(())
}

pub fn generate_mips(gpu: &D3d9Gpu, resource: &mut D3d9Resource) -> Result<(), RenderError> {
    let Some(ResourceState::Texture(t)) = resource.state.as_mut() else {
        return Err(invalid("GenerateMipSubLevels", "resource is not a texture"));
    };
    if !t.autogen || t.gpu_levels < 2 {
        return Ok(());
    }
    let texture = t
        .gpu
        .clone()
        .ok_or_else(|| unsupported("GenerateMipSubLevels", "texture has no GPU storage"))?;
    let kind = t.kind;
    let format = t.format;
    let is_3d = kind == TextureKind::Volume;
    for index in 0..t.subresources.len() {
        if !t.subresources[index].dirty {
            continue;
        }
        if t.subresources[index].lock_count.load(Ordering::Acquire) != 0 {
            return Err(invalid("GenerateMipSubLevels", "texture is locked"));
        }
        let (width, height, depth) = {
            let l = &t.subresources[index];
            (l.width, l.height, l.depth)
        };
        let layer = if kind == TextureKind::Cube {
            (index % 6) as u32
        } else {
            0
        };
        let level = if kind == TextureKind::Cube {
            (index / 6) as u32
        } else {
            index as u32
        };
        if format::is_depth(format) && format == format::D3DFMT_D32F_LOCKABLE {
            let floats = {
                let l = &t.subresources[index];
                bytes_to_f32(&l.bytes)?
            };
            gpu.write_depth32(&texture, width, height, &floats)?;
        } else {
            let converted = {
                let l = &t.subresources[index];
                format::upload_bytes(format, width, height, depth, &l.bytes)?
            };
            gpu.write_texture(
                &texture, format, level, width, height, depth, layer, is_3d, &converted,
            )?;
        }
        let l = &mut t.subresources[index];
        l.dirty = false;
        l.gpu_generation = l.generation;
    }
    let linear = t.autogen_filter == AUTOGEN_FILTER_LINEAR;
    let wgpu_format = format::d3d_to_wgpu(format)
        .ok_or_else(|| unsupported("GenerateMipSubLevels", "unsupported texture format"))?;
    let layers = if kind == TextureKind::Cube { 6 } else { 1 };
    gpu.generate_mips(
        &texture,
        wgpu_format,
        t.gpu_levels,
        t.width,
        t.height,
        layers,
        linear,
    )?;
    for l in t.subresources.iter_mut() {
        if l.lock_count.load(Ordering::Acquire) == 0 {
            l.dirty = false;
            l.gpu_generation = l.generation;
        }
    }
    Ok(())
}

enum PreloadPlan {
    Buffer,
    Surface,
    Texture {
        faces: u32,
        levels: u32,
        autogen: bool,
    },
}

pub fn preload(gpu: &D3d9Gpu, resource: &mut D3d9Resource) -> Result<(), RenderError> {
    let plan = match resource.state.as_ref() {
        Some(ResourceState::Buffer(_)) => PreloadPlan::Buffer,
        Some(ResourceState::Surface(_)) => PreloadPlan::Surface,
        Some(ResourceState::Texture(t)) => PreloadPlan::Texture {
            faces: face_count(t.kind),
            levels: t.levels,
            autogen: t.autogen,
        },
        None => return Err(invalid("PreLoad", "resource is not initialized")),
    };
    match plan {
        PreloadPlan::Buffer => upload_buffer(resource),
        PreloadPlan::Surface => upload_level(gpu, resource, 0, 0),
        PreloadPlan::Texture {
            faces,
            levels,
            autogen,
        } => {
            for face in 0..faces {
                for level in 0..levels {
                    upload_level(gpu, resource, face, level)?;
                }
            }
            if autogen {
                generate_mips(gpu, resource)?;
            }
            Ok(())
        }
    }
}

#[derive(Default)]
pub struct StagedStorage {
    pub texture: Option<wgpu::Texture>,
}

pub fn stage_restore(gpu: &D3d9Gpu, resource: &D3d9Resource) -> Result<StagedStorage, RenderError> {
    match resource.state.as_ref() {
        Some(ResourceState::Buffer(_)) => Ok(StagedStorage::default()),
        Some(ResourceState::Texture(t)) => stage_texture(gpu, t),
        Some(ResourceState::Surface(s)) => stage_surface(gpu, s),
        None => Err(invalid("Reset", "resource is not initialized")),
    }
}

pub fn install_staged(resource: &mut D3d9Resource, staged: StagedStorage, gpu: &D3d9Gpu) {
    match resource.state.as_mut() {
        Some(ResourceState::Texture(t)) => t.gpu = staged.texture,
        Some(ResourceState::Surface(s)) => s.gpu = staged.texture,
        _ => {}
    }
    resource.gpu = Some(gpu.clone());
}

fn stage_texture(gpu: &D3d9Gpu, t: &TextureState) -> Result<StagedStorage, RenderError> {
    if !has_gpu_storage(t.pool) {
        return Ok(StagedStorage::default());
    }
    for level in &t.subresources {
        if level.gpu_newer.load(Ordering::Acquire) {
            return Err(unsupported(
                "Reset",
                "texture has GPU-only contents that cannot be recovered",
            ));
        }
    }
    let wgpu_format = format::device_format(t.format, gpu.features())
        .ok_or_else(|| unsupported("Reset", "unsupported texture format"))?;
    let layers = texture_layer_count(t.kind, t.depth);
    let texture = gpu.create_texture(
        t.width,
        t.height,
        layers,
        t.gpu_levels,
        wgpu_format,
        if t.kind == TextureKind::Volume {
            wgpu::TextureDimension::D3
        } else {
            wgpu::TextureDimension::D2
        },
        texture_wgpu_usage(t.format, t.usage, t.autogen),
    )?;
    upload_staged_texture(gpu, &texture, t)?;
    if t.autogen && t.gpu_levels >= 2 {
        gpu.generate_mips(
            &texture,
            wgpu_format,
            t.gpu_levels,
            t.width,
            t.height,
            layers,
            t.autogen_filter == AUTOGEN_FILTER_LINEAR,
        )?;
    }
    Ok(StagedStorage {
        texture: Some(texture),
    })
}

fn stage_surface(gpu: &D3d9Gpu, s: &SurfaceState) -> Result<StagedStorage, RenderError> {
    if !has_gpu_storage(s.pool) {
        return Ok(StagedStorage::default());
    }
    if s.level.gpu_newer.load(Ordering::Acquire) {
        return Err(unsupported(
            "Reset",
            "surface has GPU-only contents that cannot be recovered",
        ));
    }
    let wgpu_format = format::d3d_to_wgpu(s.format)
        .ok_or_else(|| unsupported("Reset", "unsupported surface format"))?;
    let texture = gpu.create_texture(
        s.width,
        s.height,
        1,
        1,
        wgpu_format,
        wgpu::TextureDimension::D2,
        surface_wgpu_usage(s.format),
    )?;
    if format::is_depth(s.format) && s.format == format::D3DFMT_D32F_LOCKABLE {
        let floats = bytes_to_f32(&s.level.bytes)?;
        gpu.write_depth32(&texture, s.width, s.height, &floats)?;
    } else {
        let converted = format::upload_bytes(s.format, s.width, s.height, 1, &s.level.bytes)?;
        gpu.write_texture(
            &texture, s.format, 0, s.width, s.height, 1, 0, false, &converted,
        )?;
    }
    Ok(StagedStorage {
        texture: Some(texture),
    })
}

fn upload_staged_texture(
    gpu: &D3d9Gpu,
    texture: &wgpu::Texture,
    t: &TextureState,
) -> Result<(), RenderError> {
    let is_3d = t.kind == TextureKind::Volume;
    for level in 0..t.levels {
        for face in 0..face_count(t.kind) {
            let index = texture_index(t.kind, face, level, t.levels)
                .ok_or_else(|| invalid("Reset", "texture subresource out of range"))?;
            let l = &t.subresources[index];
            let layer = if t.kind == TextureKind::Cube { face } else { 0 };
            if format::is_depth(t.format) && t.format == format::D3DFMT_D32F_LOCKABLE {
                let floats = bytes_to_f32(&l.bytes)?;
                gpu.write_depth32(texture, l.width, l.height, &floats)?;
            } else {
                let (target, converted) =
                    upload_texels(gpu, t.format, l.width, l.height, l.depth, &l.bytes)?;
                gpu.write_texture(
                    texture, target, level, l.width, l.height, l.depth, layer, is_3d, &converted,
                )?;
            }
        }
    }
    Ok(())
}

pub fn readback_level_explicit(
    gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
) -> Result<(), RenderError> {
    if matches!(resource.state, Some(ResourceState::Surface(_))) {
        let ResourceState::Surface(s) = resource.state.as_mut().expect("surface state") else {
            unreachable!()
        };
        let Some(texture) = s.gpu.as_ref() else {
            return Err(unsupported("ReadbackTexture", "surface has no GPU storage"));
        };
        let physical = format::physical_total(s.format, s.width, s.height, 1)
            .ok_or_else(|| invalid("ReadbackTexture", "surface layout overflow"))?;
        let mut raw = alloc_bytes(physical)?;
        gpu.read_texture(
            texture, s.format, 0, s.width, s.height, 1, 0, false, &mut raw,
        )?;
        s.level.bytes = format::readback_bytes(s.format, s.width, s.height, 1, &raw)?;
        s.level.gpu_newer.store(false, Ordering::Release);
        return Ok(());
    }
    let Some(ResourceState::Texture(t)) = resource.state.as_mut() else {
        return Err(invalid("LockRect", "resource is not a texture"));
    };
    let index = texture_index(t.kind, face, level, t.levels)
        .ok_or_else(|| invalid("LockRect", "texture subresource out of range"))?;
    let Some(texture) = t.gpu.as_ref() else {
        return Err(unsupported("ReadbackTexture", "texture has no GPU storage"));
    };
    let is_3d = t.kind == TextureKind::Volume;
    let layer = if t.kind == TextureKind::Cube { face } else { 0 };
    let (format, width, height, depth) = {
        let l = &t.subresources[index];
        (t.format, l.width, l.height, l.depth)
    };
    let physical = format::physical_total(format, width, height, depth)
        .ok_or_else(|| invalid("ReadbackTexture", "texture layout overflow"))?;
    let mut raw = alloc_bytes(physical)?;
    gpu.read_texture(
        texture, format, level, width, height, depth, layer, is_3d, &mut raw,
    )?;
    t.subresources[index].bytes = format::readback_bytes(format, width, height, depth, &raw)?;
    t.subresources[index]
        .gpu_newer
        .store(false, Ordering::Release);
    Ok(())
}

pub fn readback_level(
    gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
) -> Result<(), RenderError> {
    let gpu_newer = match resource.state.as_ref() {
        Some(ResourceState::Surface(s)) => s.level.gpu_newer.load(Ordering::Acquire),
        Some(ResourceState::Texture(t)) => texture_index(t.kind, face, level, t.levels)
            .and_then(|index| t.subresources.get(index))
            .map(|l| l.gpu_newer.load(Ordering::Acquire))
            .unwrap_or(false),
        _ => false,
    };
    if !gpu_newer {
        return Ok(());
    }
    readback_level_explicit(gpu, resource, face, level)
}

pub fn level_generation(
    resource: &D3d9Resource,
    face: u32,
    level: u32,
) -> Result<u64, RenderError> {
    Ok(resource.level(face, level)?.generation)
}

pub fn is_default_pool(resource: &D3d9Resource) -> bool {
    match resource.state.as_ref() {
        Some(ResourceState::Buffer(b)) => b.pool == D3DPOOL_DEFAULT,
        Some(ResourceState::Texture(t)) => t.pool == D3DPOOL_DEFAULT,
        Some(ResourceState::Surface(s)) => s.pool == D3DPOOL_DEFAULT,
        None => false,
    }
}

pub struct LockRequest {
    pub face: u32,
    pub level: u32,
    pub volume: bool,
    pub full_region: bool,
    pub left: i64,
    pub top: i64,
    pub front: i64,
    pub right: i64,
    pub bottom: i64,
    pub back: i64,
    pub flags: u32,
}

pub struct LockResult {
    pub pitch: u32,
    pub slice_pitch: u32,
    pub offset: u32,
    pub size: u32,
    pub total: u32,
}

struct ResolvedLock {
    pitch: u32,
    slice_pitch: u32,
    offset: u32,
    size: u32,
    total: u32,
    region: Region,
}

fn validate_lock_flags(resource: &D3d9Resource, flags: u32) -> Result<(), RenderError> {
    let unknown = flags & !D3DLOCK_KNOWN;
    if unknown != 0 {
        return Err(invalid(
            "LockRect",
            format!("unknown D3DLOCK flag(s) {unknown:#x}"),
        ));
    }
    let is_surface = matches!(resource.state.as_ref(), Some(ResourceState::Surface(_)));
    if flags & D3DLOCK_DONOTWAIT != 0 && !is_surface {
        return Err(invalid(
            "LockRect",
            "D3DLOCK_DONOTWAIT is only valid for surfaces",
        ));
    }
    if flags & D3DLOCK_NOOVERWRITE != 0 {
        return Err(invalid(
            "LockRect",
            "D3DLOCK_NOOVERWRITE is invalid for textures and surfaces",
        ));
    }
    if flags & D3DLOCK_READONLY != 0 && flags & D3DLOCK_DISCARD != 0 {
        return Err(invalid(
            "LockRect",
            "D3DLOCK_READONLY cannot be combined with D3DLOCK_DISCARD",
        ));
    }
    let (dynamic, writeonly) = match resource.state.as_ref() {
        Some(ResourceState::Buffer(b)) => (
            b.usage & D3DUSAGE_DYNAMIC != 0,
            b.usage & D3DUSAGE_WRITEONLY != 0,
        ),
        Some(ResourceState::Texture(t)) => (t.usage & D3DUSAGE_DYNAMIC != 0, false),
        Some(ResourceState::Surface(s)) => (s.usage & D3DUSAGE_DYNAMIC != 0, false),
        None => (false, false),
    };
    if flags & D3DLOCK_DYNAMIC_ONLY != 0 && !dynamic {
        return Err(invalid(
            "LockRect",
            "D3DLOCK_DISCARD and D3DLOCK_NOOVERWRITE require D3DUSAGE_DYNAMIC",
        ));
    }
    if writeonly && flags & D3DLOCK_READONLY != 0 {
        return Err(invalid(
            "LockRect",
            "D3DUSAGE_WRITEONLY buffers cannot be locked D3DLOCK_READONLY",
        ));
    }
    Ok(())
}

fn resource_is_volume(resource: &D3d9Resource) -> bool {
    matches!(
        resource.state.as_ref(),
        Some(ResourceState::Texture(t)) if t.kind == TextureKind::Volume
    )
}

fn resource_format(resource: &D3d9Resource) -> u32 {
    match resource.state.as_ref() {
        Some(ResourceState::Texture(t)) => t.format,
        Some(ResourceState::Surface(s)) => s.format,
        Some(ResourceState::Buffer(_)) => 0,
        None => 0,
    }
}

fn resolve_lock(
    level: &LevelState,
    format: u32,
    volume: bool,
    request: &LockRequest,
) -> Result<ResolvedLock, RenderError> {
    let op = if volume { "LockBox" } else { "LockRect" };
    let block = format::is_block(format);
    let (row, slice) = if volume {
        let (row, slice, _) = format::volume_size(format, level.width, level.height, level.depth)
            .ok_or_else(|| invalid(op, "unsupported volume format"))?;
        (row, slice)
    } else {
        let row = format::row_pitch(format, level.width)
            .ok_or_else(|| invalid(op, "unsupported texture format"))?;
        let rows = if block {
            format::block_rows(level.height)
        } else {
            level.height
        };
        let slice = row
            .checked_mul(rows)
            .ok_or_else(|| invalid(op, "texture layout overflow"))?;
        (row, slice)
    };
    let total = level.bytes.len() as u32;
    if request.full_region {
        return Ok(ResolvedLock {
            pitch: row,
            slice_pitch: slice,
            offset: 0,
            size: total,
            total,
            region: Region::Full,
        });
    }
    if request.left < 0
        || request.top < 0
        || request.right <= request.left
        || request.bottom <= request.top
    {
        return Err(invalid(op, "invalid lock rectangle"));
    }
    let right = request.right as u32;
    let bottom = request.bottom as u32;
    let left = request.left as u32;
    let top = request.top as u32;
    if right > level.width || bottom > level.height {
        return Err(invalid(op, "lock region exceeds the subresource"));
    }
    let (front, back) = if volume {
        if request.front < 0 || request.back <= request.front {
            return Err(invalid(op, "invalid lock box"));
        }
        let front = request.front as u32;
        let back = request.back as u32;
        if back > level.depth {
            return Err(invalid(op, "lock box exceeds the subresource"));
        }
        (front, back)
    } else {
        (0, 1)
    };
    if block {
        if left % 4 != 0 || top % 4 != 0 {
            return Err(invalid(
                op,
                "compressed lock must start on a block boundary",
            ));
        }
        if right % 4 != 0 && right != level.width {
            return Err(invalid(
                op,
                "compressed lock right edge must be block aligned",
            ));
        }
        if bottom % 4 != 0 && bottom != level.height {
            return Err(invalid(
                op,
                "compressed lock bottom edge must be block aligned",
            ));
        }
    }
    if request.flags & D3DLOCK_DISCARD != 0 {
        return Ok(ResolvedLock {
            pitch: row,
            slice_pitch: slice,
            offset: 0,
            size: total,
            total,
            region: Region::Full,
        });
    }
    let block_bytes = format::block_bytes(format);
    let (col_bytes, left_bytes, row_units, row_index) = if block {
        let blocks_x = right.div_ceil(4) - left / 4;
        let blocks_y = bottom.div_ceil(4) - top / 4;
        (
            blocks_x * block_bytes,
            (left / 4) * block_bytes,
            blocks_y,
            top / 4,
        )
    } else {
        let bpp = format::bpp(format);
        ((right - left) * bpp, left * bpp, bottom - top, top)
    };
    let offset = front
        .checked_mul(slice)
        .and_then(|v| v.checked_add(row_index.checked_mul(row)?))
        .and_then(|v| v.checked_add(left_bytes))
        .ok_or_else(|| invalid(op, "lock offset overflow"))?;
    let depth_count = back - front;
    let size = (depth_count - 1)
        .checked_mul(slice)
        .and_then(|v| v.checked_add((row_units - 1).checked_mul(row)?))
        .and_then(|v| v.checked_add(col_bytes))
        .ok_or_else(|| invalid(op, "lock size overflow"))?;
    let end = offset
        .checked_add(size)
        .ok_or_else(|| invalid(op, "lock range overflow"))?;
    if end > total {
        return Err(invalid(op, "lock range exceeds the subresource"));
    }
    Ok(ResolvedLock {
        pitch: row,
        slice_pitch: slice,
        offset,
        size,
        total,
        region: Region::Rect {
            left,
            top,
            front,
            right,
            bottom,
            back,
        },
    })
}

fn resolve_level_lock(
    resource: &D3d9Resource,
    request: &LockRequest,
) -> Result<ResolvedLock, RenderError> {
    if !resource.is_lockable() {
        return Err(invalid("LockRect", "resource is not lockable"));
    }
    if request.volume != resource_is_volume(resource) {
        return Err(invalid(
            "LockRect",
            "lock selector does not match the resource kind",
        ));
    }
    validate_lock_flags(resource, request.flags)?;
    let level = resource.level(request.face, request.level)?;
    if level.lock_count.load(Ordering::Acquire) != 0 {
        return Err(invalid("LockRect", "the subresource is already locked"));
    }
    let format = resource_format(resource);
    resolve_lock(level, format, request.volume, request)
}

fn lock_result(resolved: &ResolvedLock) -> LockResult {
    LockResult {
        pitch: resolved.pitch,
        slice_pitch: resolved.slice_pitch,
        offset: resolved.offset,
        size: resolved.size,
        total: resolved.total,
    }
}

pub fn validate_level_lock(
    resource: &D3d9Resource,
    request: &LockRequest,
) -> Result<LockResult, RenderError> {
    resolve_level_lock(resource, request).map(|resolved| lock_result(&resolved))
}

pub fn lock_would_block(resource: &D3d9Resource, face: u32, level: u32) -> bool {
    match resource.state.as_ref() {
        Some(ResourceState::Surface(s)) => s.level.gpu_newer.load(Ordering::Acquire),
        Some(ResourceState::Texture(t)) => texture_index(t.kind, face, level, t.levels)
            .and_then(|index| t.subresources.get(index))
            .map(|l| l.gpu_newer.load(Ordering::Acquire))
            .unwrap_or(false),
        _ => false,
    }
}

pub fn level_lock_prepare(
    gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    request: &LockRequest,
) -> Result<LockResult, RenderError> {
    let resolved = resolve_level_lock(resource, request)?;
    if request.flags & D3DLOCK_DISCARD == 0 && !gpu.is_blocked() {
        if let Err(error) = readback_level(gpu, resource, request.face, request.level) {
            if !gpu.is_physical() {
                return Err(error);
            }
        }
    }
    Ok(lock_result(&resolved))
}

pub fn level_lock_commit(
    _gpu: &D3d9Gpu,
    resource: &mut D3d9Resource,
    request: &LockRequest,
) -> Result<LockResult, RenderError> {
    let resolved = resolve_level_lock(resource, request)?;
    let level = resource.level_mut(request.face, request.level)?;
    let writable = resolved.region_is_writable(request.flags);
    if writable && level.writable.try_reserve(1).is_err() {
        return Err(oom("LockRect", "lock record allocation failed"));
    }
    level.lock_count.store(1, Ordering::Release);
    if writable {
        if request.flags & D3DLOCK_NO_DIRTY_UPDATE == 0 {
            level.writable_dirty = true;
        }
        level.writable.push(resolved.region);
    }
    Ok(lock_result(&resolved))
}

impl ResolvedLock {
    fn region_is_writable(&self, flags: u32) -> bool {
        flags & D3DLOCK_READONLY == 0
    }
}

fn apply_region(level: &mut LevelState, staging: &[u8], format: u32, region: Region) {
    match region {
        Region::Full => {
            if staging.len() == level.bytes.len() {
                level.bytes.copy_from_slice(staging);
            }
        }
        Region::Rect {
            left,
            top,
            front,
            right,
            bottom,
            back,
        } => {
            let block = format::is_block(format);
            let Some(pitch) = format::row_pitch(format, level.width) else {
                return;
            };
            let rows = if block {
                format::block_rows(level.height)
            } else {
                level.height
            };
            let slice_pitch = pitch.saturating_mul(rows);
            let bpp = format::bpp(format).max(1);
            let block_bytes = format::block_bytes(format);
            let (left_bytes, row_bytes) = if block {
                let blocks_x = right.div_ceil(4) - left / 4;
                ((left / 4) * block_bytes, blocks_x * block_bytes)
            } else {
                (left * bpp, (right - left) * bpp)
            };
            let row_start = if block { top / 4 } else { top };
            let row_end = if block {
                format::block_rows(bottom)
            } else {
                bottom
            };
            for z in front..back {
                for y in row_start..row_end {
                    let offset = (z as usize)
                        .saturating_mul(slice_pitch as usize)
                        .saturating_add((y as usize).saturating_mul(pitch as usize))
                        .saturating_add(left_bytes as usize);
                    let end = offset.saturating_add(row_bytes as usize);
                    if end <= level.bytes.len() && end <= staging.len() {
                        level.bytes[offset..end].copy_from_slice(&staging[offset..end]);
                    }
                }
            }
        }
    }
}

pub fn level_bytes(resource: &D3d9Resource, face: u32, level: u32) -> Result<&[u8], RenderError> {
    Ok(resource.level(face, level)?.bytes.as_slice())
}

pub fn level_unlock(
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    bytes: &[u8],
) -> Result<bool, RenderError> {
    let format = resource_format(resource);
    let managed = resource.is_managed();
    let l = resource.level_mut(face, level)?;
    if l.lock_count.load(Ordering::Acquire) == 0 {
        return Err(invalid("UnlockRect", "resource is not locked"));
    }
    if bytes.len() != l.bytes.len() {
        return Err(invalid(
            "UnlockRect",
            "unlock staging size does not match the level",
        ));
    }
    let regions = core::mem::take(&mut l.writable);
    let dirty = l.writable_dirty;
    l.writable_dirty = false;
    l.lock_count.store(0, Ordering::Release);
    if !regions.is_empty() {
        for region in regions {
            apply_region(l, bytes, format, region);
        }
        l.generation = l.generation.wrapping_add(1);
        if !managed || dirty {
            let size = l.bytes.len() as u32;
            l.mark_dirty(0, size);
        }
    }
    Ok(true)
}

pub fn add_dirty_rect(
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    full_region: bool,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
) -> Result<(), RenderError> {
    if !matches!(resource.state.as_ref(), Some(ResourceState::Texture(t)) if t.kind != TextureKind::Volume)
    {
        return Err(invalid(
            "AddDirtyRect",
            "resource is not a 2D or cube texture",
        ));
    }
    let l = resource.level_mut(face, level)?;
    if !full_region
        && (left < 0
            || top < 0
            || right <= left
            || bottom <= top
            || right as u32 > l.width
            || bottom as u32 > l.height)
    {
        return Err(invalid(
            "AddDirtyRect",
            "dirty rectangle exceeds the subresource",
        ));
    }
    l.mark_dirty(0, l.bytes.len() as u32);
    Ok(())
}

pub fn add_dirty_box(
    resource: &mut D3d9Resource,
    level: u32,
    full_region: bool,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    front: u32,
    back: u32,
) -> Result<(), RenderError> {
    if !resource_is_volume(resource) {
        return Err(invalid("AddDirtyBox", "resource is not a volume texture"));
    }
    let l = resource.level_mut(0, level)?;
    if !full_region
        && (right <= left
            || bottom <= top
            || back <= front
            || right > l.width
            || bottom > l.height
            || back > l.depth)
    {
        return Err(invalid("AddDirtyBox", "dirty box exceeds the subresource"));
    }
    l.mark_dirty(0, l.bytes.len() as u32);
    Ok(())
}

pub fn set_autogen_filter(resource: &mut D3d9Resource, filter: u32) -> Result<(), RenderError> {
    let Some(ResourceState::Texture(t)) = resource.state.as_mut() else {
        return Err(invalid("SetAutoGenFilterType", "resource is not a texture"));
    };
    if !t.autogen {
        return Ok(());
    }
    if filter == AUTOGEN_FILTER_ANISOTROPIC {
        return Err(unsupported(
            "SetAutoGenFilterType",
            "anisotropic mip generation is not implemented",
        ));
    }
    if filter != AUTOGEN_FILTER_POINT && filter != AUTOGEN_FILTER_LINEAR {
        return Err(invalid(
            "SetAutoGenFilterType",
            "invalid D3DTEXF filter type",
        ));
    }
    if filter == AUTOGEN_FILTER_LINEAR {
        let gpu = resource
            .gpu
            .as_ref()
            .ok_or_else(|| invalid("SetAutoGenFilterType", "resource has no GPU context"))?;
        let format = format::d3d_to_wgpu(t.format)
            .ok_or_else(|| invalid("SetAutoGenFilterType", "unsupported texture format"))?;
        if !format
            .guaranteed_format_features(gpu.features())
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
        {
            return Err(invalid(
                "SetAutoGenFilterType",
                "texture format does not support linear filtering",
            ));
        }
    }
    t.autogen_filter = filter;
    Ok(())
}

pub fn autogen_filter(resource: &D3d9Resource) -> Result<u32, RenderError> {
    match resource.state.as_ref() {
        Some(ResourceState::Texture(t)) => Ok(t.autogen_filter),
        _ => Err(invalid("GetAutoGenFilterType", "resource is not a texture")),
    }
}

pub fn autogen_capable(resource: &D3d9Resource) -> bool {
    match resource.state.as_ref() {
        Some(ResourceState::Texture(t)) => t.autogen && t.gpu_levels > 1,
        _ => false,
    }
}

fn fit_transfer_rect(
    rect: Option<TargetRect>,
    width: u32,
    height: u32,
    op: &'static str,
) -> Result<(u32, u32, u32, u32), RenderError> {
    let (left, top, right, bottom) = match rect {
        Some(rect) => (
            rect.left as i64,
            rect.top as i64,
            rect.right as i64,
            rect.bottom as i64,
        ),
        None => (0, 0, width as i64, height as i64),
    };
    if left < 0
        || top < 0
        || right <= left
        || bottom <= top
        || right > width as i64
        || bottom > height as i64
    {
        return Err(invalid(op, "rectangle is outside the surface"));
    }
    Ok((left as u32, top as u32, right as u32, bottom as u32))
}

fn validate_block_transfer(
    format: u32,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    width: u32,
    height: u32,
    op: &'static str,
) -> Result<(), RenderError> {
    if !format::is_block(format) {
        return Ok(());
    }
    if left % 4 != 0 || top % 4 != 0 {
        return Err(invalid(
            op,
            "compressed transfer must start on a block boundary",
        ));
    }
    if right % 4 != 0 && right != width {
        return Err(invalid(
            op,
            "compressed transfer right edge must be block aligned",
        ));
    }
    if bottom % 4 != 0 && bottom != height {
        return Err(invalid(
            op,
            "compressed transfer bottom edge must be block aligned",
        ));
    }
    Ok(())
}

fn copy_checked(
    destination: &mut LevelState,
    source: &[u8],
    dst_at: usize,
    src_at: usize,
    len: usize,
    op: &'static str,
) -> Result<(), RenderError> {
    let dst_end = dst_at
        .checked_add(len)
        .ok_or_else(|| invalid(op, "copy range overflow"))?;
    let src_end = src_at
        .checked_add(len)
        .ok_or_else(|| invalid(op, "copy range overflow"))?;
    if dst_end > destination.bytes.len() || src_end > source.len() {
        return Err(invalid(op, "copy range is outside the subresource"));
    }
    destination.bytes[dst_at..dst_end].copy_from_slice(&source[src_at..src_end]);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_packed_region(
    destination: &mut LevelState,
    source: &[u8],
    source_width: u32,
    format: u32,
    src_left: u32,
    src_top: u32,
    src_right: u32,
    src_bottom: u32,
    dst_left: u32,
    dst_top: u32,
    op: &'static str,
) -> Result<(), RenderError> {
    let src_pitch = format::row_pitch(format, source_width)
        .ok_or_else(|| invalid(op, "invalid source pitch"))? as usize;
    let dst_pitch = format::row_pitch(format, destination.width)
        .ok_or_else(|| invalid(op, "invalid destination pitch"))? as usize;
    if format::is_block(format) {
        let block = format::block_bytes(format) as usize;
        let rows = (format::block_rows(src_bottom) - src_top / 4) as usize;
        let cols = (src_right.div_ceil(4) - src_left / 4) as usize;
        for row in 0..rows {
            let src_at = (src_top as usize / 4 + row) * src_pitch + (src_left as usize / 4) * block;
            let dst_at = (dst_top as usize / 4 + row) * dst_pitch + (dst_left as usize / 4) * block;
            copy_checked(destination, source, dst_at, src_at, cols * block, op)?;
        }
    } else {
        let bpp = format::bpp(format) as usize;
        let rows = (src_bottom - src_top) as usize;
        let row_bytes = (src_right - src_left) as usize * bpp;
        for row in 0..rows {
            let src_at = (src_top as usize + row) * src_pitch + src_left as usize * bpp;
            let dst_at = (dst_top as usize + row) * dst_pitch + dst_left as usize * bpp;
            copy_checked(destination, source, dst_at, src_at, row_bytes, op)?;
        }
    }
    Ok(())
}

fn level_lock_count(resource: &D3d9Resource, face: u32, level: u32) -> Result<u32, RenderError> {
    Ok(resource
        .level(face, level)?
        .lock_count
        .load(Ordering::Acquire))
}

fn level_multisample(resource: &D3d9Resource) -> u32 {
    match resource.state.as_ref() {
        Some(ResourceState::Surface(s)) => s.multisample_type,
        _ => 0,
    }
}

fn texture_transfer_meta(
    resource: &D3d9Resource,
    op: &'static str,
) -> Result<(TextureKind, u32, u32, u32, bool), RenderError> {
    match resource.state.as_ref() {
        Some(ResourceState::Texture(t)) => Ok((t.kind, t.format, t.levels, t.pool, t.autogen)),
        _ => Err(invalid(op, "resource is not a texture")),
    }
}

pub fn update_surface(
    gpu: &D3d9Gpu,
    source: &D3d9Resource,
    src_face: u32,
    src_level: u32,
    src_rect: Option<TargetRect>,
    destination: &mut D3d9Resource,
    dst_face: u32,
    dst_level: u32,
    dst_point: (i32, i32),
) -> Result<(), RenderError> {
    const OP: &str = "UpdateSurface";
    let src_format = resource_format(source);
    let dst_format = resource_format(destination);
    if source.pool() != Some(D3DPOOL_SYSTEMMEM) {
        return Err(invalid(OP, "source must be a system-memory surface"));
    }
    if destination.pool() != Some(D3DPOOL_DEFAULT) {
        return Err(invalid(OP, "destination must be a default-pool surface"));
    }
    if src_format != dst_format {
        return Err(invalid(OP, "source and destination formats must match"));
    }
    if format::is_depth(src_format) {
        return Err(invalid(OP, "depth/stencil surfaces are not valid operands"));
    }
    if level_multisample(source) != 0 || level_multisample(destination) != 0 {
        return Err(invalid(OP, "multisampled surfaces are not supported"));
    }
    if level_lock_count(source, src_face, src_level)? != 0 {
        return Err(invalid(OP, "source is locked"));
    }
    if level_lock_count(destination, dst_face, dst_level)? != 0 {
        return Err(invalid(OP, "destination is locked"));
    }
    let (src_width, src_height, src_depth) = {
        let level = source.level(src_face, src_level)?;
        (level.width, level.height, level.depth)
    };
    let (dst_width, dst_height, dst_depth) = {
        let level = destination.level(dst_face, dst_level)?;
        (level.width, level.height, level.depth)
    };
    if src_depth != 1 || dst_depth != 1 {
        return Err(invalid(OP, "UpdateSurface requires single-slice surfaces"));
    }
    let (src_left, src_top, src_right, src_bottom) =
        fit_transfer_rect(src_rect, src_width, src_height, OP)?;
    validate_block_transfer(
        src_format, src_left, src_top, src_right, src_bottom, src_width, src_height, OP,
    )?;
    let (dst_x, dst_y) = dst_point;
    if dst_x < 0 || dst_y < 0 {
        return Err(invalid(OP, "negative destination point"));
    }
    let (dst_x, dst_y) = (dst_x as u32, dst_y as u32);
    let rect_width = src_right - src_left;
    let rect_height = src_bottom - src_top;
    if dst_x
        .checked_add(rect_width)
        .is_none_or(|right| right > dst_width)
        || dst_y
            .checked_add(rect_height)
            .is_none_or(|bottom| bottom > dst_height)
    {
        return Err(invalid(OP, "destination rectangle is outside the surface"));
    }
    validate_block_transfer(
        src_format,
        dst_x,
        dst_y,
        dst_x + rect_width,
        dst_y + rect_height,
        dst_width,
        dst_height,
        OP,
    )?;
    let full = src_left == 0
        && src_top == 0
        && src_right == src_width
        && src_bottom == src_height
        && dst_x == 0
        && dst_y == 0
        && src_width == dst_width
        && src_height == dst_height;
    let gpu_ready = !gpu.is_blocked();
    if gpu_ready && !full && destination.gpu_texture().is_some() {
        readback_level(gpu, destination, dst_face, dst_level)?;
    }
    {
        let source_bytes = source.level(src_face, src_level)?.bytes.as_slice();
        let destination_level = destination.level_mut(dst_face, dst_level)?;
        copy_packed_region(
            destination_level,
            source_bytes,
            src_width,
            src_format,
            src_left,
            src_top,
            src_right,
            src_bottom,
            dst_x,
            dst_y,
            OP,
        )?;
        destination_level.generation = destination_level.generation.wrapping_add(1);
        destination_level.dirty = true;
        destination_level.dirty_lo = 0;
        destination_level.dirty_hi = destination_level.bytes.len() as u32;
        destination_level.gpu_newer.store(false, Ordering::Release);
    }
    if gpu_ready {
        upload_level(gpu, destination, dst_face, dst_level)?;
    }
    Ok(())
}

pub fn update_texture(
    gpu: &D3d9Gpu,
    source: &D3d9Resource,
    destination: &mut D3d9Resource,
) -> Result<(), RenderError> {
    const OP: &str = "UpdateTexture";
    let (src_kind, src_format, src_levels, src_pool, src_autogen) =
        texture_transfer_meta(source, OP)?;
    let (dst_kind, dst_format, dst_levels, dst_pool, dst_autogen) =
        texture_transfer_meta(destination, OP)?;
    if src_pool != D3DPOOL_SYSTEMMEM {
        return Err(invalid(OP, "source must be a system-memory texture"));
    }
    if dst_pool != D3DPOOL_DEFAULT {
        return Err(invalid(OP, "destination must be a default-pool texture"));
    }
    if src_kind != dst_kind {
        return Err(invalid(
            OP,
            "source and destination texture kinds must match",
        ));
    }
    if src_format != dst_format {
        return Err(invalid(OP, "source and destination formats must match"));
    }
    if format::is_depth(src_format) {
        return Err(invalid(OP, "depth/stencil textures are not valid operands"));
    }
    if dst_levels > src_levels {
        return Err(invalid(
            OP,
            "destination has more mip levels than the source",
        ));
    }
    if src_autogen && !dst_autogen {
        return Err(invalid(
            OP,
            "an autogen source texture requires an autogen destination",
        ));
    }
    let level_offset = if dst_autogen {
        let dst = destination.level(0, 0)?;
        (0..src_levels)
            .find(|&level| {
                source.level(0, level).is_ok_and(|src| {
                    src.width == dst.width && src.height == dst.height && src.depth == dst.depth
                })
            })
            .ok_or_else(|| invalid(OP, "source has no matching autogen destination level"))?
    } else {
        src_levels - dst_levels
    };
    let faces = face_count(src_kind);
    let copied_levels = if dst_autogen { 1 } else { dst_levels };
    for face in 0..faces {
        for level in 0..copied_levels {
            let src_level = level + level_offset;
            let src_index = texture_index(src_kind, face, src_level, src_levels)
                .ok_or_else(|| invalid(OP, "source subresource is missing"))?;
            let dst_index = texture_index(dst_kind, face, level, dst_levels)
                .ok_or_else(|| invalid(OP, "destination subresource is missing"))?;
            let (src_w, src_h, src_d, src_lock, src_len) = {
                let texture = match source.state.as_ref() {
                    Some(ResourceState::Texture(t)) => t,
                    _ => return Err(invalid(OP, "source is not a texture")),
                };
                let level = &texture.subresources[src_index];
                (
                    level.width,
                    level.height,
                    level.depth,
                    level.lock_count.load(Ordering::Acquire),
                    level.bytes.len(),
                )
            };
            let (dst_w, dst_h, dst_d, dst_lock, dst_len) = {
                let texture = match destination.state.as_ref() {
                    Some(ResourceState::Texture(t)) => t,
                    _ => return Err(invalid(OP, "destination is not a texture")),
                };
                let level = &texture.subresources[dst_index];
                (
                    level.width,
                    level.height,
                    level.depth,
                    level.lock_count.load(Ordering::Acquire),
                    level.bytes.len(),
                )
            };
            if src_w != dst_w || src_h != dst_h || src_d != dst_d {
                return Err(invalid(
                    OP,
                    "source and destination subresource dimensions must match",
                ));
            }
            if src_lock != 0 || dst_lock != 0 {
                return Err(invalid(
                    OP,
                    "textures must not be locked during UpdateTexture",
                ));
            }
            if src_len != dst_len {
                return Err(invalid(
                    OP,
                    "source and destination subresource sizes must match",
                ));
            }
        }
    }
    for face in 0..faces {
        for level in 0..copied_levels {
            let src_level = level + level_offset;
            let src_index = texture_index(src_kind, face, src_level, src_levels)
                .ok_or_else(|| invalid(OP, "source subresource is missing"))?;
            let dst_index = texture_index(dst_kind, face, level, dst_levels)
                .ok_or_else(|| invalid(OP, "destination subresource is missing"))?;
            let source_bytes = match source.state.as_ref() {
                Some(ResourceState::Texture(t)) => t.subresources[src_index].bytes.as_slice(),
                _ => return Err(invalid(OP, "source is not a texture")),
            };
            let destination_level = match destination.state.as_mut() {
                Some(ResourceState::Texture(t)) => &mut t.subresources[dst_index],
                _ => return Err(invalid(OP, "destination is not a texture")),
            };
            destination_level.bytes.copy_from_slice(source_bytes);
            destination_level.generation = destination_level.generation.wrapping_add(1);
            destination_level.dirty = true;
            destination_level.dirty_lo = 0;
            destination_level.dirty_hi = destination_level.bytes.len() as u32;
            destination_level.gpu_newer.store(false, Ordering::Release);
        }
    }
    if !gpu.is_blocked() {
        preload(gpu, destination)?;
    }
    Ok(())
}

pub fn store_readback(
    destination: &mut D3d9Resource,
    face: u32,
    level: u32,
    bytes: &[u8],
    format: u32,
    width: u32,
    height: u32,
) -> Result<(), RenderError> {
    const OP: &str = "GetRenderTargetData";
    if resource_format(destination) != format {
        return Err(invalid(
            OP,
            "destination format does not match the readback",
        ));
    }
    if destination.pool() != Some(D3DPOOL_SYSTEMMEM) {
        return Err(invalid(OP, "destination must be a system-memory surface"));
    }
    if format::is_depth(format) {
        return Err(invalid(
            OP,
            "depth/stencil formats cannot be stored by readback",
        ));
    }
    let multisample = match destination.state.as_ref() {
        Some(ResourceState::Surface(s)) => s.multisample_type,
        Some(ResourceState::Texture(_)) => 0,
        _ => return Err(invalid(OP, "resource is not a texture or surface")),
    };
    if multisample != 0 {
        return Err(invalid(OP, "multisampled destinations are not supported"));
    }
    let (dst_width, dst_height, dst_depth, lock_count) = {
        let level = destination.level(face, level)?;
        (
            level.width,
            level.height,
            level.depth,
            level.lock_count.load(Ordering::Acquire),
        )
    };
    if dst_depth != 1 {
        return Err(invalid(
            OP,
            "readback destinations must be single-slice surfaces",
        ));
    }
    if lock_count != 0 {
        return Err(invalid(OP, "destination is locked"));
    }
    if dst_width != width || dst_height != height {
        return Err(invalid(
            OP,
            "destination dimensions do not match the readback",
        ));
    }
    let expected = format::level_size(format, width, height)
        .ok_or_else(|| invalid(OP, "unsupported destination format"))? as usize;
    if bytes.len() < expected {
        return Err(invalid(
            OP,
            "readback bytes are shorter than the destination level",
        ));
    }
    let level = destination.level_mut(face, level)?;
    if level.bytes.len() != expected {
        return Err(invalid(OP, "destination CPU storage size is inconsistent"));
    }
    level.bytes.copy_from_slice(&bytes[..expected]);
    level.generation = level.generation.wrapping_add(1);
    level.gpu_generation = level.generation;
    level.gpu_newer.store(false, Ordering::Release);
    level.dirty = false;
    level.dirty_lo = 0;
    level.dirty_hi = 0;
    Ok(())
}
