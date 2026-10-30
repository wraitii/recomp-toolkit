use super::*;
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderState {
    color_format: [i32; 2],
    color_count: i32,
    depth_attachment: u8,
    blend_enabled: u8,
    src_rgb: i32,
    dst_rgb: i32,
    src_alpha: i32,
    dst_alpha: i32,
    write_color: u8,
    writes_point_size: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DepthState {
    compare: i32,
    write: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Viewport {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    near_z: f64,
    far_z: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub struct SamplerState {
    u: i32,
    v: i32,
    mag: i32,
    min: i32,
    mip: i32,
    anisotropy: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ColorAttachment {
    texture: u64,
    load: i32,
    store: i32,
    clear: [f32; 4],
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DepthAttachment {
    texture: u64,
    load: i32,
    store: i32,
    clear: f32,
}
#[repr(C)]
pub struct RenderPass {
    color: [ColorAttachment; 2],
    color_count: i32,
    depth: DepthAttachment,
}
struct Binding {
    buffer: wgpu::Buffer,
    offset: u64,
    size: u64,
}
pub(super) struct Recording {
    pass: Option<wgpu::RenderPass<'static>>,
    compute: Option<wgpu::ComputePass<'static>>,
    pipeline: u64,
    depth: DepthState,
    cull: i32,
    pass_depth: bool,
    buffers: [Option<Binding>; 8],
    textures: [Option<wgpu::TextureView>; 4],
    samplers: [Option<wgpu::Sampler>; 4],
}
impl Default for Recording {
    fn default() -> Self {
        Self {
            pass: None,
            compute: None,
            pipeline: 0,
            depth: DepthState {
                compare: 7,
                write: 0,
            },
            cull: 0,
            pass_depth: false,
            buffers: std::array::from_fn(|_| None),
            textures: std::array::from_fn(|_| None),
            samplers: std::array::from_fn(|_| None),
        }
    }
}
impl Recording {
    pub(super) fn finish(&mut self) {
        self.pass = None;
        self.compute = None;
    }
}
struct Family {
    name: String,
    state: RenderState,
    shader: wgpu::ShaderModule,
    variants: HashMap<u64, wgpu::RenderPipeline>,
}
pub(super) struct Cache {
    compositors: HashMap<wgpu::TextureFormat, recomp_compositor::gpu::GpuCompositor>,
    families: HashMap<u64, Family>,
    computes: HashMap<u64, (String, wgpu::ComputePipeline)>,
    samplers: HashMap<SamplerState, wgpu::Sampler>,
    dummy_buffer: wgpu::Buffer,
    dummy_texture: wgpu::TextureView,
    dummy_sampler: wgpu::Sampler,
}
impl Cache {
    pub(super) fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("host-dummy"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        Self {
            compositors: HashMap::new(),
            families: HashMap::new(),
            computes: HashMap::new(),
            samplers: HashMap::new(),
            dummy_buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("host-dummy"),
                size: 4096,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            }),
            dummy_texture: texture.create_view(&Default::default()),
            dummy_sampler: device.create_sampler(&Default::default()),
        }
    }
}
fn blend(value: i32) -> wgpu::BlendFactor {
    use wgpu::BlendFactor::*;
    match value {
        0 => Zero,
        1 => One,
        2 => SrcAlpha,
        3 => OneMinusSrcAlpha,
        4 => DstAlpha,
        5 => OneMinusDstAlpha,
        6 => Src,
        7 => OneMinusSrc,
        8 => Dst,
        9 => OneMinusDst,
        10 => SrcAlphaSaturated,
        _ => panic!("host GPU: blend {value}"),
    }
}
fn compare(value: i32) -> wgpu::CompareFunction {
    use wgpu::CompareFunction::*;
    match value {
        0 => Never,
        1 => Less,
        2 => Equal,
        3 => LessEqual,
        4 => Greater,
        5 => NotEqual,
        6 => GreaterEqual,
        7 => Always,
        _ => panic!("host GPU: compare {value}"),
    }
}
fn group(
    device: &wgpu::Device,
    cache: &Cache,
    c: &Recording,
    layout: &wgpu::BindGroupLayout,
    name: &str,
) -> wgpu::BindGroup {
    let indices: &[u32] = match name {
        "compositor" => &[0, 4, 16, 24],
        "hud" => &[0, 16, 24],
        "surface_upload" => &[4, 5, 6],
        "d3d" => &[0, 1, 5, 16, 24],
        "guest_readback" | "guest_readback_fused" => &[0, 1, 16, 17],
        "native_brightness" => &[0, 1, 16],
        _ => panic!("host GPU: shader {name}"),
    };
    let entries: Vec<_> = indices
        .iter()
        .map(|&i| {
            let resource = if i < 8 {
                let (buffer, offset, size) = c.buffers[i as usize]
                    .as_ref()
                    .map(|b| (&b.buffer, b.offset, b.size))
                    .unwrap_or((&cache.dummy_buffer, 0, 4096));
                wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer,
                    offset,
                    size: std::num::NonZeroU64::new(size),
                })
            } else if i < 24 {
                wgpu::BindingResource::TextureView(
                    c.textures[(i - 16) as usize]
                        .as_ref()
                        .unwrap_or(&cache.dummy_texture),
                )
            } else {
                wgpu::BindingResource::Sampler(
                    c.samplers[(i - 24) as usize]
                        .as_ref()
                        .unwrap_or(&cache.dummy_sampler),
                )
            };
            wgpu::BindGroupEntry {
                binding: i,
                resource,
            }
        })
        .collect();
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(name),
        layout,
        entries: &entries,
    })
}
fn variant(
    device: &wgpu::Device,
    f: &mut Family,
    c: &Recording,
    topology: i32,
) -> wgpu::RenderPipeline {
    let key = topology as u64
        | (c.cull as u64) << 4
        | (c.depth.compare as u64) << 8
        | (c.depth.write as u64) << 12
        | (c.pass_depth as u64) << 13;
    f.variants
        .entry(key)
        .or_insert_with(|| {
            let blend = (f.state.blend_enabled != 0).then(|| wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: blend(f.state.src_rgb),
                    dst_factor: blend(f.state.dst_rgb),
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: blend(f.state.src_alpha),
                    dst_factor: blend(f.state.dst_alpha),
                    operation: wgpu::BlendOperation::Add,
                },
            });
            let targets: Vec<_> = f.state.color_format[..f.state.color_count as usize]
                .iter()
                .map(|v| {
                    Some(wgpu::ColorTargetState {
                        format: format(*v).expect("host GPU target format"),
                        blend,
                        write_mask: if f.state.write_color != 0 {
                            wgpu::ColorWrites::ALL
                        } else {
                            wgpu::ColorWrites::empty()
                        },
                    })
                })
                .collect();
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(&f.name),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &f.shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &f.shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &targets,
                }),
                primitive: wgpu::PrimitiveState {
                    topology: match topology {
                        0 => wgpu::PrimitiveTopology::PointList,
                        1 => wgpu::PrimitiveTopology::LineList,
                        2 => wgpu::PrimitiveTopology::TriangleList,
                        3 => wgpu::PrimitiveTopology::TriangleStrip,
                        _ => panic!("host GPU topology {topology}"),
                    },
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: match c.cull {
                        0 => None,
                        1 => Some(wgpu::Face::Front),
                        2 => Some(wgpu::Face::Back),
                        _ => panic!("host GPU cull"),
                    },
                    ..Default::default()
                },
                depth_stencil: c.pass_depth.then(|| wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(c.depth.write != 0),
                    depth_compare: Some(compare(c.depth.compare)),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        })
        .clone()
}
fn with(gpu: *mut HostGpu, cb: u64, f: impl FnOnce(&HostGpu, &mut State, &mut commands::Command)) {
    guard("render command", || {
        let g = unsafe { gpu.as_ref() }.expect("host GPU null device");
        let mut s = g.state.lock().unwrap();
        let mut c = s.commands.remove(&cb).expect("host GPU unknown command");
        assert!(c.encoder.is_some(), "host GPU committed command");
        f(g, &mut s, &mut c);
        s.commands.insert(cb, c);
    });
}
fn buffer_index(stage: i32, slot: i32) -> usize {
    assert!((0..=2).contains(&stage), "host GPU stage");
    assert!(
        slot >= 0 && slot < if stage == 2 { 8 } else { 4 },
        "host GPU buffer slot"
    );
    (slot + if stage == 1 { 4 } else { 0 }) as usize
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_render_pipeline(
    gpu: *mut HostGpu,
    name: *const std::ffi::c_char,
    state: *const RenderState,
) -> u64 {
    guard("render_pipeline", || {
        let g = unsafe { gpu.as_ref() }.expect("host GPU null device");
        let name = unsafe { std::ffi::CStr::from_ptr(name) }
            .to_str()
            .expect("host GPU shader name");
        let state = *unsafe { state.as_ref() }.expect("host GPU render state");
        assert!((1..=2).contains(&state.color_count), "host GPU color count");
        assert!(
            state.writes_point_size == 0,
            "host GPU: programmable point size unsupported"
        );
        let mut s = g.state.lock().unwrap();
        if let Some((&id, _)) = s
            .render
            .families
            .iter()
            .find(|(_, f)| f.name == name && f.state == state)
        {
            return id;
        }
        let source = shaders::source(name).expect("host GPU unknown shader");
        let shader = g
            .context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(name),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
        let id = s.id();
        s.render.families.insert(
            id,
            Family {
                name: name.into(),
                state,
                shader,
                variants: HashMap::new(),
            },
        );
        id
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_compute_pipeline(
    gpu: *mut HostGpu,
    name: *const std::ffi::c_char,
) -> u64 {
    guard("compute_pipeline", || {
        let g = unsafe { gpu.as_ref() }.expect("host GPU null device");
        let name = unsafe { std::ffi::CStr::from_ptr(name) }
            .to_str()
            .expect("host GPU shader name");
        let mut s = g.state.lock().unwrap();
        if let Some((&id, _)) = s.render.computes.iter().find(|(_, f)| f.0 == name) {
            return id;
        }
        let source = shaders::source(name).expect("host GPU unknown compute shader");
        let module = g
            .context
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(name),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
        let pipeline = g
            .context
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(name),
                layout: None,
                module: &module,
                entry_point: Some("cs_main"),
                compilation_options: Default::default(),
                cache: None,
            });
        let id = s.id();
        s.render.computes.insert(id, (name.into(), pipeline));
        id
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_thread_execution_width(_: *mut HostGpu, _: u64) -> i32 {
    256
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_begin_render_pass(
    gpu: *mut HostGpu,
    cb: u64,
    pass: *const RenderPass,
) {
    with(gpu, cb, |_, s, c| {
        c.render.finish();
        let p = unsafe { pass.as_ref() }.expect("host GPU null pass");
        assert!((1..=2).contains(&p.color_count), "host GPU pass colors");
        let views: Vec<_> = p.color[..p.color_count as usize]
            .iter()
            .map(|a| {
                s.textures
                    .get(&a.texture)
                    .expect("host GPU color attachment")
                    .texture
                    .create_view(&Default::default())
            })
            .collect();
        let colors: Vec<_> = views
            .iter()
            .zip(&p.color)
            .map(|(view, a)| {
                Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if a.load == 0 {
                            wgpu::LoadOp::Load
                        } else {
                            wgpu::LoadOp::Clear(wgpu::Color {
                                r: a.clear[0] as f64,
                                g: a.clear[1] as f64,
                                b: a.clear[2] as f64,
                                a: a.clear[3] as f64,
                            })
                        },
                        store: if a.store == 0 {
                            wgpu::StoreOp::Store
                        } else {
                            wgpu::StoreOp::Discard
                        },
                    },
                })
            })
            .collect();
        let depth = s
            .textures
            .get(&p.depth.texture)
            .map(|t| t.texture.create_view(&Default::default()));
        c.render.pass_depth = depth.is_some();
        let depth = depth
            .as_ref()
            .map(|view| wgpu::RenderPassDepthStencilAttachment {
                view,
                depth_ops: Some(wgpu::Operations {
                    load: if p.depth.load == 0 {
                        wgpu::LoadOp::Load
                    } else {
                        wgpu::LoadOp::Clear(p.depth.clear)
                    },
                    store: if p.depth.store == 0 {
                        wgpu::StoreOp::Store
                    } else {
                        wgpu::StoreOp::Discard
                    },
                }),
                stencil_ops: None,
            });
        c.render.pass = Some(
            c.encoder
                .as_mut()
                .unwrap()
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("host-pass"),
                    color_attachments: &colors,
                    depth_stencil_attachment: depth,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime(),
        );
    });
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_pipeline(g: *mut HostGpu, c: u64, p: u64) {
    with(g, c, |_, _, c| c.render.pipeline = p)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_depth(g: *mut HostGpu, c: u64, d: DepthState) {
    with(g, c, |_, _, c| c.render.depth = d)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_cull(g: *mut HostGpu, c: u64, v: i32) {
    with(g, c, |_, _, c| c.render.cull = v)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_viewport(g: *mut HostGpu, c: u64, v: Viewport) {
    with(g, c, |_, _, c| {
        c.render
            .pass
            .as_mut()
            .expect("host GPU viewport outside pass")
            .set_viewport(
                v.x as f32,
                v.y as f32,
                v.w as f32,
                v.h as f32,
                v.near_z as f32,
                v.far_z as f32,
            );
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_bytes(
    g: *mut HostGpu,
    c: u64,
    stage: i32,
    slot: i32,
    data: *const c_void,
    size: u64,
) {
    with(g, c, |g, _, c| {
        assert!(
            !data.is_null() && size > 0 && size <= 4096,
            "host GPU inline bytes"
        );
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) };
        let mut padded = vec![0; ((size + 3) & !3) as usize];
        padded[..bytes.len()].copy_from_slice(bytes);
        let buffer = g
            .context
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("host-inline"),
                contents: &padded,
                usage: wgpu::BufferUsages::STORAGE,
            });
        c.render.buffers[buffer_index(stage, slot)] = Some(Binding {
            buffer,
            offset: 0,
            size: padded.len() as u64,
        });
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_buffer(
    g: *mut HostGpu,
    c: u64,
    stage: i32,
    slot: i32,
    id: u64,
    offset: u64,
) {
    with(g, c, |_, s, c| {
        let b = s.buffers.get(&id).expect("host GPU buffer binding");
        assert!(offset < b.size, "host GPU buffer offset");
        c.render.buffers[buffer_index(stage, slot)] = Some(Binding {
            buffer: b.buffer.clone(),
            offset,
            size: (b.size - offset) & !3,
        });
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_vertex_buffer(
    g: *mut HostGpu,
    c: u64,
    slot: i32,
    id: u64,
    offset: u64,
) {
    unsafe { host_gpu_set_buffer(g, c, 0, slot, id, offset) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_texture(
    g: *mut HostGpu,
    c: u64,
    _stage: i32,
    slot: i32,
    id: u64,
) {
    with(g, c, |_, s, c| {
        assert!((0..4).contains(&slot), "host GPU texture slot");
        c.render.textures[slot as usize] = if id == 0 {
            None
        } else {
            Some(
                s.textures
                    .get(&id)
                    .expect("host GPU texture binding")
                    .texture
                    .create_view(&Default::default()),
            )
        };
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_set_sampler(
    g: *mut HostGpu,
    c: u64,
    _stage: i32,
    slot: i32,
    v: SamplerState,
) {
    with(g, c, |g, s, c| {
        assert!((0..4).contains(&slot), "host GPU sampler slot");
        let address = |v| match v {
            0 => wgpu::AddressMode::Repeat,
            1 => wgpu::AddressMode::ClampToEdge,
            2 => wgpu::AddressMode::MirrorRepeat,
            3 => wgpu::AddressMode::ClampToBorder,
            _ => panic!("host GPU address mode"),
        };
        let filter = |v| match v {
            0 => wgpu::FilterMode::Nearest,
            1 => wgpu::FilterMode::Linear,
            _ => panic!("host GPU filter"),
        };
        let sampler = s.render.samplers.entry(v).or_insert_with(|| {
            g.context.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("host-sampler"),
                address_mode_u: address(v.u),
                address_mode_v: address(v.v),
                mag_filter: filter(v.mag),
                min_filter: filter(v.min),
                mipmap_filter: if v.mip == 2 {
                    wgpu::MipmapFilterMode::Linear
                } else {
                    wgpu::MipmapFilterMode::Nearest
                },
                lod_max_clamp: if v.mip == 0 { 0.25 } else { 32.0 },
                anisotropy_clamp: if v.mag == 1 && v.min == 1 && v.mip == 2 {
                    v.anisotropy.clamp(1, 16) as u16
                } else {
                    1
                },
                border_color: ((v.u == 3) || (v.v == 3))
                    .then_some(wgpu::SamplerBorderColor::TransparentBlack),
                ..Default::default()
            })
        });
        c.render.samplers[slot as usize] = Some(sampler.clone());
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_draw(
    g: *mut HostGpu,
    c: u64,
    topology: i32,
    first: i32,
    count: i32,
) {
    with(g, c, |g, s, c| {
        if count <= 0 {
            return;
        }
        assert!(first >= 0, "host GPU draw first");
        let f = s
            .render
            .families
            .get_mut(&c.render.pipeline)
            .expect("host GPU draw pipeline");
        let pipeline = variant(&g.context.device, f, &c.render, topology);
        let name = f.name.clone();
        let bind = group(
            &g.context.device,
            &s.render,
            &c.render,
            &pipeline.get_bind_group_layout(0),
            &name,
        );
        let pass = c.render.pass.as_mut().expect("host GPU draw outside pass");
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(first as u32..(first + count) as u32, 0..1);
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_end_render_pass(g: *mut HostGpu, c: u64) {
    with(g, c, |_, _, c| c.render.pass = None)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_begin_compute_pass(g: *mut HostGpu, c: u64) {
    with(g, c, |_, _, c| {
        c.render.finish();
        c.render.compute = Some(
            c.encoder
                .as_mut()
                .unwrap()
                .begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("host-compute"),
                    timestamp_writes: None,
                })
                .forget_lifetime(),
        );
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_dispatch(
    g: *mut HostGpu,
    c: u64,
    x: i32,
    y: i32,
    gx: i32,
    gy: i32,
    threads: u8,
) {
    with(g, c, |g, s, c| {
        assert!(
            x > 0 && y > 0 && gx > 0 && gy > 0,
            "host GPU dispatch dimensions"
        );
        let (name, pipeline) = s
            .render
            .computes
            .get(&c.render.pipeline)
            .expect("host GPU compute pipeline");
        let expected = if name == "native_brightness" {
            [16, 16]
        } else {
            [8, 8]
        };
        assert_eq!([gx, gy], expected, "host GPU compute group size");
        let bind = group(
            &g.context.device,
            &s.render,
            &c.render,
            &pipeline.get_bind_group_layout(0),
            name,
        );
        let pass = c
            .render
            .compute
            .as_mut()
            .expect("host GPU dispatch outside pass");
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(
            if threads != 0 {
                (x as u32).div_ceil(gx as u32)
            } else {
                x as u32
            },
            if threads != 0 {
                (y as u32).div_ceil(gy as u32)
            } else {
                y as u32
            },
            1,
        );
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_end_compute_pass(g: *mut HostGpu, c: u64) {
    with(g, c, |_, _, c| c.render.compute = None)
}

#[repr(C)]
pub struct CompositionDraw {
    texture: u64,
    rect: recomp_compositor::Rect,
    uv: recomp_compositor::Rect,
    blend: u32,
    filter: u32,
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn host_gpu_compose_plan(
    g: *mut HostGpu,
    cb: u64,
    target: u64,
    width: i32,
    height: i32,
    draws: *const CompositionDraw,
    count: usize,
) -> u8 {
    with(g, cb, |g, s, c| {
        c.render.finish();
        if g.context.loss.is_lost() {
            report("composition device lost");
            return;
        }
        assert!(
            width > 0 && height > 0 && count <= u32::MAX as usize,
            "host GPU composition bounds"
        );
        let draws = if count == 0 {
            &[]
        } else {
            assert!(!draws.is_null(), "host GPU null composition");
            unsafe { std::slice::from_raw_parts(draws, count) }
        };
        let output = s
            .textures
            .get(&target)
            .expect("host GPU composition target");
        assert_eq!(
            [output.desc.width, output.desc.height],
            [width, height],
            "host GPU composition dimensions"
        );
        let format = output.texture.format();
        let output = output.texture.create_view(&Default::default());
        let mut views = Vec::with_capacity(count);
        let mut plan = Vec::with_capacity(count);
        for (i, d) in draws.iter().enumerate() {
            let texture = s
                .textures
                .get(&d.texture)
                .expect("host GPU composition texture");
            views.push((
                texture.texture.create_view(&Default::default()),
                match d.filter {
                    0 => wgpu::FilterMode::Nearest,
                    1 => wgpu::FilterMode::Linear,
                    _ => panic!("host GPU composition filter"),
                },
            ));
            plan.push(recomp_compositor::Draw {
                source: recomp_compositor::Source::Ui,
                element_index: i as u32,
                blend: match d.blend {
                    0 => recomp_compositor::Blend::Opaque,
                    1 => recomp_compositor::Blend::Straight,
                    2 => recomp_compositor::Blend::Premultiplied,
                    _ => panic!("host GPU composition blend"),
                },
                dst: d.rect,
                uv: d.uv,
            });
        }
        if !s.render.compositors.contains_key(&format) {
            match recomp_compositor::gpu::GpuCompositor::new(Arc::clone(&g.context), format) {
                Ok(compositor) => {
                    s.render.compositors.insert(format, compositor);
                }
                Err(error) => {
                    eprintln!("host GPU composition: {error}");
                    return;
                }
            }
        }
        if let Err(error) = s.render.compositors[&format].encode(
            c.encoder.as_mut().unwrap(),
            &output,
            [width as u32, height as u32],
            &plan,
            |d| Some(views[d.element_index as usize].clone()),
        ) {
            if g.context.loss.is_lost() {
                eprintln!("host GPU composition: {error}");
            } else {
                panic!("host GPU composition: {error}");
            }
        }
    });
    1
}
