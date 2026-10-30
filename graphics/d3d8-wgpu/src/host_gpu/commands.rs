use super::*;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
pub(super) struct Command {
    pub encoder: Option<wgpu::CommandEncoder>,
    pub render: render::Recording,
    pub status: Arc<AtomicI32>,
    pub callbacks: Vec<(extern "C" fn(*mut c_void, i32, f64), usize)>,
    pub presents: Vec<surface_ffi::PendingPresent>,
    pub inflight: Option<Arc<Mutex<Option<Completion>>>>,
    pub delivered: Arc<(Mutex<bool>, std::sync::Condvar)>,
}
pub(super) struct Completion {
    pub callbacks: Vec<(extern "C" fn(*mut c_void, i32, f64), usize)>,
    pub presented: Vec<(extern "C" fn(*mut c_void, f64), usize)>,
}
impl Completion {
    pub fn fire(self, status: i32) {
        for (f, arg) in self.callbacks {
            f(arg as *mut c_void, status, 0.0);
        }
        for (f, arg) in self.presented {
            f(arg as *mut c_void, 0.0);
        }
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_begin(gpu: *mut HostGpu) -> u64 {
    guard("begin", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        if g.context.loss.is_lost() {
            report("begin device lost");
            return 0;
        }
        let encoder = g
            .context
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("host-command"),
            });
        let mut s = g.state.lock().unwrap();
        let id = s.id();
        s.commands.insert(
            id,
            Command {
                encoder: Some(encoder),
                render: render::Recording::default(),
                status: Arc::new(AtomicI32::new(2)),
                callbacks: Vec::new(),
                presents: Vec::new(),
                inflight: None,
                delivered: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
            },
        );
        id
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_blit(
    gpu: *mut HostGpu,
    cb: u64,
    src: u64,
    r: Region,
    dst: u64,
    x: i32,
    y: i32,
) {
    guard("blit", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let mut s = g.state.lock().unwrap();
        let (Some(a), Some(b)) = (s.textures.get(&src), s.textures.get(&dst)) else {
            report("blit texture");
            return;
        };
        if !bounds(a.desc, r, 0)
            || !bounds(
                b.desc,
                Region {
                    x,
                    y,
                    w: r.w,
                    h: r.h,
                },
                0,
            )
            || a.desc.format != b.desc.format
        {
            report("blit bounds or format");
            return;
        }
        let a = a.texture.clone();
        let b = b.texture.clone();
        let Some(c) = s.commands.get_mut(&cb) else {
            report("blit command");
            return;
        };
        c.render.finish();
        let Some(encoder) = c.encoder.as_mut() else {
            report("blit committed");
            return;
        };
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &a,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: r.x as u32,
                    y: r.y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &b,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: x as u32,
                    y: y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            extent(r).unwrap(),
        );
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_on_complete(
    gpu: *mut HostGpu,
    cb: u64,
    callback: Option<extern "C" fn(*mut c_void, i32, f64)>,
    userdata: *mut c_void,
) {
    guard("on_complete", || {
        let (Some(g), Some(callback)) = ((unsafe { gpu.as_ref() }), callback) else {
            return;
        };
        let mut s = g.state.lock().unwrap();
        let Some(c) = s.commands.get_mut(&cb) else {
            report("on_complete command");
            return;
        };
        if c.encoder.is_none() {
            report("on_complete after commit");
            return;
        }
        c.callbacks.push((callback, userdata as usize));
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_commit(gpu: *mut HostGpu, cb: u64) {
    guard("commit", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let (encoder, status, callbacks, presents, delivered) = {
            let mut s = g.state.lock().unwrap();
            let Some(c) = s.commands.get_mut(&cb) else {
                report("commit command");
                return;
            };
            c.render.finish();
            let Some(encoder) = c.encoder.take() else {
                report("commit twice");
                return;
            };
            (
                encoder,
                Arc::clone(&c.status),
                std::mem::take(&mut c.callbacks),
                std::mem::take(&mut c.presents),
                Arc::clone(&c.delivered),
            )
        };
        let lost = g.context.loss.is_lost();
        if !lost {
            g.queue_owner.with(|queue| queue.submit([encoder.finish()]));
        }
        let mut presented = Vec::new();
        for present in presents {
            if !lost && !present.cancelled.load(Ordering::Acquire) {
                surface_ffi::present_frame(g, present.chain, present.frame);
            } else {
                drop(present.frame);
                surface_ffi::present_finished(g, present.chain);
            }
            if let Some(callback) = present.callback {
                presented.push((callback, present.userdata));
            }
        }
        let completion = Arc::new(Mutex::new(Some(Completion {
            callbacks,
            presented,
        })));
        {
            let mut state = g.state.lock().unwrap();
            if let Some(command) = state.commands.get_mut(&cb) {
                command.inflight = Some(Arc::clone(&completion));
            }
        }
        let state = Arc::downgrade(&g.state);
        let callbacks_gate = Arc::clone(&g.callbacks);
        let pending = Arc::clone(&g.poller.pending);
        let loss = g.context.loss.clone();
        let sender = g.delivery.sender.clone();
        let deliver = move |result: i32| {
            status.store(result, Ordering::Release);
            let job = Box::new(move || {
                if callbacks_gate.enter() {
                    let callback = completion.lock().unwrap().take();
                    if let Some(callback) = callback {
                        callback.fire(result);
                    }
                    callbacks_gate.leave();
                }
                let (done, condvar) = &*delivered;
                *done.lock().unwrap() = true;
                condvar.notify_all();
                if result == 0 {
                    if let Some(state) = state.upgrade() {
                        state.lock().unwrap().commands.remove(&cb);
                    }
                }
            });
            if sender.send(job).is_err() {
                eprintln!("host GPU completion delivery stopped");
            }
        };
        if lost {
            deliver(1);
        } else {
            pending.fetch_add(1, Ordering::Release);
            g.context.queue.on_submitted_work_done(move || {
                pending.fetch_sub(1, Ordering::AcqRel);
                deliver(if loss.is_lost() { 1 } else { 0 });
            });
            g.poller.wake();
        }
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_wait(gpu: *mut HostGpu, cb: u64) {
    guard("wait", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let delivered = {
            let state = g.state.lock().unwrap();
            match state.commands.get(&cb) {
                Some(command) if command.encoder.is_some() => {
                    report("wait on uncommitted command");
                    return;
                }
                Some(command) => Some(Arc::clone(&command.delivered)),
                None => None,
            }
        };
        if unsafe { host_gpu_status(gpu, cb) } == 2 {
            if let Err(e) = g.context.device.poll(wgpu::PollType::wait_indefinitely()) {
                eprintln!("host GPU wait: {e}");
            }
        }
        if !g.callbacks.on_callback_thread() {
            if let Some(delivered) = delivered {
                let (done, condvar) = &*delivered;
                let mut done = done.lock().unwrap();
                while !*done {
                    done = condvar.wait(done).unwrap();
                }
            }
        }
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_status(gpu: *mut HostGpu, cb: u64) -> i32 {
    guard("status", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 1;
        };
        let _ = g.context.device.poll(wgpu::PollType::Poll);
        g.state
            .lock()
            .unwrap()
            .commands
            .get(&cb)
            .map(|c| c.status.load(Ordering::Acquire))
            .unwrap_or(0)
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_copy_buffer_to_texture(
    gpu: *mut HostGpu,
    cb: u64,
    src: u64,
    offset: u64,
    pitch: i32,
    dst: u64,
    r: Region,
) {
    guard("copy_buffer_to_texture", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let mut s = g.state.lock().unwrap();
        let (Some(b), Some(t)) = (s.buffers.get(&src), s.textures.get(&dst)) else {
            report("copy_buffer_to_texture handle");
            return;
        };
        if !bounds(t.desc, r, 0)
            || pitch < 0
            || (pitch as u64) < r.w as u64 * bpp(t.desc)
            || offset
                .checked_add((r.h as u64 - 1) * pitch as u64 + r.w as u64 * bpp(t.desc))
                .is_none_or(|v| v > b.size)
        {
            report("copy_buffer_to_texture bounds or alignment");
            return;
        }
        let row = r.w as u32 * bpp(t.desc) as u32;
        let is_depth = t.desc.format == 3;
        let b = b.buffer.clone();
        let t = t.texture.clone();
        let depth = if is_depth {
            Some(
                s.depth_restore
                    .get_or_insert_with(|| depth::DepthRestore::new(&g.context.device))
                    .clone(),
            )
        } else {
            None
        };
        let Some(c) = s.commands.get_mut(&cb) else {
            report("copy_buffer_to_texture command");
            return;
        };
        c.render.finish();
        let Some(e) = c.encoder.as_mut() else {
            report("copy_buffer_to_texture committed");
            return;
        };
        if let Some(depth) = depth {
            if let Err(error) =
                depth.encode(&g.context.device, e, &b, offset, pitch as u32, &t, r, 0)
            {
                eprintln!("host GPU copy_buffer_to_texture: {error}");
            }
            return;
        }
        let mut source = b.clone();
        let mut copy_offset = offset;
        let mut copy_pitch = pitch as u32;
        if copy_pitch % 256 != 0 || copy_offset % 4 != 0 {
            copy_pitch = row.div_ceil(256) * 256;
            source = g.context.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("host-copy-repack"),
                size: copy_pitch as u64 * r.h as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            if let Err(error) = repack::encode(
                &g.context.device,
                e,
                &b,
                &source,
                offset,
                pitch as u32,
                0,
                copy_pitch,
                row,
                r.h as u32,
            ) {
                eprintln!("host GPU copy_buffer_to_texture: {error}");
                return;
            }
            copy_offset = 0;
        }
        e.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &source,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: copy_offset,
                    bytes_per_row: Some(copy_pitch),
                    rows_per_image: Some(r.h as u32),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &t,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: r.x as u32,
                    y: r.y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            extent(r).unwrap(),
        );
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_copy_texture_to_buffer(
    gpu: *mut HostGpu,
    cb: u64,
    src: u64,
    r: Region,
    dst: u64,
    offset: u64,
    pitch: i32,
) {
    guard("copy_texture_to_buffer", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let mut s = g.state.lock().unwrap();
        let (Some(t), Some(b)) = (s.textures.get(&src), s.buffers.get(&dst)) else {
            report("copy_texture_to_buffer handle");
            return;
        };
        if !bounds(t.desc, r, 0)
            || pitch < 0
            || (pitch as u64) < r.w as u64 * bpp(t.desc)
            || offset
                .checked_add((r.h as u64 - 1) * pitch as u64 + r.w as u64 * bpp(t.desc))
                .is_none_or(|v| v > b.size)
        {
            report("copy_texture_to_buffer bounds or alignment");
            return;
        }
        let row = r.w as u32 * bpp(t.desc) as u32;
        let b = b.buffer.clone();
        let t = t.texture.clone();
        let Some(c) = s.commands.get_mut(&cb) else {
            report("copy_texture_to_buffer command");
            return;
        };
        c.render.finish();
        let Some(e) = c.encoder.as_mut() else {
            report("copy_texture_to_buffer committed");
            return;
        };
        let needs_repack = pitch as u32 % 256 != 0 || offset % 4 != 0;
        let copy_pitch = if needs_repack {
            row.div_ceil(256) * 256
        } else {
            pitch as u32
        };
        let copy_offset = if needs_repack { 0 } else { offset };
        let target = if needs_repack {
            g.context.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("host-copy-repack"),
                size: copy_pitch as u64 * r.h as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        } else {
            b.clone()
        };
        e.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &t,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: r.x as u32,
                    y: r.y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &target,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: copy_offset,
                    bytes_per_row: Some(copy_pitch),
                    rows_per_image: Some(r.h as u32),
                },
            },
            extent(r).unwrap(),
        );
        if needs_repack {
            if let Err(error) = repack::encode(
                &g.context.device,
                e,
                &target,
                &b,
                0,
                copy_pitch,
                offset,
                pitch as u32,
                row,
                r.h as u32,
            ) {
                eprintln!("host GPU copy_texture_to_buffer: {error}");
            }
        }
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_generate_mipmaps(gpu: *mut HostGpu, cb: u64, id: u64) {
    guard("generate_mipmaps", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return;
        };
        let mut state = g.state.lock().unwrap();
        let Some(texture) = state.textures.get(&id) else {
            report("generate_mipmaps texture");
            return;
        };
        if texture.desc.mip_levels <= 1 {
            return;
        }
        if texture.desc.format == 3 {
            report("generate_mipmaps depth");
            return;
        }
        let texture = texture.texture.clone();
        let Some(command) = state.commands.get_mut(&cb) else {
            report("generate_mipmaps command");
            return;
        };
        command.render.finish();
        let Some(encoder) = command.encoder.as_mut() else {
            report("generate_mipmaps committed");
            return;
        };
        let shader=g.context.device.create_shader_module(wgpu::ShaderModuleDescriptor {label:Some("host-mipmap"),source:wgpu::ShaderSource::Wgsl(r#"@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
struct V { @builtin(position) p: vec4f, @location(0) uv: vec2f };
@vertex fn vs(@builtin(vertex_index) n: u32) -> V { var v: V; let p=vec2f(f32((n << 1u) & 2u),f32(n & 2u)); v.p=vec4f(p*2.0-1.0,0.0,1.0); v.uv=vec2f(p.x,1.0-p.y); return v; }
@fragment fn fs(v: V) -> @location(0) vec4f { return textureSampleLevel(t,s,v.uv,0.0); }"#.into())});
        let pipeline = g
            .context
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("host-mipmap"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: texture.format(),
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            });
        let sampler = g.context.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("host-mipmap"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        for level in 1..texture.mip_level_count() {
            let view = |n| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: n,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            };
            let src = view(level - 1);
            let dst = view(level);
            let bind = g
                .context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("host-mipmap"),
                    layout: &pipeline.get_bind_group_layout(0),
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
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("host-mipmap"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
    });
}
