use crate::RenderError;
use crate::bridge::{
    D3d9Rect, D3d9TextureKind, D3dxBytesResult, D3dxCreateRequest, D3dxCreateResult, D3dxImageInfo,
    D3dxImageInfoResult, D3dxSurfaceReadResult, Status, StatusCode, status,
};
use crate::d3d9::D3d9Device;
use crate::d3d9::format::{
    self, D3DFMT_A8B8G8R8, D3DFMT_A8R8G8B8, D3DFMT_DXT1, D3DFMT_DXT2, D3DFMT_DXT3, D3DFMT_DXT4,
    D3DFMT_DXT5,
};
use crate::d3d9::resource::{self, D3d9Resource, TextureKind};
use ddsfile::Dds;
use image::ImageEncoder;
use std::io::Cursor;
use std::sync::atomic::Ordering;

pub mod filter;

const D3DX_DEFAULT: u32 = 0xFFFF_FFFF;
const D3DX_DEFAULT_NONPOW2: u32 = 0xFFFF_FFFE;
const D3DX_FROM_FILE: u32 = 0xFFFF_FFFD;

const D3DFMT_L6V5U5: u32 = 61;
const D3DFMT_X8L8V8U8: u32 = 62;
const D3DFMT_A2W10V10U10: u32 = 67;
const D3DFMT_CXV8U8: u32 = 117;

const D3DXIFF_BMP: u32 = 0;
const D3DXIFF_JPG: u32 = 1;
const D3DXIFF_TGA: u32 = 2;
const D3DXIFF_PNG: u32 = 3;
const D3DXIFF_DDS: u32 = 4;
const D3DXIFF_PPM: u32 = 5;

const D3DRTYPE_TEXTURE: u32 = 3;
const D3DRTYPE_VOLUMETEXTURE: u32 = 4;
const D3DRTYPE_CUBETEXTURE: u32 = 5;

struct SourceImage {
    kind: TextureKind,
    width: u32,
    height: u32,
    depth: u32,
    levels: u32,
    format: u32,
    file_format: u32,
    resource_type: u32,
    faces: Vec<Vec<Vec<u8>>>,
}

impl SourceImage {
    fn info(&self) -> D3dxImageInfo {
        D3dxImageInfo {
            width: self.width,
            height: self.height,
            depth: self.depth,
            mip_levels: self.levels,
            format: self.format,
            resource_type: self.resource_type,
            image_file_format: self.file_format,
        }
    }

    fn level_dims(&self, level: u32) -> (u32, u32, u32) {
        format::level_dimensions(self.width, self.height, self.depth, level)
    }
}

fn ok_status() -> Status {
    status(StatusCode::Ok, "")
}

fn fail_status(error: RenderError) -> Status {
    super::bridge_error(error)
}

fn empty_info() -> D3dxImageInfo {
    D3dxImageInfo {
        width: 0,
        height: 0,
        depth: 0,
        mip_levels: 0,
        format: 0,
        resource_type: 0,
        image_file_format: 0,
    }
}

fn fail_info(error: RenderError) -> D3dxImageInfoResult {
    D3dxImageInfoResult {
        status: fail_status(error),
        info: empty_info(),
    }
}

fn fail_create(error: RenderError) -> D3dxCreateResult {
    D3dxCreateResult {
        status: fail_status(error),
        info: empty_info(),
    }
}

fn fail_bytes(error: RenderError) -> D3dxBytesResult {
    D3dxBytesResult {
        status: fail_status(error),
        bytes: Vec::new(),
    }
}

fn fail_surface_read(error: RenderError) -> D3dxSurfaceReadResult {
    D3dxSurfaceReadResult {
        status: fail_status(error),
        snapshot: Box::new(D3dxSurfaceData {
            format: 0,
            block: false,
            rect_w: 0,
            rect_h: 0,
            raw: Vec::new(),
            pixels: None,
        }),
    }
}

fn texture_kind(kind: D3d9TextureKind) -> TextureKind {
    match kind {
        D3d9TextureKind::Texture2D => TextureKind::Texture2D,
        D3d9TextureKind::Cube => TextureKind::Cube,
        D3d9TextureKind::Volume => TextureKind::Volume,
        _ => TextureKind::Texture2D,
    }
}

fn resource_type(kind: TextureKind) -> u32 {
    match kind {
        TextureKind::Texture2D => D3DRTYPE_TEXTURE,
        TextureKind::Cube => D3DRTYPE_CUBETEXTURE,
        TextureKind::Volume => D3DRTYPE_VOLUMETEXTURE,
    }
}

fn is_dds(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) == 0x2053_4444
}

struct RawDds {
    width: u32,
    height: u32,
    depth: u32,
    levels: u32,
    pitch: u32,
    is_cube: bool,
    is_volume: bool,
    format: u32,
}

fn classify_dds_format(
    flags: u32,
    fourcc: u32,
    bitcount: u32,
    r: u32,
    g: u32,
    b: u32,
    a: u32,
) -> Option<u32> {
    const FOURCC_FLAG: u32 = 0x4;
    const RGB: u32 = 0x40;
    const LUMINANCE: u32 = 0x20000;
    const BUMPLUMINANCE: u32 = 0x40000;
    const BUMPDUDV: u32 = 0x80000;
    if flags & FOURCC_FLAG != 0 {
        return match fourcc {
            0x3154_5844 => Some(D3DFMT_DXT1),
            0x3254_5844 => Some(D3DFMT_DXT2),
            0x3354_5844 => Some(D3DFMT_DXT3),
            0x3454_5844 => Some(D3DFMT_DXT4),
            0x3554_5844 => Some(D3DFMT_DXT5),
            27 => Some(format::D3DFMT_R3G3B2),
            36 => Some(format::D3DFMT_A16B16G16R16),
            60 => Some(format::D3DFMT_V8U8),
            63 => Some(format::D3DFMT_Q8W8V8U8),
            64 => Some(format::D3DFMT_V16U16),
            110 => Some(format::D3DFMT_Q16W16V16U16),
            111 => Some(format::D3DFMT_R16F),
            112 => Some(format::D3DFMT_G16R16F),
            113 => Some(format::D3DFMT_A16B16G16R16F),
            114 => Some(format::D3DFMT_R32F),
            115 => Some(format::D3DFMT_G32R32F),
            116 => Some(format::D3DFMT_A32B32G32R32F),
            117 => Some(D3DFMT_CXV8U8),
            0x3856_5843 => Some(D3DFMT_CXV8U8),
            _ => None,
        };
    }
    if flags & (BUMPDUDV | BUMPLUMINANCE) != 0 {
        return match (bitcount, r, g, b, a) {
            (16, 0x0000_00FF, 0x0000_FF00, 0, 0) => Some(format::D3DFMT_V8U8),
            (16, 0x0000_001F, 0x0000_03E0, 0x0000_7C00, 0) => Some(D3DFMT_L6V5U5),
            (32, 0x0000_00FF, 0x0000_FF00, 0x00FF_0000, 0) => Some(D3DFMT_X8L8V8U8),
            (32, 0x0000_00FF, 0x0000_FF00, 0x00FF_0000, 0xFF00_0000) => {
                Some(format::D3DFMT_Q8W8V8U8)
            }
            (32, 0x0000_FFFF, 0xFFFF_0000, 0, 0) => Some(format::D3DFMT_V16U16),
            (32, 0x0000_03FF, 0x000F_FC00, 0x3FF0_0000, 0xC000_0000) => Some(D3DFMT_A2W10V10U10),
            _ => None,
        };
    }
    if flags & LUMINANCE != 0 {
        return match (bitcount, r, a) {
            (8, 0x0000_00FF, 0) => Some(format::D3DFMT_L8),
            (16, 0x0000_00FF, 0x0000_FF00) => Some(format::D3DFMT_A8L8),
            (8, 0x0000_000F, 0x0000_00F0) => Some(format::D3DFMT_A4L4),
            _ => None,
        };
    }
    if flags & RGB != 0 {
        return match (bitcount, r, g, b, a) {
            (32, 0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000) => {
                Some(format::D3DFMT_A8R8G8B8)
            }
            (32, 0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0) => Some(format::D3DFMT_X8R8G8B8),
            (32, 0x0000_00FF, 0x0000_FF00, 0x00FF_0000, 0xFF00_0000) => {
                Some(format::D3DFMT_A8B8G8R8)
            }
            (32, 0x0000_00FF, 0x0000_FF00, 0x00FF_0000, 0) => Some(format::D3DFMT_X8B8G8R8),
            (32, 0x0000_03FF, 0x000F_FC00, 0x3FF0_0000, 0xC000_0000) => {
                Some(format::D3DFMT_A2B10G10R10)
            }
            (32, 0x3FF0_0000, 0x000F_FC00, 0x0000_03FF, 0xC000_0000) => {
                Some(format::D3DFMT_A2R10G10B10)
            }
            (24, 0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0) => Some(format::D3DFMT_R8G8B8),
            (16, 0x0000_7C00, 0x0000_03E0, 0x0000_001F, 0) => Some(format::D3DFMT_X1R5G5B5),
            (16, 0x0000_7C00, 0x0000_03E0, 0x0000_001F, 0x0000_8000) => {
                Some(format::D3DFMT_A1R5G5B5)
            }
            (16, 0x0000_F800, 0x0000_07E0, 0x0000_001F, 0) => Some(format::D3DFMT_R5G6B5),
            (16, 0x0000_0F00, 0x0000_00F0, 0x0000_000F, 0) => Some(format::D3DFMT_X4R4G4B4),
            (16, 0x0000_0F00, 0x0000_00F0, 0x0000_000F, 0x0000_F000) => {
                Some(format::D3DFMT_A4R4G4B4)
            }
            (8, 0x0000_00E0, 0x0000_001C, 0x0000_0003, 0) => Some(format::D3DFMT_R3G3B2),
            (16, 0x0000_00E0, 0x0000_001C, 0x0000_0003, 0x0000_FF00) => {
                Some(format::D3DFMT_A8R3G3B2)
            }
            _ => None,
        };
    }
    if bitcount == 8 && a == 0x0000_00FF {
        return Some(format::D3DFMT_A8);
    }
    None
}

