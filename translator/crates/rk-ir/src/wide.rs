use std::collections::BTreeMap;

use rk_core::ir::{OpCode, PcodeOp, Space, Varnode};
use rk_core::Reason;

use crate::lower::{copy, op};
use crate::regs::Registers;
use crate::{CgOp, Op, SimdOp};

pub fn wide_lanes(
    value: Varnode,
    lanes: &BTreeMap<Varnode, [Varnode; 4]>,
    regs: &Registers,
    _fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<[Varnode; 4], Reason> {
    if let Some(lanes) = lanes.get(&value) {
        return Ok(*lanes);
    }
    if value.space == Space::Register {
        if !regs.vector(value.offset) {
            return Err(Reason::Unsupported {
                detail: "wide non-vector register".into(),
            });
        }
        return Ok(std::array::from_fn(|lane| {
            Varnode::new(Space::Register, value.offset + lane as u64 * 4, 4)
        }));
    }
    if value.space == Space::Const {
        return Ok(std::array::from_fn(|lane| {
            Varnode::constant(
                value.offset.checked_shr(lane as u32 * 32).unwrap_or(0) & 0xffff_ffff,
                4,
            )
        }));
    }
    Err(Reason::Unsupported {
        detail: "wide value without dword lanes".into(),
    })
}

pub fn wide_output(
    value: Varnode,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<[Varnode; 4], Reason> {
    if value.space == Space::Register {
        wide_lanes(value, &BTreeMap::new(), regs, fresh)
    } else if value.space == Space::Unique {
        Ok(std::array::from_fn(|_| fresh(4)))
    } else {
        Err(Reason::Unsupported {
            detail: "wide output space".into(),
        })
    }
}

fn lane_address(
    address: Varnode,
    lane: usize,
    fresh: &mut dyn FnMut(u32) -> Varnode,
    out: &mut Vec<PcodeOp>,
) -> Result<Varnode, Reason> {
    if address.size != 4 {
        return Err(Reason::Unsupported {
            detail: "wide memory address width".into(),
        });
    }
    if address.space == Space::Const {
        return Ok(Varnode::constant(address.offset + lane as u64 * 4, 4));
    }
    let offset = Varnode::constant(lane as u64 * 4, address.size);
    let result = fresh(address.size);
    out.push(op(OpCode::IntAdd, result, vec![address, offset]));
    Ok(result)
}

fn half_lanes(
    value: Varnode,
    lanes: &BTreeMap<Varnode, [Varnode; 2]>,
    regs: &Registers,
) -> Result<[Varnode; 2], Reason> {
    if let Some(lanes) = lanes.get(&value) {
        return Ok(*lanes);
    }
    if value.space == Space::Register {
        if !regs.vector(value.offset) {
            return Err(Reason::Unsupported {
                detail: "wide non-vector register".into(),
            });
        }
        return Ok(std::array::from_fn(|lane| {
            Varnode::new(Space::Register, value.offset + lane as u64 * 4, 4)
        }));
    }
    if value.space == Space::Const {
        return Ok(std::array::from_fn(|lane| {
            Varnode::constant((value.offset >> (lane * 32)) & 0xffff_ffff, 4)
        }));
    }
    Err(Reason::Unsupported {
        detail: "wide value without dword lanes".into(),
    })
}

fn half_output(
    value: Varnode,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<[Varnode; 2], Reason> {
    if value.space == Space::Register {
        half_lanes(value, &BTreeMap::new(), regs)
    } else if value.space == Space::Unique {
        Ok(std::array::from_fn(|_| fresh(4)))
    } else {
        Err(Reason::Unsupported {
            detail: "wide output space".into(),
        })
    }
}

fn split_wide(
    ops: &[PcodeOp],
    simd: Option<SimdOp>,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let unsupported = || Reason::Unsupported {
        detail: "unsupported wide SSE shape".into(),
    };
    let mut out = Vec::with_capacity(ops.len() * 4);
    let mut lanes: BTreeMap<Varnode, [Varnode; 4]> = BTreeMap::new();
    let mut halves: BTreeMap<Varnode, [Varnode; 2]> = BTreeMap::new();
    for item in ops {
        if item.opcode == OpCode::CallOther {
            if simd == Some(SimdOp::SignMask { lane_bytes: 4 }) {
                let output = item.output.ok_or_else(unsupported)?;
                let source = wide_lanes(
                    *item.inputs.last().ok_or_else(unsupported)?,
                    &lanes,
                    regs,
                    fresh,
                )?;
                let mut acc = Varnode::constant(0, 4);
                for lane in 0..4 {
                    let shifted = fresh(4);
                    let bit = fresh(4);
                    let placed = fresh(4);
                    let merged = fresh(4);
                    out.push(op(
                        OpCode::IntRight,
                        shifted,
                        vec![source[lane], Varnode::constant(31, 4)],
                    ));
                    out.push(op(
                        OpCode::IntAnd,
                        bit,
                        vec![shifted, Varnode::constant(1, 4)],
                    ));
                    out.push(op(
                        OpCode::IntLeft,
                        placed,
                        vec![bit, Varnode::constant(lane as u64, 4)],
                    ));
                    out.push(op(OpCode::IntOr, merged, vec![acc, placed]));
                    acc = merged;
                }
                out.push(copy(output, acc));
                continue;
            }
            if let Some(SimdOp::Float {
                opcode:
                    code @ (OpCode::FloatMin
                    | OpCode::FloatMax
                    | OpCode::FloatDiv
                    | OpCode::FloatSqrt
                    | OpCode::FloatRcp
                    | OpCode::FloatRsqrt),
                lane_bytes: 4,
            }) = simd
            {
                let output = item.output.ok_or_else(unsupported)?;
                let source = *item.inputs.last().ok_or_else(unsupported)?;
                if output.size == 16 {
                    let output_lanes = wide_output(output, regs, fresh)?;
                    if matches!(code, OpCode::FloatMin | OpCode::FloatMax | OpCode::FloatDiv) {
                        let vectors: Vec<Varnode> = item
                            .inputs
                            .iter()
                            .copied()
                            .filter(|value| value.size == 16)
                            .collect();
                        if vectors.len() < 2 {
                            return Err(unsupported());
                        }
                        let left = wide_lanes(vectors[0], &lanes, regs, fresh)?;
                        let right = wide_lanes(vectors[1], &lanes, regs, fresh)?;
                        for lane in 0..4 {
                            out.push(op(code, output_lanes[lane], vec![left[lane], right[lane]]));
                        }
                    } else {
                        let source_lanes = wide_lanes(source, &lanes, regs, fresh)?;
                        for lane in 0..4 {
                            out.push(op(code, output_lanes[lane], vec![source_lanes[lane]]));
                        }
                    }
                    lanes.insert(output, output_lanes);
                } else if output.size == 4 && source.size == 4 {
                    out.push(op(code, output, vec![source]));
                } else {
                    return Err(unsupported());
                }
                continue;
            }
        }
        if item.opcode == OpCode::SubPiece
            && item.output.is_some_and(|v| v.size == 4)
            && item.inputs.len() == 2
            && item.inputs[0].size == 16
            && item.inputs[1].space == Space::Const
        {
            let offset = item.inputs[1].offset;
            if offset % 4 != 0 || offset >= 16 {
                return Err(unsupported());
            }
            let source = wide_lanes(item.inputs[0], &lanes, regs, fresh)?;
            out.push(copy(item.output.unwrap(), source[offset as usize / 4]));
            continue;
        }
        let half_output_value = item.output.filter(|v| v.size == 8);
        let half_inputs = item
            .inputs
            .iter()
            .filter(|v| v.size == 8)
            .copied()
            .collect::<Vec<_>>();
        if half_output_value.is_some() || !half_inputs.is_empty() {
            match item.opcode {
                OpCode::Load if half_output_value.is_some() && item.inputs.len() == 1 => {
                    let output = half_output(half_output_value.unwrap(), regs, fresh)?;
                    for lane in 0..2 {
                        let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                        out.push(PcodeOp {
                            opcode: OpCode::Load,
                            output: Some(output[lane]),
                            inputs: vec![address],
                            userop: None,
                        });
                    }
                    halves.insert(half_output_value.unwrap(), output);
                }
                OpCode::Copy if half_output_value.is_some() && half_inputs.len() == 1 => {
                    let source = half_lanes(half_inputs[0], &halves, regs)?;
                    let output = half_output(half_output_value.unwrap(), regs, fresh)?;
                    for lane in 0..2 {
                        out.push(copy(output[lane], source[lane]));
                    }
                    halves.insert(half_output_value.unwrap(), output);
                }
                OpCode::Store if half_output_value.is_none() && item.inputs.len() == 2 => {
                    let source = half_lanes(item.inputs[1], &halves, regs)?;
                    for lane in 0..2 {
                        let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                        out.push(PcodeOp {
                            opcode: OpCode::Store,
                            output: None,
                            inputs: vec![address, source[lane]],
                            userop: None,
                        });
                    }
                }
                _ => return Err(unsupported()),
            }
            continue;
        }
        let wide_output_value = item.output.filter(|v| v.size == 16);
        let wide_inputs = item
            .inputs
            .iter()
            .filter(|v| v.size == 16)
            .copied()
            .collect::<Vec<_>>();
        if wide_output_value.is_none() && wide_inputs.is_empty() {
            out.push(item.clone());
            continue;
        }
        match item.opcode {
            OpCode::Load if wide_output_value.is_some() && item.inputs.len() == 1 => {
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                    out.push(PcodeOp {
                        opcode: OpCode::Load,
                        output: Some(output[lane]),
                        inputs: vec![address],
                        userop: None,
                    });
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            OpCode::Copy if wide_output_value.is_some() && wide_inputs.len() == 1 => {
                let source = wide_lanes(wide_inputs[0], &lanes, regs, fresh)?;
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    out.push(copy(output[lane], source[lane]));
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            OpCode::Store if wide_output_value.is_none() && item.inputs.len() == 2 => {
                let source = wide_lanes(item.inputs[1], &lanes, regs, fresh)?;
                for lane in 0..4 {
                    let address = lane_address(item.inputs[0], lane, fresh, &mut out)?;
                    out.push(PcodeOp {
                        opcode: OpCode::Store,
                        output: None,
                        inputs: vec![address, source[lane]],
                        userop: None,
                    });
                }
            }
            OpCode::IntAnd | OpCode::IntOr | OpCode::IntXor
                if wide_output_value.is_some() && wide_inputs.len() == 2 =>
            {
                let left = wide_lanes(wide_inputs[0], &lanes, regs, fresh)?;
                let right = wide_lanes(wide_inputs[1], &lanes, regs, fresh)?;
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    out.push(op(item.opcode, output[lane], vec![left[lane], right[lane]]));
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            OpCode::IntNegate if wide_output_value.is_some() && wide_inputs.len() == 1 => {
                let source = wide_lanes(wide_inputs[0], &lanes, regs, fresh)?;
                let output = wide_output(wide_output_value.unwrap(), regs, fresh)?;
                for lane in 0..4 {
                    out.push(op(item.opcode, output[lane], vec![source[lane]]));
                }
                lanes.insert(wide_output_value.unwrap(), output);
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(out)
}

fn split_psrldq(
    source: Varnode,
    destination: Varnode,
    shift: u8,
    regs: &Registers,
    fresh: &mut dyn FnMut(u32) -> Varnode,
) -> Result<Vec<PcodeOp>, Reason> {
    let shift = usize::from(shift);
    let source = wide_lanes(source, &BTreeMap::new(), regs, fresh)?;
    let output = wide_output(destination, regs, fresh)?;
    let mut out = Vec::new();
    let mut snapshot = [Varnode::constant(0, 4); 4];
    for lane in 0..4 {
        snapshot[lane] = fresh(4);
        out.push(copy(snapshot[lane], source[lane]));
    }
    for lane in 0..4 {
        let byte = lane * 4 + shift;
        if byte >= 16 {
            out.push(copy(output[lane], Varnode::constant(0, 4)));
            continue;
        }
        let index = byte / 4;
        let bits = (byte % 4) as u64 * 8;
        let value = if bits == 0 {
            snapshot[index]
        } else {
            let down = fresh(4);
            let up = fresh(4);
            let merged = fresh(4);
            out.push(op(
                OpCode::IntRight,
                down,
                vec![snapshot[index], Varnode::constant(bits, 4)],
            ));
            let high = if index < 3 {
                snapshot[index + 1]
            } else {
                Varnode::constant(0, 4)
            };
            out.push(op(
                OpCode::IntLeft,
                up,
                vec![high, Varnode::constant(32 - bits, 4)],
            ));
            out.push(op(OpCode::IntOr, merged, vec![down, up]));
            merged
        };
        out.push(copy(output[lane], value));
    }
    Ok(out)
}

fn temporary_start(ops: &[PcodeOp]) -> u64 {
    ops.iter()
        .flat_map(|item| item.output.iter().chain(&item.inputs))
        .filter(|v| v.space == Space::Unique)
        .map(|v| v.offset + u64::from(v.size))
        .max()
        .unwrap_or(0x80000000)
        .saturating_add(15)
        & !15
}

pub fn required(ops: &[CgOp]) -> bool {
    ops.iter().any(|item| match item.op {
        Op::Simd(_) => true,
        Op::P(_) => item.output.iter().chain(&item.inputs).any(|v| {
            v.size > 8
                || (v.size == 8 && v.space == Space::Register && Registers::get().vector(v.offset))
        }),
        _ => false,
    })
}

pub fn legalize(ops: &[CgOp], addr: u64) -> Result<Vec<CgOp>, rk_core::Diagnostic> {
    if ops.iter().any(|item| {
        matches!(item.op, Op::P(OpCode::Branch | OpCode::CBranch))
            && item.inputs.first().is_some_and(|v| v.space == Space::Const)
    }) {
        return Err(rk_core::Diagnostic::new(
            addr,
            Reason::Unsupported {
                detail: "wide p-code with internal control flow".into(),
            },
        ));
    }
    let count = ops
        .iter()
        .take_while(|item| matches!(item.op, Op::P(_) | Op::Simd(_)))
        .count();
    if count == 0 {
        return Ok(ops.to_vec());
    }
    let mut prefix = Vec::new();
    if let Some(item) = ops[..count]
        .iter()
        .find(|item| matches!(item.op, Op::Simd(SimdOp::ShiftBytesRight { .. })))
    {
        if count != 1 {
            return Err(rk_core::Diagnostic::new(
                addr,
                Reason::Unsupported {
                    detail: "mixed SIMD byte shift".into(),
                },
            ));
        }
        let Op::Simd(SimdOp::ShiftBytesRight { bytes }) = item.op else {
            unreachable!()
        };
        let output = item.output.ok_or_else(|| {
            rk_core::Diagnostic::new(
                addr,
                Reason::Unsupported {
                    detail: "SIMD byte shift without output".into(),
                },
            )
        })?;
        let source = *item.inputs.first().ok_or_else(|| {
            rk_core::Diagnostic::new(
                addr,
                Reason::Unsupported {
                    detail: "SIMD byte shift without source".into(),
                },
            )
        })?;
        let mut unique = source.offset.max(output.offset).saturating_add(0x10);
        let mut fresh = |size| {
            let value = Varnode::new(Space::Unique, unique, size);
            unique += 0x10;
            value
        };
        prefix = split_psrldq(source, output, bytes, Registers::get(), &mut fresh)
            .map_err(|reason| rk_core::Diagnostic::new(addr, reason))?;
    }
    let raw: Vec<PcodeOp> = if prefix.is_empty() {
        let mut raw = Vec::new();
        let mut unique = ops[..count]
            .iter()
            .flat_map(|item| item.output.iter().chain(&item.inputs))
            .filter(|v| v.space == Space::Unique)
            .map(|v| v.offset + u64::from(v.size))
            .max()
            .unwrap_or(0x80000000)
            .saturating_add(15)
            & !15;
        for item in &ops[..count] {
            if let Op::Simd(SimdOp::Shuffle {
                lane_bytes: 4,
                lanes,
            }) = item.op
            {
                let output = item.output.ok_or_else(|| {
                    rk_core::Diagnostic::new(
                        addr,
                        Reason::Unsupported {
                            detail: "SIMD shuffle without output".into(),
                        },
                    )
                })?;
                if output.size != 16
                    || item.inputs.is_empty()
                    || item.inputs.len() > 2
                    || item.inputs.iter().any(|v| v.size != 16)
                    || lanes
                        .iter()
                        .any(|lane| usize::from(*lane) >= item.inputs.len() * 4)
                {
                    return Err(rk_core::Diagnostic::new(
                        addr,
                        Reason::Unsupported {
                            detail: "unsupported SIMD shuffle shape".into(),
                        },
                    ));
                }
                let mut values = Vec::new();
                for lane in lanes {
                    let source = item.inputs[usize::from(lane) / 4];
                    let temp = Varnode::new(Space::Unique, unique, 4);
                    unique += 0x10;
                    raw.push(op(
                        OpCode::SubPiece,
                        temp,
                        vec![source, Varnode::constant(u64::from(lane % 4) * 4, 4)],
                    ));
                    values.push(temp);
                }
                for (lane, source) in values.into_iter().enumerate() {
                    raw.push(copy(
                        Varnode::new(output.space, output.offset + lane as u64 * 4, 4),
                        source,
                    ));
                }
            } else if let Op::Simd(SimdOp::ShuffleScalars { lane_bytes: 4 }) = item.op {
                let output = item.output.ok_or_else(|| {
                    rk_core::Diagnostic::new(
                        addr,
                        Reason::Unsupported {
                            detail: "SIMD shuffle without output".into(),
                        },
                    )
                })?;
                if output.size != 16 || item.inputs.len() != 4 {
                    return Err(rk_core::Diagnostic::new(
                        addr,
                        Reason::Unsupported {
                            detail: "unsupported SIMD shuffle shape".into(),
                        },
                    ));
                }
                for (lane, source) in item.inputs.iter().enumerate() {
                    raw.push(copy(
                        Varnode::new(output.space, output.offset + lane as u64 * 4, 4),
                        *source,
                    ));
                }
            } else {
                raw.push(PcodeOp {
                    opcode: match item.op {
                        Op::P(code) => code,
                        Op::Simd(_) => OpCode::CallOther,
                        _ => unreachable!(),
                    },
                    output: item.output,
                    inputs: item.inputs.clone(),
                    userop: None,
                });
            }
        }
        raw
    } else {
        prefix
    };
    let mut unique = temporary_start(&raw);
    let mut fresh = |size| {
        let value = Varnode::new(Space::Unique, unique, size);
        unique += 0x10;
        value
    };
    let simd = ops[..count].iter().find_map(|item| match item.op {
        Op::Simd(op) => Some(op),
        _ => None,
    });
    let expanded = split_wide(&raw, simd, Registers::get(), &mut fresh)
        .map_err(|reason| rk_core::Diagnostic::new(addr, reason))?;
    let mut result: Vec<CgOp> = expanded
        .into_iter()
        .map(|item| CgOp {
            op: Op::P(item.opcode),
            output: item.output,
            inputs: item.inputs,
        })
        .collect();
    result.extend_from_slice(&ops[count..]);
    Ok(result)
}
