use super::buffer::{memory, memory_error, render_error};
use crate::bridge::{
    D3d9GuestRectRequest, D3d9GuestRectRequestResult, D3d9GuestSurfaceCreateResult,
    D3d9GuestTextureCreateResult, D3d9SurfaceRequest, D3d9TextureKind, D3d9TextureRequest, Status,
    StatusCode, status,
};
use crate::d3d9::{
    self,
    resource::{self, D3d9Resource, LockRequest, ResourceState, TextureKind},
};
use crate::guest::{GuestAddr, GuestAllocation};

#[derive(Default)]
pub struct GuestTexture {
    staging: Vec<(u32, LevelStaging)>,
    scratch: Vec<u8>,
}

struct LevelStaging {
    allocation: GuestAllocation,
    size: u32,
}

pub fn new_guest_texture() -> Box<GuestTexture> {
    Box::default()
}
fn invalid(message: impl Into<String>) -> Status {
    status(StatusCode::InvalidArgument, message)
}
fn done(result: Result<(), Status>) -> Status {
    match result {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => error,
    }
}

pub unsafe fn guest_texture_prepare_create(
    callbacks: usize,
    out: u32,
    width: u32,
    height: u32,
    levels: u32,
    usage: u32,
    format: u32,
    pool: u32,
    shared_handle: bool,
) -> D3d9GuestTextureCreateResult {
    let request = D3d9TextureRequest {
        kind: D3d9TextureKind::Texture2D,
        width,
        height,
        depth: 1,
        levels,
        usage,
        format,
        pool,
        shared_handle,
    };
    let result = unsafe { memory(callbacks) }.and_then(|mut memory| {
        let out = GuestAddr::new(out).map_err(memory_error)?;
        memory.write_u32(out, 0).map_err(memory_error)
    });
    D3d9GuestTextureCreateResult {
        status: done(result),
        request,
    }
}

pub unsafe fn guest_surface_prepare_create(
    callbacks: usize,
    out: u32,
    width: u32,
    height: u32,
    format: u32,
    offscreen: bool,
    pool_or_multisample: u32,
    quality: u32,
    lockable_or_discard: bool,
    usage: u32,
    shared_handle: bool,
) -> D3d9GuestSurfaceCreateResult {
    let request = D3d9SurfaceRequest {
        width,
        height,
        format,
        pool: if offscreen { pool_or_multisample } else { 0 },
        usage: if offscreen { 0 } else { usage },
        multisample_type: if offscreen { 0 } else { pool_or_multisample },
        multisample_quality: if offscreen { 0 } else { quality },
        shared_handle,
        lockable: !offscreen && usage == resource::D3DUSAGE_RENDERTARGET && lockable_or_discard,
        discard: !offscreen && usage == resource::D3DUSAGE_DEPTHSTENCIL && lockable_or_discard,
    };
    let result = unsafe { memory(callbacks) }.and_then(|mut memory| {
        let out = GuestAddr::new(out).map_err(memory_error)?;
        memory.write_u32(out, 0).map_err(memory_error)
    });
    D3d9GuestSurfaceCreateResult {
        status: done(result),
        request,
    }
}

