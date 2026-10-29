use std::sync::Arc;
use std::time::Instant;

use d3d8_wgpu::backend::GpuContext;
use d3d8_wgpu::d3d9::api::{
    AlphaInterpretation, ApiErrorKind, BufferHandle, ColorInterpretation, Device, GpuImageLease,
    ImageFormat, Texture2dRequest, TextureRect,
};
use d3d8_wgpu::d3d9::format::D3DFMT_VERTEXDATA;
use d3d8_wgpu::d3d9::resource::{
    BufferKind, BufferLockRequest, BufferRequest, D3DLOCK_READONLY, D3DPOOL_DEFAULT,
    D3DPOOL_MANAGED,
};
use d3d8_wgpu::d3d9::{D3d9Device, DeviceRequest, DisplayMode, PresentParams};

fn supplied_context() -> Result<Arc<GpuContext>, Box<dyn std::error::Error>> {
    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("independent consumer"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    }))?;
    let context = Arc::new(GpuContext::from_supplied(adapter, device, queue)?);
    let loss = context.physical_loss();
    context
        .device
        .set_device_lost_callback(move |reason, message| {
            loss.mark(format!("{reason:?}: {message}"));
        });
    Ok(context)
}

fn triangle(color: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (x, y) in [(8.0f32, 8.0f32), (32.0, 56.0), (56.0, 8.0)] {
        for value in [x, y, 0.5, 1.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&color.to_le_bytes());
    }
    bytes
}

fn textured_triangle() -> Vec<u8> {
    let mut bytes = Vec::new();
    for (x, y) in [(8.0f32, 8.0f32), (32.0, 56.0), (56.0, 8.0)] {
        for value in [x, y, 0.5, 1.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0xffffffffu32.to_le_bytes());
        for value in [0.5f32, 0.5] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

fn texture_workload(
    device: &mut Device,
    present: PresentParams,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = Texture2dRequest {
        width: 4,
        height: 4,
        levels: 3,
        usage: 0,
        format: 21,
        pool: D3DPOOL_MANAGED,
        shared_handle: false,
    };
    let created = device.create_texture_2d(&request)?;
    assert!(!created.no_autogen);
    let texture = created.handle;
    let mip = device.texture_level_descriptor(texture, 1)?;
    assert_eq!(
        (mip.format, mip.width, mip.height, mip.levels),
        (21, 2, 2, 3)
    );
    assert_eq!(
        device
            .texture_level_descriptor(texture, 3)
            .err()
            .unwrap()
            .kind,
        ApiErrorKind::InvalidArgument
    );
    let mut full = device.lock_texture_rect(texture, 0, None, 0)?;
    assert_eq!(
        (full.pitch, full.offset, full.size, full.bytes.len()),
        (16, 0, 64, 64)
    );
    for pixel in full.bytes.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 255, 255]);
    }
    let result = device.unlock_texture_rect(texture, 0, &full.bytes);
    assert!(result.closed);
    result.status?;
    let rect = TextureRect {
        left: 1,
        top: 1,
        right: 3,
        bottom: 3,
    };
    let mut lock = device.lock_texture_rect(texture, 0, Some(rect), 0)?;
    assert_eq!((lock.pitch, lock.offset, lock.size), (16, 20, 24));
    assert_eq!(
        device
            .lock_texture_rect(texture, 0, None, 0)
            .err()
            .unwrap()
            .kind,
        ApiErrorKind::InvalidArgument
    );
    for y in 1..3 {
        for x in 1..3 {
            lock.bytes[(y * 16 + x * 4)..(y * 16 + x * 4 + 4)].copy_from_slice(&[0, 255, 0, 255]);
        }
    }
    let result = device.unlock_texture_rect(texture, 0, &lock.bytes);
    assert!(result.closed);
    result.status?;
    let read = device.lock_texture_rect(texture, 0, None, D3DLOCK_READONLY)?;
    assert_eq!(
        &read.bytes[0..4],
        &[0, 0, 255, 255],
        "outside rect preserved"
    );
    assert_eq!(
        &read.bytes[20..24],
        &[0, 255, 0, 255],
        "rect update preserved"
    );
    let result = device.unlock_texture_rect(texture, 0, &read.bytes);
    assert!(result.closed);
    result.status?;
    let mip = device.lock_texture_rect(texture, 1, None, D3DLOCK_READONLY)?;
    assert_eq!((mip.pitch, mip.bytes.len()), (8, 16));
    assert!(
        mip.bytes.iter().all(|byte| *byte == 0),
        "other mip preserved"
    );
    let result = device.unlock_texture_rect(texture, 1, &mip.bytes);
    assert!(result.closed);
    result.status?;
    let temporary = device
        .create_texture_2d(&Texture2dRequest {
            pool: D3DPOOL_DEFAULT,
            ..request
        })?
        .handle;
    assert!(
        device
            .reset_device(present, DisplayMode::default())
            .is_err()
    );
    device.set_texture(0, Some(temporary))?;
    device.release_texture(temporary)?;
    assert!(
        device
            .reset_device(present, DisplayMode::default())
            .is_err(),
        "released bound DEFAULT texture must block reset"
    );
    device.set_texture(0, None)?;
    device.reset_device(present, DisplayMode::default())?;
    let bytes = textured_triangle();
    let buffer = device.create_buffer(&BufferRequest {
        kind: BufferKind::Vertex,
        size: bytes.len() as u32,
        usage: 0,
        pool: D3DPOOL_MANAGED,
        fvf: 0x144,
        format: D3DFMT_VERTEXDATA,
        shared_handle: false,
    })?;
    device.update_buffer(buffer, &bytes)?;
    device.set_stream(buffer, 0, 28)?;
    device.set_fvf(0x144)?;
    device.set_render_state(22, 1)?;
    device.set_texture(0, Some(texture))?;
    device.set_texture_stage_state(0, 1, 2)?;
    device.set_texture_stage_state(0, 2, 2)?;
    device.set_texture_stage_state(0, 4, 2)?;
    device.set_texture_stage_state(0, 5, 2)?;
    device.set_sampler_state(0, 5, 1)?;
    device.set_sampler_state(0, 6, 1)?;
    device.clear(0xff000000)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0, 1)?;
    let mut lock = device.lock_texture_rect(texture, 0, Some(rect), 0)?;
    for y in 1..3 {
        for x in 1..3 {
            lock.bytes[(y * 16 + x * 4)..(y * 16 + x * 4 + 4)].copy_from_slice(&[0, 0, 255, 255]);
        }
    }
    let result = device.unlock_texture_rect(texture, 0, &lock.bytes);
    assert!(result.closed);
    result.status?;
    device.end_scene()?;
    let image = device.present_image()?;
    let center = (32 * 64 + 32) * 4;
    assert_eq!(
        &image.bytes[center..center + 4],
        &[0, 255, 0, 255],
        "textured triangle after reset"
    );
    device.release_texture(texture)?;
    assert_eq!(
        device
            .texture_level_descriptor(texture, 0)
            .err()
            .unwrap()
            .kind,
        ApiErrorKind::InvalidArgument
    );
    device.clear(0xff000000)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0, 1)?;
    device.end_scene()?;
    let image = device.present_image()?;
    assert_eq!(
        &image.bytes[center..center + 4],
        &[255, 0, 0, 255],
        "released bound texture"
    );
    device.set_texture(0, None)?;
    device.release_buffer(buffer)?;
    Ok(())
}

