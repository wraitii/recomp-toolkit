use super::format;
use crate::RenderError;
use crate::backend::GpuContext;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

pub const COPY_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
const STAGING_CHUNK: u64 = 1 << 20;

fn align_up(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

#[derive(Clone, Default)]
pub struct D3d9Loss {
    physical: PhysicalLoss,
    reset_required: Arc<AtomicBool>,
}

pub use recomp_gpu::PhysicalLoss;

impl D3d9Loss {
    pub(crate) fn physical_loss(&self) -> PhysicalLoss {
        self.physical.clone()
    }
    pub fn from_physical(physical: PhysicalLoss) -> Self {
        Self {
            physical,
            reset_required: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn mark_physical(&self, reason: String) {
        self.physical.mark(reason);
    }

    pub fn is_physical(&self) -> bool {
        self.physical.is_lost()
    }

    pub fn mark_reset_required(&self) {
        self.reset_required.store(true, Ordering::Release);
    }

    pub fn is_reset_required(&self) -> bool {
        self.reset_required.load(Ordering::Acquire)
    }

    pub fn is_blocked(&self) -> bool {
        self.is_physical() || self.is_reset_required()
    }

    pub fn clear_reset_required(&self) {
        self.reset_required.store(false, Ordering::Release);
    }

    pub fn reason(&self) -> Option<String> {
        self.physical.reason()
    }

    pub fn physical_message(&self) -> String {
        let reason = self
            .reason()
            .unwrap_or_else(|| "reason unknown".to_string());
        format!("the physical wgpu device was lost ({reason})")
    }
}

pub(crate) struct PendingClear {
    pub color: Option<(wgpu::TextureView, wgpu::Color)>,
    pub depth: Option<(
        wgpu::TextureView,
        wgpu::Operations<f32>,
        Option<wgpu::Operations<u32>>,
    )>,
}

impl PendingClear {
    pub(crate) fn folds_into(
        &self,
        color_view: &wgpu::TextureView,
        depth: Option<&(wgpu::TextureView, bool)>,
    ) -> bool {
        self.color
            .as_ref()
            .is_none_or(|(view, _)| view == color_view)
            && self.depth.as_ref().is_none_or(|(view, _, stencil)| {
                depth.is_some_and(|(batch_view, has_stencil)| {
                    batch_view == view && *has_stencil == stencil.is_some()
                })
            })
    }

    fn record(&self, encoder: &mut wgpu::CommandEncoder) {
        let color = self
            .color
            .as_ref()
            .map(|(view, value)| wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(*value),
                    store: wgpu::StoreOp::Store,
                },
            });
        let colors = [color];
        let colors: &[Option<wgpu::RenderPassColorAttachment>] =
            if self.color.is_some() { &colors } else { &[] };
        let depth = self.depth.as_ref().map(|(view, depth_ops, stencil_ops)| {
            wgpu::RenderPassDepthStencilAttachment {
                view,
                depth_ops: Some(*depth_ops),
                stencil_ops: *stencil_ops,
            }
        });
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("d3d9-clear"),
            color_attachments: colors,
            depth_stencil_attachment: depth,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
}

enum SubmitJob {
    Frame(wgpu::CommandEncoder, wgpu::util::StagingBelt),
    After(Box<dyn FnOnce(&wgpu::Queue) + Send>),
    Sync(mpsc::Sender<()>),
}

struct Submitter {
    jobs: Option<mpsc::Sender<SubmitJob>>,
    belts: mpsc::Receiver<wgpu::util::StagingBelt>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Submitter {
    fn new(queue: wgpu::Queue, physical: PhysicalLoss) -> Self {
        let (jobs, received) = mpsc::channel::<SubmitJob>();
        let (returned, belts) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("d3d9-submit".into())
            .spawn(move || {
                for job in received {
                    match job {
                        SubmitJob::Frame(encoder, mut staging) => {
                            if !physical.is_lost() {
                                queue.submit([encoder.finish()]);
                                staging.recall();
                            }
                            let _ = returned.send(staging);
                        }
                        SubmitJob::After(work) => work(&queue),
                        SubmitJob::Sync(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("failed to start the D3D9 submit thread");
        Self {
            jobs: Some(jobs),
            belts,
            thread: Some(thread),
        }
    }

    fn send(&self, job: SubmitJob) {
        self.jobs
            .as_ref()
            .expect("D3D9 submit queue is open")
            .send(job)
            .expect("D3D9 submit thread exited");
    }

    fn sync(&self) {
        let (done, wait) = mpsc::channel();
        self.send(SubmitJob::Sync(done));
        wait.recv().expect("D3D9 submit thread exited");
    }
}

impl Drop for Submitter {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct PendingWork {
    device: wgpu::Device,
    physical: PhysicalLoss,
    encoder: Option<wgpu::CommandEncoder>,
    staging: wgpu::util::StagingBelt,
    clear: Option<PendingClear>,
    submitter: Submitter,
    flush_pending: Arc<AtomicBool>,
}

fn frame_encoder<'a>(
    device: &wgpu::Device,
    encoder: &'a mut Option<wgpu::CommandEncoder>,
) -> &'a mut wgpu::CommandEncoder {
    encoder.get_or_insert_with(|| {
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("d3d9-frame"),
        })
    })
}

impl PendingWork {
    pub(crate) fn synchronize(&mut self) {
        self.submit();
        self.submitter.sync();
    }
    fn apply_clear(&mut self) {
        if let Some(clear) = self.clear.take() {
            clear.record(frame_encoder(&self.device, &mut self.encoder));
        }
    }

    fn submit(&mut self) {
        if self.physical.is_lost() {
            self.clear = None;
            self.encoder = None;
            return;
        }
        self.apply_clear();
        let Some(encoder) = self.encoder.take() else {
            return;
        };
        self.staging.finish();
        let next =
            self.submitter.belts.try_recv().unwrap_or_else(|_| {
                wgpu::util::StagingBelt::new(self.device.clone(), STAGING_CHUNK)
            });
        let staging = std::mem::replace(&mut self.staging, next);
        self.submitter.send(SubmitJob::Frame(encoder, staging));
    }
}

impl Drop for PendingWork {
    fn drop(&mut self) {
        self.submit();
    }
}

pub struct D3d9Gpu {
    pub device: wgpu::Device,
    pub(crate) backend: wgpu::Backend,
    pub(crate) shader_mode: Option<String>,
    pub(crate) shader_mode_error: Option<String>,
    pub(crate) tint: bool,
    pub(crate) interpreter:
        Arc<std::sync::OnceLock<crate::render::interpreter::PreparedInterpreter>>,
    queue: wgpu::Queue,
    pending: Arc<Mutex<PendingWork>>,
    loss: D3d9Loss,
    mipgen: Arc<Mutex<MipGenState>>,
    depth_upload: Arc<Mutex<Option<DepthUpload>>>,
}

impl Clone for D3d9Gpu {
    fn clone(&self) -> Self {
        Self {
            device: self.device.clone(),
            backend: self.backend,
            shader_mode: self.shader_mode.clone(),
            shader_mode_error: self.shader_mode_error.clone(),
            tint: self.tint,
            interpreter: Arc::clone(&self.interpreter),
            queue: self.queue.clone(),
            pending: Arc::clone(&self.pending),
            loss: self.loss.clone(),
            mipgen: Arc::clone(&self.mipgen),
            depth_upload: Arc::clone(&self.depth_upload),
        }
    }
}

pub(crate) fn pending_work(gpu: &GpuContext) -> Arc<Mutex<PendingWork>> {
    Arc::clone(gpu.d3d9_pending.get_or_init(|| {
        Arc::new(Mutex::new(PendingWork {
            device: gpu.device.clone(),
            physical: gpu.physical_loss(),
            encoder: None,
            staging: wgpu::util::StagingBelt::new(gpu.device.clone(), STAGING_CHUNK),
            clear: None,
            submitter: Submitter::new(gpu.queue.clone(), gpu.physical_loss()),
            flush_pending: Arc::new(AtomicBool::new(false)),
        }))
    }))
}

impl D3d9Gpu {
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        self.loss.physical.same_context(&other.loss.physical)
            && Arc::ptr_eq(&self.loss.reset_required, &other.loss.reset_required)
    }

    pub fn from_context(gpu: &GpuContext) -> Self {
        Self::from_context_with_loss(gpu, D3d9Loss::from_physical(gpu.physical_loss()))
    }

    pub(crate) fn from_context_with_loss(gpu: &GpuContext, loss: D3d9Loss) -> Self {
        Self {
            device: gpu.device.clone(),
            backend: gpu.adapter_info.backend,
            shader_mode: None,
            shader_mode_error: None,
            tint: false,
            interpreter: Arc::new(std::sync::OnceLock::new()),
            queue: gpu.queue.clone(),
            pending: pending_work(gpu),
            loss,
            mipgen: Arc::new(Mutex::new(MipGenState::default())),
            depth_upload: Arc::new(Mutex::new(None)),
        }
    }

    pub fn queue(&self) -> &wgpu::Queue {
        let mut pending = self.pending.lock().unwrap();
        pending.submit();
        pending.submitter.sync();
        &self.queue
    }

    pub fn submit_pending(&self) {
        self.pending.lock().unwrap().submit();
    }

    pub(crate) fn submit_native_completion(
        &self,
        completed: impl Fn(bool) + Send + Sync + 'static,
    ) -> Result<bool, RenderError> {
        let mut pending = self.pending.lock().unwrap();
        pending.submit();
        let Some(marker) = crate::backend::native_completion::marker(&self.device, completed)?
        else {
            return Ok(false);
        };
        pending.encoder = Some(marker);
        pending.submit();
        Ok(true)
    }

    pub(crate) fn request_flush(&self) {
        let mut pending = self.pending.lock().unwrap();
        pending.submit();
        if pending.flush_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let flush_pending = Arc::clone(&pending.flush_pending);
        let physical = self.loss.physical_loss();
        pending
            .submitter
            .send(SubmitJob::After(Box::new(move |queue| {
                if physical.is_lost() {
                    flush_pending.store(false, Ordering::Release);
                    return;
                }
                queue.submit(std::iter::empty());
                queue.on_submitted_work_done(move || {
                    flush_pending.store(false, Ordering::Release);
                });
            })));
    }

    pub(crate) fn submit_then(&self, work: impl FnOnce(&wgpu::Queue) + Send + 'static) {
        let mut pending = self.pending.lock().unwrap();
        pending.submit();
        pending.submitter.send(SubmitJob::After(Box::new(work)));
    }

    pub fn record<R>(
        &self,
        f: impl FnOnce(&mut wgpu::CommandEncoder, &mut wgpu::util::StagingBelt) -> R,
    ) -> R {
        self.record_folding_clear(|_| false, |encoder, staging, _| f(encoder, staging))
    }

    pub(crate) fn record_folding_clear<R>(
        &self,
        folds: impl FnOnce(&PendingClear) -> bool,
        f: impl FnOnce(
            &mut wgpu::CommandEncoder,
            &mut wgpu::util::StagingBelt,
            Option<PendingClear>,
        ) -> R,
    ) -> R {
        let mut pending = self.pending.lock().unwrap();
        let fold = pending.clear.take_if(|clear| folds(clear));
        pending.apply_clear();
        let PendingWork {
            device,
            encoder,
            staging,
            ..
        } = &mut *pending;
        f(frame_encoder(device, encoder), staging, fold)
    }

    pub(crate) fn set_clear(&self, clear: PendingClear) {
        let mut pending = self.pending.lock().unwrap();
        pending.apply_clear();
        pending.clear = Some(clear);
    }

    pub fn loss(&self) -> &D3d9Loss {
        &self.loss
    }

    pub fn is_blocked(&self) -> bool {
        self.loss.is_blocked()
    }

    pub fn is_physical(&self) -> bool {
        self.loss.is_physical()
    }

    pub fn flush(&self) -> Result<(), RenderError> {
        self.validate("RestoreFlush", || {
            self.queue().submit(std::iter::empty());
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .map_err(|error| RenderError::new("RestoreFlush", error.to_string()))?;
            Ok(())
        })
    }

    pub fn features(&self) -> wgpu::Features {
        self.device.features()
    }

    pub fn limits(&self) -> wgpu::Limits {
        self.device.limits()
    }

    pub fn available_texture_mem_bytes(&self) -> Result<u64, RenderError> {
        #[cfg(target_os = "macos")]
        {
            let hal =
                unsafe { self.device.as_hal::<wgpu::hal::api::Metal>() }.ok_or_else(|| {
                    RenderError::new("GetAvailableTextureMem", "the device is not a Metal device")
                })?;
            use objc2_metal::MTLDevice;
            let raw = hal.raw_device();
            Ok(raw
                .recommendedMaxWorkingSetSize()
                .saturating_sub(raw.currentAllocatedSize() as u64))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(RenderError::new(
                "GetAvailableTextureMem",
                "texture memory estimate is implemented only on the Metal backend",
            ))
        }
    }

    pub fn validate<T>(
        &self,
        operation: &'static str,
        f: impl FnOnce() -> Result<T, RenderError>,
    ) -> Result<T, RenderError> {
        if self.is_physical() {
            return Err(RenderError::new(operation, self.loss.physical_message()));
        }
        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom_scope = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let value = f();
        let oom = pollster::block_on(oom_scope.pop());
        let validation = pollster::block_on(validation_scope.pop());
        if let Some(error) = oom {
            return Err(RenderError::out_of_memory(operation, error.to_string()));
        }
        if let Some(error) = validation {
            return Err(RenderError::new(operation, error.to_string()));
        }
        value
    }

    pub fn create_texture(
        &self,
        width: u32,
        height: u32,
        depth: u32,
        levels: u32,
        format: wgpu::TextureFormat,
        dimension: wgpu::TextureDimension,
        usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture, RenderError> {
        self.validate("CreateTexture", || {
            Ok(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("d3d9-texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: depth,
                },
                mip_level_count: levels,
                sample_count: 1,
                dimension,
                format,
                usage,
                view_formats: &[],
            }))
        })
    }

    pub fn write_texture(
        &self,
        texture: &wgpu::Texture,
        d3d_format: u32,
        level: u32,
        width: u32,
        height: u32,
        depth: u32,
        layer: u32,
        is_3d: bool,
        bytes: &[u8],
    ) -> Result<(), RenderError> {
        let (pitch, slice_size) = format::physical_layout(d3d_format, width, height)
            .ok_or_else(|| RenderError::invalid("UploadTexture", "invalid texture layout"))?;
        let (copy_w, copy_h, copy_rows) = format::physical_copy_extent(d3d_format, width, height);
        let slices = if is_3d { depth.max(1) } else { 1 };
        let required = (slice_size as usize)
            .checked_mul(slices as usize)
            .ok_or_else(|| RenderError::invalid("UploadTexture", "texture byte size overflow"))?;
        if bytes.len() < required {
            return Err(RenderError::invalid(
                "UploadTexture",
                "converted level is shorter than its physical layout",
            ));
        }
        self.validate("UploadTexture", || {
            for slice in 0..slices {
                let start = slice as usize * slice_size as usize;
                let chunk = &bytes[start..start + slice_size as usize];
                let z = if is_3d { slice } else { layer };
                let aspect = if format::is_depth(d3d_format) {
                    wgpu::TextureAspect::DepthOnly
                } else {
                    wgpu::TextureAspect::All
                };
                self.queue().write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: level,
                        origin: wgpu::Origin3d { x: 0, y: 0, z },
                        aspect,
                    },
                    chunk,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch),
                        rows_per_image: Some(copy_rows),
                    },
                    wgpu::Extent3d {
                        width: copy_w,
                        height: copy_h,
                        depth_or_array_layers: 1,
                    },
                );
            }
            Ok(())
        })
    }

    pub fn write_depth32(
        &self,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        bytes: &[f32],
    ) -> Result<(), RenderError> {
        if bytes.len() < (width as usize) * (height as usize) {
            return Err(RenderError::invalid(
                "UploadTexture",
                "depth upload is shorter than its layout",
            ));
        }
        let mut state = self
            .depth_upload
            .lock()
            .expect("depth-upload mutex poisoned");
        if state.is_none() {
            *state = Some(self.depth_upload_pipeline()?);
        }
        let upload = state.as_ref().expect("depth-upload pipeline present");
        let pipeline = upload.pipeline.clone();
        let layout = upload.layout.clone();
        drop(state);

        let source = self.validate("UploadTexture", || {
            Ok(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("d3d9-depth-source"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }))
        })?;
        let mut raw = Vec::new();
        raw.try_reserve_exact(bytes.len() * 4).map_err(|_| {
            RenderError::out_of_memory("UploadTexture", "depth upload staging failed")
        })?;
        for value in bytes {
            raw.extend_from_slice(&value.to_le_bytes());
        }
        self.validate("UploadTexture", || {
            self.queue().write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &source,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &raw,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            Ok(())
        })?;
        let (source_view, dest_view, bind) = self.validate("UploadTexture", || {
            let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());
            let dest_view = texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("d3d9-depth-dest"),
                format: None,
                dimension: Some(wgpu::TextureViewDimension::D2),
                aspect: wgpu::TextureAspect::DepthOnly,
                base_mip_level: 0,
                mip_level_count: Some(1),
                base_array_layer: 0,
                array_layer_count: Some(1),
                usage: None,
            });
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("d3d9-depth-upload"),
                layout: &layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&source_view),
                }],
            });
            Ok((source_view, dest_view, bind))
        })?;
        let _ = source_view;
        self.validate("UploadTexture", || {
            self.record(|encoder, _| {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("d3d9-depth-upload"),
                    color_attachments: &[],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &dest_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(0.0),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &bind, &[]);
                pass.draw(0..3, 0..1);
            });
            Ok(())
        })
    }

    fn depth_upload_pipeline(&self) -> Result<DepthUpload, RenderError> {
        self.validate("UploadTexture", || {
            let shader = self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("d3d9-depth-upload"),
                    source: wgpu::ShaderSource::Wgsl(DEPTH_UPLOAD_WGSL.into()),
                });
            let layout = self
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("d3d9-depth-upload"),
                    entries: &[wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    }],
                });
            let pipeline_layout =
                self.device
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("d3d9-depth-upload"),
                        bind_group_layouts: &[Some(&layout)],
                        immediate_size: 0,
                    });
            let pipeline = self
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("d3d9-depth-upload"),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_main"),
                        targets: &[],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: wgpu::TextureFormat::Depth32Float,
                        depth_write_enabled: Some(true),
                        depth_compare: Some(wgpu::CompareFunction::Always),
                        stencil: wgpu::StencilState::default(),
                        bias: wgpu::DepthBiasState::default(),
                    }),
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                });
            Ok(DepthUpload { pipeline, layout })
        })
    }

    fn map_buffer(&self, buffer: &wgpu::Buffer, size: u64) -> Result<(), RenderError> {
        if size == 0 {
            return Ok(());
        }
        if size % wgpu::MAP_ALIGNMENT != 0 {
            return Err(RenderError::invalid(
                "Readback",
                "map size is not alignment safe",
            ));
        }
        let slice = buffer.slice(0..size);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::new("Readback::poll", e.to_string()))?;
        rx.recv()
            .map_err(|e| RenderError::new("Readback::map_callback", e.to_string()))?
            .map_err(|e| RenderError::new("Readback::map_async", e.to_string()))
    }

    pub fn read_texture(
        &self,
        texture: &wgpu::Texture,
        d3d_format: u32,
        level: u32,
        width: u32,
        height: u32,
        depth: u32,
        layer: u32,
        is_3d: bool,
        out: &mut [u8],
    ) -> Result<(), RenderError> {
        let (pitch, slice_size) = format::physical_layout(d3d_format, width, height)
            .ok_or_else(|| RenderError::invalid("ReadbackTexture", "invalid texture layout"))?;
        let (_, _, copy_rows) = format::physical_copy_extent(d3d_format, width, height);
        let (copy_w, copy_h, _) = format::physical_copy_extent(d3d_format, width, height);
        let slices = if is_3d { depth.max(1) } else { 1 };
        let required = slice_size as usize * slices as usize;
        if out.len() < required {
            return Err(RenderError::invalid(
                "ReadbackTexture",
                "destination is shorter than the logical level",
            ));
        }
        let padded = align_up(pitch, COPY_ALIGN);
        let buffer_bytes = padded as u64 * copy_rows as u64;
        if format::is_depth(d3d_format) && !format::depth_readback_supported(d3d_format) {
            return Err(RenderError::new(
                "ReadbackTexture",
                "non-lockable depth formats cannot be read back",
            ));
        }
        let aspect = if format::is_depth(d3d_format) {
            wgpu::TextureAspect::DepthOnly
        } else {
            wgpu::TextureAspect::All
        };
        for slice in 0..slices {
            let z = if is_3d { slice } else { layer };
            self.validate("ReadbackTexture", || {
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("d3d9-readback"),
                    size: buffer_bytes,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                let mut encoder =
                    self.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("d3d9-readback"),
                        });
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: level,
                        origin: wgpu::Origin3d { x: 0, y: 0, z },
                        aspect,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(padded),
                            rows_per_image: Some(copy_rows),
                        },
                    },
                    wgpu::Extent3d {
                        width: copy_w,
                        height: copy_h,
                        depth_or_array_layers: 1,
                    },
                );
                self.queue().submit(std::iter::once(encoder.finish()));
                self.map_buffer(&buffer, buffer_bytes)?;
                {
                    let mapped = buffer
                        .slice(0..buffer_bytes)
                        .get_mapped_range()
                        .map_err(|error| RenderError::new("ReadbackTexture", error.to_string()))?;
                    let base = slice as usize * slice_size as usize;
                    for row in 0..copy_rows as usize {
                        let src = row * padded as usize;
                        let dst = base + row * pitch as usize;
                        let row_len = pitch as usize;
                        if src + row_len > mapped.len() || dst + row_len > out.len() {
                            return Err(RenderError::invalid(
                                "ReadbackTexture",
                                "readback row lies outside the destination",
                            ));
                        }
                        out[dst..dst + row_len].copy_from_slice(&mapped[src..src + row_len]);
                    }
                }
                buffer.unmap();
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn generate_mips(
        &self,
        texture: &wgpu::Texture,
        format: wgpu::TextureFormat,
        levels: u32,
        base_width: u32,
        base_height: u32,
        layers: u32,
        linear: bool,
    ) -> Result<(), RenderError> {
        if levels < 2 || layers == 0 {
            return Ok(());
        }
        let mut state = self.mipgen.lock().expect("mipgen mutex poisoned");
        self.validate("GenerateMipSubLevels", || {
            state.ensure(&self.device, format, linear)
        })?;
        let cached = state.cached.as_ref().expect("mipgen cache present");
        let pipeline = cached.pipeline.clone();
        let layout = cached.layout.clone();
        let sampler = cached.sampler.clone();
        drop(state);

        self.validate("GenerateMipSubLevels", || {
            for layer in 0..layers {
                for level in 1..levels {
                    let view = |mip: u32| {
                        texture.create_view(&wgpu::TextureViewDescriptor {
                            label: Some("d3d9-mipgen"),
                            format: Some(format),
                            dimension: Some(wgpu::TextureViewDimension::D2),
                            aspect: wgpu::TextureAspect::All,
                            base_mip_level: mip,
                            mip_level_count: Some(1),
                            base_array_layer: layer,
                            array_layer_count: Some(1),
                            usage: None,
                        })
                    };
                    let src = view(level - 1);
                    let dst = view(level);
                    let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("d3d9-mipgen"),
                        layout: &layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&src),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&sampler),
                            },
                        ],
                    });
                    self.record(|encoder, _| {
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("d3d9-mipgen"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &dst,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                    store: wgpu::StoreOp::Store,
                                },
                                depth_slice: None,
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        });
                        pass.set_pipeline(&pipeline);
                        pass.set_bind_group(0, &bind, &[]);
                        pass.draw(0..3, 0..1);
                    });
                }
            }
            let _ = (base_width, base_height);
            Ok(())
        })
    }
}

