use crate::RenderError;
use std::collections::BTreeMap;

pub const MAX_TEXTURE_STAGES: usize = 8;
pub const MAX_SAMPLERS: usize = 16;
pub const MAX_STREAMS: usize = 16;
pub const MAX_ACTIVE_LIGHTS: usize = 8;
pub const MAX_CLIP_PLANES: usize = 6;
pub const MAX_STREAM_STRIDE: u32 = 0xffff;

pub const VS_CONST_F: usize = 256;
pub const VS_CONST_I: usize = 16;
pub const VS_CONST_B: usize = 16;
pub const PS_CONST_F: usize = 224;
pub const PS_CONST_I: usize = 16;
pub const PS_CONST_B: usize = 16;

pub const D3DSBT_ALL: u32 = 1;
pub const D3DSBT_PIXELSTATE: u32 = 2;
pub const D3DSBT_VERTEXSTATE: u32 = 3;

pub const D3DSTREAMSOURCE_INDEXEDDATA: u32 = 0x4000_0000;
pub const D3DSTREAMSOURCE_INSTANCEDATA: u32 = 0x8000_0000;
pub const D3DSTREAMSOURCE_MASK: u32 = D3DSTREAMSOURCE_INDEXEDDATA | D3DSTREAMSOURCE_INSTANCEDATA;

pub const MAX_ANISOTROPY: u32 = 16;

