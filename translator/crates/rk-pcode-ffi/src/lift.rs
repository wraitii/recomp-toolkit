use std::cell::RefCell;
use std::collections::BTreeMap;

use rk_core::ir::{Insn, OpCode, PcodeOp, Space, Varnode};
use rk_core::Addr;
use rk_ir::x87::X87Environment;
use rk_ir::{CgOp, FlagRecipes, InstructionLowerer, IsaLowering, Op, Registers, X87Op};
use rk_sleigh::Lifter;

const LIFTED_BYTES_LIMIT: usize = 65536;

thread_local! {
    static LIFTER: RefCell<Option<Lifter>> = const { RefCell::new(None) };
    static LIFTED_BYTES: RefCell<BTreeMap<u32, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
}

pub struct Lifted {
    pub length: u8,
    pub mnemonic: String,
    pub ops: Vec<CgOp>,
}

pub fn is_internal_branch(op: &CgOp) -> bool {
    matches!(op.op, Op::P(OpCode::Branch | OpCode::CBranch))
        && op.inputs.first().is_some_and(|v| v.space == Space::Const)
}

fn with_lifter<T>(f: impl FnOnce(&mut Lifter) -> rk_core::Result<T>) -> Result<T, String> {
    LIFTER.with(|l| {
        let mut l = l.borrow_mut();
        let lifter = match l.as_mut() {
            Some(lifter) => lifter,
            None => l.insert(Lifter::new().map_err(|e| e.to_string())?),
        };
        f(lifter).map_err(|e| e.to_string())
    })
}

fn invalidate_changed_bytes(addr: u32, bytes: &[u8]) {
    // SLEIGH keeps a disassembly cache keyed by address, so rewriting live bytes at a
    // previously lifted PC would otherwise return the old instruction.
    let changed = LIFTED_BYTES.with(|seen| {
        let mut seen = seen.borrow_mut();
        let changed = match seen.get(&addr) {
            Some(previous) => previous.as_slice() != bytes,
            None if seen.len() >= LIFTED_BYTES_LIMIT => {
                seen.clear();
                true
            }
            None => false,
        };
        seen.insert(addr, bytes.to_vec());
        changed
    });
    if changed {
        LIFTER.with(|l| *l.borrow_mut() = None);
    }
}

fn fpu_control() -> Varnode {
    Registers::get().arch().x86.fpu_control
}

fn from_raw(op: PcodeOp) -> CgOp {
    match op.opcode {
        OpCode::Load | OpCode::Store => CgOp {
            op: Op::P(op.opcode),
            output: op.output,
            inputs: op.inputs.get(1..).unwrap_or(&[]).to_vec(),
        },
        OpCode::Return => CgOp {
            op: Op::Return,
            output: op.output,
            inputs: op.inputs,
        },
        OpCode::Branch | OpCode::CBranch
            if op.inputs.first().is_some_and(|v| v.space == Space::Const) =>
        {
            CgOp {
                op: Op::P(op.opcode),
                output: op.output,
                inputs: op.inputs,
            }
        }
        _ => rk_ir::lower::from_pcode(op),
    }
}

fn normalize(op: CgOp) -> CgOp {
    match op.op {
        Op::X87(X87Op::Environment(X87Environment::LoadCw)) => CgOp {
            op: Op::P(OpCode::Load),
            output: Some(fpu_control()),
            inputs: op.inputs[..1].to_vec(),
        },
        Op::X87(X87Op::Environment(X87Environment::StoreCw)) => CgOp {
            op: Op::P(OpCode::Store),
            output: None,
            inputs: vec![op.inputs[0], fpu_control()],
        },
        _ => op,
    }
}

pub fn recipes() -> &'static dyn FlagRecipes {
    rk_x86::Lowering {
        x87: &rk_x87::Recipes,
    }
    .flag_recipes()
}

pub fn lift(addr: u32, bytes: &[u8]) -> Result<Lifted, String> {
    invalidate_changed_bytes(addr, bytes);
    rk_x86::install().map_err(|e| e.to_string())?;
    let at = Addr::from(addr);
    let length = with_lifter(|l| l.length(at, bytes))?;
    let machine = rk_core::isa::machine();
    if length == 0 || length > machine.max_length || length as usize > bytes.len() {
        return Err(format!("instruction length {length} is not modelled"));
    }
    let bytes = &bytes[..length as usize];
    let lifted = with_lifter(|l| l.lift(at, bytes))?;
    let decoded = machine
        .decoder
        .decode(at, bytes)
        .ok_or_else(|| "instruction decoder rejected lifted bytes".to_owned())?;
    let insn = Insn::new(at, bytes, decoded, lifted.ops, lifted.x87);
    if insn.mnemonic == "SHRD" {
        return Err("x86 SHRD overflow-flag semantics are not modelled".into());
    }
    let isa = rk_x86::Lowering {
        x87: &rk_x87::Recipes,
    };
    let mut lowerer = InstructionLowerer::new(&isa).with_pcode_control();
    let (ops, _) = lowerer.lower_insn(&insn).map_err(|e| e.to_string())?;
    let ops = if rk_ir::wide::required(&ops) {
        rk_ir::wide::legalize(&ops, insn.addr).map_err(|e| e.to_string())?
    } else {
        ops
    };
    let raw_internal = insn.ops.iter().any(|op| {
        matches!(op.opcode, OpCode::Branch | OpCode::CBranch)
            && op.inputs.first().is_some_and(|v| v.space == Space::Const)
    });
    if raw_internal && ops.iter().any(is_internal_branch) {
        let unchanged = ops.len() == insn.ops.len()
            && ops.iter().zip(&insn.ops).all(|(a, b)| {
                a.op == Op::P(b.opcode) && a.output == b.output && a.inputs == b.inputs
            });
        if !unchanged {
            return Err(format!(
                "{}: corrected internal p-code control flow is not modelled",
                insn.mnemonic
            ));
        }
        let raw = with_lifter(|l| l.lift_raw(at, bytes))?;
        return Ok(Lifted {
            length,
            mnemonic: insn.mnemonic,
            ops: raw.ops.into_iter().map(from_raw).collect(),
        });
    }
    if let Some(index) = ops.iter().find_map(|op| match op.op {
        Op::Slow(index) if !matches!(insn.mnemonic.as_str(), "RDTSC" | "CPUID") => Some(index),
        _ => None,
    }) {
        return Err(format!(
            "{}: unsupported {:?}",
            insn.mnemonic,
            lowerer.slow_insns()[index as usize].family
        ));
    }
    Ok(Lifted {
        length,
        mnemonic: insn.mnemonic,
        ops: ops.into_iter().map(normalize).collect(),
    })
}
