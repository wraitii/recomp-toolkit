use crate::RenderError;
use smallvec::SmallVec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LayoutAttribute {
    pub(crate) format: wgpu::VertexFormat,
    pub(crate) offset: u32,
    pub(crate) location: u32,
}

impl LayoutAttribute {
    pub(crate) fn new(attribute: &wgpu::VertexAttribute) -> Result<Self, RenderError> {
        Ok(Self {
            format: attribute.format,
            offset: u32::try_from(attribute.offset).map_err(|_| {
                RenderError::invalid("Draw", "vertex attribute offset exceeds 32 bits")
            })?,
            location: attribute.shader_location,
        })
    }

    pub(crate) fn wgpu(self) -> wgpu::VertexAttribute {
        wgpu::VertexAttribute {
            format: self.format,
            offset: u64::from(self.offset),
            shader_location: self.location,
        }
    }
}

pub(crate) type VertexAttributes = SmallVec<[LayoutAttribute; 8]>;
pub(crate) type VertexLayouts = SmallVec<[(u64, VertexAttributes); 2]>;