const D3DTA_SELECTMASK: u32 = 0x0000_000f;
const D3DTA_CONSTANT: u32 = 0x0000_0006;
const D3DTA_CURRENT: u32 = 0x0000_0001;
const D3DTA_TEMP: u32 = 0x0000_0005;
const D3DTA_COMPLEMENT: u32 = 0x0000_0010;
const D3DTA_ALPHAREPLICATE: u32 = 0x0000_0020;
const D3DTA_ARG_MASK: u32 = D3DTA_SELECTMASK | D3DTA_COMPLEMENT | D3DTA_ALPHAREPLICATE;
const D3DTSS_TCI_CAMERASPACENORMAL: u32 = 0x0001_0000;
const D3DTSS_TCI_CAMERASPACEPOSITION: u32 = 0x0002_0000;
const D3DTSS_TCI_CAMERASPACEREFLECTIONVECTOR: u32 = 0x0003_0000;
const D3DTSS_TCI_SPHEREMAP: u32 = 0x0004_0000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub min_z: f32,
    pub max_z: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scissor {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSource {
    pub buffer: u32,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct D3d9Bindings {
    pub textures: [u32; MAX_SAMPLERS],
    pub streams: [u32; MAX_STREAMS],
    pub indices: u32,
    pub vertex_shader: u32,
    pub pixel_shader: u32,
    pub declaration: u32,
}

enum RKind {
    Bool,
    Range(u32, u32),
    Set(&'static [u32]),
    D3dta,
    TexCoordIndex,
    TextureTransformFlags,
    Any,
}

fn invalid(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::invalid(op, why)
}

fn unsupported(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::new(op, why)
}

fn validate_kind(
    kind: &RKind,
    value: u32,
    op: &'static str,
    state: u32,
) -> Result<u32, RenderError> {
    match kind {
        RKind::Bool => Ok(value),
        RKind::Range(min, max) => {
            if value >= *min && value <= *max {
                Ok(value)
            } else {
                Err(invalid(
                    op,
                    format!("value {value} for state {state} is out of range {min}..={max}"),
                ))
            }
        }
        RKind::Set(set) => {
            if set.contains(&value) {
                Ok(value)
            } else {
                Err(invalid(
                    op,
                    format!("value {value} is not a valid value for state {state}"),
                ))
            }
        }
        RKind::D3dta => {
            if value & !D3DTA_ARG_MASK == 0 && value & D3DTA_SELECTMASK <= D3DTA_CONSTANT {
                Ok(value)
            } else {
                Err(invalid(
                    op,
                    format!("value {value} for state {state} is not a D3DTA texture argument"),
                ))
            }
        }
        RKind::TexCoordIndex => {
            let generation = value & 0xffff_0000;
            if value & 0xffff <= 7
                && matches!(
                    generation,
                    0 | D3DTSS_TCI_CAMERASPACENORMAL
                        | D3DTSS_TCI_CAMERASPACEPOSITION
                        | D3DTSS_TCI_CAMERASPACEREFLECTIONVECTOR
                        | D3DTSS_TCI_SPHEREMAP
                )
            {
                Ok(value)
            } else {
                Err(invalid(
                    op,
                    format!(
                        "value {value} for state {state} is not a texture coordinate index/generation mode"
                    ),
                ))
            }
        }
        RKind::TextureTransformFlags => {
            if matches!(value, 0..=4 | 0x100 | 0x102 | 0x103 | 0x104) {
                Ok(value)
            } else {
                Err(invalid(
                    op,
                    format!("value {value} for state {state} is not a D3DTTFF value"),
                ))
            }
        }
        RKind::Any => Ok(value),
    }
}

fn render_state_spec(state: u32) -> Option<(u32, RKind)> {
    let f = |value: f32| value.to_bits();
    Some(match state {
        7 => (0, RKind::Range(0, 2)),
        8 | 9 => (if state == 8 { 3 } else { 2 }, RKind::Range(1, 3)),
        14 => (1, RKind::Bool),
        15 => (0, RKind::Bool),
        16 => (1, RKind::Bool),
        19 => (2, RKind::Range(1, 17)),
        20 => (1, RKind::Range(1, 17)),
        22 => (3, RKind::Range(1, 3)),
        23 => (4, RKind::Range(1, 8)),
        24 => (0, RKind::Any),
        25 => (8, RKind::Range(1, 8)),
        26 => (0, RKind::Bool),
        27 => (0, RKind::Bool),
        28 => (0, RKind::Bool),
        29 => (0, RKind::Bool),
        34 => (0, RKind::Any),
        35 => (0, RKind::Range(0, 3)),
        36 => (0, RKind::Any),
        37 => (f(1.0), RKind::Any),
        38 => (f(1.0), RKind::Any),
        48 => (0, RKind::Bool),
        52 => (0, RKind::Bool),
        53..=55 => (1, RKind::Range(1, 8)),
        56 => (8, RKind::Range(1, 8)),
        57 => (0, RKind::Any),
        58 | 59 => (0xffff_ffff, RKind::Any),
        60 => (0xffff_ffff, RKind::Any),
        128..=135 => (0, RKind::Any),
        136 => (1, RKind::Bool),
        137 => (1, RKind::Bool),
        139 => (0, RKind::Any),
        140 => (0, RKind::Range(0, 3)),
        141 => (1, RKind::Bool),
        142 => (1, RKind::Bool),
        143 => (0, RKind::Bool),
        145 => (1, RKind::Range(0, 2)),
        146 => (2, RKind::Range(0, 2)),
        147 | 148 => (0, RKind::Range(0, 2)),
        151 => (0, RKind::Set(&[0, 1, 2, 3, 255, 256])),
        152 => (0, RKind::Any),
        154 => (f(1.0), RKind::Any),
        155 => (f(1.0), RKind::Any),
        156 => (0, RKind::Bool),
        157 => (0, RKind::Bool),
        158 => (f(1.0), RKind::Any),
        159 => (0, RKind::Any),
        160 => (0, RKind::Any),
        161 => (1, RKind::Bool),
        162 => (0xffff_ffff, RKind::Any),
        163 => (0, RKind::Range(0, 1)),
        165 => (0, RKind::Any),
        166 => (f(1.0), RKind::Any),
        167 => (0, RKind::Bool),
        168 => (0xf, RKind::Any),
        170 => (0, RKind::Any),
        171 => (1, RKind::Range(1, 5)),
        172 => (3, RKind::Set(&[1, 2, 3, 5])),
        173 => (1, RKind::Set(&[1, 2, 3, 5])),
        174 => (0, RKind::Bool),
        175 => (0, RKind::Any),
        176 => (0, RKind::Bool),
        178 | 179 => (f(1.0), RKind::Any),
        180 | 181 => (0, RKind::Any),
        182 => (f(1.0), RKind::Any),
        183 => (0, RKind::Any),
        184 => (0, RKind::Bool),
        185 => (0, RKind::Bool),
        186..=188 => (1, RKind::Range(1, 8)),
        189 => (8, RKind::Range(1, 8)),
        190..=192 => (0xf, RKind::Any),
        193 => (0xffff_ffff, RKind::Any),
        194 => (0, RKind::Bool),
        195 => (0, RKind::Any),
        198..=205 => (0, RKind::Any),
        206 => (0, RKind::Bool),
        207 => (2, RKind::Range(1, 17)),
        208 => (1, RKind::Range(1, 17)),
        209 => (1, RKind::Range(1, 5)),
        _ => return None,
    })
}

fn texture_stage_state_spec(state: u32) -> Option<RKind> {
    Some(match state {
        1 | 4 => RKind::Range(1, 26),
        2 | 3 | 5 | 6 | 26 | 27 => RKind::D3dta,
        7..=10 | 22 | 23 | 32 => RKind::Any,
        11 => RKind::TexCoordIndex,
        24 => RKind::TextureTransformFlags,
        28 => RKind::Set(&[D3DTA_CURRENT, D3DTA_TEMP]),
        _ => return None,
    })
}

fn sampler_state_spec(state: u32) -> Option<RKind> {
    Some(match state {
        1..=3 => RKind::Range(1, 5),
        4 => RKind::Any,
        5 | 6 => RKind::Set(&[1, 2, 3]),
        7 => RKind::Range(0, 2),
        8 | 9 | 12 | 13 => RKind::Any,
        10 => RKind::Range(1, MAX_ANISOTROPY),
        11 => RKind::Bool,
        _ => return None,
    })
}

const PIXEL_RENDER_STATES: &[u32] = &[
    7, 29, 8, 9, 14, 15, 16, 19, 20, 23, 24, 25, 26, 36, 37, 38, 27, 195, 52, 53, 54, 55, 56, 57,
    58, 59, 60, 128, 129, 130, 131, 132, 133, 134, 135, 198, 199, 200, 201, 202, 203, 204, 205,
    142, 148, 147, 145, 146, 168, 171, 174, 175, 176, 185, 186, 187, 188, 189, 190, 191, 192, 193,
    194, 206, 207, 208, 209,
];

const VERTEX_RENDER_STATES: &[u32] = &[
    22, 34, 35, 36, 37, 38, 48, 139, 141, 140, 136, 137, 142, 148, 147, 145, 146, 151, 152, 154,
    155, 156, 157, 158, 159, 160, 161, 162, 163, 166, 167, 170, 172, 173, 178, 179, 180, 181, 182,
    183, 184,
];

pub struct D3d9StateBlock {
    pub block_type: u32,
    pub render: Vec<(u32, u32)>,
    pub tss: Vec<(u32, u32, u32)>,
    pub sampler: Vec<(u32, u32, u32)>,
    pub transform: Vec<(u32, [f32; 16])>,
    pub viewport: Option<Viewport>,
    pub scissor: Option<Scissor>,
    pub material: Option<[u32; 17]>,
    pub lights: Vec<(u32, [u32; 26])>,
    pub light_enabled: Vec<(u32, bool)>,
    pub clip: Vec<(u32, [f32; 4])>,
    pub textures: Vec<(u32, u32)>,
    pub streams: Vec<(u32, StreamSource)>,
    pub stream_freq: Vec<(u32, u32)>,
    pub indices: Option<u32>,
    pub fvf: Option<u32>,
    pub declaration: Option<u32>,
    pub vertex_shader: Option<u32>,
    pub pixel_shader: Option<u32>,
    pub vs_const_f: Vec<(u32, [f32; 4])>,
    pub vs_const_i: Vec<(u32, [i32; 4])>,
    pub vs_const_b: Vec<(u32, u32)>,
    pub ps_const_f: Vec<(u32, [f32; 4])>,
    pub ps_const_i: Vec<(u32, [i32; 4])>,
    pub ps_const_b: Vec<(u32, u32)>,
}

impl D3d9StateBlock {
    fn new(block_type: u32) -> Self {
        Self {
            block_type,
            render: Vec::new(),
            tss: Vec::new(),
            sampler: Vec::new(),
            transform: Vec::new(),
            viewport: None,
            scissor: None,
            material: None,
            lights: Vec::new(),
            light_enabled: Vec::new(),
            clip: Vec::new(),
            textures: Vec::new(),
            streams: Vec::new(),
            stream_freq: Vec::new(),
            indices: None,
            fvf: None,
            declaration: None,
            vertex_shader: None,
            pixel_shader: None,
            vs_const_f: Vec::new(),
            vs_const_i: Vec::new(),
            vs_const_b: Vec::new(),
            ps_const_f: Vec::new(),
            ps_const_i: Vec::new(),
            ps_const_b: Vec::new(),
        }
    }

    fn set_render(&mut self, state: u32, value: u32) {
        put(&mut self.render, state, value, |entry| entry.0);
    }
    fn set_tss(&mut self, stage: u32, state: u32, value: u32) {
        if let Some(entry) = self
            .tss
            .iter_mut()
            .find(|entry| entry.0 == stage && entry.1 == state)
        {
            entry.2 = value;
        } else {
            self.tss.push((stage, state, value));
        }
    }
    fn set_sampler(&mut self, sampler: u32, state: u32, value: u32) {
        if let Some(entry) = self
            .sampler
            .iter_mut()
            .find(|entry| entry.0 == sampler && entry.1 == state)
        {
            entry.2 = value;
        } else {
            self.sampler.push((sampler, state, value));
        }
    }
    fn set_texture(&mut self, stage: u32, value: u32) {
        put(&mut self.textures, stage, value, |entry| entry.0);
    }
    fn set_stream_freq(&mut self, stream: u32, value: u32) {
        put(&mut self.stream_freq, stream, value, |entry| entry.0);
    }

    fn bindings(&self) -> D3d9Bindings {
        let mut bindings = D3d9Bindings::default();
        for (stage, id) in &self.textures {
            if let Some(slot) = bindings.textures.get_mut(*stage as usize) {
                *slot = *id;
            }
        }
        for (stage, id) in &self.streams {
            if let Some(slot) = bindings.streams.get_mut(*stage as usize) {
                *slot = id.buffer;
            }
        }
        bindings.indices = self.indices.unwrap_or(0);
        bindings.vertex_shader = self.vertex_shader.unwrap_or(0);
        bindings.pixel_shader = self.pixel_shader.unwrap_or(0);
        bindings.declaration = self.declaration.unwrap_or(0);
        bindings
    }

    fn capture_from(&mut self, state: &D3d9State) {
        for entry in self.render.iter_mut() {
            entry.1 = state.render[entry.0 as usize];
        }
        for entry in self.tss.iter_mut() {
            entry.2 = state.tss[entry.0 as usize][entry.1 as usize];
        }
        for entry in self.sampler.iter_mut() {
            entry.2 = state.sampler[entry.0 as usize][entry.1 as usize];
        }
        for entry in self.transform.iter_mut() {
            entry.1 = state.transform[entry.0 as usize];
        }
        if self.viewport.is_some() {
            self.viewport = Some(*state.viewport());
        }
        if self.scissor.is_some() {
            self.scissor = Some(state.scissor());
        }
        if self.material.is_some() {
            self.material = Some(state.material);
        }
        for entry in self.lights.iter_mut() {
            entry.1 = state
                .lights
                .get(&entry.0)
                .copied()
                .unwrap_or_else(default_light);
        }
        for entry in self.light_enabled.iter_mut() {
            entry.1 = state.light_enabled.get(&entry.0).copied().unwrap_or(false);
        }
        for entry in self.clip.iter_mut() {
            entry.1 = state.clip.get(&entry.0).copied().unwrap_or([0.0; 4]);
        }
        for entry in self.textures.iter_mut() {
            entry.1 = state.textures[entry.0 as usize];
        }
        for entry in self.streams.iter_mut() {
            entry.1 = state.streams[entry.0 as usize];
        }
        for entry in self.stream_freq.iter_mut() {
            entry.1 = state.stream_freq[entry.0 as usize];
        }
        if self.indices.is_some() {
            self.indices = Some(state.indices);
        }
        if self.fvf.is_some() {
            self.fvf = Some(state.fvf);
        }
        if self.declaration.is_some() {
            self.declaration = Some(state.declaration);
        }
        if self.vertex_shader.is_some() {
            self.vertex_shader = Some(state.vertex_shader);
        }
        if self.pixel_shader.is_some() {
            self.pixel_shader = Some(state.pixel_shader);
        }
        for entry in self.vs_const_f.iter_mut() {
            entry.1 = state.vs_const_f[entry.0 as usize];
        }
        for entry in self.vs_const_i.iter_mut() {
            entry.1 = state.vs_const_i[entry.0 as usize];
        }
        for entry in self.vs_const_b.iter_mut() {
            entry.1 = state.vs_const_b[entry.0 as usize];
        }
        for entry in self.ps_const_f.iter_mut() {
            entry.1 = state.ps_const_f[entry.0 as usize];
        }
        for entry in self.ps_const_i.iter_mut() {
            entry.1 = state.ps_const_i[entry.0 as usize];
        }
        for entry in self.ps_const_b.iter_mut() {
            entry.1 = state.ps_const_b[entry.0 as usize];
        }
    }

    fn apply_to(&self, state: &mut D3d9State) {
        for (index, value) in &self.render {
            state.put_render(*index as usize, *value);
        }
        for (stage, index, value) in &self.tss {
            state.put_tss(*stage as usize, *index as usize, *value);
        }
        for (sampler, index, value) in &self.sampler {
            state.put_sampler(*sampler as usize, *index as usize, *value);
        }
        for (index, value) in &self.transform {
            state.transform[*index as usize] = *value;
        }
        if let Some(viewport) = self.viewport {
            state.viewport = Some(viewport);
        }
        if let Some(scissor) = self.scissor {
            state.scissor = Some(scissor);
        }
        if let Some(material) = self.material {
            state.material = material;
        }
        for (index, value) in &self.lights {
            state.lights.insert(*index, *value);
        }
        for (index, value) in &self.light_enabled {
            state.light_enabled.insert(*index, *value);
        }
        for (index, value) in &self.clip {
            state.clip.insert(*index, *value);
        }
        for (stage, id) in &self.textures {
            state.put_texture(*stage as usize, *id);
        }
        for (stream, value) in &self.streams {
            state.put_stream(*stream as usize, *value);
        }
        for (stream, value) in &self.stream_freq {
            state.put_stream_freq(*stream as usize, *value);
        }
        if let Some(indices) = self.indices {
            state.indices = indices;
        }
        if let Some(fvf) = self.fvf {
            state.put_declaration(state.declaration, fvf);
        }
        if let Some(declaration) = self.declaration {
            state.put_declaration(declaration, state.fvf);
        }
        if let Some(shader) = self.vertex_shader {
            state.put_shader(false, shader);
        }
        if let Some(shader) = self.pixel_shader {
            state.put_shader(true, shader);
        }
        for (index, value) in &self.vs_const_f {
            state.vs_const_f[*index as usize] = *value;
        }
        for (index, value) in &self.vs_const_i {
            state.vs_const_i[*index as usize] = *value;
        }
        for (index, value) in &self.vs_const_b {
            state.vs_const_b[*index as usize] = *value;
        }
        for (index, value) in &self.ps_const_f {
            state.ps_const_f[*index as usize] = *value;
        }
        for (index, value) in &self.ps_const_i {
            state.ps_const_i[*index as usize] = *value;
        }
        for (index, value) in &self.ps_const_b {
            state.ps_const_b[*index as usize] = *value;
        }
    }
}

fn put<T: Copy>(list: &mut Vec<(u32, T)>, index: u32, value: T, key: fn(&(u32, T)) -> u32) {
    if let Some(entry) = list.iter_mut().find(|entry| key(entry) == index) {
        entry.1 = value;
    } else {
        list.push((index, value));
    }
}

pub struct D3d9State {
    render: [u32; 256],
    tss: [[u32; 33]; MAX_TEXTURE_STAGES],
    sampler: [[u32; 14]; MAX_SAMPLERS],
    transform: [[f32; 16]; 512],
    viewport: Option<Viewport>,
    scissor: Option<Scissor>,
    material: [u32; 17],
    lights: BTreeMap<u32, [u32; 26]>,
    light_enabled: BTreeMap<u32, bool>,
    clip: BTreeMap<u32, [f32; 4]>,
    textures: [u32; MAX_SAMPLERS],
    streams: [StreamSource; MAX_STREAMS],
    stream_freq: [u32; MAX_STREAMS],
    indices: u32,
    fvf: u32,
    declaration: u32,
    vertex_shader: u32,
    pixel_shader: u32,
    vs_const_f: [[f32; 4]; VS_CONST_F],
    vs_const_i: [[i32; 4]; VS_CONST_I],
    vs_const_b: [u32; VS_CONST_B],
    ps_const_f: [[f32; 4]; PS_CONST_F],
    ps_const_i: [[i32; 4]; PS_CONST_I],
    ps_const_b: [u32; PS_CONST_B],
    target_viewport: Viewport,
    blocks: BTreeMap<u32, D3d9StateBlock>,
    next_block: u32,
    recording: Option<D3d9StateBlock>,
    recording_handle: u32,
    pipeline_fingerprint: u64,
    selection_revision: u64,
}

impl Default for D3d9State {
    fn default() -> Self {
        let mut render = [0u32; 256];
        for state in 0..256u32 {
            if let Some((default, _)) = render_state_spec(state) {
                render[state as usize] = default;
            }
        }
        let mut sampler = [[0u32; 14]; MAX_SAMPLERS];
        for slot in sampler.iter_mut() {
            slot[1] = 1;
            slot[2] = 1;
            slot[3] = 1;
            slot[5] = 1;
            slot[6] = 1;
            slot[10] = 1;
            slot[13] = 256;
        }
        let mut tss = [[0u32; 33]; MAX_TEXTURE_STAGES];
        for (stage, slot) in tss.iter_mut().enumerate() {
            slot[1] = if stage == 0 { 4 } else { 1 };
            slot[2] = 2;
            slot[3] = 1;
            slot[4] = if stage == 0 { 2 } else { 1 };
            slot[5] = 2;
            slot[6] = 1;
            slot[11] = stage as u32;
            slot[26] = 1;
            slot[27] = 1;
            slot[28] = 1;
        }
        let mut material = [0u32; 17];
        for value in material.iter_mut().take(4) {
            *value = 1.0f32.to_bits();
        }
        let mut transform = [[0.0f32; 16]; 512];
        for index in 0..512u32 {
            if transform_index(index).is_some() {
                transform[index as usize][0] = 1.0;
                transform[index as usize][5] = 1.0;
                transform[index as usize][10] = 1.0;
                transform[index as usize][15] = 1.0;
            }
        }
        let mut state = Self {
            render,
            tss,
            sampler,
            transform,
            viewport: None,
            scissor: None,
            material,
            lights: BTreeMap::new(),
            light_enabled: BTreeMap::new(),
            clip: BTreeMap::new(),
            textures: [0; MAX_SAMPLERS],
            streams: [StreamSource::default(); MAX_STREAMS],
            stream_freq: [1; MAX_STREAMS],
            indices: 0,
            fvf: 0,
            declaration: 0,
            vertex_shader: 0,
            pixel_shader: 0,
            vs_const_f: [[0.0; 4]; VS_CONST_F],
            vs_const_i: [[0; 4]; VS_CONST_I],
            vs_const_b: [0; VS_CONST_B],
            ps_const_f: [[0.0; 4]; PS_CONST_F],
            ps_const_i: [[0; 4]; PS_CONST_I],
            ps_const_b: [0; PS_CONST_B],
            target_viewport: Viewport {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                min_z: 0.0,
                max_z: 1.0,
            },
            blocks: BTreeMap::new(),
            next_block: 1,
            recording: None,
            recording_handle: 0,
            pipeline_fingerprint: 0,
            selection_revision: 0,
        };
        state.pipeline_fingerprint = state.full_fingerprint();
        state
    }
}

impl Default for StreamSource {
    fn default() -> Self {
        Self {
            buffer: 0,
            offset: 0,
            stride: 0,
        }
    }
}

impl D3d9State {
    pub fn set_target_size(&mut self, width: u32, height: u32) {
        self.target_viewport.width = width;
        self.target_viewport.height = height;
    }

    pub fn viewport(&self) -> &Viewport {
        self.viewport.as_ref().unwrap_or(&self.target_viewport)
    }

    pub fn scissor(&self) -> Scissor {
        self.scissor.unwrap_or(Scissor {
            left: 0,
            top: 0,
            right: self.target_viewport.width as i32,
            bottom: self.target_viewport.height as i32,
        })
    }

    pub fn set_render_state(&mut self, state: u32, value: u32) -> Result<(), RenderError> {
        let (_, kind) = render_state_spec(state).ok_or_else(|| {
            invalid(
                "SetRenderState",
                format!("render state {state} is not a D3DRENDERSTATETYPE value"),
            )
        })?;
        let value = validate_kind(&kind, value, "SetRenderState", state)?;
        if self.recording.is_some() {
            self.recording.as_mut().unwrap().set_render(state, value);
        } else {
            self.put_render(state as usize, value);
        }
        Ok(())
    }

    pub fn render_state(&self, state: u32) -> Result<u32, RenderError> {
        render_state_spec(state).ok_or_else(|| {
            invalid(
                "GetRenderState",
                format!("render state {state} is not a D3DRENDERSTATETYPE value"),
            )
        })?;
        Ok(self.render[state as usize])
    }

    pub fn set_texture_stage_state(
        &mut self,
        stage: u32,
        state: u32,
        value: u32,
    ) -> Result<(), RenderError> {
        if stage as usize >= MAX_TEXTURE_STAGES {
            return Err(invalid(
                "SetTextureStageState",
                format!("texture stage {stage} is out of range"),
            ));
        }
        let kind = texture_stage_state_spec(state).ok_or_else(|| {
            invalid(
                "SetTextureStageState",
                format!("texture stage state {state} is not a D3DTEXTURESTAGESTATETYPE value"),
            )
        })?;
        let value = validate_kind(&kind, value, "SetTextureStageState", state)?;
        if self.recording.is_some() {
            self.recording
                .as_mut()
                .unwrap()
                .set_tss(stage, state, value);
        } else {
            self.put_tss(stage as usize, state as usize, value);
        }
        Ok(())
    }

    pub(crate) fn render_raw(&self, state: u32) -> u32 {
        self.render[state as usize]
    }

    pub(crate) fn pipeline_fingerprint(&self) -> u64 {
        self.pipeline_fingerprint
    }

    pub(crate) fn memo_trace_fingerprints(&self) -> [u64; 3] {
        let mut resources = self.pipeline_fingerprint;
        for stage in 0..MAX_SAMPLERS {
            resources ^= fingerprint_word(FP_TEXTURE + stage as u64, self.textures[stage]);
        }
        let mut bindings = resources;
        for (stage, row) in self.sampler.iter().enumerate() {
            for (index, &value) in row.iter().enumerate() {
                bindings ^= fingerprint_word(FP_SAMPLER + (stage * 14 + index) as u64, value);
            }
        }
        [self.pipeline_fingerprint, resources, bindings]
    }

    pub(crate) fn selection_revision(&self) -> u64 {
        self.selection_revision
    }

    fn track(&mut self, slot: u64, old: u32, new: u32) {
        if old != new {
            self.pipeline_fingerprint ^= fingerprint_word(slot, old) ^ fingerprint_word(slot, new);
            if matches!(slot,
                7 | 8 | 9 | 14 | 19 | 20 | 22 | 23 | 27 | 28 | 35
                | 52..=56 | 58 | 59 | 152 | 162 | 168 | 171 | 175
                | 185..=189 | 195 | 206..=209 | FP_VERTEX_SHADER | FP_PIXEL_SHADER
            ) {
                self.selection_revision += 1;
            }
        }
    }

    fn put_render(&mut self, index: usize, value: u32) {
        let old = std::mem::replace(&mut self.render[index], value);
        self.track(index as u64, old, value);
    }

    fn put_tss(&mut self, stage: usize, index: usize, value: u32) {
        let old = std::mem::replace(&mut self.tss[stage][index], value);
        self.track(FP_TSS + (stage * 33 + index) as u64, old, value);
    }

    fn put_sampler(&mut self, sampler: usize, index: usize, value: u32) {
        let old = std::mem::replace(&mut self.sampler[sampler][index], value);
        self.track(FP_SAMPLER + (sampler * 14 + index) as u64, old, value);
    }

    fn put_texture(&mut self, stage: usize, id: u32) {
        let old = std::mem::replace(&mut self.textures[stage], id);
        self.track(FP_TEXTURE + stage as u64, old, id);
    }

    fn put_stream(&mut self, stream: usize, source: StreamSource) {
        let old = std::mem::replace(&mut self.streams[stream], source);
        self.track(FP_STRIDE + stream as u64, old.stride, source.stride);
    }

    fn put_stream_freq(&mut self, stream: usize, divider: u32) {
        let old = std::mem::replace(&mut self.stream_freq[stream], divider);
        self.track(FP_FREQ + stream as u64, old, divider);
    }

    fn put_declaration(&mut self, handle: u32, fvf: u32) {
        let old = std::mem::replace(&mut self.declaration, handle);
        self.track(FP_DECLARATION, old, handle);
        let old = std::mem::replace(&mut self.fvf, fvf);
        self.track(FP_FVF, old, fvf);
    }

    fn put_shader(&mut self, pixel: bool, handle: u32) {
        let (field, slot) = if pixel {
            (&mut self.pixel_shader, FP_PIXEL_SHADER)
        } else {
            (&mut self.vertex_shader, FP_VERTEX_SHADER)
        };
        let old = std::mem::replace(field, handle);
        self.track(slot, old, handle);
    }

    fn full_fingerprint(&self) -> u64 {
        let mut hash = 0;
        for (index, &value) in self.render.iter().enumerate() {
            hash ^= fingerprint_word(index as u64, value);
        }
        for (stage, row) in self.tss.iter().enumerate() {
            for (index, &value) in row.iter().enumerate() {
                hash ^= fingerprint_word(FP_TSS + (stage * 33 + index) as u64, value);
            }
        }
        for (sampler, row) in self.sampler.iter().enumerate() {
            for (index, &value) in row.iter().enumerate() {
                hash ^= fingerprint_word(FP_SAMPLER + (sampler * 14 + index) as u64, value);
            }
        }
        for stage in 0..MAX_SAMPLERS {
            hash ^= fingerprint_word(FP_TEXTURE + stage as u64, self.textures[stage]);
        }
        for stream in 0..MAX_STREAMS {
            hash ^= fingerprint_word(FP_STRIDE + stream as u64, self.streams[stream].stride);
            hash ^= fingerprint_word(FP_FREQ + stream as u64, self.stream_freq[stream]);
        }
        hash ^ fingerprint_word(FP_DECLARATION, self.declaration)
            ^ fingerprint_word(FP_FVF, self.fvf)
            ^ fingerprint_word(FP_VERTEX_SHADER, self.vertex_shader)
            ^ fingerprint_word(FP_PIXEL_SHADER, self.pixel_shader)
    }

    pub(crate) fn texture_stage_raw(&self, stage: u32, state: u32) -> u32 {
        self.tss[stage as usize][state as usize]
    }

    pub(crate) fn sampler_raw(&self, sampler: u32, state: u32) -> u32 {
        self.sampler[sampler as usize][state as usize]
    }

    pub fn texture_stage_state(&self, stage: u32, state: u32) -> Result<u32, RenderError> {
        if stage as usize >= MAX_TEXTURE_STAGES {
            return Err(invalid(
                "GetTextureStageState",
                format!("texture stage {stage} is out of range"),
            ));
        }
        if texture_stage_state_spec(state).is_none() {
            return Err(invalid(
                "GetTextureStageState",
                format!("texture stage state {state} is not a D3DTEXTURESTAGESTATETYPE value"),
            ));
        }
        Ok(self.tss[stage as usize][state as usize])
    }

    pub fn set_sampler_state(
        &mut self,
        sampler: u32,
        state: u32,
        value: u32,
    ) -> Result<(), RenderError> {
        if sampler as usize >= MAX_SAMPLERS {
            return Err(invalid(
                "SetSamplerState",
                format!("sampler {sampler} is out of range"),
            ));
        }
        let kind = sampler_state_spec(state).ok_or_else(|| {
            invalid(
                "SetSamplerState",
                format!("sampler state {state} is not a D3DSAMPLERSTATETYPE value"),
            )
        })?;
        let value = validate_kind(&kind, value, "SetSamplerState", state)?;
        if self.recording.is_some() {
            self.recording
                .as_mut()
                .unwrap()
                .set_sampler(sampler, state, value);
        } else {
            self.put_sampler(sampler as usize, state as usize, value);
        }
        Ok(())
    }

    pub fn sampler_state(&self, sampler: u32, state: u32) -> Result<u32, RenderError> {
        if sampler as usize >= MAX_SAMPLERS {
            return Err(invalid(
                "GetSamplerState",
                format!("sampler {sampler} is out of range"),
            ));
        }
        if sampler_state_spec(state).is_none() {
            return Err(invalid(
                "GetSamplerState",
                format!("sampler state {state} is not a D3DSAMPLERSTATETYPE value"),
            ));
        }
        Ok(self.sampler[sampler as usize][state as usize])
    }

    pub fn set_transform(&mut self, state: u32, value: [f32; 16]) -> Result<(), RenderError> {
        let index = transform_index(state).ok_or_else(|| {
            invalid(
                "SetTransform",
                format!("transform state {state} is not a D3DTRANSFORMSTATETYPE value"),
            )
        })?;
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            if let Some(entry) = block.transform.iter_mut().find(|entry| entry.0 == index) {
                entry.1 = value;
            } else {
                block.transform.push((index, value));
            }
        } else {
            self.transform[index as usize] = value;
        }
        Ok(())
    }

    pub fn transform(&self, state: u32) -> Result<[f32; 16], RenderError> {
        let index = transform_index(state).ok_or_else(|| {
            invalid(
                "GetTransform",
                format!("transform state {state} is not a D3DTRANSFORMSTATETYPE value"),
            )
        })?;
        Ok(self.transform[index as usize])
    }

    pub fn multiply_transform(&mut self, state: u32, matrix: [f32; 16]) -> Result<(), RenderError> {
        let index = transform_index(state).ok_or_else(|| {
            invalid(
                "MultiplyTransform",
                format!("transform state {state} is not a D3DTRANSFORMSTATETYPE value"),
            )
        })?;
        let current = self
            .recording
            .as_ref()
            .and_then(|block| {
                block
                    .transform
                    .iter()
                    .find(|entry| entry.0 == index)
                    .map(|entry| entry.1)
            })
            .unwrap_or(self.transform[index as usize]);
        let product = multiply_matrices(current, matrix);
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            if let Some(entry) = block.transform.iter_mut().find(|entry| entry.0 == index) {
                entry.1 = product;
            } else {
                block.transform.push((index, product));
            }
        } else {
            self.transform[index as usize] = product;
        }
        Ok(())
    }

    pub fn set_viewport(&mut self, viewport: Viewport) -> Result<(), RenderError> {
        if viewport.width == 0 || viewport.height == 0 {
            return Err(invalid(
                "SetViewport",
                "viewport width and height must be nonzero",
            ));
        }
        if !viewport.min_z.is_finite() || !viewport.max_z.is_finite() {
            return Err(invalid(
                "SetViewport",
                "viewport depth range must be finite",
            ));
        }
        if self.recording.is_some() {
            self.recording.as_mut().unwrap().viewport = Some(viewport);
        } else {
            self.viewport = Some(viewport);
        }
        Ok(())
    }

    pub fn set_scissor(&mut self, scissor: Scissor) -> Result<(), RenderError> {
        if scissor.right < scissor.left || scissor.bottom < scissor.top {
            return Err(invalid(
                "SetScissorRect",
                "scissor right/bottom must not be less than left/top",
            ));
        }
        if self.recording.is_some() {
            self.recording.as_mut().unwrap().scissor = Some(scissor);
        } else {
            self.scissor = Some(scissor);
        }
        Ok(())
    }

    pub fn set_material(&mut self, material: [u32; 17]) -> Result<(), RenderError> {
        if self.recording.is_some() {
            self.recording.as_mut().unwrap().material = Some(material);
        } else {
            self.material = material;
        }
        Ok(())
    }

    pub fn material(&self) -> [u32; 17] {
        self.material
    }

    pub fn set_light(&mut self, index: u32, light: [u32; 26]) -> Result<(), RenderError> {
        match light[0] {
            1 | 2 => {
                for slot in [21usize, 22, 23] {
                    if !(f32::from_bits(light[slot]) >= 0.0) {
                        return Err(invalid(
                            "SetLight",
                            "point and spot light attenuation must be nonnegative",
                        ));
                    }
                }
            }
            3 => {}
            other => {
                return Err(invalid(
                    "SetLight",
                    format!("D3DLIGHTTYPE {other} is out of range"),
                ));
            }
        }
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            if let Some(entry) = block.lights.iter_mut().find(|entry| entry.0 == index) {
                entry.1 = light;
            } else {
                block.lights.push((index, light));
            }
            if !block.light_enabled.iter().any(|entry| entry.0 == index) {
                block.light_enabled.push((index, false));
            }
        } else {
            self.lights.insert(index, light);
            self.light_enabled.entry(index).or_insert(false);
        }
        Ok(())
    }

    pub fn light(&self, index: u32) -> Result<[u32; 26], RenderError> {
        self.lights
            .get(&index)
            .copied()
            .ok_or_else(|| invalid("GetLight", format!("light {index} does not exist")))
    }

    fn light_enable_state(&self, index: u32) -> (bool, usize) {
        match self.recording.as_ref() {
            Some(block) => {
                let currently_enabled = block
                    .light_enabled
                    .iter()
                    .find(|entry| entry.0 == index)
                    .map(|entry| entry.1)
                    .unwrap_or_else(|| self.light_enabled.get(&index).copied().unwrap_or(false));
                let mut active = self.light_enabled.values().filter(|value| **value).count();
                for entry in &block.light_enabled {
                    let live = self.light_enabled.get(&entry.0).copied().unwrap_or(false);
                    if entry.1 && !live {
                        active += 1;
                    } else if !entry.1 && live {
                        active = active.saturating_sub(1);
                    }
                }
                (currently_enabled, active)
            }
            None => (
                self.light_enabled.get(&index).copied().unwrap_or(false),
                self.light_enabled.values().filter(|value| **value).count(),
            ),
        }
    }

    pub fn set_light_enable(&mut self, index: u32, enable: bool) -> Result<(), RenderError> {
        if enable {
            let (currently_enabled, active) = self.light_enable_state(index);
            if !currently_enabled && active >= MAX_ACTIVE_LIGHTS {
                return Err(invalid(
                    "LightEnable",
                    format!("more than {MAX_ACTIVE_LIGHTS} active lights are not supported"),
                ));
            }
        }
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            if !block.lights.iter().any(|entry| entry.0 == index) {
                block.lights.push((index, default_light()));
            }
            if let Some(entry) = block
                .light_enabled
                .iter_mut()
                .find(|entry| entry.0 == index)
            {
                entry.1 = enable;
            } else {
                block.light_enabled.push((index, enable));
            }
        } else {
            self.lights.entry(index).or_insert_with(default_light);
            self.light_enabled.insert(index, enable);
        }
        Ok(())
    }

    pub fn light_enabled(&self, index: u32) -> Result<bool, RenderError> {
        if !self.lights.contains_key(&index) && !self.light_enabled.contains_key(&index) {
            return Err(invalid(
                "GetLightEnable",
                format!("light {index} does not exist"),
            ));
        }
        Ok(self.light_enabled.get(&index).copied().unwrap_or(false))
    }

    pub fn set_clip_plane(&mut self, index: u32, plane: [f32; 4]) -> Result<(), RenderError> {
        if index as usize >= MAX_CLIP_PLANES {
            return Err(invalid(
                "SetClipPlane",
                format!("clip plane {index} is out of range"),
            ));
        }
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            if let Some(entry) = block.clip.iter_mut().find(|entry| entry.0 == index) {
                entry.1 = plane;
            } else {
                block.clip.push((index, plane));
            }
        } else {
            self.clip.insert(index, plane);
        }
        Ok(())
    }

    pub fn clip_plane(&self, index: u32) -> [f32; 4] {
        self.clip.get(&index).copied().unwrap_or([0.0; 4])
    }

    pub fn set_texture(&mut self, stage: u32, id: u32) -> Result<(), RenderError> {
        if stage as usize >= MAX_SAMPLERS {
            return Err(invalid(
                "SetTexture",
                format!("texture stage {stage} is out of range"),
            ));
        }
        if self.recording.is_some() {
            self.recording.as_mut().unwrap().set_texture(stage, id);
        } else {
            self.put_texture(stage as usize, id);
        }
        Ok(())
    }

    pub fn texture(&self, stage: u32) -> Result<u32, RenderError> {
        if stage as usize >= MAX_SAMPLERS {
            return Err(invalid(
                "GetTexture",
                format!("texture stage {stage} is out of range"),
            ));
        }
        Ok(self.textures[stage as usize])
    }

    pub fn set_stream_source(
        &mut self,
        stream: u32,
        source: StreamSource,
    ) -> Result<(), RenderError> {
        if stream as usize >= MAX_STREAMS {
            return Err(invalid(
                "SetStreamSource",
                format!("stream {stream} is out of range"),
            ));
        }
        if source.stride > MAX_STREAM_STRIDE {
            return Err(invalid(
                "SetStreamSource",
                format!("stream stride {} exceeds MaxStreamStride", source.stride),
            ));
        }
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            if let Some(entry) = block.streams.iter_mut().find(|entry| entry.0 == stream) {
                entry.1 = source;
            } else {
                block.streams.push((stream, source));
            }
        } else {
            self.put_stream(stream as usize, source);
        }
        Ok(())
    }

    pub fn stream_source(&self, stream: u32) -> Result<StreamSource, RenderError> {
        if stream as usize >= MAX_STREAMS {
            return Err(invalid(
                "GetStreamSource",
                format!("stream {stream} is out of range"),
            ));
        }
        Ok(self.streams[stream as usize])
    }

    pub fn set_stream_freq(&mut self, stream: u32, divider: u32) -> Result<(), RenderError> {
        if stream as usize >= MAX_STREAMS {
            return Err(invalid(
                "SetStreamSourceFreq",
                format!("stream {stream} is out of range"),
            ));
        }
        if divider == 0 {
            return Err(invalid(
                "SetStreamSourceFreq",
                "stream source frequency must be nonzero",
            ));
        }
        if divider & D3DSTREAMSOURCE_MASK != 0 {
            return Err(unsupported(
                "SetStreamSourceFreq",
                "instanced stream source frequencies are not implemented",
            ));
        }
        if divider > 1 {
            return Err(unsupported(
                "SetStreamSourceFreq",
                "non-instancing stream source frequencies greater than 1 are not implemented",
            ));
        }
        if self.recording.is_some() {
            self.recording
                .as_mut()
                .unwrap()
                .set_stream_freq(stream, divider);
        } else {
            self.put_stream_freq(stream as usize, divider);
        }
        Ok(())
    }

    pub fn stream_freq(&self, stream: u32) -> Result<u32, RenderError> {
        if stream as usize >= MAX_STREAMS {
            return Err(invalid(
                "GetStreamSourceFreq",
                format!("stream {stream} is out of range"),
            ));
        }
        Ok(self.stream_freq[stream as usize])
    }

    pub fn set_indices(&mut self, id: u32) {
        if self.recording.is_some() {
            self.recording.as_mut().unwrap().indices = Some(id);
        } else {
            self.indices = id;
        }
    }

    pub fn set_declaration(&mut self, handle: u32, fvf: u32) {
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            block.declaration = Some(handle);
            block.fvf = Some(fvf);
        } else {
            self.put_declaration(handle, fvf);
        }
    }

    pub fn fvf(&self) -> u32 {
        self.fvf
    }

    pub fn declaration(&self) -> u32 {
        self.declaration
    }

    pub fn set_shader(&mut self, pixel: bool, handle: u32) {
        if self.recording.is_some() {
            let block = self.recording.as_mut().unwrap();
            if pixel {
                block.pixel_shader = Some(handle);
            } else {
                block.vertex_shader = Some(handle);
            }
        } else {
            self.put_shader(pixel, handle);
        }
    }

    pub fn shader(&self, pixel: bool) -> u32 {
        if pixel {
            self.pixel_shader
        } else {
            self.vertex_shader
        }
    }

    pub fn set_constants_f(
        &mut self,
        pixel: bool,
        start: u32,
        values: &[f32],
    ) -> Result<(), RenderError> {
        let limit = if pixel { PS_CONST_F } else { VS_CONST_F };
        if values.len() % 4 != 0 || start as usize + values.len() / 4 > limit {
            return Err(invalid(
                "SetShaderConstantF",
                "shader float constant range is out of bounds",
            ));
        }
        for (vector, chunk) in values.chunks_exact(4).enumerate() {
            let index = start + vector as u32;
            let value = [chunk[0], chunk[1], chunk[2], chunk[3]];
            if self.recording.is_some() {
                let block = self.recording.as_mut().unwrap();
                let list = if pixel {
                    &mut block.ps_const_f
                } else {
                    &mut block.vs_const_f
                };
                put(list, index, value, |entry| entry.0);
            } else if pixel {
                self.ps_const_f[index as usize] = value;
            } else {
                self.vs_const_f[index as usize] = value;
            }
        }
        Ok(())
    }

    pub fn constants_f_bytes(&self, pixel: bool, count: u32) -> Result<&[u8], RenderError> {
        let constants: &[[f32; 4]] = if pixel {
            &self.ps_const_f
        } else {
            &self.vs_const_f
        };
        constants
            .get(..count as usize)
            .map(bytemuck::cast_slice)
            .ok_or_else(|| {
                invalid(
                    "GetShaderConstantF",
                    "shader float constant range is out of bounds",
                )
            })
    }

    pub fn constants_f(
        &self,
        pixel: bool,
        start: u32,
        count: u32,
    ) -> Result<Vec<f32>, RenderError> {
        let limit = if pixel { PS_CONST_F } else { VS_CONST_F };
        if start as usize + count as usize > limit {
            return Err(invalid(
                "GetShaderConstantF",
                "shader float constant range is out of bounds",
            ));
        }
        let mut out = Vec::with_capacity(count as usize * 4);
        for vector in 0..count {
            let index = start as usize + vector as usize;
            let value = if pixel {
                &self.ps_const_f[index]
            } else {
                &self.vs_const_f[index]
            };
            out.extend_from_slice(value);
        }
        Ok(out)
    }

    pub fn set_constants_i(
        &mut self,
        pixel: bool,
        start: u32,
        values: &[i32],
    ) -> Result<(), RenderError> {
        let limit = if pixel { PS_CONST_I } else { VS_CONST_I };
        if values.len() % 4 != 0 || start as usize + values.len() / 4 > limit {
            return Err(invalid(
                "SetShaderConstantI",
                "shader integer constant range is out of bounds",
            ));
        }
        for (vector, chunk) in values.chunks_exact(4).enumerate() {
            let index = start + vector as u32;
            let value = [chunk[0], chunk[1], chunk[2], chunk[3]];
            if self.recording.is_some() {
                let block = self.recording.as_mut().unwrap();
                let list = if pixel {
                    &mut block.ps_const_i
                } else {
                    &mut block.vs_const_i
                };
                put(list, index, value, |entry| entry.0);
            } else if pixel {
                self.ps_const_i[index as usize] = value;
            } else {
                self.vs_const_i[index as usize] = value;
            }
        }
        Ok(())
    }

    pub fn constants_i(
        &self,
        pixel: bool,
        start: u32,
        count: u32,
    ) -> Result<Vec<i32>, RenderError> {
        let limit = if pixel { PS_CONST_I } else { VS_CONST_I };
        if start as usize + count as usize > limit {
            return Err(invalid(
                "GetShaderConstantI",
                "shader integer constant range is out of bounds",
            ));
        }
        let mut out = Vec::with_capacity(count as usize * 4);
        for vector in 0..count {
            let index = start as usize + vector as usize;
            let value = if pixel {
                &self.ps_const_i[index]
            } else {
                &self.vs_const_i[index]
            };
            out.extend_from_slice(value);
        }
        Ok(out)
    }

    pub fn set_constants_b(
        &mut self,
        pixel: bool,
        start: u32,
        values: &[u32],
    ) -> Result<(), RenderError> {
        let limit = if pixel { PS_CONST_B } else { VS_CONST_B };
        if start as usize + values.len() > limit {
            return Err(invalid(
                "SetShaderConstantB",
                "shader boolean constant range is out of bounds",
            ));
        }
        for (offset, value) in values.iter().enumerate() {
            let normalized = *value;
            if self.recording.is_some() {
                let block = self.recording.as_mut().unwrap();
                let list = if pixel {
                    &mut block.ps_const_b
                } else {
                    &mut block.vs_const_b
                };
                put(list, start + offset as u32, normalized, |entry| entry.0);
            } else {
                let table = if pixel {
                    &mut self.ps_const_b
                } else {
                    &mut self.vs_const_b
                };
                table[start as usize + offset] = normalized;
            }
        }
        Ok(())
    }

    pub fn constants_b(
        &self,
        pixel: bool,
        start: u32,
        count: u32,
    ) -> Result<Vec<u32>, RenderError> {
        let limit = if pixel { PS_CONST_B } else { VS_CONST_B };
        if start as usize + count as usize > limit {
            return Err(invalid(
                "GetShaderConstantB",
                "shader boolean constant range is out of bounds",
            ));
        }
        let table = if pixel {
            &self.ps_const_b
        } else {
            &self.vs_const_b
        };
        Ok((0..count)
            .map(|offset| table[start as usize + offset as usize])
            .collect())
    }

    pub fn bindings(&self) -> D3d9Bindings {
        D3d9Bindings {
            textures: self.textures,
            streams: self.streams.map(|stream| stream.buffer),
            indices: self.indices,
            vertex_shader: self.vertex_shader,
            pixel_shader: self.pixel_shader,
            declaration: self.declaration,
        }
    }

    pub fn recording_bindings(&self) -> (D3d9Bindings, bool) {
        match self.recording.as_ref() {
            Some(block) => (block.bindings(), true),
            None => (D3d9Bindings::default(), false),
        }
    }

    pub fn create_block(&mut self, block_type: u32) -> Result<u32, RenderError> {
        if self.recording.is_some() {
            return Err(invalid(
                "CreateStateBlock",
                "CreateStateBlock is not valid while the device is recording",
            ));
        }
        if block_type != D3DSBT_ALL
            && block_type != D3DSBT_PIXELSTATE
            && block_type != D3DSBT_VERTEXSTATE
        {
            return Err(invalid(
                "CreateStateBlock",
                format!("D3DSTATEBLOCKTYPE {block_type} is out of range"),
            ));
        }
        let mut block = D3d9StateBlock::new(block_type);
        let all = block_type == D3DSBT_ALL;
        let pixel = all || block_type == D3DSBT_PIXELSTATE;
        let vertex = all || block_type == D3DSBT_VERTEXSTATE;
        let states: &[u32] = match block_type {
            D3DSBT_PIXELSTATE => PIXEL_RENDER_STATES,
            D3DSBT_VERTEXSTATE => VERTEX_RENDER_STATES,
            _ => {
                for state in 0..256u32 {
                    if render_state_spec(state).is_some() {
                        block.render.push((state, self.render[state as usize]));
                    }
                }
                &[]
            }
        };
        for state in states {
            block.render.push((*state, self.render[(*state) as usize]));
        }
        if pixel {
            for stage in 0..MAX_TEXTURE_STAGES {
                for state in 1..=32u32 {
                    block
                        .tss
                        .push((stage as u32, state, self.tss[stage][state as usize]));
                }
            }
            for sampler in 0..MAX_SAMPLERS {
                for state in 1..=12u32 {
                    block.sampler.push((
                        sampler as u32,
                        state,
                        self.sampler[sampler][state as usize],
                    ));
                }
            }
            for stage in 0..MAX_SAMPLERS {
                block.textures.push((stage as u32, self.textures[stage]));
            }
            block.pixel_shader = Some(self.pixel_shader);
            for index in 0..PS_CONST_F {
                block
                    .ps_const_f
                    .push((index as u32, self.ps_const_f[index]));
            }
            for index in 0..PS_CONST_I {
                block
                    .ps_const_i
                    .push((index as u32, self.ps_const_i[index]));
            }
            for index in 0..PS_CONST_B {
                block
                    .ps_const_b
                    .push((index as u32, self.ps_const_b[index]));
            }
        }
        if vertex {
            for index in 0..VS_CONST_F {
                block
                    .vs_const_f
                    .push((index as u32, self.vs_const_f[index]));
            }
            for index in 0..VS_CONST_I {
                block
                    .vs_const_i
                    .push((index as u32, self.vs_const_i[index]));
            }
            for index in 0..VS_CONST_B {
                block
                    .vs_const_b
                    .push((index as u32, self.vs_const_b[index]));
            }
            block.vertex_shader = Some(self.vertex_shader);
            block.declaration = Some(self.declaration);
            block.fvf = Some(self.fvf);
            for (index, value) in &self.lights {
                block.lights.push((*index, *value));
            }
            for (index, value) in &self.light_enabled {
                block.light_enabled.push((*index, *value));
            }
            for stream in 0..MAX_STREAMS {
                block
                    .stream_freq
                    .push((stream as u32, self.stream_freq[stream]));
            }
            for sampler in 0..MAX_SAMPLERS {
                block
                    .sampler
                    .push((sampler as u32, 13, self.sampler[sampler][13]));
            }
            if block_type == D3DSBT_VERTEXSTATE {
                for state in [11u32, 24] {
                    for stage in 0..MAX_TEXTURE_STAGES {
                        block
                            .tss
                            .push((stage as u32, state, self.tss[stage][state as usize]));
                    }
                }
            }
        }
        if all {
            block.viewport = Some(*self.viewport());
            block.scissor = Some(self.scissor());
            block.material = Some(self.material);
            for index in 0..MAX_CLIP_PLANES {
                block.clip.push((
                    index as u32,
                    self.clip.get(&(index as u32)).copied().unwrap_or([0.0; 4]),
                ));
            }
            for index in 0..MAX_STREAMS {
                block.streams.push((index as u32, self.streams[index]));
            }
            block.indices = Some(self.indices);
            for index in 0..512u32 {
                if transform_index(index).is_some() {
                    block
                        .transform
                        .push((index, self.transform[index as usize]));
                }
            }
        }
        let handle = self.next_block;
        self.next_block = self.next_block.checked_add(1).ok_or_else(|| {
            RenderError::out_of_memory("CreateStateBlock", "state block handle range exhausted")
        })?;
        self.blocks.insert(handle, block);
        Ok(handle)
    }

    pub fn begin_block(&mut self) -> Result<(), RenderError> {
        if self.recording.is_some() {
            return Err(invalid(
                "BeginStateBlock",
                "the device is already recording a state block",
            ));
        }
        let handle = self.next_block;
        self.next_block = self.next_block.checked_add(1).ok_or_else(|| {
            RenderError::out_of_memory("BeginStateBlock", "state block handle range exhausted")
        })?;
        self.recording = Some(D3d9StateBlock::new(D3DSBT_ALL));
        self.recording_handle = handle;
        Ok(())
    }

    pub fn end_block(&mut self) -> Result<u32, RenderError> {
        let block = self
            .recording
            .take()
            .ok_or_else(|| invalid("EndStateBlock", "the device is not recording a state block"))?;
        let handle = self.recording_handle;
        self.blocks.insert(handle, block);
        Ok(handle)
    }

    pub fn capture_block(&mut self, handle: u32) -> Result<(), RenderError> {
        if self.recording.is_some() {
            return Err(invalid(
                "CaptureStateBlock",
                "CaptureStateBlock is not valid while the device is recording",
            ));
        }
        let mut block = self.blocks.remove(&handle).ok_or_else(|| {
            invalid(
                "CaptureStateBlock",
                format!("state block {handle} does not exist"),
            )
        })?;
        block.capture_from(self);
        self.blocks.insert(handle, block);
        Ok(())
    }

    pub fn apply_block(&mut self, handle: u32) -> Result<(), RenderError> {
        let block = self.blocks.remove(&handle).ok_or_else(|| {
            invalid(
                "ApplyStateBlock",
                format!("state block {handle} does not exist"),
            )
        })?;
        if self.recording.is_some() {
            self.blocks.insert(handle, block);
            return Err(invalid(
                "ApplyStateBlock",
                "ApplyStateBlock is not valid while the device is recording",
            ));
        }
        block.apply_to(self);
        self.blocks.insert(handle, block);
        Ok(())
    }

    pub fn block_bindings(&self, handle: u32) -> Result<D3d9Bindings, RenderError> {
        self.blocks
            .get(&handle)
            .map(D3d9StateBlock::bindings)
            .ok_or_else(|| {
                invalid(
                    "StateBlockBindings",
                    format!("state block {handle} does not exist"),
                )
            })
    }

    pub fn destroy_block(&mut self, handle: u32) -> Result<(), RenderError> {
        if self.blocks.remove(&handle).is_none() {
            return Err(invalid(
                "DeleteStateBlock",
                format!("state block {handle} does not exist"),
            ));
        }
        Ok(())
    }

    pub fn recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn live_block_count(&self) -> usize {
        self.blocks.len() + usize::from(self.recording.is_some())
    }

    pub fn clear_pipeline(&mut self) {
        let target_viewport = self.target_viewport;
        let blocks = std::mem::take(&mut self.blocks);
        let next_block = self.next_block;
        let recording_handle = self.recording_handle;
        drop(blocks);
        *self = Self::default();
        self.target_viewport = target_viewport;
        self.next_block = next_block;
        self.recording_handle = recording_handle;
    }

    pub fn has_live_state_blocks(&self) -> bool {
        !self.blocks.is_empty() || self.recording.is_some()
    }

    pub fn clear_up_stream(&mut self) {
        self.put_stream(0, StreamSource::default());
    }

    pub fn clear_up_indexed(&mut self) {
        self.put_stream(0, StreamSource::default());
        self.indices = 0;
    }

    pub fn reset_viewport(&mut self) {
        self.viewport = None;
    }

    pub fn enabled_lights(&self) -> Vec<[u32; 26]> {
        let mut out = Vec::new();
        for (&index, light) in &self.lights {
            if self.light_enabled.get(&index).copied().unwrap_or(false) {
                out.push(*light);
            }
        }
        out
    }

    pub(crate) fn effect_capture(&self, keys: &EffectKeys) -> Result<EffectSnapshot, RenderError> {
        let mut block = D3d9StateBlock::new(D3DSBT_ALL);
        for &state in &keys.render {
            if render_state_spec(state).is_none() {
                return Err(invalid(
                    "Begin",
                    format!("render state {state} is not supported for effect capture"),
                ));
            }
            block.set_render(state, 0);
        }
        for &(stage, state) in &keys.tss {
            if stage as usize >= MAX_TEXTURE_STAGES || texture_stage_state_spec(state).is_none() {
                return Err(invalid(
                    "Begin",
                    format!(
                        "texture stage state {stage}/{state} is out of range for effect capture"
                    ),
                ));
            }
            block.set_tss(stage, state, 0);
        }
        for &(sampler, state) in &keys.sampler {
            if sampler as usize >= MAX_SAMPLERS || sampler_state_spec(state).is_none() {
                return Err(invalid(
                    "Begin",
                    format!("sampler state {sampler}/{state} is out of range for effect capture"),
                ));
            }
            block.set_sampler(sampler, state, 0);
        }
        for &stage in &keys.textures {
            if stage as usize >= MAX_SAMPLERS {
                return Err(invalid(
                    "Begin",
                    format!("texture stage {stage} is out of range for effect capture"),
                ));
            }
            block.set_texture(stage, 0);
        }
        for &index in &keys.transform {
            if transform_index(index).is_none() {
                return Err(invalid(
                    "Begin",
                    format!("transform {index} is out of range for effect capture"),
                ));
            }
            if !block.transform.iter().any(|entry| entry.0 == index) {
                block.transform.push((index, [0.0; 16]));
            }
        }
        if keys.vertex_shader {
            block.vertex_shader = Some(0);
        }
        if keys.pixel_shader {
            block.pixel_shader = Some(0);
        }
        for &(start, count) in &keys.vs_const_f {
            if start as usize + count as usize > VS_CONST_F {
                return Err(invalid(
                    "Begin",
                    "vertex shader constant range is out of bounds for effect capture",
                ));
            }
            for index in start..start + count {
                if !block.vs_const_f.iter().any(|entry| entry.0 == index) {
                    block.vs_const_f.push((index, [0.0; 4]));
                }
            }
        }
        for &(start, count) in &keys.ps_const_f {
            if start as usize + count as usize > PS_CONST_F {
                return Err(invalid(
                    "Begin",
                    "pixel shader constant range is out of bounds for effect capture",
                ));
            }
            for index in start..start + count {
                if !block.ps_const_f.iter().any(|entry| entry.0 == index) {
                    block.ps_const_f.push((index, [0.0; 4]));
                }
            }
        }
        block.capture_from(self);
        Ok(EffectSnapshot { block })
    }
}

