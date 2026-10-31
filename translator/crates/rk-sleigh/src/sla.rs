use std::collections::BTreeMap;

use miniz_oxide::inflate::decompress_to_vec_zlib;
use rk_core::{Error, Result};

const ELEMENT_START: u8 = 0x40;
const ELEMENT_END: u8 = 0x80;
const ATTRIBUTE: u8 = 0xc0;
const USEROP: u32 = 25;
const USEROP_HEAD: u32 = 26;
const ID: u32 = 3;
const INDEX: u32 = 9;
const NAME: u32 = 12;

enum Value {
    Integer(u64),
    Text(String),
    Other,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let byte = *self.data.get(self.pos).ok_or_else(malformed)?;
        self.pos += 1;
        Ok(byte)
    }

    fn id(&mut self, header: u8) -> Result<u32> {
        let mut id = u32::from(header & 0x1f);
        if header & 0x20 != 0 {
            id = (id << 7) | u32::from(self.byte()? & 0x7f);
        }
        Ok(id)
    }

    fn integer(&mut self, length: u8) -> Result<u64> {
        let mut value = 0;
        for _ in 0..length {
            value = (value << 7) | u64::from(self.byte()? & 0x7f);
        }
        Ok(value)
    }

    fn value(&mut self) -> Result<Value> {
        let kind = self.byte()?;
        let length = kind & 0xf;
        Ok(match kind >> 4 {
            1 | 6 => Value::Other,
            2 | 4 => Value::Integer(self.integer(length)?),
            7 => {
                let size = usize::try_from(self.integer(length)?).map_err(|_| malformed())?;
                let end = self.pos.checked_add(size).ok_or_else(malformed)?;
                let text = self.data.get(self.pos..end).ok_or_else(malformed)?;
                self.pos = end;
                Value::Text(String::from_utf8_lossy(text).into_owned())
            }
            _ => {
                self.integer(length)?;
                Value::Other
            }
        })
    }
}

fn malformed() -> Error {
    Error::input("malformed SLA symbol table")
}

pub fn userop_names(sla: &[u8]) -> Result<BTreeMap<u64, String>> {
    let body = sla
        .strip_prefix(b"sla\x04")
        .ok_or_else(|| Error::input("unsupported SLA header"))?;
    let data = decompress_to_vec_zlib(body)
        .map_err(|e| Error::input(format!("cannot inflate SLA: {e:?}")))?;
    let mut reader = Reader {
        data: &data,
        pos: 0,
    };
    let mut open = Vec::new();
    let mut attributes = BTreeMap::new();
    let mut heads = BTreeMap::new();
    let mut indices = Vec::new();
    while reader.pos < data.len() {
        let header = reader.byte()?;
        let id = reader.id(header)?;
        match header & 0xc0 {
            ELEMENT_START => {
                open.push(id);
                attributes.clear();
            }
            ELEMENT_END => {
                let integer = |key| match attributes.get(&key) {
                    Some(Value::Integer(value)) => Some(*value),
                    _ => None,
                };
                match open.pop() {
                    Some(USEROP_HEAD) => {
                        if let (Some(symbol), Some(Value::Text(name))) =
                            (integer(ID), attributes.get(&NAME))
                        {
                            heads.insert(symbol, name.clone());
                        }
                    }
                    Some(USEROP) => indices.extend(integer(INDEX).zip(integer(ID))),
                    _ => {}
                }
                attributes.clear();
            }
            ATTRIBUTE => {
                attributes.insert(id, reader.value()?);
            }
            _ => return Err(malformed()),
        }
    }
    indices
        .into_iter()
        .map(|(index, symbol)| Ok((index, heads.get(&symbol).ok_or_else(malformed)?.clone())))
        .collect()
}
