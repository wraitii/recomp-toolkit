use std::collections::BTreeMap;

use rk_core::ir::{Insn, OpCode, PcodeOp, Space, Varnode};
use rk_core::{Addr, Reason};
use rk_ir::lower::{copy, from_pcode, is_control, op};
use rk_ir::{CgOp, Op, Registers};

use rk_ir::wide::{wide_lanes, wide_output};

pub(crate) fn correct_extra(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: format!("unsupported {mnem} p-code shape"),
    };
    if mnem.starts_with("CMOV") {
        return correct_cmov(ops, insn, fresh).map_err(|_| unsupported());
    }
    match mnem {
        "XADD" => correct_xadd(ops, regs, fresh),
        "CMPXCHG" => correct_cmpxchg(ops, regs, insn, fresh),
        "CMPXCHG8B" => correct_cmpxchg8b(ops, regs, insn, fresh),
        _ => Ok(ops.to_vec()),
    }
}

pub(crate) fn correct_sse(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    if crate::wide::deferred(mnem) {
        return Ok(if mnem.starts_with("CMP") && mnem.ends_with("PS") {
            rewrite_temps(ops, regs, fresh)
        } else {
            ops.to_vec()
        });
    }
    let unsupported = || Reason::Unsupported {
        detail: format!("unsupported {mnem} p-code shape"),
    };
    if matches!(mnem, "MINSS" | "MAXSS") {
        let at = ops.len().checked_sub(3).ok_or_else(unsupported)?;
        let (compare, branch, copy) = (&ops[at], &ops[at + 1], &ops[at + 2]);
        let dest = copy.output.ok_or_else(unsupported)?;
        let source = *copy.inputs.first().ok_or_else(unsupported)?;
        let expected = if mnem == "MINSS" {
            (dest, source)
        } else {
            (source, dest)
        };
        if copy.opcode != OpCode::Copy
            || dest.space != Space::Register
            || dest.size != 4
            || source.size != 4
            || compare.opcode != OpCode::FloatLess
            || compare.output.is_none_or(|v| v.size != 1)
            || compare.inputs != [expected.0, expected.1]
            || branch.opcode != OpCode::CBranch
            || branch.output.is_some()
            || branch.inputs.len() != 2
            || branch.inputs[0].space != Space::Ram
            || branch.inputs[0].offset != (insn.end())
            || Some(branch.inputs[1]) != compare.output
            || ops[..at].iter().any(|op| {
                matches!(
                    op.opcode,
                    OpCode::Branch | OpCode::CBranch | OpCode::BranchInd | OpCode::Store
                )
            })
        {
            return Err(unsupported());
        }
        let mut out = ops[..at].to_vec();
        let code = if mnem == "MINSS" {
            OpCode::FloatMin
        } else {
            OpCode::FloatMax
        };
        out.push(op(code, dest, vec![dest, source]));
        return Ok(out);
    }
    if matches!(mnem, "COMISS" | "UCOMISS") {
        if ops.iter().any(|op| op.opcode == OpCode::Store) {
            return Err(unsupported());
        }
        let mut out = Vec::with_capacity(ops.len());
        let mut load = None;
        for op in ops {
            if op.opcode == OpCode::Load {
                let key = (op.inputs.clone(), op.output);
                if load.as_ref() == Some(&key) {
                    continue;
                }
                load = Some(key);
            }
            out.push(op.clone());
        }
        return Ok(out);
    }
    if mnem == "CVTTSS2SI" {
        return Ok(ops
            .iter()
            .map(|item| {
                if item.opcode == OpCode::FloatTrunc
                    && item.output.is_some_and(|v| v.size == 4)
                    && item.inputs.len() == 1
                    && item.inputs[0].size == 4
                {
                    op(OpCode::Cvttss2si, item.output.unwrap(), item.inputs.clone())
                } else {
                    item.clone()
                }
            })
            .collect());
    }
    if mnem == "CVTSS2SI" {
        let at = ops.len().checked_sub(2).ok_or_else(unsupported)?;
        let (round, trunc) = (&ops[at], &ops[at + 1]);
        let rounded = round.output.ok_or_else(unsupported)?;
        let dest = trunc.output.ok_or_else(unsupported)?;
        if round.opcode != OpCode::FloatRound
            || rounded.space != Space::Unique
            || rounded.size != 4
            || round.inputs.len() != 1
            || round.inputs[0].size != 4
            || trunc.opcode != OpCode::FloatTrunc
            || dest.space != Space::Register
            || dest.size != 4
            || trunc.inputs != [rounded]
        {
            return Err(unsupported());
        }
        let mut out = ops[..at].to_vec();
        out.push(op(OpCode::Cvtss2si, dest, round.inputs.clone()));
        return Ok(out);
    }
    if mnem == "CVTSI2SS" {
        return Ok(ops
            .iter()
            .map(|item| {
                if item.opcode == OpCode::FloatInt2Float
                    && item.output.is_some_and(|v| v.size == 4)
                    && item.inputs.len() == 1
                    && item.inputs[0].size == 4
                {
                    op(OpCode::Cvtsi2ss, item.output.unwrap(), item.inputs.clone())
                } else {
                    item.clone()
                }
            })
            .collect());
    }
    if let Some((userop, code)) = match mnem {
        "RCPSS" => Some(("rcpss", OpCode::FloatRcp)),
        "RSQRTSS" => Some(("rsqrtss", OpCode::FloatRsqrt)),
        _ => None,
    } {
        let mut out = Vec::with_capacity(ops.len());
        for item in ops {
            if item.opcode == OpCode::CallOther && regs.userop(item.userop) == Some(userop) {
                let output = item.output.ok_or_else(unsupported)?;
                let source = *item.inputs.last().ok_or_else(unsupported)?;
                if output.size == 16 && source.size == 16 {
                    let output_lanes = wide_output(output, regs, fresh)?;
                    let source_lanes = wide_lanes(source, &BTreeMap::new(), regs, fresh)?;
                    out.push(op(code, output_lanes[0], vec![source_lanes[0]]));
                } else if output.size == 4 && source.size == 4 {
                    out.push(op(code, output, vec![source]));
                } else {
                    return Err(unsupported());
                }
            } else {
                out.push(item.clone());
            }
        }
        return Ok(out);
    }
    Ok(ops.to_vec())
}

