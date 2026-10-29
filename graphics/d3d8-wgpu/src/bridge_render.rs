use crate::RenderError;
use crate::backend::GpuContext;
use crate::bridge::{
    AdapterInfo, AdapterInfoResult, Device, FrameHandoff, FrameHandoffResult, Light, LightResult,
    Material, MaterialResult, Rect, Status, StatusCode, TextureLevel, Viewport, ViewportResult,
    status, status_from_error,
};
use crate::d3d8::{
    device::{Device as RenderDevice, TextureLevelUpload},
    math::Mat4,
    resource::IndexedDraw,
    state::{
        Light as RenderLight, LitInput, Material as RenderMaterial, Viewport as RenderViewport,
    },
};

fn gpu_error(error: RenderError) -> Status {
    status(StatusCode::Backend, error.to_string())
}

fn status_from_error_or_ok(result: Result<(), RenderError>) -> Status {
    result.map_or_else(status_from_error, |_| status(StatusCode::Ok, ""))
}

fn renderer<'a>(device: &'a Device, operation: &str) -> Result<&'a RenderDevice, Status> {
    device.gpu.as_ref().ok_or_else(|| {
        status(
            StatusCode::Backend,
            format!("{operation}: GPU renderer is not initialized"),
        )
    })
}

fn renderer_mut<'a>(
    device: &'a mut Device,
    operation: &str,
) -> Result<&'a mut RenderDevice, Status> {
    device.gpu.as_mut().ok_or_else(|| {
        status(
            StatusCode::Backend,
            format!("{operation}: GPU renderer is not initialized"),
        )
    })
}

fn empty_light() -> Light {
    Light {
        light_type: 0,
        diffuse_r: 0.0,
        diffuse_g: 0.0,
        diffuse_b: 0.0,
        diffuse_a: 0.0,
        specular_r: 0.0,
        specular_g: 0.0,
        specular_b: 0.0,
        specular_a: 0.0,
        ambient_r: 0.0,
        ambient_g: 0.0,
        ambient_b: 0.0,
        ambient_a: 0.0,
        position_x: 0.0,
        position_y: 0.0,
        position_z: 0.0,
        direction_x: 0.0,
        direction_y: 0.0,
        direction_z: 0.0,
        range: 0.0,
        falloff: 0.0,
        attenuation0: 0.0,
        attenuation1: 0.0,
        attenuation2: 0.0,
        theta: 0.0,
        phi: 0.0,
    }
}

fn empty_frame() -> FrameHandoff {
    FrameHandoff {
        texture: std::ptr::null_mut(),
        lease: 0,
        width: 0,
        height: 0,
    }
}

fn adapter_info_from_gpu(gpu: &GpuContext) -> AdapterInfo {
    let info = &gpu.adapter_info;
    let limits = gpu.device.limits();
    AdapterInfo {
        name: info.name.clone(),
        vendor_id: info.vendor,
        device_id: info.device,
        max_texture_dimension_2d: limits.max_texture_dimension_2d,
        max_bind_groups: limits.max_bind_groups,
        max_vertex_buffers: limits.max_vertex_buffers,
        max_vertex_attributes: limits.max_vertex_attributes,
    }
}

pub(crate) fn adapter_info() -> AdapterInfoResult {
    match pollster::block_on(GpuContext::new_headless()) {
        Ok(gpu) => AdapterInfoResult {
            status: status(StatusCode::Ok, ""),
            info: adapter_info_from_gpu(&gpu),
        },
        Err(error) => AdapterInfoResult {
            status: gpu_error(error),
            info: AdapterInfo {
                name: String::new(),
                vendor_id: 0,
                device_id: 0,
                max_texture_dimension_2d: 0,
                max_bind_groups: 0,
                max_vertex_buffers: 0,
                max_vertex_attributes: 0,
            },
        },
    }
}

