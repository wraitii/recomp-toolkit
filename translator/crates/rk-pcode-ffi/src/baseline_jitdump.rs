use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::{RpOp, RpVarnode};

static NEXT_NAME: AtomicU64 = AtomicU64::new(0);
static WRITER: OnceLock<Mutex<Option<Writer>>> = OnceLock::new();

struct Writer {
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
    fn new(clock_ns: extern "C" fn() -> u64, pid: u32) -> io::Result<Self> {
        let arch = if cfg!(target_arch = "aarch64") {
            183
        } else if cfg!(target_arch = "x86_64") {
            62
        } else {
            return Err(io::Error::other("unsupported host architecture"));
        };
        let timestamp = clock_ns();
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
            file,
            source,
            source_path,
            next_line: 1,
            next_index: 0,
        })
    }

    fn record(
        &mut self,
        name: &str,
        pc: u32,
        instructions: &[Vec<RpOp>],
        lengths: &[u8],
        locations: &[(u32, u32)],
        code: *const u8,
        size: usize,
        timestamp: u64,
        pid: u32,
        tid: u32,
    ) -> io::Result<()> {
        let mut lines = vec![self.next_line];
        let mut listing = format!("{name}\n");
        self.next_line += 1;
        let mut guest_pc = pc;
        for (ops, length) in instructions.iter().zip(lengths) {
            lines.push(self.next_line);
            listing.push_str(&format!("{guest_pc:08x} ({length} bytes)\n"));
            self.next_line += 1;
            for op in ops {
                listing.push_str(&format!("    {}\n", format_op(op)));
                self.next_line += 1;
            }
            guest_pc = guest_pc.wrapping_add(u32::from(*length));
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
        u64_bytes(&mut debug, code as usize as u64);
        u64_bytes(&mut debug, locations.len() as u64);
        for &(offset, instruction) in locations {
            u64_bytes(&mut debug, code as usize as u64 + u64::from(offset));
            u32_bytes(&mut debug, lines[instruction as usize]);
            u32_bytes(&mut debug, 0);
            debug.extend_from_slice(self.source_path.as_bytes());
            debug.push(0);
        }
        self.file.write_all(&debug)?;
        let total = 56usize
            .checked_add(name.len() + 1)
            .and_then(|n| n.checked_add(size))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("jitdump record too large"))?;
        let mut record = Vec::with_capacity(total as usize);
        u32_bytes(&mut record, 0);
        u32_bytes(&mut record, total);
        u64_bytes(&mut record, timestamp);
        u32_bytes(&mut record, pid);
        u32_bytes(&mut record, tid);
        u64_bytes(&mut record, code as usize as u64);
        u64_bytes(&mut record, code as usize as u64);
        u64_bytes(&mut record, size as u64);
        u64_bytes(&mut record, self.next_index);
        record.extend_from_slice(name.as_bytes());
        record.push(0);
        record.extend_from_slice(unsafe { std::slice::from_raw_parts(code, size) });
        self.file.write_all(&record)?;
        self.next_index += 1;
        Ok(())
    }
}

const OPCODES: &[&str] = &[
    "Unknown",
    "Copy",
    "Load",
    "Store",
    "Branch",
    "CBranch",
    "BranchInd",
    "Call",
    "CallInd",
    "CallOther",
    "Return",
    "IntEqual",
    "IntNotEqual",
    "IntSLess",
    "IntSLessEqual",
    "IntLess",
    "IntLessEqual",
    "IntZExt",
    "IntSExt",
    "IntAdd",
    "IntSub",
    "IntCarry",
    "IntSCarry",
    "IntSBorrow",
    "Int2Comp",
    "IntNegate",
    "IntXor",
    "IntAnd",
    "IntOr",
    "IntLeft",
    "IntRight",
    "IntSRight",
    "IntMult",
    "IntDiv",
    "IntSDiv",
    "IntRem",
    "IntSRem",
    "BoolNegate",
    "BoolXor",
    "BoolAnd",
    "BoolOr",
    "FloatEqual",
    "FloatNotEqual",
    "FloatLess",
    "FloatLessEqual",
    "FloatNan",
    "FloatAdd",
    "FloatDiv",
    "FloatMult",
    "FloatSub",
    "FloatNeg",
    "FloatAbs",
    "FloatSqrt",
    "FloatInt2Float",
    "FloatFloat2Float",
    "FloatTrunc",
    "FloatCeil",
    "FloatFloor",
    "FloatRound",
    "Piece",
    "SubPiece",
    "PopCount",
    "LzCount",
    "FloatMin",
    "FloatMax",
    "Cvtss2si",
    "Cvttss2si",
    "Cvtsi2ss",
    "FloatRcp",
    "FloatRsqrt",
    "DivMod",
    "SDivMod",
];

fn format_varnode(v: RpVarnode) -> String {
    let space = ["const", "ram", "reg", "unique"]
        .get(v.space as usize)
        .unwrap_or(&"unknown");
    format!("{space}[0x{:x}]:{}", v.offset, v.size)
}

fn format_op(op: &RpOp) -> String {
    let mut text = String::new();
    if op.has_output != 0 {
        text.push_str(&format!("{} = ", format_varnode(op.output)));
    }
    text.push_str(OPCODES.get(op.opcode as usize).unwrap_or(&"Unknown"));
    if op.has_userop != 0 {
        text.push_str(&format!("[0x{:x}]", op.userop));
    }
    for (index, input) in op.inputs[..op.input_count as usize].iter().enumerate() {
        text.push_str(if index == 0 { " " } else { ", " });
        text.push_str(&format_varnode(*input));
    }
    text
}

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RECOMP_BASELINE_JITDUMP").as_deref() == Ok("1"))
}

pub fn name(pc: u32) -> String {
    let index = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
    format!("baseline_{pc:08x}_{index}")
}

pub fn record(
    name: &str,
    pc: u32,
    instructions: &[Vec<RpOp>],
    lengths: &[u8],
    locations: &[(u32, u32)],
    code: *const u8,
    size: usize,
    clock_ns: extern "C" fn() -> u64,
    pid: u32,
    tid: u32,
) {
    let writer = WRITER.get_or_init(|| {
        let writer = if enabled() {
            match Writer::new(clock_ns, pid) {
                Ok(writer) => Some(writer),
                Err(error) => {
                    eprintln!("baseline jitdump disabled: {error}");
                    None
                }
            }
        } else {
            None
        };
        Mutex::new(writer)
    });
    let mut writer = writer.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(active) = writer.as_mut() {
        if let Err(error) = active.record(
            name,
            pc,
            instructions,
            lengths,
            locations,
            code,
            size,
            clock_ns(),
            pid,
            tid,
        ) {
            eprintln!("baseline jitdump disabled: {error}");
            *writer = None;
        }
    }
}
