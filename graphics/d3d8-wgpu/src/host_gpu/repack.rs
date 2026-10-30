use wgpu::util::DeviceExt;
const SOURCE: &str = r#"
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
struct Params { src_offset:u32, src_pitch:u32, dst_offset:u32, dst_pitch:u32, row:u32, height:u32, first:u32, count:u32 };
@group(0) @binding(2) var<uniform> p: Params;
@compute @workgroup_size(64) fn main(@builtin(global_invocation_id) gid: vec3u) {
  if gid.x >= p.count { return; }
  let word = p.first + gid.x;
  var result = dst[word];
  for (var lane=0u; lane<4u; lane++) {
    let address=word*4u+lane;
    if address < p.dst_offset { continue; }
    let relative=address-p.dst_offset;
    let y=relative/p.dst_pitch;
    let x=relative%p.dst_pitch;
    if y>=p.height || x>=p.row { continue; }
    let source=p.src_offset+y*p.src_pitch+x;
    let value=(src[source/4u] >> ((source%4u)*8u)) & 255u;
    let shift=lane*8u;
    result=(result & ~(255u<<shift)) | (value<<shift);
  }
  dst[word]=result;
}
"#;
pub(super) fn encode(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    src: &wgpu::Buffer,
    dst: &wgpu::Buffer,
    src_offset: u64,
    src_pitch: u32,
    dst_offset: u64,
    dst_pitch: u32,
    row: u32,
    height: u32,
) -> Result<(), &'static str> {
    let end = dst_offset
        .checked_add((height as u64 - 1) * dst_pitch as u64 + row as u64)
        .ok_or("repack size overflow")?;
    let first = dst_offset / 4;
    let count = end.div_ceil(4) - first;
    let params = [
        u32::try_from(src_offset).map_err(|_| "repack source offset")?,
        src_pitch,
        u32::try_from(dst_offset).map_err(|_| "repack destination offset")?,
        dst_pitch,
        row,
        height,
        u32::try_from(first).map_err(|_| "repack first word")?,
        u32::try_from(count).map_err(|_| "repack word count")?,
    ];
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("host-repack"),
        source: wgpu::ShaderSource::Wgsl(SOURCE.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("host-repack"),
        layout: None,
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("host-repack-params"),
        contents: bytemuck::cast_slice(&params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("host-repack"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: src.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: dst.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: uniform.as_entire_binding(),
            },
        ],
    });
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("host-repack"),
        timestamp_writes: None,
    });
    pass.set_pipeline(&pipeline);
    pass.set_bind_group(0, &group, &[]);
    pass.dispatch_workgroups((count as u32).div_ceil(64), 1, 1);
    Ok(())
}
