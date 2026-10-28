use rk_core::{Diagnostic, Result};
use rk_ir::{Op, StateKey};

use crate::{Ssa, ValueId, DISPATCH_BLOCK};

pub fn validate(s: &Ssa) -> Result<()> {
    let invalid = |detail: String| {
        Diagnostic::error(
            s.addr,
            rk_core::Reason::Unsupported {
                detail: format!("invalid SSA: {detail}"),
            },
        )
    };
    for (index, value) in s.values.iter().enumerate() {
        let start = value.args_start as usize;
        let end = start + value.args_len as usize;
        let args = s
            .args
            .get(start..end)
            .ok_or_else(|| invalid(format!("value {index} has invalid arguments")))?;
        if args.iter().any(|&arg| arg as usize >= s.values.len()) {
            return Err(invalid(format!("value {index} references a missing value")));
        }
        if !shape(s, index as ValueId) {
            let widths: Vec<u8> = args.iter().map(|&arg| s.value(arg).size).collect();
            return Err(invalid(format!(
                "value {index} has invalid arity or widths: {:?} {} <- {widths:?}",
                value.op, value.size,
            )));
        }
    }
    for (index, block) in s.blocks.iter().enumerate() {
        if block
            .preds
            .iter()
            .any(|&pred| pred != DISPATCH_BLOCK && pred as usize >= s.blocks.len())
            || block
                .ops
                .iter()
                .chain(&block.phis)
                .any(|&value| value as usize >= s.values.len())
        {
            return Err(invalid(format!("block {index} has an invalid reference")));
        }
        for &phi in &block.phis {
            if s.value(phi).op != Op::Phi
                || s.args(phi).len() != block.preds.len()
                || s.args(phi)
                    .iter()
                    .any(|&arg| s.value(arg).size != s.value(phi).size)
            {
                return Err(invalid(format!("block {index} has an invalid phi {phi}")));
            }
        }
    }
    for boundary in &s.boundaries {
        let start = boundary.state_start as usize;
        let end = start + boundary.state_len as usize;
        if boundary.at as usize >= s.values.len()
            || s.state.get(start..end).is_none_or(|state| {
                state
                    .iter()
                    .any(|&(_, value)| value as usize >= s.values.len())
            })
        {
            return Err(invalid("invalid boundary reference".into()));
        }
    }
    for value in 0..s.values.len() as ValueId {
        if s.boundary_index.get(&value).is_some_and(|&index| {
            s.boundaries
                .get(index as usize)
                .is_none_or(|boundary| boundary.at != value)
        }) {
            return Err(invalid("invalid boundary index".into()));
        }
    }
    dominance(s).map_err(invalid)?;
    Ok(())
}

