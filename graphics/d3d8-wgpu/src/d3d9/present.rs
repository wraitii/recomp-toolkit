use super::gpu::{D3d9Gpu, PhysicalLoss};
use crate::RenderError;
use crate::backend::{GpuContext, OffscreenTarget};
use crate::bridge::FrameHandoff;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::backend::native_present::{
    NativeReadiness, NativeSlot, NativeSubmission, begin_slot, retain_slot,
};

pub(crate) struct PresentRing {
    context: PhysicalLoss,
    slots: Vec<Arc<NativeSlot>>,
    next: usize,
}

fn wait_slot(busy: &AtomicU32, loss: &PhysicalLoss) -> Result<(), RenderError> {
    let start = std::time::Instant::now();
    while busy.load(Ordering::Acquire) != 0 {
        if loss.is_lost() {
            return Err(RenderError::new(
                "D3D9::PresentHandoff",
                "physical device lost while waiting for a frame slot",
            ));
        }
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

pub(crate) fn handoff(
    slot: &mut Option<PresentRing>,
    gpu: &D3d9Gpu,
    target: &OffscreenTarget,
    context: &Arc<GpuContext>,
) -> Result<FrameHandoff, RenderError> {
    let frame = gpu
        .validate("PresentHandoff", || prepare_handoff(slot, gpu, target))
        .map_err(|error| {
            if error.kind == crate::RenderErrorKind::Unsupported {
                RenderError::invalid_data("PresentHandoff", error.to_string())
            } else {
                error
            }
        })?;
    let (mut frame, readiness) = frame;
    frame.lease = retain_slot(readiness, Arc::clone(context));
    frame.texture = frame.lease as *mut u8;
    Ok(frame)
}

fn prepare_handoff(
    slot: &mut Option<PresentRing>,
    gpu: &D3d9Gpu,
    target: &OffscreenTarget,
) -> Result<(FrameHandoff, Arc<NativeReadiness>), RenderError> {
    let recreate = slot.as_ref().is_none_or(|ring| {
        !ring.context.same_context(&gpu.loss().physical_loss())
            || ring.slots[0].texture.width() != target.width
            || ring.slots[0].texture.height() != target.height
            || ring.slots[0].texture.format() != target.format
    });
    if recreate {
        *slot = Some(PresentRing {
            context: gpu.loss().physical_loss(),
            slots: (0..3)
                .map(|_| {
                    Arc::new(NativeSlot {
                        busy: AtomicU32::new(0),
                        texture: gpu.device.create_texture(&wgpu::TextureDescriptor {
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
                            usage: wgpu::TextureUsages::COPY_DST
                                | wgpu::TextureUsages::COPY_SRC
                                | wgpu::TextureUsages::TEXTURE_BINDING,
                            view_formats: &[],
                        }),
                    })
                })
                .collect(),
            next: 0,
        });
    }
    let ring = slot.as_mut().expect("present ring is initialized");
    let index = ring.next;
    let slot = Arc::clone(&ring.slots[index]);
    wait_slot(&slot.busy, &gpu.loss().physical_loss())?;
    let readiness = begin_slot(Arc::clone(&slot), Some(gpu.loss().physical_loss()));
    if gpu.is_physical() {
        return Err(RenderError::new(
            "D3D9::PresentHandoff",
            gpu.loss().physical_message(),
        ));
    }
    let submission = NativeSubmission::new(
        Arc::clone(&readiness),
        target.texture.clone(),
        slot.texture.clone(),
        gpu.device.clone(),
    );
    gpu.submit_then(move |queue| submission.submit(queue));
    ring.next = (index + 1) % ring.slots.len();
    Ok((
        FrameHandoff {
            texture: std::ptr::null_mut(),
            lease: 0,
            width: target.width,
            height: target.height,
        },
        readiness,
    ))
}
