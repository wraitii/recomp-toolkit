use recomp_gpu::SharedContext;
use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{Arc, Mutex},
};

mod commands;
mod depth;
mod render;
mod repack;
mod shaders;
mod surface;
mod surface_ffi;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TextureDesc {
    width: i32,
    height: i32,
    format: i32,
    usage: u32,
    mip_levels: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Region {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}
struct Texture {
    texture: wgpu::Texture,
    desc: TextureDesc,
    allocated: u64,
}
struct Buffer {
    buffer: wgpu::Buffer,
    size: u64,
}
struct State {
    next: u64,
    render: render::Cache,
    depth_restore: Option<depth::DepthRestore>,
    textures: HashMap<u64, Texture>,
    buffers: HashMap<u64, Buffer>,
    commands: HashMap<u64, commands::Command>,
    chains: HashMap<u64, Arc<Mutex<surface::SurfaceState>>>,
}
pub struct HostGpu {
    context: Arc<SharedContext>,
    queue_owner: crate::kit::QueueOwner,
    state: Arc<Mutex<State>>,
    poller: Poller,
    delivery: DeliveryWorker,
    callbacks: Arc<CallbackGate>,
}
type DeliveryJob = Box<dyn FnOnce() + Send>;
struct DeliveryWorker {
    sender: std::sync::mpsc::Sender<DeliveryJob>,
    thread: Option<std::thread::JoinHandle<()>>,
}
thread_local! { static IS_DELIVERY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
impl DeliveryWorker {
    fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel::<DeliveryJob>();
        let thread = std::thread::spawn(move || {
            IS_DELIVERY.with(|v| v.set(true));
            while let Ok(job) = receiver.recv() {
                job();
            }
        });
        Self {
            sender,
            thread: Some(thread),
        }
    }
    fn stop(&mut self) {
        let (closed, _) = std::sync::mpsc::channel();
        drop(std::mem::replace(&mut self.sender, closed));
        if let Some(thread) = self.thread.take() {
            if !IS_DELIVERY.try_with(|v| v.get()).unwrap_or(false) {
                let _ = thread.join();
            }
        }
    }
}
impl Drop for DeliveryWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
#[derive(Default)]
struct CallbackGate {
    state: Mutex<(bool, usize)>,
    finished: std::sync::Condvar,
}
thread_local! { static ACTIVE_CALLBACKS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) }; }
impl CallbackGate {
    fn on_callback_thread(&self) -> bool {
        ACTIVE_CALLBACKS
            .try_with(|v| v.borrow().contains(&(self as *const Self as usize)))
            .unwrap_or(false)
    }
    fn enter(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.0 {
            return false;
        }
        state.1 += 1;
        ACTIVE_CALLBACKS.with(|v| v.borrow_mut().push(self as *const Self as usize));
        true
    }
    fn leave(&self) {
        ACTIVE_CALLBACKS.with(|v| {
            let id = self as *const Self as usize;
            let mut callbacks = v.borrow_mut();
            let index = callbacks.iter().rposition(|value| *value == id).unwrap();
            callbacks.remove(index);
        });
        let mut state = self.state.lock().unwrap();
        state.1 -= 1;
        self.finished.notify_all();
    }
    fn close(&self) {
        let local = ACTIVE_CALLBACKS
            .try_with(|v| {
                v.borrow()
                    .iter()
                    .filter(|&&id| id == self as *const Self as usize)
                    .count()
            })
            .unwrap_or(0);
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        while state.1 > local {
            state = self.finished.wait(state).unwrap();
        }
    }
}
struct Poller {
    signal: std::sync::mpsc::Sender<()>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}