pub(crate) fn clear(
    device: &mut Device,
    rects: &[Rect],
    flags: u32,
    color: u32,
    z: f32,
    stencil: u32,
) -> Status {
    let mut internal_rects = Vec::new();
    if internal_rects.try_reserve_exact(rects.len()).is_err() {
        return status(
            StatusCode::OutOfMemory,
            "clear: rectangle allocation failed",
        );
    }
    internal_rects.extend(rects.iter().map(|rect| crate::d3d8::state::Rect {
        x1: rect.x1,
        y1: rect.y1,
        x2: rect.x2,
        y2: rect.y2,
    }));
    match renderer_mut(device, "clear") {
        Ok(renderer) => {
            status_from_error_or_ok(renderer.clear(&internal_rects, flags, color, z, stencil))
        }
        Err(status) => status,
    }
}

pub(crate) fn begin_scene(device: &mut Device) -> Status {
    match renderer_mut(device, "begin_scene") {
        Ok(renderer) => status_from_error_or_ok(renderer.begin_scene()),
        Err(status) => status,
    }
}

pub(crate) fn end_scene(device: &mut Device) -> Status {
    match renderer_mut(device, "end_scene") {
        Ok(renderer) => status_from_error_or_ok(renderer.end_scene()),
        Err(status) => status,
    }
}

pub(crate) fn set_render_state(device: &mut Device, state: u32, value: u32) -> Status {
    device
        .state_mut()
        .set_render_state(state, value)
        .map_or_else(status_from_error, |_| status(StatusCode::Ok, ""))
}

pub(crate) fn set_texture_stage_state(
    device: &mut Device,
    stage: u32,
    state: u32,
    value: u32,
) -> Status {
    device
        .state_mut()
        .set_texture_stage_state(stage, state, value)
        .map_or_else(status_from_error, |_| status(StatusCode::Ok, ""))
}