pub(crate) fn rewrite_temps(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let is_temp = |value: Varnode| {
        value.space == Space::Register
            && regs
                .covering(value.offset)
                .is_some_and(|(name, _)| name.starts_with("xmmTmp"))
    };
    let mut values: BTreeMap<Varnode, Varnode> = BTreeMap::new();
    let mut out = Vec::with_capacity(ops.len());
    for item in ops {
        let mut item = item.clone();
        for input in &mut item.inputs {
            if is_temp(*input) {
                if let Some(value) = values.get(input) {
                    *input = *value;
                }
            }
        }
        if let Some(output) = item.output.filter(|value| is_temp(*value)) {
            let value = fresh(output.size);
            values.insert(output, value);
            item.output = Some(value);
        }
        out.push(item);
    }
    out
}

pub(crate) fn select_value(
    condition: Varnode,
    when_true: Varnode,
    when_false: Varnode,
    output: Varnode,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let size = when_false.size;
    let extended = fresh(size);
    let mask = fresh(size);
    let changed = fresh(size);
    let selected = fresh(size);
    vec![
        op(OpCode::IntZExt, extended, vec![condition]),
        op(OpCode::Int2Comp, mask, vec![extended]),
        op(OpCode::IntXor, changed, vec![when_true, when_false]),
        op(OpCode::IntAnd, selected, vec![changed, mask]),
        op(OpCode::IntXor, output, vec![when_false, selected]),
    ]
}

pub(crate) fn add_af(
    a: Varnode,
    b: Varnode,
    result: Varnode,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let x = fresh(result.size);
    let y = fresh(result.size);
    let z = fresh(result.size);
    vec![
        op(OpCode::IntXor, x, vec![a, b]),
        op(OpCode::IntXor, y, vec![x, result]),
        op(
            OpCode::IntRight,
            z,
            vec![y, Varnode::constant(4, result.size)],
        ),
        op(
            OpCode::IntAnd,
            regs.arch().x86.af,
            vec![z, Varnode::constant(1, result.size)],
        ),
    ]
}

pub(crate) fn internal_target(target: Varnode, end: Addr) -> bool {
    target.space == Space::Const || (target.space == Space::Ram && target.offset == end)
}

