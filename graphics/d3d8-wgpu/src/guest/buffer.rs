use crate::bridge::{
    D3d9BufferKind, D3d9BufferRequest, D3d9GuestBufferCreateResult, Status, StatusCode, status,
};
use crate::d3d9::resource::{BufferLockRequest, D3DLOCK_READONLY, D3d9Resource, ResourceState};
use crate::guest::{GuestAddr, GuestAllocation, GuestMemory, GuestMemoryCallbacks};
use crate::{RenderError, RenderErrorKind};

const RETAIN_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Default)]
pub struct GuestBufferBudget {
    generation: Option<u64>,
    retained: u64,
}

#[derive(Default)]
pub struct GuestBuffer {
    allocation: Option<GuestAllocation>,
    size: u32,
    retain: bool,
    active: bool,
    synchronized: bool,
    writable: bool,
    scratch: Vec<u8>,
}

pub fn new_guest_buffer() -> Box<GuestBuffer> {
    Box::default()
}
pub fn new_guest_buffer_budget() -> Box<GuestBufferBudget> {
    Box::default()
}

fn error(code: StatusCode, message: impl Into<String>) -> Status {
    status(code, message)
}
pub(super) fn memory_error(error: impl core::fmt::Display) -> Status {
    status(StatusCode::InvalidArgument, error.to_string())
}
pub(super) fn render_error(error: RenderError) -> Status {
    let code = match error.kind {
        RenderErrorKind::InvalidArgument => StatusCode::InvalidArgument,
        RenderErrorKind::InvalidData => StatusCode::InvalidData,
        RenderErrorKind::OutOfMemory => StatusCode::OutOfMemory,
        RenderErrorKind::Unsupported => StatusCode::Unsupported,
    };
    let message = if matches!(error.operation, "LockBuffer" | "UnlockBuffer") {
        error.cause
    } else {
        error.to_string()
    };
    status(code, message)
}

pub(super) unsafe fn memory<'a>(callbacks: usize) -> Result<GuestMemory<'a>, Status> {
    let table =
        unsafe { (callbacks as *const GuestMemoryCallbacks).as_ref() }.ok_or_else(|| {
            error(
                StatusCode::InvalidArgument,
                "guest memory callbacks are null",
            )
        })?;
    unsafe { GuestMemory::new(table) }.map_err(memory_error)
}

fn normalize(budget: &mut GuestBufferBudget, generation: u64) {
    if budget.generation != Some(generation) {
        budget.generation = Some(generation);
        budget.retained = 0;
    }
}

fn release_staging(
    buffer: &mut GuestBuffer,
    budget: &mut GuestBufferBudget,
    memory: &mut GuestMemory<'_>,
) -> Result<(), Status> {
    let generation = memory.generation();
    normalize(budget, generation);
    let mut accounting = Ok(());
    let result = if let Some(allocation) = buffer.allocation.take() {
        if allocation.generation() == generation {
            if buffer.retain {
                match budget.retained.checked_sub(u64::from(buffer.size)) {
                    Some(retained) => budget.retained = retained,
                    None => {
                        accounting = Err(error(
                            StatusCode::InvalidArgument,
                            "guest buffer staging budget is inconsistent",
                        ))
                    }
                }
            }
            memory.free(allocation).map_err(memory_error)
        } else {
            Ok(())
        }
    } else {
        Ok(())
    };
    let scratch = std::mem::take(&mut buffer.scratch);
    *buffer = GuestBuffer::default();
    buffer.scratch = scratch;
    if let Err(error) = accounting {
        if let Err(cleanup) = result {
            log::error!("D3D9 buffer staging free failed: {}", cleanup.message);
        }
        return Err(error);
    }
    result
}

fn read_staging(
    memory: &GuestMemory<'_>,
    address: GuestAddr,
    size: u32,
    scratch: &mut Vec<u8>,
) -> Result<(), Status> {
    if scratch.len() < size as usize {
        scratch
            .try_reserve_exact(size as usize - scratch.len())
            .map_err(|_| {
                error(
                    StatusCode::OutOfMemory,
                    "guest buffer staging allocation failed",
                )
            })?;
    }
    scratch.resize(size as usize, 0);
    memory.read(address, scratch).map_err(memory_error)
}

