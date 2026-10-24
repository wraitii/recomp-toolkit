use std::collections::BTreeMap;

use rk_core::program::{Image, Import, Relocation, Section};
use rk_core::Addr;

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const DIR_IMPORT: usize = 1;
const DIR_BASERELOC: usize = 5;

pub(crate) fn u16_at(data: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(off..off + 2)?.try_into().ok()?))
}

pub(crate) fn u32_at(data: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?))
}

pub(crate) fn c_string(data: &[u8], off: usize) -> Option<String> {
    let tail = data.get(off..)?;
    let end = tail.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&tail[..end]).into_owned())
}

pub(crate) struct Headers<'a> {
    file: &'a [u8],
    opt: usize,
    directory_count: usize,
    pub preferred_base: u32,
    pub size: u32,
    pub data: Vec<u8>,
    pub sections: Vec<Section>,
}

impl<'a> Headers<'a> {
    pub(crate) fn parse(file: &'a [u8]) -> Option<Self> {
        let pe = u32_at(file, 0x3c)? as usize;
        if file.get(pe..pe + 4)? != b"PE\0\0" {
            return None;
        }
        let coff = pe + 4;
        let section_count = u16_at(file, coff + 2)? as usize;
        let optional_size = u16_at(file, coff + 16)? as usize;
        let opt = coff + 20;
        if u16_at(file, opt)? != 0x10b {
            return None;
        }
        let preferred_base = u32_at(file, opt + 28)?;
        let size = u32_at(file, opt + 56)?;
        let headers = u32_at(file, opt + 60)? as usize;
        let directory_count = u32_at(file, opt + 92)? as usize;
        let table = opt + optional_size;
        let mut sections = Vec::with_capacity(section_count);
        for i in 0..section_count {
            let at = table + i * 40;
            sections.push(Section {
                virtual_size: u32_at(file, at + 8)?,
                virtual_address: u32_at(file, at + 12)?,
                raw_size: u32_at(file, at + 16)?,
                raw_pointer: u32_at(file, at + 20)?,
                characteristics: u32_at(file, at + 36)?,
            });
        }

        let mut data = file.get(..headers.min(file.len()))?.to_vec();
        for s in &sections {
            if s.virtual_size == 0 && s.raw_size == 0 {
                continue;
            }
            let start = s.raw_pointer as usize;
            let end = start + s.raw_size as usize;
            if end > file.len() {
                continue;
            }
            data.resize(s.virtual_address as usize, 0);
            data.extend_from_slice(&file[start..end]);
        }
        data.resize(size as usize, 0);

        Some(Self {
            file,
            opt,
            directory_count,
            preferred_base,
            size,
            data,
            sections,
        })
    }

    pub(crate) fn entry_rva(&self) -> Option<u32> {
        u32_at(self.file, self.opt + 16)
    }

    pub(crate) fn directory_count(&self) -> usize {
        self.directory_count
    }

    pub(crate) fn directory(&self, index: usize) -> Option<(u32, u32)> {
        if index >= self.directory_count {
            return Some((0, 0));
        }
        Some((
            u32_at(self.file, self.opt + 96 + index * 8)?,
            u32_at(self.file, self.opt + 100 + index * 8)?,
        ))
    }

    pub(crate) fn exec_ranges(&self, base: Addr) -> Vec<(Addr, Addr)> {
        self.sections
            .iter()
            .filter(|s| s.characteristics & SCN_MEM_EXECUTE != 0)
            .map(|s| {
                let start = base + s.virtual_address;
                (start, start + s.virtual_size.max(s.raw_size))
            })
            .collect()
    }
}

pub(crate) fn parse_headers(
    mut headers: Headers<'_>,
    base: Option<Addr>,
) -> Option<std::result::Result<Image, String>> {
    let preferred_base = headers.preferred_base;
    let size = headers.size;
    let base = base.unwrap_or(preferred_base);
    let delta = base.wrapping_sub(preferred_base);
    let mut data = std::mem::take(&mut headers.data);
    let (reloc_rva, reloc_size) = headers.directory(DIR_BASERELOC)?;
    let mut relocations = Vec::new();
    let mut off = reloc_rva as usize;
    let reloc_end = off + reloc_size as usize;
    while reloc_size != 0 && off + 8 <= reloc_end {
        let page = u32_at(&data, off)? as usize;
        let block = u32_at(&data, off + 4)? as usize;
        if block < 8 || off + block > reloc_end {
            if delta != 0 {
                return Some(Err(format!("malformed relocation block at {off:x}")));
            }
            break;
        }
        for slot in (off + 8..off + block).step_by(2) {
            let entry = u16_at(&data, slot)?;
            if entry >> 12 != 3 {
                continue;
            }
            let site = page + (entry & 0xfff) as usize;
            if site + 4 > size as usize {
                if delta != 0 {
                    return Some(Err(format!("relocation outside image at {site:x}")));
                }
                continue;
            }
            let value = u32_at(&data, site)?.wrapping_add(delta);
            data[site..site + 4].copy_from_slice(&value.to_le_bytes());
            relocations.push(Relocation {
                target: value,
                site: base + site as u32,
            });
        }
        off += block;
    }
    if delta != 0 && reloc_size == 0 {
        return Some(Err("rebased image has no relocation directory".into()));
    }

    let mut imports = BTreeMap::new();
    let (import_rva, _) = headers.directory(DIR_IMPORT)?;
    let mut descriptor = import_rva as usize;
    while import_rva != 0 && descriptor + 20 <= data.len() {
        let original = u32_at(&data, descriptor)?;
        let name = u32_at(&data, descriptor + 12)?;
        let first = u32_at(&data, descriptor + 16)?;
        if original == 0 && name == 0 && first == 0 {
            break;
        }
        let dll = c_string(&data, name as usize)?.to_lowercase();
        let lookup = if original != 0 { original } else { first };
        for i in 0.. {
            let thunk = u32_at(&data, lookup as usize + i * 4)?;
            if thunk == 0 {
                break;
            }
            let symbol = if thunk & 0x8000_0000 != 0 {
                format!("ord{}", thunk & 0xffff)
            } else {
                c_string(&data, thunk as usize + 2)?
            };
            imports.insert(
                base + first + (i as u32) * 4,
                Import {
                    dll: dll.clone(),
                    name: symbol,
                },
            );
        }
        descriptor += 20;
    }

    let exec_ranges = headers.exec_ranges(base);
    Some(Ok(Image {
        base,
        size,
        data,
        exec_ranges,
        imports,
        relocations,
    }))
}
