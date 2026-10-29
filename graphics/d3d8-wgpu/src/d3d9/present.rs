use super::gpu::{D3d9Gpu, PhysicalLoss};
use crate::RenderError;
use crate::backend::OffscreenTarget;
use crate::bridge::FrameHandoff;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::backend::native_present::{
    ReadyEvent, create_ready_event, ready_event_handle, signal_ready,
};

pub(crate) struct PresentRing {
    context: PhysicalLoss,
    ready_event: ReadyEvent,
    ready_value: u64,
    textures: Vec<wgpu::Texture>,
    busy: Vec<Box<AtomicU32>>,
    next: usize,
}

impl Drop for PresentRing {
    fn drop(&mut self) {
        for busy in std::mem::take(&mut self.busy) {
            if let Err(error) = wait_slot(&busy) {
                log::error!("{error}");
                Box::leak(busy);
            }
        }
    }
}

fn wait_slot(busy: &AtomicU32) -> Result<(), RenderError> {
    let start = std::time::Instant::now();
    while busy.load(Ordering::Acquire) != 0 {
        if start.elapsed() > std::time::Duration::from_secs(2) {
            return Err(RenderError::new(
                "D3D9::PresentHandoff",
                "host never released a presented frame slot",
            ));
        }
        std::thread::yield_now();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_texture(texture: &wgpu::Texture) -> Result<*mut u8, RenderError> {
    let hal = unsafe { texture.as_hal::<wgpu::hal::api::Metal>() }.ok_or_else(|| {
        RenderError::new("D3D9::PresentHandoff", "the texture is not a Metal texture")
    })?;
    let raw = hal.raw_handle();
    Ok(std::ptr::from_ref(raw).cast_mut().cast())
}

#[cfg(not(target_os = "macos"))]
fn native_texture(_: &wgpu::Texture) -> Result<*mut u8, RenderError> {
    Err(RenderError::new(
        "D3D9::PresentHandoff",
        "native texture handoff is unavailable on this backend",
    ))
}

pub(crate) fn handoff(
    slot: &mut Option<PresentRing>,
    gpu: &D3d9Gpu,
    target: &OffscreenTarget,
) -> Result<FrameHandoff, RenderError> {
    native_texture(&target.texture)?;
    let frame = gpu
        .validate("PresentHandoff", || prepare_handoff(slot, gpu, target))
        .map_err(|error| {
            if error.kind == crate::RenderErrorKind::Unsupported {
                RenderError::invalid_data("PresentHandoff", error.to_string())
            } else {
                error
            }
        })?;
    unsafe { &*frame.busy.cast::<AtomicU32>() }.store(1, Ordering::Release);
    Ok(frame)
}

fn prepare_handoff(
    slot: &mut Option<PresentRing>,
    gpu: &D3d9Gpu,
    target: &OffscreenTarget,
) -> Result<FrameHandoff, RenderError> {
    let recreate = slot.as_ref().is_none_or(|ring| {
        !ring.context.same_context(&gpu.loss().physical_loss())
            || ring.textures[0].width() != target.width
            || ring.textures[0].height() != target.height
            || ring.textures[0].format() != target.format
    });
    if recreate {
        if let Some(ring) = slot.as_ref() {
            for busy in &ring.busy {
                wait_slot(busy)?;
            }
        }
        *slot = Some(PresentRing {
            context: gpu.loss().physical_loss(),
            ready_event: create_ready_event(&gpu.device)?,
            ready_value: 0,
            textures: (0..3)
                .map(|_| {
                    gpu.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("d3d9-present-ring"),
                        size: wgpu::Extent3d {
                            width: target.width,
                            height: target.height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: target.format,
                        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    })
                })
                .collect(),
            busy: (0..3).map(|_| Box::new(AtomicU32::new(0))).collect(),
            next: 0,
        });
    }
    let ring = slot.as_mut().expect("present ring is initialized");
    let index = ring.next;
    let texture = native_texture(&ring.textures[index])?;
    wait_slot(&ring.busy[index])?;
    let ready_value = ring.ready_value.checked_add(1).ok_or_else(|| {
        RenderError::new(
            "D3D9::PresentHandoff",
            "native GPU synchronization value overflow",
        )
    })?;
    gpu.record(|encoder, _| {
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &ring.textures[index],
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: target.width,
                height: target.height,
                depth_or_array_layers: 1,
            },
        );
    });
    let event = ring.ready_event.clone();
    gpu.submit_then(move |queue| {
        if let Err(error) = signal_ready(queue, &event, ready_value) {
            log::error!("{error}");
        }
    });
    let ready_event = ready_event_handle(&ring.ready_event);
    ring.ready_value = ready_value;
    gpu.device
        .poll(wgpu::PollType::Poll)
        .map_err(|error| RenderError::new("D3D9::PresentHandoff", error.to_string()))?;
    ring.next = (index + 1) % ring.textures.len();
    Ok(FrameHandoff {
        ready_event,
        ready_value,
        texture,
        busy: ring.busy[index].as_ref() as *const AtomicU32 as *mut u32,
        width: target.width,
        height: target.height,
    })
}
