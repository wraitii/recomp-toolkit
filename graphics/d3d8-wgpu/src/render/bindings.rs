use crate::shader::interface::*;

pub(crate) struct DrawParameters {
    vertex: [f32; VERTEX_PARAMETER_BYTES / 4],
    pixel: [u8; INTERPRETER_PARAMETER_BYTES],
    pixel_size: usize,
}

impl DrawParameters {
    #[inline]
    pub(crate) fn new(half_pixel: [f32; 4], pixel_words: [u32; 12]) -> Self {
        let mut vertex = [1.0; VERTEX_PARAMETER_BYTES / 4];
        vertex[..4].copy_from_slice(&half_pixel);
        let mut pixel = [0; INTERPRETER_PARAMETER_BYTES];
        for (bytes, word) in pixel.chunks_exact_mut(4).zip(pixel_words) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        Self {
            vertex,
            pixel,
            pixel_size: PIXEL_PARAMETER_BYTES,
        }
    }

    pub(crate) fn sampler_lod_bias(&mut self, slot: u32, value: u32) {
        let offset = PIXEL_RASTER_PARAMETER_BYTES + 4 * slot as usize;
        self.pixel[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn interpreter_flags(&mut self, opaque: u32, unbound: u32, tint: bool) {
        let flags = &mut self.pixel[PIXEL_PARAMETER_BYTES..];
        for (bytes, value) in flags
            .chunks_exact_mut(4)
            .zip([opaque, u32::from(tint), unbound])
        {
            bytes.copy_from_slice(&value.to_le_bytes());
        }
        self.pixel_size = INTERPRETER_PARAMETER_BYTES;
    }

    pub(crate) fn values<'a>(
        &'a self,
        vertex_bank: &'a [u8],
        pixel_bank: &'a [u8],
    ) -> [&'a [u8]; 4] {
        [
            vertex_bank,
            bytemuck::cast_slice(&self.vertex),
            pixel_bank,
            &self.pixel[..self.pixel_size],
        ]
    }
}

pub(crate) fn uniform_entry(
    binding: u32,
    size: u64,
    visibility: wgpu::ShaderStages,
    dynamic: bool,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: wgpu::BufferSize::new(size),
        },
        count: None,
    }
}
