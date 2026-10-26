//! Optional host-side post-process of a finished render target.
//!
//! FXAA runs in place on an [`OffscreenTarget`]: the target is filtered into a
//! cached scratch texture of the same size and format, then copied back, so
//! the guest keeps drawing into the same texture (and depth buffer) afterwards.
//! It is a display enhancement with no D3D8 counterpart; the caller decides
//! where in the frame it runs and whether it runs at all.
use super::*;

/// FXAA 2.0 style luma-edge filter (Lottes' original, in the common
/// four-corner form) behind 3.11's edge-contrast early exit. Operates on gamma-space values, as D3D8 content is
/// authored; alpha passes through unchanged.
const FXAA_WGSL: &str = r#"
struct Params {
    inv_size: vec2<f32>,
    pad: vec2<f32>,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_samp: sampler;
@group(0) @binding(2) var<uniform> params: Params;

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    return vec4<f32>(pos[idx], 0.0, 1.0);
}

const EDGE_THRESHOLD: f32 = 0.166;
const EDGE_THRESHOLD_MIN: f32 = 0.0833;

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.299, 0.587, 0.114));
}

fn tap(uv: vec2<f32>) -> vec3<f32> {
    return textureSampleLevel(src_tex, src_samp, uv, 0.0).rgb;
}

@fragment
fn fs_main(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let o = params.inv_size;
    let uv = frag.xy * o;
    let centre = textureSampleLevel(src_tex, src_samp, uv, 0.0);
    let nw = tap(uv + vec2<f32>(-1.0, -1.0) * o);
    let ne = tap(uv + vec2<f32>( 1.0, -1.0) * o);
    let sw = tap(uv + vec2<f32>(-1.0,  1.0) * o);
    let se = tap(uv + vec2<f32>( 1.0,  1.0) * o);
    let l_nw = luma(nw);
    let l_ne = luma(ne);
    let l_sw = luma(sw);
    let l_se = luma(se);
    let l_m = luma(centre.rgb);
    let l_min = min(l_m, min(min(l_nw, l_ne), min(l_sw, l_se)));
    let l_max = max(l_m, max(max(l_nw, l_ne), max(l_sw, l_se)));

    // Leave flat areas and texture detail alone: only filter where the local
    // contrast is a real edge (FXAA 3.11's relative and absolute thresholds,
    // at its default quality).
    if (l_max - l_min < max(EDGE_THRESHOLD_MIN, l_max * EDGE_THRESHOLD)) {
        return centre;
    }

    var dir = vec2<f32>(-((l_nw + l_ne) - (l_sw + l_se)), (l_nw + l_sw) - (l_ne + l_se));
    let reduce = max((l_nw + l_ne + l_sw + l_se) * (0.25 * (1.0 / 8.0)), 1.0 / 128.0);
    let rcp = 1.0 / (min(abs(dir.x), abs(dir.y)) + reduce);
    dir = clamp(dir * rcp, vec2<f32>(-8.0), vec2<f32>(8.0)) * o;

    let a = 0.5 * (tap(uv + dir * (1.0 / 3.0 - 0.5)) + tap(uv + dir * (2.0 / 3.0 - 0.5)));
    let b = a * 0.5 + 0.25 * (tap(uv + dir * -0.5) + tap(uv + dir * 0.5));
    let l_b = luma(b);
    if (l_b < l_min || l_b > l_max) {
        return vec4<f32>(a, centre.a);
    }
    return vec4<f32>(b, centre.a);
}
"#;

/// Pipeline and scratch texture, rebuilt only when the target format or size
/// changes.
pub(super) struct FxaaState {
    format: wgpu::TextureFormat,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    scratch: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
}

