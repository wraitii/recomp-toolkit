use std::io::Write;

use rk_core::Hex;
use rk_ir::{properties, Op, Registers, StateKey};

use crate::Ssa;

fn key_name(key: StateKey) -> String {
    match key {
        StateKey::Memory => "MEM".into(),
        StateKey::Register(offset) => Registers::get()
            .covering(offset as u64)
            .map(|(name, v)| {
                if v.size == 1 {
                    name.to_string()
                } else {
                    format!("{name}[{}]", offset as u64 - v.offset)
                }
            })
            .unwrap_or_else(|| format!("reg:{offset:08x}")),
    }
}

fn hex_const(data: u64) -> String {
    format!("0x{data:x}")
}

fn lane(s: &Ssa, value: crate::ValueId, index: u8) -> String {
    let item = s.value(value);
    match item.op {
        Op::Const(data) => hex_const(data >> (8 * index as u32) & 0xff),
        Op::P(rk_core::ir::OpCode::Copy) => s
            .args(value)
            .first()
            .map_or_else(|| format!("v{value}[{index}]"), |arg| lane(s, *arg, index)),
        Op::Pack => s
            .args(value)
            .get(index as usize)
            .map_or_else(|| format!("v{value}[{index}]"), |arg| expr(s, *arg)),
        _ => format!("v{value}[{index}]"),
    }
}

fn expr(s: &Ssa, value: crate::ValueId) -> String {
    let item = s.value(value);
    match item.op {
        Op::Const(data) => hex_const(data),
        Op::Input(key) => key_name(key),
        Op::Target(addr) => Hex(addr).to_string(),
        Op::Byte(index) => s
            .args(value)
            .first()
            .map_or_else(|| format!("v{value}[{index}]"), |arg| lane(s, *arg, index)),
        Op::Pack => {
            let args = s.args(value);
            if !args.is_empty()
                && args.iter().enumerate().all(|(index, arg)| {
                    let item = s.value(*arg);
                    item.op == Op::Byte(index as u8)
                        && item.args_len == 1
                        && s.args(*arg)[0] == s.args(args[0])[0]
                })
            {
                return expr(s, s.args(args[0])[0]);
            }
            let body = args
                .iter()
                .map(|arg| expr(s, *arg))
                .collect::<Vec<_>>()
                .join(", ");
            format!("Pack({body})")
        }
        Op::P(rk_core::ir::OpCode::Copy) => s
            .args(value)
            .first()
            .map_or_else(|| format!("v{value}"), |arg| expr(s, *arg)),
        Op::StateReload(key) => format!("reload {}", key_name(key)),
        Op::StateKeep(key) => format!("keep {}", key_name(key)),
        _ => format!("v{value}"),
    }
}

fn op_name(op: Op) -> String {
    match op {
        Op::P(code) => format!("{code:?}"),
        Op::Call { target } => format!("Call {}", Hex(target)),
        Op::Tail { target } => format!("Tail {}", Hex(target)),
        Op::ImportTail { slot } => format!("ImportTail {}", Hex(slot)),
        Op::Branch { target } => format!("Branch {}", Hex(target)),
        Op::CBranch { target } => format!("CBranch {}", Hex(target)),
        Op::Trap(addr) => format!("Trap {}", Hex(addr)),
        Op::Div { addr, size } => format!("Div{size} {}", Hex(addr)),
        Op::Idiv { addr, size } => format!("Idiv{size} {}", Hex(addr)),
        Op::String(name) => format!("String {}", name.0),
        Op::Slow(index) => format!("Slow#{index}"),
        _ => format!("{op:?}"),
    }
}

pub fn dump(s: &Ssa, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "== ssa {}", Hex(s.addr))?;
    writeln!(
        out,
        "dispatch {} loads, {} entries, {} blocks, {} values",
        s.dispatch.loads.len(),
        s.entries.len(),
        s.blocks.len(),
        s.values.len()
    )?;
    for (key, value) in &s.dispatch.loads {
        writeln!(out, "dispatch v{value} = Input {}", key_name(*key))?;
    }
    for entry in &s.entries {
        writeln!(out, "entry {} -> block {}", Hex(entry.addr), entry.target)?;
    }
    for (index, block) in s.blocks.iter().enumerate() {
        writeln!(
            out,
            "block {index} insns {}..{} preds {:?}",
            block.start, block.end, block.preds
        )?;
        for &phi in &block.phis {
            let args = s
                .args(phi)
                .iter()
                .map(|value| expr(s, *value))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                out,
                "  {} v{phi}: phi({args})",
                Hex(s.value_addr[phi as usize])
            )?;
        }
        for &id in &block.ops {
            let value = s.value(id);
            let props = properties(value.op);
            let args = s
                .args(id)
                .iter()
                .map(|arg| expr(s, *arg))
                .collect::<Vec<_>>()
                .join(", ");
            write!(
                out,
                "  {} v{id}: {} size={} args=[{args}]",
                Hex(s.value_addr[id as usize]),
                op_name(value.op),
                value.size
            )?;
            if props.observes {
                write!(out, " observe")?;
            }
            writeln!(out)?;
            if let Some(state) = s.boundary(id) {
                let changed: Vec<_> = state
                    .iter()
                    .filter(|(key, value)| s.input(*key) != Some(*value))
                    .map(|(key, value)| (*key, *value))
                    .collect();
                let reloaded = changed
                    .iter()
                    .filter(|(_, value)| {
                        matches!(s.value(*value).op, Op::StateReload(_) | Op::StateKeep(_))
                    })
                    .count();
                if reloaded * 2 >= s.keys.len() {
                    write!(out, "    state reload all")?;
                    let rest = changed
                        .iter()
                        .filter(|(_, value)| {
                            !matches!(s.value(*value).op, Op::StateReload(_) | Op::StateKeep(_))
                        })
                        .map(|(key, value)| format!("{}<{}", key_name(*key), expr(s, *value)))
                        .collect::<Vec<_>>();
                    if !rest.is_empty() {
                        write!(out, " except {}", rest.join(" "))?;
                    }
                    writeln!(out)?;
                } else {
                    let changed = changed
                        .iter()
                        .map(|(key, value)| format!("{}<{}", key_name(*key), expr(s, *value)))
                        .collect::<Vec<_>>();
                    if !changed.is_empty() {
                        writeln!(out, "    state {}", changed.join(" "))?;
                    }
                }
            }
        }
    }
    for (index, slow) in s.slow.iter().enumerate() {
        if s.values.iter().any(|v| v.op == Op::Slow(index as u32)) {
            writeln!(out, "slow {index}: {:?} at {}", slow.family, Hex(slow.addr))?;
        }
    }
    Ok(())
}
