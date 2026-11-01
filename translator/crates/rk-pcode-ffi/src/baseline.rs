use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::slice;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, InstBuilder, MemFlags, Signature, SourceLoc, StackSlotData, StackSlotKind,
    Value,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

use crate::{invalidate_lifter, lift_one, set_error, RpOp, RpVarnode};

struct Compiler {
    module: JITModule,
    count: u32,
}

thread_local! {
    static COMPILER: RefCell<Option<Compiler>> = const { RefCell::new(None) };
}

fn mask(size: u32) -> Option<u64> {
    match size {
        1..=7 => Some((1u64 << (size * 8)) - 1),
        8 => Some(u64::MAX),
        _ => None,
    }
}

fn truncate(b: &mut FunctionBuilder<'_>, v: Value, size: u32) -> Option<Value> {
    let m = mask(size)?;
    Some(if m == u64::MAX {
        v
    } else {
        b.ins().band_imm(v, m as i64)
    })
}

fn signed(b: &mut FunctionBuilder<'_>, v: Value, size: u32) -> Option<Value> {
    mask(size)?;
    let shift = 64 - size * 8;
    if shift == 0 {
        Some(v)
    } else {
        let shifted = b.ins().ishl_imm(v, shift as i64);
        Some(b.ins().sshr_imm(shifted, shift as i64))
    }
}

struct Emitter<'a, 'b> {
    b: &'a mut FunctionBuilder<'b>,
    host: Value,
    cpu: Value,
    gpr_offset: u32,
    flag_offsets: [u32; 7],
    misc_offset: u32,
    read: Value,
    write: Value,
    read_sig: cranelift_codegen::ir::SigRef,
    write_sig: cranelift_codegen::ir::SigRef,
    read_mem: Value,
    write_mem: Value,
    read_mem_sig: cranelift_codegen::ir::SigRef,
    write_mem_sig: cranelift_codegen::ir::SigRef,
    ptr: cranelift_codegen::ir::Type,
    guest_base: usize,
    guest_size: u32,
    null_limit: u32,
    direct_stores: bool,
    unique: BTreeMap<u64, Value>,
    unique_vars: BTreeMap<u64, (Variable, Variable)>,
    scalar: Value,
    scalar_sig: cranelift_codegen::ir::SigRef,
}

fn split_flag(v: RpVarnode) -> Option<usize> {
    if v.space != 2 || v.size != 1 {
        return None;
    }
    match v.offset {
        0x200 => Some(0),
        0x202 => Some(1),
        0x204 => Some(2),
        0x206 => Some(3),
        0x207 => Some(4),
        0x20a => Some(5),
        0x20b => Some(6),
        _ => None,
    }
}

impl Emitter<'_, '_> {
    fn load(&mut self, v: RpVarnode) -> Option<Value> {
        mask(v.size)?;
        match v.space {
            0 => Some(
                self.b
                    .ins()
                    .iconst(types::I64, (v.offset & mask(v.size)?) as i64),
            ),
            2 if v.offset < 0x20 && v.offset % 4 + u64::from(v.size) <= 4 => {
                let offset = self.gpr_offset + (v.offset as u32 / 4) * 4;
                let word = self
                    .b
                    .ins()
                    .load(types::I32, MemFlags::new(), self.cpu, offset as i32);
                let value = self.b.ins().uextend(types::I64, word);
                let value = self.b.ins().ushr_imm(value, (v.offset % 4 * 8) as i64);
                truncate(self.b, value, v.size)
            }
            2 if split_flag(v).is_some() => {
                let offset = self.flag_offsets[split_flag(v)?] as i32;
                let value = self
                    .b
                    .ins()
                    .load(types::I32, MemFlags::new(), self.cpu, offset);
                let value = self.b.ins().uextend(types::I64, value);
                Some(self.b.ins().band_imm(value, 1))
            }
            2 => {
                let offset = self.b.ins().iconst(types::I64, v.offset as i64);
                let size = self.b.ins().iconst(types::I32, v.size as i64);
                let call = self.b.ins().call_indirect(
                    self.read_sig,
                    self.read,
                    &[self.host, offset, size],
                );
                let value = self.b.inst_results(call)[0];
                truncate(self.b, value, v.size)
            }
            3 => {
                let mut value = self.b.ins().iconst(types::I64, 0);
                for i in 0..v.size {
                    let offset = v.offset.wrapping_add(u64::from(i));
                    let byte = if self.unique_vars.is_empty() {
                        *self.unique.get(&offset)?
                    } else {
                        let (value, defined) = *self.unique_vars.get(&offset)?;
                        let defined = self.b.use_var(defined);
                        let missing = self.b.ins().icmp_imm(IntCC::Equal, defined, 0);
                        self.guard(missing, 10);
                        self.b.use_var(value)
                    };
                    let part = if i == 0 {
                        byte
                    } else {
                        self.b.ins().ishl_imm(byte, i64::from(i * 8))
                    };
                    value = self.b.ins().bor(value, part);
                }
                Some(value)
            }
            1 => {
                let addr = self.b.ins().iconst(types::I64, v.offset as i64);
                Some(self.read_memory(addr, v.size))
            }
            _ => None,
        }
    }

