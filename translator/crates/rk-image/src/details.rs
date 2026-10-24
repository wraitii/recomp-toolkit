use rk_core::program::PeDetails;
use rk_core::Addr;

use crate::pe::{c_string, u32_at, Headers};

const DIR_EXPORT: usize = 0;
const DIR_IMPORT: usize = 1;
const DIR_DEBUG: usize = 6;
const DIR_TLS: usize = 9;
const DIR_DELAY_IMPORT: usize = 13;

pub(crate) fn parse_headers(
    headers: &Headers<'_>,
    base: Addr,
) -> Option<std::result::Result<PeDetails, String>> {
    let preferred_base = headers.preferred_base;
    let entry_rva = headers.entry_rva()?;
    let size = headers.size;
    let original = headers.data.as_slice();

    let delta = base.wrapping_sub(preferred_base);
    let mut data_ranges = Vec::new();
    for index in 0..headers.directory_count().min(16) {
        if index == 4 {
            continue;
        }
        let (rva, len) = headers.directory(index)?;
        if rva != 0 && len != 0 {
            data_ranges.push((base + rva, base + rva + len, "PE directory"));
        }
    }
    let (debug_rva, debug_size) = headers.directory(DIR_DEBUG)?;
    let mut off = debug_rva as usize;
    let end = off + debug_size as usize;
    while debug_rva != 0 && off + 28 <= end {
        let at = u32_at(original, off + 20)?;
        let len = u32_at(original, off + 16)?;
        if at != 0 && len != 0 {
            data_ranges.push((base + at, base + at + len, "PE debug data"));
        }
        off += 28;
    }

    let exec_ranges = headers.exec_ranges(base);
    let mut import_ranges = Vec::new();
    let mut add_import = |rva: u32, len: u32| {
        if rva == 0 || len == 0 {
            return;
        }
        let low = base + rva;
        let high = low + len;
        for &(start, end) in &exec_ranges {
            if low.max(start) < high.min(end) {
                import_ranges.push((low.max(start), high.min(end), "PE import data"));
            }
        }
    };
    let (import_rva, _) = headers.directory(DIR_IMPORT)?;
    let mut descriptor = import_rva as usize;
    while import_rva != 0 && descriptor + 20 <= original.len() {
        let original_thunk = u32_at(original, descriptor)?;
        let name = u32_at(original, descriptor + 12)?;
        let first_thunk = u32_at(original, descriptor + 16)?;
        if original_thunk == 0 && name == 0 && first_thunk == 0 {
            break;
        }
        if let Some(dll) = c_string(original, name as usize) {
            add_import(name, dll.len() as u32 + 1);
        }
        let lookup = if original_thunk != 0 {
            original_thunk
        } else {
            first_thunk
        };
        let mut count = 0u32;
        loop {
            let Some(thunk) = u32_at(original, lookup as usize + count as usize * 4) else {
                break;
            };
            if thunk == 0 {
                break;
            }
            if thunk & 0x8000_0000 == 0 {
                if let Some(symbol) = c_string(original, thunk as usize + 2) {
                    add_import(thunk + 2, symbol.len() as u32 + 1);
                }
            }
            count += 1;
        }
        add_import(original_thunk, 4 * (count + 1));
        add_import(first_thunk, 4 * (count + 1));
        descriptor += 20;
    }
    let (delay_rva, _) = headers.directory(DIR_DELAY_IMPORT)?;
    let mut descriptor = delay_rva as usize;
    while delay_rva != 0 && descriptor + 32 <= original.len() {
        let name = u32_at(original, descriptor + 4)?;
        let handle = u32_at(original, descriptor + 8)?;
        let iat = u32_at(original, descriptor + 12)?;
        let int = u32_at(original, descriptor + 16)?;
        let bound = u32_at(original, descriptor + 20)?;
        let unload = u32_at(original, descriptor + 24)?;
        if name == 0 && iat == 0 && int == 0 {
            break;
        }
        if let Some(dll) = c_string(original, name as usize) {
            add_import(name, dll.len() as u32 + 1);
        }
        let lookup = if int != 0 { int } else { iat };
        let mut count = 0u32;
        loop {
            let Some(thunk) = u32_at(original, lookup as usize + count as usize * 4) else {
                break;
            };
            if thunk == 0 {
                break;
            }
            if thunk & 0x8000_0000 == 0 {
                if let Some(symbol) = c_string(original, thunk as usize + 2) {
                    add_import(thunk + 2, symbol.len() as u32 + 1);
                }
            }
            count += 1;
        }
        for array in [int, iat, bound, unload] {
            add_import(array, 4 * (count + 1));
        }
        add_import(handle, 4);
        descriptor += 32;
    }
    data_ranges.extend(import_ranges);

    let mut exports = Vec::new();
    let (export_rva, _) = headers.directory(DIR_EXPORT)?;
    if export_rva != 0 {
        let base_at = export_rva as usize;
        let count = u32_at(original, base_at + 20)?;
        let functions = u32_at(original, base_at + 28)?;
        for i in 0..count {
            let rva = u32_at(original, functions as usize + i as usize * 4)?;
            if rva != 0 {
                exports.push(base + rva);
            }
        }
    }

    let (tls_rva, _) = headers.directory(DIR_TLS)?;
    let tls_callbacks = if tls_rva != 0 {
        u32_at(original, tls_rva as usize + 12).map(|at| at.wrapping_add(delta))
    } else {
        None
    };

    Some(Ok(PeDetails {
        preferred_base,
        entry_point: base + entry_rva,
        size,
        delta,
        original: original.to_vec(),
        sections: headers.sections.clone(),
        data_ranges,
        exports,
        tls_callbacks,
    }))
}
