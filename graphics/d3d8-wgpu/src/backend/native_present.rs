use crate::RenderError;

#[cfg(target_os = "macos")]
pub(crate) type ReadyEvent =
    objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLSharedEvent>>;
#[cfg(not(target_os = "macos"))]
pub(crate) type ReadyEvent = ();

#[cfg(target_os = "macos")]
pub(crate) fn create_ready_event(device: &wgpu::Device) -> Result<ReadyEvent, RenderError> {
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
        .ok_or_else(|| RenderError::new("PresentHandoff", "the device is not a Metal device"))?;
    use objc2_metal::MTLDevice;
    hal.raw_device()
        .newSharedEvent()
        .ok_or_else(|| RenderError::new("PresentHandoff", "Metal shared-event allocation failed"))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn create_ready_event(_: &wgpu::Device) -> Result<ReadyEvent, RenderError> {
    Err(RenderError::new(
        "PresentHandoff",
        "native GPU synchronization is unavailable on this backend",
    ))
}

#[cfg(target_os = "macos")]
pub(crate) fn signal_ready(
    queue: &wgpu::Queue,
    event: &ReadyEvent,
    value: u64,
) -> Result<*mut u8, RenderError> {
    let hal = unsafe { queue.as_hal::<wgpu::hal::api::Metal>() }
        .ok_or_else(|| RenderError::new("PresentHandoff", "the queue is not a Metal queue"))?;
    use objc2_metal::{MTLCommandBuffer, MTLCommandQueue};
    objc2::rc::autoreleasepool(|_| {
        let command = hal.as_raw().commandBuffer().ok_or_else(|| {
            RenderError::new(
                "PresentHandoff",
                "Metal signal command-buffer allocation failed",
            )
        })?;
        command.encodeSignalEvent_value((**event).as_ref(), value);
        command.commit();
        Ok::<_, RenderError>(())
    })?;
    Ok(ready_event_handle(event))
}

#[cfg(target_os = "macos")]
pub(crate) fn ready_event_handle(event: &ReadyEvent) -> *mut u8 {
    objc2::rc::Retained::as_ptr(event).cast_mut().cast()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn ready_event_handle(_: &ReadyEvent) -> *mut u8 {
    std::ptr::null_mut()
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn signal_ready(
    _: &wgpu::Queue,
    _: &ReadyEvent,
    _: u64,
) -> Result<*mut u8, RenderError> {
    Err(RenderError::new(
        "PresentHandoff",
        "native GPU synchronization is unavailable on this backend",
    ))
}