    fn store(&mut self, v: RpVarnode, value: Value) -> Option<()> {
        let value = truncate(self.b, value, v.size)?;
        match v.space {
            2 if v.offset < 0x20 && v.offset % 4 + u64::from(v.size) <= 4 => {
                let offset = (self.gpr_offset + (v.offset as u32 / 4) * 4) as i32;
                let shift = (v.offset % 4 * 8) as i64;
                let value = self.b.ins().ireduce(types::I32, value);
                let value = self.b.ins().ishl_imm(value, shift);
                let value = if v.size == 4 {
                    value
                } else {
                    let word = self
                        .b
                        .ins()
                        .load(types::I32, MemFlags::new(), self.cpu, offset);
                    let keep = !((mask(v.size)? as u32) << shift);
                    let word = self.b.ins().band_imm(word, i64::from(keep));
                    self.b.ins().bor(word, value)
                };
                self.b.ins().store(MemFlags::new(), value, self.cpu, offset);
            }
            2 if split_flag(v).is_some() => {
                let offset = self.flag_offsets[split_flag(v)?] as i32;
                let value = self.b.ins().band_imm(value, 1);
                let value = self.b.ins().ireduce(types::I32, value);
                self.b.ins().store(MemFlags::new(), value, self.cpu, offset);
                let misc = self.b.ins().load(
                    types::I32,
                    MemFlags::new(),
                    self.cpu,
                    self.misc_offset as i32,
                );
                let misc = self.b.ins().band_imm(misc, i64::from(!0xcd5u32));
                let misc = self.b.ins().bor_imm(misc, 2);
                self.b
                    .ins()
                    .store(MemFlags::new(), misc, self.cpu, self.misc_offset as i32);
            }
            2 => {
                let offset = self.b.ins().iconst(types::I64, v.offset as i64);
                let size = self.b.ins().iconst(types::I32, v.size as i64);
                self.b.ins().call_indirect(
                    self.write_sig,
                    self.write,
                    &[self.host, offset, size, value],
                );
            }
            3 => {
                for i in 0..v.size {
                    let part = if i == 0 {
                        value
                    } else {
                        self.b.ins().ushr_imm(value, i64::from(i * 8))
                    };
                    let byte = self.b.ins().band_imm(part, 255);
                    let offset = v.offset.wrapping_add(u64::from(i));
                    if self.unique_vars.is_empty() {
                        self.unique.insert(offset, byte);
                    } else {
                        let (value, defined) = *self.unique_vars.get(&offset)?;
                        self.b.def_var(value, byte);
                        let one = self.b.ins().iconst(types::I8, 1);
                        self.b.def_var(defined, one);
                    }
                }
            }
            1 => {
                let addr = self.b.ins().iconst(types::I64, v.offset as i64);
                self.write_memory(addr, v.size, value);
            }
            _ => return None,
        }
        Some(())
    }

    fn read_memory(&mut self, addr: Value, size: u32) -> Value {
        if let Some(ty) = memory_type(size) {
            let pointer = self.memory_pointer(addr, size);
            let value = self.b.ins().load(ty, MemFlags::new(), pointer, 0);
            return if ty == types::I64 {
                value
            } else {
                self.b.ins().uextend(types::I64, value)
            };
        }
        let slot =
            self.b
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let out = self.b.ins().stack_addr(self.ptr, slot, 0);
        let size = self.b.ins().iconst(types::I32, i64::from(size));
        let call = self.b.ins().call_indirect(
            self.read_mem_sig,
            self.read_mem,
            &[self.host, addr, size, out],
        );
        let status = self.b.inst_results(call)[0];
        self.guard(status, 2);
        self.b.ins().stack_load(types::I64, slot, 0)
    }

    fn write_memory(&mut self, addr: Value, size: u32, value: Value) {
        if self.direct_stores {
            if let Some(ty) = memory_type(size) {
                let pointer = self.memory_pointer(addr, size);
                let value = if ty == types::I64 {
                    value
                } else {
                    self.b.ins().ireduce(ty, value)
                };
                self.b.ins().store(MemFlags::new(), value, pointer, 0);
                return;
            }
        }
        let size = self.b.ins().iconst(types::I32, i64::from(size));
        let call = self.b.ins().call_indirect(
            self.write_mem_sig,
            self.write_mem,
            &[self.host, addr, size, value],
        );
        let status = self.b.inst_results(call)[0];
        self.guard(status, 2);
    }

    fn guard(&mut self, status: Value, code: i64) {
        let failed = self.b.create_block();
        let next = self.b.create_block();
        self.b.ins().brif(status, failed, &[], next, &[]);
        self.b.switch_to_block(failed);
        let result = self.b.ins().iconst(types::I32, code);
        self.b.ins().return_(&[result]);
        self.b.switch_to_block(next);
    }

