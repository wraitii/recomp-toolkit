use crate::RenderError;
use recomp_gpu::PhysicalLoss;
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
    context: Arc<super::GpuContext>,
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
    device: wgpu::Device,
    submitted: bool,
}
impl NativeSubmission {
    pub(crate) fn new(
        readiness: Arc<NativeReadiness>,
        source: wgpu::Texture,
        destination: wgpu::Texture,
        device: wgpu::Device,
    ) -> Self {
        Self {
            readiness,
            source,
            destination,
            device,
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
        match submit_copy(
            &self.device,
            queue,
            &self.source,
            &self.destination,
            Arc::clone(&self.readiness),
            None,
        ) {
            Ok(()) => self.submitted = true,
            Err(error) => log::error!("present submission failed: {error}"),
        }
    }
}
impl Drop for NativeSubmission {
    fn drop(&mut self) {
        if !self.submitted {
            self.readiness.state.store(2, Ordering::Release);
        }
    }
}
pub(crate) fn retain_slot(
    readiness: Arc<NativeReadiness>,
    context: Arc<super::GpuContext>,
) -> usize {
    Box::into_raw(Box::new(NativeLease { readiness, context })) as usize
}
pub(crate) unsafe fn lease_texture(
    lease: usize,
) -> Option<(Arc<super::GpuContext>, wgpu::Texture)> {
    let lease = unsafe { (lease as *const NativeLease).as_ref() }?;
    Some((
        Arc::clone(&lease.context),
        lease.readiness.slot.texture.clone(),
    ))
}
pub unsafe fn present_lease_ready(lease: usize) -> u32 {
    let Some(lease) = (unsafe { (lease as *const NativeLease).as_ref() }) else {
        return 2;
    };
    if lease
        .readiness
        .loss
        .as_ref()
        .is_some_and(PhysicalLoss::is_lost)
    {
        return 2;
    }
    let _ = lease.context.device.poll(wgpu::PollType::Poll);
    lease.readiness.state.load(Ordering::Acquire)
}
pub unsafe fn release_present_lease(lease: usize) {
    if lease != 0 {
        drop(unsafe { Box::from_raw(lease as *mut NativeLease) });
    }
}
pub(crate) type NativeConverter = wgpu::ComputePipeline;
pub(crate) fn create_converter(device: &wgpu::Device) -> Result<NativeConverter, RenderError> {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("present-rgb565"),
        source: wgpu::ShaderSource::Wgsl(
            r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;
@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) p: vec3<u32>) {
    let size = textureDimensions(dst);
    if p.x >= size.x || p.y >= size.y { return; }
    let bytes = vec3<u32>(textureLoad(src, vec2<i32>(p.xy), 0).rgb * 255.0 + vec3<f32>(0.5));
    let reduced = bytes >> vec3<u32>(3u, 2u, 3u);
    let expanded = reduced * vec3<u32>(255u) / vec3<u32>(31u, 63u, 31u);
    textureStore(dst, vec2<i32>(p.xy), vec4<f32>(vec3<f32>(expanded) / 255.0, 1.0));
}
"#
            .into(),
        ),
    });
    Ok(
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("present-rgb565"),
            layout: None,
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        }),
    )
}
pub(crate) fn submit_copy(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    source: &wgpu::Texture,
    destination: &wgpu::Texture,
    readiness: Arc<NativeReadiness>,
    converter: Option<&NativeConverter>,
) -> Result<(), RenderError> {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("present-copy"),
    });
    if let Some(pipeline) = converter {
        let src = source.create_view(&Default::default());
        let dst = destination.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present-rgb565"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&dst),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(
            destination.width().div_ceil(8),
            destination.height().div_ceil(8),
            1,
        );
    } else {
        encoder.copy_texture_to_texture(
            source.as_image_copy(),
            destination.as_image_copy(),
            wgpu::Extent3d {
                width: destination.width(),
                height: destination.height(),
                depth_or_array_layers: 1,
            },
        );
    }
    queue.submit([encoder.finish()]);
    queue.on_submitted_work_done(move || {
        readiness.state.store(1, Ordering::Release);
    });
    Ok(())
}
