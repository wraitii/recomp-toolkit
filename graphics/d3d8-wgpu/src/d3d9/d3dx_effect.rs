mod preshader;

use crate::RenderError;
use crate::bridge::{
    D3d9BindingsResult, D3dxBytesResult, D3dxEffectCommand, D3dxEffectCommandKind,
    D3dxEffectCreateResult, D3dxEffectDesc, D3dxEffectDescResult, D3dxEffectParamDesc,
    D3dxEffectParamDescResult, D3dxEffectPassDesc, D3dxEffectPassDescResult, D3dxEffectRunResult,
    D3dxEffectShadersResult, D3dxEffectTechniqueDesc, D3dxEffectTechniqueDescResult, Status,
    StatusCode, ValueResult, status,
};
use crate::d3d9::D3d9Device;
use crate::d3d9::d3dx_shader;
use crate::d3d9::resource::D3DPOOL_DEFAULT;
use crate::d3d9::state::{EffectKeys, EffectSnapshot};
use crate::d3d9::stats;
use crate::fast_map::FastMap;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const FX_TAG: u32 = 0xFEFF_0901;

const FXC_SCALAR: u32 = 0;
const FXC_VECTOR: u32 = 1;
const FXC_MATRIX_ROWS: u32 = 2;
const FXC_MATRIX_COLUMNS: u32 = 3;
const FXC_OBJECT: u32 = 4;
const FXC_STRUCT: u32 = 5;

const FXT_BOOL: u32 = 1;
const FXT_INT: u32 = 2;
const FXT_FLOAT: u32 = 3;
const FXT_STRING: u32 = 4;
const FXT_TEXTURE: u32 = 5;
const FXT_TEXTURECUBE: u32 = 9;
const FXT_SAMPLER: u32 = 10;
const FXT_SAMPLER_LAST: u32 = 14;

const D3DXRS_BOOL: u32 = 0;
const D3DXRS_INT4: u32 = 1;
const D3DXRS_FLOAT4: u32 = 2;
const D3DXRS_SAMPLER: u32 = 3;

const H_PARAM: u32 = 0x0100_0000;
const H_TECH: u32 = 0x0200_0000;
const H_PASS: u32 = 0x0300_0000;
const H_ANNOT: u32 = 0x0400_0000;
const H_CHILD: u32 = 0x0500_0000;
const H_MASK: u32 = 0xff00_0000;

const K_PARAM: u32 = 0;
const K_TECH: u32 = 1;
const K_PASS: u32 = 2;
const K_ANNOT: u32 = 3;
const K_ELEM: u32 = 4;

const FXS_FVF: u32 = 122;
const FXS_VERTEXSHADER: u32 = 146;
const FXS_PIXELSHADER: u32 = 147;
const FXS_SAMPLER_TEXTURE: u32 = 164;
const FXS_SAMPLER_FIRST: u32 = 165;
const FXS_SAMPLER_LAST: u32 = 177;
const FXS_SETSAMPLER: u32 = 178;

const D3DTS_TEXTURE0: u32 = 16;

const FX_PARAM_SHARED: u32 = 1;

const RENDER_STATES: [u32; 103] = [
    7, 8, 9, 14, 15, 16, 19, 20, 22, 23, 24, 25, 26, 27, 28, 29, 34, 35, 36, 37, 38, 48, 52, 53,
    54, 55, 56, 57, 58, 59, 60, 128, 129, 130, 131, 132, 133, 134, 135, 198, 199, 200, 201, 202,
    203, 204, 205, 136, 137, 139, 140, 141, 142, 143, 145, 146, 147, 148, 151, 152, 154, 155, 166,
    156, 157, 158, 159, 160, 161, 162, 163, 165, 167, 168, 170, 171, 172, 173, 174, 175, 176, 178,
    179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 206, 207,
    208, 209,
];

const STAGE_STATES: [u32; 18] = [
    1, 26, 2, 3, 4, 27, 5, 6, 28, 7, 8, 9, 10, 11, 22, 23, 24, 32,
];

const SET_VALUE: u32 = 0;
const SET_BOOL: u32 = 1;
const SET_BOOL_ARRAY: u32 = 2;
const SET_INT: u32 = 3;
const SET_INT_ARRAY: u32 = 4;
const SET_FLOAT: u32 = 5;
const SET_FLOAT_ARRAY: u32 = 6;
const SET_VECTOR: u32 = 7;
const SET_VECTOR_ARRAY: u32 = 8;
const SET_MATRIX: u32 = 9;
const SET_MATRIX_ARRAY: u32 = 10;
const SET_MATRIX_POINTER_ARRAY: u32 = 11;
const SET_MATRIX_TRANSPOSE: u32 = 12;
const SET_MATRIX_TRANSPOSE_ARRAY: u32 = 13;
const SET_MATRIX_TRANSPOSE_POINTER_ARRAY: u32 = 14;
const SET_LAST: u32 = SET_MATRIX_TRANSPOSE_POINTER_ARRAY;

const READ_FLOAT: u32 = 0;
const READ_INT: u32 = 1;
const READ_BOOL: u32 = 2;
const READ_VALUE: u32 = 3;
const READ_FLOAT_ARRAY: u32 = 4;
const READ_INT_ARRAY: u32 = 5;
const READ_BOOL_ARRAY: u32 = 6;
const READ_VECTOR: u32 = 7;
const READ_VECTOR_ARRAY: u32 = 8;
const READ_MATRIX: u32 = 9;
const READ_MATRIX_ARRAY: u32 = 10;
const READ_MATRIX_TRANSPOSE: u32 = 11;
const READ_MATRIX_TRANSPOSE_ARRAY: u32 = 12;
const READ_STRING: u32 = 13;
const READ_TEXTURE: u32 = 14;

const MAX_NESTING: u32 = 32;
const MAX_NODES: usize = 4096;
const MAX_SAMPLER_STATES: u32 = 256;

static NEXT_EFFECT: AtomicU32 = AtomicU32::new(1);
static NEXT_POOL: AtomicU32 = AtomicU32::new(1);

fn invalid_data(message: impl Into<String>) -> RenderError {
    RenderError::invalid_data("D3DXEffect", message)
}

fn unsupported(operation: &str, message: impl Into<String>) -> RenderError {
    RenderError::new("D3DXEffect", format!("{operation}: {}", message.into()))
}

fn failed(operation: &str, error: RenderError) -> Status {
    let code = match error.kind {
        crate::RenderErrorKind::InvalidArgument => StatusCode::InvalidArgument,
        crate::RenderErrorKind::InvalidData => StatusCode::InvalidData,
        crate::RenderErrorKind::OutOfMemory => StatusCode::OutOfMemory,
        crate::RenderErrorKind::Unsupported => StatusCode::Unsupported,
    };
    status(code, format!("{operation}: {}", error.cause))
}

fn ok() -> Status {
    status(StatusCode::Ok, "")
}

fn invalid_call(operation: &str) -> Status {
    status(StatusCode::InvalidArgument, operation.to_string())
}

