use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferHandle {
    owner: u64,
    id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureHandle {
    owner: u64,
    id: u32,
}

pub struct Texture2dRequest {
    pub width: u32,
    pub height: u32,
    pub levels: u32,
    pub usage: u32,
    pub format: u32,
    pub pool: u32,
    pub shared_handle: bool,
}

pub struct TextureCreate {
    pub handle: TextureHandle,
    pub no_autogen: bool,
}

pub struct TextureLevelDescriptor {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub levels: u32,
}

#[derive(Clone, Copy)]
pub struct TextureRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub struct TextureLock {
    pub pitch: u32,
    pub offset: u32,
    pub size: u32,
    pub bytes: Vec<u8>,
}

pub struct TextureUnlockResult {
    pub closed: bool,
    pub status: Result<(), ApiError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiErrorKind {
    InvalidArgument,
    Unsupported,
    Backend,
    NotImplemented,
    OutOfMemory,
    WasStillDrawing,
    DeviceLost,
    DeviceNotReset,
    NoAutogen,
    NotAvailable,
    InvalidData,
    Unknown(u8),
}

impl ApiErrorKind {
    fn from_status(code: StatusCode) -> Self {
        match code {
            StatusCode::InvalidArgument => Self::InvalidArgument,
            StatusCode::Unsupported => Self::Unsupported,
            StatusCode::Backend => Self::Backend,
            StatusCode::NotImplemented => Self::NotImplemented,
            StatusCode::OutOfMemory => Self::OutOfMemory,
            StatusCode::WasStillDrawing => Self::WasStillDrawing,
            StatusCode::DeviceLost => Self::DeviceLost,
            StatusCode::DeviceNotReset => Self::DeviceNotReset,
            StatusCode::NoAutogen => Self::NoAutogen,
            StatusCode::NotAvailable => Self::NotAvailable,
            StatusCode::InvalidData => Self::InvalidData,
            _ => Self::Unknown(code.repr),
        }
    }

    pub(crate) fn status_code(self) -> StatusCode {
        match self {
            Self::InvalidArgument => StatusCode::InvalidArgument,
            Self::Unsupported => StatusCode::Unsupported,
            Self::Backend => StatusCode::Backend,
            Self::NotImplemented => StatusCode::NotImplemented,
            Self::OutOfMemory => StatusCode::OutOfMemory,
            Self::WasStillDrawing => StatusCode::WasStillDrawing,
            Self::DeviceLost => StatusCode::DeviceLost,
            Self::DeviceNotReset => StatusCode::DeviceNotReset,
            Self::NoAutogen => StatusCode::NoAutogen,
            Self::NotAvailable => StatusCode::NotAvailable,
            Self::InvalidData => StatusCode::InvalidData,
            Self::Unknown(repr) => StatusCode { repr },
        }
    }
}

#[derive(Debug)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
}

impl core::fmt::Display for ApiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<RenderError> for ApiError {
    fn from(error: RenderError) -> Self {
        let kind = match error.kind {
            crate::RenderErrorKind::InvalidArgument => ApiErrorKind::InvalidArgument,
            crate::RenderErrorKind::Unsupported => ApiErrorKind::Unsupported,
            crate::RenderErrorKind::OutOfMemory => ApiErrorKind::OutOfMemory,
            crate::RenderErrorKind::InvalidData => ApiErrorKind::InvalidData,
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

impl From<Status> for ApiError {
    fn from(status: Status) -> Self {
        Self {
            kind: ApiErrorKind::from_status(status.code),
            message: status.message,
        }
    }
}

fn check(status: Status) -> Result<(), ApiError> {
    if status.code == StatusCode::Ok {
        Ok(())
    } else {
        Err(status.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Rgba8Unorm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlphaInterpretation {
    Unspecified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorInterpretation {
    UnormNoTransferConversion,
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub alpha: AlphaInterpretation,
    pub color: ColorInterpretation,
}

pub struct GpuImageLease {
    width: u32,
    height: u32,
    format: ImageFormat,
    alpha: AlphaInterpretation,
    color: ColorInterpretation,
    context: Arc<GpuContext>,
    texture: wgpu::Texture,
}

impl GpuImageLease {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn format(&self) -> ImageFormat {
        self.format
    }
    pub fn alpha(&self) -> AlphaInterpretation {
        self.alpha
    }
    pub fn color(&self) -> ColorInterpretation {
        self.color
    }

    pub fn context(&self) -> &Arc<GpuContext> {
        &self.context
    }

    pub fn texture_for(&self, context: &Arc<GpuContext>) -> Result<&wgpu::Texture, ApiError> {
        if !Arc::ptr_eq(&self.context, context) {
            return Err(ApiError {
                kind: ApiErrorKind::InvalidArgument,
                message: "image belongs to a different GPU context".into(),
            });
        }
        if context.physical_loss().is_lost() {
            return Err(ApiError {
                kind: ApiErrorKind::DeviceLost,
                message: "image GPU context is lost".into(),
            });
        }
        Ok(&self.texture)
    }
}

pub struct Device {
    device: D3d9Device,
    owner: u64,
    next_id: u32,
    buffers: BTreeMap<u32, D3d9Resource>,
    textures: BTreeMap<u32, D3d9Resource>,
    released: BTreeSet<u32>,
}

impl Device {
    pub fn new(device: D3d9Device) -> Self {
        static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
        Self {
            device,
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            next_id: 1,
            buffers: BTreeMap::new(),
            textures: BTreeMap::new(),
            released: BTreeSet::new(),
        }
    }

    pub fn supply_replacement(&mut self, context: Arc<GpuContext>) -> Result<(), ApiError> {
        self.device.supply_replacement(context)?;
        Ok(())
    }

    fn id(&self, handle: BufferHandle) -> Result<u32, ApiError> {
        if handle.owner != self.owner
            || !self.buffers.contains_key(&handle.id)
            || self.released.contains(&handle.id)
        {
            return Err(ApiError {
                kind: ApiErrorKind::InvalidArgument,
                message: "invalid buffer handle".into(),
            });
        }
        Ok(handle.id)
    }

    fn settle_released(&mut self) -> Result<(), ApiError> {
        if self
            .device
            .loss
            .as_ref()
            .is_some_and(|loss| loss.is_blocked())
        {
            self.device.draw.discard_batch();
        } else {
            flush_draws(&mut self.device)?;
        }
        Ok(())
    }

    fn reap_released(&mut self) {
        let bindings = self.device.state.bindings();
        let dead: Vec<u32> = self
            .released
            .iter()
            .copied()
            .filter(|id| {
                !bindings.textures.contains(id)
                    && !bindings.streams.contains(id)
                    && bindings.indices != *id
            })
            .collect();
        for id in dead {
            if let Err(error) = check(d3d9_unregister_resource(&mut self.device, id)) {
                log::error!("D3D9 could not unregister released resource {id}: {error}");
                continue;
            }
            self.buffers.remove(&id);
            self.textures.remove(&id);
            self.released.remove(&id);
        }
    }

    pub fn create_buffer(
        &mut self,
        request: &resource::BufferRequest,
    ) -> Result<BufferHandle, ApiError> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| ApiError {
            kind: ApiErrorKind::OutOfMemory,
            message: "buffer handle space exhausted".into(),
        })?;
        let gpu = self.device.resource_gpu()?.clone();
        let mut buffer = D3d9Resource::default();
        resource::create_buffer(&gpu, &mut buffer, request)?;
        check(d3d9_register_resource(&mut self.device, id, &buffer))?;
        self.buffers.insert(id, buffer);
        Ok(BufferHandle {
            owner: self.owner,
            id,
        })
    }

    pub fn lock_buffer(
        &mut self,
        handle: BufferHandle,
        request: &resource::BufferLockRequest,
    ) -> Result<(resource::BufferLockRange, Vec<u8>), ApiError> {
        let id = self.id(handle)?;
        let buffer = self.buffers.get_mut(&id).unwrap();
        if let Some(resource::ResourceState::Buffer(state)) = buffer.state.as_ref() {
            if state.lock_count.load(Ordering::Acquire) != 0 {
                return Err(ApiError {
                    kind: ApiErrorKind::Unsupported,
                    message: "nested standalone buffer locks are not implemented".into(),
                });
            }
        }
        let range = buffer.prepare_buffer_lock(request)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(buffer.buffer_bytes().len())
            .map_err(|_| ApiError {
                kind: ApiErrorKind::OutOfMemory,
                message: "buffer staging allocation failed".into(),
            })?;
        bytes.extend_from_slice(buffer.buffer_bytes());
        buffer.commit_buffer_lock(request)?;
        Ok((range, bytes))
    }

    pub fn unlock_buffer(&mut self, handle: BufferHandle, bytes: &[u8]) -> Result<bool, ApiError> {
        let id = self.id(handle)?;
        let result = self.buffers.get_mut(&id).unwrap().unlock_buffer(bytes);
        result.status?;
        Ok(result.closed)
    }

    pub fn update_buffer(&mut self, handle: BufferHandle, bytes: &[u8]) -> Result<(), ApiError> {
        let id = self.id(handle)?;
        let buffer = self.buffers.get_mut(&id).unwrap();
        if bytes.len() != buffer.buffer_bytes().len() {
            return Err(ApiError {
                kind: ApiErrorKind::InvalidArgument,
                message: "buffer staging size does not match the buffer".into(),
            });
        }
        if let Some(resource::ResourceState::Buffer(state)) = buffer.state.as_ref() {
            if state.lock_count.load(Ordering::Acquire) != 0 {
                return Err(ApiError {
                    kind: ApiErrorKind::InvalidArgument,
                    message: "buffer is locked".into(),
                });
            }
        }
        buffer.commit_buffer_lock(&resource::BufferLockRequest {
            offset: 0,
            size: 0,
            flags: 0,
        })?;
        buffer.unlock_buffer(bytes).status?;
        Ok(())
    }

    fn texture_id(&self, handle: TextureHandle) -> Result<u32, ApiError> {
        if handle.owner != self.owner
            || !self.textures.contains_key(&handle.id)
            || self.released.contains(&handle.id)
        {
            return Err(ApiError {
                kind: ApiErrorKind::InvalidArgument,
                message: "invalid texture handle".into(),
            });
        }
        Ok(handle.id)
    }

    pub fn create_texture_2d(
        &mut self,
        request: &Texture2dRequest,
    ) -> Result<TextureCreate, ApiError> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or_else(|| ApiError {
            kind: ApiErrorKind::OutOfMemory,
            message: "texture handle space exhausted".into(),
        })?;
        let gpu = self.device.resource_gpu()?.clone();
        let mut texture = D3d9Resource::default();
        let mapped = resource::TextureRequest {
            kind: resource::TextureKind::Texture2D,
            width: request.width,
            height: request.height,
            depth: 1,
            levels: request.levels,
            usage: request.usage,
            format: request.format,
            pool: request.pool,
            shared_handle: request.shared_handle,
        };
        resource::create_texture(&gpu, &mut texture, &mapped)?;
        let no_autogen = texture_no_autogen(&texture, request.usage);
        check(d3d9_register_resource(&mut self.device, id, &texture))?;
        self.textures.insert(id, texture);
        Ok(TextureCreate {
            handle: TextureHandle {
                owner: self.owner,
                id,
            },
            no_autogen,
        })
    }

    pub fn texture_level_descriptor(
        &self,
        handle: TextureHandle,
        level: u32,
    ) -> Result<TextureLevelDescriptor, ApiError> {
        let id = self.texture_id(handle)?;
        let texture = &self.textures[&id];
        let (width, height, _, levels) = resource::level_descriptor(texture, 0, level)?;
        let Some(resource::ResourceState::Texture(state)) = texture.state.as_ref() else {
            unreachable!()
        };
        Ok(TextureLevelDescriptor {
            format: state.format,
            width,
            height,
            levels,
        })
    }

    pub fn lock_texture_rect(
        &mut self,
        handle: TextureHandle,
        level: u32,
        rect: Option<TextureRect>,
        flags: u32,
    ) -> Result<TextureLock, ApiError> {
        let id = self.texture_id(handle)?;
        flush_draws(&mut self.device)?;
        let texture = self.textures.get_mut(&id).unwrap();
        let rect_value = rect.unwrap_or(TextureRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        });
        let request = resource::LockRequest {
            face: 0,
            level,
            volume: false,
            full_region: rect.is_none(),
            left: rect_value.left.into(),
            top: rect_value.top.into(),
            front: 0,
            right: rect_value.right.into(),
            bottom: rect_value.bottom.into(),
            back: 0,
            flags,
        };
        let layout = level_lock_prepare(texture, &request)?;
        let source = resource::level_bytes(texture, 0, level)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(source.len())
            .map_err(|_| ApiError {
                kind: ApiErrorKind::OutOfMemory,
                message: "texture staging allocation failed".into(),
            })?;
        bytes.extend_from_slice(source);
        let gpu = texture.gpu_handle()?;
        resource::level_lock_commit(&gpu, texture, &request)?;
        Ok(TextureLock {
            pitch: layout.pitch,
            offset: layout.offset,
            size: layout.size,
            bytes,
        })
    }

    pub fn unlock_texture_rect(
        &mut self,
        handle: TextureHandle,
        level: u32,
        bytes: &[u8],
    ) -> TextureUnlockResult {
        let id = match self.texture_id(handle) {
            Ok(id) => id,
            Err(error) => {
                return TextureUnlockResult {
                    closed: false,
                    status: Err(error),
                };
            }
        };
        if let Err(error) = flush_draws(&mut self.device) {
            return TextureUnlockResult {
                closed: false,
                status: Err(error.into()),
            };
        }
        let (result, closed) = level_unlock(self.textures.get_mut(&id).unwrap(), 0, level, bytes);
        TextureUnlockResult {
            closed,
            status: result.map_err(Into::into),
        }
    }

    pub fn set_texture(
        &mut self,
        stage: u32,
        handle: Option<TextureHandle>,
    ) -> Result<(), ApiError> {
        let id = match handle {
            Some(handle) => self.texture_id(handle)?,
            None => 0,
        };
        if !self.released.is_empty() {
            self.settle_released()?;
        }
        set_texture(&mut self.device, stage, id)?;
        self.reap_released();
        Ok(())
    }

    pub fn set_texture_stage_state(
        &mut self,
        stage: u32,
        state: u32,
        value: u32,
    ) -> Result<(), ApiError> {
        self.device
            .state
            .set_texture_stage_state(stage, state, value)?;
        Ok(())
    }

    pub fn set_sampler_state(
        &mut self,
        stage: u32,
        state: u32,
        value: u32,
    ) -> Result<(), ApiError> {
        self.device.state.set_sampler_state(stage, state, value)?;
        Ok(())
    }

    pub fn release_texture(&mut self, handle: TextureHandle) -> Result<(), ApiError> {
        let id = self.texture_id(handle)?;
        self.settle_released()?;
        self.released.insert(id);
        self.reap_released();
        Ok(())
    }

    pub fn set_stream(
        &mut self,
        handle: BufferHandle,
        offset: u32,
        stride: u32,
    ) -> Result<(), ApiError> {
        let id = self.id(handle)?;
        if !self.released.is_empty() {
            self.settle_released()?;
        }
        set_stream_source(&mut self.device, 0, id, offset, stride)?;
        self.reap_released();
        Ok(())
    }

    pub fn clear_stream(&mut self) -> Result<(), ApiError> {
        if !self.released.is_empty() {
            self.settle_released()?;
        }
        set_stream_source(&mut self.device, 0, 0, 0, 0)?;
        self.reap_released();
        Ok(())
    }

    pub fn set_fvf(&mut self, fvf: u32) -> Result<(), ApiError> {
        let handle = create_fvf_declaration(&mut self.device, fvf)?;
        self.device.state.set_declaration(handle, fvf);
        Ok(())
    }

    pub fn set_render_state(&mut self, state: u32, value: u32) -> Result<(), ApiError> {
        self.device.state.set_render_state(state, value)?;
        Ok(())
    }

    pub fn clear(&mut self, color: u32) -> Result<(), ApiError> {
        clear(&mut self.device, 1, color, 1.0, 0, &[])
    }

    pub fn begin_scene(&mut self) -> Result<(), ApiError> {
        Ok(begin_scene(&mut self.device)?)
    }
    pub fn end_scene(&mut self) -> Result<(), ApiError> {
        Ok(end_scene(&mut self.device)?)
    }
    pub fn draw_primitive(
        &mut self,
        primitive: u32,
        start: u32,
        count: u32,
    ) -> Result<(), ApiError> {
        draw_primitive(&mut self.device, primitive, start, count)
    }
    pub fn present_gpu_image(&mut self) -> Result<GpuImageLease, ApiError> {
        let (context, texture, width, height) = present_gpu_snapshot(&mut self.device)?;
        Ok(GpuImageLease {
            width,
            height,
            format: ImageFormat::Rgba8Unorm,
            alpha: AlphaInterpretation::Unspecified,
            color: ColorInterpretation::UnormNoTransferConversion,
            context,
            texture,
        })
    }

    pub fn present_image(&mut self) -> Result<Image, ApiError> {
        present_cpu_image(&mut self.device, &[], &[])
    }
    pub fn release_buffer(&mut self, handle: BufferHandle) -> Result<(), ApiError> {
        let id = self.id(handle)?;
        self.settle_released()?;
        self.released.insert(id);
        self.reap_released();
        Ok(())
    }
    pub fn reset_device(
        &mut self,
        present: PresentParams,
        display_mode: DisplayMode,
    ) -> Result<PresentParams, ApiError> {
        let result = d3d9_reset(
            &mut self.device,
            &crate::bridge::D3d9ResetRequest {
                present: present.into(),
                display_mode: display_mode.into(),
                implicit_external_refs: false,
                window_valid: true,
            },
        );
        check(result.status)?;
        for (&id, resource) in self.buffers.iter().chain(&self.textures) {
            if let Err(error) = check(d3d9_prepare_resource_reset(&mut self.device, id, resource)) {
                d3d9_abort_reset(&mut self.device);
                return Err(error);
            }
        }
        if let Err(error) = check(d3d9_reset_ready(&mut self.device)) {
            d3d9_abort_reset(&mut self.device);
            return Err(error);
        }
        for (&id, resource) in self.buffers.iter_mut().chain(&mut self.textures) {
            d3d9_commit_resource_reset(&mut self.device, id, resource);
        }
        let result = d3d9_finish_reset(&mut self.device);
        check(result.status)?;
        self.reap_released();
        Ok(result.present.into())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self
            .device
            .loss
            .as_ref()
            .is_some_and(|loss| loss.is_physical())
        {
            self.device.draw.discard_batch();
        } else {
            if let Err(error) = flush_draws(&mut self.device) {
                log::error!("D3D9 device drop could not flush draws: {error}");
            }
            if let Some(gpu) = &self.device.gpu {
                gpu.synchronize_d3d9();
            }
        }
        while let Some((id, _buffer)) = self.buffers.pop_first() {
            if let Err(error) = check(d3d9_unregister_resource(&mut self.device, id)) {
                log::error!("D3D9 device drop could not unregister buffer {id}: {error}");
            }
        }
        while let Some((id, _texture)) = self.textures.pop_first() {
            if let Err(error) = check(d3d9_unregister_resource(&mut self.device, id)) {
                log::error!("D3D9 device drop could not unregister texture {id}: {error}");
            }
        }
    }
}
