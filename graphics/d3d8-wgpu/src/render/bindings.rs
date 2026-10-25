pub(crate) struct DrawParameters {
    vertex: [f32; 68],
    pixel: [u8; 48],
}

impl DrawParameters {
    pub(crate) fn new(
        half_pixel: [f32; 4],
        attribute_scales: [[f32; 4]; 16],
        pixel_words: [u32; 12],
    ) -> Self {
        let mut vertex = [0.0; 68];
        vertex[..4].copy_from_slice(&half_pixel);
        for (slot, scale) in vertex[4..].chunks_exact_mut(4).zip(attribute_scales) {
            slot.copy_from_slice(&scale);
        }
        let mut pixel = [0; 48];
        for (bytes, word) in pixel.chunks_exact_mut(4).zip(pixel_words) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        Self { vertex, pixel }
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
            &self.pixel,
        ]
    }
}