fn rd_u32(data: &[u8], offset: usize) -> Result<u32, RenderError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| invalid_data("u32 offset overflow"))?;
    if end > data.len() {
        return Err(invalid_data("u32 read past effect end"));
    }
    Ok(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

fn texture_type(t: u32) -> bool {
    (FXT_TEXTURE..=FXT_TEXTURECUBE).contains(&t)
}

#[derive(Clone)]
struct EfxType {
    type_: u32,
    class: u32,
    rows: u32,
    cols: u32,
    elements: u32,
    name: Vec<u8>,
    semantic: Vec<u8>,
    members: Vec<EfxType>,
    bytes: u32,
}

impl EfxType {
    fn object_type(&self) -> bool {
        self.class == FXC_OBJECT
    }
    fn sampler(&self) -> bool {
        self.class == FXC_OBJECT && (FXT_SAMPLER..=FXT_SAMPLER_LAST).contains(&self.type_)
    }
    fn texture(&self) -> bool {
        self.class == FXC_OBJECT && texture_type(self.type_)
    }
    fn count(&self) -> u32 {
        if self.elements == 0 { 1 } else { self.elements }
    }
}

#[derive(Clone)]
enum EfxValue {
    Numeric(Vec<u8>),
    Object(Vec<u32>),
    Sampler(Vec<EfxState>),
}

#[derive(Clone)]
struct EfxState {
    op: u32,
    index: u32,
    type_: EfxType,
    value: EfxValue,
    shader: Option<Vec<u8>>,
    preshader: Option<Vec<u8>>,
    referenced: Option<Vec<u8>>,
    selector: Option<Vec<u8>>,
    preshader_program: OnceLock<Result<Arc<CompiledPreshader>, RenderError>>,
    selector_program: OnceLock<Result<Arc<CompiledPreshader>, RenderError>>,
    deps: OnceLock<Option<Vec<usize>>>,
}

struct CompiledPreshader {
    program: preshader::Preshader,
    constants: Vec<CtabConst>,
    params: Vec<usize>,
    outputs: Vec<u32>,
}

static NEXT_VERSION: AtomicU64 = AtomicU64::new(1);

fn next_version() -> u64 {
    NEXT_VERSION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
struct EfxParam {
    type_: EfxType,
    value: EfxValue,
    flags: u32,
    annotations: Vec<u32>,
    version: u64,
    pool_slot: Option<usize>,
}

#[derive(Clone)]
struct EfxPass {
    name: Vec<u8>,
    states: Arc<Vec<EfxState>>,
    annotations: Vec<u32>,
}

#[derive(Clone)]
struct EfxTechnique {
    name: Vec<u8>,
    passes: Vec<EfxPass>,
    annotations: Vec<u32>,
}

#[derive(Clone)]
struct EfxObject {
    data: Vec<u8>,
    param: Vec<u8>,
    ctab: OnceLock<Result<Vec<CtabConst>, RenderError>>,
    saved_outputs: OnceLock<Result<Vec<u32>, RenderError>>,
    ctab_params: OnceLock<Result<Vec<usize>, RenderError>>,
    preshader: OnceLock<Result<Option<Arc<CompiledPreshader>>, RenderError>>,
    const_version: Cell<u64>,
}

struct ParamShape {
    name: Vec<u8>,
    type_: u32,
    class: u32,
    rows: u32,
    cols: u32,
    elements: u32,
    members: Vec<ParamShape>,
}

impl ParamShape {
    fn of(t: &EfxType) -> Self {
        Self {
            name: t.name.clone(),
            type_: t.type_,
            class: t.class,
            rows: t.rows,
            cols: t.cols,
            elements: t.elements,
            members: t.members.iter().map(ParamShape::of).collect(),
        }
    }

    fn matches(&self, t: &EfxType) -> bool {
        self.name == t.name
            && self.type_ == t.type_
            && self.class == t.class
            && self.rows == t.rows
            && self.cols == t.cols
            && self.elements == t.elements
            && self.members.len() == t.members.len()
            && self
                .members
                .iter()
                .zip(&t.members)
                .all(|(a, b)| a.matches(b))
    }
}

struct PoolEntry {
    shape: ParamShape,
    value: Vec<u8>,
    objects: Vec<u32>,
    members: BTreeSet<u32>,
    version: u64,
}

// Parameters sharing a name but not a shape get separate pool entries.
#[derive(Default)]
struct PoolState {
    device_id: Option<u32>,
    entries: Vec<Option<PoolEntry>>,
    names: BTreeMap<Vec<u8>, Vec<usize>>,
    free: Vec<usize>,
}

impl PoolState {
    fn slot(&self, slot: usize) -> Option<&PoolEntry> {
        self.entries.get(slot)?.as_ref()
    }

    fn slot_mut(&mut self, slot: usize) -> Option<&mut PoolEntry> {
        self.entries.get_mut(slot)?.as_mut()
    }

    fn find_slot(&self, t: &EfxType) -> Option<usize> {
        self.names
            .get(&t.name)?
            .iter()
            .copied()
            .find(|&slot| self.slot(slot).is_some_and(|entry| entry.shape.matches(t)))
    }

    fn insert(
        &mut self,
        t: &EfxType,
        value: Vec<u8>,
        objects: Vec<u32>,
        member: u32,
        version: u64,
    ) -> usize {
        let entry = PoolEntry {
            shape: ParamShape::of(t),
            value,
            objects,
            members: BTreeSet::from([member]),
            version,
        };
        let slot = if let Some(slot) = self.free.pop() {
            self.entries[slot] = Some(entry);
            slot
        } else {
            self.entries.push(Some(entry));
            self.entries.len() - 1
        };
        self.names.entry(t.name.clone()).or_default().push(slot);
        slot
    }

    fn remove_member(&mut self, id: u32) {
        for slot in 0..self.entries.len() {
            let Some(entry) = self.entries[slot].as_mut() else {
                continue;
            };
            entry.members.remove(&id);
            if !entry.members.is_empty() {
                continue;
            }
            let name = entry.shape.name.clone();
            self.entries[slot] = None;
            self.free.push(slot);
            if let Some(slots) = self.names.get_mut(&name) {
                slots.retain(|&candidate| candidate != slot);
                if slots.is_empty() {
                    self.names.remove(&name);
                }
            }
        }
    }

    fn values(&self) -> impl Iterator<Item = &PoolEntry> {
        self.entries.iter().flatten()
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut PoolEntry> {
        self.entries.iter_mut().flatten()
    }
}

pub struct D3dxEffectPool {
    id: u32,
    state: Arc<Mutex<PoolState>>,
}

#[derive(Clone, Copy)]
struct ChildLink {
    parent: u32,
    step: u32,
}

#[derive(Default)]
struct ChildState {
    next: u32,
    links: BTreeMap<u32, ChildLink>,
    cache: BTreeMap<(u32, u32), u32>,
}

#[derive(Clone, Copy)]
enum EffectBatch {
    BeginPass(u32),
    Commit,
    ManagerChange,
}

pub struct D3dxEffect {
    id: u32,
    device_id: u32,
    params: Vec<EfxParam>,
    techniques: Vec<EfxTechnique>,
    annotations: Vec<EfxParam>,
    objects: BTreeMap<u32, EfxObject>,
    shaders: BTreeMap<u32, (bool, u32)>,
    pool: Option<Arc<Mutex<PoolState>>>,
    pool_id: u32,
    technique: u32,
    pass: i32,
    started: bool,
    begin_flags: u32,
    creation_flags: u32,
    saved: Option<EffectSnapshot>,
    lost: bool,
    manager_mode: bool,
    batch: Option<EffectBatch>,
    children: RefCell<ChildState>,
    param_names: FastMap<Vec<u8>, usize>,
    pass_version: u64,
    spare_commands: Vec<D3dxEffectCommand>,
    spare_values: Vec<f32>,
    preshader_inputs: RefCell<Vec<[f32; 4]>>,
    targets: RefCell<FastMap<u32, ParamTarget>>,
}

impl Default for D3dxEffect {
    fn default() -> Self {
        Self {
            id: NEXT_EFFECT.fetch_add(1, Ordering::Relaxed),
            device_id: 0,
            params: Vec::new(),
            techniques: Vec::new(),
            annotations: Vec::new(),
            objects: BTreeMap::new(),
            shaders: BTreeMap::new(),
            pool: None,
            pool_id: 0,
            technique: u32::MAX,
            pass: -1,
            started: false,
            begin_flags: 0,
            creation_flags: 0,
            saved: None,
            lost: false,
            manager_mode: false,
            batch: None,
            children: RefCell::new(ChildState::default()),
            param_names: Default::default(),
            pass_version: 0,
            spare_commands: Vec::new(),
            spare_values: Vec::new(),
            preshader_inputs: RefCell::default(),
            targets: RefCell::default(),
        }
    }
}

pub fn new_d3dx_effect() -> Box<D3dxEffect> {
    Box::new(D3dxEffect::default())
}

pub fn new_d3dx_effect_pool() -> Box<D3dxEffectPool> {
    Box::new(D3dxEffectPool {
        id: NEXT_POOL.fetch_add(1, Ordering::Relaxed),
        state: Arc::new(Mutex::new(PoolState::default())),
    })
}

fn type_bytes(t: &EfxType) -> Result<u32, RenderError> {
    Ok(match t.class {
        FXC_SCALAR | FXC_VECTOR | FXC_MATRIX_ROWS | FXC_MATRIX_COLUMNS => 4u32
            .checked_mul(t.rows)
            .and_then(|v| v.checked_mul(t.cols))
            .and_then(|v| v.checked_mul(t.count()))
            .ok_or_else(|| invalid_data("parameter byte size overflow"))?,
        FXC_STRUCT => {
            let mut total = 0u32;
            for member in &t.members {
                total = total
                    .checked_add(member.bytes)
                    .ok_or_else(|| invalid_data("struct byte size overflow"))?;
            }
            total
                .checked_mul(t.count())
                .ok_or_else(|| invalid_data("struct byte size overflow"))?
        }
        FXC_OBJECT => 4u32
            .checked_mul(t.count())
            .ok_or_else(|| invalid_data("object byte size overflow"))?,
        _ => return Err(invalid_data("unknown parameter class")),
    })
}

fn read_name(body: &[u8], offset: u32) -> Result<Vec<u8>, RenderError> {
    if offset == 0 {
        return Ok(Vec::new());
    }
    let size = rd_u32(body, offset as usize)? as usize;
    let start = offset as usize + 4;
    let end = start
        .checked_add(size)
        .ok_or_else(|| invalid_data("name size overflow"))?;
    if end > body.len() {
        return Err(invalid_data("name string runs past the effect"));
    }
    Ok(trim_nul(&body[start..end]))
}

fn trim_nul(data: &[u8]) -> Vec<u8> {
    let nul = data.iter().position(|b| *b == 0).unwrap_or(data.len());
    data[..nul].to_vec()
}

fn read_type(body: &[u8], offset: u32, depth: u32) -> Result<EfxType, RenderError> {
    if depth > MAX_NESTING {
        return Err(unsupported("Parse", "parameter type nesting is too deep"));
    }
    let base = offset as usize;
    let mut t = EfxType {
        type_: rd_u32(body, base)?,
        class: rd_u32(body, base + 4)?,
        rows: 0,
        cols: 0,
        elements: 0,
        name: read_name(body, rd_u32(body, base + 8)?)?,
        semantic: read_name(body, rd_u32(body, base + 12)?)?,
        members: Vec::new(),
        bytes: 0,
    };
    if !(1..=16).contains(&t.type_) {
        return Err(invalid_data("parameter type is not a D3DXPT value"));
    }
    t.elements = rd_u32(body, base + 16)?;
    match t.class {
        FXC_SCALAR | FXC_VECTOR | FXC_MATRIX_ROWS | FXC_MATRIX_COLUMNS => {
            let a = rd_u32(body, base + 20)?;
            let b = rd_u32(body, base + 24)?;
            if t.class == FXC_VECTOR {
                t.cols = a;
                t.rows = b;
            } else {
                t.rows = a;
                t.cols = b;
            }
            if t.rows > 4 || t.cols > 4 || t.rows == 0 || t.cols == 0 {
                return Err(invalid_data("parameter dimensions exceed four"));
            }
        }
        FXC_OBJECT => {
            if !(4..=16).contains(&t.type_) {
                return Err(unsupported("Parse", "object parameter type is unsupported"));
            }
        }
        FXC_STRUCT => {
            return Err(unsupported(
                "Parse",
                "struct effect parameters are unsupported",
            ));
        }
        _ => return Err(invalid_data("parameter class is not a D3DXPC value")),
    }
    t.bytes = type_bytes(&t)?;
    Ok(t)
}

fn read_value(
    body: &[u8],
    t: &EfxType,
    offset: u32,
    sampler: bool,
) -> Result<EfxValue, RenderError> {
    if t.class == FXC_OBJECT && t.sampler() && sampler {
        let count = rd_u32(body, offset as usize)?;
        if count > MAX_SAMPLER_STATES {
            return Err(unsupported("Parse", "sampler state count is unreasonable"));
        }
        let mut states = Vec::new();
        let mut at = offset as usize + 4;
        for _ in 0..count {
            let (state, next) = read_state(body, at)?;
            states.push(state);
            at = next;
        }
        return Ok(EfxValue::Sampler(states));
    }
    let start = offset as usize;
    if t.object_type() {
        let count = t.count() as usize;
        let mut ids = Vec::with_capacity(count);
        for i in 0..count {
            ids.push(rd_u32(body, start + 4 * i)?);
        }
        return Ok(EfxValue::Object(ids));
    }
    let end = start
        .checked_add(t.bytes as usize)
        .ok_or_else(|| invalid_data("value size overflow"))?;
    if end > body.len() {
        return Err(invalid_data("value runs past the effect"));
    }
    Ok(EfxValue::Numeric(body[start..end].to_vec()))
}

fn read_state(body: &[u8], at: usize) -> Result<(EfxState, usize), RenderError> {
    let op = rd_u32(body, at)?;
    let index = rd_u32(body, at + 4)?;
    let type_offset = rd_u32(body, at + 8)?;
    let value_offset = rd_u32(body, at + 12)?;
    let type_ = read_type(body, type_offset, 0)?;
    let value = read_value(body, &type_, value_offset, false)?;
    Ok((
        EfxState {
            op,
            index,
            type_,
            value,
            shader: None,
            preshader: None,
            referenced: None,
            selector: None,
            preshader_program: OnceLock::new(),
            selector_program: OnceLock::new(),
            deps: OnceLock::new(),
        },
        at + 16,
    ))
}

fn read_annotations(
    body: &[u8],
    at: &mut usize,
    count: u32,
    annotations: &mut Vec<EfxParam>,
    out: &mut Vec<u32>,
) -> Result<(), RenderError> {
    if count as usize > MAX_NODES {
        return Err(unsupported("Parse", "annotation count is unreasonable"));
    }
    for _ in 0..count {
        let type_offset = rd_u32(body, *at)?;
        let value_offset = rd_u32(body, *at + 4)?;
        *at += 8;
        let type_ = read_type(body, type_offset, 0)?;
        let value = read_value(body, &type_, value_offset, false)?;
        out.push(annotations.len() as u32);
        annotations.push(EfxParam {
            type_,
            value,
            flags: 0,
            annotations: Vec::new(),
            version: 0,
            pool_slot: None,
        });
    }
    Ok(())
}

fn parse_effect(blob: &[u8]) -> Result<D3dxEffect, RenderError> {
    if blob.len() < 8 {
        return Err(invalid_data("effect is shorter than its header"));
    }
    if rd_u32(blob, 0)? != FX_TAG {
        return Err(unsupported("Parse", "effect is not a compiled fx_2_0 blob"));
    }
    let start = rd_u32(blob, 4)? as usize;
    let body = &blob[8..];
    let mut at = start;
    let nparams = rd_u32(body, at)?;
    let ntech = rd_u32(body, at + 4)?;
    let _nshader = rd_u32(body, at + 8)?;
    let _nobjects = rd_u32(body, at + 12)?;
    at += 16;
    if nparams as usize > MAX_NODES || ntech as usize > MAX_NODES {
        return Err(unsupported(
            "Parse",
            "effect header counts are unreasonable",
        ));
    }
    let mut effect = D3dxEffect::default();
    effect.technique = if ntech > 0 { 0 } else { u32::MAX };
    for _ in 0..nparams {
        let type_offset = rd_u32(body, at)?;
        let value_offset = rd_u32(body, at + 4)?;
        let flags = rd_u32(body, at + 8)?;
        let annot_count = rd_u32(body, at + 12)?;
        at += 16;
        let mut param_annotations = Vec::new();
        read_annotations(
            body,
            &mut at,
            annot_count,
            &mut effect.annotations,
            &mut param_annotations,
        )?;
        let type_ = read_type(body, type_offset, 0)?;
        let value = read_value(body, &type_, value_offset, true)?;
        effect.params.push(EfxParam {
            type_,
            value,
            flags,
            annotations: param_annotations,
            version: 0,
            pool_slot: None,
        });
    }
    for _ in 0..ntech {
        let name = read_name(body, rd_u32(body, at)?)?;
        let nann = rd_u32(body, at + 4)?;
        let npass = rd_u32(body, at + 8)?;
        at += 12;
        let mut tech_annotations = Vec::new();
        read_annotations(
            body,
            &mut at,
            nann,
            &mut effect.annotations,
            &mut tech_annotations,
        )?;
        let mut passes = Vec::new();
        for _ in 0..npass {
            let pname = read_name(body, rd_u32(body, at)?)?;
            let pann = rd_u32(body, at + 4)?;
            let nstate = rd_u32(body, at + 8)?;
            at += 12;
            let mut pass_annotations = Vec::new();
            read_annotations(
                body,
                &mut at,
                pann,
                &mut effect.annotations,
                &mut pass_annotations,
            )?;
            let mut states = Vec::new();
            for _ in 0..nstate {
                let (state, next) = read_state(body, at)?;
                states.push(state);
                at = next;
            }
            passes.push(EfxPass {
                name: pname,
                states: Arc::new(states),
                annotations: pass_annotations,
            });
        }
        effect.techniques.push(EfxTechnique {
            name,
            passes,
            annotations: tech_annotations,
        });
    }
    let nstrings = rd_u32(body, at)?;
    let nresources = rd_u32(body, at + 4)?;
    at += 8;
    if nstrings as usize > MAX_NODES || nresources as usize > MAX_NODES {
        return Err(unsupported(
            "Parse",
            "string or resource count is unreasonable",
        ));
    }
    for _ in 0..nstrings {
        let id = rd_u32(body, at)?;
        let size = rd_u32(body, at + 4)? as usize;
        at += 8;
        let end = at
            .checked_add(size)
            .ok_or_else(|| invalid_data("string size overflow"))?;
        if end > body.len() {
            return Err(invalid_data("string data runs past the effect"));
        }
        effect.objects.insert(
            id,
            EfxObject {
                data: body[at..end].to_vec(),
                param: Vec::new(),
                ctab: OnceLock::new(),
                saved_outputs: OnceLock::new(),
                ctab_params: OnceLock::new(),
                preshader: OnceLock::new(),
                const_version: Cell::new(0),
            },
        );
        at = end + ((4 - (size & 3)) & 3);
    }
    for _ in 0..nresources {
        let technique = rd_u32(body, at)?;
        let index = rd_u32(body, at + 4)?;
        let element = rd_u32(body, at + 8)?;
        let state_index = rd_u32(body, at + 12)?;
        let usage = rd_u32(body, at + 16)?;
        let size = rd_u32(body, at + 20)? as usize;
        at += 24;
        let end = at
            .checked_add(size)
            .ok_or_else(|| invalid_data("resource size overflow"))?;
        if end > body.len() {
            return Err(invalid_data("resource data runs past the effect"));
        }
        let data = body[at..end].to_vec();
        at = end + ((4 - (size & 3)) & 3);
        let state: &mut EfxState = if technique == u32::MAX {
            let Some(param) = effect.params.get_mut(index as usize) else {
                return Err(invalid_data(
                    "resource names a parameter that does not exist",
                ));
            };
            if element != 0xffff_ffff {
                if param.type_.elements > 1 {
                    return Err(unsupported(
                        "Parse",
                        "sampler parameter arrays are unsupported",
                    ));
                }
                if element >= param.type_.elements.max(1) {
                    return Err(invalid_data(
                        "resource element index exceeds the parameter element count",
                    ));
                }
            }
            let EfxValue::Sampler(states) = &mut param.value else {
                return Err(invalid_data("resource names a non-sampler parameter"));
            };
            let Some(state) = states.get_mut(state_index as usize) else {
                return Err(invalid_data(
                    "resource names a sampler state that does not exist",
                ));
            };
            state
        } else {
            let Some(technique) = effect.techniques.get_mut(technique as usize) else {
                return Err(invalid_data(
                    "resource names a technique that does not exist",
                ));
            };
            let Some(pass) = technique.passes.get_mut(index as usize) else {
                return Err(invalid_data("resource names a pass that does not exist"));
            };
            let Some(state) = Arc::make_mut(&mut pass.states).get_mut(state_index as usize) else {
                return Err(invalid_data(
                    "resource names a pass state that does not exist",
                ));
            };
            state
        };
        if usage == 0 && state.type_.class != FXC_OBJECT {
            state.preshader = Some(data);
            continue;
        }
        let id = match &state.value {
            EfxValue::Object(ids) => {
                if element != 0xffff_ffff && element as usize >= ids.len() {
                    return Err(invalid_data(
                        "resource element index exceeds the object array",
                    ));
                }
                if element == 0xffff_ffff {
                    if ids.is_empty() {
                        return Err(invalid_data("object state has no object id"));
                    }
                    ids[0]
                } else {
                    ids[element as usize]
                }
            }
            _ => return Err(invalid_data("object state has a non-object value")),
        };
        match usage {
            0 => state.shader = Some(data),
            1 => state.referenced = Some(trim_nul(&data)),
            2 => {
                if data.len() < 4 {
                    return Err(invalid_data(
                        "array selector is shorter than its name field",
                    ));
                }
                let string_size = rd_u32(&data, 0)? as usize;
                if 4 + string_size > data.len() {
                    return Err(invalid_data("array selector name runs past the blob"));
                }
                state.referenced = Some(trim_nul(&data[4..4 + string_size]));
                state.selector = Some(data);
            }
            _ => return Err(unsupported("Parse", "resource usage is unsupported")),
        }
        if state.op == FXS_SETSAMPLER {
            continue;
        }
        let entry = effect.objects.entry(id).or_insert(EfxObject {
            data: Vec::new(),
            param: Vec::new(),
            ctab: OnceLock::new(),
            saved_outputs: OnceLock::new(),
            ctab_params: OnceLock::new(),
            preshader: OnceLock::new(),
            const_version: Cell::new(0),
        });
        match usage {
            0 => entry.data = state.shader.clone().unwrap_or_default(),
            1 | 2 => entry.param = state.referenced.clone().unwrap_or_default(),
            _ => {}
        }
    }
    for param in &mut effect.params {
        if !param.type_.texture() {
            continue;
        }
        if let EfxValue::Object(ids) = &mut param.value {
            for id in ids.iter_mut() {
                *id = 0;
            }
        }
    }
    for (index, param) in effect.params.iter().enumerate() {
        effect
            .param_names
            .entry(param.type_.name.clone())
            .or_insert(index);
    }
    Ok(effect)
}

#[derive(Clone)]
struct CtabConst {
    name: Vec<u8>,
    set: u32,
    index: u32,
    count: u32,
    class: u32,
    rows: u32,
    cols: u32,
    elements: u32,
}

fn parse_ctab(code: &[u8]) -> Result<Vec<CtabConst>, RenderError> {
    stats::record(|s| {
        s.ctab_parses += 1;
        s.ctab_input_bytes += code.len() as u64;
    });
    let constants = d3dx_shader::ctab_constants(code)?;
    Ok(constants
        .into_iter()
        .map(|constant| CtabConst {
            name: constant.name,
            set: constant.register_set,
            index: constant.register_index,
            count: constant.register_count,
            class: constant.class_,
            rows: constant.rows,
            cols: constant.columns,
            elements: constant.elements.max(1),
        })
        .collect())
}

fn object_ctab(object: &EfxObject, saved: bool) -> Result<&[CtabConst], RenderError> {
    object
        .ctab
        .get_or_init(|| {
            if saved {
                stats::record(|s| s.saved_ctab_parses += 1);
            }
            parse_ctab(&object.data)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

fn handle_index(handle: u32, tag: u32) -> Option<u32> {
    if handle & H_MASK == tag {
        Some(handle & 0x00ff_ffff)
    } else {
        None
    }
}

fn pass_handle(tech: u32, pass: u32) -> u32 {
    H_PASS | ((tech & 0xfff) << 12) | (pass & 0xfff)
}

fn pass_parts(handle: u32) -> Option<(u32, u32)> {
    if handle & H_MASK != H_PASS {
        return None;
    }
    let body = handle & 0x00ff_ffff;
    Some((body >> 12, body & 0xfff))
}

fn convert_word(input: u32, word: u32, target: u32) -> u32 {
    match target {
        FXT_BOOL => {
            let truth = match input {
                FXT_BOOL | FXT_INT => word != 0,
                FXT_FLOAT => f32::from_bits(word) != 0.0,
                _ => word != 0,
            };
            truth as u32
        }
        FXT_INT => match input {
            FXT_BOOL => (word != 0) as u32,
            FXT_FLOAT => (f32::from_bits(word) as i32) as u32,
            _ => word,
        },
        _ => {
            let value = match input {
                FXT_BOOL => (word != 0) as u32 as f32,
                FXT_INT => (word as i32) as f32,
                _ => f32::from_bits(word),
            };
            value.to_bits()
        }
    }
}

fn setter_input_type(setter: u32) -> u32 {
    match setter {
        SET_BOOL | SET_BOOL_ARRAY => FXT_BOOL,
        SET_INT | SET_INT_ARRAY => FXT_INT,
        _ => FXT_FLOAT,
    }
}

fn setter_applies(setter: u32, class: u32) -> bool {
    match setter {
        SET_BOOL | SET_INT | SET_FLOAT => class == FXC_SCALAR,
        SET_VECTOR => class != FXC_MATRIX_ROWS && class != FXC_MATRIX_COLUMNS,
        SET_VALUE
        | SET_BOOL_ARRAY
        | SET_INT_ARRAY
        | SET_FLOAT_ARRAY
        | SET_VECTOR_ARRAY
        | SET_MATRIX
        | SET_MATRIX_ARRAY
        | SET_MATRIX_POINTER_ARRAY
        | SET_MATRIX_TRANSPOSE
        | SET_MATRIX_TRANSPOSE_ARRAY
        | SET_MATRIX_TRANSPOSE_POINTER_ARRAY => true,
        _ => false,
    }
}

fn setter_element_words(setter: u32) -> Option<usize> {
    Some(match setter {
        SET_BOOL | SET_BOOL_ARRAY | SET_INT | SET_INT_ARRAY | SET_FLOAT | SET_FLOAT_ARRAY => 1,
        SET_VECTOR | SET_VECTOR_ARRAY => 4,
        SET_MATRIX
        | SET_MATRIX_ARRAY
        | SET_MATRIX_POINTER_ARRAY
        | SET_MATRIX_TRANSPOSE
        | SET_MATRIX_TRANSPOSE_ARRAY
        | SET_MATRIX_TRANSPOSE_POINTER_ARRAY => 16,
        _ => return None,
    })
}

fn matrix_transpose(setter: u32) -> bool {
    matches!(
        setter,
        SET_MATRIX_TRANSPOSE | SET_MATRIX_TRANSPOSE_ARRAY | SET_MATRIX_TRANSPOSE_POINTER_ARRAY
    )
}

fn write_matrix(
    dst: &mut [u8],
    rows: usize,
    cols: usize,
    src: &[u32],
    transpose: bool,
    input: u32,
    target: u32,
) {
    for r in 0..rows {
        for c in 0..cols {
            let (sr, sc) = if transpose { (c, r) } else { (r, c) };
            if sr < 4 && sc < 4 {
                let index = r * cols + c;
                let word = convert_word(input, src[sr * 4 + sc], target);
                let at = index * 4;
                if at + 4 <= dst.len() {
                    dst[at..at + 4].copy_from_slice(&word.to_le_bytes());
                }
            }
        }
    }
}

#[derive(Clone)]
struct ParamTarget {
    ty: Arc<EfxType>,
    flags: u32,
    annotation_count: u32,
    root: Option<usize>,
    annot: Option<usize>,
    offset: usize,
    object: Option<usize>,
}

fn resolve_node(ty: &EfxType, path: &[u32]) -> Option<(EfxType, usize, Option<usize>)> {
    let mut node = ty.clone();
    let mut offset = 0usize;
    let mut object = None;
    for step in path {
        let step = *step as usize;
        if node.class == FXC_OBJECT {
            if node.elements as usize > 1 {
                if step >= node.elements as usize {
                    return None;
                }
                object = Some(step);
                node.elements = 1;
                continue;
            }
            return None;
        }
        if node.elements > 1 {
            if step >= node.elements as usize {
                return None;
            }
            let elem_bytes = (node.bytes / node.elements) as usize;
            offset += step * elem_bytes;
            node.elements = 1;
            continue;
        }
        if node.class == FXC_STRUCT {
            let mut base = 0usize;
            let mut found = None;
            for (i, member) in node.members.iter().enumerate() {
                if i == step {
                    found = Some((member.clone(), base));
                    break;
                }
                base += member.bytes as usize;
            }
            let (member, base) = found?;
            offset += base;
            node = member;
            continue;
        }
        return None;
    }
    Some((node, offset, object))
}

impl D3dxEffect {
    fn resolve_handle(&self, handle: u32) -> Option<(usize, Vec<u32>)> {
        if handle & H_MASK == H_PARAM {
            let index = handle_index(handle, H_PARAM)? as usize;
            self.params.get(index)?;
            return Some((index, Vec::new()));
        }
        if handle & H_MASK == H_CHILD {
            let link = *self.children.borrow().links.get(&handle)?;
            let (root, mut path) = self.resolve_handle(link.parent)?;
            path.push(link.step);
            return Some((root, path));
        }
        None
    }

    fn child_handle(&self, parent: u32, step: u32) -> u32 {
        let mut children = self.children.borrow_mut();
        if let Some(handle) = children.cache.get(&(parent, step)) {
            return *handle;
        }
        let slot = children.next;
        children.next += 1;
        let handle = H_CHILD | slot;
        children.links.insert(handle, ChildLink { parent, step });
        children.cache.insert((parent, step), handle);
        handle
    }

    fn param(&self, handle: u32) -> Option<&EfxParam> {
        let index = handle_index(handle, H_PARAM)? as usize;
        self.params.get(index)
    }

    fn technique(&self, handle: u32) -> Option<&EfxTechnique> {
        handle_index(handle, H_TECH).and_then(|index| self.techniques.get(index as usize))
    }

    fn pass(&self, handle: u32) -> Option<&EfxPass> {
        let (tech, pass) = pass_parts(handle)?;
        self.techniques
            .get(tech as usize)?
            .passes
            .get(pass as usize)
    }

    fn target(&self, handle: u32) -> Option<ParamTarget> {
        if let Some(target) = self.targets.borrow().get(&handle) {
            return Some(target.clone());
        }
        let target = self.resolve_target(handle)?;
        let mut targets = self.targets.borrow_mut();
        if targets.len() >= 4096 {
            targets.clear();
        }
        targets.insert(handle, target.clone());
        Some(target)
    }

    fn resolve_target(&self, handle: u32) -> Option<ParamTarget> {
        if handle & H_MASK == H_ANNOT {
            let index = handle_index(handle, H_ANNOT)? as usize;
            let param = self.annotations.get(index)?;
            return Some(ParamTarget {
                ty: Arc::new(param.type_.clone()),
                flags: param.flags,
                annotation_count: param.annotations.len() as u32,
                root: None,
                annot: Some(index),
                offset: 0,
                object: None,
            });
        }
        let (root, path) = self.resolve_handle(handle)?;
        let param = self.params.get(root)?;
        let (node, offset, object) = resolve_node(&param.type_, &path)?;
        Some(ParamTarget {
            ty: Arc::new(node),
            flags: param.flags,
            annotation_count: param.annotations.len() as u32,
            root: Some(root),
            annot: None,
            offset,
            object,
        })
    }

    fn find_param_index(&self, name: &[u8]) -> Option<usize> {
        self.param_names.get(name).copied()
    }

    fn param_version(&self, index: usize) -> u64 {
        let param = &self.params[index];
        let mut version = param.version;
        if let Some(slot) = param.pool_slot {
            if let Some(pool) = &self.pool {
                if let Some(entry) = pool.lock().unwrap().slot(slot) {
                    version = version.max(entry.version);
                }
            }
        }
        version
    }

    fn with_pool_entry<R>(&self, index: usize, f: impl FnOnce(&PoolEntry) -> R) -> Option<R> {
        let param = self.params.get(index)?;
        if !self.shared(index) {
            return None;
        }
        let slot = param.pool_slot?;
        let state = self.pool.as_ref()?.lock().unwrap();
        state.slot(slot).map(f)
    }

    fn with_numeric<R>(&self, index: usize, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let mut f = Some(f);
        self.with_pool_entry(index, |entry| f.take().map(|f| f(&entry.value)))
            .flatten()
            .or_else(|| match &self.params.get(index)?.value {
                EfxValue::Numeric(bytes) => f.take().map(|f| f(bytes)),
                _ => None,
            })
    }

    fn touch(&mut self, root: Option<usize>) {
        if let Some(index) = root {
            self.params[index].version = next_version();
        }
        self.publish_shared(root);
    }

    fn shared(&self, index: usize) -> bool {
        self.params
            .get(index)
            .map(|param| param.flags & FX_PARAM_SHARED != 0)
            .unwrap_or(false)
    }

    fn pool_value(&self, index: usize) -> Option<Cow<'_, [u8]>> {
        if let Some(value) = self.with_pool_entry(index, |entry| entry.value.clone()) {
            return Some(Cow::Owned(value));
        }
        match &self.params.get(index)?.value {
            EfxValue::Numeric(bytes) => Some(Cow::Borrowed(bytes)),
            _ => None,
        }
    }

    fn pool_objects(&self, index: usize) -> Vec<u32> {
        if let Some(objects) = self.with_pool_entry(index, |entry| entry.objects.clone()) {
            return objects;
        }
        match self.params.get(index).map(|param| &param.value) {
            Some(EfxValue::Object(ids)) => ids.clone(),
            _ => Vec::new(),
        }
    }

    fn pool_object(&self, index: usize, slot: usize) -> u32 {
        if let Some(object) =
            self.with_pool_entry(index, |entry| entry.objects.get(slot).copied().unwrap_or(0))
        {
            return object;
        }
        match self.params.get(index).map(|param| &param.value) {
            Some(EfxValue::Object(ids)) => ids.get(slot).copied().unwrap_or(0),
            _ => 0,
        }
    }

    fn sync_shared(&mut self, root: Option<usize>) {
        let Some(index) = root else { return };
        if !self.shared(index) || self.pool.is_none() {
            return;
        }
        match &self.params[index].value {
            EfxValue::Numeric(_) => {
                let Some(pool) = &self.pool else { return };
                let param = &mut self.params[index];
                let Some(slot) = param.pool_slot else {
                    return;
                };
                let state = pool.lock().unwrap();
                let Some(entry) = state.slot(slot) else {
                    return;
                };
                if param.version != 0 && entry.version == param.version {
                    return;
                }
                if let EfxValue::Numeric(bytes) = &mut param.value {
                    bytes.clear();
                    bytes.extend_from_slice(&entry.value);
                }
                param.version = param.version.max(entry.version);
            }
            EfxValue::Object(_) => {
                self.params[index].value = EfxValue::Object(self.pool_objects(index))
            }
            EfxValue::Sampler(_) => {}
        }
    }

    fn publish_shared(&mut self, root: Option<usize>) {
        let Some(index) = root else {
            return;
        };
        if !self.shared(index) {
            return;
        }
        let Some(pool) = &self.pool else {
            return;
        };
        let id = self.id;
        let param = &self.params[index];
        let Some(slot) = param.pool_slot else {
            return;
        };
        let mut state = pool.lock().unwrap();
        if let Some(entry) = state.slot_mut(slot) {
            if let EfxValue::Numeric(bytes) = &param.value {
                let copy = entry.value.len().min(bytes.len());
                entry.value[..copy].copy_from_slice(&bytes[..copy]);
            }
            if let EfxValue::Object(ids) = &param.value {
                entry.objects.clone_from(ids);
            }
            entry.version = entry.version.max(param.version);
            entry.members.insert(id);
            return;
        }
        let (value, objects) = match &param.value {
            EfxValue::Numeric(bytes) => (bytes.clone(), Vec::new()),
            EfxValue::Object(ids) => (Vec::new(), ids.clone()),
            EfxValue::Sampler(_) => (Vec::new(), Vec::new()),
        };
        state.insert(&param.type_, value, objects, id, param.version);
    }

    fn target_numeric(&self, target: &ParamTarget) -> Option<Vec<u8>> {
        let bytes = match (target.annot, target.root) {
            (Some(index), _) => match &self.annotations[index].value {
                EfxValue::Numeric(bytes) => Cow::Borrowed(bytes.as_slice()),
                _ => return None,
            },
            (_, Some(root)) => match &self.params[root].value {
                EfxValue::Numeric(_) => self.pool_value(root)?,
                _ => return None,
            },
            _ => return None,
        };
        let len = target.ty.bytes as usize;
        let end = target.offset.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        Some(bytes[target.offset..end].to_vec())
    }

    fn target_object(&self, target: &ParamTarget) -> u32 {
        if let Some(index) = target.annot {
            return match &self.annotations[index].value {
                EfxValue::Object(ids) => ids.first().copied().unwrap_or(0),
                _ => 0,
            };
        }
        let Some(root) = target.root else {
            return 0;
        };
        self.pool_object(root, target.object.unwrap_or(0))
    }

    fn state_shader_object(effect: &D3dxEffect, state: &EfxState) -> Option<u32> {
        if let Some(name) = &state.referenced {
            let index = effect.find_param_index(name)?;
            if let EfxValue::Object(ids) = &effect.params[index].value {
                return ids.first().copied();
            }
            return None;
        }
        match &state.value {
            EfxValue::Object(ids) => ids.first().copied(),
            _ => None,
        }
    }

    fn referenced_array_ids<'a>(effect: &'a D3dxEffect, state: &EfxState) -> Option<&'a Vec<u32>> {
        let name = state.referenced.as_ref()?;
        let index = effect.find_param_index(name)?;
        match &effect.params[index].value {
            EfxValue::Object(ids) => Some(ids),
            _ => None,
        }
    }
}

impl D3dxEffectPool {
    fn id(&self) -> u32 {
        self.id
    }
}

pub fn d3dx_effect_pool_id(pool: &D3dxEffectPool) -> u32 {
    pool.id()
}

pub fn d3dx_effect_create(
    effect: &mut D3dxEffect,
    _device: &mut D3d9Device,
    bytes: &[u8],
    flags: u32,
) -> D3dxEffectCreateResult {
    match parse_effect(bytes) {
        Ok(parsed) => {
            let id = effect.id;
            *effect = parsed;
            effect.id = id;
            effect.creation_flags = flags;
            D3dxEffectCreateResult {
                status: ok(),
                parameters: effect.params.len() as u32,
                techniques: effect.techniques.len() as u32,
            }
        }
        Err(error) => D3dxEffectCreateResult {
            status: failed("D3DXCreateEffect", error),
            parameters: 0,
            techniques: 0,
        },
    }
}

pub fn d3dx_effect_join_pool(
    effect: &mut D3dxEffect,
    pool: &D3dxEffectPool,
    device_id: u32,
) -> Status {
    {
        let mut state = pool.state.lock().unwrap();
        if let Some(existing) = state.device_id {
            if existing != device_id {
                return status(
                    StatusCode::InvalidArgument,
                    "shared effect pools must belong to the same device",
                );
            }
        } else {
            state.device_id = Some(device_id);
        }
    }
    effect.device_id = device_id;
    effect.pool_id = pool.id();
    effect.pool = Some(pool.state.clone());
    let mut state = pool.state.lock().unwrap();
    let id = effect.id;
    for index in 0..effect.params.len() {
        if !effect.shared(index) {
            continue;
        }
        if effect.params[index].type_.class == FXC_OBJECT && !effect.params[index].type_.texture() {
            return failed(
                "D3DXCreateEffect",
                unsupported(
                    "Pool",
                    "shared sampler, string and shader parameters are unsupported",
                ),
            );
        }
        let param = &mut effect.params[index];
        let slot = if let Some(slot) = state.find_slot(&param.type_) {
            let entry = state.slot_mut(slot).expect("pool slot is live");
            match &mut param.value {
                EfxValue::Numeric(bytes) => {
                    let copy = entry.value.len().min(bytes.len());
                    bytes[..copy].copy_from_slice(&entry.value[..copy]);
                }
                EfxValue::Object(ids) => *ids = entry.objects.clone(),
                EfxValue::Sampler(_) => {}
            }
            entry.members.insert(id);
            slot
        } else {
            let (value, objects) = match &param.value {
                EfxValue::Numeric(bytes) => (bytes.clone(), Vec::new()),
                EfxValue::Object(ids) => (Vec::new(), ids.clone()),
                EfxValue::Sampler(_) => (Vec::new(), Vec::new()),
            };
            state.insert(&param.type_, value, objects, id, 0)
        };
        param.pool_slot = Some(slot);
    }
    ok()
}

pub fn d3dx_effect_release(effect: &mut D3dxEffect) {
    let Some(pool) = effect.pool.take() else {
        return;
    };
    let id = effect.id;
    let mut state = pool.lock().unwrap();
    state.remove_member(id);
}

pub fn d3dx_effect_texture_refs(effect: &D3dxEffect) -> Vec<u32> {
    let mut refs = BTreeSet::new();
    for (index, param) in effect.params.iter().enumerate() {
        if (effect.shared(index) && effect.pool.is_some()) || !param.type_.texture() {
            continue;
        }
        if let EfxValue::Object(ids) = &param.value {
            for id in ids {
                if *id != 0 {
                    refs.insert(*id);
                }
            }
        }
    }
    refs.into_iter().collect()
}

pub fn d3dx_effect_pool_texture_refs(pool: &D3dxEffectPool) -> Vec<u32> {
    let state = pool.state.lock().unwrap();
    let mut refs = BTreeSet::new();
    for entry in state.values() {
        if entry.members.is_empty() || !texture_type(entry.shape.type_) {
            continue;
        }
        for id in &entry.objects {
            if *id != 0 {
                refs.insert(*id);
            }
        }
    }
    refs.into_iter().collect()
}

pub fn d3dx_effect_saved_bindings(effect: &D3dxEffect) -> D3d9BindingsResult {
    let live = effect
        .saved
        .as_ref()
        .map(|snapshot| super::bridge_bindings(snapshot.bindings()))
        .unwrap_or_else(super::empty_bridge_bindings);
    D3d9BindingsResult {
        status: ok(),
        live,
        recording: super::empty_bridge_bindings(),
        recording_active: false,
    }
}

pub fn d3dx_effect_set(effect: &mut D3dxEffect, handle: u32, setter: u32, data: &[u8]) -> Status {
    stats::record(|s| s.effect_setters += 1);
    if setter > SET_LAST {
        return invalid_call("ID3DXEffect::SetValue");
    }
    let Some(target) = effect.target(handle) else {
        return invalid_call("ID3DXEffect::SetValue");
    };
    if target.ty.class == FXC_OBJECT || target.ty.class == FXC_STRUCT {
        return invalid_call("ID3DXEffect::SetValue");
    }
    effect.sync_shared(target.root);
    let mut stack = [0u32; 64];
    let mut heap = Vec::new();
    if setter == SET_VALUE {
        let Ok(words) = source_words(data, &mut stack, &mut heap) else {
            return invalid_call("ID3DXEffect::SetValue");
        };
        let len = target.ty.bytes as usize;
        if words.len() * 4 > len {
            return invalid_call("ID3DXEffect::SetValue");
        }
        let Some(dst) = target_numeric_mut(effect, &target) else {
            return invalid_call("ID3DXEffect::SetValue");
        };
        for (slot, word) in words.iter().enumerate() {
            dst[slot * 4..slot * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        effect.touch(target.root);
        return ok();
    }
    if !setter_applies(setter, target.ty.class) || target.ty.type_ == FXT_STRING {
        return invalid_call("ID3DXEffect::SetValue");
    }
    let Some(element_words) = setter_element_words(setter) else {
        return invalid_call("ID3DXEffect::SetValue");
    };
    let Ok(words) = source_words(data, &mut stack, &mut heap) else {
        return invalid_call("ID3DXEffect::SetValue");
    };
    if words.is_empty() || words.len() % element_words != 0 {
        return invalid_call("ID3DXEffect::SetValue");
    }
    let supplied = words.len() / element_words;
    let input = setter_input_type(setter);
    let rows = target.ty.rows.max(1) as usize;
    let cols = target.ty.cols.max(1) as usize;
    let elements = target.ty.count() as usize;
    let Some(dst) = target_numeric_mut(effect, &target) else {
        return invalid_call("ID3DXEffect::SetValue");
    };
    match setter {
        SET_MATRIX | SET_MATRIX_TRANSPOSE => {
            if elements != 1 {
                return invalid_call("ID3DXEffect::SetValue");
            }
            write_matrix(
                dst,
                rows,
                cols,
                &words,
                matrix_transpose(setter),
                FXT_FLOAT,
                target.ty.type_,
            );
        }
        SET_MATRIX_ARRAY
        | SET_MATRIX_POINTER_ARRAY
        | SET_MATRIX_TRANSPOSE_ARRAY
        | SET_MATRIX_TRANSPOSE_POINTER_ARRAY => {
            let count = supplied.min(elements);
            for element in 0..count {
                let base = element * rows * cols;
                if base + rows * cols > dst.len() / 4 {
                    break;
                }
                let src = &words[element * 16..element * 16 + 16];
                write_matrix(
                    &mut dst[base * 4..(base + rows * cols) * 4],
                    rows,
                    cols,
                    src,
                    matrix_transpose(setter),
                    FXT_FLOAT,
                    target.ty.type_,
                );
            }
        }
        SET_VECTOR | SET_VECTOR_ARRAY => {
            if elements != 1 && setter == SET_VECTOR {
                return invalid_call("ID3DXEffect::SetValue");
            }
            let count = if setter == SET_VECTOR {
                1
            } else {
                supplied.min(elements)
            };
            for element in 0..count {
                let base = element * rows * cols;
                for lane in 0..cols {
                    if base + lane >= dst.len() / 4 {
                        break;
                    }
                    let word = convert_word(
                        input,
                        words.get(element * 4 + lane).copied().unwrap_or(0),
                        target.ty.type_,
                    );
                    let at = (base + lane) * 4;
                    dst[at..at + 4].copy_from_slice(&word.to_le_bytes());
                }
            }
        }
        SET_BOOL | SET_INT | SET_FLOAT => {
            if elements != 1 {
                return invalid_call("ID3DXEffect::SetValue");
            }
            let word = convert_word(input, words[0], target.ty.type_);
            dst[..4].copy_from_slice(&word.to_le_bytes());
        }
        SET_BOOL_ARRAY | SET_INT_ARRAY | SET_FLOAT_ARRAY => {
            let total = dst.len() / 4;
            let size = supplied.min(total);
            for i in 0..size {
                let word = convert_word(input, words[i], target.ty.type_);
                let at = i * 4;
                dst[at..at + 4].copy_from_slice(&word.to_le_bytes());
            }
        }
        _ => return invalid_call("ID3DXEffect::SetValue"),
    }
    effect.touch(target.root);
    ok()
}

pub fn d3dx_effect_set_raw(
    effect: &mut D3dxEffect,
    handle: u32,
    offset: u32,
    data: &[u8],
) -> Status {
    let Some(target) = effect.target(handle) else {
        return invalid_call("ID3DXEffect::SetRawValue");
    };
    if target.ty.class == FXC_OBJECT || target.ty.class == FXC_STRUCT {
        return invalid_call("ID3DXEffect::SetRawValue");
    }
    effect.sync_shared(target.root);
    let Some(dst) = target_numeric_mut(effect, &target) else {
        return invalid_call("ID3DXEffect::SetRawValue");
    };
    let start = offset as usize;
    let Some(end) = start
        .checked_add(data.len())
        .filter(|end| *end <= dst.len())
    else {
        return invalid_call("ID3DXEffect::SetRawValue");
    };
    dst[start..end].copy_from_slice(data);
    effect.touch(target.root);
    ok()
}

fn source_words<'a>(
    data: &[u8],
    stack: &'a mut [u32; 64],
    heap: &'a mut Vec<u32>,
) -> Result<&'a [u32], RenderError> {
    if data.len() % 4 != 0 {
        return Err(invalid_data("setter input is not DWORD aligned"));
    }
    let words = data
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
    let count = data.len() / 4;
    if count <= stack.len() {
        for (slot, word) in stack.iter_mut().zip(words) {
            *slot = word;
        }
        Ok(&stack[..count])
    } else {
        heap.extend(words);
        Ok(heap)
    }
}

fn target_numeric_mut<'a>(
    effect: &'a mut D3dxEffect,
    target: &ParamTarget,
) -> Option<&'a mut [u8]> {
    let len = target.ty.bytes as usize;
    if let Some(index) = target.annot {
        if let EfxValue::Numeric(bytes) = &mut effect.annotations[index].value {
            if len <= bytes.len() {
                return Some(&mut bytes[..len]);
            }
        }
        return None;
    }
    let root = target.root?;
    if let EfxValue::Numeric(bytes) = &mut effect.params[root].value {
        let end = target.offset.checked_add(len)?;
        if end <= bytes.len() {
            return Some(&mut bytes[target.offset..end]);
        }
    }
    None
}

pub fn d3dx_effect_get(effect: &D3dxEffect, handle: u32, kind: u32) -> D3dxBytesResult {
    let Some(target) = effect.target(handle) else {
        return D3dxBytesResult {
            status: invalid_call("ID3DXEffect::GetValue"),
            bytes: Vec::new(),
        };
    };
    if kind == READ_STRING {
        return get_string(effect, &target);
    }
    if target.ty.class == FXC_OBJECT || target.ty.class == FXC_STRUCT {
        return D3dxBytesResult {
            status: invalid_call("ID3DXEffect::GetValue"),
            bytes: Vec::new(),
        };
    }
    let Some(bytes) = effect.target_numeric(&target) else {
        return D3dxBytesResult {
            status: invalid_call("ID3DXEffect::GetValue"),
            bytes: Vec::new(),
        };
    };
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect();
    let rows = target.ty.rows.max(1) as usize;
    let cols = target.ty.cols.max(1) as usize;
    let elements = target.ty.count() as usize;
    let type_ = target.ty.type_;
    let out = match kind {
        READ_FLOAT | READ_INT | READ_BOOL => {
            let out_type = match kind {
                READ_FLOAT => FXT_FLOAT,
                READ_INT => FXT_INT,
                _ => FXT_BOOL,
            };
            convert_slice(&words, type_, out_type, 1)
        }
        READ_VECTOR | READ_VECTOR_ARRAY => get_vector(&words, rows, cols, elements, type_),
        READ_MATRIX | READ_MATRIX_ARRAY => get_matrix(&words, rows, cols, elements, type_, false),
        READ_MATRIX_TRANSPOSE | READ_MATRIX_TRANSPOSE_ARRAY => {
            get_matrix(&words, rows, cols, elements, type_, true)
        }
        READ_FLOAT_ARRAY => convert_slice(&words, type_, FXT_FLOAT, words.len()),
        READ_INT_ARRAY => convert_slice(&words, type_, FXT_INT, words.len()),
        READ_BOOL_ARRAY => convert_slice(&words, type_, FXT_BOOL, words.len()),
        READ_VALUE => bytes.clone(),
        READ_TEXTURE => {
            return D3dxBytesResult {
                status: invalid_call("ID3DXEffect::GetValue"),
                bytes: Vec::new(),
            };
        }
        _ => {
            return D3dxBytesResult {
                status: invalid_call("ID3DXEffect::GetValue"),
                bytes: Vec::new(),
            };
        }
    };
    D3dxBytesResult {
        status: ok(),
        bytes: out,
    }
}

fn get_vector(words: &[u32], rows: usize, cols: usize, elements: usize, type_: u32) -> Vec<u8> {
    let zero = 0.0f32.to_bits();
    let mut out = Vec::with_capacity(elements * 16);
    for element in 0..elements {
        for lane in 0..4usize {
            let bits = if lane < cols {
                words
                    .get(element * rows * cols + lane)
                    .map(|word| convert_word(type_, *word, FXT_FLOAT))
                    .unwrap_or(zero)
            } else {
                zero
            };
            out.extend_from_slice(&bits.to_le_bytes());
        }
    }
    out
}

fn get_matrix(
    words: &[u32],
    rows: usize,
    cols: usize,
    elements: usize,
    type_: u32,
    transpose: bool,
) -> Vec<u8> {
    let zero = 0.0f32.to_bits();
    let mut out = vec![0u8; elements * 64];
    for element in 0..elements {
        for i in 0..4usize {
            for k in 0..4usize {
                let bits = if i < rows && k < cols {
                    words
                        .get(element * rows * cols + i * cols + k)
                        .map(|word| convert_word(type_, *word, FXT_FLOAT))
                        .unwrap_or(zero)
                } else {
                    zero
                };
                let (row, col) = if transpose { (k, i) } else { (i, k) };
                let at = (element * 16 + row * 4 + col) * 4;
                out[at..at + 4].copy_from_slice(&bits.to_le_bytes());
            }
        }
    }
    out
}

fn convert_slice(words: &[u32], input: u32, target: u32, count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count.min(words.len()) * 4);
    for word in words.iter().take(count) {
        out.extend_from_slice(&convert_word(input, *word, target).to_le_bytes());
    }
    out
}

