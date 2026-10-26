pub(crate) const VERTEX_PARAMETER_BYTES: usize = 272;
pub(crate) const PIXEL_PARAMETER_BYTES: usize = 48;
pub(crate) const INTERPRETER_PARAMETER_BYTES: usize = PIXEL_PARAMETER_BYTES + 16;
pub(crate) const TEXTURE_BINDING: u32 = 4;
pub(crate) const SAMPLER_BINDING: u32 = 20;
pub(crate) const VARYINGS: &str = include_str!("varyings.wgsl");
pub(crate) const VERTEX_PARAMETERS: &str =
    "struct VSP { halfpix: vec4<f32>, ascale: array<vec4<f32>, 16> }\n";

pub(crate) fn pixel_parameters(interpreted: bool) -> String {
    let flags = if interpreted {
        ", texture_flags: vec4<u32>"
    } else {
        ""
    };
    format!(
        "struct PSP {{ alpha_ref: f32, alpha_func: i32, fog_mode: i32, pad0: i32, fog_color: vec4<f32>, fog_start: f32, fog_end: f32, fog_density: f32, pad1: f32{flags} }}\n"
    )
}