thread_local! { static IS_POLLER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
impl Poller {
    fn new(context: Arc<SharedContext>) -> Self {
        let (signal, receiver) = std::sync::mpsc::channel();
        let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let active = Arc::clone(&pending);
        let thread = std::thread::spawn(move || {
            IS_POLLER.with(|v| v.set(true));
            loop {
                if active.load(std::sync::atomic::Ordering::Acquire) == 0 {
                    if receiver.recv().is_err() {
                        break;
                    }
                } else if matches!(
                    receiver.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Disconnected)
                ) {
                    break;
                }
                if active.load(std::sync::atomic::Ordering::Acquire) != 0 {
                    match context.device.poll(wgpu::PollType::Wait {
                        submission_index: None,
                        timeout: Some(std::time::Duration::from_millis(10)),
                    }) {
                        Ok(_) | Err(wgpu::PollError::Timeout) => (),
                        Err(error) => eprintln!("host GPU completion polling: {error}"),
                    }
                }
            }
        });
        Self {
            signal,
            pending,
            thread: Some(thread),
        }
    }
    fn wake(&self) {
        let _ = self.signal.send(());
    }
    fn stop(&mut self) {
        let (closed, _) = std::sync::mpsc::channel();
        drop(std::mem::replace(&mut self.signal, closed));
        if let Some(thread) = self.thread.take() {
            if !IS_POLLER.try_with(|v| v.get()).unwrap_or(false) {
                let _ = thread.join();
            }
        }
    }
}
impl Drop for Poller {
    fn drop(&mut self) {
        self.stop();
    }
}
impl State {
    fn id(&mut self) -> u64 {
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .expect("host GPU handle exhaustion");
        id
    }
}
pub(crate) fn guard<T>(name: &str, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            eprintln!("host GPU {name}: panic");
            std::process::abort()
        }
    }
}
fn format(value: i32) -> Option<wgpu::TextureFormat> {
    match value {
        0 => Some(wgpu::TextureFormat::Bgra8Unorm),
        1 => Some(wgpu::TextureFormat::Rgba8Unorm),
        2 => Some(wgpu::TextureFormat::R8Unorm),
        3 => Some(wgpu::TextureFormat::Depth32Float),
        _ => None,
    }
}
fn bpp(desc: TextureDesc) -> u64 {
    if desc.format == 2 { 1 } else { 4 }
}
fn extent(r: Region) -> Option<wgpu::Extent3d> {
    if r.x < 0 || r.y < 0 || r.w <= 0 || r.h <= 0 {
        return None;
    }
    Some(wgpu::Extent3d {
        width: r.w as u32,
        height: r.h as u32,
        depth_or_array_layers: 1,
    })
}
fn bounds(d: TextureDesc, r: Region, level: i32) -> bool {
    level >= 0
        && level < d.mip_levels
        && r.x >= 0
        && r.y >= 0
        && r.w > 0
        && r.h > 0
        && (r.x as i64 + r.w as i64) <= (d.width >> level).max(1) as i64
        && (r.y as i64 + r.h as i64) <= (d.height >> level).max(1) as i64
}
fn report(name: &str) {
    eprintln!("host GPU {name}: invalid or unsupported request");
}
impl HostGpu {
    pub(crate) fn new(context: Arc<SharedContext>, queue_owner: crate::kit::QueueOwner) -> Self {
        Self {
            poller: Poller::new(Arc::clone(&context)),
            delivery: DeliveryWorker::new(),
            callbacks: Arc::new(CallbackGate::default()),
            state: Arc::new(Mutex::new(State {
                next: 1,
                render: render::Cache::new(&context.device),
                depth_restore: None,
                textures: HashMap::new(),
                buffers: HashMap::new(),
                commands: HashMap::new(),
                chains: HashMap::new(),
            })),
            context,
            queue_owner,
        }
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_free(gpu: *mut HostGpu) {
    guard("free", || {
        if !gpu.is_null() {
            let mut gpu = unsafe { Box::from_raw(gpu) };
            gpu.poller.stop();
            gpu.callbacks.close();
            gpu.delivery.stop();
            let callbacks = {
                let mut state = gpu.state.lock().unwrap();
                let mut callbacks = Vec::new();
                for command in state.commands.values_mut() {
                    command.render.finish();
                    if let Some(inflight) = command.inflight.take() {
                        if let Some(completion) = inflight.lock().unwrap().take() {
                            for (callback, userdata) in completion.callbacks {
                                callbacks.push((Some(callback), None, userdata));
                            }
                            for (callback, userdata) in completion.presented {
                                callbacks.push((None, Some(callback), userdata));
                            }
                        }
                    }
                    for (callback, userdata) in command.callbacks.drain(..) {
                        callbacks.push((Some(callback), None, userdata));
                    }
                    for present in command.presents.drain(..) {
                        callbacks.push((None, present.callback, present.userdata));
                    }
                }
                callbacks
            };
            for (completion, presented, userdata) in callbacks {
                if let Some(callback) = completion {
                    callback(userdata as *mut c_void, 1, 0.0);
                }
                if let Some(callback) = presented {
                    callback(userdata as *mut c_void, 0.0);
                }
            }
        }
    });
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_texture_create(
    gpu: *mut HostGpu,
    desc: *const TextureDesc,
) -> u64 {
    guard("texture_create", || {
        let (Some(g), Some(d)) = (unsafe { gpu.as_ref() }, unsafe { desc.as_ref() }) else {
            return 0;
        };
        if g.context.loss.is_lost() {
            report("texture_create device lost");
            return 0;
        }
        let d = *d;
        let Some(format) = format(d.format) else {
            report("texture_create format");
            return 0;
        };
        if d.width <= 0
            || d.height <= 0
            || d.mip_levels <= 0
            || d.mip_levels as u32 > 32 - (d.width.max(d.height) as u32).leading_zeros()
        {
            report("texture_create dimensions");
            return 0;
        }
        let mut usage = wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST;
        if d.mip_levels > 1 && d.format != 3 {
            usage |= wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT;
        }
        if d.usage & 1 != 0 {
            usage |= wgpu::TextureUsages::TEXTURE_BINDING;
        }
        if d.usage & 2 != 0 {
            usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
        }
        if d.usage & 4 != 0 {
            usage |= wgpu::TextureUsages::STORAGE_BINDING;
        }
        let texture = g.context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("host-texture"),
            size: wgpu::Extent3d {
                width: d.width as u32,
                height: d.height as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: d.mip_levels as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let allocated = (0..d.mip_levels)
            .map(|n| (d.width >> n).max(1) as u64 * (d.height >> n).max(1) as u64 * bpp(d))
            .sum();
        let mut s = g.state.lock().unwrap();
        let id = s.id();
        s.textures.insert(
            id,
            Texture {
                texture,
                desc: d,
                allocated,
            },
        );
        id
    })
}
impl HostGpu {
    pub(crate) fn import_texture(
        &self,
        context: &Arc<SharedContext>,
        texture: wgpu::Texture,
    ) -> u64 {
        if self.context.loss.is_lost() || !Arc::ptr_eq(context, &self.context) {
            report("texture_import_lease context lost or mismatched");
            return 0;
        }
        if !texture
            .usage()
            .contains(wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING)
            || texture.sample_count() != 1
            || texture.dimension() != wgpu::TextureDimension::D2
        {
            report("texture_import_lease image contract");
            return 0;
        }
        let fmt = match texture.format() {
            wgpu::TextureFormat::Bgra8Unorm => 0,
            wgpu::TextureFormat::Rgba8Unorm => 1,
            wgpu::TextureFormat::R8Unorm => 2,
            _ => {
                report("texture_import_lease format");
                return 0;
            }
        };
        let d = TextureDesc {
            width: texture.width() as i32,
            height: texture.height() as i32,
            format: fmt,
            usage: 1,
            mip_levels: texture.mip_level_count() as i32,
        };
        let allocated = texture.width() as u64 * texture.height() as u64 * bpp(d);
        let mut s = self.state.lock().unwrap();
        let id = s.id();
        s.textures.insert(
            id,
            Texture {
                texture,
                desc: d,
                allocated,
            },
        );
        id
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_texture_upload(
    gpu: *mut HostGpu,
    id: u64,
    r: Region,
    data: *const c_void,
    pitch: i32,
    level: i32,
) -> u8 {
    guard("texture_upload", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        if g.context.loss.is_lost() {
            report("texture_upload device lost");
            return 0;
        }
        let mut s = g.state.lock().unwrap();
        let Some(t) = s.textures.get(&id) else {
            report("texture_upload handle");
            return 0;
        };
        if data.is_null()
            || !bounds(t.desc, r, level)
            || pitch < 0
            || (pitch as u64) < r.w as u64 * bpp(t.desc)
        {
            report("texture_upload bounds");
            return 0;
        }
        let len = (pitch as usize)
            .checked_mul((r.h - 1) as usize)
            .and_then(|n| n.checked_add(r.w as usize * bpp(t.desc) as usize));
        let Some(len) = len else { return 0 };
        let is_depth = t.desc.format == 3;
        let texture = t.texture.clone();
        let depth = if is_depth {
            Some(
                s.depth_restore
                    .get_or_insert_with(|| depth::DepthRestore::new(&g.context.device))
                    .clone(),
            )
        } else {
            None
        };
        drop(s);
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
        if let Some(depth) = depth {
            use wgpu::util::DeviceExt;
            let mut padded = vec![0; len.div_ceil(4) * 4];
            padded[..len].copy_from_slice(bytes);
            let source = g
                .context
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("host-depth-upload"),
                    contents: &padded,
                    usage: wgpu::BufferUsages::STORAGE,
                });
            let mut encoder =
                g.context
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("host-depth-upload"),
                    });
            if let Err(error) = depth.encode(
                &g.context.device,
                &mut encoder,
                &source,
                0,
                pitch as u32,
                &texture,
                r,
                level as u32,
            ) {
                eprintln!("host GPU texture_upload: {error}");
                return 0;
            }
            g.queue_owner.with(|queue| queue.submit([encoder.finish()]));
            return 1;
        }
        g.queue_owner.with(|queue| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d {
                        x: r.x as u32,
                        y: r.y as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pitch as u32),
                    rows_per_image: Some(r.h as u32),
                },
                extent(r).unwrap(),
            )
        });
        1
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_texture_describe(
    gpu: *mut HostGpu,
    id: u64,
    desc: *mut TextureDesc,
    allocated: *mut u64,
) -> u8 {
    guard("texture_describe", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        let s = g.state.lock().unwrap();
        let Some(t) = s.textures.get(&id) else {
            return 0;
        };
        if let Some(d) = unsafe { desc.as_mut() } {
            *d = t.desc
        };
        if let Some(a) = unsafe { allocated.as_mut() } {
            *a = t.allocated
        };
        1
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_texture_destroy(gpu: *mut HostGpu, id: u64) {
    guard("texture_destroy", || {
        if let Some(g) = unsafe { gpu.as_ref() } {
            g.state.lock().unwrap().textures.remove(&id);
        }
    });
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_buffer_create(
    gpu: *mut HostGpu,
    size: u64,
    data: *const c_void,
) -> u64 {
    guard("buffer_create", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        if g.context.loss.is_lost() {
            report("buffer_create device lost");
            return 0;
        }
        if size == 0 || size > g.context.device.limits().max_buffer_size {
            report("buffer_create size");
            return 0;
        }
        let aligned = (size + 3) & !3;
        let buffer = g.context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("host-buffer"),
            size: aligned,
            usage: wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::VERTEX
                | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        if !data.is_null() {
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
            let mut padded = vec![0; aligned as usize];
            padded[..bytes.len()].copy_from_slice(bytes);
            g.queue_owner
                .with(|queue| queue.write_buffer(&buffer, 0, &padded));
        }
        let mut s = g.state.lock().unwrap();
        let id = s.id();
        s.buffers.insert(id, Buffer { buffer, size });
        id
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_buffer_update(
    gpu: *mut HostGpu,
    id: u64,
    offset: u64,
    data: *const c_void,
    size: u64,
) -> u8 {
    guard("buffer_update", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        if g.context.loss.is_lost() {
            report("buffer_update device lost");
            return 0;
        }
        let s = g.state.lock().unwrap();
        let Some(b) = s.buffers.get(&id) else {
            return 0;
        };
        if data.is_null() || offset.checked_add(size).is_none_or(|n| n > b.size) {
            report("buffer_update bounds");
            return 0;
        }
        let buffer = b.buffer.clone();
        drop(s);
        if size == 0 {
            return 1;
        }
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
        if offset % 4 != 0 || size % 4 != 0 {
            use wgpu::util::DeviceExt;
            let Ok(row) = u32::try_from(size) else {
                report("buffer_update size");
                return 0;
            };
            let mut padded = vec![0; size.div_ceil(4) as usize * 4];
            padded[..bytes.len()].copy_from_slice(bytes);
            let staging = g
                .context
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("host-unaligned-update"),
                    contents: &padded,
                    usage: wgpu::BufferUsages::STORAGE,
                });
            let mut encoder =
                g.context
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("host-unaligned-update"),
                    });
            if let Err(error) = repack::encode(
                &g.context.device,
                &mut encoder,
                &staging,
                &buffer,
                0,
                row,
                offset,
                row,
                row,
                1,
            ) {
                eprintln!("host GPU buffer_update: {error}");
                return 0;
            }
            g.queue_owner.with(|queue| queue.submit([encoder.finish()]));
            return 1;
        }
        g.queue_owner
            .with(|queue| queue.write_buffer(&buffer, offset, bytes));
        1
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_buffer_size(gpu: *mut HostGpu, id: u64) -> u64 {
    guard("buffer_size", || {
        unsafe { gpu.as_ref() }
            .and_then(|g| g.state.lock().unwrap().buffers.get(&id).map(|b| b.size))
            .unwrap_or(0)
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_buffer_destroy(gpu: *mut HostGpu, id: u64) {
    guard("buffer_destroy", || {
        if let Some(g) = unsafe { gpu.as_ref() } {
            g.state.lock().unwrap().buffers.remove(&id);
        }
    });
}
fn read_staging(
    g: &HostGpu,
    encoder: wgpu::CommandEncoder,
    staging: &wgpu::Buffer,
) -> Option<Vec<u8>> {
    if g.context.loss.is_lost() {
        report("readback device lost");
        return None;
    }
    g.queue_owner.with(|queue| queue.submit([encoder.finish()]));
    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    if g.context
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .is_err()
        || !matches!(rx.recv(), Ok(Ok(())))
    {
        report("readback map");
        return None;
    }
    let bytes = slice.get_mapped_range().ok()?.to_vec();
    staging.unmap();
    Some(bytes)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_texture_read(
    gpu: *mut HostGpu,
    id: u64,
    r: Region,
    data: *mut c_void,
    pitch: i32,
) -> u8 {
    guard("texture_read", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        if g.context.loss.is_lost() {
            report("texture_read device lost");
            return 0;
        }
        let s = g.state.lock().unwrap();
        let Some(t) = s.textures.get(&id) else {
            return 0;
        };
        if data.is_null()
            || !bounds(t.desc, r, 0)
            || pitch < 0
            || (pitch as u64) < r.w as u64 * bpp(t.desc)
        {
            report("texture_read bounds");
            return 0;
        }
        let row = r.w as u64 * bpp(t.desc);
        let texture = t.texture.clone();
        drop(s);
        let padded = (row + 255) & !255;
        let size = padded * r.h as u64;
        let staging = g.context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("host-texture-read"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            g.context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("host-texture-read"),
                });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: r.x as u32,
                    y: r.y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded as u32),
                    rows_per_image: Some(r.h as u32),
                },
            },
            extent(r).unwrap(),
        );
        let Some(bytes) = read_staging(g, encoder, &staging) else {
            return 0;
        };
        for y in 0..r.h as usize {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(y * padded as usize),
                    data.cast::<u8>().add(y * pitch as usize),
                    row as usize,
                )
            }
        }
        1
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_buffer_read(
    gpu: *mut HostGpu,
    id: u64,
    data: *mut c_void,
    size: u64,
) -> u8 {
    guard("buffer_read", || {
        let Some(g) = (unsafe { gpu.as_ref() }) else {
            return 0;
        };
        if g.context.loss.is_lost() {
            report("buffer_read device lost");
            return 0;
        }
        let s = g.state.lock().unwrap();
        let Some(b) = s.buffers.get(&id) else {
            return 0;
        };
        if data.is_null() || size > b.size {
            report("buffer_read bounds");
            return 0;
        }
        let buffer = b.buffer.clone();
        drop(s);
        let aligned = (size + 3) & !3;
        if aligned == 0 {
            return 1;
        }
        let staging = g.context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("host-buffer-read"),
            size: aligned,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            g.context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("host-buffer-read"),
                });
        encoder.copy_buffer_to_buffer(&buffer, 0, &staging, 0, aligned);
        let Some(bytes) = read_staging(g, encoder, &staging) else {
            return 0;
        };
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), size as usize) };
        1
    })
}