fn get_string(effect: &D3dxEffect, target: &ParamTarget) -> D3dxBytesResult {
    let id = effect.target_object(target);
    match effect.objects.get(&id) {
        Some(object) => D3dxBytesResult {
            status: ok(),
            bytes: trim_nul(&object.data),
        },
        None => D3dxBytesResult {
            status: invalid_call("ID3DXEffect::GetString"),
            bytes: Vec::new(),
        },
    }
}

pub fn d3dx_effect_set_texture(effect: &mut D3dxEffect, handle: u32, id: u32) -> ValueResult {
    stats::record(|s| s.effect_texture_setters += 1);
    let Some(target) = effect.target(handle) else {
        return ValueResult {
            status: invalid_call("ID3DXEffect::SetTexture"),
            value: 0,
        };
    };
    if !target.ty.texture() {
        return ValueResult {
            status: invalid_call("ID3DXEffect::SetTexture"),
            value: 0,
        };
    }
    let Some(root) = target.root else {
        return ValueResult {
            status: invalid_call("ID3DXEffect::SetTexture"),
            value: 0,
        };
    };
    effect.sync_shared(target.root);
    let previous = effect.target_object(&target);
    match target.object {
        Some(index) => match &mut effect.params[root].value {
            EfxValue::Object(ids) if index < ids.len() => ids[index] = id,
            EfxValue::Object(ids) => ids.push(id),
            _ => {
                return ValueResult {
                    status: invalid_call("ID3DXEffect::SetTexture"),
                    value: 0,
                };
            }
        },
        None => match &mut effect.params[root].value {
            EfxValue::Object(ids) => {
                if ids.is_empty() {
                    ids.push(id);
                } else {
                    ids[0] = id;
                }
            }
            _ => {
                return ValueResult {
                    status: invalid_call("ID3DXEffect::SetTexture"),
                    value: 0,
                };
            }
        },
    }
    effect.touch(target.root);
    ValueResult {
        status: ok(),
        value: previous,
    }
}

