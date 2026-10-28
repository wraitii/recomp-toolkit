use super::identity::ProgramKey;
use super::program::Program;
use super::specialize::{Variant, translate};
use crate::RenderError;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProgramSettings {
    pub variant: Variant,
    pub opaque_slots: u16,
    pub flat_shading: bool,
    pub pixel_output: bool,
}

#[derive(Clone)]
pub(crate) enum ShaderRecipe {
    Program {
        key: ProgramKey,
        program: Arc<Program>,
        settings: ProgramSettings,
    },
    Source(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ShaderRecipeKey {
    Program(ProgramKey, ProgramSettings),
    Source(Arc<str>),
}

impl ShaderRecipe {
    pub(crate) fn retained_bytes(&self) -> usize {
        match self {
            Self::Source(source) => source.len(),
            Self::Program { key, program, .. } => {
                std::mem::size_of::<Program>()
                    + key.byte_len()
                    + program.code.capacity() * std::mem::size_of::<super::program::Inst>()
                    + (program.inputs.capacity() + program.outputs.capacity())
                        * std::mem::size_of::<super::program::DclIn>()
                    + (program.samplers.len()
                        + program.defs.len()
                        + program.idefs.len()
                        + program.bdefs.len())
                        * 128
            }
        }
    }

    pub(crate) fn key(&self) -> ShaderRecipeKey {
        match self {
            Self::Program { key, settings, .. } => {
                ShaderRecipeKey::Program(key.clone(), settings.clone())
            }
            Self::Source(source) => ShaderRecipeKey::Source(Arc::clone(source)),
        }
    }

    pub(crate) fn render(&self) -> Result<Arc<str>, RenderError> {
        match self {
            Self::Source(source) => Ok(Arc::clone(source)),
            Self::Program {
                program, settings, ..
            } => {
                let mut source = translate(program, &settings.variant)?;
                if program.pixel {
                    source = opaque_samples(source, settings.opaque_slots);
                    if settings.pixel_output {
                        source = source.replace("return col;", "return d9_output(col,vin);");
                        source += include_str!("raster.wgsl");
                    }
                }
                Ok(shading_source(source, settings.flat_shading).into())
            }
        }
    }
}

fn opaque_samples(mut source: String, opaque_slots: u16) -> String {
    for stage in (0..16).filter(|stage| opaque_slots & (1 << stage) != 0) {
        for operation in ["textureSample", "textureSampleBias"] {
            let needle = format!("{operation}(tx{},", stage);
            let mut start = 0;
            while let Some(at) = source[start..].find(&needle) {
                let at = start + at;
                let mut depth = 0;
                let mut end = at;
                for (offset, ch) in source[at..].char_indices() {
                    if ch == '(' {
                        depth += 1;
                    } else if ch == ')' {
                        depth -= 1;
                        if depth == 0 {
                            end = at + offset + 1;
                            break;
                        }
                    }
                }
                if end <= at {
                    break;
                }
                let replacement = format!("vec4<f32>(({}).rgb,1.0)", &source[at..end]);
                source.replace_range(at..end, &replacement);
                start = at + replacement.len();
            }
        }
    }
    source
}
pub(crate) fn shading_source(mut source: String, flat_shading: bool) -> String {
    if flat_shading {
        for (location, name) in [(0, "d0"), (1, "d1")] {
            source = source.replace(
                &format!("@location({location}) {name}:"),
                &format!("@location({location}) @interpolate(flat, first) {name}:"),
            );
        }
    }
    source
}