pub unsafe fn guest_buffer_prepare_create(
    callbacks: usize,
    out: u32,
    vertex: bool,
    length: u32,
    usage: u32,
    format_or_fvf: u32,
    pool: u32,
    shared_handle: bool,
) -> D3d9GuestBufferCreateResult {
    let request = D3d9BufferRequest {
        kind: if vertex {
            D3d9BufferKind::Vertex
        } else {
            D3d9BufferKind::Index
        },
        size: length,
        usage,
        pool,
        fvf: if vertex { format_or_fvf } else { 0 },
        format: if vertex { 100 } else { format_or_fvf },
        shared_handle,
    };
    let result = unsafe { memory(callbacks) }.and_then(|mut memory| {
        let address = GuestAddr::new(out).map_err(memory_error)?;
        memory.validate(address, 4).map_err(memory_error)?;
        memory.write_u32(address, 0).map_err(memory_error)
    });
    D3d9GuestBufferCreateResult {
        status: match result {
            Ok(()) => status(StatusCode::Ok, ""),
            Err(error) => error,
        },
        request,
    }
}

pub unsafe fn guest_publish_interface(callbacks: usize, out: u32, view: u32) -> Status {
    match unsafe { memory(callbacks) }.and_then(|mut memory| {
        let address = GuestAddr::new(out).map_err(memory_error)?;
        memory.write_u32(address, view).map_err(memory_error)
    }) {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => error,
    }
}

pub unsafe fn guest_buffer_lock(
    buffer: &mut GuestBuffer,
    budget: &mut GuestBufferBudget,
    resource: &mut D3d9Resource,
    callbacks: usize,
    offset: u32,
    size: u32,
    out: u32,
    flags: u32,
) -> Status {
    let result = (|| {
        let mut memory = unsafe { memory(callbacks) }?;
        let out = GuestAddr::new(out).map_err(memory_error)?;
        memory.validate(out, 4).map_err(memory_error)?;
        let request = BufferLockRequest {
            offset,
            size,
            flags,
        };
        let range = resource
            .prepare_buffer_lock(&request)
            .map_err(render_error)?;
        let generation = memory.generation();
        normalize(budget, generation);
        if buffer
            .allocation
            .as_ref()
            .is_some_and(|allocation| allocation.generation() != generation)
        {
            return Err(error(
                StatusCode::InvalidArgument,
                "buffer staging belongs to an earlier guest heap generation",
            ));
        }
        let fresh = buffer.allocation.is_none();
        if fresh {
            if range.total == 0 {
                return Err(error(
                    StatusCode::InvalidArgument,
                    "zero buffer staging size",
                ));
            }
            let remaining = RETAIN_LIMIT.checked_sub(budget.retained).ok_or_else(|| {
                error(
                    StatusCode::InvalidArgument,
                    "guest buffer staging budget exceeds its limit",
                )
            })?;
            let allocation = memory
                .allocate(range.total, 16)
                .map_err(|error| status(StatusCode::OutOfMemory, error.to_string()))?;
            buffer.allocation = Some(allocation);
            buffer.size = range.total;
            buffer.retain = u64::from(range.total) <= remaining;
            if buffer.retain {
                budget.retained += u64::from(range.total);
            }
        }
        let address = buffer.allocation.as_ref().unwrap().address();
        let starting = !buffer.active;
        let attempt = (|| {
            if starting && !buffer.synchronized {
                let source = resource.buffer_bytes();
                if source.len() != range.total as usize || buffer.size != range.total {
                    return Err(error(
                        StatusCode::InvalidArgument,
                        "buffer staging size does not match the buffer",
                    ));
                }
                memory.write(address, source).map_err(memory_error)?;
            }
            let pointer = address.checked_add(range.offset).map_err(memory_error)?;
            let previous_dirty = match resource.state.as_ref() {
                Some(ResourceState::Buffer(state)) => state.writable_dirty,
                _ => false,
            };
            resource
                .commit_buffer_lock(&request)
                .map_err(render_error)?;
            crate::d3d9::trace_buffer_lock(resource, &range, flags);
            if let Err(error) = memory.write_u32(out, pointer.get()) {
                resource.rollback_buffer_lock(flags, previous_dirty);
                return Err(memory_error(error));
            }
            if starting {
                buffer.active = true;
                buffer.synchronized = false;
                buffer.writable = false;
            }
            if flags & D3DLOCK_READONLY == 0 {
                buffer.writable = true;
            }
            Ok(())
        })();
        if attempt.is_err() && fresh && !buffer.active {
            if let Err(cleanup) = release_staging(buffer, budget, &mut memory) {
                log::error!("D3D9 buffer staging cleanup failed: {}", cleanup.message);
            }
        }
        attempt
    })();
    match result {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => error,
    }
}

