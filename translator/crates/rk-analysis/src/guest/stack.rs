use std::collections::{BTreeMap, BTreeSet};

use rk_core::ir::{FunctionIr, Insn, OpCode, PcodeOp, Space, Varnode};
use rk_core::memory::{Address, Base};
use rk_core::program::Program;
use rk_core::Addr;
use rk_decode::{branch_target, Decoder};
use rk_ir::Registers;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StackState {
    sp: Option<i32>,
    bp: Option<i32>,
}

pub fn analyze(program: &Program, analysis: &rk_frontend::Analysis) -> BTreeMap<Addr, i32> {
    let Ok(decoder) = Decoder::new() else {
        return BTreeMap::new();
    };
    let universe: BTreeSet<Addr> = analysis.ir.keys().copied().collect();
    let mut dependencies = BTreeMap::new();
    let mut callers: BTreeMap<Addr, Vec<Addr>> = BTreeMap::new();
    for (&addr, result) in &analysis.ir {
        let deps = match result {
            Ok(fir) => dependencies_of(program, &decoder, fir, &universe),
            Err(_) => BTreeSet::new(),
        };
        for &dep in &deps {
            callers.entry(dep).or_default().push(addr);
        }
        dependencies.insert(addr, deps);
    }

    let mut pending: BTreeMap<Addr, usize> = dependencies
        .iter()
        .map(|(&addr, deps)| (addr, deps.len()))
        .collect();
    let mut ready: Vec<Addr> = pending
        .iter()
        .filter_map(|(&addr, &count)| (count == 0).then_some(addr))
        .collect();
    let mut verified = BTreeMap::new();
    while let Some(addr) = ready.pop() {
        if let Ok(fir) = &analysis.ir[&addr] {
            if let Ok(Some(cleanup)) = verify(program, &decoder, fir, &verified) {
                verified.insert(addr, cleanup);
            }
        }
        if let Some(rows) = callers.get(&addr) {
            for &caller in rows {
                let Some(count) = pending.get_mut(&caller) else {
                    continue;
                };
                if *count == 0 {
                    continue;
                }
                *count -= 1;
                if *count == 0 {
                    ready.push(caller);
                }
            }
        }
    }
    verified
}

pub fn return_immediates(
    program: &Program,
    analysis: &rk_frontend::Analysis,
) -> BTreeMap<Addr, i32> {
    analysis
        .ir
        .iter()
        .filter_map(|(&addr, result)| {
            let fir = result.as_ref().ok()?;
            if !closed(fir) {
                return None;
            }
            let mut returns = BTreeSet::new();
            for index in reachable(fir) {
                let insn = fir.insns.get(index as usize)?;
                let successors = fir.succ.get(index as usize)?;
                if fir
                    .exits
                    .get(&index)
                    .is_some_and(|targets| !targets.is_empty())
                {
                    return None;
                }
                if insn.mnemonic.eq_ignore_ascii_case("RET") {
                    if !successors.is_empty() {
                        return None;
                    }
                    returns.insert(ret_cleanup(program, insn)?);
                } else if successors.is_empty() && !fir.noreturn.contains_key(&index) {
                    return None;
                }
                if successors
                    .iter()
                    .any(|&next| next as usize >= fir.insns.len())
                {
                    return None;
                }
            }
            (returns.len() == 1).then(|| (addr, *returns.first().unwrap()))
        })
        .collect()
}

fn dependencies_of(
    program: &Program,
    decoder: &Decoder,
    fir: &FunctionIr,
    universe: &BTreeSet<Addr>,
) -> BTreeSet<Addr> {
    let mut out = BTreeSet::new();
    for index in reachable(fir) {
        let insn = &fir.insns[index as usize];
        let call = insn.mnemonic.eq_ignore_ascii_case("CALL");
        let tail = insn.mnemonic.eq_ignore_ascii_case("JMP") && fir.exits.contains_key(&index);
        if !call && !tail {
            continue;
        }
        if call && fir.noreturn.contains_key(&index) {
            continue;
        }
        let Some(target) = direct_target(program, decoder, insn) else {
            continue;
        };
        if universe.contains(&target) && !program.is_noreturn(target) {
            out.insert(target);
        }
    }
    out
}