struct DepthUpload {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

#[derive(Default)]
struct MipGenState {
    cached: Option<MipGen>,
}

struct MipGen {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    format: wgpu::TextureFormat,
    linear: bool,
}

impl MipGenState {
    fn ensure(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        linear: bool,
    ) -> Result<(), RenderError> {
        if matches!(&self.cached, Some(c) if c.format == format && c.linear == linear) {
            return Ok(());
        }
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("d3d9-mipgen"),
            source: wgpu::ShaderSource::Wgsl(MIPGEN_WGSL.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("d3d9-mipgen"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: linear },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(if linear {
                        wgpu::SamplerBindingType::Filtering
                    } else {
                        wgpu::SamplerBindingType::NonFiltering
                    }),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("d3d9-mipgen"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("d3d9-mipgen"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let filter = if linear {
            wgpu::FilterMode::Linear
        } else {
            wgpu::FilterMode::Nearest
        };
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("d3d9-mipgen"),
            mag_filter: filter,
            min_filter: filter,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        self.cached = Some(MipGen {
            pipeline,
            layout,
            sampler,
            format,
            linear,
        });
        Ok(())
    }
}

const MIPGEN_WGSL: &str = r#"
@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    var out: VsOut;
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    out.pos = vec4<f32>(pos[idx], 0.0, 1.0);
    out.uv = vec2<f32>((pos[idx].x + 1.0) * 0.5, (1.0 - pos[idx].y) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(src_tex, src_samp, in.uv, 0.0);
}
"#;

const DEPTH_UPLOAD_WGSL: &str = r#"
@group(0) @binding(0) var src_tex: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    return vec4<f32>(pos[idx], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @builtin(frag_depth) f32 {
    return textureLoad(src_tex, vec2<i32>(i32(frag.x), i32(frag.y)), 0).r;
}
"#;