pub fn d3dx_effect_texture_shared(effect: &D3dxEffect, handle: u32) -> bool {
    effect
        .target(handle)
        .and_then(|target| target.root)
        .is_some_and(|root| effect.shared(root))
}

pub fn d3dx_effect_texture_ref_count(effect: &D3dxEffect, id: u32) -> u32 {
    if id == 0 {
        return 0;
    }
    let pooled = effect.pool.is_some();
    effect
        .params
        .iter()
        .enumerate()
        .filter(|(index, param)| !(effect.shared(*index) && pooled) && param.type_.texture())
        .filter_map(|(_, param)| match &param.value {
            EfxValue::Object(ids) => Some(ids),
            _ => None,
        })
        .flatten()
        .filter(|&&candidate| candidate == id)
        .count() as u32
}

pub fn d3dx_effect_pool_texture_ref_count(pool: &D3dxEffectPool, id: u32) -> u32 {
    if id == 0 {
        return 0;
    }
    let state = pool.state.lock().unwrap();
    state
        .values()
        .filter(|entry| texture_type(entry.shape.type_))
        .flat_map(|entry| &entry.objects)
        .filter(|&&candidate| candidate == id)
        .count() as u32
}

pub fn d3dx_effect_texture(effect: &D3dxEffect, handle: u32) -> ValueResult {
    let Some(target) = effect.target(handle) else {
        return ValueResult {
            status: invalid_call("ID3DXEffect::GetTexture"),
            value: 0,
        };
    };
    if !target.ty.texture() {
        return ValueResult {
            status: invalid_call("ID3DXEffect::GetTexture"),
            value: 0,
        };
    }
    ValueResult {
        status: ok(),
        value: effect.target_object(&target),
    }
}