    fn memory_pointer(&mut self, addr: Value, size: u32) -> Value {
        let too_wide = self
            .b
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThan, addr, i64::from(u32::MAX));
        self.guard(too_wide, 7);
        let below =
            self.b
                .ins()
                .icmp_imm(IntCC::UnsignedLessThan, addr, i64::from(self.null_limit));
        self.guard(below, 5);
        let outside = self.b.ins().icmp_imm(
            IntCC::UnsignedGreaterThan,
            addr,
            i64::from(self.guest_size - size),
        );
        self.guard(outside, 6);
        self.b.ins().iadd_imm(addr, self.guest_base as i64)
    }

    fn op(&mut self, op: &RpOp) -> Option<()> {
        if op.opcode == 2 {
            if op.input_count != 2 || op.has_output != 1 || op.inputs[0].space != 1 {
                return None;
            }
            let addr = self.load(op.inputs[1])?;
            let value = self.read_memory(addr, op.output.size);
            return self.store(op.output, value);
        }
        if op.opcode == 3 {
            if op.input_count != 3 || op.has_output != 0 || op.inputs[0].space != 1 {
                return None;
            }
            let addr = self.load(op.inputs[1])?;
            let value = self.load(op.inputs[2])?;
            self.write_memory(addr, op.inputs[2].size, value);
            return Some(());
        }
        if scalar_helper(op.opcode) {
            let a = self.load(op.inputs[0])?;
            let b = if op.input_count > 1 {
                self.load(op.inputs[1])?
            } else {
                self.b.ins().iconst(types::I64, 0)
            };
            let descriptor = u64::from(op.opcode)
                | u64::from(op.inputs[0].size) << 8
                | u64::from(op.output.size) << 16
                | u64::from(op.input_count) << 24
                | if matches!(op.opcode, 70 | 71) {
                    op.inputs[2].offset << 32
                } else {
                    0
                };
            let descriptor = self.b.ins().iconst(types::I64, descriptor as i64);
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                3,
            ));
            let out = self.b.ins().stack_addr(self.ptr, slot, 0);
            let call = self.b.ins().call_indirect(
                self.scalar_sig,
                self.scalar,
                &[self.host, descriptor, a, b, out],
            );
            let status = self.b.inst_results(call)[0];
            self.guard(status, 2);
            let value = self.b.ins().stack_load(types::I64, slot, 0);
            return self.store(op.output, value);
        }
        if op.has_output != 1 || op.input_count == 0 || op.input_count > 2 {
            return None;
        }
        let unary = matches!(op.opcode, 1 | 17 | 18 | 24 | 25 | 37 | 61 | 62);
        if op.input_count != if unary { 1 } else { 2 } {
            return None;
        }
        match op.opcode {
            1 | 24 | 25 if op.output.size != op.inputs[0].size => return None,
            11..=16 | 21..=23 | 37..=40 if op.output.size != 1 => return None,
            19 | 20 | 26 | 28..=32 if op.output.size != op.inputs[0].size => return None,
            27 if op.output.size > op.inputs[0].size => return None,
            59 if op.inputs[0].size + op.inputs[1].size != op.output.size => return None,
            60 if op.output.size > op.inputs[0].size => return None,
            _ => {}
        }
        let a = self.load(op.inputs[0])?;
        let right = if unary {
            None
        } else {
            Some(self.load(op.inputs[1])?)
        };
        let out = match op.opcode {
            1 | 17 => a,
            18 => signed(self.b, a, op.inputs[0].size)?,
            24 => self.b.ins().ineg(a),
            25 => self.b.ins().bnot(a),
            37 => self.b.ins().bxor_imm(a, 1),
            61 => self.b.ins().popcnt(a),
            62 => {
                let v = self.b.ins().clz(a);
                self.b
                    .ins()
                    .iadd_imm(v, -i64::from(64 - op.inputs[0].size * 8))
            }
            11..=16 => {
                if op.inputs[0].size != op.inputs[1].size {
                    return None;
                }
                let mut lhs = a;
                let mut rhs = right?;
                let cc = match op.opcode {
                    11 => IntCC::Equal,
                    12 => IntCC::NotEqual,
                    13 => {
                        lhs = signed(self.b, lhs, op.inputs[0].size)?;
                        rhs = signed(self.b, rhs, op.inputs[1].size)?;
                        IntCC::SignedLessThan
                    }
                    14 => {
                        lhs = signed(self.b, lhs, op.inputs[0].size)?;
                        rhs = signed(self.b, rhs, op.inputs[1].size)?;
                        IntCC::SignedLessThanOrEqual
                    }
                    15 => IntCC::UnsignedLessThan,
                    _ => IntCC::UnsignedLessThanOrEqual,
                };
                let flag = self.b.ins().icmp(cc, lhs, rhs);
                self.b.ins().uextend(types::I64, flag)
            }
            21..=23 => {
                if op.inputs[0].size != op.inputs[1].size {
                    return None;
                }
                let rhs = right?;
                let result = if op.opcode == 23 {
                    self.b.ins().isub(a, rhs)
                } else {
                    self.b.ins().iadd(a, rhs)
                };
                let result = truncate(self.b, result, op.inputs[0].size)?;
                if op.opcode == 21 {
                    let flag = self.b.ins().icmp(IntCC::UnsignedLessThan, result, a);
                    self.b.ins().uextend(types::I64, flag)
                } else {
                    let ab = self.b.ins().bxor(a, rhs);
                    let ab = if op.opcode == 22 {
                        self.b.ins().bnot(ab)
                    } else {
                        ab
                    };
                    let ar = self.b.ins().bxor(a, result);
                    let sign = self.b.ins().band(ab, ar);
                    let sign = self
                        .b
                        .ins()
                        .ushr_imm(sign, i64::from(op.inputs[0].size * 8 - 1));
                    self.b.ins().band_imm(sign, 1)
                }
            }
            29..=31 => {
                let count = right?;
                let bits = op.inputs[0].size * 8;
                let large = self.b.ins().icmp_imm(
                    IntCC::UnsignedGreaterThanOrEqual,
                    count,
                    i64::from(bits),
                );
                let input = if op.opcode == 31 {
                    signed(self.b, a, op.inputs[0].size)?
                } else {
                    a
                };
                let shifted = match op.opcode {
                    29 => self.b.ins().ishl(input, count),
                    30 => self.b.ins().ushr(input, count),
                    _ => self.b.ins().sshr(input, count),
                };
                let fill = if op.opcode == 31 {
                    self.b.ins().sshr_imm(input, 63)
                } else {
                    self.b.ins().iconst(types::I64, 0)
                };
                self.b.ins().select(large, fill, shifted)
            }
            59 => {
                let high = self.b.ins().ishl_imm(a, i64::from(op.inputs[1].size * 8));
                self.b.ins().bor(high, right?)
            }
            60 => {
                let count = right?;
                let large = self
                    .b
                    .ins()
                    .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, count, 8);
                let bits = self.b.ins().ishl_imm(count, 3);
                let shifted = self.b.ins().ushr(a, bits);
                let zero = self.b.ins().iconst(types::I64, 0);
                self.b.ins().select(large, zero, shifted)
            }
            19 | 20 | 26..=28 | 32 | 38..=40 => {
                if op.inputs[0].size != op.inputs[1].size {
                    return None;
                }
                let rhs = right?;
                match op.opcode {
                    19 => self.b.ins().iadd(a, rhs),
                    20 => self.b.ins().isub(a, rhs),
                    26 | 38 => self.b.ins().bxor(a, rhs),
                    27 | 39 => self.b.ins().band(a, rhs),
                    28 | 40 => self.b.ins().bor(a, rhs),
                    _ => self.b.ins().imul(a, rhs),
                }
            }
            _ => return None,
        };
        let out = if matches!(op.opcode, 37..=40) {
            self.b.ins().band_imm(out, 1)
        } else {
            out
        };
        self.store(op.output, out)
    }
}