fn parse_raw_dds(op: &'static str, bytes: &[u8]) -> Result<RawDds, RenderError> {
    if bytes.len() < 128 {
        return Err(RenderError::invalid_data(op, "DDS header is truncated"));
    }
    let dword = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    if dword(4) != 124 {
        return Err(RenderError::invalid_data(
            op,
            "DDS header size field is not 124",
        ));
    }
    const REQUIRED_FLAGS: u32 = 0x1 | 0x2 | 0x4 | 0x1000;
    let header_flags = dword(8);
    if header_flags & REQUIRED_FLAGS != REQUIRED_FLAGS {
        return Err(RenderError::invalid_data(
            op,
            "DDS header is missing required flags",
        ));
    }
    if dword(76) != 32 {
        return Err(RenderError::invalid_data(
            op,
            "DDS pixel format size is not 32",
        ));
    }
    let height = dword(12);
    let width = dword(16);
    let caps2 = dword(112);
    let is_cube = caps2 & 0x200 != 0;
    let is_volume = caps2 & 0x20_0000 != 0;
    if is_cube && is_volume {
        return Err(RenderError::invalid_data(
            op,
            "DDS declares both cube and volume",
        ));
    }
    if is_cube && caps2 & 0xfc00 != 0xfc00 {
        return Err(RenderError::invalid_data(
            op,
            "DDS cube map does not declare all six faces",
        ));
    }
    let depth = if is_volume { dword(24).max(1) } else { 1 };
    let levels = dword(28).max(1);
    let flags = dword(80);
    let fourcc = dword(84);
    let bitcount = dword(88);
    let r = dword(92);
    let g = dword(96);
    let b = dword(100);
    let a = dword(104);
    let format = classify_dds_format(flags, fourcc, bitcount, r, g, b, a)
        .ok_or_else(|| RenderError::new(op, "unsupported DDS pixel format"))?;
    let pitch = if header_flags & 0x8 != 0 && !format::is_block(format) {
        dword(20)
    } else {
        0
    };
    Ok(RawDds {
        width,
        height,
        depth,
        levels,
        pitch,
        is_cube,
        is_volume,
        format,
    })
}

fn validate_dds_layout(op: &'static str, raw: &RawDds) -> Result<(), RenderError> {
    if raw.width == 0 || raw.height == 0 || raw.depth == 0 {
        return Err(RenderError::invalid_data(op, "DDS has a zero dimension"));
    }
    let full = format::full_level_count(raw.width, raw.height, raw.depth);
    if raw.levels == 0 || raw.levels > full {
        return Err(RenderError::invalid_data(op, "DDS mip count is invalid"));
    }
    if raw.is_cube && (raw.width != raw.height || raw.depth != 1) {
        return Err(RenderError::invalid_data(
            op,
            "cube DDS has mismatched dimensions",
        ));
    }
    Ok(())
}

fn image_file_format(format: image::ImageFormat) -> u32 {
    match format {
        image::ImageFormat::Bmp => D3DXIFF_BMP,
        image::ImageFormat::Jpeg => D3DXIFF_JPG,
        image::ImageFormat::Tga => D3DXIFF_TGA,
        image::ImageFormat::Png => D3DXIFF_PNG,
        image::ImageFormat::Dds => D3DXIFF_DDS,
        _ => D3DXIFF_BMP,
    }
}

fn dds_info(bytes: &[u8]) -> Result<D3dxImageInfo, RenderError> {
    let op = "D3DXGetImageInfoFromFileInMemory";
    let raw = parse_raw_dds(op, bytes)?;
    validate_dds_layout(op, &raw)?;
    let kind = if raw.is_cube {
        TextureKind::Cube
    } else if raw.is_volume {
        TextureKind::Volume
    } else {
        TextureKind::Texture2D
    };
    Ok(D3dxImageInfo {
        width: raw.width,
        height: raw.height,
        depth: raw.depth,
        mip_levels: raw.levels,
        format: raw.format,
        resource_type: resource_type(kind),
        image_file_format: D3DXIFF_DDS,
    })
}

