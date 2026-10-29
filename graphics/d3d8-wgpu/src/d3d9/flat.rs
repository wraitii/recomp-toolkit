//! C-ABI entry points for the hottest D3D9 and D3DX effect methods. Each returns
//! the `StatusCode` value and parks a failing `Status` on the device or effect for
//! `d3d9_take_last_error`/`d3dx_effect_take_last_error`, so a successful call builds
//! no `Status` and crosses no cxx shim. A panic aborts here as it does in cxx.

use super::d3dx_effect::{
    D3dxEffect, d3dx_effect_begin, d3dx_effect_end, d3dx_effect_end_pass, d3dx_effect_set,
    d3dx_effect_set_texture,
};
use super::*;

fn park(slot: &mut Option<Status>, status: Status) -> u32 {
    let code = u32::from(status.code.repr);
    if status.code != StatusCode::Ok {
        *slot = Some(status);
    }
    code
}

fn park_value(slot: &mut Option<Status>, result: ValueResult, value: &mut u32) -> u32 {
    *value = result.value;
    park(slot, result.status)
}

unsafe fn bytes<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    }
}

fn taken(slot: &mut Option<Status>, what: &str) -> Status {
    slot.take().unwrap_or_else(|| {
        status(
            StatusCode::Backend,
            format!("{what}: no failure is stored for this call"),
        )
    })
}

pub fn d3d9_take_last_error(device: &mut D3d9Device) -> Status {
    taken(&mut device.last_error, "D3D9")
}

pub fn d3dx_effect_take_last_error(effect: &mut D3dxEffect) -> Status {
    taken(&mut effect.last_error, "ID3DXEffect")
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_set_render_state(device: &mut D3d9Device, state: u32, value: u32) -> u32 {
    let result = d3d9_set_render_state(device, state, value);
    park(&mut device.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_set_texture_stage_state(
    device: &mut D3d9Device,
    stage: u32,
    state: u32,
    value: u32,
) -> u32 {
    let result = d3d9_set_texture_stage_state(device, stage, state, value);
    park(&mut device.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_set_sampler_state(
    device: &mut D3d9Device,
    sampler: u32,
    state: u32,
    value: u32,
) -> u32 {
    let result = d3d9_set_sampler_state(device, sampler, state, value);
    park(&mut device.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_set_stream_source(
    device: &mut D3d9Device,
    stream: u32,
    buffer: u32,
    offset: u32,
    stride: u32,
) -> u32 {
    let result = d3d9_set_stream_source(device, stream, buffer, offset, stride);
    park(&mut device.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_set_indices(device: &mut D3d9Device, id: u32) -> u32 {
    let result = d3d9_set_indices(device, id);
    park(&mut device.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_set_vertex_declaration(device: &mut D3d9Device, handle: u32) -> u32 {
    let result = d3d9_set_vertex_declaration(device, handle);
    park(&mut device.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_draw_primitive(
    device: &mut D3d9Device,
    primitive: u32,
    start_vertex: u32,
    primitive_count: u32,
) -> u32 {
    let result = d3d9_draw_primitive(device, primitive, start_vertex, primitive_count);
    park(&mut device.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3d9_draw_indexed_primitive(
    device: &mut D3d9Device,
    primitive: u32,
    base_vertex: i32,
    min_index: u32,
    num_vertices: u32,
    start_index: u32,
    primitive_count: u32,
) -> u32 {
    let result = d3d9_draw_indexed_primitive(
        device,
        primitive,
        base_vertex,
        min_index,
        num_vertices,
        start_index,
        primitive_count,
    );
    park(&mut device.last_error, result)
}

/// # Safety
/// `vertices` must point to `len` readable bytes when `len` is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rk_d3d9_draw_primitive_up(
    device: &mut D3d9Device,
    primitive: u32,
    primitive_count: u32,
    vertices: *const u8,
    len: usize,
    stride: u32,
) -> u32 {
    let vertices = unsafe { bytes(vertices, len) };
    let result = d3d9_draw_primitive_up(device, primitive, primitive_count, vertices, stride);
    park(&mut device.last_error, result)
}

/// # Safety
/// `data` must point to `len` writable bytes when `len` is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rk_d3d9_query_get_data(
    device: &mut D3d9Device,
    handle: u32,
    data: *mut u8,
    len: usize,
    size: u32,
    flags: u32,
    value: &mut u32,
) -> u32 {
    let data = if len == 0 {
        &mut []
    } else {
        unsafe { std::slice::from_raw_parts_mut(data, len) }
    };
    let result = d3d9_query_get_data(device, handle, data, size, flags);
    park_value(&mut device.last_error, result, value)
}

/// # Safety
/// `data` must point to `len` readable bytes when `len` is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rk_d3dx_effect_set(
    effect: &mut D3dxEffect,
    handle: u32,
    setter: u32,
    data: *const u8,
    len: usize,
) -> u32 {
    let data = unsafe { bytes(data, len) };
    let result = d3dx_effect_set(effect, handle, setter, data);
    park(&mut effect.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3dx_effect_set_texture(
    effect: &mut D3dxEffect,
    handle: u32,
    id: u32,
    old_id: &mut u32,
) -> u32 {
    let result = d3dx_effect_set_texture(effect, handle, id);
    park_value(&mut effect.last_error, result, old_id)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3dx_effect_begin(
    effect: &mut D3dxEffect,
    device: &mut D3d9Device,
    flags: u32,
    passes: &mut u32,
) -> u32 {
    let result = d3dx_effect_begin(effect, device, flags);
    park_value(&mut effect.last_error, result, passes)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3dx_effect_end_pass(effect: &mut D3dxEffect) -> u32 {
    let result = d3dx_effect_end_pass(effect);
    park(&mut effect.last_error, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn rk_d3dx_effect_end(effect: &mut D3dxEffect, device: &mut D3d9Device) -> u32 {
    let result = d3dx_effect_end(effect, device);
    park(&mut effect.last_error, result)
}