pub(crate) fn correct_cmov(
    ops: &[PcodeOp],
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported CMOV p-code shape".into(),
    };
    let at = ops.len().checked_sub(4).ok_or_else(unsupported)?;
    let (source_copy, negate, branch, dest_copy) =
        (&ops[at], &ops[at + 1], &ops[at + 2], &ops[at + 3]);
    let dest = dest_copy.output.ok_or_else(unsupported)?;
    let source = *source_copy.inputs.first().ok_or_else(unsupported)?;
    let condition = *negate.inputs.first().ok_or_else(unsupported)?;
    if source_copy.opcode != OpCode::Copy
        || source_copy.output.is_none()
        || source.size != dest.size
        || negate.opcode != OpCode::BoolNegate
        || negate.output.is_none_or(|v| v.size != 1)
        || branch.opcode != OpCode::CBranch
        || branch.output.is_some()
        || branch.inputs.len() != 2
        || branch.inputs[0].space != Space::Ram
        || branch.inputs[0].offset != (insn.end())
        || branch.inputs[1] != negate.output.unwrap()
        || dest_copy.opcode != OpCode::Copy
        || dest.space != Space::Register
        || !matches!(dest.size, 2 | 4)
        || dest_copy.inputs != [source_copy.output.unwrap()]
        || ops[..at].iter().any(|op| {
            matches!(
                op.opcode,
                OpCode::Branch | OpCode::CBranch | OpCode::BranchInd | OpCode::Store
            )
        })
    {
        return Err(unsupported());
    }
    let mut out = ops[..at].to_vec();
    out.extend(select_value(condition, source, dest, dest, fresh));
    Ok(out)
}

pub(crate) fn correct_xadd(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported XADD p-code shape".into(),
    };
    let primary = ops
        .iter()
        .rposition(|op| {
            op.opcode == OpCode::IntAdd
                && op.output.is_some_and(|v| matches!(v.size, 1 | 2 | 4))
                && op.inputs.len() == 2
        })
        .ok_or_else(unsupported)?;
    let result = ops[primary].output.unwrap();
    let old = ops[primary].inputs[0];
    let source = ops[primary].inputs[1];
    let stores: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Store)
        .map(|(index, _)| index)
        .collect();
    if old.size != result.size || source.size != result.size || stores.len() > 1 {
        return Err(unsupported());
    }
    let mut address = None;
    if let Some(&store_index) = stores.first() {
        let store = &ops[store_index];
        let load_index = ops
            .iter()
            .position(|op| op.opcode == OpCode::Load && op.output == Some(old))
            .ok_or_else(unsupported)?;
        if store.inputs.len() != 2
            || store.inputs[1].size != result.size
            || ops[load_index].inputs != store.inputs[..1]
        {
            return Err(unsupported());
        }
        address = Some((store.inputs[0], load_index));
    }
    let source_write = ops[primary + 1..]
        .iter()
        .any(|op| op.opcode == OpCode::Copy && op.output == Some(source) && op.inputs == [old]);
    let destination_write = if stores.is_empty() {
        ops[primary + 1..]
            .iter()
            .any(|op| op.opcode == OpCode::Copy && op.output == Some(old) && op.inputs == [result])
    } else {
        let store = &ops[stores[0]];
        ops[primary + 1..].iter().any(|op| {
            (op.opcode == OpCode::Copy
                && op.output == Some(store.inputs[1])
                && op.inputs == [result])
                || (op.opcode == OpCode::Store && op.inputs[1] == result)
        })
    };
    if !source_write || !destination_write {
        return Err(unsupported());
    }
    let captured = address.map(|(base, load_index)| {
        let unique = fresh(4);
        (base, load_index, unique)
    });
    let mut out = Vec::with_capacity(ops.len() + 6);
    let mut seen_load = false;
    for (index, item) in ops.iter().enumerate() {
        let mut item = item.clone();
        if let Some((base, load_index, unique)) = captured {
            if index == load_index {
                out.push(copy(unique, base));
            }
            if item.opcode == OpCode::Load && item.output == Some(old) {
                item.inputs[0] = unique;
                if seen_load {
                    continue;
                }
                seen_load = index == load_index;
            } else if item.opcode == OpCode::Store {
                item.inputs[0] = unique;
            }
        }
        out.push(item);
        if index == primary {
            out.extend(add_af(old, source, result, regs, fresh));
        }
    }
    Ok(out)
}