pub fn d3dx_effect_desc(effect: &D3dxEffect) -> D3dxEffectDescResult {
    D3dxEffectDescResult {
        status: ok(),
        desc: D3dxEffectDesc {
            parameters: effect.params.len() as u32,
            techniques: effect.techniques.len() as u32,
            functions: 0,
        },
    }
}

pub fn d3dx_effect_param_desc(effect: &D3dxEffect, handle: u32) -> D3dxEffectParamDescResult {
    let Some(target) = effect.target(handle) else {
        return D3dxEffectParamDescResult {
            status: invalid_call("ID3DXEffect::GetParameterDesc"),
            desc: D3dxEffectParamDesc::default(),
            name: Vec::new(),
            semantic: Vec::new(),
        };
    };
    D3dxEffectParamDescResult {
        status: ok(),
        desc: D3dxEffectParamDesc {
            class_: target.ty.class,
            type_: target.ty.type_,
            rows: target.ty.rows,
            columns: target.ty.cols,
            elements: target.ty.elements,
            annotations: target.annotation_count,
            struct_members: target.ty.members.len() as u32,
            flags: target.flags,
            bytes: target.ty.bytes,
        },
        name: target.ty.name.clone(),
        semantic: target.ty.semantic.clone(),
    }
}

pub fn d3dx_effect_param_name(effect: &D3dxEffect, handle: u32) -> D3dxBytesResult {
    let name = effect
        .target(handle)
        .map(|target| target.ty.name.clone())
        .or_else(|| effect.technique(handle).map(|tech| tech.name.clone()))
        .or_else(|| effect.pass(handle).map(|pass| pass.name.clone()))
        .unwrap_or_default();
    D3dxBytesResult {
        status: ok(),
        bytes: name,
    }
}

pub fn d3dx_effect_param_semantic(effect: &D3dxEffect, handle: u32) -> D3dxBytesResult {
    let semantic = effect
        .target(handle)
        .map(|target| target.ty.semantic.clone())
        .unwrap_or_default();
    D3dxBytesResult {
        status: ok(),
        bytes: semantic,
    }
}

pub fn d3dx_effect_technique_desc(
    effect: &D3dxEffect,
    handle: u32,
) -> D3dxEffectTechniqueDescResult {
    let Some(tech) = effect.technique(handle) else {
        return D3dxEffectTechniqueDescResult {
            status: invalid_call("ID3DXEffect::GetTechniqueDesc"),
            desc: D3dxEffectTechniqueDesc::default(),
            name: Vec::new(),
        };
    };
    D3dxEffectTechniqueDescResult {
        status: ok(),
        desc: D3dxEffectTechniqueDesc {
            passes: tech.passes.len() as u32,
            annotations: tech.annotations.len() as u32,
        },
        name: tech.name.clone(),
    }
}

pub fn d3dx_effect_pass_desc(effect: &D3dxEffect, handle: u32) -> D3dxEffectPassDescResult {
    let Some(pass) = effect.pass(handle) else {
        return D3dxEffectPassDescResult {
            status: invalid_call("ID3DXEffect::GetPassDesc"),
            desc: D3dxEffectPassDesc::default(),
            name: Vec::new(),
        };
    };
    D3dxEffectPassDescResult {
        status: ok(),
        desc: D3dxEffectPassDesc {
            annotations: pass.annotations.len() as u32,
            vertex_shader: pass_shader_bytes(effect, pass, false),
            pixel_shader: pass_shader_bytes(effect, pass, true),
        },
        name: pass.name.clone(),
    }
}

fn pass_shader_bytes(effect: &D3dxEffect, pass: &EfxPass, pixel: bool) -> Vec<u8> {
    if effect.creation_flags & (1 << 11) != 0 {
        return Vec::new();
    }
    let op = if pixel {
        FXS_PIXELSHADER
    } else {
        FXS_VERTEXSHADER
    };
    for state in pass.states.iter() {
        if state.op != op {
            continue;
        }
        if let Some(shader) = &state.shader {
            return shader.clone();
        }
        if let Some(id) = D3dxEffect::state_shader_object(effect, state) {
            if let Some(object) = effect.objects.get(&id) {
                return object.data.clone();
            }
        }
    }
    Vec::new()
}

pub fn d3dx_effect_pass_shader(effect: &D3dxEffect, handle: u32, pixel: bool) -> D3dxBytesResult {
    let Some(pass) = effect.pass(handle) else {
        return D3dxBytesResult {
            status: invalid_call("ID3DXEffect::GetPassDesc"),
            bytes: Vec::new(),
        };
    };
    D3dxBytesResult {
        status: ok(),
        bytes: pass_shader_bytes(effect, pass, pixel),
    }
}

pub fn d3dx_effect_find(effect: &D3dxEffect, parent: u32, name: &[u8], kind: u32) -> u32 {
    if name.is_empty() {
        return if parent != 0 && matches!(kind, K_PARAM) {
            parent
        } else {
            0
        };
    }
    match kind {
        K_TECH => effect
            .techniques
            .iter()
            .position(|tech| tech.name == name)
            .map(|index| H_TECH | index as u32)
            .unwrap_or(0),
        K_PASS => {
            for (ti, tech) in effect.techniques.iter().enumerate() {
                for (pi, pass) in tech.passes.iter().enumerate() {
                    if pass.name == name {
                        return pass_handle(ti as u32, pi as u32);
                    }
                }
            }
            0
        }
        K_ANNOT => {
            let annotations: Option<&Vec<u32>> = if parent == 0 {
                None
            } else if let Some(param) = effect.param(parent) {
                Some(&param.annotations)
            } else if let Some(pass) = effect.pass(parent) {
                Some(&pass.annotations)
            } else if let Some(tech) = effect.technique(parent) {
                Some(&tech.annotations)
            } else {
                None
            };
            annotations
                .and_then(|list| {
                    list.iter()
                        .find(|index| effect.annotations[**index as usize].type_.name == name)
                        .map(|index| H_ANNOT | *index)
                })
                .unwrap_or(0)
        }
        K_PARAM | K_ELEM => find_param_path(effect, parent, name, kind),
        _ => 0,
    }
}

fn find_param_path(effect: &D3dxEffect, parent: u32, name: &[u8], kind: u32) -> u32 {
    let segments = match split_path(name) {
        Some(segments) => segments,
        None => return 0,
    };
    let mut current = parent;
    let mut first = true;
    for segment in segments {
        match segment {
            PathSegment::Member(member) => {
                if first && parent == 0 {
                    let matched = effect
                        .params
                        .iter()
                        .position(|param| param.type_.name == member);
                    match matched {
                        Some(index) => current = H_PARAM | index as u32,
                        None => return 0,
                    }
                } else {
                    let Some((root, path)) = effect.resolve_handle(current) else {
                        return 0;
                    };
                    let param = &effect.params[root];
                    let Some((node, _, _)) = resolve_node(&param.type_, &path) else {
                        return 0;
                    };
                    if node.class != FXC_STRUCT {
                        return 0;
                    }
                    let Some(step) = node
                        .members
                        .iter()
                        .position(|candidate| candidate.name == member)
                    else {
                        return 0;
                    };
                    current = effect.child_handle(current, step as u32);
                    first = false;
                    continue;
                }
            }
            PathSegment::Element(index) => {
                let Some((root, path)) = effect.resolve_handle(current) else {
                    return 0;
                };
                let param = &effect.params[root];
                let Some((node, _, _)) = resolve_node(&param.type_, &path) else {
                    return 0;
                };
                if node.elements <= 1 || index >= node.elements {
                    return 0;
                }
                current = effect.child_handle(current, index);
            }
        }
        first = false;
    }
    if kind == K_ELEM {
        return current;
    }
    current
}

enum PathSegment {
    Member(Vec<u8>),
    Element(u32),
}

fn split_path(name: &[u8]) -> Option<Vec<PathSegment>> {
    let mut segments = Vec::new();
    let mut at = 0usize;
    while at < name.len() {
        if name[at] == b'.' || name[at] == b'[' {
            return None;
        }
        let end = name[at..]
            .iter()
            .position(|b| *b == b'.' || *b == b'[')
            .map(|p| p + at)
            .unwrap_or(name.len());
        segments.push(PathSegment::Member(name[at..end].to_vec()));
        at = end;
        while at < name.len() && name[at] == b'[' {
            let close = name[at..].iter().position(|b| *b == b']')? + at;
            let digits = &name[at + 1..close];
            if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let index = std::str::from_utf8(digits).ok()?.parse::<u32>().ok()?;
            segments.push(PathSegment::Element(index));
            at = close + 1;
        }
        if at < name.len() {
            if name[at] != b'.' {
                return None;
            }
            at += 1;
        }
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments)
    }
}

pub fn d3dx_effect_enum(effect: &D3dxEffect, parent: u32, index: u32, kind: u32) -> u32 {
    match kind {
        K_PARAM => {
            if parent == 0 {
                return if (index as usize) < effect.params.len() {
                    H_PARAM | index
                } else {
                    0
                };
            }
            let Some((root, path)) = effect.resolve_handle(parent) else {
                return 0;
            };
            let param = &effect.params[root];
            let Some((node, _, _)) = resolve_node(&param.type_, &path) else {
                return 0;
            };
            if node.elements > 1 || node.class != FXC_STRUCT {
                return 0;
            }
            if (index as usize) < node.members.len() {
                effect.child_handle(parent, index)
            } else {
                0
            }
        }
        K_ELEM => {
            if parent == 0 {
                return if (index as usize) < effect.params.len() {
                    H_PARAM | index
                } else {
                    0
                };
            }
            let Some((root, path)) = effect.resolve_handle(parent) else {
                return 0;
            };
            let param = &effect.params[root];
            let Some((node, _, _)) = resolve_node(&param.type_, &path) else {
                return 0;
            };
            if node.elements > 1 && index < node.elements {
                effect.child_handle(parent, index)
            } else {
                0
            }
        }
        K_TECH => {
            if (index as usize) < effect.techniques.len() {
                H_TECH | index
            } else {
                0
            }
        }
        K_PASS => {
            let Some(tech) = handle_index(parent, H_TECH) else {
                return 0;
            };
            let Some(technique) = effect.techniques.get(tech as usize) else {
                return 0;
            };
            if (index as usize) < technique.passes.len() {
                pass_handle(tech, index)
            } else {
                0
            }
        }
        K_ANNOT => {
            let list: Option<&Vec<u32>> = if parent == 0 {
                None
            } else if let Some(param) = effect.param(parent) {
                Some(&param.annotations)
            } else if let Some(pass) = effect.pass(parent) {
                Some(&pass.annotations)
            } else if let Some(tech) = effect.technique(parent) {
                Some(&tech.annotations)
            } else {
                None
            };
            match list {
                Some(list) if (index as usize) < list.len() => H_ANNOT | list[index as usize],
                _ => 0,
            }
        }
        _ => 0,
    }
}

pub fn d3dx_effect_set_technique(effect: &mut D3dxEffect, handle: u32) -> Status {
    if effect.batch.is_some() {
        return invalid_call("ID3DXEffect::SetTechnique");
    }
    if handle == 0 {
        effect.technique = u32::MAX;
        return ok();
    }
    let Some(index) = handle_index(handle, H_TECH) else {
        return invalid_call("ID3DXEffect::SetTechnique");
    };
    if (index as usize) < effect.techniques.len() {
        effect.technique = index;
        ok()
    } else {
        invalid_call("ID3DXEffect::SetTechnique")
    }
}

pub fn d3dx_effect_current_technique(effect: &D3dxEffect) -> u32 {
    if effect.technique == u32::MAX {
        0
    } else {
        H_TECH | effect.technique
    }
}

fn technique_supported(effect: &D3dxEffect, technique: &EfxTechnique) -> Result<(), RenderError> {
    for pass in &technique.passes {
        for state in pass.states.iter() {
            if state.op <= 102 {
                if RENDER_STATES.get(state.op as usize).is_none() {
                    return Err(unsupported(
                        "ValidateTechnique",
                        format!("render state operation {} is unsupported", state.op),
                    ));
                }
                continue;
            }
            if (103..=120).contains(&state.op) {
                if STAGE_STATES.get((state.op - 103) as usize).is_none() || state.index >= 8 {
                    return Err(unsupported(
                        "ValidateTechnique",
                        format!("texture stage operation {} is unsupported", state.op),
                    ));
                }
                continue;
            }
            if state.op == 126 {
                if state.index >= 8 {
                    return Err(unsupported(
                        "ValidateTechnique",
                        "texture transform index is out of range",
                    ));
                }
                continue;
            }
            if state.op == FXS_SAMPLER_TEXTURE {
                if state.index >= 16 {
                    return Err(unsupported(
                        "ValidateTechnique",
                        "texture stage is out of range",
                    ));
                }
                validate_state_references(effect, state, technique)?;
                continue;
            }
            if state.op == FXS_SETSAMPLER
                || (FXS_SAMPLER_FIRST..=FXS_SAMPLER_LAST).contains(&state.op)
            {
                if state.index >= 16 {
                    return Err(unsupported(
                        "ValidateTechnique",
                        "sampler stage is out of range",
                    ));
                }
                validate_state_references(effect, state, technique)?;
                continue;
            }
            if state.op == FXS_VERTEXSHADER || state.op == FXS_PIXELSHADER {
                validate_state_references(effect, state, technique)?;
                continue;
            }
            if state.op == FXS_FVF {
                continue;
            }
            return Err(unsupported(
                "ValidateTechnique",
                format!("effect state operation {} is unsupported", state.op),
            ));
        }
    }
    Ok(())
}

fn validate_state_references(
    effect: &D3dxEffect,
    state: &EfxState,
    _technique: &EfxTechnique,
) -> Result<(), RenderError> {
    match state.op {
        FXS_VERTEXSHADER | FXS_PIXELSHADER => {
            if state.selector.is_some() {
                let ids = D3dxEffect::referenced_array_ids(effect, state).ok_or_else(|| {
                    invalid_data("array selector references a missing shader array")
                })?;
                if ids.is_empty() {
                    return Err(invalid_data("array selector shader array is empty"));
                }
                for id in ids {
                    let object = effect
                        .objects
                        .get(id)
                        .ok_or_else(|| invalid_data("array selector shader object is missing"))?;
                    if object.data.is_empty() {
                        return Err(invalid_data("array selector shader object has no bytecode"));
                    }
                }
            } else if let Some(id) = D3dxEffect::state_shader_object(effect, state) {
                let object = effect
                    .objects
                    .get(&id)
                    .ok_or_else(|| invalid_data("pass shader object is missing"))?;
                if object.data.is_empty() {
                    return Err(invalid_data("pass shader object has no bytecode"));
                }
            }
            Ok(())
        }
        FXS_SAMPLER_TEXTURE => {
            if let Some(name) = &state.referenced {
                effect
                    .find_param_index(name)
                    .ok_or_else(|| invalid_data("texture parameter is missing"))?;
            }
            Ok(())
        }
        FXS_SETSAMPLER => sampler_state_parameter(effect, state).map(|_| ()),
        _ => Ok(()),
    }
}

pub fn d3dx_effect_validate_technique(effect: &D3dxEffect, handle: u32) -> Status {
    let Some(technique) = effect.technique(handle) else {
        return invalid_call("ID3DXEffect::ValidateTechnique");
    };
    match technique_supported(effect, technique) {
        Ok(()) => ok(),
        Err(error) => failed("ID3DXEffect::ValidateTechnique", error),
    }
}