impl GpuContext {
    /// Run FXAA over `target` in place. The caller must have submitted every
    /// draw that should be filtered; work recorded afterwards is not affected.
    pub fn fxaa_in_place(&self, target: &OffscreenTarget) -> Result<(), RenderError> {
        let mut guard = self.fxaa.lock().unwrap();
        self.scope_validation("fxaa", || {
            if guard.as_ref().is_none_or(|s| s.format != target.format) {
                *guard = Some(self.build_fxaa(target.format));
            }
            let state = guard.as_mut().unwrap();
            let (w, h) = (target.width, target.height);
            if state.scratch.as_ref().is_none_or(|s| (s.0, s.1) != (w, h)) {
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("d3d8-fxaa-scratch"),
                    size: wgpu::Extent3d {
                        width: target.texture.width(),
                        height: target.texture.height(),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: target.format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                state.scratch = Some((w, h, texture, view));
            }
            // The filter covers the whole backing texture.
            let tex_w = target.texture.width() as f32;
            let tex_h = target.texture.height() as f32;
            let params = [1.0 / tex_w, 1.0 / tex_h, 0.0, 0.0f32];
            self.queue
                .write_buffer(&state.params, 0, bytemuck::cast_slice(&params));
            let (_, _, scratch, scratch_view) = state.scratch.as_ref().unwrap();
            let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("d3d8-fxaa"),
                layout: &state.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&target.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&state.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: state.params.as_entire_binding(),
                    },
                ],
            });
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("d3d8-fxaa"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("d3d8-fxaa"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: scratch_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&state.pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..3, 0..1);
            }
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: scratch,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &target.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: target.texture.width(),
                    height: target.texture.height(),
                    depth_or_array_layers: 1,
                },
            );
            self.queue.submit(std::iter::once(encoder.finish()));
            Ok(())
        })
    }

    fn build_fxaa(&self, format: wgpu::TextureFormat) -> FxaaState {
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("d3d8-fxaa"),
                source: wgpu::ShaderSource::Wgsl(FXAA_WGSL.into()),
            });
        let bind_group_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("d3d8-fxaa"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("d3d8-fxaa"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("d3d8-fxaa"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("d3d8-fxaa"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("d3d8-fxaa-params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        FxaaState {
            format,
            bind_group_layout,
            pipeline,
            sampler,
            params,
            scratch: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A staircase edge gains intermediate values, flat areas stay exact and
    /// alpha is untouched. Needs a GPU adapter (Metal on macOS), like the probes.
    #[test]
    fn fxaa_smooths_a_staircase_and_keeps_flat_areas() {
        let gpu = pollster::block_on(GpuContext::new_headless()).expect("adapter");
        let n = 16u32;
        let target = gpu
            .create_target(n, n, crate::d3d8::format::D3DFMT_A8R8G8B8, 0)
            .expect("target");
        let mut before = vec![0u8; (n * n * 4) as usize];
        for y in 0..n {
            for x in 0..n {
                let v = if x + y >= n { 255 } else { 0 };
                let i = ((y * n + x) * 4) as usize;
                before[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &before,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(n * 4),
                rows_per_image: Some(n),
            },
            wgpu::Extent3d {
                width: n,
                height: n,
                depth_or_array_layers: 1,
            },
        );
        gpu.fxaa_in_place(&target).expect("fxaa");
        let after = gpu.read_pixels(&target).expect("readback");
        let px = |buf: &[u8], x: u32, y: u32| buf[((y * n + x) * 4) as usize];
        assert_eq!(px(&after, 0, 0), 0, "flat black corner changed");
        assert_eq!(px(&after, n - 1, n - 1), 255, "flat white corner changed");
        let softened = (0..n)
            .map(|d| (d, n - 1 - d))
            .filter(|&(x, y)| {
                let (b, a) = (px(&before, x, y), px(&after, x, y));
                a != b && a > 0x10 && a < 0xF0
            })
            .count();
        assert!(softened >= 4, "only {softened} edge pixels softened");
        assert!(after.chunks(4).all(|p| p[3] == 255), "alpha changed");
        // A second run reuses the pipeline and scratch target.
        gpu.fxaa_in_place(&target).expect("second fxaa");
    }
}
