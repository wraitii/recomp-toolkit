use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::abi::Config;
use super::ir::Instruction;

static NEXT_NAME: AtomicU64 = AtomicU64::new(0);
static WRITER: OnceLock<Mutex<Option<Writer>>> = OnceLock::new();

pub struct Function<'a> {
    pub name: &'a str,
    pub pc: u32,
    pub instructions: &'a [Instruction],
    pub locations: &'a [(u32, u32)],
    pub code: &'a [u8],
}

struct Writer {
    pid: u32,
    file: File,
    source: File,
    source_path: String,
    next_line: u32,
    next_index: u64,
}

fn u32_bytes(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn u64_bytes(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

impl Writer {
    fn new(timestamp: u64, pid: u32) -> io::Result<Self> {
        let arch = if cfg!(target_arch = "aarch64") {
            183
        } else if cfg!(target_arch = "x86_64") {
            62
        } else {
            return Err(io::Error::other("unsupported host architecture"));
        };
        let path = std::env::temp_dir().join(format!("jit-{pid}-{timestamp}.dump"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let source_path = path.with_extension("pcode");
        let source = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&source_path)?;
        let source_path = source_path.to_string_lossy().into_owned();
        let mut header = Vec::with_capacity(40);
        for value in [0x4a695444, 1, 40, arch, 0, pid] {
            u32_bytes(&mut header, value);
        }
        u64_bytes(&mut header, timestamp);
        u64_bytes(&mut header, 0);
        file.write_all(&header)?;
        eprintln!("baseline jitdump: {}", path.display());
        Ok(Self {
            pid,
            file,
            source,
            source_path,
            next_line: 1,
            next_index: 0,
        })
    }

    fn record(&mut self, function: &Function, timestamp: u64, tid: u32) -> io::Result<()> {
        let Function {
            name,
            pc,
            instructions,
            locations,
            code,
        } = *function;
        let address = code.as_ptr() as u64;
        let mut lines = vec![self.next_line];
        let mut listing = format!("{name}\n");
        self.next_line += 1;
        let mut guest_pc = pc;
        for instruction in instructions {
            lines.push(self.next_line);
            listing.push_str(&format!(
                "{guest_pc:08x} {} ({} bytes)\n",
                instruction.mnemonic, instruction.length
            ));
            self.next_line += 1;
            for op in &instruction.ops {
                listing.push_str(&format!("    {op:?}\n"));
                self.next_line += 1;
            }
            guest_pc = guest_pc.wrapping_add(u32::from(instruction.length));
        }
        self.source.write_all(listing.as_bytes())?;
        let debug_size = 32usize
            .checked_add(
                locations
                    .len()
                    .checked_mul(17 + self.source_path.len())
                    .ok_or_else(|| io::Error::other("jitdump debug record too large"))?,
            )
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("jitdump debug record too large"))?;
        let mut debug = Vec::with_capacity(debug_size as usize);
        u32_bytes(&mut debug, 2);
        u32_bytes(&mut debug, debug_size);
        u64_bytes(&mut debug, timestamp);
        u64_bytes(&mut debug, address);
        u64_bytes(&mut debug, locations.len() as u64);
        for &(offset, instruction) in locations {
            u64_bytes(&mut debug, address + u64::from(offset));
            u32_bytes(&mut debug, lines[instruction as usize]);
            u32_bytes(&mut debug, 0);
            debug.extend_from_slice(self.source_path.as_bytes());
            debug.push(0);
        }
        self.file.write_all(&debug)?;
        let total = 56usize
            .checked_add(name.len() + 1)
            .and_then(|n| n.checked_add(code.len()))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("jitdump record too large"))?;
        let mut record = Vec::with_capacity(total as usize);
        u32_bytes(&mut record, 0);
        u32_bytes(&mut record, total);
        u64_bytes(&mut record, timestamp);
        u32_bytes(&mut record, self.pid);
        u32_bytes(&mut record, tid);
        u64_bytes(&mut record, address);
        u64_bytes(&mut record, address);
        u64_bytes(&mut record, code.len() as u64);
        u64_bytes(&mut record, self.next_index);
        record.extend_from_slice(name.as_bytes());
        record.push(0);
        record.extend_from_slice(code);
        self.file.write_all(&record)?;
        self.next_index += 1;
        Ok(())
    }
}

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RECOMP_BASELINE_JITDUMP").as_deref() == Ok("1"))
}

pub fn name(pc: u32) -> String {
    let index = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
    format!("baseline_{pc:08x}_{index}")
}

pub fn record(function: &Function, config: &Config) {
    let now = || config.callbacks.clock_ns();
    let writer = WRITER.get_or_init(|| {
        Mutex::new(match Writer::new(now(), config.pid) {
            Ok(writer) => Some(writer),
            Err(error) => {
                eprintln!("baseline jitdump disabled: {error}");
                None
            }
        })
    });
    let mut writer = writer.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(active) = writer.as_mut() {
        if let Err(error) = active.record(function, now(), config.tid) {
            eprintln!("baseline jitdump disabled: {error}");
            *writer = None;
        }
    }
}