#[repr(C)]
pub struct RpBaselineBlock {
    code: *const c_void,
    count: u32,
    byte_count: u32,
    lengths: [u8; 16],
    bytes: [u8; 240],
}

#[repr(C)]
pub struct RpBaselineRejection {
    reason: u32,
    opcode: u32,
    length: u32,
    mnemonic: [u8; 48],
    detail: [u8; 128],
}

fn copy_text(dest: &mut [u8], text: &str) {
    let n = text.len().min(dest.len() - 1);
    dest[..n].copy_from_slice(&text.as_bytes()[..n]);
    dest[n] = 0;
}

fn memory_type(size: u32) -> Option<cranelift_codegen::ir::Type> {
    Some(match size {
        1 => types::I8,
        2 => types::I16,
        4 => types::I32,
        8 => types::I64,
        _ => return None,
    })
}

fn scalar_helper(opcode: u32) -> bool {
    matches!(opcode, 33..=36 | 41..=52 | 63 | 64 | 68..=71)
}

fn internal_branch(op: &RpOp) -> bool {
    matches!(op.opcode, 4 | 5) && op.input_count > 0 && op.inputs[0].space == 0
}

fn internal_target(index: usize, op: &RpOp, len: usize) -> Option<usize> {
    let target = op.inputs[0];
    if !(1..=8).contains(&target.size) {
        return None;
    }
    let shift = 64 - target.size * 8;
    let offset = ((target.offset << shift) as i64) >> shift;
    let target = (index as i64).checked_add(offset)?;
    (target >= 0 && target <= len as i64).then_some(target as usize)
}

fn register_supported(v: RpVarnode) -> bool {
    (v.offset < 0x20 && v.offset + u64::from(v.size) <= 0x20 && v.size <= 4)
        || (v.offset >= 0x200 && v.offset <= 0x214 && v.offset != 0x20c && v.size == 1)
        || (v.offset == 0x110 && v.size == 4)
        || ((v.offset == 0x280 || v.offset == 0x284) && (v.size == 2 || v.size == 4))
        || (v.offset == 0x300 && v.size == 2)
        || (v.offset >= 0x1100
            && v.offset < 0x1180
            && v.offset % 16 < 8
            && u64::from(v.size) <= 8 - v.offset % 16)
        || (v.offset >= 0x1200
            && v.offset < 0x1400
            && v.offset % 64 < 16
            && u64::from(v.size) <= 16 - v.offset % 64)
        || (v.offset >= 0xffff0000
            && v.offset < 0xffff0004
            && u64::from(v.size) <= 0xffff0004 - v.offset)
}

fn supported_userop(op: &RpOp) -> bool {
    if op.opcode != 9
        || op.has_userop != 1
        || op.input_count == 0
        || op.inputs[0].space != 0
        || op.inputs[0].offset != op.userop
    {
        return false;
    }
    if (0x1000..0x102d).contains(&op.userop) || op.userop >= u64::MAX - 4 {
        return op.input_count == 1 && op.has_output == 0;
    }
    if op.userop >> 32 != 1 {
        return false;
    }
    let kind = op.userop & 255;
    let a = (op.userop >> 8) & 255;
    let bits = (op.userop >> 16) & 255;
    let flags = (op.userop >> 24) & 15;
    let extra = (op.userop >> 28) & 15;
    let real = matches!(bits, 32 | 64 | 80);
    let integer = matches!(bits, 16 | 32 | 64);
    let memory = match kind {
        1 if flags == 0 && extra == 0 && a < 8 && bits == 0 => false,
        1 if flags == 0 && extra == 0 && a == 8 && real => true,
        2 if a == 0 && flags == 0 && extra == 0 && integer => true,
        3 if a < 8 && bits == 0 && flags <= 1 && extra == 0 => false,
        3 if a == 8 && flags <= 1 && (extra == 0 && real || extra == 1 && integer) => true,
        4 if a & 15 < 8 && a >> 4 < 8 && extra <= 1 && bits == 0 && flags & 8 == 0 => false,
        4 if a == 0
            && extra == 0
            && (flags & 8 == 0 && matches!(bits, 32 | 64)
                || flags & 8 != 0 && matches!(bits, 16 | 32)) =>
        {
            true
        }
        5 if a < 8 && bits == 0 && flags & !5 == 0 && extra <= 2 => false,
        5 if a == 0
            && flags & !3 == 0
            && extra <= 1
            && (flags & 2 == 0 && matches!(bits, 32 | 64)
                || flags & 2 != 0 && matches!(bits, 16 | 32)) =>
        {
            true
        }
        6 if a < 8
            && bits == 0
            && flags <= 1
            && extra <= 1
            && op.has_output == 1
            && op.output.size == 2 =>
        {
            false
        }
        9 if a == 0
            && bits == 0
            && flags == 0
            && extra == 0
            && op.has_output == 1
            && op.output.size == 2 =>
        {
            false
        }
        7 | 8 if a < 8 && bits == 0 && flags == 0 && extra == 0 => false,
        10 | 12 | 16 | 18..=20 if a == 0 && bits == 0 && flags == 0 && extra == 0 => false,
        11 | 17 if a <= 1 && bits == 0 && flags == 0 && extra == 0 => false,
        13 | 14 if a <= 6 && bits == 0 && flags == 0 && extra == 0 => false,
        15 if a == 0 && bits == 16 && flags == 0 && extra == 0 => true,
        _ => return false,
    };
    op.input_count == if memory { 2 } else { 1 } && (matches!(kind, 6 | 9) || op.has_output == 0)
}