pub(crate) fn correct_cmpxchg(
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported CMPXCHG p-code shape".into(),
    };
    let end = insn.end();
    let if_index = ops
        .iter()
        .position(|op| op.opcode == OpCode::CBranch)
        .ok_or_else(unsupported)?;
    let branch_index = ops[if_index + 1..]
        .iter()
        .position(|op| op.opcode == OpCode::Branch)
        .map(|index| index + if_index + 1)
        .ok_or_else(unsupported)?;
    let cmp_index = ops[..if_index]
        .iter()
        .rposition(|op| {
            op.opcode == OpCode::IntSub
                && op.output.is_some_and(|v| matches!(v.size, 1 | 2 | 4))
                && op.inputs.len() == 2
        })
        .ok_or_else(unsupported)?;
    let result = ops[cmp_index].output.unwrap();
    let acc = ops[cmp_index].inputs[0];
    let old = ops[cmp_index].inputs[1];
    let condition = *ops[if_index].inputs.get(1).ok_or_else(unsupported)?;
    if !internal_target(ops[if_index].inputs[0], end)
        || !internal_target(ops[branch_index].inputs[0], end)
        || acc.space != Space::Register
        || acc.size != old.size
        || result.size != acc.size
        || condition.size != 1
        || ops[..if_index]
            .iter()
            .any(|op| matches!(op.opcode, OpCode::Branch | OpCode::CBranch | OpCode::Store))
    {
        return Err(unsupported());
    }
    let old_copy = ops[..if_index]
        .iter()
        .find(|op| op.opcode == OpCode::Copy && op.output == Some(old) && op.inputs.len() == 1)
        .ok_or_else(unsupported)?;
    let destination = old_copy.inputs[0];
    if !ops[if_index + 1..branch_index]
        .iter()
        .any(|op| op.opcode == OpCode::Copy && op.output == Some(acc) && op.inputs == [old])
    {
        return Err(unsupported());
    }
    let after = &ops[branch_index + 1..];
    let stores: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, op)| op.opcode == OpCode::Store)
        .map(|(index, _)| index)
        .collect();
    if stores.len() > 1 {
        return Err(unsupported());
    }
    let (source, memory) = if let Some(&store_index) = stores.first() {
        let store = &after[store_index];
        let stored = *store.inputs.get(1).ok_or_else(unsupported)?;
        let success = after[..store_index]
            .iter()
            .find(|op| {
                op.opcode == OpCode::Copy && op.output == Some(stored) && op.inputs.len() == 1
            })
            .ok_or_else(unsupported)?;
        let load_index = ops[..if_index]
            .iter()
            .position(|op| op.opcode == OpCode::Load && op.output == Some(destination))
            .ok_or_else(unsupported)?;
        if store.inputs.len() != 2
            || store.inputs[1].size != result.size
            || ops[load_index].inputs[0] != store.inputs[0]
        {
            return Err(unsupported());
        }
        (success.inputs[0], Some((store.inputs[0], load_index)))
    } else {
        let success = after
            .iter()
            .find(|op| {
                op.opcode == OpCode::Copy && op.output == Some(destination) && op.inputs.len() == 1
            })
            .ok_or_else(unsupported)?;
        if destination.space != Space::Register {
            return Err(unsupported());
        }
        (success.inputs[0], None)
    };
    if source.size != result.size {
        return Err(unsupported());
    }
    let mut out = Vec::with_capacity(ops.len() + 20);
    let captured = memory.map(|(base, load_index)| {
        let unique = fresh(4);
        (base, load_index, unique)
    });
    for (index, item) in ops[..if_index].iter().enumerate() {
        let mut item = item.clone();
        if let Some((base, load_index, unique)) = captured {
            if index == load_index {
                out.push(copy(unique, base));
                item.inputs[0] = unique;
            }
        }
        out.push(item);
    }
    out.extend(add_af(acc, old, result, regs, fresh));
    out.extend(select_value(condition, acc, old, acc, fresh));
    if let Some((_, _, unique)) = captured {
        // x86 writes the old value back on mismatch; Ghidra's p-code skips that store.
        let selected = fresh(result.size);
        out.extend(select_value(condition, source, old, selected, fresh));
        out.push(PcodeOp {
            opcode: OpCode::Store,
            output: None,
            inputs: vec![unique, selected],
            userop: None,
        });
    } else {
        out.extend(select_value(condition, source, old, destination, fresh));
    }
    Ok(out)
}

