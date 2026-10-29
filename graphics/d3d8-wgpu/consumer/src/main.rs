use d3d8_wgpu::RenderErrorKind;
use d3d8_wgpu::d3d9::format::{D3DFMT_INDEX16, D3DFMT_VERTEXDATA};
use d3d8_wgpu::d3d9::resource::{
    BufferKind, BufferLockRequest, BufferRequest, D3DLOCK_DONOTWAIT, D3DLOCK_READONLY,
    D3DPOOL_DEFAULT, D3DPOOL_SCRATCH, D3DPOOL_SYSTEMMEM, D3d9Resource,
};
use d3d8_wgpu::d3d9::{
    D3d9Device, DeviceCreateErrorKind, DeviceRequest, DisplayMode, PresentParams,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let present = PresentParams {
        width: 64,
        ..Default::default()
    };
    let error = D3d9Device::new_owned(&DeviceRequest {
        adapter: 1,
        device_type: 0,
        behavior_flags: 0,
        present,
        display_mode: DisplayMode::default(),
    })
    .err()
    .expect("invalid adapter must fail before GPU initialization");
    assert_eq!(error.kind, DeviceCreateErrorKind::InvalidArgument);
    assert_eq!(error.present.width, present.width);
    let request = BufferRequest {
        kind: BufferKind::Vertex,
        size: 16,
        usage: 0,
        pool: D3DPOOL_SYSTEMMEM,
        fvf: 0,
        format: D3DFMT_VERTEXDATA,
        shared_handle: false,
    };
    let mut resource = D3d9Resource::new_cpu_buffer(&request)?;
    let lock = BufferLockRequest {
        offset: 4,
        size: 4,
        flags: 0,
    };
    let range = resource.commit_buffer_lock(&lock)?;
    assert_eq!((range.offset, range.size, range.total), (4, 4, 16));
    resource.commit_buffer_lock(&BufferLockRequest {
        flags: D3DLOCK_READONLY,
        ..lock
    })?;
    assert_eq!(
        resource
            .prepare_buffer_lock(&BufferLockRequest {
                flags: D3DLOCK_DONOTWAIT,
                ..lock
            })
            .unwrap_err()
            .kind,
        RenderErrorKind::InvalidArgument
    );
    assert_eq!(
        resource.sync_locked_buffer(&[0; 1]).unwrap_err().kind,
        RenderErrorKind::InvalidArgument
    );
    let mut bytes = resource.buffer_bytes().to_vec();
    bytes[4..8].copy_from_slice(&[1, 2, 3, 4]);
    bytes[12] = 5;
    resource.sync_locked_buffer(&bytes)?;
    assert_eq!(resource.buffer_bytes(), bytes);
    let nested = resource.unlock_buffer(&bytes);
    nested.status?;
    assert!(!nested.closed);
    let last = resource.unlock_buffer(&bytes);
    last.status?;
    assert!(last.closed);
    assert_eq!(
        D3d9Resource::new_cpu_buffer(&BufferRequest {
            pool: D3DPOOL_DEFAULT,
            ..request
        })
        .err()
        .unwrap()
        .kind,
        RenderErrorKind::InvalidArgument
    );
    let scratch = D3d9Resource::new_cpu_buffer(&BufferRequest {
        kind: BufferKind::Index,
        format: D3DFMT_INDEX16,
        pool: D3DPOOL_SCRATCH,
        ..request
    })?;
    drop(scratch);
    drop(resource);
    println!("D3D9 CPU buffer consumer passed");
    Ok(())
}