fn eligible(ops: &[RpOp]) -> Result<(), (u32, u32)> {
    let mut unique = BTreeSet::new();
    let internal = ops.iter().any(internal_branch);
    let value = |v: RpVarnode, write: bool, unique: &BTreeSet<u64>| -> Option<u32> {
        if mask(v.size).is_none() {
            return Some(3);
        }
        match v.space {
            0 if !write => None,
            1 if v.offset <= u64::from(u32::MAX) => None,
            2 if register_supported(v) => None,
            3 if write
                || internal
                || (0..v.size).all(|i| unique.contains(&v.offset.wrapping_add(u64::from(i)))) =>
            {
                None
            }
            3 => Some(4),
            _ => Some(3),
        }
    };
    for (index, op) in ops.iter().enumerate() {
        if op.input_count > 4 {
            return Err((3, op.opcode));
        }
        if op.opcode == 9 {
            if !supported_userop(op) {
                return Err((2, op.opcode));
            }
            if op.input_count == 2 {
                if let Some(reason) = value(op.inputs[1], false, &unique) {
                    return Err((reason, op.opcode));
                }
            }
            if op.has_output != 0 {
                if let Some(reason) = value(op.output, true, &unique) {
                    return Err((reason, op.opcode));
                }
                if op.output.space == 3 {
                    for i in 0..op.output.size {
                        unique.insert(op.output.offset.wrapping_add(u64::from(i)));
                    }
                }
            }
            continue;
        }
        if matches!(op.opcode, 4..=8 | 10) {
            let is_internal = internal_branch(op);
            let direct = matches!(op.opcode, 4 | 5 | 7);
            if (!is_internal && index + 1 != ops.len())
                || op.has_output != 0
                || op.input_count != if op.opcode == 5 { 2 } else { 1 }
                || (is_internal && internal_target(index, op, ops.len()).is_none())
                || (direct
                    && !is_internal
                    && (op.inputs[0].space != 1 || op.inputs[0].offset > u32::MAX.into()))
            {
                return Err((1, op.opcode));
            }
            let first_value = usize::from(direct);
            for i in first_value..op.input_count as usize {
                if let Some(reason) = value(op.inputs[i], false, &unique) {
                    return Err((reason, op.opcode));
                }
            }
            continue;
        }
        if scalar_helper(op.opcode) {
            let unary = matches!(op.opcode, 45 | 50..=52 | 68 | 69);
            let count = if matches!(op.opcode, 70 | 71) {
                3
            } else if unary {
                1
            } else {
                2
            };
            if op.has_output != 1 || op.input_count != count {
                return Err((3, op.opcode));
            }
            if matches!(op.opcode, 70 | 71) {
                if op.output.size != 8
                    || op.inputs[2].space != 0
                    || !matches!(op.inputs[2].offset, 1 | 2 | 4)
                    || u64::from(op.inputs[0].size) != op.inputs[2].offset * 2
                    || op.inputs[0].size != op.inputs[1].size
                {
                    return Err((3, op.opcode));
                }
            } else if matches!(op.opcode, 33..=36) {
                if op.output.size != op.inputs[0].size || op.inputs[0].size != op.inputs[1].size {
                    return Err((3, op.opcode));
                }
            } else {
                let out_size = if matches!(op.opcode, 41..=45) { 1 } else { 4 };
                if op.output.size != out_size
                    || op.inputs[..count as usize].iter().any(|v| v.size != 4)
                {
                    return Err((3, op.opcode));
                }
            }
            for input in &op.inputs[..count as usize] {
                if let Some(reason) = value(*input, false, &unique) {
                    return Err((reason, op.opcode));
                }
            }
            if let Some(reason) = value(op.output, true, &unique) {
                return Err((reason, op.opcode));
            }
            if op.output.space == 3 {
                for i in 0..op.output.size {
                    unique.insert(op.output.offset.wrapping_add(u64::from(i)));
                }
            }
            continue;
        }
        let memory = op.opcode == 2 || op.opcode == 3;
        let unary = matches!(op.opcode, 1 | 17 | 18 | 24 | 25 | 37 | 61 | 62);
        if memory {
            if op.inputs[0].space != 1
                || (op.opcode == 2 && (op.input_count != 2 || op.has_output != 1))
                || (op.opcode == 3 && (op.input_count != 3 || op.has_output != 0))
            {
                return Err((3, op.opcode));
            }
        } else {
            if !matches!(op.opcode, 1 | 11..=32 | 37..=40 | 59..=62) {
                return Err((
                    if matches!(op.opcode, 4..=8 | 10) {
                        1
                    } else {
                        2
                    },
                    op.opcode,
                ));
            }
            if op.has_output != 1 || op.input_count != if unary { 1 } else { 2 } {
                return Err((3, op.opcode));
            }
            match op.opcode {
                1 | 24 | 25 if op.output.size != op.inputs[0].size => return Err((3, op.opcode)),
                11..=16 | 21..=23 | 37..=40 if op.output.size != 1 => return Err((3, op.opcode)),
                19 | 20 | 26 | 28..=32 if op.output.size != op.inputs[0].size => {
                    return Err((3, op.opcode))
                }
                27 if op.output.size > op.inputs[0].size => return Err((3, op.opcode)),
                59 if op.inputs[0].size + op.inputs[1].size != op.output.size => {
                    return Err((3, op.opcode))
                }
                60 if op.output.size > op.inputs[0].size => return Err((3, op.opcode)),
                _ => {}
            }
            if matches!(op.opcode, 11..=16 | 19..=23 | 26..=28 | 32 | 38..=40)
                && op.inputs[0].size != op.inputs[1].size
            {
                return Err((3, op.opcode));
            }
        }
        for i in usize::from(memory)..op.input_count as usize {
            if let Some(reason) = value(op.inputs[i], false, &unique) {
                return Err((reason, op.opcode));
            }
        }
        if op.has_output != 0 {
            if let Some(reason) = value(op.output, true, &unique) {
                return Err((reason, op.opcode));
            }
            if op.output.space == 3 {
                for i in 0..op.output.size {
                    unique.insert(op.output.offset.wrapping_add(u64::from(i)));
                }
            }
        }
    }
    Ok(())
}

