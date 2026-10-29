use crate::bridge::{Status, StatusCode};

pub fn guest_status_hresult(status: &Status) -> u32 {
    match status.code {
        StatusCode::Ok => 0,
        StatusCode::InvalidArgument => 0x8876086c,
        StatusCode::InvalidData => 0x88760b59,
        StatusCode::OutOfMemory => 0x8007000e,
        StatusCode::WasStillDrawing => 0x8876021c,
        StatusCode::DeviceLost => 0x88760868,
        StatusCode::DeviceNotReset => 0x88760869,
        StatusCode::NoAutogen => 0x0876086f,
        _ => 0x8876086a,
    }
}