#[derive(Default)]
pub(crate) struct EffectKeys {
    pub render: Vec<u32>,
    pub tss: Vec<(u32, u32)>,
    pub sampler: Vec<(u32, u32)>,
    pub transform: Vec<u32>,
    pub textures: Vec<u32>,
    pub vertex_shader: bool,
    pub pixel_shader: bool,
    pub vs_const_f: Vec<(u32, u32)>,
    pub ps_const_f: Vec<(u32, u32)>,
}

pub(crate) struct EffectSnapshot {
    block: D3d9StateBlock,
}

impl EffectSnapshot {
    pub(crate) fn restore(&self, state: &mut D3d9State) {
        self.block.apply_to(state);
    }

    pub(crate) fn bindings(&self) -> D3d9Bindings {
        self.block.bindings()
    }
}

const FP_TSS: u64 = 256;
const FP_SAMPLER: u64 = 1024;
const FP_TEXTURE: u64 = 2048;
const FP_STRIDE: u64 = 2112;
const FP_FREQ: u64 = 2176;
const FP_DECLARATION: u64 = 2240;
const FP_FVF: u64 = 2241;
const FP_VERTEX_SHADER: u64 = 2242;
const FP_PIXEL_SHADER: u64 = 2243;

fn fingerprint_word(slot: u64, value: u32) -> u64 {
    let mut z = (slot << 32 | u64::from(value)).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn transform_index(state: u32) -> Option<u32> {
    match state {
        2 | 3 => Some(state),
        16..=23 => Some(state),
        256..=511 => Some(state),
        _ => None,
    }
}

fn default_light() -> [u32; 26] {
    let mut light = [0u32; 26];
    light[0] = 3;
    light[1] = 1.0f32.to_bits();
    light[2] = 1.0f32.to_bits();
    light[3] = 1.0f32.to_bits();
    light[18] = 1.0f32.to_bits();
    light
}

fn multiply_matrices(a: [f32; 16], b: [f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for row in 0..4 {
        for col in 0..4 {
            out[row * 4 + col] = (0..4).map(|k| a[row * 4 + k] * b[k * 4 + col]).sum();
        }
    }
    out
}
