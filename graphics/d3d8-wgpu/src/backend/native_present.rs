use crate::RenderError;
use crate::d3d9::gpu::PhysicalLoss;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) struct NativeSlot {
    pub texture: wgpu::Texture,
    pub busy: AtomicU32,
}

pub(crate) struct NativeReadiness {
    slot: Arc<NativeSlot>,
    state: AtomicU32,
    loss: Option<PhysicalLoss>,
}

impl Drop for NativeReadiness {
    fn drop(&mut self) {
        self.slot.busy.store(0, Ordering::Release);
    }
}

struct NativeLease {
    readiness: Arc<NativeReadiness>,
    _device: wgpu::Device,
    _queue: wgpu::Queue,
    _context: Option<Arc<super::GpuContext>>,
}

struct Completion {
    producer: std::sync::Mutex<Option<(Arc<NativeReadiness>, wgpu::Texture)>>,
}

impl Completion {
    fn finish(&self, success: bool) {
        let producer = self
            .producer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some((readiness, _source)) = producer {
            if !success {
                if let Some(loss) = &readiness.loss {
                    loss.mark("native present copy failed".into());
                }
            }
            let _ = readiness.state.compare_exchange(
                0,
                if success { 1 } else { 2 },
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some((readiness, _source)) = self
            .producer
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = readiness
                .state
                .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

pub(crate) fn begin_slot(
    slot: Arc<NativeSlot>,
    loss: Option<PhysicalLoss>,
) -> Arc<NativeReadiness> {
    slot.busy.store(1, Ordering::Release);
    Arc::new(NativeReadiness {
        slot,
        state: AtomicU32::new(0),
        loss,
    })
}

pub(crate) struct NativeSubmission {
    readiness: Arc<NativeReadiness>,
    source: wgpu::Texture,
    destination: wgpu::Texture,
    submitted: bool,
}

impl NativeSubmission {
    pub(crate) fn new(
        readiness: Arc<NativeReadiness>,
        source: wgpu::Texture,
        destination: wgpu::Texture,
    ) -> Self {
        Self {
            readiness,
            source,
            destination,
            submitted: false,
        }
    }

    pub(crate) fn submit(mut self, queue: &wgpu::Queue) {
        if self
            .readiness
            .loss
            .as_ref()
            .is_some_and(PhysicalLoss::is_lost)
        {
            return;
        }
        queue.submit(std::iter::empty());
        match submit_copy(
            queue,
            &self.source,
            &self.destination,
            Arc::clone(&self.readiness),
            None,
        ) {
            Ok(()) => self.submitted = true,
            Err(error) => log::error!("D3D9 native present submission failed: {error}"),
        }
    }
}

impl Drop for NativeSubmission {
    fn drop(&mut self) {
        if !self.submitted {
            let _ =
                self.readiness
                    .state
                    .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

pub(crate) fn retain_slot(
    readiness: Arc<NativeReadiness>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    context: Option<Arc<super::GpuContext>>,
) -> usize {
    Box::into_raw(Box::new(NativeLease {
        readiness,
        _device: device.clone(),
        _queue: queue.clone(),
        _context: context,
    })) as usize
}

pub unsafe fn present_lease_ready(lease: usize) -> u32 {
    if lease == 0 {
        return 2;
    }
    let lease = unsafe { &*(lease as *const NativeLease) };
    if lease
        .readiness
        .loss
        .as_ref()
        .is_some_and(PhysicalLoss::is_lost)
    {
        2
    } else {
        lease.readiness.state.load(Ordering::Acquire)
    }
}

pub unsafe fn release_present_lease(lease: usize) {
    if lease != 0 {
        drop(unsafe { Box::from_raw(lease as *mut NativeLease) });
    }
}

#[cfg(target_os = "macos")]
pub(crate) type NativeConverter =
    objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLComputePipelineState>>;
#[cfg(not(target_os = "macos"))]
pub(crate) type NativeConverter = ();

#[cfg(target_os = "macos")]
pub(crate) fn create_converter(device: &wgpu::Device) -> Result<NativeConverter, RenderError> {
    use objc2_foundation::NSString;
    use objc2_metal::{MTLDevice, MTLLibrary};
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .ok_or_else(|| RenderError::new("PresentHandoff", "the device is not a Metal device"))?;
    let source = NSString::from_str(
        r#"#include <metal_stdlib>
using namespace metal;
kernel void convert(texture2d<float, access::read> src [[texture(0)]], texture2d<float, access::write> dst [[texture(1)]], uint2 p [[thread_position_in_grid]]) {
    if (p.x >= dst.get_width() || p.y >= dst.get_height()) return;
    uint3 bytes = uint3(src.read(p).rgb * 255.0f + 0.5f);
    uint3 reduced = bytes >> uint3(3, 2, 3);
    uint3 expanded = reduced * uint3(255) / uint3(31, 63, 31);
    dst.write(float4(float3(expanded) / 255.0f, 1.0f), p);
}"#,
    );
    let library = hal
        .raw_device()
        .newLibraryWithSource_options_error(&source, None)
        .map_err(|e| RenderError::new("PresentHandoff", e.to_string()))?;
    let function = library
        .newFunctionWithName(&NSString::from_str("convert"))
        .ok_or_else(|| {
            RenderError::new("PresentHandoff", "RGB565 conversion function unavailable")
        })?;
    hal.raw_device()
        .newComputePipelineStateWithFunction_error(&function)
        .map_err(|e| RenderError::new("PresentHandoff", e.to_string()))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn create_converter(_: &wgpu::Device) -> Result<NativeConverter, RenderError> {
    Err(RenderError::new(
        "PresentHandoff",
        "native texture handoff is unavailable on this backend",
    ))
}

#[cfg(target_os = "macos")]
pub(crate) fn submit_copy(
    queue: &wgpu::Queue,
    source: &wgpu::Texture,
    destination: &wgpu::Texture,
    readiness: Arc<NativeReadiness>,
    converter: Option<&NativeConverter>,
) -> Result<(), RenderError> {
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{
        MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
        MTLCommandQueue, MTLComputeCommandEncoder, MTLOrigin, MTLSize,
    };
    let queue_hal = unsafe { queue.as_hal::<wgpu::hal::api::Metal>() }
        .ok_or_else(|| RenderError::new("PresentHandoff", "the queue is not a Metal queue"))?;
    let src_hal = unsafe { source.as_hal::<wgpu::hal::api::Metal>() }
        .ok_or_else(|| RenderError::new("PresentHandoff", "source is not a Metal texture"))?;
    let dst_hal = unsafe { destination.as_hal::<wgpu::hal::api::Metal>() }
        .ok_or_else(|| RenderError::new("PresentHandoff", "destination is not a Metal texture"))?;
    objc2::rc::autoreleasepool(|_| {
        let command = queue_hal.as_raw().commandBuffer().ok_or_else(|| {
            RenderError::new("PresentHandoff", "Metal command buffer allocation failed")
        })?;
        let completion = Completion {
            producer: std::sync::Mutex::new(Some((readiness, source.clone()))),
        };
        let handler = block2::RcBlock::new(
            move |command: std::ptr::NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                completion.finish(unsafe {
                    command.as_ref().status() == MTLCommandBufferStatus::Completed
                });
            },
        );
        unsafe {
            command.addCompletedHandler(block2::RcBlock::as_ptr(&handler));
        }
        let src = src_hal.raw_handle();
        let dst = dst_hal.raw_handle();
        if let Some(pipeline) = converter {
            let encoder = command.computeCommandEncoder().ok_or_else(|| {
                RenderError::new("PresentHandoff", "Metal compute encoder allocation failed")
            })?;
            encoder.setComputePipelineState(pipeline);
            unsafe {
                encoder.setTexture_atIndex(Some(src), 0);
                encoder.setTexture_atIndex(Some(dst), 1);
            }
            encoder.dispatchThreads_threadsPerThreadgroup(
                MTLSize {
                    width: destination.width() as usize,
                    height: destination.height() as usize,
                    depth: 1,
                },
                MTLSize {
                    width: 8,
                    height: 8,
                    depth: 1,
                },
            );
            encoder.endEncoding();
        } else {
            let encoder = command.blitCommandEncoder().ok_or_else(|| {
                RenderError::new("PresentHandoff", "Metal blit encoder allocation failed")
            })?;
            let origin = MTLOrigin { x: 0, y: 0, z: 0 };
            unsafe {
                encoder.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(src, 0, 0, origin, MTLSize { width: destination.width() as usize, height: destination.height() as usize, depth: 1 }, dst, 0, 0, origin);
            }
            encoder.endEncoding();
        }
        command.commit();
        Ok(())
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn submit_copy(
    _: &wgpu::Queue,
    _: &wgpu::Texture,
    _: &wgpu::Texture,
    _: Arc<NativeReadiness>,
    _: Option<&NativeConverter>,
) -> Result<(), RenderError> {
    Err(RenderError::new(
        "PresentHandoff",
        "native texture handoff is unavailable on this backend",
    ))
}