fn extract_tight(
    op: &'static str,
    data: &[u8],
    offset: usize,
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    src_pitch: u32,
) -> Result<(Vec<u8>, usize), RenderError> {
    let tight_pitch = format::row_pitch(format, w)
        .ok_or_else(|| RenderError::invalid_data(op, "DDS row pitch overflow"))?;
    let rows = if format::is_block(format) {
        format::block_rows(h)
    } else {
        h
    };
    if src_pitch == 0 || src_pitch == tight_pitch {
        let size = (tight_pitch as usize)
            .checked_mul(rows as usize)
            .and_then(|v| v.checked_mul(d as usize))
            .ok_or_else(|| RenderError::invalid_data(op, "DDS level size overflow"))?;
        let end = offset
            .checked_add(size)
            .ok_or_else(|| RenderError::invalid_data(op, "DDS level offset overflow"))?;
        let slice = data.get(offset..end).ok_or_else(|| {
            RenderError::invalid_data(op, "DDS data is shorter than the header layout")
        })?;
        return Ok((slice.to_vec(), size));
    }
    if src_pitch < tight_pitch {
        return Err(RenderError::invalid_data(
            op,
            "DDS row pitch is smaller than the format requires",
        ));
    }
    let mut out = Vec::new();
    out.try_reserve_exact((tight_pitch as usize) * (rows as usize) * (d as usize))
        .map_err(|_| RenderError::out_of_memory(op, "DDS row padding allocation failed"))?;
    for row in 0..(rows as usize) * (d as usize) {
        let start = offset
            .checked_add(row * src_pitch as usize)
            .ok_or_else(|| RenderError::invalid_data(op, "DDS padded row overflow"))?;
        let end = start
            .checked_add(tight_pitch as usize)
            .ok_or_else(|| RenderError::invalid_data(op, "DDS padded row overflow"))?;
        let row_bytes = data
            .get(start..end)
            .ok_or_else(|| RenderError::invalid_data(op, "DDS padded row is out of bounds"))?;
        out.extend_from_slice(row_bytes);
    }
    let consumed = (src_pitch as usize) * (rows as usize) * (d as usize);
    Ok((out, consumed))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LowerPitch {
    Tight,
    Dword,
}

fn round4_pitch(op: &'static str, pitch: u32) -> Result<u32, RenderError> {
    pitch
        .checked_add(3)
        .map(|value| value & !3)
        .ok_or_else(|| RenderError::invalid_data(op, "DDS aligned row pitch overflow"))
}

fn dds_level_consumed(
    op: &'static str,
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    src_pitch: u32,
) -> Result<usize, RenderError> {
    let tight = format::row_pitch(format, w)
        .ok_or_else(|| RenderError::invalid_data(op, "DDS row pitch overflow"))?;
    let effective = if src_pitch == 0 {
        tight
    } else if src_pitch < tight {
        return Err(RenderError::invalid_data(
            op,
            "DDS row pitch is smaller than the format requires",
        ));
    } else {
        src_pitch
    };
    let rows = if format::is_block(format) {
        format::block_rows(h)
    } else {
        h
    };
    (effective as usize)
        .checked_mul(rows as usize)
        .and_then(|v| v.checked_mul(d as usize))
        .ok_or_else(|| RenderError::invalid_data(op, "DDS level size overflow"))
}

fn dds_expected_size(
    op: &'static str,
    raw: &RawDds,
    faces: u32,
    lower: LowerPitch,
) -> Result<usize, RenderError> {
    let mut total = 0usize;
    for _ in 0..faces {
        for level in 0..raw.levels {
            let (w, h, d) = format::level_dimensions(raw.width, raw.height, raw.depth, level);
            let tight = format::row_pitch(raw.format, w)
                .ok_or_else(|| RenderError::invalid_data(op, "DDS row pitch overflow"))?;
            let src = if level == 0 {
                raw.pitch
            } else {
                match lower {
                    LowerPitch::Tight => 0,
                    LowerPitch::Dword => round4_pitch(op, tight)?,
                }
            };
            total = total
                .checked_add(dds_level_consumed(op, raw.format, w, h, d, src)?)
                .ok_or_else(|| RenderError::invalid_data(op, "DDS total size overflow"))?;
        }
    }
    Ok(total)
}

fn dds_lower_pitch(
    op: &'static str,
    raw: &RawDds,
    level: u32,
    lower: LowerPitch,
) -> Result<u32, RenderError> {
    if level == 0 {
        return Ok(raw.pitch);
    }
    match lower {
        LowerPitch::Tight => Ok(0),
        LowerPitch::Dword => {
            let (w, _, _) = format::level_dimensions(raw.width, raw.height, raw.depth, level);
            let tight = format::row_pitch(raw.format, w)
                .ok_or_else(|| RenderError::invalid_data(op, "DDS row pitch overflow"))?;
            round4_pitch(op, tight)
        }
    }
}

fn decode_dds(op: &'static str, bytes: &[u8]) -> Result<SourceImage, RenderError> {
    let raw = parse_raw_dds(op, bytes)?;
    validate_dds_layout(op, &raw)?;
    let dds = Dds::read(Cursor::new(bytes))
        .map_err(|e| RenderError::invalid_data(op, format!("DDS parse failed: {e}")))?;
    let kind = if raw.is_cube {
        TextureKind::Cube
    } else if raw.is_volume {
        TextureKind::Volume
    } else {
        TextureKind::Texture2D
    };
    let faces = if raw.is_cube { 6 } else { 1 };
    let data_len = dds.data.len();
    let lower = if format::is_block(raw.format) {
        let expected = dds_expected_size(op, &raw, faces, LowerPitch::Tight)?;
        if data_len != expected {
            return Err(RenderError::invalid_data(
                op,
                format!("DDS block data is {data_len} bytes but the layout requires {expected}"),
            ));
        }
        LowerPitch::Tight
    } else {
        let tight = dds_expected_size(op, &raw, faces, LowerPitch::Tight)?;
        let dword = dds_expected_size(op, &raw, faces, LowerPitch::Dword)?;
        if data_len == tight {
            LowerPitch::Tight
        } else if data_len == dword {
            LowerPitch::Dword
        } else if data_len < tight {
            return Err(RenderError::invalid_data(
                op,
                format!("DDS data is {data_len} bytes but the layout requires at least {tight}"),
            ));
        } else {
            return Err(RenderError::new(
                op,
                format!(
                    "DDS data is {data_len} bytes for {}x{}x{} with {} levels, format {:#x}, pitch {}; only tight ({tight}) and DWORD-mip ({dword}) layouts are supported",
                    raw.width, raw.height, raw.depth, raw.levels, raw.format, raw.pitch
                ),
            ));
        }
    };
    let mut out_faces: Vec<Vec<Vec<u8>>> = Vec::with_capacity(faces as usize);
    let mut offset = 0usize;
    for _ in 0..faces {
        let mut face_levels = Vec::with_capacity(raw.levels as usize);
        for level in 0..raw.levels {
            let (lw, lh, ld) = format::level_dimensions(raw.width, raw.height, raw.depth, level);
            let src_pitch = dds_lower_pitch(op, &raw, level, lower)?;
            let (tight, consumed) =
                extract_tight(op, &dds.data, offset, raw.format, lw, lh, ld, src_pitch)?;
            offset = offset
                .checked_add(consumed)
                .ok_or_else(|| RenderError::invalid_data(op, "DDS offset overflow"))?;
            face_levels.push(tight);
        }
        out_faces.push(face_levels);
    }
    if offset != data_len {
        return Err(RenderError::invalid_data(
            op,
            "DDS data does not match the selected row-pitch layout",
        ));
    }
    Ok(SourceImage {
        kind,
        width: raw.width,
        height: raw.height,
        depth: raw.depth,
        levels: raw.levels,
        format: raw.format,
        file_format: D3DXIFF_DDS,
        resource_type: resource_type(kind),
        faces: out_faces,
    })
}

fn decode_raster(op: &'static str, bytes: &[u8]) -> Result<SourceImage, RenderError> {
    if let Ok(guessed) = image::guess_format(bytes) {
        let decoded = image::load_from_memory_with_format(bytes, guessed)
            .map_err(|e| raster_decode_error(op, e))?;
        return finish_raster(op, decoded, guessed);
    }
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Tga)
        .map_err(|e| raster_decode_error(op, e))?;
    finish_raster(op, decoded, image::ImageFormat::Tga)
}

fn raster_decode_error(op: &'static str, error: image::ImageError) -> RenderError {
    match error {
        image::ImageError::Unsupported(inner) => {
            RenderError::new(op, format!("unsupported image format: {inner}"))
        }
        image::ImageError::Limits(inner) => {
            RenderError::out_of_memory(op, format!("image allocation limit: {inner}"))
        }
        other => RenderError::invalid_data(op, format!("image decode failed: {other}")),
    }
}

fn finish_raster(
    op: &'static str,
    decoded: image::DynamicImage,
    guessed: image::ImageFormat,
) -> Result<SourceImage, RenderError> {
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err(RenderError::invalid_data(op, "image has zero dimensions"));
    }
    Ok(SourceImage {
        kind: TextureKind::Texture2D,
        width,
        height,
        depth: 1,
        levels: 1,
        format: D3DFMT_A8B8G8R8,
        file_format: image_file_format(guessed),
        resource_type: D3DRTYPE_TEXTURE,
        faces: vec![vec![rgba.into_raw()]],
    })
}

fn decode_source(op: &'static str, bytes: &[u8]) -> Result<SourceImage, RenderError> {
    if is_dds(bytes) {
        decode_dds(op, bytes)
    } else {
        decode_raster(op, bytes)
    }
}

pub fn d3d9_d3dx_image_info(bytes: &[u8]) -> D3dxImageInfoResult {
    if is_dds(bytes) {
        return match dds_info(bytes) {
            Ok(info) => D3dxImageInfoResult {
                status: ok_status(),
                info,
            },
            Err(error) => fail_info(error),
        };
    }
    match decode_raster("D3DXGetImageInfoFromFileInMemory", bytes) {
        Ok(source) => D3dxImageInfoResult {
            status: ok_status(),
            info: source.info(),
        },
        Err(error) => fail_info(error),
    }
}

fn dds_skip(mip_filter: u32) -> u32 {
    if mip_filter == D3DX_DEFAULT {
        0
    } else {
        (mip_filter >> 26) & 0x1F
    }
}

fn resolve_dimension(value: u32, source: u32) -> Result<u32, RenderError> {
    match value {
        0 | D3DX_DEFAULT => source.checked_next_power_of_two().ok_or_else(|| {
            RenderError::invalid("D3DXCreateTexture", "texture dimension overflows")
        }),
        D3DX_DEFAULT_NONPOW2 | D3DX_FROM_FILE => Ok(source),
        other => Ok(other),
    }
}

fn resolve_target_format(requested: u32, source: &SourceImage) -> u32 {
    if requested == 0 || requested == D3DX_DEFAULT || requested == D3DX_FROM_FILE {
        if source.file_format == D3DXIFF_DDS {
            source.format
        } else {
            D3DFMT_A8R8G8B8
        }
    } else {
        requested
    }
}

fn level_storage(
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    kind: TextureKind,
) -> Result<u32, RenderError> {
    if matches!(kind, TextureKind::Volume) {
        format::volume_size(format, w, h, d)
            .map(|(_, _, total)| total)
            .ok_or_else(|| RenderError::invalid("D3DX", "volume level layout overflow"))
    } else {
        format::level_size(format, w, h)
            .ok_or_else(|| RenderError::invalid("D3DX", "texture level layout overflow"))
    }
}

fn block_slice_size(format: u32, w: u32, h: u32) -> Result<usize, RenderError> {
    format::level_size(format, w, h)
        .map(|size| size as usize)
        .ok_or_else(|| RenderError::invalid("D3DX", "block slice layout overflow"))
}