pub unsafe fn guest_rect_prepare(
    callbacks: usize,
    out: u32,
    rect: u32,
    level: u32,
    flags: u32,
) -> D3d9GuestRectRequestResult {
    let mut request = D3d9GuestRectRequest {
        level,
        flags,
        full_region: rect == 0,
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let result = (|| {
        let memory = unsafe { memory(callbacks) }?;
        memory
            .validate(GuestAddr::new(out).map_err(memory_error)?, 8)
            .map_err(memory_error)?;
        if rect != 0 {
            let mut bytes = [0u8; 16];
            memory
                .read(GuestAddr::new(rect).map_err(memory_error)?, &mut bytes)
                .map_err(memory_error)?;
            let read = |i: usize| i32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
            request.left = read(0);
            request.top = read(4);
            request.right = read(8);
            request.bottom = read(12);
        }
        Ok(())
    })();
    D3d9GuestRectRequestResult {
        status: done(result),
        request,
    }
}

fn lock_request(request: &D3d9GuestRectRequest) -> LockRequest {
    LockRequest {
        face: 0,
        level: request.level,
        volume: false,
        full_region: request.full_region,
        left: request.left.into(),
        top: request.top.into(),
        front: 0,
        right: request.right.into(),
        bottom: request.bottom.into(),
        back: 0,
        flags: request.flags,
    }
}

pub unsafe fn guest_rect_lock(
    state: &mut GuestTexture,
    resource: &mut D3d9Resource,
    callbacks: usize,
    request: &D3d9GuestRectRequest,
    out: u32,
) -> Status {
    done((|| {
        let mut memory = unsafe { memory(callbacks) }?;
        let out = GuestAddr::new(out).map_err(memory_error)?;
        memory.validate(out, 8).map_err(memory_error)?;
        let mapped = lock_request(request);
        let prepared = d3d9::level_lock_prepare(resource, &mapped)
            .map_err(|error| status(error.kind.status_code(), error.message))?;
        if state
            .staging
            .iter()
            .any(|(level, _)| *level == request.level)
        {
            return Err(invalid("the subresource is already locked"));
        }
        if prepared.total == 0 {
            return Err(invalid("zero texture staging size"));
        }
        state.staging.try_reserve(1).map_err(|_| {
            status(
                StatusCode::OutOfMemory,
                "texture staging table allocation failed",
            )
        })?;
        let allocation = memory
            .allocate(prepared.total, 16)
            .map_err(|error| status(StatusCode::OutOfMemory, error.to_string()))?;
        let result = (|| {
            let source = resource::level_bytes(resource, 0, request.level).map_err(render_error)?;
            if source.len() != prepared.total as usize {
                return Err(invalid("texture staging size does not match the level"));
            }
            memory
                .write(allocation.address(), source)
                .map_err(memory_error)?;
            let pointer = allocation
                .address()
                .checked_add(prepared.offset)
                .map_err(memory_error)?;
            let gpu = resource.gpu_handle().map_err(render_error)?;
            resource::level_lock_commit(&gpu, resource, &mapped).map_err(render_error)?;
            if let Err(error) = memory.write_words(out, &[prepared.pitch, pointer.get()]) {
                resource::rollback_level_lock(resource, 0, request.level, request.flags)
                    .map_err(render_error)?;
                return Err(memory_error(error));
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                state.staging.push((
                    request.level,
                    LevelStaging {
                        allocation,
                        size: prepared.total,
                    },
                ));
                Ok(())
            }
            Err(error) => {
                if let Err(cleanup) = memory.free(allocation) {
                    log::error!("D3D9 texture staging cleanup failed: {cleanup}");
                }
                Err(error)
            }
        }
    })())
}

pub unsafe fn guest_rect_prepare_unlock(
    state: &GuestTexture,
    callbacks: usize,
    level: u32,
) -> Status {
    done((|| {
        let memory = unsafe { memory(callbacks) }?;
        let allocation = state
            .staging
            .iter()
            .find(|(candidate, _)| *candidate == level)
            .map(|(_, staging)| staging)
            .ok_or_else(|| invalid("resource is not locked"))?;
        if allocation.allocation.generation() != memory.generation() {
            return Err(invalid(
                "texture staging belongs to an earlier guest heap generation",
            ));
        }
        Ok(())
    })())
}

pub unsafe fn guest_rect_unlock(
    state: &mut GuestTexture,
    resource: &mut D3d9Resource,
    callbacks: usize,
    level: u32,
) -> Status {
    done((|| {
        let mut memory = unsafe { memory(callbacks) }?;
        let allocation = state
            .staging
            .iter()
            .find(|(candidate, _)| *candidate == level)
            .map(|(_, staging)| staging)
            .ok_or_else(|| invalid("resource is not locked"))?;
        if allocation.allocation.generation() != memory.generation() {
            return Err(invalid(
                "texture staging belongs to an earlier guest heap generation",
            ));
        }
        let size = allocation.size as usize;
        if state.scratch.len() < size {
            state
                .scratch
                .try_reserve_exact(size - state.scratch.len())
                .map_err(|_| {
                    status(StatusCode::OutOfMemory, "texture staging allocation failed")
                })?;
        }
        state.scratch.resize(size, 0);
        memory
            .read(allocation.allocation.address(), &mut state.scratch)
            .map_err(memory_error)?;
        let (result, closed) = d3d9::level_unlock(resource, 0, level, &state.scratch);
        let free_result = if closed {
            let index = state
                .staging
                .iter()
                .position(|(candidate, _)| *candidate == level)
                .expect("texture staging is present");
            memory
                .free(state.staging.remove(index).1.allocation)
                .map_err(memory_error)
        } else {
            Ok(())
        };
        result.map_err(render_error)?;
        free_result
    })())
}

pub unsafe fn guest_texture_release(state: &mut GuestTexture, callbacks: usize) -> Status {
    done((|| {
        let mut memory = unsafe { memory(callbacks) }?;
        let mut failure = None;
        while let Some((_, staging)) = state.staging.pop() {
            if let Err(error) = memory.free(staging.allocation) {
                if failure.is_none() {
                    failure = Some(memory_error(error));
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })())
}

pub unsafe fn guest_surface_desc(
    resource: &D3d9Resource,
    callbacks: usize,
    out: u32,
    level: u32,
    texture_view: bool,
) -> Status {
    done((|| {
        let mut memory = unsafe { memory(callbacks) }?;
        let out = GuestAddr::new(out).map_err(memory_error)?;
        memory.validate(out, 32).map_err(memory_error)?;
        let fields = if texture_view {
            let Some(ResourceState::Texture(texture)) = resource.state.as_ref() else {
                return Err(invalid("D3D9 texture is not initialized"));
            };
            if texture.kind != TextureKind::Texture2D {
                return Err(invalid("resource is not a 2D texture"));
            }
            let (width, height, _, _) =
                resource::level_descriptor(resource, 0, level).map_err(render_error)?;
            [
                texture.format,
                resource.resource_type().unwrap_or(0),
                texture.usage,
                texture.pool,
                0,
                0,
                width,
                height,
            ]
        } else {
            let Some(ResourceState::Surface(surface)) = resource.state.as_ref() else {
                return Err(invalid("D3D9 surface is not initialized"));
            };
            [
                surface.format,
                resource.resource_type().unwrap_or(0),
                surface.usage,
                surface.pool,
                surface.multisample_type,
                surface.multisample_quality,
                surface.width,
                surface.height,
            ]
        };
        memory.write(out, &[0u8; 32]).map_err(memory_error)?;
        if texture_view {
            memory
                .write_words(out, &fields[..4])
                .map_err(memory_error)?;
            memory
                .write_words(out.checked_add(24).map_err(memory_error)?, &fields[6..])
                .map_err(memory_error)
        } else {
            memory.write_words(out, &fields).map_err(memory_error)
        }
    })())
}