fn read_lease(lease: &GpuImageLease) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let context = lease.context();
    let texture = lease.texture_for(context)?;
    context.synchronize_d3d9();
    let row = (lease.width() * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = context.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("consumer lease readback"),
        size: u64::from(row) * u64::from(lease.height()),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = context
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("consumer lease copy"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(lease.height()),
            },
        },
        wgpu::Extent3d {
            width: lease.width(),
            height: lease.height(),
            depth_or_array_layers: 1,
        },
    );
    context.queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    context.device.poll(wgpu::PollType::wait_indefinitely())?;
    rx.recv()??;
    let mapped = slice.get_mapped_range()?;
    let bytes = mapped
        .chunks_exact(row as usize)
        .flat_map(|row| row[..lease.width() as usize * 4].iter().copied())
        .collect();
    drop(mapped);
    buffer.unmap();
    Ok(bytes)
}

fn draw(
    device: &mut Device,
    buffer: BufferHandle,
    color: u32,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    device.update_buffer(buffer, &triangle(color))?;
    let (_, mut bytes) = device.lock_buffer(
        buffer,
        &BufferLockRequest {
            offset: 0,
            size: 0,
            flags: 0,
        },
    )?;
    bytes.copy_from_slice(&triangle(color));
    assert!(device.unlock_buffer(buffer, &bytes)?);
    device.set_stream(buffer, 0, 20)?;
    device.set_fvf(0x44)?;
    device.set_render_state(22, 1)?;
    device.clear(0xff000000)?;
    device.begin_scene()?;
    device.draw_primitive(4, 0, 1)?;
    device.end_scene()?;
    let image = device.present_image()?;
    assert_eq!((image.width, image.height), (64, 64));
    assert_eq!(image.format, ImageFormat::Rgba8Unorm);
    assert_eq!(image.alpha, AlphaInterpretation::Unspecified);
    assert_eq!(image.color, ColorInterpretation::UnormNoTransferConversion);
    assert_eq!(&image.bytes[..4], &[0, 0, 0, 255], "image background");
    Ok(image.bytes)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let present = PresentParams {
        width: 64,
        height: 64,
        format: 21,
        count: 1,
        swap_effect: 1,
        windowed: 1,
        ..Default::default()
    };
    let request = DeviceRequest {
        adapter: 0,
        device_type: 1,
        behavior_flags: 0x40,
        present,
        display_mode: DisplayMode::default(),
    };
    let start = Instant::now();
    let (owned, _) = D3d9Device::new_owned(&request)?;
    let context = supplied_context()?;
    let (supplied, _) = D3d9Device::new_supplied(&request, context.clone())?;
    let (shared, _) = D3d9Device::new_supplied(&request, context.clone())?;
    let create_time = start.elapsed();
    let mut owned = Device::new(owned);
    let mut supplied = Device::new(supplied);
    let mut shared = Device::new(shared);
    let buffer_request = BufferRequest {
        kind: BufferKind::Vertex,
        size: 60,
        usage: 0,
        pool: D3DPOOL_MANAGED,
        fvf: 0x44,
        format: D3DFMT_VERTEXDATA,
        shared_handle: false,
    };
    let first = owned.create_buffer(&buffer_request)?;
    let second = supplied.create_buffer(&buffer_request)?;
    let third = shared.create_buffer(&buffer_request)?;
    assert_eq!(
        supplied.set_stream(first, 0, 20).unwrap_err().kind,
        ApiErrorKind::InvalidArgument
    );
    assert_eq!(
        owned.update_buffer(first, &[0; 1]).unwrap_err().kind,
        ApiErrorKind::InvalidArgument
    );
    let start = Instant::now();
    let red = draw(&mut owned, first, 0xffff0000)?;
    let green = draw(&mut supplied, second, 0xff00ff00)?;
    let draw_time = start.elapsed();
    let start = Instant::now();
    let cpu_snapshot = supplied.present_image()?;
    let cpu_present_time = start.elapsed();
    let start = Instant::now();
    let lease = supplied.present_gpu_image()?;
    let lease_time = start.elapsed();
    assert_eq!((lease.width(), lease.height()), (64, 64));
    assert_eq!(lease.format(), ImageFormat::Rgba8Unorm);
    assert_eq!(lease.alpha(), AlphaInterpretation::Unspecified);
    assert_eq!(
        lease.color(),
        ColorInterpretation::UnormNoTransferConversion
    );
    assert_eq!(read_lease(&lease)?, cpu_snapshot.bytes);
    draw(&mut supplied, second, 0xff0000ff)?;
    draw(&mut supplied, second, 0xffff0000)?;
    assert_eq!(read_lease(&lease)?, green, "lease after later frames");
    let center = (32 * 64 + 32) * 4;
    assert_eq!(
        &red[center..center + 4],
        &[255, 0, 0, 255],
        "owned image center"
    );
    assert_eq!(
        &green[center..center + 4],
        &[0, 255, 0, 255],
        "supplied image center"
    );
    let temporary = owned.create_buffer(&BufferRequest {
        pool: D3DPOOL_DEFAULT,
        ..buffer_request
    })?;
    assert!(owned.reset_device(present, DisplayMode::default()).is_err());
    owned.release_buffer(temporary)?;
    let start = Instant::now();
    owned.reset_device(present, DisplayMode::default())?;
    supplied.reset_device(present, DisplayMode::default())?;
    let reset_time = start.elapsed();
    assert_eq!(read_lease(&lease)?, green, "lease after reset");
    let blue = draw(&mut shared, third, 0xff0000ff)?;
    let retained = shared.present_gpu_image()?;
    assert_eq!(&blue[center..center + 4], &[0, 0, 255, 255]);
    assert_eq!(
        &draw(&mut owned, first, 0xffff0000)?[center..center + 4],
        &[255, 0, 0, 255]
    );
    assert_eq!(
        &draw(&mut supplied, second, 0xff00ff00)?[center..center + 4],
        &[0, 255, 0, 255]
    );
    let start = Instant::now();
    texture_workload(&mut supplied, present)?;
    let texture_time = start.elapsed();
    owned.release_buffer(first)?;
    assert!(owned.set_stream(first, 0, 20).is_err());
    drop(owned);
    shared.release_buffer(third)?;
    drop(shared);
    assert_eq!(read_lease(&retained)?, blue, "lease after renderer drop");
    drop(retained);
    let foreign = supplied_context()?;
    assert_eq!(
        lease.texture_for(&foreign).unwrap_err().kind,
        ApiErrorKind::InvalidArgument
    );
    drop(foreign);
    assert_eq!(
        &draw(&mut supplied, second, 0xff00ff00)?[center..center + 4],
        &[0, 255, 0, 255]
    );
    let (shared, _) = D3d9Device::new_supplied(&request, context.clone())?;
    let mut shared = Device::new(shared);
    let third = shared.create_buffer(&buffer_request)?;
    let recover_texture = supplied
        .create_texture_2d(&Texture2dRequest {
            width: 2,
            height: 2,
            levels: 1,
            usage: 0,
            format: 21,
            pool: D3DPOOL_MANAGED,
            shared_handle: false,
        })?
        .handle;
    let mut recover_lock = supplied.lock_texture_rect(recover_texture, 0, None, 0)?;
    recover_lock.bytes.fill(0x5a);
    let result = supplied.unlock_texture_rect(recover_texture, 0, &recover_lock.bytes);
    assert!(result.closed);
    result.status?;
    supplied.begin_scene()?;
    shared.begin_scene()?;
    context
        .physical_loss()
        .mark("consumer simulated loss".into());
    assert_eq!(
        supplied.draw_primitive(4, 0, 1).unwrap_err().kind,
        ApiErrorKind::DeviceLost
    );
    assert_eq!(
        shared.draw_primitive(4, 0, 1).unwrap_err().kind,
        ApiErrorKind::DeviceLost
    );
    assert!(draw(&mut supplied, second, 0xff00ff00).is_err());
    assert!(draw(&mut shared, third, 0xff0000ff).is_err());
    assert!(
        supplied
            .reset_device(present, DisplayMode::default())
            .is_err()
    );
    assert!(
        shared
            .reset_device(present, DisplayMode::default())
            .is_err()
    );
    let replacement = supplied_context()?;
    supplied.supply_replacement(replacement.clone())?;
    shared.supply_replacement(replacement.clone())?;
    supplied.reset_device(present, DisplayMode::default())?;
    shared.reset_device(present, DisplayMode::default())?;
    let recovered = supplied.lock_texture_rect(recover_texture, 0, None, D3DLOCK_READONLY)?;
    assert_eq!(
        recovered.bytes, recover_lock.bytes,
        "managed texture after replacement"
    );
    let result = supplied.unlock_texture_rect(recover_texture, 0, &recovered.bytes);
    assert!(result.closed);
    result.status?;
    let vertices = textured_triangle();
    let buffer = supplied.create_buffer(&BufferRequest {
        kind: BufferKind::Vertex,
        size: vertices.len() as u32,
        usage: 0,
        pool: D3DPOOL_MANAGED,
        fvf: 0x144,
        format: D3DFMT_VERTEXDATA,
        shared_handle: false,
    })?;
    supplied.update_buffer(buffer, &vertices)?;
    supplied.set_stream(buffer, 0, 28)?;
    supplied.set_fvf(0x144)?;
    supplied.set_render_state(22, 1)?;
    supplied.set_texture(0, Some(recover_texture))?;
    supplied.set_texture_stage_state(0, 1, 2)?;
    supplied.set_texture_stage_state(0, 2, 2)?;
    supplied.set_texture_stage_state(0, 4, 2)?;
    supplied.set_texture_stage_state(0, 5, 2)?;
    supplied.set_sampler_state(0, 5, 1)?;
    supplied.set_sampler_state(0, 6, 1)?;
    supplied.clear(0xff000000)?;
    supplied.begin_scene()?;
    supplied.draw_primitive(4, 0, 1)?;
    supplied.end_scene()?;
    let image = supplied.present_image()?;
    assert_eq!(
        &image.bytes[center..center + 4],
        &[0x5a, 0x5a, 0x5a, 0x5a],
        "managed texture sampled after replacement"
    );
    supplied.set_texture(0, None)?;
    supplied.release_texture(recover_texture)?;
    supplied.release_buffer(buffer)?;
    assert_eq!(
        &draw(&mut supplied, second, 0xff00ff00)?[center..center + 4],
        &[0, 255, 0, 255],
        "supplied image after replacement"
    );
    assert_eq!(
        &draw(&mut shared, third, 0xff0000ff)?[center..center + 4],
        &[0, 0, 255, 255],
        "shared image after replacement"
    );
    assert_eq!(&red[center..center + 4], &[255, 0, 0, 255]);
    drop(supplied);
    drop(shared);
    drop(replacement);
    assert_eq!(
        lease.texture_for(&context).unwrap_err().kind,
        ApiErrorKind::DeviceLost
    );
    drop(context);
    drop(lease);
    println!(
        "D3D9 GPU consumer passed: three devices create={create_time:?}, two draw/readbacks={draw_time:?}, two resets={reset_time:?}, texture lifecycle/draw/readback={texture_time:?}, CPU present={cpu_present_time:?}, GPU lease={lease_time:?}, CPU bytes/frame={}, GPU lease copies/frame=1",
        red.len()
    );
    Ok(())
}
