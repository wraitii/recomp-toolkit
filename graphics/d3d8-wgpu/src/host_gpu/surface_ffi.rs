use super::*;
use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct PendingPresent {
    pub chain: u64,
    pub cancelled: Arc<AtomicBool>,
    pub frame: recomp_compositor::surface::SurfaceFrame,
    pub callback: Option<extern "C" fn(*mut c_void, f64)>,
    pub userdata: usize,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_swapchain_create(
    gpu: *mut HostGpu,
    native: *mut c_void,
    width: i32,
    height: i32,
) -> u64 {
    guard("swapchain_create", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        if width < 0 || height < 0 {
            report("swapchain_create dimensions");
            return 0;
        }
        let context = Arc::clone(&g.context);
        let surface = unsafe {
            surface::SurfaceState::new(
                crate::kit::instance(),
                context,
                native,
                width as u32,
                height as u32,
            )
        };
        match surface {
            Ok(surface) => {
                let mut state = g.state.lock().unwrap();
                let id = state.id();
                state.chains.insert(id, Arc::new(Mutex::new(surface)));
                id
            }
            Err(error) => {
                eprintln!("host GPU swapchain_create: {error}");
                0
            }
        }
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_swapchain_resize(
    gpu: *mut HostGpu,
    chain: u64,
    width: i32,
    height: i32,
) {
    guard("swapchain_resize", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        if width < 0 || height < 0 {
            report("swapchain_resize dimensions");
            return;
        }
        let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
        let Some(surface) = surface else {
            report("swapchain_resize handle");
            return;
        };
        surface
            .lock()
            .unwrap()
            .resize(&g.context.device, width as u32, height as u32);
    });
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_swapchain_format(gpu: *mut HostGpu, chain: u64) -> i32 {
    guard("swapchain_format", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return -1;
        };
        let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
        let format = surface
            .as_ref()
            .map(|surface| surface.lock().unwrap().format());
        match format {
            Some(wgpu::TextureFormat::Bgra8Unorm) => 0,
            Some(wgpu::TextureFormat::Rgba8Unorm) => 1,
            _ => {
                report("swapchain_format");
                -1
            }
        }
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_swapchain_acquire(gpu: *mut HostGpu, chain: u64) -> u64 {
    guard("swapchain_acquire", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
        let Some(surface) = surface else {
            report("swapchain_acquire handle");
            return 0;
        };
        let mut surface = surface.lock().unwrap();
        if surface.cancelled.load(Ordering::Acquire) {
            return 0;
        }
        let texture = match surface.acquire(&g.context.device) {
            Ok(Some(texture)) => texture,
            Ok(None) => return 0,
            Err(error) => {
                eprintln!("host GPU swapchain_acquire: {error}");
                return 0;
            }
        };
        let format = match texture.format() {
            wgpu::TextureFormat::Bgra8Unorm => 0,
            wgpu::TextureFormat::Rgba8Unorm => 1,
            _ => {
                surface.release();
                report("swapchain_acquire format");
                return 0;
            }
        };
        let desc = TextureDesc {
            width: texture.width() as i32,
            height: texture.height() as i32,
            format,
            usage: 2,
            mip_levels: 1,
        };
        let mut state = g.state.lock().unwrap();
        let id = state.id();
        state.textures.insert(
            id,
            Texture {
                texture,
                desc,
                allocated: desc.width as u64 * desc.height as u64 * 4,
            },
        );
        surface.drawable = Some(id);
        id
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_swapchain_release(gpu: *mut HostGpu, chain: u64, drawable: u64) {
    guard("swapchain_release", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
        let Some(surface) = surface else {
            report("swapchain_release handle");
            return;
        };
        let mut surface = surface.lock().unwrap();
        if surface.drawable != Some(drawable) {
            report("swapchain_release drawable");
            return;
        }
        surface.drawable = None;
        g.state.lock().unwrap().textures.remove(&drawable);
        surface.release();
    });
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_swapchain_present(
    gpu: *mut HostGpu,
    command: u64,
    chain: u64,
    drawable: u64,
    _duration: f64,
    callback: Option<extern "C" fn(*mut c_void, f64)>,
    userdata: *mut c_void,
) {
    guard("swapchain_present", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            if let Some(f) = callback {
                f(userdata, 0.0)
            }
            return;
        };
        let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
        let Some(surface) = surface else {
            report("swapchain_present handle");
            if let Some(f) = callback {
                f(userdata, 0.0)
            }
            return;
        };
        let mut surface = surface.lock().unwrap();
        let mut state = g.state.lock().unwrap();
        let valid = !surface.cancelled.load(Ordering::Acquire)
            && surface.drawable == Some(drawable)
            && state
                .commands
                .get(&command)
                .is_some_and(|c| c.encoder.is_some());
        if !valid {
            drop(state);
            drop(surface);
            report("swapchain_present handle");
            if let Some(f) = callback {
                f(userdata, 0.0)
            }
            return;
        }
        let frame = surface.take_frame().unwrap();
        let cancelled = Arc::clone(&surface.cancelled);
        state.textures.remove(&drawable);
        state
            .commands
            .get_mut(&command)
            .unwrap()
            .presents
            .push(PendingPresent {
                chain,
                cancelled,
                frame,
                callback,
                userdata: userdata as usize,
            });
    });
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_swapchain_destroy(gpu: *mut HostGpu, chain: u64) {
    guard("swapchain_destroy", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
        let Some(surface) = surface else { return };
        let mut surface = surface.lock().unwrap();
        surface.cancel();
        let mut state = g.state.lock().unwrap();
        state.chains.remove(&chain);
        if let Some(drawable) = surface.drawable {
            state.textures.remove(&drawable);
        }
        let mut callbacks = Vec::new();
        for command in state.commands.values_mut() {
            let mut index = 0;
            while index < command.presents.len() {
                if command.presents[index].chain == chain {
                    let present = command.presents.remove(index);
                    if let Some(callback) = present.callback {
                        callbacks.push((callback, present.userdata));
                    }
                } else {
                    index += 1;
                }
            }
        }
        drop(state);
        drop(surface);
        for (callback, userdata) in callbacks {
            callback(userdata as *mut c_void, 0.0);
        }
    });
}

pub(super) fn present_finished(g: &HostGpu, chain: u64) {
    let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
    if let Some(surface) = surface {
        surface.lock().unwrap().finish(&g.context.device);
    }
}

pub(super) fn present_frame(
    g: &HostGpu,
    chain: u64,
    frame: recomp_compositor::surface::SurfaceFrame,
) {
    let surface = g.state.lock().unwrap().chains.get(&chain).cloned();
    if let Some(surface) = surface {
        surface.lock().unwrap().present(frame);
    }
}