fn shape(s: &Ssa, id: ValueId) -> bool {
    use rk_core::ir::OpCode::*;
    let value = s.value(id);
    let args = s.args(id);
    let n = args.len();
    let z = value.size;
    let size = |i: usize| s.value(args[i]).size;
    let float = |size| matches!(size, 4 | 8 | 10);
    let same = || z > 0 && args.iter().all(|&arg| s.value(arg).size == z);
    let comparison = || z == 1 && size(0) > 0 && size(0) == size(1);
    match value.op {
        Op::P(code) => match code {
            Copy | Int2Comp | IntNegate => n == 1 && same(),
            FloatNeg | FloatAbs | FloatSqrt | FloatCeil | FloatFloor | FloatRound | FloatRcp
            | FloatRsqrt => n == 1 && same() && float(z),
            IntAnd => n == 2 && z > 0 && z <= size(0) && z <= size(1),
            IntAdd | IntSub | IntXor | IntOr | IntMult | IntDiv | IntSDiv | IntRem | IntSRem => {
                n == 2 && same()
            }
            FloatAdd | FloatSub | FloatMult | FloatDiv | FloatMin | FloatMax => {
                n == 2 && same() && float(z)
            }
            IntEqual | IntNotEqual | IntSLess | IntSLessEqual | IntLess | IntLessEqual
            | IntCarry | IntSCarry | IntSBorrow => n == 2 && comparison(),
            FloatEqual | FloatNotEqual | FloatLess | FloatLessEqual => {
                n == 2 && comparison() && float(size(0))
            }
            BoolNegate => n == 1 && z == 1 && same(),
            BoolXor | BoolAnd | BoolOr => n == 2 && z == 1 && same(),
            IntLeft | IntRight | IntSRight => n == 2 && z > 0 && size(0) == z && size(1) > 0,
            IntZExt | IntSExt => n == 1 && size(0) > 0 && z > size(0),
            Piece => {
                n == 2
                    && size(0) > 0
                    && size(1) > 0
                    && u16::from(z) == u16::from(size(0)) + u16::from(size(1))
            }
            SubPiece => {
                n == 2
                    && z > 0
                    && size(1) > 0
                    && match s.value(args[1]).op {
                        Op::Const(offset) => offset
                            .checked_add(u64::from(z))
                            .is_some_and(|end| end <= u64::from(size(0))),
                        _ => false,
                    }
            }
            FloatNan => n == 1 && z == 1 && matches!(size(0), 4 | 8 | 10),
            FloatInt2Float => n == 1 && float(z) && size(0) > 0,
            FloatFloat2Float => n == 1 && float(z) && float(size(0)),
            FloatTrunc => n == 1 && z > 0 && float(size(0)),
            Cvtss2si | Cvttss2si | Cvtsi2ss => n == 1 && z == 4 && size(0) == 4,
            PopCount | LzCount => n == 1 && z > 0 && size(0) > 0,
            Load => n == 2 && size(0) == 0 && size(1) == 4 && z > 0,
            Store => n == 3 && size(0) == 0 && size(1) == 4 && size(2) > 0 && z == 0,
            Branch | Call => n == 0 && z == 0,
            CBranch => n == 2 && z == 0 && size(1) == 1,
            BranchInd | Return => n == 1 && z == 0 && size(0) == 4,
            CallInd | CallOther | Unknown => n == 1 && size(0) == 0 && z == 0,
        },
        Op::Input(key) => n == 0 && state_width(key, z),
        Op::Const(_) => n == 0 && z > 0,
        Op::Target(_) => n == 0 && z == 4,
        Op::Phi => args.iter().all(|&arg| s.value(arg).size == z),
        Op::Byte(lane) => n == 1 && z == 1 && lane < size(0),
        Op::Pack => n == z as usize && z > 0 && args.iter().all(|&arg| s.value(arg).size == 1),
        Op::Memory => {
            n >= 1
                && z == 0
                && (size(0) == 0 || rk_ir::properties(s.value(args[0]).op).memory)
                && args.iter().skip(1).all(|&arg| s.value(arg).size == 0)
        }
        Op::StateReload(key) => n == 1 && state_width(key, z),
        Op::StateKeep(key) => n == 2 && state_width(key, z) && size(1) == z,
        Op::CcKind(_) => n == 0 && z == 1,
        Op::Call { .. } | Op::String(_) | Op::Seh(_) | Op::Slow(_) | Op::CallOther(_) => {
            n == 1 && size(0) == 0 && z == 0
        }
        Op::CallInd => n == 2 && size(0) == 0 && size(1) == 4 && z == 0,
        Op::Branch { .. }
        | Op::Tail { .. }
        | Op::ImportTail { .. }
        | Op::Return
        | Op::Trap(_)
        | Op::Breakpoint => n == 0 && z == 0,
        Op::CBranch { .. } => n == 1 && size(0) == 1 && z == 0,
        Op::BranchInd => n == 1 && size(0) == 4 && z == 0,
        Op::Div { size: width, .. } | Op::Idiv { size: width, .. } => {
            matches!(width, 1 | 2 | 4)
                && n == 4
                && size(0) == 0
                && size(1) == width * 2
                && size(2) == width * 2
                && size(3) == 4
                && z == 8
        }
        Op::X87(op) => {
            let memory = usize::from(op.touches_memory());
            let address =
                memory != 0 && !matches!(op, rk_ir::x87::X87Op::Compare { zero: true, .. });
            n == memory + usize::from(address)
                && z == op.result_size()
                && (memory == 0 || size(0) == 0)
                && (!address || size(1) == 4)
        }
    }
}

fn state_width(key: StateKey, size: u8) -> bool {
    match key {
        StateKey::Memory => size == 0,
        StateKey::Register(_) => matches!(size, 1 | 2 | 4 | 8 | 10 | 16),
    }
}