pub fn d3dx_effect_find_next_valid(effect: &D3dxEffect, handle: u32) -> u32 {
    let from = if handle == 0 {
        0
    } else {
        match handle_index(handle, H_TECH) {
            Some(index) if (index as usize) < effect.techniques.len() => index + 1,
            _ => return 0,
        }
    };
    for index in from..effect.techniques.len() as u32 {
        if technique_supported(effect, &effect.techniques[index as usize]).is_ok() {
            return H_TECH | index;
        }
    }
    0
}

pub fn d3dx_effect_is_parameter_used(effect: &D3dxEffect, param: u32, tech: u32) -> bool {
    let Some((root, path)) = effect.resolve_handle(param) else {
        return false;
    };
    let Some(param) = effect.params.get(root) else {
        return false;
    };
    let Some((node, _, _)) = resolve_node(&param.type_, &path) else {
        return false;
    };
    let name = node.name;
    let Some(tech) = effect.technique(tech) else {
        return false;
    };
    for pass in &tech.passes {
        for state in pass.states.iter() {
            if state_uses_name(effect, state, &name) {
                return true;
            }
        }
    }
    false
}

fn state_uses_name(effect: &D3dxEffect, state: &EfxState, name: &[u8]) -> bool {
    if state.referenced.as_deref() == Some(name) {
        return true;
    }
    if state.op == FXS_SETSAMPLER {
        if let Ok(index) = sampler_state_parameter(effect, state) {
            if effect.params[index].type_.name == name || sampler_uses_name(effect, index, name) {
                return true;
            }
        }
    }
    if state.op == FXS_VERTEXSHADER || state.op == FXS_PIXELSHADER {
        let mut ids = Vec::new();
        if state.selector.is_some() {
            if let Some(list) = D3dxEffect::referenced_array_ids(effect, state) {
                ids.extend(list.iter().copied());
            }
        } else if let Some(id) = D3dxEffect::state_shader_object(effect, state) {
            ids.push(id);
        }
        for id in ids {
            if let Some(object) = effect.objects.get(&id) {
                if ctab_uses_name(effect, &object.data, name) {
                    return true;
                }
            }
        }
    }
    if let Some(code) = &state.preshader {
        if ctab_uses_name(effect, code, name) {
            return true;
        }
    }
    false
}

fn sampler_uses_name(effect: &D3dxEffect, index: usize, name: &[u8]) -> bool {
    let EfxValue::Sampler(states) = &effect.params[index].value else {
        return false;
    };
    for state in states {
        if state_uses_name(effect, state, name) {
            return true;
        }
    }
    false
}

fn ctab_uses_name(effect: &D3dxEffect, code: &[u8], name: &[u8]) -> bool {
    let Ok(constants) = parse_ctab(code) else {
        return true;
    };
    if constants.iter().any(|constant| {
        constant.name == name
            || (constant.set == D3DXRS_SAMPLER
                && effect
                    .find_param_index(&constant.name)
                    .is_some_and(|index| sampler_uses_name(effect, index, name)))
    }) {
        return true;
    }
    match preshader::shader_preshader(code) {
        Ok(Some(code)) => ctab_uses_name(effect, code, name),
        Ok(None) => false,
        Err(_) => true,
    }
}

fn object_shader_handle(
    effect: &mut D3dxEffect,
    device: &mut D3d9Device,
    id: u32,
) -> Result<u32, RenderError> {
    if let Some((_, handle)) = effect.shaders.get(&id) {
        return Ok(*handle);
    }
    let object = effect
        .objects
        .get(&id)
        .ok_or_else(|| invalid_data("pass shader object is missing"))?;
    let data = object.data.clone();
    if data.len() < 4 || data.len() % 4 != 0 {
        return Err(invalid_data("pass shader bytecode is not DWORD aligned"));
    }
    if let Some(code) = preshader::shader_preshader(&data)? {
        preshader::parse(code)?;
    }
    let version = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let pixel = version >> 16 == 0xffff;
    let words: Vec<u32> = data
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect();
    let handle = device.objects.add_shader(pixel, &words)?;
    effect.shaders.insert(id, (pixel, handle));
    Ok(handle)
}

pub fn d3dx_effect_prepare_shaders(
    effect: &mut D3dxEffect,
    device: &mut D3d9Device,
) -> D3dxEffectShadersResult {
    let mut ids = BTreeSet::new();
    for technique in &effect.techniques {
        for pass in &technique.passes {
            for state in pass.states.iter() {
                if state.op != FXS_VERTEXSHADER && state.op != FXS_PIXELSHADER {
                    continue;
                }
                if state.selector.is_some() {
                    if let Some(list) = D3dxEffect::referenced_array_ids(effect, state) {
                        ids.extend(list.iter().copied());
                    }
                } else if let Some(id) = D3dxEffect::state_shader_object(effect, state) {
                    ids.insert(id);
                }
            }
        }
    }
    let mut vertex = Vec::new();
    let mut pixel = Vec::new();
    let mut created: Vec<(u32, bool, u32)> = Vec::new();
    for id in ids {
        let existed = effect.shaders.contains_key(&id);
        match object_shader_handle(effect, device, id) {
            Ok(handle) => {
                let is_pixel = effect
                    .shaders
                    .get(&id)
                    .map(|entry| entry.0)
                    .unwrap_or(false);
                if !existed {
                    created.push((id, is_pixel, handle));
                }
                if is_pixel {
                    pixel.push(handle);
                } else {
                    vertex.push(handle);
                }
            }
            Err(error) => {
                for (id, is_pixel, handle) in created.into_iter().rev() {
                    effect.shaders.remove(&id);
                    let _ = device.objects.destroy_shader(is_pixel, handle);
                }
                return D3dxEffectShadersResult {
                    status: failed("ID3DXEffect::Begin", error),
                    vertex: Vec::new(),
                    pixel: Vec::new(),
                };
            }
        }
    }
    vertex.sort_unstable();
    vertex.dedup();
    pixel.sort_unstable();
    pixel.dedup();
    D3dxEffectShadersResult {
        status: ok(),
        vertex,
        pixel,
    }
}

struct RegisterLayout {
    rows: usize,
    cols: usize,
    count: usize,
    stride: usize,
    columns: bool,
    lanes: usize,
    valid: usize,
    float: bool,
    convert: fn(u32) -> f32,
}

impl RegisterLayout {
    fn index(&self, element: usize, register: usize, lane: usize) -> usize {
        let base = element * self.rows * self.cols;
        if self.columns {
            base + lane * self.cols + register
        } else {
            base + register * self.cols + lane
        }
    }

    fn fill(&self, data: &[u8], out: &mut [f32], convert: impl Fn(u32) -> f32) {
        let step = if self.columns { self.cols * 4 } else { 4 };
        for (register, slot) in out.chunks_exact_mut(4).enumerate() {
            let element = register / self.stride;
            let register = register % self.stride;
            if register >= self.valid {
                continue;
            }
            let mut index = self.index(element, register, 0) * 4;
            for value in &mut slot[..self.lanes] {
                let word: [u8; 4] = data[index..index + 4].try_into().expect("four bytes");
                *value = convert(u32::from_le_bytes(word));
                index += step;
            }
        }
    }

    fn write(&self, data: &[u8], out: &mut [f32]) -> Result<(), RenderError> {
        out.fill(0.0);
        if let Some(last) = (0..self.count)
            .rev()
            .find(|register| register % self.stride < self.valid)
            .filter(|_| self.lanes > 0)
        {
            let max_index = self.index(last / self.stride, last % self.stride, self.lanes - 1);
            if (max_index + 1) * 4 > data.len() {
                return Err(invalid_data("shader constant data is too short"));
            }
            if self.float {
                self.fill(data, out, f32::from_bits);
            } else {
                self.fill(data, out, self.convert);
            }
        }
        Ok(())
    }
}

fn register_layout(param: &EfxParam, constant: &CtabConst) -> Result<RegisterLayout, RenderError> {
    let rows = param.type_.rows.max(1) as usize;
    let cols = param.type_.cols.max(1) as usize;
    let count = constant.count as usize;
    let stride = match constant.class {
        FXC_MATRIX_COLUMNS => constant.cols as usize,
        FXC_MATRIX_ROWS => constant.rows as usize,
        FXC_SCALAR | FXC_VECTOR => 1,
        _ => {
            return Err(unsupported(
                "BeginPass",
                "shader constant class is unsupported",
            ));
        }
    };
    if stride == 0 || count > 256 || count > stride * constant.elements.max(1) as usize {
        return Err(invalid_data("shader constant register span is invalid"));
    }
    let columns = constant.class == FXC_MATRIX_COLUMNS;
    let (lanes, valid) = if columns {
        ((constant.rows as usize).min(rows).min(4), cols)
    } else {
        ((constant.cols as usize).min(cols).min(4), rows)
    };
    let convert: fn(u32) -> f32 = match param.type_.type_ {
        FXT_FLOAT => f32::from_bits,
        FXT_INT => |word| (word as i32) as f32,
        FXT_BOOL => |word| (word != 0) as u32 as f32,
        _ if lanes == 0 || valid == 0 => |_| 0.0,
        _ => {
            return Err(unsupported(
                "BeginPass",
                "shader constant type is unsupported",
            ));
        }
    };
    Ok(RegisterLayout {
        rows,
        cols,
        count,
        stride,
        columns,
        lanes,
        valid,
        float: param.type_.type_ == FXT_FLOAT,
        convert,
    })
}

fn registers_into(
    param: &EfxParam,
    data: &[u8],
    constant: &CtabConst,
    out: &mut [f32],
) -> Result<(), RenderError> {
    register_layout(param, constant)?.write(data, out)
}

fn constant_registers_into(
    effect: &D3dxEffect,
    index: usize,
    constant: &CtabConst,
    out: &mut [f32],
) -> Result<(), RenderError> {
    let param = effect
        .params
        .get(index)
        .ok_or_else(|| invalid_data("shader constant parameter is missing"))?;
    effect
        .with_numeric(index, |data| registers_into(param, data, constant, out))
        .ok_or_else(|| invalid_data("shader constant parameter has no numeric value"))?
}

fn ctab_params(
    effect: &D3dxEffect,
    ctab: &[CtabConst],
    what: &str,
) -> Result<Vec<usize>, RenderError> {
    ctab.iter()
        .map(|constant| {
            effect
                .find_param_index(&constant.name)
                .ok_or_else(|| invalid_data(format!("{what} parameter is missing")))
        })
        .collect()
}

fn compile_preshader(
    effect: &D3dxEffect,
    code: &[u8],
) -> Result<Arc<CompiledPreshader>, RenderError> {
    let program = preshader::parse(code)?;
    let constants = parse_ctab(code)?;
    let params = ctab_params(effect, &constants, "preshader input")?;
    let outputs = program.output_registers();
    Ok(Arc::new(CompiledPreshader {
        program,
        constants,
        params,
        outputs,
    }))
}

fn run_preshader<'a>(
    effect: &D3dxEffect,
    cell: &'a OnceLock<Result<Arc<CompiledPreshader>, RenderError>>,
    code: &[u8],
) -> Result<Vec<f32>, RenderError> {
    let compiled = cell
        .get_or_init(|| compile_preshader(effect, code))
        .as_ref()
        .map_err(Clone::clone)?;
    let mut inputs = effect.preshader_inputs.borrow_mut();
    ctab_inputs_with(effect, &compiled.constants, &compiled.params, &mut inputs)?;
    preshader::evaluate(&compiled.program, &inputs)
}

fn ctab_inputs_with(
    effect: &D3dxEffect,
    ctab: &[CtabConst],
    params: &[usize],
    inputs: &mut Vec<[f32; 4]>,
) -> Result<(), RenderError> {
    inputs.clear();
    inputs.resize(256, [0.0f32; 4]);
    for (constant, &index) in ctab.iter().zip(params) {
        if constant.set != D3DXRS_FLOAT4 {
            return Err(unsupported(
                "Preshader",
                format!("input register set {} is unsupported", constant.set),
            ));
        }
        let start = constant.index as usize * 4;
        let range = start..start + constant.count as usize * 4;
        let flat = inputs.as_flattened_mut();
        if range.end > flat.len() {
            return Err(invalid_data(
                "preshader input register range is out of bounds",
            ));
        }
        constant_registers_into(effect, index, constant, &mut flat[range])?;
    }
    Ok(())
}

fn sampler_state_parameter(effect: &D3dxEffect, state: &EfxState) -> Result<usize, RenderError> {
    let name = if let Some(name) = &state.referenced {
        name
    } else {
        let id = match &state.value {
            EfxValue::Object(ids) => ids
                .first()
                .ok_or_else(|| invalid_data("set sampler state has no object id"))?,
            _ => return Err(invalid_data("set sampler state has a non-object value")),
        };
        &effect
            .objects
            .get(id)
            .ok_or_else(|| invalid_data("set sampler object is missing"))?
            .param
    };
    let index = effect
        .find_param_index(name)
        .ok_or_else(|| invalid_data("set sampler parameter is missing"))?;
    if !matches!(effect.params[index].value, EfxValue::Sampler(_)) {
        return Err(invalid_data("set sampler parameter is not a sampler"));
    }
    Ok(index)
}

fn sampler_param_states<'a>(effect: &'a D3dxEffect, index: usize) -> Option<&'a Vec<EfxState>> {
    match &effect.params[index].value {
        EfxValue::Sampler(states) => Some(states),
        _ => None,
    }
}

enum StateSink<'a> {
    Device(&'a mut D3d9Device),
    Commands(&'a mut Vec<D3dxEffectCommand>, &'a mut Vec<f32>),
}

