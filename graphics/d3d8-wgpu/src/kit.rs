use crate::{
    RenderError,
    backend::GpuContext,
    host_gpu::{HostGpu, guard},
};
use std::{ffi::c_void, sync::Arc};

pub(crate) fn instance() -> &'static wgpu::Instance {
    static INSTANCE: std::sync::OnceLock<wgpu::Instance> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(wgpu::Instance::default)
}

pub(crate) fn context() -> Result<Arc<GpuContext>, RenderError> {
    static CONTEXT: std::sync::Mutex<std::sync::Weak<GpuContext>> =
        std::sync::Mutex::new(std::sync::Weak::new());
    let mut context = CONTEXT.lock().unwrap();
    if let Some(gpu) = context.upgrade() {
        if gpu.physical_loss().is_lost() {
            return Err(RenderError::new(
                "kit_context",
                "physical device lost; reconstruct all clients",
            ));
        }
        return Ok(gpu);
    }
    let gpu = pollster::block_on(async {
        let adapter = instance()
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map_err(|e| RenderError::new("kit_context::request_adapter", e.to_string()))?;
        let requested = wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER
            | wgpu::Features::TEXTURE_COMPRESSION_BC
            | wgpu::Features::TEXTURE_COMPRESSION_BC_SLICED_3D
            | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
            | wgpu::Features::POLYGON_MODE_LINE
            | wgpu::Features::POLYGON_MODE_POINT;
        GpuContext::from_adapter(
            adapter,
            None,
            #[cfg(feature = "windowing")]
            None,
            requested,
        )
        .await
    })?;
    let gpu = Arc::new(gpu);
    *context = Arc::downgrade(&gpu);
    Ok(gpu)
}

pub(crate) struct QueueOwner(Arc<GpuContext>);
impl QueueOwner {
    pub(crate) fn with<T>(&self, submit: impl FnOnce(&wgpu::Queue) -> T) -> T {
        self.0.with_synchronized_queue(submit)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn host_gpu_create() -> *mut HostGpu {
    guard("create", || match context() {
        Ok(renderer) => Box::into_raw(Box::new(HostGpu::new(
            renderer.shared_context(),
            QueueOwner(renderer),
        ))),
        Err(error) => {
            eprintln!("host GPU create: {error}");
            std::ptr::null_mut()
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_texture_import_lease(
    gpu: *mut HostGpu,
    lease: *mut c_void,
) -> u64 {
    guard("texture_import_lease", || {
        let Some(gpu) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        let Some((renderer, texture)) =
            (unsafe { crate::backend::native_present::lease_texture(lease as usize) })
        else {
            eprintln!("host GPU texture_import_lease: invalid producer lease");
            return 0;
        };
        gpu.import_texture(&renderer.shared_context(), texture)
    })
}
