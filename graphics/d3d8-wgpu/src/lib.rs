//! Reusable D3D7/8/9 rendering on [`wgpu`], with recomp-kit adapters.
//!
//! D3D semantics and GPU resources live in [`d3d8`], [`d3d9`] and [`backend`].
//! [`guest`] supplies resource adapters; [`bridge`] supplies the native boundary.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod backend;
pub mod bridge;
pub(crate) mod bridge_render;
pub(crate) mod bridge_translate;
pub mod d3d8;
pub mod d3d9;
pub mod guest;
pub(crate) mod host_gpu;
pub(crate) mod kit;
pub(crate) mod fast_map;
#[cfg(feature = "probe")]
pub mod probe;
pub(crate) mod render;
pub(crate) mod shader;

/// Named failure at the renderer boundary; unsupported behavior is never success.
#[derive(Clone, Debug)]
pub struct RenderError {
    pub kind: RenderErrorKind,
    pub operation: &'static str,
    pub cause: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderErrorKind {
    Unsupported,
    InvalidArgument,
    InvalidData,
    OutOfMemory,
}

impl RenderError {
    pub fn new(operation: &'static str, cause: impl Into<String>) -> Self {
        Self {
            kind: RenderErrorKind::Unsupported,
            operation,
            cause: cause.into(),
        }
    }
    pub fn invalid(operation: &'static str, cause: impl Into<String>) -> Self {
        Self {
            kind: RenderErrorKind::InvalidArgument,
            operation,
            cause: cause.into(),
        }
    }
    pub fn out_of_memory(operation: &'static str, cause: impl Into<String>) -> Self {
        Self {
            kind: RenderErrorKind::OutOfMemory,
            operation,
            cause: cause.into(),
        }
    }
    pub fn invalid_data(operation: &'static str, cause: impl Into<String>) -> Self {
        Self {
            kind: RenderErrorKind::InvalidData,
            operation,
            cause: cause.into(),
        }
    }
}

impl core::fmt::Display for RenderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.operation, self.cause)
    }
}

impl std::error::Error for RenderError {}
