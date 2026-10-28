use crate::RenderError;

pub(crate) fn marker(
    device: &wgpu::Device,
    completed: impl Fn(bool) + Send + Sync + 'static,
) -> Result<Option<wgpu::CommandEncoder>, RenderError> {
    #[cfg(target_os = "macos")]
    unsafe {
        use objc2::runtime::ProtocolObject;
        use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus};

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("d3d9-event-query-marker"),
        });
        let registered = encoder.as_hal_mut::<wgpu::hal::api::Metal, _, _>(move |hal| {
            let Some(hal) = hal else {
                return Ok(false);
            };
            let command = hal.raw_command_buffer().ok_or_else(|| {
                RenderError::new("Issue", "Metal query command buffer is missing")
            })?;
            let handler = block2::RcBlock::new(
                move |command: std::ptr::NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                    completed(command.as_ref().status() == MTLCommandBufferStatus::Completed);
                },
            );
            command.addCompletedHandler(block2::RcBlock::as_ptr(&handler));
            Ok(true)
        })?;
        Ok(registered.then_some(encoder))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (device, completed);
        Ok(None)
    }
}