fn compile(
    compiler: &mut Compiler,
    pc: u32,
    instructions: &[Vec<RpOp>],
    lengths: &[u8],
    gpr_offset: u32,
    flag_offsets: [u32; 7],
    misc_offset: u32,
    eip_offset: u32,
    clock_ns: extern "C" fn() -> u64,
    pid: u32,
    tid: u32,
    guest_base: usize,
    guest_size: u32,
    null_limit: u32,
    direct_stores: u32,
    x87_select: extern "C" fn(u64) -> usize,
) -> Result<Option<*const u8>, String> {
    if compiler.count >= 65536 {
        return Ok(None);
    }
    let profiling = crate::baseline_jitdump::enabled();
    let ptr = compiler.module.target_config().pointer_type();
    let mut signature = compiler.module.make_signature();
    for _ in 0..10 {
        signature.params.push(AbiParam::new(ptr));
    }
    signature.returns.push(AbiParam::new(types::I32));
    let mut ctx = compiler.module.make_context();
    ctx.func.signature = signature.clone();
    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = b.create_block();
        b.switch_to_block(block);
        b.append_block_params_for_function_params(block);
        let args = b.block_params(block).to_vec();
        let mut read_sig = Signature::new(signature.call_conv);
        read_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(types::I64),
            AbiParam::new(types::I32),
        ]);
        read_sig.returns.push(AbiParam::new(types::I64));
        let read_sig = b.import_signature(read_sig);
        let mut write_sig = Signature::new(signature.call_conv);
        write_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(types::I64),
            AbiParam::new(types::I32),
            AbiParam::new(types::I64),
        ]);
        let write_sig = b.import_signature(write_sig);
        let mut read_mem_sig = Signature::new(signature.call_conv);
        read_mem_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(types::I64),
            AbiParam::new(types::I32),
            AbiParam::new(ptr),
        ]);
        read_mem_sig.returns.push(AbiParam::new(types::I32));
        let read_mem_sig = b.import_signature(read_mem_sig);
        let mut write_mem_sig = Signature::new(signature.call_conv);
        write_mem_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(types::I64),
            AbiParam::new(types::I32),
            AbiParam::new(types::I64),
        ]);
        write_mem_sig.returns.push(AbiParam::new(types::I32));
        let write_mem_sig = b.import_signature(write_mem_sig);
        let mut x87_sig = Signature::new(signature.call_conv);
        x87_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(types::I64),
            AbiParam::new(types::I64),
            AbiParam::new(types::I32),
        ]);
        x87_sig.params.push(AbiParam::new(ptr));
        x87_sig.returns.push(AbiParam::new(types::I32));
        let x87_sig = b.import_signature(x87_sig);
        let mut x87_special_sig = Signature::new(signature.call_conv);
        x87_special_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(types::I64),
            AbiParam::new(types::I32),
        ]);
        x87_special_sig.returns.push(AbiParam::new(types::I32));
        let x87_special_sig = b.import_signature(x87_special_sig);
        let mut scalar_sig = Signature::new(signature.call_conv);
        scalar_sig.params.extend([
            AbiParam::new(ptr),
            AbiParam::new(types::I64),
            AbiParam::new(types::I64),
            AbiParam::new(types::I64),
            AbiParam::new(ptr),
        ]);
        scalar_sig.returns.push(AbiParam::new(types::I32));
        let scalar_sig = b.import_signature(scalar_sig);
        let mut instruction_pc = pc;
        for (index, ops) in instructions.iter().enumerate() {
            if profiling {
                b.set_srcloc(SourceLoc::new(index as u32 + 1));
            }
            let mut emit = Emitter {
                b: &mut b,
                host: args[0],
                cpu: args[6],
                gpr_offset,
                flag_offsets,
                misc_offset,
                read: args[1],
                write: args[2],
                read_sig,
                write_sig,
                read_mem: args[4],
                write_mem: args[5],
                read_mem_sig,
                write_mem_sig,
                ptr,
                guest_base,
                guest_size,
                null_limit,
                direct_stores: direct_stores != 0,
                unique: BTreeMap::new(),
                unique_vars: BTreeMap::new(),
                scalar: args[9],
                scalar_sig,
            };
            let current_pc = emit.b.ins().iconst(types::I32, i64::from(instruction_pc));
            let length = emit.b.ins().iconst(types::I32, i64::from(lengths[index]));
            let index_value = emit.b.ins().iconst(types::I32, index as i64);
            emit.b.ins().store(MemFlags::new(), current_pc, args[3], 0);
            emit.b.ins().store(MemFlags::new(), length, args[3], 4);
            emit.b.ins().store(MemFlags::new(), index_value, args[3], 8);
            emit.b
                .ins()
                .store(MemFlags::new(), current_pc, args[6], eip_offset as i32);
            instruction_pc = instruction_pc.wrapping_add(u32::from(lengths[index]));
            let internal = ops.iter().any(internal_branch);
            let blocks: Vec<_> = if internal {
                (0..=ops.len()).map(|_| emit.b.create_block()).collect()
            } else {
                Vec::new()
            };
            let budget = if internal {
                for op in ops {
                    for v in op.inputs[..op.input_count as usize]
                        .iter()
                        .chain((op.has_output != 0).then_some(&op.output))
                    {
                        if v.space == 3 {
                            for byte in 0..v.size {
                                let offset = v.offset.wrapping_add(u64::from(byte));
                                emit.unique_vars.entry(offset).or_insert_with(|| {
                                    let value = emit.b.declare_var(types::I64);
                                    let defined = emit.b.declare_var(types::I8);
                                    let zero = emit.b.ins().iconst(types::I64, 0);
                                    emit.b.def_var(value, zero);
                                    let zero = emit.b.ins().iconst(types::I8, 0);
                                    emit.b.def_var(defined, zero);
                                    (value, defined)
                                });
                            }
                        }
                    }
                }
                let budget = emit.b.declare_var(types::I32);
                let value = emit.b.ins().iconst(types::I32, 65536);
                emit.b.def_var(budget, value);
                Some(budget)
            } else {
                None
            };
            for (op_index, op) in ops.iter().enumerate() {
                if internal {
                    emit.b.ins().jump(blocks[op_index], &[]);
                    emit.b.switch_to_block(blocks[op_index]);
                    let value = emit.b.use_var(budget.unwrap());
                    let exhausted = emit.b.ins().icmp_imm(IntCC::Equal, value, 0);
                    emit.guard(exhausted, 11);
                    let value = emit.b.ins().iadd_imm(value, -1);
                    emit.b.def_var(budget.unwrap(), value);
                }
                if internal_branch(op) {
                    let target = internal_target(op_index, op, ops.len())
                        .ok_or("invalid internal branch")?;
                    if op.opcode == 5 {
                        let condition = emit
                            .load(op.inputs[1])
                            .ok_or("invalid internal condition")?;
                        emit.b.ins().brif(
                            condition,
                            blocks[target],
                            &[],
                            blocks[op_index + 1],
                            &[],
                        );
                    } else {
                        emit.b.ins().jump(blocks[target], &[]);
                    }
                    let unreachable = emit.b.create_block();
                    emit.b.switch_to_block(unreachable);
                    continue;
                }
                if op.opcode == 9 {
                    let id = emit.b.ins().iconst(types::I64, op.userop as i64);
                    let argument = if op.input_count == 2 {
                        emit.load(op.inputs[1]).ok_or("invalid userop argument")?
                    } else {
                        emit.b.ins().iconst(types::I64, 0)
                    };
                    if op.userop >> 32 == 1 && op.has_output == 0 {
                        let specialized = x87_select(op.userop);
                        if specialized != 0 {
                            let target = emit.b.ins().iconst(ptr, specialized as i64);
                            let operand = (op.userop >> 8) & 255 | ((op.userop >> 28) & 15) << 8;
                            let operand = emit.b.ins().iconst(types::I32, operand as i64);
                            let call = emit.b.ins().call_indirect(
                                x87_special_sig,
                                target,
                                &[args[0], argument, operand],
                            );
                            let status = emit.b.inst_results(call)[0];
                            emit.guard(status, 2);
                            continue;
                        }
                    }
                    let slot = emit.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        3,
                    ));
                    let out = emit.b.ins().stack_addr(ptr, slot, 0);
                    let count = emit.b.ins().iconst(types::I32, i64::from(op.input_count));
                    let call = emit.b.ins().call_indirect(
                        x87_sig,
                        args[8],
                        &[args[0], id, argument, count, out],
                    );
                    let status = emit.b.inst_results(call)[0];
                    emit.guard(status, 2);
                    if op.has_output != 0 {
                        let value = emit.b.ins().stack_load(types::I64, slot, 0);
                        emit.store(op.output, value)
                            .ok_or("invalid userop output")?;
                    }
                    continue;
                }
                if matches!(op.opcode, 4..=8 | 10) {
                    let target = if matches!(op.opcode, 6 | 8 | 10) {
                        emit.load(op.inputs[0]).ok_or("invalid branch target")?
                    } else {
                        emit.b.ins().iconst(types::I64, op.inputs[0].offset as i64)
                    };
                    if op.opcode == 5 {
                        let condition =
                            emit.load(op.inputs[1]).ok_or("invalid branch condition")?;
                        let taken = emit.b.create_block();
                        let fallthrough = emit.b.create_block();
                        emit.b.ins().brif(condition, taken, &[], fallthrough, &[]);
                        emit.b.switch_to_block(taken);
                        emit.b.ins().store(MemFlags::new(), target, args[7], 0);
                        let status = emit.b.ins().iconst(types::I32, 3);
                        emit.b.ins().return_(&[status]);
                        emit.b.switch_to_block(fallthrough);
                    } else {
                        emit.b.ins().store(MemFlags::new(), target, args[7], 0);
                        let exit = match op.opcode {
                            7 | 8 => 8,
                            10 => 9,
                            _ => 3,
                        };
                        let status = emit.b.ins().iconst(types::I32, exit);
                        emit.b.ins().return_(&[status]);
                        let unreachable = emit.b.create_block();
                        emit.b.switch_to_block(unreachable);
                    }
                    continue;
                }
                if emit.op(op).is_none() {
                    return Err("baseline eligibility mismatch".into());
                }
            }
            if internal {
                emit.b.ins().jump(blocks[ops.len()], &[]);
                emit.b.switch_to_block(blocks[ops.len()]);
            }
        }
        b.set_srcloc(SourceLoc::default());
        let success = b.ins().iconst(types::I32, 0);
        b.ins().return_(&[success]);
        b.seal_all_blocks();
        b.finalize();
    }
    let name = crate::baseline_jitdump::name(pc);
    let id = compiler
        .module
        .declare_function(&name, Linkage::Local, &signature)
        .map_err(|e| e.to_string())?;
    compiler
        .module
        .define_function(id, &mut ctx)
        .map_err(|e| e.to_string())?;
    let compiled = ctx.compiled_code().unwrap();
    let size = compiled.code_buffer().len();
    let mut locations = BTreeMap::new();
    if profiling {
        locations.insert(0, 0);
        for loc in compiled.buffer.get_srclocs_sorted() {
            locations.insert(
                loc.start,
                if loc.loc.is_default() {
                    0
                } else {
                    loc.loc.bits()
                },
            );
            locations.insert(loc.end, 0);
        }
        locations.remove(&(size as u32));
    }
    let locations: Vec<_> = locations.into_iter().collect();
    compiler
        .module
        .finalize_definitions()
        .map_err(|e| e.to_string())?;
    compiler.count += 1;
    let code = compiler.module.get_finalized_function(id);
    crate::baseline_jitdump::record(
        &name,
        pc,
        instructions,
        lengths,
        &locations,
        code,
        size,
        clock_ns,
        pid,
        tid,
    );
    Ok(Some(code))
}