pub(crate) fn correct_cmpxchg8b(
    ops: &[PcodeOp],
    regs: &Registers,
    insn: &Insn,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported CMPXCHG8B p-code shape".into(),
    };
    let end = insn.end();
    let zf = regs.arch().x86.zf;
    let zf_index = ops
        .iter()
        .position(|op| {
            op.opcode == OpCode::IntEqual && op.output == Some(zf) && op.inputs.len() == 2
        })
        .ok_or_else(unsupported)?;
    let if_index = ops[zf_index + 1..]
        .iter()
        .position(|op| op.opcode == OpCode::CBranch)
        .map(|index| index + zf_index + 1)
        .ok_or_else(unsupported)?;
    let branch_index = ops[if_index + 1..]
        .iter()
        .position(|op| op.opcode == OpCode::Branch)
        .map(|index| index + if_index + 1)
        .ok_or_else(unsupported)?;
    let load_index = ops[..zf_index]
        .iter()
        .position(|op| op.opcode == OpCode::Load && op.output.is_some_and(|v| v.size == 8))
        .ok_or_else(unsupported)?;
    let loaded = ops[load_index].output.unwrap();
    let old = ops[zf_index].inputs[1];
    let after = &ops[branch_index + 1..];
    let store = after
        .iter()
        .find(|op| op.opcode == OpCode::Store)
        .ok_or_else(unsupported)?;
    let arch = regs.arch();
    let eax = arch.x86.eax;
    let ecx = arch.x86.ecx;
    let edx = arch.x86.edx;
    let ebx = arch.x86.ebx;
    if !internal_target(ops[if_index].inputs[0], end)
        || !internal_target(ops[branch_index].inputs[0], end)
        || old.size != 8
        || !ops[..zf_index]
            .iter()
            .any(|op| op.opcode == OpCode::Copy && op.output == Some(old) && op.inputs == [loaded])
        || store.inputs.len() != 2
        || store.inputs[1].size != 8
        || store.inputs[0] != ops[load_index].inputs[0]
        || !after
            .iter()
            .any(|op| op.opcode == OpCode::IntZExt && op.inputs == [ecx])
        || !after
            .iter()
            .any(|op| op.opcode == OpCode::IntZExt && op.inputs == [ebx])
        || ops[..zf_index]
            .iter()
            .any(|op| matches!(op.opcode, OpCode::Branch | OpCode::CBranch | OpCode::Store))
    {
        return Err(unsupported());
    }
    let base = store.inputs[0];
    let address = fresh(4);
    let mut out = Vec::with_capacity(ops.len() + 20);
    for (index, item) in ops[..=zf_index].iter().enumerate() {
        let mut item = item.clone();
        if index == load_index {
            out.push(copy(address, base));
            item.inputs[0] = address;
        }
        out.push(item);
    }
    let high = fresh(8);
    let low = fresh(8);
    let shifted = fresh(8);
    let desired = fresh(8);
    out.extend([
        op(OpCode::IntZExt, high, vec![ecx]),
        op(
            OpCode::IntLeft,
            shifted,
            vec![high, Varnode::constant(32, 4)],
        ),
        op(OpCode::IntZExt, low, vec![ebx]),
        op(OpCode::IntOr, desired, vec![shifted, low]),
    ]);
    let selected = fresh(8);
    // x86 writes the old value back on mismatch; Ghidra's p-code skips that store.
    out.extend(select_value(zf, desired, old, selected, fresh));
    out.push(PcodeOp {
        opcode: OpCode::Store,
        output: None,
        inputs: vec![address, selected],
        userop: None,
    });
    let original_low = fresh(4);
    let original_high = fresh(4);
    out.extend([
        op(
            OpCode::SubPiece,
            original_low,
            vec![old, Varnode::constant(0, 4)],
        ),
        op(
            OpCode::SubPiece,
            original_high,
            vec![old, Varnode::constant(4, 4)],
        ),
    ]);
    out.extend(select_value(zf, eax, original_low, eax, fresh));
    out.extend(select_value(zf, edx, original_high, edx, fresh));
    Ok(out)
}

pub(crate) fn with_af_zero(ops: &[PcodeOp], regs: &Registers) -> Vec<PcodeOp> {
    let mut out = ops.to_vec();
    out.push(copy(regs.arch().x86.af, Varnode::constant(0, 1)));
    out
}

pub(crate) fn correct_bsr(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported BSR p-code shape".into(),
    };
    let zf = regs.arch().x86.zf;
    let at = ops
        .iter()
        .position(|op| op.output == Some(zf))
        .ok_or_else(unsupported)?;
    let zero = &ops[at];
    let source = *zero.inputs.first().ok_or_else(unsupported)?;
    let last = ops.last().ok_or_else(unsupported)?;
    let dst = last.output.ok_or_else(unsupported)?;
    if zero.opcode != OpCode::IntEqual
        || zero.inputs.len() != 2
        || zero.inputs[1] != Varnode::constant(0, source.size)
        || last.opcode != OpCode::Copy
        || dst.space != Space::Register
        || !matches!(dst.size, 2 | 4)
        || source.size != dst.size
        || ops[..at].iter().any(is_control)
    {
        return Err(unsupported());
    }
    let leading = fresh(dst.size);
    let index = fresh(dst.size);
    let extended = fresh(dst.size);
    let mask = fresh(dst.size);
    let changed = fresh(dst.size);
    let selected = fresh(dst.size);
    let mut out = ops[..=at].to_vec();
    out.extend([
        op(OpCode::LzCount, leading, vec![source]),
        op(
            OpCode::IntSub,
            index,
            vec![
                Varnode::constant((dst.size * 8 - 1) as u64, dst.size),
                leading,
            ],
        ),
        op(OpCode::IntZExt, extended, vec![zf]),
        op(
            OpCode::IntSub,
            mask,
            vec![extended, Varnode::constant(1, dst.size)],
        ),
        op(OpCode::IntXor, changed, vec![dst, index]),
        op(OpCode::IntAnd, selected, vec![changed, mask]),
        op(OpCode::IntXor, dst, vec![dst, selected]),
    ]);
    Ok(out)
}