fn dominance(s: &Ssa) -> std::result::Result<(), String> {
    let root = s.blocks.len();
    let mut succ = vec![Vec::new(); root + 1];
    for (block, item) in s.blocks.iter().enumerate() {
        for &pred in &item.preds {
            let pred = if pred == DISPATCH_BLOCK {
                root
            } else {
                pred as usize
            };
            succ[pred].push(block);
        }
    }
    let mut seen = vec![false; root + 1];
    let mut post = Vec::new();
    let mut work = vec![(root, false)];
    while let Some((block, exit)) = work.pop() {
        if exit {
            post.push(block);
            continue;
        }
        if std::mem::replace(&mut seen[block], true) {
            continue;
        }
        work.push((block, true));
        work.extend(succ[block].iter().map(|&next| (next, false)));
    }
    if seen.iter().any(|&seen| !seen) {
        return Err("block unreachable from dispatch".into());
    }
    post.reverse();
    let mut rank = vec![0; root + 1];
    for (i, &block) in post.iter().enumerate() {
        rank[block] = i;
    }
    let mut idom = vec![None; root + 1];
    idom[root] = Some(root);
    loop {
        let mut changed = false;
        for &block in post.iter().skip(1) {
            let mut parents = s.blocks[block]
                .preds
                .iter()
                .map(|&pred| {
                    if pred == DISPATCH_BLOCK {
                        root
                    } else {
                        pred as usize
                    }
                })
                .filter(|&pred| idom[pred].is_some());
            let Some(mut parent) = parents.next() else {
                continue;
            };
            for mut other in parents {
                while parent != other {
                    while rank[parent] > rank[other] {
                        parent = idom[parent].unwrap();
                    }
                    while rank[other] > rank[parent] {
                        other = idom[other].unwrap();
                    }
                }
            }
            if idom[block] != Some(parent) {
                idom[block] = Some(parent);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut tree = vec![Vec::new(); root + 1];
    for (block, parent) in idom.iter().enumerate().take(root) {
        tree[parent.unwrap()].push(block);
    }
    let mut enter = vec![0; root + 1];
    let mut leave = enter.clone();
    let mut clock = 0;
    work.push((root, false));
    while let Some((block, exit)) = work.pop() {
        if exit {
            leave[block] = clock;
            continue;
        }
        enter[block] = clock;
        clock += 1;
        work.push((block, true));
        work.extend(tree[block].iter().map(|&child| (child, false)));
    }
    let mut defs = vec![None; s.values.len()];
    for (block, item) in s.blocks.iter().enumerate() {
        for (&id, position) in item
            .phis
            .iter()
            .map(|id| (id, 0))
            .chain(item.ops.iter().enumerate().map(|(i, id)| (id, i + 1)))
        {
            if defs[id as usize].replace((block, position)).is_some() {
                return Err(format!("value {id} has multiple definitions"));
            }
        }
    }
    let mut checked = rk_core::Lookup::<(ValueId, usize), usize>::default();
    let mut active = vec![false; s.values.len()];
    let mut available = |id: ValueId,
                         block: usize,
                         position: usize|
     -> std::result::Result<(), String> {
        let mut pending = vec![(id, false)];
        while let Some((value, exit)) = pending.pop() {
            if exit {
                active[value as usize] = false;
                checked.insert((value, block), position);
                continue;
            }
            if checked
                .get(&(value, block))
                .is_some_and(|&prior| prior <= position)
            {
                continue;
            }
            let op = s.value(value).op;
            if matches!(op, Op::Input(_) | Op::Const(_) | Op::Target(_)) {
                continue;
            }
            if let Some((owner, at)) = defs[value as usize] {
                if !(enter[owner] <= enter[block] && enter[block] < leave[owner])
                    || (owner == block && at >= position && at != 0)
                {
                    return Err(format!("value {value} does not dominate use of {id} in block {block} at {position}"));
                }
                continue;
            }
            if op == Op::Phi
                || (!rk_ir::properties(op).pure
                    && !matches!(op, Op::Memory | Op::StateReload(_) | Op::StateKeep(_)))
            {
                return Err(format!("value {value} uses an unplaced operation {op:?}"));
            }
            if std::mem::replace(&mut active[value as usize], true) {
                return Err(format!("value {value} has a cyclic definition"));
            }
            pending.push((value, true));
            pending.extend(s.args(value).iter().map(|&arg| (arg, false)));
        }
        Ok(())
    };
    for (block, item) in s.blocks.iter().enumerate() {
        for &phi in &item.phis {
            for (&arg, &pred) in s.args(phi).iter().zip(&item.preds) {
                let pred = if pred == DISPATCH_BLOCK {
                    root
                } else {
                    pred as usize
                };
                let position = s.blocks.get(pred).map_or(0, |b| b.ops.len() + 1);
                available(arg, pred, position)?;
            }
        }
        for (i, &id) in item.ops.iter().enumerate() {
            for &arg in s.args(id) {
                available(arg, block, i + 1)?;
            }
            if let Some(state) = s.boundary(id) {
                for &(_, value) in state {
                    available(value, block, i + 1)?;
                }
            }
        }
    }
    Ok(())
}