fn decode_bc(
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    bytes: &[u8],
) -> Result<Vec<[f32; 4]>, RenderError> {
    let block = format::block_bytes(format) as usize;
    if block == 0 || w == 0 || h == 0 || d == 0 {
        return Err(RenderError::invalid(
            "D3DX",
            "invalid block-compressed level",
        ));
    }
    let blocks_w = w.div_ceil(4);
    let blocks_h = h.div_ceil(4);
    let slice = block_slice_size(format, w, h)?;
    let mut out = vec![[0.0f32; 4]; (w as usize) * (h as usize) * (d as usize)];
    for z in 0..d {
        let start = (z as usize) * slice;
        let end = start
            .checked_add(slice)
            .ok_or_else(|| RenderError::invalid("D3DX", "block slice offset overflow"))?;
        if end > bytes.len() {
            return Err(RenderError::invalid(
                "D3DX",
                "compressed level is shorter than its layout",
            ));
        }
        let slice_bytes = &bytes[start..end];
        for by in 0..blocks_h {
            for bx in 0..blocks_w {
                let offset = ((by * blocks_w + bx) as usize) * block;
                let mut rgba = [0u8; 64];
                let src = &slice_bytes[offset..offset + block];
                match format {
                    D3DFMT_DXT1 => bcdec_rs::bc1(src, &mut rgba, 16),
                    D3DFMT_DXT2 | D3DFMT_DXT3 => bcdec_rs::bc2(src, &mut rgba, 16),
                    D3DFMT_DXT4 | D3DFMT_DXT5 => bcdec_rs::bc3(src, &mut rgba, 16),
                    _ => return Err(RenderError::new("D3DX", "unsupported block format")),
                }
                let premultiplied = matches!(format, D3DFMT_DXT2 | D3DFMT_DXT4);
                for py in 0..4u32 {
                    for px in 0..4u32 {
                        let x = bx * 4 + px;
                        let y = by * 4 + py;
                        if x < w && y < h {
                            let base = ((py * 4 + px) as usize) * 4;
                            let mut r = rgba[base] as f32 / 255.0;
                            let mut g = rgba[base + 1] as f32 / 255.0;
                            let mut b = rgba[base + 2] as f32 / 255.0;
                            let a = rgba[base + 3] as f32 / 255.0;
                            if premultiplied && a > 0.0 {
                                r /= a;
                                g /= a;
                                b /= a;
                            }
                            let index =
                                (z as usize) * (w as usize) * (h as usize) + (y * w + x) as usize;
                            out[index] = [r, g, b, a];
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn encode_bc(
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    pixels: &[[f32; 4]],
) -> Result<Vec<u8>, RenderError> {
    let compressed = match format {
        D3DFMT_DXT1 => texpresso::Format::Bc1,
        D3DFMT_DXT2 | D3DFMT_DXT3 => texpresso::Format::Bc2,
        D3DFMT_DXT4 | D3DFMT_DXT5 => texpresso::Format::Bc3,
        _ => return Err(RenderError::new("D3DX", "unsupported block format")),
    };
    let premultiplied = matches!(format, D3DFMT_DXT2 | D3DFMT_DXT4);
    let slice = format::level_size(format, w, h)
        .ok_or_else(|| RenderError::invalid("D3DX", "block slice layout overflow"))?
        as usize;
    let mut out = Vec::with_capacity(slice * d as usize);
    for z in 0..d {
        let base = (z as usize) * (w as usize) * (h as usize);
        let mut rgba8 = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for px in &pixels[base..base + (w as usize) * (h as usize)] {
            let a = px[3].clamp(0.0, 1.0);
            let (mut r, mut g, mut b) = (
                px[0].clamp(0.0, 1.0),
                px[1].clamp(0.0, 1.0),
                px[2].clamp(0.0, 1.0),
            );
            if premultiplied {
                r *= a;
                g *= a;
                b *= a;
            }
            rgba8.push((r * 255.0 + 0.5) as u8);
            rgba8.push((g * 255.0 + 0.5) as u8);
            rgba8.push((b * 255.0 + 0.5) as u8);
            rgba8.push((a * 255.0 + 0.5) as u8);
        }
        let mut block_out = vec![0u8; slice];
        compressed.compress(
            &rgba8,
            w as usize,
            h as usize,
            texpresso::Params::default(),
            &mut block_out,
        );
        out.extend_from_slice(&block_out);
    }
    Ok(out)
}

fn decode_level_f32(
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    bytes: &[u8],
) -> Result<Vec<[f32; 4]>, RenderError> {
    if format::is_block(format) {
        return decode_bc(format, w, h, d, bytes);
    }
    format::to_rgba_f32(format, w, h, d, bytes)
}

fn dither_step(format: u32) -> Option<(f32, f32, f32)> {
    Some(match format {
        format::D3DFMT_R5G6B5 => (1.0 / 31.0, 1.0 / 63.0, 1.0 / 31.0),
        format::D3DFMT_X1R5G5B5 | format::D3DFMT_A1R5G5B5 => (1.0 / 31.0, 1.0 / 31.0, 1.0 / 31.0),
        format::D3DFMT_A4R4G4B4 | format::D3DFMT_X4R4G4B4 => (1.0 / 15.0, 1.0 / 15.0, 1.0 / 15.0),
        format::D3DFMT_R3G3B2 => (1.0 / 7.0, 1.0 / 7.0, 1.0 / 3.0),
        format::D3DFMT_A8R3G3B2 => (1.0 / 7.0, 1.0 / 7.0, 1.0 / 3.0),
        format::D3DFMT_A2B10G10R10 | format::D3DFMT_A2R10G10B10 => {
            (1.0 / 1023.0, 1.0 / 1023.0, 1.0 / 1023.0)
        }
        _ => return None,
    })
}

fn dither_pixels_at(
    pixels: &[[f32; 4]],
    format: u32,
    w: u32,
    x0: u32,
    y0: u32,
    dither: bool,
) -> Vec<[f32; 4]> {
    if !dither {
        return pixels.to_vec();
    }
    let Some((sr, sg, sb)) = dither_step(format) else {
        return pixels.to_vec();
    };
    let mut out = pixels.to_vec();
    for (index, px) in out.iter_mut().enumerate() {
        let x = (index as u32) % w.max(1) + x0;
        let y = (index as u32) / w.max(1) + y0;
        let noise = bayer4(x, y);
        px[0] = (px[0] + (noise - 0.5) * sr).clamp(0.0, 1.0);
        px[1] = (px[1] + (noise - 0.5) * sg).clamp(0.0, 1.0);
        px[2] = (px[2] + (noise - 0.5) * sb).clamp(0.0, 1.0);
    }
    out
}

fn encode_level_f32(
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    pixels: &[[f32; 4]],
    dither: bool,
) -> Result<Vec<u8>, RenderError> {
    encode_level_f32_at(format, w, h, d, pixels, dither, 0, 0)
}

fn encode_level_f32_at(
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    pixels: &[[f32; 4]],
    dither: bool,
    x0: u32,
    y0: u32,
) -> Result<Vec<u8>, RenderError> {
    let pixels = dither_pixels_at(pixels, format, w, x0, y0, dither);
    if format::is_block(format) {
        return encode_bc(format, w, h, d, &pixels);
    }
    format::from_rgba_f32(format, w, h, d, pixels.as_slice())
}

fn bayer4(x: u32, y: u32) -> f32 {
    const BAYER: [[f32; 4]; 4] = [
        [0.0, 8.0, 2.0, 10.0],
        [12.0, 4.0, 14.0, 6.0],
        [3.0, 11.0, 1.0, 9.0],
        [15.0, 7.0, 13.0, 5.0],
    ];
    BAYER[(y % 4) as usize][(x % 4) as usize] / 16.0
}

fn apply_color_key(pixels: &mut [[f32; 4]], key: u32) {
    if key == 0 {
        return;
    }
    let kr = ((key >> 16) & 0xFF) as u8 as f32 / 255.0;
    let kg = ((key >> 8) & 0xFF) as u8 as f32 / 255.0;
    let kb = (key & 0xFF) as u8 as f32 / 255.0;
    let ka = ((key >> 24) & 0xFF) as u8 as f32 / 255.0;
    for px in pixels.iter_mut() {
        if (px[0] - kr).abs() < 0.5 / 255.0
            && (px[1] - kg).abs() < 0.5 / 255.0
            && (px[2] - kb).abs() < 0.5 / 255.0
            && (px[3] - ka).abs() < 0.5 / 255.0
        {
            *px = [0.0, 0.0, 0.0, 0.0];
        }
    }
}

fn resolve_rect(
    op: &'static str,
    rect: &D3d9Rect,
    present: bool,
    w: u32,
    h: u32,
) -> Result<(u32, u32, u32, u32), RenderError> {
    if !present {
        return Ok((0, 0, w, h));
    }
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return Err(RenderError::invalid(op, "empty or inverted rectangle"));
    }
    if rect.left < 0 || rect.top < 0 || rect.right as u32 > w || rect.bottom as u32 > h {
        return Err(RenderError::invalid(op, "rectangle is outside the surface"));
    }
    let (x0, y0, x1, y1) = (
        rect.left as u32,
        rect.top as u32,
        rect.right as u32,
        rect.bottom as u32,
    );
    Ok((x0, y0, x1 - x0, y1 - y0))
}

fn validate_color_level(
    resource: &D3d9Resource,
    face: u32,
    level: u32,
    op: &'static str,
) -> Result<(), RenderError> {
    let (format, _, _, _) = resource.subresource_layout(face, level)?;
    if format::is_depth(format) {
        return Err(RenderError::new(
            op,
            "depth formats are not supported in D3DX color paths",
        ));
    }
    if let Some((_, _, _, _, msaa)) = resource.surface_shape() {
        if msaa != 0 {
            return Err(RenderError::new(
                op,
                "multisampled surfaces are not supported",
            ));
        }
    }
    let handle = resource.level_lock_handle(face, level)?;
    if handle.load(Ordering::Acquire) != 0 {
        return Err(RenderError::new(op, "surface is locked"));
    }
    Ok(())
}

fn crop_pixels(pixels: &[[f32; 4]], w: u32, x0: u32, y0: u32, cw: u32, ch: u32) -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity((cw as usize) * (ch as usize));
    for y in 0..ch {
        let row = ((y0 + y) * w + x0) as usize;
        out.extend_from_slice(&pixels[row..row + cw as usize]);
    }
    out
}

pub fn d3d9_d3dx_create_texture(
    device: &D3d9Device,
    resource: &mut D3d9Resource,
    request: &D3dxCreateRequest,
    bytes: &[u8],
) -> D3dxCreateResult {
    if resource.initialized() {
        return fail_create(RenderError::invalid(
            "D3DXCreateTexture",
            "output resource is already initialized",
        ));
    }
    let source = match decode_source("D3DXCreateTexture", bytes) {
        Ok(source) => source,
        Err(error) => return fail_create(error),
    };
    let kind = texture_kind(request.kind);
    let kind_matches = match kind {
        TextureKind::Texture2D => matches!(source.kind, TextureKind::Texture2D),
        TextureKind::Cube => matches!(source.kind, TextureKind::Cube),
        TextureKind::Volume => {
            matches!(source.kind, TextureKind::Volume | TextureKind::Texture2D)
        }
    };
    if !kind_matches {
        return fail_create(RenderError::new(
            "D3DXCreateTexture",
            "requested texture kind does not match the source image kind",
        ));
    }
    let is_dds = source.file_format == D3DXIFF_DDS;
    let skip = if is_dds {
        dds_skip(request.mip_filter)
    } else {
        0
    };
    if skip >= source.levels && source.levels > 0 {
        return fail_create(RenderError::invalid(
            "D3DXCreateTexture",
            "D3DX_SKIP_DDS_MIP_LEVELS skips past the file's mip chain",
        ));
    }
    let (source_top_w, source_top_h, source_top_d) = source.level_dims(skip);
    let available_levels = source.levels.saturating_sub(skip).max(1);
    let target_format = resolve_target_format(request.format, &source);
    if !format::is_supported(target_format) {
        return fail_create(RenderError::new(
            "D3DXCreateTexture",
            format!("unsupported texture D3DFORMAT {}", target_format),
        ));
    }
    let (base_w, base_h, base_d) = match kind {
        TextureKind::Texture2D => (
            match resolve_dimension(request.width, source_top_w) {
                Ok(v) => v,
                Err(error) => return fail_create(error),
            },
            match resolve_dimension(request.height, source_top_h) {
                Ok(v) => v,
                Err(error) => return fail_create(error),
            },
            1,
        ),
        TextureKind::Cube => {
            let edge = match resolve_dimension(request.width, source_top_w) {
                Ok(v) => v,
                Err(error) => return fail_create(error),
            };
            (edge, edge, 1)
        }
        TextureKind::Volume => (
            match resolve_dimension(request.width, source_top_w) {
                Ok(v) => v,
                Err(error) => return fail_create(error),
            },
            match resolve_dimension(request.height, source_top_h) {
                Ok(v) => v,
                Err(error) => return fail_create(error),
            },
            match resolve_dimension(request.depth, source_top_d.max(1)) {
                Ok(v) => v,
                Err(error) => return fail_create(error),
            },
        ),
    };
    let full_levels = format::full_level_count(base_w, base_h, base_d);
    let levels = match request.levels {
        0 | D3DX_DEFAULT => full_levels,
        D3DX_FROM_FILE => available_levels,
        other => other,
    };
    if base_w == 0 || base_h == 0 || base_d == 0 || levels == 0 || levels > full_levels {
        return fail_create(RenderError::invalid(
            "D3DXCreateTexture",
            "invalid texture dimensions or mip level count",
        ));
    }
    let validated_filter = match filter::validate(request.filter, filter::CREATION_DEFAULT, false) {
        Ok(flags) => flags,
        Err(error) => return fail_create(error),
    };
    let validated_mip = match filter::validate(request.mip_filter, filter::MIP_DEFAULT, is_dds) {
        Ok(flags) => flags,
        Err(error) => return fail_create(error),
    };
    let dither = validated_filter & filter::DITHER != 0;
    let mip_dither = validated_mip & filter::DITHER != 0;
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return fail_create(error),
    };
    let mapped = resource::TextureRequest {
        kind,
        width: base_w,
        height: base_h,
        depth: base_d,
        levels,
        usage: request.usage,
        format: target_format,
        pool: request.pool,
        shared_handle: false,
    };
    let mut temp = D3d9Resource::default();
    if let Err(error) = resource::create_texture(&gpu, &mut temp, &mapped) {
        return fail_create(error);
    }
    let faces = match kind {
        TextureKind::Cube => 6,
        _ => 1,
    };
    for face in 0..faces {
        let mut previous: Option<(Vec<[f32; 4]>, u32, u32, u32)> = None;
        for level in 0..levels {
            let (lw, lh, ld) = format::level_dimensions(base_w, base_h, base_d, level);
            let size = match level_storage(target_format, lw, lh, ld, kind) {
                Ok(size) => size as usize,
                Err(error) => return fail_create(error),
            };
            let source_index = skip + level;
            let source_bytes = if source_index < source.levels {
                source
                    .faces
                    .get(face as usize)
                    .and_then(|levels| levels.get(source_index as usize))
            } else {
                None
            };
            let encoded = if let Some(exact) = source_bytes {
                let (sw, sh, sd) = source.level_dims(source_index);
                if source.format == target_format
                    && sw == lw
                    && sh == lh
                    && sd == ld
                    && request.color_key == 0
                    && validated_filter & filter::SRGB == 0
                    && exact.len() == size
                {
                    if level + 1 < levels && source_index + 1 >= source.levels {
                        match decode_level_f32(source.format, sw, sh, sd, exact) {
                            Ok(pixels) => previous = Some((pixels, lw, lh, ld)),
                            Err(error) => return fail_create(error),
                        }
                    }
                    exact.clone()
                } else {
                    let work = match decode_level_f32(source.format, sw, sh, sd, exact) {
                        Ok(mut pixels) => {
                            apply_color_key(&mut pixels, request.color_key);
                            match filter::resize(&pixels, sw, sh, sd, lw, lh, ld, validated_filter)
                            {
                                Ok(resized) => resized,
                                Err(error) => return fail_create(error),
                            }
                        }
                        Err(error) => return fail_create(error),
                    };
                    previous = Some((work.clone(), lw, lh, ld));
                    match encode_level_f32(target_format, lw, lh, ld, &work, dither) {
                        Ok(encoded) => encoded,
                        Err(error) => return fail_create(error),
                    }
                }
            } else if let Some((previous_pixels, pw, ph, pd)) = previous.take() {
                let work =
                    match filter::resize(&previous_pixels, pw, ph, pd, lw, lh, ld, validated_mip) {
                        Ok(resized) => resized,
                        Err(error) => return fail_create(error),
                    };
                previous = Some((work.clone(), lw, lh, ld));
                match encode_level_f32(target_format, lw, lh, ld, &work, mip_dither) {
                    Ok(encoded) => encoded,
                    Err(error) => return fail_create(error),
                }
            } else {
                return fail_create(RenderError::new(
                    "D3DXCreateTexture",
                    "no source data for a requested mip level",
                ));
            };
            if encoded.len() != size {
                return fail_create(RenderError::new(
                    "D3DXCreateTexture",
                    "encoded level size does not match the target layout",
                ));
            }
            if let Err(error) = temp.write_subresource(face, level, &encoded) {
                return fail_create(error);
            }
            if let Err(error) = resource::upload_level(&gpu, &mut temp, face, level) {
                return fail_create(error);
            }
        }
    }
    *resource = temp;
    D3dxCreateResult {
        status: ok_status(),
        info: source.info(),
    }
}

pub struct D3dxSurfaceData {
    format: u32,
    block: bool,
    rect_w: u32,
    rect_h: u32,
    raw: Vec<u8>,
    pixels: Option<Vec<[f32; 4]>>,
}

fn block_aligned(x0: u32, y0: u32, cw: u32, ch: u32, w: u32, h: u32) -> bool {
    cw > 0
        && ch > 0
        && x0 % 4 == 0
        && y0 % 4 == 0
        && ((x0 + cw) % 4 == 0 || x0 + cw == w)
        && ((y0 + ch) % 4 == 0 || y0 + ch == h)
}

fn extract_blocks(
    bytes: &[u8],
    w: u32,
    format: u32,
    x0: u32,
    y0: u32,
    cw: u32,
    ch: u32,
) -> Vec<u8> {
    let block = format::block_bytes(format) as usize;
    let blocks_w = w.div_ceil(4);
    let bx = x0 / 4;
    let by = y0 / 4;
    let bw = (x0 + cw).div_ceil(4) - bx;
    let bh = (y0 + ch).div_ceil(4) - by;
    let mut out = Vec::with_capacity((bw as usize) * (bh as usize) * block);
    for row in 0..bh {
        let start = (((by + row) * blocks_w + bx) as usize) * block;
        out.extend_from_slice(&bytes[start..start + (bw as usize) * block]);
    }
    out
}

fn patch_blocks(
    level: &mut [u8],
    w: u32,
    format: u32,
    x0: u32,
    y0: u32,
    cw: u32,
    ch: u32,
    raw: &[u8],
) -> Result<(), RenderError> {
    let block = format::block_bytes(format) as usize;
    let blocks_w = w.div_ceil(4);
    let bx = x0 / 4;
    let by = y0 / 4;
    let bw = (x0 + cw).div_ceil(4) - bx;
    let bh = (y0 + ch).div_ceil(4) - by;
    if raw.len() != (bw as usize) * (bh as usize) * block {
        return Err(RenderError::invalid(
            "D3DXLoadSurfaceFromSurface",
            "block rect does not match its bytes",
        ));
    }
    for row in 0..bh {
        let start = (((by + row) * blocks_w + bx) as usize) * block;
        let src = (row as usize) * (bw as usize) * block;
        level[start..start + (bw as usize) * block]
            .copy_from_slice(&raw[src..src + (bw as usize) * block]);
    }
    Ok(())
}

fn extract_rows(bytes: &[u8], format: u32, w: u32, x0: u32, y0: u32, cw: u32, ch: u32) -> Vec<u8> {
    let bpp = format::bpp(format) as usize;
    let mut out = Vec::with_capacity((cw as usize) * (ch as usize) * bpp);
    for row in 0..ch {
        let start = (((y0 + row) * w + x0) as usize) * bpp;
        out.extend_from_slice(&bytes[start..start + (cw as usize) * bpp]);
    }
    out
}

fn patch_rows(
    level: &mut [u8],
    format: u32,
    w: u32,
    x0: u32,
    y0: u32,
    cw: u32,
    ch: u32,
    raw: &[u8],
) -> Result<(), RenderError> {
    let bpp = format::bpp(format) as usize;
    if raw.len() != (cw as usize) * (ch as usize) * bpp {
        return Err(RenderError::invalid(
            "D3DXLoadSurfaceFromSurface",
            "row rect does not match its bytes",
        ));
    }
    for row in 0..ch {
        let start = (((y0 + row) * w + x0) as usize) * bpp;
        let src = (row as usize) * (cw as usize) * bpp;
        level[start..start + (cw as usize) * bpp]
            .copy_from_slice(&raw[src..src + (cw as usize) * bpp]);
    }
    Ok(())
}

pub fn d3d9_d3dx_surface_read(
    device: &mut D3d9Device,
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    rect: &D3d9Rect,
    rect_present: bool,
    color_key: u32,
) -> D3dxSurfaceReadResult {
    if let Err(error) = validate_color_level(resource, face, level, "D3DXLoadSurfaceFromSurface") {
        return fail_surface_read(error);
    }
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return fail_surface_read(error),
    };
    if let Err(error) = super::flush_draws_with_gpu(device, &gpu) {
        return fail_surface_read(error);
    }
    if let Err(error) = resource::readback_level(&gpu, resource, face, level) {
        return fail_surface_read(error);
    }
    let (format, w, h, _d) = match resource.subresource_layout(face, level) {
        Ok(layout) => layout,
        Err(error) => return fail_surface_read(error),
    };
    let bytes = match resource::level_bytes(resource, face, level) {
        Ok(bytes) => bytes,
        Err(error) => return fail_surface_read(error),
    };
    let (x0, y0, cw, ch) =
        match resolve_rect("D3DXLoadSurfaceFromSurface", rect, rect_present, w, h) {
            Ok(region) => region,
            Err(error) => return fail_surface_read(error),
        };
    let block = format::is_block(format);
    let aligned = block && block_aligned(x0, y0, cw, ch, w, h);
    let mut snapshot = D3dxSurfaceData {
        format,
        block,
        rect_w: cw,
        rect_h: ch,
        raw: Vec::new(),
        pixels: None,
    };
    if color_key == 0 && (!block || aligned) {
        snapshot.raw = if block {
            extract_blocks(bytes, w, format, x0, y0, cw, ch)
        } else {
            extract_rows(bytes, format, w, x0, y0, cw, ch)
        };
    } else {
        let pixels = match decode_level_f32(format, w, h, 1, bytes) {
            Ok(pixels) => pixels,
            Err(error) => return fail_surface_read(error),
        };
        let mut cropped = crop_pixels(&pixels, w, x0, y0, cw, ch);
        apply_color_key(&mut cropped, color_key);
        snapshot.pixels = Some(cropped);
    }
    D3dxSurfaceReadResult {
        status: ok_status(),
        snapshot: Box::new(snapshot),
    }
}

pub fn d3d9_d3dx_surface_store(
    device: &mut D3d9Device,
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    rect: &D3d9Rect,
    rect_present: bool,
    snapshot: &D3dxSurfaceData,
    filter_flags: u32,
) -> Status {
    if let Err(error) = validate_color_level(resource, face, level, "D3DXLoadSurfaceFromSurface") {
        return fail_status(error);
    }
    let validated = match filter::validate(filter_flags, filter::CREATION_DEFAULT, false) {
        Ok(flags) => flags,
        Err(error) => return fail_status(error),
    };
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return fail_status(error),
    };
    if let Err(error) = super::flush_draws_with_gpu(device, &gpu) {
        return fail_status(error);
    }
    if let Err(error) = resource::readback_level(&gpu, resource, face, level) {
        return fail_status(error);
    }
    let (format, w, h, _d) = match resource.subresource_layout(face, level) {
        Ok(layout) => layout,
        Err(error) => return fail_status(error),
    };
    let (x0, y0, cw, ch) =
        match resolve_rect("D3DXLoadSurfaceFromSurface", rect, rect_present, w, h) {
            Ok(region) => region,
            Err(error) => return fail_status(error),
        };
    let existing = match resource::level_bytes(resource, face, level) {
        Ok(bytes) => bytes.to_vec(),
        Err(error) => return fail_status(error),
    };
    let srgb = validated & filter::SRGB;
    let exact_bytes = snapshot.pixels.is_none()
        && snapshot.format == format
        && snapshot.rect_w == cw
        && snapshot.rect_h == ch
        && validated & filter::MIRROR == 0
        && (srgb == 0 || srgb == filter::SRGB);
    if exact_bytes && (!snapshot.block || block_aligned(x0, y0, cw, ch, w, h)) {
        let mut patched = existing;
        let result = if snapshot.block {
            patch_blocks(&mut patched, w, format, x0, y0, cw, ch, &snapshot.raw)
        } else {
            patch_rows(&mut patched, format, w, x0, y0, cw, ch, &snapshot.raw)
        };
        if let Err(error) = result {
            return fail_status(error);
        }
        if let Err(error) = resource.write_subresource(face, level, &patched) {
            return fail_status(error);
        }
        return match resource::upload_level(&gpu, resource, face, level) {
            Ok(()) => ok_status(),
            Err(error) => fail_status(error),
        };
    }
    let source_pixels = match &snapshot.pixels {
        Some(pixels) => pixels.clone(),
        None => {
            match decode_level_f32(
                snapshot.format,
                snapshot.rect_w,
                snapshot.rect_h,
                1,
                &snapshot.raw,
            ) {
                Ok(pixels) => pixels,
                Err(error) => return fail_status(error),
            }
        }
    };
    let resized = match filter::resize(
        &source_pixels,
        snapshot.rect_w,
        snapshot.rect_h,
        1,
        cw,
        ch,
        1,
        validated,
    ) {
        Ok(resized) => resized,
        Err(error) => return fail_status(error),
    };
    let dither = validated & filter::DITHER != 0;
    let mut write_pixels = resized;
    let (wx, wy, ww, wh) = if format::is_block(format) && !block_aligned(x0, y0, cw, ch, w, h) {
        let left = x0 & !3;
        let top = y0 & !3;
        let width = (x0 + cw).div_ceil(4).saturating_mul(4).min(w) - left;
        let height = (y0 + ch).div_ceil(4).saturating_mul(4).min(h) - top;
        let raw = extract_blocks(&existing, w, format, left, top, width, height);
        let mut pixels = match decode_level_f32(format, width, height, 1, &raw) {
            Ok(pixels) => pixels,
            Err(error) => return fail_status(error),
        };
        for y in 0..ch {
            let dst = ((y0 - top + y) * width + x0 - left) as usize;
            let src = (y * cw) as usize;
            pixels[dst..dst + cw as usize].copy_from_slice(&write_pixels[src..src + cw as usize]);
        }
        write_pixels = pixels;
        (left, top, width, height)
    } else {
        (x0, y0, cw, ch)
    };
    let encoded = match encode_level_f32_at(format, ww, wh, 1, &write_pixels, dither, wx, wy) {
        Ok(encoded) => encoded,
        Err(error) => return fail_status(error),
    };
    let mut patched = existing;
    let result = if format::is_block(format) {
        patch_blocks(&mut patched, w, format, wx, wy, ww, wh, &encoded)
    } else {
        patch_rows(&mut patched, format, w, x0, y0, cw, ch, &encoded)
    };
    if let Err(error) = result {
        return fail_status(error);
    }
    if let Err(error) = resource.write_subresource(face, level, &patched) {
        return fail_status(error);
    }
    match resource::upload_level(&gpu, resource, face, level) {
        Ok(()) => ok_status(),
        Err(error) => fail_status(error),
    }
}

fn validate_save_format(op: &'static str, format: u32) -> Result<(), RenderError> {
    match format {
        D3DXIFF_BMP | D3DXIFF_JPG | D3DXIFF_PNG | D3DXIFF_DDS | D3DXIFF_TGA | D3DXIFF_PPM => Ok(()),
        other => Err(RenderError::new(
            op,
            format!("unsupported D3DXIMAGE_FILEFORMAT {other}"),
        )),
    }
}

fn encode_image(format: u32, w: u32, h: u32, pixels: &[[f32; 4]]) -> Result<Vec<u8>, RenderError> {
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for px in pixels {
        for c in px {
            rgba.push((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
    }
    if format == D3DXIFF_PPM {
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for px in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
        let mut cursor = Cursor::new(Vec::new());
        image::codecs::pnm::PnmEncoder::new(&mut cursor)
            .with_subtype(image::codecs::pnm::PnmSubtype::Pixmap(
                image::codecs::pnm::SampleEncoding::Binary,
            ))
            .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .map_err(|e| RenderError::new("D3DX", format!("image encode failed: {e}")))?;
        return Ok(cursor.into_inner());
    }
    let image = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| RenderError::invalid("D3DX", "image dimensions overflow"))?;
    let dynamic = image::DynamicImage::ImageRgba8(image);
    let image_format = match format {
        D3DXIFF_BMP => image::ImageFormat::Bmp,
        D3DXIFF_JPG => image::ImageFormat::Jpeg,
        D3DXIFF_TGA => image::ImageFormat::Tga,
        D3DXIFF_PNG => image::ImageFormat::Png,
        _ => return Err(RenderError::new("D3DX", "unsupported image output format")),
    };
    let mut cursor = Cursor::new(Vec::new());
    dynamic
        .write_to(&mut cursor, image_format)
        .map_err(|e| RenderError::new("D3DX", format!("image encode failed: {e}")))?;
    Ok(cursor.into_inner())
}

fn dds_pixel_format(format: u32) -> Option<(u32, u32, u32, u32, u32, u32, u32)> {
    const ALPHAPIXELS: u32 = 0x1;
    const FOURCC: u32 = 0x4;
    const RGB: u32 = 0x40;
    const LUMINANCE: u32 = 0x20000;
    const BUMPDUDV: u32 = 0x80000;
    Some(match format {
        D3DFMT_DXT1 => (FOURCC, 0x3154_5844, 0, 0, 0, 0, 0),
        D3DFMT_DXT2 => (FOURCC, 0x3254_5844, 0, 0, 0, 0, 0),
        D3DFMT_DXT3 => (FOURCC, 0x3354_5844, 0, 0, 0, 0, 0),
        D3DFMT_DXT4 => (FOURCC, 0x3454_5844, 0, 0, 0, 0, 0),
        D3DFMT_DXT5 => (FOURCC, 0x3554_5844, 0, 0, 0, 0, 0),
        format::D3DFMT_A16B16G16R16 => (FOURCC, 36, 0, 0, 0, 0, 0),
        format::D3DFMT_Q16W16V16U16 => (FOURCC, 110, 0, 0, 0, 0, 0),
        format::D3DFMT_R16F => (FOURCC, 111, 0, 0, 0, 0, 0),
        format::D3DFMT_G16R16F => (FOURCC, 112, 0, 0, 0, 0, 0),
        format::D3DFMT_A16B16G16R16F => (FOURCC, 113, 0, 0, 0, 0, 0),
        format::D3DFMT_R32F => (FOURCC, 114, 0, 0, 0, 0, 0),
        format::D3DFMT_G32R32F => (FOURCC, 115, 0, 0, 0, 0, 0),
        format::D3DFMT_A32B32G32R32F => (FOURCC, 116, 0, 0, 0, 0, 0),
        format::D3DFMT_A8R8G8B8 => (
            RGB | ALPHAPIXELS,
            0,
            32,
            0x00FF_0000,
            0x0000_FF00,
            0x0000_00FF,
            0xFF00_0000,
        ),
        format::D3DFMT_X8R8G8B8 => (RGB, 0, 32, 0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0),
        format::D3DFMT_A8B8G8R8 => (
            RGB | ALPHAPIXELS,
            0,
            32,
            0x0000_00FF,
            0x0000_FF00,
            0x00FF_0000,
            0xFF00_0000,
        ),
        format::D3DFMT_X8B8G8R8 => (RGB, 0, 32, 0x0000_00FF, 0x0000_FF00, 0x00FF_0000, 0),
        format::D3DFMT_R8G8B8 => (RGB, 0, 24, 0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0),
        format::D3DFMT_R5G6B5 => (RGB, 0, 16, 0x0000_F800, 0x0000_07E0, 0x0000_001F, 0),
        format::D3DFMT_X1R5G5B5 => (RGB, 0, 16, 0x0000_7C00, 0x0000_03E0, 0x0000_001F, 0),
        format::D3DFMT_A1R5G5B5 => (
            RGB | ALPHAPIXELS,
            0,
            16,
            0x0000_7C00,
            0x0000_03E0,
            0x0000_001F,
            0x0000_8000,
        ),
        format::D3DFMT_A4R4G4B4 => (
            RGB | ALPHAPIXELS,
            0,
            16,
            0x0000_0F00,
            0x0000_00F0,
            0x0000_000F,
            0x0000_F000,
        ),
        format::D3DFMT_X4R4G4B4 => (RGB, 0, 16, 0x0000_0F00, 0x0000_00F0, 0x0000_000F, 0),
        format::D3DFMT_R3G3B2 => (RGB, 0, 8, 0x0000_00E0, 0x0000_001C, 0x0000_0003, 0),
        format::D3DFMT_A8R3G3B2 => (
            RGB | ALPHAPIXELS,
            0,
            16,
            0x0000_00E0,
            0x0000_001C,
            0x0000_0003,
            0x0000_FF00,
        ),
        format::D3DFMT_A2B10G10R10 => (
            RGB | ALPHAPIXELS,
            0,
            32,
            0x0000_03FF,
            0x000F_FC00,
            0x3FF0_0000,
            0xC000_0000,
        ),
        format::D3DFMT_A2R10G10B10 => (
            RGB | ALPHAPIXELS,
            0,
            32,
            0x3FF0_0000,
            0x000F_FC00,
            0x0000_03FF,
            0xC000_0000,
        ),
        format::D3DFMT_A8 => (ALPHAPIXELS, 0, 8, 0, 0, 0, 0x0000_00FF),
        format::D3DFMT_L8 => (LUMINANCE, 0, 8, 0x0000_00FF, 0, 0, 0),
        format::D3DFMT_A8L8 => (
            LUMINANCE | ALPHAPIXELS,
            0,
            16,
            0x0000_00FF,
            0,
            0,
            0x0000_FF00,
        ),
        format::D3DFMT_A4L4 => (
            LUMINANCE | ALPHAPIXELS,
            0,
            8,
            0x0000_000F,
            0,
            0,
            0x0000_00F0,
        ),
        format::D3DFMT_V8U8 => (BUMPDUDV, 0, 16, 0x0000_00FF, 0x0000_FF00, 0, 0),
        format::D3DFMT_Q8W8V8U8 => (
            BUMPDUDV,
            0,
            32,
            0x0000_00FF,
            0x0000_FF00,
            0x00FF_0000,
            0xFF00_0000,
        ),
        format::D3DFMT_V16U16 => (BUMPDUDV, 0, 32, 0x0000_FFFF, 0xFFFF_0000, 0, 0),
        _ => return None,
    })
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn encode_dds(
    kind: TextureKind,
    format: u32,
    w: u32,
    h: u32,
    d: u32,
    levels: u32,
    level_bytes: &[Vec<u8>],
) -> Result<Vec<u8>, RenderError> {
    let (pf_flags, fourcc, bitcount, r, g, b, a) = dds_pixel_format(format)
        .ok_or_else(|| RenderError::new("D3DXSaveTextureToFile", "unsupported DDS format"))?;
    let block = format::is_block(format);
    let top = if block {
        format::level_size(format, w, h)
    } else {
        format::row_pitch(format, w)
    }
    .ok_or_else(|| RenderError::new("D3DXSaveTextureToFile", "DDS layout overflow"))?;
    let is_cube = matches!(kind, TextureKind::Cube);
    let is_volume = matches!(kind, TextureKind::Volume);
    let mut flags = 0x1u32 | 0x2 | 0x4 | 0x1000;
    if levels > 1 {
        flags |= 0x20000;
    }
    flags |= if block { 0x80000 } else { 0x8 };
    let mut caps1 = 0x1000u32;
    if levels > 1 || is_cube {
        caps1 |= 0x8;
    }
    let caps2 = if is_cube {
        0x200u32 | 0x400 | 0x800 | 0x1000 | 0x2000 | 0x4000 | 0x8000
    } else if is_volume {
        0x20_0000
    } else {
        0
    };
    let mut out = Vec::with_capacity(128 + level_bytes.iter().map(Vec::len).sum::<usize>());
    out.extend_from_slice(b"DDS ");
    push_u32(&mut out, 124);
    push_u32(&mut out, flags);
    push_u32(&mut out, h);
    push_u32(&mut out, w);
    push_u32(&mut out, top);
    push_u32(&mut out, if is_volume { d } else { 0 });
    push_u32(&mut out, levels);
    for _ in 0..11 {
        push_u32(&mut out, 0);
    }
    push_u32(&mut out, 32);
    push_u32(&mut out, pf_flags);
    push_u32(&mut out, fourcc);
    push_u32(&mut out, bitcount);
    push_u32(&mut out, r);
    push_u32(&mut out, g);
    push_u32(&mut out, b);
    push_u32(&mut out, a);
    push_u32(&mut out, caps1);
    push_u32(&mut out, caps2);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    for bytes in level_bytes {
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

pub fn d3d9_d3dx_save_surface(
    device: &mut D3d9Device,
    resource: &mut D3d9Resource,
    face: u32,
    level: u32,
    rect: &D3d9Rect,
    rect_present: bool,
    format: u32,
) -> D3dxBytesResult {
    if let Err(error) = validate_save_format("D3DXSaveSurfaceToFile", format) {
        return fail_bytes(error);
    }
    if let Err(error) = validate_color_level(resource, face, level, "D3DXSaveSurfaceToFile") {
        return fail_bytes(error);
    }
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return fail_bytes(error),
    };
    if let Err(error) = super::flush_draws_with_gpu(device, &gpu) {
        return fail_bytes(error);
    }
    if let Err(error) = resource::readback_level(&gpu, resource, face, level) {
        return fail_bytes(error);
    }
    let (source_format, w, h, d) = match resource.subresource_layout(face, level) {
        Ok(layout) => layout,
        Err(error) => return fail_bytes(error),
    };
    let (x0, y0, cw, ch) = match resolve_rect("D3DXSaveSurfaceToFile", rect, rect_present, w, h) {
        Ok(region) => region,
        Err(error) => return fail_bytes(error),
    };
    let bytes = match resource::level_bytes(resource, face, level) {
        Ok(bytes) => bytes,
        Err(error) => return fail_bytes(error),
    };
    if format == D3DXIFF_DDS {
        if d != 1 {
            return fail_bytes(RenderError::new(
                "D3DXSaveSurfaceToFile",
                "DDS save of a volume subresource is not supported",
            ));
        }
        let block = format::is_block(source_format);
        let extracted = if block {
            if block_aligned(x0, y0, cw, ch, w, h) {
                extract_blocks(bytes, w, source_format, x0, y0, cw, ch)
            } else {
                let pixels = match decode_level_f32(source_format, w, h, 1, bytes) {
                    Ok(pixels) => pixels,
                    Err(error) => return fail_bytes(error),
                };
                let cropped = crop_pixels(&pixels, w, x0, y0, cw, ch);
                match encode_level_f32(source_format, cw, ch, 1, &cropped, false) {
                    Ok(encoded) => encoded,
                    Err(error) => return fail_bytes(error),
                }
            }
        } else {
            extract_rows(bytes, source_format, w, x0, y0, cw, ch)
        };
        return match encode_dds(
            TextureKind::Texture2D,
            source_format,
            cw,
            ch,
            1,
            1,
            &[extracted],
        ) {
            Ok(bytes) => D3dxBytesResult {
                status: ok_status(),
                bytes,
            },
            Err(error) => fail_bytes(error),
        };
    }
    let pixels = match decode_level_f32(source_format, w, h, d, bytes) {
        Ok(pixels) => pixels,
        Err(error) => return fail_bytes(error),
    };
    let cropped = crop_pixels(&pixels, w, x0, y0, cw, ch);
    match encode_image(format, cw, ch, &cropped) {
        Ok(bytes) => D3dxBytesResult {
            status: ok_status(),
            bytes,
        },
        Err(error) => fail_bytes(error),
    }
}

pub fn d3d9_d3dx_save_texture(
    device: &mut D3d9Device,
    resource: &mut D3d9Resource,
    format: u32,
) -> D3dxBytesResult {
    if let Err(error) = validate_save_format("D3DXSaveTextureToFile", format) {
        return fail_bytes(error);
    }
    let gpu = match device.resource_gpu() {
        Ok(gpu) => gpu.clone(),
        Err(error) => return fail_bytes(error),
    };
    if let Err(error) = super::flush_draws_with_gpu(device, &gpu) {
        return fail_bytes(error);
    }
    let (kind, source_format, w, h, d, levels) = match resource.texture_shape() {
        Some(shape) => shape,
        None => {
            return fail_bytes(RenderError::invalid(
                "D3DXSaveTextureToFile",
                "resource is not a texture",
            ));
        }
    };
    if format != D3DXIFF_DDS {
        if matches!(kind, TextureKind::Volume) {
            return fail_bytes(RenderError::new(
                "D3DXSaveTextureToFile",
                "non-DDS volume save is not supported",
            ));
        }
        if let Err(error) = resource::readback_level(&gpu, resource, 0, 0) {
            return fail_bytes(error);
        }
        let (rf, rw, rh, rd) = match resource.subresource_layout(0, 0) {
            Ok(layout) => layout,
            Err(error) => return fail_bytes(error),
        };
        let bytes = match resource::level_bytes(resource, 0, 0) {
            Ok(bytes) => bytes,
            Err(error) => return fail_bytes(error),
        };
        let pixels = match decode_level_f32(rf, rw, rh, rd, bytes) {
            Ok(pixels) => pixels,
            Err(error) => return fail_bytes(error),
        };
        return match encode_image(format, rw, rh, &pixels) {
            Ok(bytes) => D3dxBytesResult {
                status: ok_status(),
                bytes,
            },
            Err(error) => fail_bytes(error),
        };
    }
    let faces = match kind {
        TextureKind::Cube => 6,
        _ => 1,
    };
    let mut ordered: Vec<Vec<u8>> = Vec::new();
    if matches!(kind, TextureKind::Volume) {
        for level in 0..levels {
            if let Err(error) = resource::readback_level(&gpu, resource, 0, level) {
                return fail_bytes(error);
            }
            let slice = match resource::level_bytes(resource, 0, level) {
                Ok(bytes) => bytes.to_vec(),
                Err(error) => return fail_bytes(error),
            };
            ordered.push(slice);
        }
    } else {
        for face in 0..faces {
            for level in 0..levels {
                if let Err(error) = resource::readback_level(&gpu, resource, face, level) {
                    return fail_bytes(error);
                }
                let slice = match resource::level_bytes(resource, face, level) {
                    Ok(bytes) => bytes.to_vec(),
                    Err(error) => return fail_bytes(error),
                };
                ordered.push(slice);
            }
        }
    }
    match encode_dds(kind, source_format, w, h, d, levels, &ordered) {
        Ok(bytes) => D3dxBytesResult {
            status: ok_status(),
            bytes,
        },
        Err(error) => fail_bytes(error),
    }
}