pub(crate) fn defer_flags(
    ops: Vec<PcodeOp>,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Vec<PcodeOp> {
    let mut names = BTreeMap::new();
    let mut deferred = Vec::new();
    let mut out = Vec::with_capacity(ops.len() + 8);
    for op in ops {
        let mut op = op;
        for input in &mut op.inputs {
            if let Some(&replacement) = names.get(input) {
                *input = replacement;
            }
        }
        if let Some(output) = op.output {
            let is_flag = output.space == Space::Register
                && regs
                    .arch()
                    .x86
                    .arithmetic_flags()
                    .iter()
                    .any(|flag| output.offset == flag.offset && output.size == flag.size);
            if is_flag {
                let replacement = fresh(output.size);
                op.output = Some(replacement);
                deferred.push((output, replacement));
                names.insert(output, replacement);
            }
        }
        out.push(op);
    }
    out.extend(
        deferred
            .into_iter()
            .map(|(register, value)| copy(register, value)),
    );
    out
}

fn without_flags(ops: Vec<PcodeOp>, regs: &Registers, live: &[Varnode]) -> Vec<PcodeOp> {
    let flags = regs.arch().x86.arithmetic_flags();
    let mut live: Vec<Varnode> = live.to_vec();
    let mut out = Vec::with_capacity(ops.len());
    for op in ops.into_iter().rev() {
        if let Some(output) = op.output {
            let dead = flags.contains(&output)
                || (output.space == Space::Unique && !live.contains(&output));
            if dead {
                continue;
            }
        }
        live.extend(op.inputs.iter().copied());
        out.push(op);
    }
    out.reverse();
    out
}

pub(crate) fn correct_arithmetic(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
    flag_operands: &mut Option<(Varnode, Varnode)>,
    lazy_flags: &mut Option<Vec<Varnode>>,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let (_, a, b, result, insert) = if mnem == "ADC" || mnem == "SBB" {
        let opcode = if mnem == "ADC" {
            OpCode::IntAdd
        } else {
            OpCode::IntSub
        };
        let primary = ops
            .iter()
            .rposition(|op| {
                op.opcode == opcode
                    && op
                        .output
                        .is_some_and(|v| matches!(v.space, Space::Register | Space::Unique))
            })
            .ok_or_else(|| Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            })?;
        let result = ops[primary].output.unwrap();
        let first_cf = ops
            .iter()
            .position(|op| op.output == Some(regs.arch().x86.cf))
            .ok_or_else(|| Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            })?;
        let expected = if mnem == "ADC" {
            OpCode::IntCarry
        } else {
            OpCode::IntLess
        };
        let inputs = if ops[first_cf].opcode == expected {
            ops[first_cf].inputs.clone()
        } else if ops[first_cf].opcode == OpCode::Copy
            && ops[first_cf].inputs == [Varnode::constant(0, 1)]
        {
            vec![result, result]
        } else {
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        };
        if inputs.len() < 2 {
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        }
        let a = fresh(result.size);
        let b = fresh(result.size);
        *flag_operands = Some((a, b));
        ops.splice(first_cf..first_cf, [copy(a, inputs[0]), copy(b, inputs[1])]);
        (primary + 2, a, b, result, first_cf + 3)
    } else {
        let opcode = if mnem == "ADD" || mnem == "INC" {
            OpCode::IntAdd
        } else {
            OpCode::IntSub
        };
        let Some(primary) = ops.iter().rposition(|op| {
            op.opcode == opcode
                && op
                    .output
                    .is_some_and(|v| v.space == Space::Register || v.space == Space::Unique)
        }) else {
            let is_flag = |v: Varnode| {
                regs.arch().x86.arithmetic_flags().iter().any(|flag| {
                    v.space == Space::Register && v.offset == flag.offset && v.size == flag.size
                })
            };
            let zero_result = ops.iter().any(|op| {
                op.opcode == OpCode::Copy
                    && op
                        .inputs
                        .first()
                        .is_some_and(|v| v.space == Space::Const && v.offset == 0)
                    && op
                        .output
                        .is_some_and(|v| v.space == Space::Register && !is_flag(v))
            });
            if (mnem == "SUB" || mnem == "CMP") && zero_result {
                let mut out = ops.to_vec();
                let af = regs.arch().x86.af;
                if !out.iter().any(|op| op.output == Some(af)) {
                    out.push(copy(af, Varnode::constant(0, 1)));
                }
                return Ok(out);
            }
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        };
        let result = ops[primary].output.unwrap();
        let inputs = ops[primary].inputs.clone();
        if inputs.len() < 2 {
            return Err(Reason::UnsupportedArithmetic {
                mnemonic: mnem.into(),
            });
        }
        let a = fresh(result.size);
        let b = fresh(result.size);
        ops.splice(primary..primary, [copy(a, inputs[0]), copy(b, inputs[1])]);
        if matches!(mnem, "ADD" | "SUB" | "CMP") {
            let kept = fresh(result.size);
            ops.insert(primary + 3, copy(kept, result));
            *lazy_flags = Some(vec![a, b, kept]);
            return Ok(without_flags(ops, regs, &[a, b, kept]));
        }
        (primary + 2, a, b, result, primary + 3)
    };
    let x = fresh(result.size);
    let y = fresh(result.size);
    let z = fresh(result.size);
    let recipe = vec![
        op(OpCode::IntXor, x, vec![a, b]),
        op(OpCode::IntXor, y, vec![x, result]),
        op(
            OpCode::IntRight,
            z,
            vec![y, Varnode::constant(4, result.size)],
        ),
        op(
            OpCode::IntAnd,
            regs.arch().x86.af,
            vec![z, Varnode::constant(1, result.size)],
        ),
    ];
    ops.splice(insert..insert, recipe);
    Ok(ops)
}