#[no_mangle]
pub unsafe extern "C" fn rp_baseline_compile(
    pc: u32,
    data: *const u8,
    len: usize,
    out: *mut RpBaselineBlock,
    gpr_offset: u32,
    flag_offsets: *const u32,
    misc_offset: u32,
    eip_offset: u32,
    thunk_kind: Option<extern "C" fn(u32) -> i32>,
    rejection: *mut RpBaselineRejection,
    clock_ns: extern "C" fn() -> u64,
    pid: u32,
    tid: u32,
    guest_base: usize,
    guest_size: u32,
    null_limit: u32,
    direct_stores: u32,
    x87_select: Option<extern "C" fn(u64) -> usize>,
) -> i32 {
    if out.is_null()
        || data.is_null()
        || flag_offsets.is_null()
        || thunk_kind.is_none()
        || x87_select.is_none()
    {
        return -1;
    }
    if guest_base == 0 || guest_size < 8 || null_limit > guest_size {
        set_error("invalid baseline memory configuration".into());
        return -1;
    }
    (*out).code = std::ptr::null();
    (*out).count = 0;
    (*out).byte_count = 0;
    let bytes = slice::from_raw_parts(data, len.min(240));
    let mut instructions = Vec::new();
    let mut offset = 0usize;
    let mut first_mnemonic = String::new();
    while instructions.len() < 16 && offset < bytes.len() {
        let window = &bytes[offset..bytes.len().min(offset + 15)];
        let Some(addr) = pc.checked_add(offset as u32) else {
            break;
        };
        if offset != 0 && thunk_kind.unwrap()(addr) == 1 {
            break;
        }
        invalidate_lifter(addr, window);
        let (length, mnemonic, ops) = match lift_one(addr, window) {
            Ok(lifted) => lifted,
            Err(error) => {
                if offset == 0 && !rejection.is_null() {
                    (*rejection).reason = 5;
                    (*rejection).opcode = u32::MAX;
                    (*rejection).length = window.len() as u32;
                    copy_text(&mut (*rejection).detail, &error);
                }
                break;
            }
        };
        if offset == 0 {
            first_mnemonic = mnemonic.clone();
        }
        let failure = if length == 0 {
            Some((3, u32::MAX))
        } else {
            eligible(&ops).err()
        };
        if let Some((reason, opcode)) = failure {
            if offset == 0 && !rejection.is_null() {
                (*rejection).reason = reason;
                (*rejection).opcode = opcode;
                (*rejection).length = u32::from(length);
                copy_text(&mut (*rejection).mnemonic, &mnemonic);
            }
            break;
        }
        (*out).lengths[instructions.len()] = length;
        let terminal = ops
            .last()
            .is_some_and(|op| matches!(op.opcode, 4..=8 | 10) && !internal_branch(op));
        instructions.push(ops);
        offset += usize::from(length);
        if terminal {
            break;
        }
    }
    if instructions.is_empty() {
        return 0;
    }
    (*out).count = instructions.len() as u32;
    (*out).byte_count = offset as u32;
    (&mut (*out).bytes)[..offset].copy_from_slice(&bytes[..offset]);
    COMPILER.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            let verify = std::env::var("RECOMP_BASELINE_VERIFY").as_deref() == Ok("1");
            let builder = match JITBuilder::with_flags(
                &[
                    ("enable_verifier", if verify { "true" } else { "false" }),
                    ("opt_level", "speed"),
                ],
                default_libcall_names(),
            ) {
                Ok(builder) => builder,
                Err(e) => {
                    set_error(e.to_string());
                    return -1;
                }
            };
            *cell = Some(Compiler {
                module: JITModule::new(builder),
                count: 0,
            });
        }
        if cell.as_ref().unwrap().count >= 65536 {
            if !rejection.is_null() {
                (*rejection).reason = 6;
                (*rejection).opcode = u32::MAX;
                (*rejection).length = u32::from((*out).lengths[0]);
                copy_text(&mut (*rejection).mnemonic, &first_mnemonic);
            }
            return 2;
        }
        match compile(
            cell.as_mut().unwrap(),
            pc,
            &instructions,
            &(&(*out).lengths)[..instructions.len()],
            gpr_offset,
            slice::from_raw_parts(flag_offsets, 7).try_into().unwrap(),
            misc_offset,
            eip_offset,
            clock_ns,
            pid,
            tid,
            guest_base,
            guest_size,
            null_limit,
            direct_stores,
            x87_select.unwrap(),
        ) {
            Ok(Some(code)) => {
                (*out).code = code.cast();
                1
            }
            Ok(None) => 0,
            Err(error) => {
                set_error(error);
                -1
            }
        }
    })
}