pub unsafe fn guest_buffer_unlock(
    buffer: &mut GuestBuffer,
    budget: &mut GuestBufferBudget,
    resource: &mut D3d9Resource,
    callbacks: usize,
) -> Status {
    let result = (|| {
        let mut memory = unsafe { memory(callbacks) }?;
        let Some(allocation) = buffer.allocation.as_ref() else {
            return Ok(());
        };
        if allocation.generation() != memory.generation() {
            return Err(error(
                StatusCode::InvalidArgument,
                "buffer staging belongs to an earlier guest heap generation",
            ));
        }
        let address = allocation.address();
        read_staging(&memory, address, buffer.size, &mut buffer.scratch)?;
        let result = resource.unlock_buffer(&buffer.scratch);
        let cleanup = if result.closed {
            buffer.active = false;
            buffer.synchronized = result.status.is_ok() && buffer.writable;
            buffer.writable = false;
            if !buffer.retain || result.status.is_err() {
                release_staging(buffer, budget, &mut memory)
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        if let Err(error) = result.status {
            if let Err(cleanup) = cleanup {
                log::error!("D3D9 buffer staging cleanup failed: {}", cleanup.message);
            }
            return Err(render_error(error));
        }
        cleanup
    })();
    match result {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => error,
    }
}

pub unsafe fn guest_buffer_sync(
    buffer: &mut GuestBuffer,
    resource: &mut D3d9Resource,
    callbacks: usize,
) -> Status {
    let result = (|| {
        if !buffer.active || !buffer.writable {
            return Ok(());
        }
        let memory = unsafe { memory(callbacks) }?;
        let allocation = buffer.allocation.as_ref().ok_or_else(|| {
            error(
                StatusCode::InvalidArgument,
                "active buffer staging is missing",
            )
        })?;
        if allocation.generation() != memory.generation() {
            return Err(error(
                StatusCode::InvalidArgument,
                "buffer staging belongs to an earlier guest heap generation",
            ));
        }
        let address = allocation.address();
        read_staging(&memory, address, buffer.size, &mut buffer.scratch)?;
        resource
            .sync_locked_buffer(&buffer.scratch)
            .map_err(render_error)
    })();
    match result {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => error,
    }
}

pub unsafe fn guest_buffer_release(
    buffer: &mut GuestBuffer,
    budget: &mut GuestBufferBudget,
    callbacks: usize,
) -> Status {
    match unsafe { memory(callbacks) }
        .and_then(|mut memory| release_staging(buffer, budget, &mut memory))
    {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => error,
    }
}

pub unsafe fn guest_buffer_descriptor(
    resource: &D3d9Resource,
    callbacks: usize,
    out: u32,
    vertex: bool,
) -> Status {
    let result = (|| {
        let mut memory = unsafe { memory(callbacks) }?;
        let out = GuestAddr::new(out).map_err(memory_error)?;
        let size = if vertex { 24 } else { 20 };
        memory.validate(out, size).map_err(memory_error)?;
        let Some(ResourceState::Buffer(buffer)) = resource.state.as_ref() else {
            return Err(error(
                StatusCode::InvalidArgument,
                "D3D9 buffer is not initialized",
            ));
        };
        let fields = [
            buffer.format,
            resource.resource_type().unwrap_or(0),
            buffer.usage,
            buffer.pool,
            buffer.size,
            buffer.fvf,
        ];
        memory
            .write(out, &[0u8; 24][..size as usize])
            .map_err(memory_error)?;
        memory
            .write_words(out, &fields[..size as usize / 4])
            .map_err(memory_error)
    })();
    match result {
        Ok(()) => status(StatusCode::Ok, ""),
        Err(error) => error,
    }
}