fn reachable(fir: &FunctionIr) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let Some(entry) = fir.index_of(fir.addr) else {
        return seen;
    };
    let mut work = vec![entry];
    while let Some(index) = work.pop() {
        if index as usize >= fir.insns.len() || !seen.insert(index) {
            continue;
        }
        if let Some(successors) = fir.succ.get(index as usize) {
            work.extend(successors.iter().copied());
        }
    }
    seen
}

fn verify(
    program: &Program,
    decoder: &Decoder,
    fir: &FunctionIr,
    verified: &BTreeMap<Addr, i32>,
) -> std::result::Result<Option<i32>, ()> {
    if !closed(fir) {
        return Err(());
    }
    let entry = fir.index_of(fir.addr).ok_or(())?;
    let registers = Registers::get();
    let esp = registers.require("ESP");
    let ebp = registers.require("EBP");
    let mut before = BTreeMap::new();
    before.insert(
        entry,
        StackState {
            sp: Some(0),
            bp: None,
        },
    );
    let mut work = vec![entry];
    let mut exits = BTreeSet::new();
    while let Some(index) = work.pop() {
        let state = *before.get(&index).ok_or(())?;
        let insn = fir.insns.get(index as usize).ok_or(())?;
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        if mnemonic == "RET" {
            if fir
                .succ
                .get(index as usize)
                .is_some_and(|successors| !successors.is_empty())
                || fir
                    .exits
                    .get(&index)
                    .is_some_and(|targets| !targets.is_empty())
                || state.sp != Some(0)
            {
                return Err(());
            }
            exits.insert(ret_cleanup(program, insn).ok_or(())?);
            continue;
        }

        let mut after = state;
        observe_pcode(insn, &mut after, esp, ebp);
        let successors: &[u32] = fir
            .succ
            .get(index as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if mnemonic == "CALL" {
            if fir.noreturn.contains_key(&index) {
                if !successors.is_empty() {
                    return Err(());
                }
                continue;
            }
            let target = direct_target(program, decoder, insn).ok_or(())?;
            let Some(&cleanup) = verified.get(&target) else {
                if successors.is_empty() && program.is_noreturn(target) {
                    continue;
                }
                return Err(());
            };
            after.sp = after.sp.map(|sp| sp.wrapping_add(cleanup).wrapping_add(4));
            after.bp = None;
        }

        if let Some(targets) = fir.exits.get(&index).filter(|targets| !targets.is_empty()) {
            if mnemonic != "JMP" || !successors.is_empty() {
                return Err(());
            }
            let target = direct_target(program, decoder, insn).ok_or(())?;
            if targets.len() != 1 || targets[0] != target {
                return Err(());
            }
            if program.is_noreturn(target) {
                continue;
            }
            if after.sp != Some(0) {
                return Err(());
            }
            exits.insert(*verified.get(&target).ok_or(())?);
            continue;
        }

        if successors.is_empty() {
            if fir.noreturn.contains_key(&index) || mnemonic == "INT3" {
                continue;
            }
            return Err(());
        }
        for &successor in successors {
            if let Some(previous) = before.get(&successor) {
                if *previous != after {
                    return Err(());
                }
            } else {
                before.insert(successor, after);
                work.push(successor);
            }
        }
    }
    if exits.len() > 1 {
        return Err(());
    }
    Ok(exits.first().copied())
}

fn closed(fir: &FunctionIr) -> bool {
    fir.entries.is_empty()
        && fir.dynamic.is_empty()
        && fir.seh.is_empty()
        && fir.seh_handlers.is_empty()
        && fir.seh_unwind_only.is_empty()
        && fir.import_tails.is_empty()
}

fn observe_pcode(insn: &Insn, state: &mut StackState, esp: Varnode, ebp: Varnode) {
    let mut locals = BTreeMap::new();
    for op in &insn.ops {
        let mut value = evaluate(op, state, &locals, esp, ebp);
        let Some(output) = op.output else {
            continue;
        };
        if output.size != 4
            && matches!(
                value,
                Address::Known {
                    base: Base::Frame,
                    ..
                }
            )
        {
            value = Address::Unknown;
        }
        if overlaps(output, esp) {
            state.sp = if output == esp { relative(value) } else { None };
        }
        if overlaps(output, ebp) {
            state.bp = if output == ebp { relative(value) } else { None };
        }
        locals.insert(output, value);
    }
}

fn evaluate(
    op: &PcodeOp,
    state: &StackState,
    locals: &BTreeMap<Varnode, Address>,
    esp: Varnode,
    ebp: Varnode,
) -> Address {
    let input = |index: usize| {
        op.inputs
            .get(index)
            .map(|value| read_value(*value, state, locals, esp, ebp))
            .unwrap_or(Address::Unknown)
    };
    match op.opcode {
        OpCode::Copy | OpCode::IntZExt | OpCode::IntSExt => input(0),
        OpCode::IntAdd => input(0).add(input(1)),
        OpCode::IntSub => input(0).sub(input(1)),
        _ => Address::Unknown,
    }
}

fn read_value(
    value: Varnode,
    state: &StackState,
    locals: &BTreeMap<Varnode, Address>,
    esp: Varnode,
    ebp: Varnode,
) -> Address {
    if value.space == Space::Const {
        return Address::exact(Base::Absolute, constant(value) as u32);
    }
    if value == esp {
        return state
            .sp
            .map(|offset| Address::exact(Base::Frame, offset as u32))
            .unwrap_or(Address::Unknown);
    }
    if value == ebp {
        return state
            .bp
            .map(|offset| Address::exact(Base::Frame, offset as u32))
            .unwrap_or(Address::Unknown);
    }
    if value.space == Space::Register && (overlaps(value, esp) || overlaps(value, ebp)) {
        return Address::Unknown;
    }
    locals.get(&value).copied().unwrap_or(Address::Unknown)
}

fn constant(value: Varnode) -> i32 {
    let bits = value.size.saturating_mul(8).min(32);
    if bits == 0 {
        return 0;
    }
    let mask = if bits == 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << bits) - 1
    };
    let raw = value.offset & mask;
    if bits == 32 {
        raw as u32 as i32
    } else {
        let sign = 1u64 << (bits - 1);
        (raw ^ sign).wrapping_sub(sign) as i32
    }
}

fn relative(value: Address) -> Option<i32> {
    match value {
        Address::Known {
            base: Base::Frame,
            offset,
            span: 0,
        } => Some(offset as i32),
        _ => None,
    }
}

fn overlaps(left: Varnode, right: Varnode) -> bool {
    left.space == right.space
        && left.offset < right.offset.saturating_add(right.size as u64)
        && right.offset < left.offset.saturating_add(left.size as u64)
}

fn direct_target(program: &Program, decoder: &Decoder, insn: &Insn) -> Option<Addr> {
    let raw = program.image.bytes(insn.addr, insn.length as usize)?;
    let decoded = decoder.decode_span(insn.addr, raw)?;
    (decoded.length == insn.length)
        .then(|| branch_target(&decoded))
        .flatten()
}

fn ret_cleanup(program: &Program, insn: &Insn) -> Option<i32> {
    let raw = program.image.bytes(insn.addr, insn.length as usize)?;
    let raw = raw.strip_prefix(&[0xf3]).unwrap_or(raw);
    match raw {
        [0xc3] => Some(0),
        [0xc2, low, high] => Some(i32::from(u16::from_le_bytes([*low, *high]))),
        _ => None,
    }
}