impl StateSink<'_> {
    fn render_state(&mut self, state: u32, value: u32) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => device
                .state
                .set_render_state(state, value)
                .map_err(|_| invalid_data("render state is out of bounds")),
            StateSink::Commands(commands, values_out) => {
                commands.push(effect_command(
                    D3dxEffectCommandKind::SetRenderState,
                    0,
                    state,
                    value,
                ));
                Ok(())
            }
        }
    }

    fn texture_stage_state(
        &mut self,
        stage: u32,
        state: u32,
        value: u32,
    ) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => device
                .state
                .set_texture_stage_state(stage, state, value)
                .map_err(|_| invalid_data("texture stage state is out of bounds")),
            StateSink::Commands(commands, values_out) => {
                commands.push(effect_command(
                    D3dxEffectCommandKind::SetTextureStageState,
                    stage,
                    state,
                    value,
                ));
                Ok(())
            }
        }
    }

    fn sampler_state(&mut self, sampler: u32, state: u32, value: u32) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => device
                .state
                .set_sampler_state(sampler, state, value)
                .map_err(|_| invalid_data("sampler state is out of bounds")),
            StateSink::Commands(commands, values_out) => {
                commands.push(effect_command(
                    D3dxEffectCommandKind::SetSamplerState,
                    sampler,
                    state,
                    value,
                ));
                Ok(())
            }
        }
    }

    fn fvf(&mut self, format: u32) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => {
                if format == 0 {
                    return Ok(());
                }
                let result = crate::d3d9::d3d9_create_fvf_declaration(device, format);
                if result.status.code != StatusCode::Ok {
                    return Err(invalid_data(result.status.message));
                }
                let status = crate::d3d9::d3d9_set_fvf_binding(device, result.value, format);
                if status.code != StatusCode::Ok {
                    return Err(invalid_data(status.message));
                }
                Ok(())
            }
            StateSink::Commands(commands, values_out) => {
                commands.push(effect_command(D3dxEffectCommandKind::SetFVF, 0, 0, format));
                Ok(())
            }
        }
    }

    fn transform(&mut self, state: u32, matrix: [f32; 16]) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => device
                .state
                .set_transform(state, matrix)
                .map_err(|_| invalid_data("texture transform is out of bounds")),
            StateSink::Commands(commands, values_out) => {
                let mut command = effect_command(D3dxEffectCommandKind::SetTransform, 0, state, 0);
                command.offset = values_out.len() as u32;
                command.count = 16;
                values_out.extend_from_slice(&matrix);
                commands.push(command);
                Ok(())
            }
        }
    }

    fn texture(&mut self, stage: u32, id: u32) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => device
                .state
                .set_texture(stage, id)
                .map_err(|_| invalid_data("texture stage is out of bounds")),
            StateSink::Commands(commands, values_out) => {
                commands.push(effect_command(
                    D3dxEffectCommandKind::SetTexture,
                    stage,
                    0,
                    id,
                ));
                Ok(())
            }
        }
    }

    fn shader(&mut self, pixel: bool, handle: u32) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => {
                device.state.set_shader(pixel, handle);
                Ok(())
            }
            StateSink::Commands(commands, values_out) => {
                let kind = if pixel {
                    D3dxEffectCommandKind::SetPixelShader
                } else {
                    D3dxEffectCommandKind::SetVertexShader
                };
                commands.push(effect_command(kind, 0, 0, handle));
                Ok(())
            }
        }
    }

    fn shader_constants(
        &mut self,
        pixel: bool,
        start: u32,
        values: Cow<'_, [f32]>,
    ) -> Result<(), RenderError> {
        match self {
            StateSink::Device(device) => device
                .state
                .set_constants_f(pixel, start, &values)
                .map_err(|_| invalid_data("shader constant range is out of bounds")),
            StateSink::Commands(commands, values_out) => {
                let kind = if pixel {
                    D3dxEffectCommandKind::SetPixelShaderConstantF
                } else {
                    D3dxEffectCommandKind::SetVertexShaderConstantF
                };
                let mut command = effect_command(kind, start, 0, 0);
                command.offset = values_out.len() as u32;
                command.count = values.len() as u32;
                values_out.extend_from_slice(&values);
                commands.push(command);
                Ok(())
            }
        }
    }

    fn shader_registers(
        &mut self,
        pixel: bool,
        start: u32,
        count: u32,
        fill: impl FnOnce(&mut [f32]) -> Result<(), RenderError>,
    ) -> Result<(), RenderError> {
        let len = count as usize * 4;
        stats::record(|s| {
            s.constant_updates += 1;
            s.constant_registers += u64::from(count);
        });
        match self {
            StateSink::Device(_) => {
                stats::record(|s| s.constant_temp_bytes += (len * size_of::<f32>()) as u64);
                let mut values = vec![0.0f32; len];
                fill(&mut values)?;
                self.shader_constants(pixel, start, Cow::Owned(values))
            }
            StateSink::Commands(commands, values_out) => {
                let offset = values_out.len();
                values_out.resize(offset + len, 0.0);
                if let Err(error) = fill(&mut values_out[offset..]) {
                    values_out.truncate(offset);
                    return Err(error);
                }
                let kind = if pixel {
                    D3dxEffectCommandKind::SetPixelShaderConstantF
                } else {
                    D3dxEffectCommandKind::SetVertexShaderConstantF
                };
                let mut command = effect_command(kind, start, 0, 0);
                command.offset = offset as u32;
                command.count = len as u32;
                commands.push(command);
                Ok(())
            }
        }
    }
}

fn effect_command(
    kind: D3dxEffectCommandKind,
    index: u32,
    state: u32,
    value: u32,
) -> D3dxEffectCommand {
    D3dxEffectCommand {
        kind,
        index,
        state,
        value,
        offset: 0,
        count: 0,
    }
}

#[derive(Clone, Copy)]
struct Apply {
    since: Option<u64>,
    now: u64,
    commit: bool,
}

impl Apply {
    fn all(self) -> Self {
        Self {
            since: None,
            ..self
        }
    }
}

fn selector_code(state: &EfxState) -> Option<&[u8]> {
    let blob = state.selector.as_deref()?;
    let name_size = rd_u32(blob, 0).ok()? as usize;
    let start = 4usize.checked_add(name_size)?;
    blob.get(start..).filter(|code| !code.is_empty())
}

fn state_deps<'a>(effect: &D3dxEffect, state: &'a EfxState) -> Option<&'a [usize]> {
    state
        .deps
        .get_or_init(|| {
            let mut deps = Vec::new();
            if let Some(name) = &state.referenced {
                deps.push(effect.find_param_index(name)?);
            }
            if state.selector.is_some() && selector_code(state).is_none() {
                return None;
            }
            for code in [state.preshader.as_deref(), selector_code(state)]
                .into_iter()
                .flatten()
            {
                let ctab = parse_ctab(code).ok()?;
                deps.extend(ctab_params(effect, &ctab, "state").ok()?);
            }
            Some(deps)
        })
        .as_deref()
}

fn dirty(effect: &D3dxEffect, state: &EfxState, since: Option<u64>) -> bool {
    let Some(since) = since else {
        return true;
    };
    state_deps(effect, state).is_none_or(|deps| {
        deps.iter()
            .any(|&index| effect.param_version(index) > since)
    })
}

fn apply_sampler_state(
    effect: &D3dxEffect,
    sink: &mut StateSink,
    state: &EfxState,
    stage: u32,
    since: Option<u64>,
) -> Result<(), RenderError> {
    if !dirty(effect, state, since) {
        return Ok(());
    }
    if state.op == FXS_SAMPLER_TEXTURE {
        let id = match &state.referenced {
            Some(name) => {
                let index = effect
                    .find_param_index(name)
                    .ok_or_else(|| invalid_data("sampler texture parameter is missing"))?;
                effect.pool_object(index, 0)
            }
            None => 0,
        };
        sink.texture(stage, id)?;
        return Ok(());
    }
    if (FXS_SAMPLER_FIRST..=FXS_SAMPLER_LAST).contains(&state.op) {
        let samp = state.op - FXS_SAMPLER_FIRST + 1;
        let value = numeric_value(effect, state)?;
        sink.sampler_state(stage, samp, value)?;
        return Ok(());
    }
    Err(unsupported("BeginPass", "unsupported sampler state"))
}