// OF is undefined for counts above 1; this and correct_sar match the runtime's
// shl/shr/sar _f helpers (docs/ir.md, Lazy flags) instead of Ghidra's preserved OF.
pub(crate) fn correct_shift(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let opcode = if mnem == "SHL" {
        OpCode::IntLeft
    } else {
        OpCode::IntRight
    };
    let index = ops
        .iter()
        .position(|op| {
            op.opcode == opcode
                && op.output.is_some_and(|v| {
                    matches!(v.space, Space::Register | Space::Unique)
                        && matches!(v.size, 1 | 2 | 4)
                })
        })
        .ok_or_else(|| Reason::UnsupportedShift {
            mnemonic: mnem.into(),
        })?;
    let source = ops[index].output.unwrap();
    let count = ops[index].inputs[1];
    let original = fresh(source.size);
    let old_of = fresh(1);
    ops.insert(index, copy(original, ops[index].inputs[0]));
    ops.insert(0, copy(old_of, regs.arch().x86.of));
    let nonzero = fresh(1);
    let zero = fresh(1);
    let sign = fresh(1);
    let changed = fresh(1);
    let kept = fresh(1);
    let updated = fresh(1);
    let sign_source = if mnem == "SHL" { source } else { original };
    let mut recipe = vec![
        op(
            OpCode::IntNotEqual,
            nonzero,
            vec![count, Varnode::constant(0, count.size)],
        ),
        PcodeOp {
            opcode: OpCode::BoolNegate,
            output: Some(zero),
            inputs: vec![nonzero],
            userop: None,
        },
        op(
            OpCode::IntSLess,
            sign,
            vec![sign_source, Varnode::constant(0, sign_source.size)],
        ),
    ];
    if mnem == "SHL" {
        let x = fresh(1);
        recipe.push(op(OpCode::IntXor, x, vec![sign, regs.arch().x86.cf]));
        recipe.push(op(OpCode::IntAnd, changed, vec![nonzero, x]));
    } else {
        recipe.push(op(OpCode::IntAnd, changed, vec![nonzero, sign]));
    }
    recipe.push(op(OpCode::IntAnd, kept, vec![zero, old_of]));
    recipe.push(op(OpCode::IntOr, updated, vec![changed, kept]));
    recipe.push(copy(regs.arch().x86.of, updated));
    ops.extend(recipe);
    Ok(ops)
}

pub(crate) fn correct_sar(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let index = ops
        .iter()
        .position(|op| {
            op.opcode == OpCode::IntSRight
                && op.output.is_some_and(|v| {
                    matches!(v.space, Space::Register | Space::Unique)
                        && matches!(v.size, 1 | 2 | 4)
                })
        })
        .ok_or_else(|| Reason::UnsupportedShift {
            mnemonic: "SAR".into(),
        })?;
    let count = ops[index].inputs[1];
    let old_of = fresh(1);
    let nonzero = fresh(1);
    let zero = fresh(1);
    let updated = fresh(1);
    ops.insert(0, copy(old_of, regs.arch().x86.of));
    ops.extend([
        op(
            OpCode::IntNotEqual,
            nonzero,
            vec![count, Varnode::constant(0, count.size)],
        ),
        PcodeOp {
            opcode: OpCode::BoolNegate,
            output: Some(zero),
            inputs: vec![nonzero],
            userop: None,
        },
        op(OpCode::IntAnd, updated, vec![zero, old_of]),
        copy(regs.arch().x86.of, updated),
    ]);
    Ok(ops)
}