pub(crate) fn set_texture(
    device: &mut Device,
    stage: u32,
    texture_id: u32,
    format: u32,
    levels: &[TextureLevel<'_>],
) -> Status {
    let mut uploads = Vec::new();
    if uploads.try_reserve_exact(levels.len()).is_err() {
        return status(
            StatusCode::OutOfMemory,
            "set_texture: upload descriptor allocation failed",
        );
    }
    for level in levels {
        uploads.push(TextureLevelUpload {
            level: level.level,
            generation: level.generation,
            force_upload: level.force_upload,
            width: level.width,
            height: level.height,
            data: level.data,
        });
    }
    match renderer_mut(device, "set_texture") {
        Ok(renderer) => {
            status_from_error_or_ok(renderer.set_texture(stage, texture_id, format, &uploads))
        }
        Err(status) => status,
    }
}

pub(crate) fn set_render_target(
    device: &mut Device,
    id: u32,
    level: u32,
    generation: u64,
    format: u32,
    width: u32,
    height: u32,
    data: &[u8],
    depth: bool,
    depth_id: u32,
    depth_width: u32,
    depth_height: u32,
    depth_format: u32,
    change_color: bool,
) -> Status {
    let renderer = match renderer_mut(device, "set_render_target") {
        Ok(renderer) => renderer,
        Err(status) => return status,
    };
    let attachment = if depth {
        match renderer.depth_surface(depth_id, depth_width, depth_height, depth_format) {
            Ok(attachment) => Some(attachment),
            Err(error) => return status_from_error(error),
        }
    } else {
        None
    };
    status_from_error_or_ok(renderer.set_render_target_depth(
        id,
        level,
        generation,
        format,
        width,
        height,
        data,
        attachment,
        change_color,
    ))
}

pub(crate) fn read_texture(
    device: &Device,
    id: u32,
    level: u32,
    generation: u64,
    out: &mut [u8],
) -> Status {
    match renderer(device, "read_texture") {
        Ok(renderer) => match renderer.read_texture_if_current(id, level, generation) {
            Ok(Some(bytes)) => match out.get_mut(..bytes.len()) {
                Some(destination) => {
                    destination.copy_from_slice(&bytes);
                    status(StatusCode::Ok, "")
                }
                None => status(
                    StatusCode::InvalidArgument,
                    "read_texture: output buffer too small",
                ),
            },
            Ok(None) => status(StatusCode::Ok, ""),
            Err(error) => status_from_error(error),
        },
        Err(status) => status,
    }
}

pub(crate) fn release_texture(device: &mut Device, texture_id: u32) {
    if let Some(renderer) = device.gpu.as_mut() {
        renderer.release_texture(texture_id);
    }
}

pub(crate) fn set_transform(device: &mut Device, kind: u32, values: &[f32]) -> Status {
    if values.len() != 16 {
        return status(
            StatusCode::InvalidArgument,
            "set_transform: expected 16 floats",
        );
    }
    let mut rows = [[0.0; 4]; 4];
    for (index, value) in values.iter().enumerate() {
        rows[index / 4][index % 4] = *value;
    }
    device
        .state_mut()
        .set_transform(kind, Mat4 { rows })
        .map_or_else(status_from_error, |_| status(StatusCode::Ok, ""))
}

pub(crate) fn set_viewport(device: &mut Device, viewport: &Viewport) -> Status {
    let value = RenderViewport {
        x: viewport.x,
        y: viewport.y,
        width: viewport.width,
        height: viewport.height,
        min_z: viewport.min_z,
        max_z: viewport.max_z,
    };
    if let Err(error) = device.state_mut().set_viewport(value) {
        return status_from_error(error);
    }
    device.viewport_was_set = true;
    status(StatusCode::Ok, "")
}

pub(crate) fn get_viewport(device: &Device) -> ViewportResult {
    let viewport = device.state().viewport;
    ViewportResult {
        status: status(StatusCode::Ok, ""),
        viewport: Viewport {
            x: viewport.x,
            y: viewport.y,
            width: viewport.width,
            height: viewport.height,
            min_z: viewport.min_z,
            max_z: viewport.max_z,
        },
    }
}

fn render_material(value: &Material) -> RenderMaterial {
    RenderMaterial {
        diffuse: [
            value.diffuse_r,
            value.diffuse_g,
            value.diffuse_b,
            value.diffuse_a,
        ],
        ambient: [
            value.ambient_r,
            value.ambient_g,
            value.ambient_b,
            value.ambient_a,
        ],
        specular: [
            value.specular_r,
            value.specular_g,
            value.specular_b,
            value.specular_a,
        ],
        emissive: [
            value.emissive_r,
            value.emissive_g,
            value.emissive_b,
            value.emissive_a,
        ],
        power: value.power,
    }
}

fn bridge_material(value: RenderMaterial) -> Material {
    Material {
        diffuse_r: value.diffuse[0],
        diffuse_g: value.diffuse[1],
        diffuse_b: value.diffuse[2],
        diffuse_a: value.diffuse[3],
        ambient_r: value.ambient[0],
        ambient_g: value.ambient[1],
        ambient_b: value.ambient[2],
        ambient_a: value.ambient[3],
        specular_r: value.specular[0],
        specular_g: value.specular[1],
        specular_b: value.specular[2],
        specular_a: value.specular[3],
        emissive_r: value.emissive[0],
        emissive_g: value.emissive[1],
        emissive_b: value.emissive[2],
        emissive_a: value.emissive[3],
        power: value.power,
    }
}

pub(crate) fn set_material(device: &mut Device, material: &Material) -> Status {
    device.state_mut().set_material(render_material(material));
    status(StatusCode::Ok, "")
}

pub(crate) fn get_material(device: &Device) -> MaterialResult {
    MaterialResult {
        status: status(StatusCode::Ok, ""),
        material: bridge_material(device.state().material()),
    }
}

fn render_light(value: &Light) -> RenderLight {
    RenderLight {
        light_type: value.light_type,
        diffuse: [
            value.diffuse_r,
            value.diffuse_g,
            value.diffuse_b,
            value.diffuse_a,
        ],
        ambient: [
            value.ambient_r,
            value.ambient_g,
            value.ambient_b,
            value.ambient_a,
        ],
        specular: [
            value.specular_r,
            value.specular_g,
            value.specular_b,
            value.specular_a,
        ],
        position: [value.position_x, value.position_y, value.position_z],
        direction: [value.direction_x, value.direction_y, value.direction_z],
        range: value.range,
        falloff: value.falloff,
        attenuation0: value.attenuation0,
        attenuation1: value.attenuation1,
        attenuation2: value.attenuation2,
        theta: value.theta,
        phi: value.phi,
    }
}

fn bridge_light(value: RenderLight) -> Light {
    Light {
        light_type: value.light_type,
        diffuse_r: value.diffuse[0],
        diffuse_g: value.diffuse[1],
        diffuse_b: value.diffuse[2],
        diffuse_a: value.diffuse[3],
        ambient_r: value.ambient[0],
        ambient_g: value.ambient[1],
        ambient_b: value.ambient[2],
        ambient_a: value.ambient[3],
        specular_r: value.specular[0],
        specular_g: value.specular[1],
        specular_b: value.specular[2],
        specular_a: value.specular[3],
        position_x: value.position[0],
        position_y: value.position[1],
        position_z: value.position[2],
        direction_x: value.direction[0],
        direction_y: value.direction[1],
        direction_z: value.direction[2],
        range: value.range,
        falloff: value.falloff,
        attenuation0: value.attenuation0,
        attenuation1: value.attenuation1,
        attenuation2: value.attenuation2,
        theta: value.theta,
        phi: value.phi,
    }
}

pub(crate) fn set_light(device: &mut Device, index: u32, light: &Light) -> Status {
    device
        .state_mut()
        .set_light(index, render_light(light))
        .map_or_else(status_from_error, |_| status(StatusCode::Ok, ""))
}

pub(crate) fn get_light(device: &Device, index: u32) -> LightResult {
    match device.state().light(index) {
        Ok(light) => LightResult {
            status: status(StatusCode::Ok, ""),
            light: bridge_light(light),
        },
        Err(error) => LightResult {
            status: status_from_error(error),
            light: empty_light(),
        },
    }
}

pub(crate) fn light_enable(device: &mut Device, index: u32, enabled: bool) -> Status {
    device
        .state_mut()
        .light_enable(index, enabled)
        .map_or_else(status_from_error, |_| status(StatusCode::Ok, ""))
}

pub(crate) fn get_light_enable(device: &Device, index: u32) -> crate::bridge::ValueResult {
    match device.state().light_enabled(index) {
        Ok(enabled) => crate::bridge::ValueResult {
            status: status(StatusCode::Ok, ""),
            value: u32::from(enabled),
        },
        Err(error) => crate::bridge::ValueResult {
            status: status_from_error(error),
            value: 0,
        },
    }
}

pub(crate) fn draw_primitive(
    device: &mut Device,
    topology: u32,
    fvf: u32,
    vertices: &[u8],
    stride: u32,
    start_vertex: u32,
    primitive_count: u32,
) -> Status {
    let buffer = match crate::d3d8::resource::VertexBuffer::borrowed(vertices, stride) {
        Ok(buffer) => buffer,
        Err(error) => return status_from_error(error),
    };
    match renderer_mut(device, "draw_primitive") {
        Ok(renderer) => status_from_error_or_ok(renderer.draw_primitive(
            topology,
            fvf,
            &buffer,
            start_vertex,
            primitive_count,
        )),
        Err(status) => status,
    }
}

pub(crate) fn draw_indexed_primitive(
    device: &mut Device,
    topology: u32,
    fvf: u32,
    vertices: &[u8],
    stride: u32,
    indices: &[u8],
    index_format: u32,
    base_vertex: u32,
    min_index: u32,
    num_vertices: u32,
    start_index: u32,
    primitive_count: u32,
) -> Status {
    if vertices.is_empty() || indices.is_empty() {
        return status(
            StatusCode::InvalidArgument,
            "draw_indexed_primitive: null or empty buffers",
        );
    }
    let draw = IndexedDraw {
        topology,
        index_format,
        stride,
        base_vertex,
        min_index,
        num_vertices,
        start_index,
        primitive_count,
    };
    match renderer_mut(device, "draw_indexed_primitive") {
        Ok(renderer) => status_from_error_or_ok(
            renderer.draw_indexed_primitive(topology, fvf, vertices, indices, draw),
        ),
        Err(status) => status,
    }
}

pub(crate) fn process_vertices(
    device: &mut Device,
    src: &[u8],
    src_fvf: u32,
    src_stride: u32,
    dest: &mut [u8],
    dest_fvf: u32,
    src_start: u32,
    dest_index: u32,
    count: u32,
) -> Status {
    if src.is_empty() || dest.is_empty() {
        return status(
            StatusCode::InvalidArgument,
            "process_vertices: null or empty source/destination",
        );
    }
    let layout = match LitInput::from_fvf(src_fvf) {
        Ok(layout) => layout,
        Err(error) => return status_from_error(error),
    };
    if src_stride as usize != layout.stride {
        return status(
            StatusCode::InvalidArgument,
            format!(
                "process_vertices: stream stride {src_stride} does not match FVF {src_fvf:#06x} stride {}",
                layout.stride
            ),
        );
    }
    device
        .state_mut()
        .trace_process_vertices(src_fvf, dest_fvf, src_stride, count);
    status_from_error_or_ok(device.state_mut().process_vertices(
        src,
        src_start as usize,
        count as usize,
        layout,
        dest_fvf,
        dest_index as usize,
        dest,
    ))
}

pub(crate) fn read_pixels_len(device: &Device, out: &mut u64) -> Status {
    match renderer(device, "read_pixels") {
        Ok(renderer) => {
            *out = renderer.read_pixels_len();
            status(StatusCode::Ok, "")
        }
        Err(status) => status,
    }
}

pub(crate) fn read_pixels(device: &Device, out: &mut [u8]) -> Status {
    match renderer(device, "read_pixels") {
        Ok(renderer) => status_from_error_or_ok(renderer.read_pixels_into(out)),
        Err(status) => status,
    }
}

pub(crate) fn write_pixels(
    device: &mut Device,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Status {
    match renderer_mut(device, "write_pixels") {
        Ok(renderer) => status_from_error_or_ok(renderer.write_pixels(x, y, width, height, rgba)),
        Err(status) => status,
    }
}

pub(crate) fn present(device: &mut Device) -> Status {
    match renderer_mut(device, "present") {
        Ok(renderer) => status_from_error_or_ok(renderer.present()),
        Err(status) => status,
    }
}

pub(crate) fn present_handoff(device: &Device, bpp: u32) -> FrameHandoffResult {
    match renderer(device, "present_handoff") {
        Ok(renderer) => match renderer.present_surface_handoff(bpp) {
            Ok(frame) => FrameHandoffResult {
                status: status(StatusCode::Ok, ""),
                handoff: FrameHandoff {
                    texture: frame.texture.cast_mut().cast(),
                    lease: frame.lease,
                    width: frame.width,
                    height: frame.height,
                },
            },
            Err(error) => FrameHandoffResult {
                status: status_from_error(error),
                handoff: empty_frame(),
            },
        },
        Err(status) => FrameHandoffResult {
            status,
            handoff: empty_frame(),
        },
    }
}

pub(crate) fn scene_boundary(device: &mut Device, mode: u32) -> Status {
    match renderer_mut(device, "scene_boundary") {
        Ok(renderer) => status_from_error_or_ok(renderer.scene_boundary(mode)),
        Err(status) => status,
    }
}