fn numeric_value(effect: &D3dxEffect, state: &EfxState) -> Result<u32, RenderError> {
    if let Some(code) = &state.preshader {
        let output = run_preshader(effect, &state.preshader_program, code)?;
        let value = output.first().copied().unwrap_or(0.0);
        let input = match state.type_.type_ {
            FXT_FLOAT => FXT_FLOAT,
            FXT_BOOL => FXT_BOOL,
            _ => FXT_INT,
        };
        let word = match input {
            FXT_FLOAT => value.to_bits(),
            FXT_BOOL => (value != 0.0) as u32,
            _ => value as i32 as u32,
        };
        return Ok(word);
    }
    if let Some(name) = &state.referenced {
        let index = effect
            .find_param_index(name)
            .ok_or_else(|| invalid_data("referenced state parameter is missing"))?;
        let word = effect
            .with_numeric(index, |bytes| {
                bytes
                    .get(..4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            })
            .ok_or_else(|| invalid_data("referenced state parameter has no numeric value"))?
            .ok_or_else(|| invalid_data("referenced state parameter is too short"))?;
        return Ok(convert_word(
            effect.params[index].type_.type_,
            word,
            state.type_.type_,
        ));
    }
    match &state.value {
        EfxValue::Numeric(bytes) if bytes.len() >= 4 => {
            Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        _ => Err(invalid_data("numeric state has no value")),
    }
}

fn transform_value(effect: &D3dxEffect, state: &EfxState) -> Result<[f32; 16], RenderError> {
    let (words, type_) = if let Some(code) = &state.preshader {
        (
            run_preshader(effect, &state.preshader_program, code)?
                .into_iter()
                .map(f32::to_bits)
                .collect::<Vec<_>>(),
            &state.type_,
        )
    } else {
        let (bytes, type_) = if let Some(name) = &state.referenced {
            let index = effect
                .find_param_index(name)
                .ok_or_else(|| invalid_data("referenced transform parameter is missing"))?;
            (
                effect
                    .pool_value(index)
                    .ok_or_else(|| invalid_data("referenced transform parameter has no value"))?,
                &effect.params[index].type_,
            )
        } else {
            let EfxValue::Numeric(bytes) = &state.value else {
                return Err(invalid_data("transform state has no numeric matrix"));
            };
            (Cow::Borrowed(bytes.as_slice()), &state.type_)
        };
        (
            bytes
                .chunks_exact(4)
                .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
                .collect(),
            type_,
        )
    };
    let rows = type_.rows as usize;
    let cols = type_.cols as usize;
    if !matches!(type_.class, FXC_MATRIX_ROWS | FXC_MATRIX_COLUMNS)
        || type_.count() != 1
        || rows == 0
        || cols == 0
        || rows > 4
        || cols > 4
        || words.len() < rows * cols
    {
        return Err(invalid_data(
            "transform state did not provide a numeric matrix",
        ));
    }
    let mut matrix = [0.0f32; 16];
    if state.preshader.is_some() {
        for row in 0..rows {
            for col in 0..cols {
                let word = words.get(col * 4 + row).ok_or_else(|| {
                    invalid_data("transform preshader matrix register span is too short")
                })?;
                matrix[row * 4 + col] = f32::from_bits(*word);
            }
        }
        return Ok(matrix);
    }
    let bytes = get_matrix(&words, rows, cols, 1, type_.type_, false);
    for (value, word) in matrix.iter_mut().zip(bytes.chunks_exact(4)) {
        *value = f32::from_le_bytes([word[0], word[1], word[2], word[3]]);
    }
    Ok(matrix)
}

fn selector_index(effect: &D3dxEffect, state: &EfxState) -> Result<usize, RenderError> {
    state
        .selector
        .as_ref()
        .ok_or_else(|| invalid_data("array selector resource is missing"))?;
    let code =
        selector_code(state).ok_or_else(|| invalid_data("array selector preshader is missing"))?;
    let output = run_preshader(effect, &state.selector_program, code)?;
    let value = output
        .first()
        .copied()
        .ok_or_else(|| invalid_data("array selector has no output"))?;
    if !value.is_finite() {
        return Err(RenderError::invalid(
            "ID3DXEffect::BeginPass",
            "array selector index is not finite",
        ));
    }
    let mut index = value as i64;
    if index == -1 {
        index = 0;
    }
    let ids = D3dxEffect::referenced_array_ids(effect, state)
        .ok_or_else(|| invalid_data("array selector parameter is missing"))?;
    if index < 0 || index as usize >= ids.len() {
        return Err(RenderError::invalid(
            "ID3DXEffect::BeginPass",
            "array selector index is out of range",
        ));
    }
    Ok(index as usize)
}

fn apply_shader_constants(
    effect: &D3dxEffect,
    sink: &mut StateSink,
    pixel: bool,
    object: &EfxObject,
    ctx: Apply,
    changed: Option<u64>,
) -> Result<(), RenderError> {
    let ctab = object_ctab(object, false)?;
    let params = object
        .ctab_params
        .get_or_init(|| ctab_params(effect, ctab, "shader constant"))
        .as_ref()
        .map_err(Clone::clone)?;
    let stale = |index: usize| changed.is_none_or(|since| effect.param_version(index) > since);
    for (constant, &index) in ctab.iter().zip(params) {
        match constant.set {
            D3DXRS_FLOAT4 => {
                if stale(index) {
                    sink.shader_registers(pixel, constant.index, constant.count, |out| {
                        constant_registers_into(effect, index, constant, out)
                    })?;
                }
            }
            D3DXRS_SAMPLER => {
                if constant.count != 1 {
                    return Err(unsupported(
                        "BeginPass",
                        "shader sampler arrays are unsupported",
                    ));
                }
                let states = sampler_param_states(effect, index)
                    .ok_or_else(|| invalid_data("shader sampler parameter is missing"))?;
                let since = if changed.is_none() { None } else { ctx.since };
                for state in states {
                    apply_sampler_state(effect, sink, state, constant.index, since)?;
                }
            }
            D3DXRS_BOOL | D3DXRS_INT4 => {
                return Err(unsupported(
                    "BeginPass",
                    "boolean and integer shader constants are not implemented",
                ));
            }
            _ => {
                return Err(unsupported(
                    "BeginPass",
                    format!("shader register set {} is unsupported", constant.set),
                ));
            }
        }
    }
    let compiled = object
        .preshader
        .get_or_init(|| {
            let Some(code) = preshader::shader_preshader(&object.data)? else {
                return Ok(None);
            };
            compile_preshader(effect, code).map(Some)
        })
        .as_ref()
        .map_err(Clone::clone)?;
    if let Some(compiled) = compiled {
        if compiled.params.iter().any(|&index| stale(index)) {
            let mut inputs = effect.preshader_inputs.borrow_mut();
            ctab_inputs_with(effect, &compiled.constants, &compiled.params, &mut inputs)?;
            let values = preshader::evaluate(&compiled.program, &inputs)?;
            for &register in &compiled.outputs {
                let start = register as usize * 4;
                sink.shader_constants(pixel, register, Cow::Borrowed(&values[start..start + 4]))?;
            }
        }
    }
    object.const_version.set(ctx.now);
    Ok(())
}

fn apply_shader_state(
    effect: &D3dxEffect,
    sink: &mut StateSink,
    state: &EfxState,
    pixel: bool,
    ctx: Apply,
) -> Result<(), RenderError> {
    let id = if state.selector.is_some() {
        let index = match selector_index(effect, state) {
            Ok(index) => index,
            Err(error) => {
                if ctx.commit && error.kind == crate::RenderErrorKind::InvalidArgument {
                    return Ok(());
                }
                return Err(error);
            }
        };
        D3dxEffect::referenced_array_ids(effect, state).and_then(|ids| ids.get(index).copied())
    } else if let Some(name) = &state.referenced {
        let index = effect
            .find_param_index(name)
            .ok_or_else(|| invalid_data("referenced shader parameter is missing"))?;
        match &effect.params[index].value {
            EfxValue::Object(ids) => ids.first().copied(),
            _ => {
                return Err(invalid_data(
                    "referenced shader parameter is not a shader object",
                ));
            }
        }
    } else {
        match &state.value {
            EfxValue::Object(ids) => ids.first().copied(),
            EfxValue::Numeric(_)
                if state.type_.type_ == FXT_INT
                    && state.type_.bytes == 4
                    && numeric_value(effect, state)? == 0 =>
            {
                Some(0)
            }
            _ => return Err(invalid_data("shader state has a non-object value")),
        }
    };
    let handle = match id {
        Some(0) | None => 0,
        Some(id) => effect
            .shaders
            .get(&id)
            .map(|entry| entry.1)
            .ok_or_else(|| invalid_data("pass shader was not prepared"))?,
    };
    let changed = dirty(effect, state, ctx.since);
    if changed {
        sink.shader(pixel, handle)?;
    }
    if handle != 0 {
        if let Some(id) = id {
            if let Some(object) = effect.objects.get(&id) {
                if !object.data.is_empty() {
                    let since = (!changed).then(|| object.const_version.get());
                    apply_shader_constants(effect, sink, pixel, object, ctx, since)?;
                }
            }
        }
    }
    Ok(())
}

fn apply_state(
    effect: &D3dxEffect,
    sink: &mut StateSink,
    state: &EfxState,
    ctx: Apply,
) -> Result<(), RenderError> {
    match state.op {
        FXS_VERTEXSHADER => return apply_shader_state(effect, sink, state, false, ctx),
        FXS_PIXELSHADER => return apply_shader_state(effect, sink, state, true, ctx),
        FXS_SETSAMPLER => {
            let index = sampler_state_parameter(effect, state)?;
            let states = sampler_param_states(effect, index)
                .ok_or_else(|| invalid_data("set sampler parameter is not a sampler"))?;
            let child = if dirty(effect, state, ctx.since) {
                ctx.all()
            } else {
                ctx
            };
            for sampler_state in states {
                apply_sampler_state(effect, sink, sampler_state, state.index, child.since)?;
            }
            return Ok(());
        }
        _ => {}
    }
    if !dirty(effect, state, ctx.since) {
        return Ok(());
    }
    match state.op {
        0..=102 => {
            let rs = *RENDER_STATES
                .get(state.op as usize)
                .ok_or_else(|| invalid_data("render state operation is out of range"))?;
            let value = numeric_value(effect, state)?;
            sink.render_state(rs, value)?;
        }
        103..=120 => {
            let tss = *STAGE_STATES
                .get((state.op - 103) as usize)
                .ok_or_else(|| invalid_data("texture stage operation is out of range"))?;
            let value = numeric_value(effect, state)?;
            sink.texture_stage_state(state.index, tss, value)?;
        }
        FXS_FVF => sink.fvf(numeric_value(effect, state)?)?,
        126 => {
            let matrix = transform_value(effect, state)?;
            sink.transform(D3DTS_TEXTURE0 + state.index, matrix)?;
        }
        FXS_SAMPLER_TEXTURE => {
            let id = match &state.referenced {
                Some(name) => {
                    let index = effect
                        .find_param_index(name)
                        .ok_or_else(|| invalid_data("texture parameter is missing"))?;
                    effect.pool_object(index, 0)
                }
                None => 0,
            };
            sink.texture(state.index, id)?;
        }
        FXS_SAMPLER_FIRST..=FXS_SAMPLER_LAST => {
            let samp = state.op - FXS_SAMPLER_FIRST + 1;
            let value = numeric_value(effect, state)?;
            sink.sampler_state(state.index, samp, value)?;
        }
        _ => {
            return Err(unsupported(
                "BeginPass",
                format!("effect state operation {} is unsupported", state.op),
            ));
        }
    }
    Ok(())
}

fn sampler_state_keys(
    keys: &mut EffectKeys,
    stage: u32,
    states: &[EfxState],
) -> Result<(), RenderError> {
    for state in states {
        match state.op {
            FXS_SAMPLER_TEXTURE => keys.textures.push(stage),
            FXS_SAMPLER_FIRST..=FXS_SAMPLER_LAST => {
                keys.sampler.push((stage, state.op - FXS_SAMPLER_FIRST + 1));
            }
            _ => {
                return Err(unsupported(
                    "Begin",
                    format!("sampler state operation {} is unsupported", state.op),
                ));
            }
        }
    }
    Ok(())
}

fn shader_constant_keys(
    effect: &D3dxEffect,
    keys: &mut EffectKeys,
    pixel: bool,
    object: &EfxObject,
    save_shader: bool,
) -> Result<(), RenderError> {
    if save_shader {
        let outputs = object
            .saved_outputs
            .get_or_init(|| match preshader::shader_preshader(&object.data)? {
                Some(code) => Ok(preshader::parse(code)?.output_registers()),
                None => Ok(Vec::new()),
            })
            .as_ref()
            .map_err(Clone::clone)?;
        for &register in outputs {
            if pixel {
                keys.ps_const_f.push((register, 1));
            } else {
                keys.vs_const_f.push((register, 1));
            }
        }
    }
    for constant in object_ctab(object, true)? {
        match constant.set {
            D3DXRS_FLOAT4 => {
                if !save_shader {
                    continue;
                }
                let count = constant.count;
                if pixel {
                    keys.ps_const_f.push((constant.index, count));
                } else {
                    keys.vs_const_f.push((constant.index, count));
                }
            }
            D3DXRS_SAMPLER => {
                if let Some(index) = effect.find_param_index(&constant.name) {
                    if let Some(states) = sampler_param_states(effect, index) {
                        sampler_state_keys(keys, constant.index, states)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn shader_state_keys(
    effect: &D3dxEffect,
    keys: &mut EffectKeys,
    pixel: bool,
    state: &EfxState,
    save_shader: bool,
) -> Result<(), RenderError> {
    if pixel && save_shader {
        keys.pixel_shader = true;
    } else if save_shader {
        keys.vertex_shader = true;
    }
    if state.selector.is_some() {
        let ids = D3dxEffect::referenced_array_ids(effect, state)
            .ok_or_else(|| invalid_data("array selector references a missing shader array"))?;
        for id in ids {
            let object = effect
                .objects
                .get(id)
                .ok_or_else(|| invalid_data("array selector shader object is missing"))?;
            if !object.data.is_empty() {
                shader_constant_keys(effect, keys, pixel, object, save_shader)?;
            }
        }
        return Ok(());
    }
    if let Some(id) = D3dxEffect::state_shader_object(effect, state) {
        if let Some(object) = effect.objects.get(&id) {
            if !object.data.is_empty() {
                shader_constant_keys(effect, keys, pixel, object, save_shader)?;
            }
        }
    }
    Ok(())
}

fn touched_keys(effect: &D3dxEffect, skip_shader: bool) -> Result<EffectKeys, RenderError> {
    stats::record(|s| s.saved_key_builds += 1);
    let mut keys = EffectKeys::default();
    let Some(technique) = effect.techniques.get(effect.technique as usize) else {
        return Ok(keys);
    };
    for pass in &technique.passes {
        for state in pass.states.iter() {
            if state.op <= 102 {
                let rs = *RENDER_STATES
                    .get(state.op as usize)
                    .ok_or_else(|| invalid_data("render state operation is out of range"))?;
                keys.render.push(rs);
                continue;
            }
            if (103..=120).contains(&state.op) {
                let tss = *STAGE_STATES
                    .get((state.op - 103) as usize)
                    .ok_or_else(|| invalid_data("texture stage operation is out of range"))?;
                keys.tss.push((state.index, tss));
                continue;
            }
            match state.op {
                126 => keys.transform.push(D3DTS_TEXTURE0 + state.index),
                FXS_VERTEXSHADER => {
                    shader_state_keys(effect, &mut keys, false, state, !skip_shader)?;
                }
                FXS_PIXELSHADER => {
                    shader_state_keys(effect, &mut keys, true, state, !skip_shader)?;
                }
                FXS_SAMPLER_TEXTURE => keys.textures.push(state.index),
                FXS_SETSAMPLER => {
                    let index = sampler_state_parameter(effect, state)?;
                    let states = sampler_param_states(effect, index)
                        .ok_or_else(|| invalid_data("set sampler parameter is not a sampler"))?;
                    sampler_state_keys(&mut keys, state.index, states)?;
                }
                FXS_SAMPLER_FIRST..=FXS_SAMPLER_LAST => {
                    keys.sampler
                        .push((state.index, state.op - FXS_SAMPLER_FIRST + 1));
                }
                _ => {
                    return Err(unsupported(
                        "Begin",
                        format!("effect state operation {} is unsupported", state.op),
                    ));
                }
            }
        }
    }
    Ok(keys)
}

pub fn d3dx_effect_begin(
    effect: &mut D3dxEffect,
    device: &mut D3d9Device,
    flags: u32,
) -> ValueResult {
    stats::record(|s| s.effect_begins += 1);
    if effect.lost {
        return ValueResult {
            status: invalid_call("ID3DXEffect::Begin"),
            value: 0,
        };
    }
    if effect.started || effect.batch.is_some() {
        return ValueResult {
            status: invalid_call("ID3DXEffect::Begin"),
            value: 0,
        };
    }
    if effect.technique == u32::MAX {
        return ValueResult {
            status: invalid_call("ID3DXEffect::Begin"),
            value: 0,
        };
    }
    let Some(technique) = effect.techniques.get(effect.technique as usize) else {
        return ValueResult {
            status: invalid_call("ID3DXEffect::Begin"),
            value: 0,
        };
    };
    if let Err(error) = technique_supported(effect, technique) {
        return ValueResult {
            status: failed("ID3DXEffect::Begin", error),
            value: 0,
        };
    }
    let passes = technique.passes.len() as u32;
    let skip_state = flags & 1 != 0;
    let skip_shader = flags & 2 != 0;
    if !skip_state && !effect.manager_mode {
        let keys = match touched_keys(effect, skip_shader) {
            Ok(keys) => keys,
            Err(error) => {
                return ValueResult {
                    status: failed("ID3DXEffect::Begin", error),
                    value: 0,
                };
            }
        };
        match device.state.effect_capture(&keys) {
            Ok(snapshot) => {
                effect.saved = Some(snapshot);
            }
            Err(error) => {
                return ValueResult {
                    status: failed("ID3DXEffect::Begin", error),
                    value: 0,
                };
            }
        }
    }
    effect.begin_flags = flags;
    effect.started = true;
    effect.pass = -1;
    ValueResult {
        status: ok(),
        value: passes,
    }
}

pub fn d3dx_effect_begin_pass(
    effect: &mut D3dxEffect,
    device: &mut D3d9Device,
    index: u32,
) -> D3dxEffectRunResult {
    stats::record(|s| s.effect_begin_passes += 1);
    if effect.lost {
        return run_result(invalid_call("ID3DXEffect::BeginPass"));
    }
    if effect.batch.is_some() {
        return run_result(invalid_call("ID3DXEffect::BeginPass"));
    }
    if !effect.started || effect.pass >= 0 {
        return run_result(invalid_call("ID3DXEffect::BeginPass"));
    }
    let Some(technique) = effect.techniques.get(effect.technique as usize) else {
        return run_result(invalid_call("ID3DXEffect::BeginPass"));
    };
    let Some(pass) = technique.passes.get(index as usize) else {
        return run_result(invalid_call("ID3DXEffect::BeginPass"));
    };
    let states = pass.states.clone();
    prepare_pass(effect, device, &states, index, false)
}

pub fn d3dx_effect_commit(effect: &mut D3dxEffect, device: &mut D3d9Device) -> D3dxEffectRunResult {
    stats::record(|s| s.effect_commits += 1);
    if effect.lost {
        return run_result(invalid_call("ID3DXEffect::CommitChanges"));
    }
    if effect.batch.is_some() {
        return run_result(invalid_call("ID3DXEffect::CommitChanges"));
    }
    if !effect.started {
        return run_result(invalid_call("ID3DXEffect::CommitChanges"));
    }
    if effect.pass < 0 {
        return run_result(ok());
    }
    let index = effect.pass as u32;
    let Some(technique) = effect.techniques.get(effect.technique as usize) else {
        return run_result(invalid_call("ID3DXEffect::CommitChanges"));
    };
    let Some(pass) = technique.passes.get(index as usize) else {
        return run_result(invalid_call("ID3DXEffect::CommitChanges"));
    };
    let states = pass.states.clone();
    prepare_pass(effect, device, &states, index, true)
}

fn prepare_pass(
    effect: &mut D3dxEffect,
    device: &mut D3d9Device,
    states: &[EfxState],
    index: u32,
    commit: bool,
) -> D3dxEffectRunResult {
    let label = if commit {
        "ID3DXEffect::CommitChanges"
    } else {
        "ID3DXEffect::BeginPass"
    };
    let manager = effect.manager_mode;
    let mut commands = std::mem::take(&mut effect.spare_commands);
    let mut values = std::mem::take(&mut effect.spare_values);
    commands.clear();
    values.clear();
    let ctx = Apply {
        since: commit.then_some(effect.pass_version),
        now: next_version(),
        commit,
    };
    if manager {
        let mut sink = StateSink::Commands(&mut commands, &mut values);
        for state in states {
            if let Err(error) = apply_state(effect, &mut sink, state, ctx) {
                return run_result(failed(label, error));
            }
        }
        effect.pass_version = ctx.now;
        effect.batch = Some(if commit {
            EffectBatch::Commit
        } else {
            EffectBatch::BeginPass(index)
        });
        D3dxEffectRunResult {
            status: ok(),
            commands,
            values,
            reserved: true,
        }
    } else {
        let mut sink = StateSink::Device(device);
        for state in states {
            if let Err(error) = apply_state(effect, &mut sink, state, ctx) {
                return run_result(failed(label, error));
            }
        }
        effect.pass_version = ctx.now;
        effect.pass = index as i32;
        D3dxEffectRunResult {
            status: ok(),
            commands,
            values,
            reserved: false,
        }
    }
}

fn run_result(status: Status) -> D3dxEffectRunResult {
    D3dxEffectRunResult {
        status,
        commands: Vec::new(),
        values: Vec::new(),
        reserved: false,
    }
}

pub fn d3dx_effect_recycle(
    effect: &mut D3dxEffect,
    commands: Vec<D3dxEffectCommand>,
    values: Vec<f32>,
) {
    if commands.capacity() > effect.spare_commands.capacity() {
        effect.spare_commands = commands;
    }
    if values.capacity() > effect.spare_values.capacity() {
        effect.spare_values = values;
    }
}

pub fn d3dx_effect_batch_finish(effect: &mut D3dxEffect, success: bool) -> Status {
    match effect.batch.take() {
        Some(EffectBatch::BeginPass(index)) => {
            if success {
                effect.pass = index as i32;
            }
            ok()
        }
        Some(EffectBatch::Commit | EffectBatch::ManagerChange) => ok(),
        None => invalid_call("ID3DXEffect::BatchFinish"),
    }
}

pub fn d3dx_effect_batch_active(effect: &D3dxEffect) -> bool {
    effect.batch.is_some()
}

pub fn d3dx_effect_set_state_manager(effect: &mut D3dxEffect, active: bool) -> Status {
    if effect.batch.is_some() {
        return invalid_call("ID3DXEffect::SetStateManager");
    }
    effect.manager_mode = active;
    effect.batch = Some(EffectBatch::ManagerChange);
    if active {
        effect.saved = None;
    }
    ok()
}

pub fn d3dx_effect_end_pass(effect: &mut D3dxEffect) -> Status {
    if effect.batch.is_some() {
        return invalid_call("ID3DXEffect::EndPass");
    }
    if !effect.started || effect.pass < 0 {
        return invalid_call("ID3DXEffect::EndPass");
    }
    effect.pass = -1;
    ok()
}

pub fn d3dx_effect_end(effect: &mut D3dxEffect, device: &mut D3d9Device) -> Status {
    if effect.batch.is_some() {
        return invalid_call("ID3DXEffect::End");
    }
    if !effect.started {
        return ok();
    }
    if !effect.manager_mode && effect.begin_flags & 1 == 0 {
        if let Some(snapshot) = &effect.saved {
            snapshot.restore(&mut device.state);
        }
    }
    effect.saved = None;
    effect.started = false;
    effect.pass = -1;
    ok()
}

fn clear_lost_textures(effect: &mut D3dxEffect, device: &D3d9Device) {
    for param in &mut effect.params {
        if !param.type_.texture() {
            continue;
        }
        if let EfxValue::Object(ids) = &mut param.value {
            for id in ids.iter_mut() {
                if *id != 0 && device.resource_pool(*id) == Some(D3DPOOL_DEFAULT) {
                    *id = 0;
                    param.version = next_version();
                }
            }
        }
    }
    if let Some(pool) = &effect.pool {
        let mut state = pool.lock().unwrap();
        for entry in state.values_mut() {
            for id in &mut entry.objects {
                if *id != 0 && device.resource_pool(*id) == Some(D3DPOOL_DEFAULT) {
                    *id = 0;
                    entry.version = next_version();
                }
            }
        }
    }
}

pub fn d3dx_effect_on_lost(effect: &mut D3dxEffect, device: &mut D3d9Device) -> Status {
    if effect.batch.is_some() {
        return invalid_call("ID3DXEffect::OnLostDevice");
    }
    effect.saved = None;
    effect.started = false;
    effect.pass = -1;
    effect.lost = true;
    clear_lost_textures(effect, device);
    ok()
}

pub fn d3dx_effect_on_reset(effect: &mut D3dxEffect, _device: &mut D3d9Device) -> Status {
    if effect.batch.is_some() {
        return invalid_call("ID3DXEffect::OnResetDevice");
    }
    effect.saved = None;
    effect.started = false;
    effect.pass = -1;
    effect.lost = false;
    ok()
}

pub fn d3dx_effect_flags(effect: &D3dxEffect, handle: u32) -> u32 {
    effect
        .target(handle)
        .map(|target| target.flags)
        .unwrap_or(0)
}

pub fn d3dx_effect_pool_id_of(effect: &D3dxEffect) -> u32 {
    effect.pool_id
}

pub fn d3dx_effect_shutdown(effect: &mut D3dxEffect, _device: &mut D3d9Device) {
    effect.saved = None;
    effect.started = false;
    effect.pass = -1;
    effect.batch = None;
}
