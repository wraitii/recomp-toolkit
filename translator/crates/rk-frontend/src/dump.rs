use std::io::Write;

use rk_core::ir::FunctionIr;
use rk_core::{Error, Result};
use rk_sleigh::{format_varnode, Lifter};

pub fn dump(fir: &FunctionIr, out: &mut impl Write) -> Result<()> {
    let lifter = Lifter::new()?;
    writeln!(out, "== function {:08x}", fir.addr).map_err(|e| Error::input(e.to_string()))?;
    for (i, ins) in fir.insns.iter().enumerate() {
        writeln!(out, "{}", ins.text).map_err(|e| Error::input(e.to_string()))?;
        for op in &ins.ops {
            let lhs = op
                .output
                .as_ref()
                .map(|v| format!("{} = ", format_varnode(v, &lifter)))
                .unwrap_or_default();
            let inputs: Vec<String> = op
                .inputs
                .iter()
                .map(|v| format_varnode(v, &lifter))
                .collect();
            let suffix = op
                .userop
                .map(|id| format!(" userop={id}"))
                .unwrap_or_default();
            writeln!(out, "  {lhs}{:?} {}{suffix}", op.opcode, inputs.join(", "))
                .map_err(|e| Error::input(e.to_string()))?;
        }
        let next = fir.succ.get(i).cloned().unwrap_or_default();
        if !next.is_empty() {
            let next: Vec<String> = next.iter().map(|n| format!("{n}")).collect();
            writeln!(out, "    -> {}", next.join(",")).map_err(|e| Error::input(e.to_string()))?;
        }
        if let Some(exits) = fir.exits.get(&(i as u32)) {
            let exits: Vec<String> = exits.iter().map(|a| format!("{a:08x}")).collect();
            writeln!(out, "    exit {}", exits.join(","))
                .map_err(|e| Error::input(e.to_string()))?;
        }
    }
    Ok(())
}