pub(crate) fn correct_neg(
    ops: &[PcodeOp],
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let mut ops = ops.to_vec();
    let result = ops
        .iter()
        .find(|op| {
            op.opcode == OpCode::Int2Comp
                && op
                    .output
                    .is_some_and(|v| matches!(v.space, Space::Register | Space::Unique))
        })
        .and_then(|op| op.output)
        .ok_or_else(|| Reason::UnsupportedArithmetic {
            mnemonic: "NEG".into(),
        })?;
    let low = fresh(result.size);
    let af = fresh(1);
    ops.extend([
        op(
            OpCode::IntAnd,
            low,
            vec![result, Varnode::constant(0xf, result.size)],
        ),
        op(
            OpCode::IntNotEqual,
            af,
            vec![low, Varnode::constant(0, result.size)],
        ),
        copy(regs.arch().x86.af, af),
    ]);
    Ok(ops)
}

pub(crate) fn correct_div_cg(
    mnem: &str,
    ops: &[PcodeOp],
    regs: &Registers,
    addr: Addr,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<CgOp>, Reason> {
    let signed = mnem == "IDIV";
    let div = if signed {
        OpCode::IntSDiv
    } else {
        OpCode::IntDiv
    };
    let rem = if signed {
        OpCode::IntSRem
    } else {
        OpCode::IntRem
    };
    let unsupported = || Reason::UnsupportedDivide {
        mnemonic: mnem.into(),
    };
    let index = ops
        .iter()
        .position(|op| op.opcode == div)
        .ok_or_else(unsupported)?;
    let division = &ops[index];
    let quotient = division.output.ok_or_else(unsupported)?;
    let remainder = ops
        .iter()
        .find(|op| op.opcode == rem && op.inputs == division.inputs)
        .ok_or_else(unsupported)?;
    let remainder_value = remainder.output.ok_or_else(unsupported)?;
    let width = match quotient.size {
        2 => 1u32,
        4 => 2u32,
        8 => 4u32,
        _ => return Err(unsupported()),
    };
    if division.inputs.iter().any(|v| v.size != quotient.size) {
        return Err(unsupported());
    }
    let arch = regs.arch();
    let outputs: [(Varnode, Varnode, u64); 2] = match width {
        1 => [
            (arch.x86.eax.piece(0, 1), quotient, 0),
            (arch.x86.eax.piece(1, 1), remainder_value, 1),
        ],
        2 => [
            (arch.x86.eax.piece(0, 2), quotient, 0),
            (arch.x86.edx.piece(0, 2), remainder_value, 4),
        ],
        _ => [
            (arch.x86.eax, quotient, 0),
            (arch.x86.edx, remainder_value, 4),
        ],
    };
    let mut emitted = [false; 2];
    for op in &ops[index..] {
        if op.opcode == div || op.opcode == rem {
            continue;
        }
        let matched = outputs
            .iter()
            .enumerate()
            .position(|(n, (register, source, _))| {
                !emitted[n]
                    && op.opcode == OpCode::SubPiece
                    && op.output == Some(*register)
                    && op.inputs.len() == 2
                    && op.inputs[0] == *source
                    && op.inputs[1].space == Space::Const
                    && op.inputs[1].offset == 0
            });
        match matched {
            Some(n) => emitted[n] = true,
            None => return Err(unsupported()),
        }
    }
    if emitted.iter().any(|found| !found) {
        return Err(unsupported());
    }
    let result = fresh(8);
    let mut inputs = division.inputs.clone();
    inputs.push(Varnode::constant(addr, 4));
    let opcode = Op::DivMod {
        signed,
        size: width as u8,
    };
    let mut out: Vec<CgOp> = ops[..index].iter().cloned().map(from_pcode).collect();
    out.push(CgOp {
        op: opcode,
        output: Some(result),
        inputs,
    });
    out.extend(outputs.into_iter().map(|(register, _, offset)| CgOp {
        op: Op::P(OpCode::SubPiece),
        output: Some(register),
        inputs: vec![result, Varnode::constant(offset, 4)],
    }));
    Ok(out)
}
