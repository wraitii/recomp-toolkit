use std::collections::BTreeMap;
use std::mem::offset_of;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, Block, Inst, InstBuilder, MemFlags, SigRef, Signature, SourceLoc,
    StackSlotData, StackSlotKind, Type, Value,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, Variable};
use rk_core::ir::{OpCode, Space, Varnode};

use super::abi::{Config, Frame, Status, X87Call};
use super::helper::Helper;
use super::ir::{register, Exit, Instruction, Op, Target};
use super::state::{
    Register, EFLAGS_RESERVED_ONE, EFLAGS_SPLIT_MASK, FPU_STATUS_CLEARED_BY_FNCLEX, FPU_TAGS_EMPTY,
    SPLIT_FLAG_BITS,
};

const OP_BUDGET: i64 = 65536;
const FLOAT_SIGN: i64 = 0x8000_0000;
const LOW_HALF: i64 = 0xffff;

fn mask(size: u32) -> u64 {
    if size >= 8 {
        u64::MAX
    } else {
        (1 << (size * 8)) - 1
    }
}

fn int_type(size: u32) -> Option<Type> {
    Some(match size {
        1 => types::I8,
        2 => types::I16,
        4 => types::I32,
        8 => types::I64,
        _ => return None,
    })
}

fn pieces(size: u32) -> impl Iterator<Item = (u32, Type)> {
    let mut done = 0;
    std::iter::from_fn(move || {
        let part = [8, 4, 2, 1].into_iter().find(|part| done + part <= size)?;
        let at = done;
        done += part;
        Some((at, int_type(part).unwrap()))
    })
}

fn flags() -> MemFlags {
    MemFlags::new().with_notrap()
}

pub fn signature(call_conv: CallConv, ptr: Type) -> Signature {
    let mut signature = Signature::new(call_conv);
    signature
        .params
        .extend([AbiParam::new(ptr), AbiParam::new(ptr)]);
    signature.returns.push(AbiParam::new(types::I32));
    signature
}

struct Signatures {
    write_memory: SigRef,
    string: SigRef,
    cpu: SigRef,
    x87: SigRef,
    x87_specialized: SigRef,
    float: SigRef,
}

impl Signatures {
    fn new(b: &mut FunctionBuilder<'_>, call_conv: CallConv, ptr: Type) -> Self {
        let mut import = |params: &[AbiParam], ret: Option<Type>| {
            let mut signature = Signature::new(call_conv);
            signature.params.extend_from_slice(params);
            signature.returns.extend(ret.map(AbiParam::new));
            b.import_signature(signature)
        };
        let (pointer, i32, i64) = (
            AbiParam::new(ptr),
            AbiParam::new(types::I32),
            AbiParam::new(types::I64),
        );
        let byte = AbiParam::new(types::I8).uext();
        Self {
            write_memory: import(&[i32, i32, i64], None),
            string: import(&[pointer, byte, byte, byte], Some(types::I32)),
            cpu: import(&[pointer], None),
            x87: import(&[pointer, pointer, i64, pointer], Some(types::I32)),
            x87_specialized: import(&[pointer, i64, i32], Some(types::I32)),
            float: import(&[pointer, i64, i64], Some(types::I64)),
        }
    }
}

enum Uniques {
    Static(BTreeMap<u64, Value>),
    Dynamic(BTreeMap<u64, (Variable, Variable)>),
}

struct Emitter<'a, 'f> {
    b: &'a mut FunctionBuilder<'f>,
    config: &'a Config,
    ptr: Type,
    frame: Value,
    cpu: Value,
    signatures: Signatures,
    exits: BTreeMap<Status, Block>,
    uniques: Uniques,
}

pub fn build(
    b: &mut FunctionBuilder<'_>,
    config: &Config,
    call_conv: CallConv,
    ptr: Type,
    pc: u32,
    instructions: &[Instruction],
    profiling: bool,
) {
    let entry = b.create_block();
    b.switch_to_block(entry);
    b.append_block_params_for_function_params(entry);
    let [frame, cpu] = b.block_params(entry).try_into().unwrap();
    let signatures = Signatures::new(b, call_conv, ptr);
    let mut emitter = Emitter {
        b,
        config,
        ptr,
        frame,
        cpu,
        signatures,
        exits: BTreeMap::new(),
        uniques: Uniques::Static(BTreeMap::new()),
    };
    let mut pc = pc;
    for (index, instruction) in instructions.iter().enumerate() {
        if profiling {
            emitter.b.set_srcloc(SourceLoc::new(index as u32 + 1));
        }
        emitter.instruction(index, pc, instruction);
        pc = pc.wrapping_add(u32::from(instruction.length));
    }
    emitter.b.set_srcloc(SourceLoc::default());
    emitter.leave(Status::Fallthrough);
    for (status, block) in std::mem::take(&mut emitter.exits) {
        emitter.b.switch_to_block(block);
        emitter.b.set_cold_block(block);
        emitter.leave(status);
    }
}

impl Emitter<'_, '_> {
    fn iconst(&mut self, value: i64) -> Value {
        self.b.ins().iconst(types::I64, value)
    }

    fn leave(&mut self, status: Status) {
        let status = self.b.ins().iconst(types::I32, i64::from(status.repr));
        self.b.ins().return_(&[status]);
    }

    fn guard(&mut self, failed: Value, status: Status) {
        let exit = *self
            .exits
            .entry(status)
            .or_insert_with(|| self.b.create_block());
        let next = self.b.create_block();
        self.b.ins().brif(failed, exit, &[], next, &[]);
        self.b.switch_to_block(next);
    }

    fn start_unreachable(&mut self) {
        let block = self.b.create_block();
        self.b.switch_to_block(block);
    }

    fn instruction(&mut self, index: usize, pc: u32, instruction: &Instruction) {
        let value = self.b.ins().iconst(types::I32, index as i64);
        self.b
            .ins()
            .store(flags(), value, self.frame, offset_of!(Frame, index) as i32);
        let value = self.b.ins().iconst(types::I32, i64::from(pc));
        self.b
            .ins()
            .store(flags(), value, self.cpu, self.config.cpu.eip as i32);
        if !instruction.internal_control {
            self.uniques = Uniques::Static(BTreeMap::new());
            for op in &instruction.ops {
                self.op(op, &[], 0);
            }
            return;
        }
        let mut variables = BTreeMap::new();
        for byte in instruction.uniques() {
            let value = self.b.declare_var(types::I64);
            let defined = self.b.declare_var(types::I8);
            let zero = self.iconst(0);
            self.b.def_var(value, zero);
            let no = self.b.ins().iconst(types::I8, 0);
            self.b.def_var(defined, no);
            variables.insert(byte, (value, defined));
        }
        self.uniques = Uniques::Dynamic(variables);
        let blocks: Vec<_> = (0..=instruction.ops.len())
            .map(|_| self.b.create_block())
            .collect();
        let budget = self.b.declare_var(types::I32);
        let initial = self.b.ins().iconst(types::I32, OP_BUDGET);
        self.b.def_var(budget, initial);
        for (op_index, op) in instruction.ops.iter().enumerate() {
            self.b.ins().jump(blocks[op_index], &[]);
            self.b.switch_to_block(blocks[op_index]);
            let remaining = self.b.use_var(budget);
            let exhausted = self.b.ins().icmp_imm(IntCC::Equal, remaining, 0);
            self.guard(exhausted, Status::BudgetExhausted);
            let remaining = self.b.ins().iadd_imm(remaining, -1);
            self.b.def_var(budget, remaining);
            self.op(op, &blocks, op_index);
        }
        let end = blocks[instruction.ops.len()];
        self.b.ins().jump(end, &[]);
        self.b.switch_to_block(end);
    }

    fn op(&mut self, op: &Op, blocks: &[Block], index: usize) {
        match *op {
            Op::Unary { code, out, a } => {
                let value = self.unary(code, a);
                self.write(out, value);
            }
            Op::Binary { code, out, a, b } => {
                let value = self.binary(code, a, b);
                self.write(out, value);
            }
            Op::Divide { code, out, a, b } => {
                let value = self.divide(code, a, b);
                self.write(out, value);
            }
            Op::DivMod {
                signed,
                size,
                out,
                a,
                b,
            } => {
                let value = self.divmod(signed, size, a, b);
                self.write(out, value);
            }
            Op::Float { helper, out, a, b } => {
                let a = self.read(a);
                let b = match b {
                    Some(b) => self.read(b),
                    None => self.iconst(0),
                };
                let callee = self.config.callbacks.float(helper);
                let value = self.call(self.signatures.float, callee, &[self.cpu, a, b]);
                self.write(out, value);
            }
            Op::Load { out, addr } => {
                let addr = self.read(addr);
                let value = self.guest_load(addr, out.size);
                self.write(out, value);
            }
            Op::Store { addr, value } => {
                let addr = self.read(addr);
                let data = self.read(value);
                self.guest_store(addr, value.size, data);
            }
            Op::Jump { target, condition } => {
                match condition {
                    Some(condition) => {
                        let condition = self.read(condition);
                        self.b
                            .ins()
                            .brif(condition, blocks[target], &[], blocks[index + 1], &[]);
                    }
                    None => {
                        self.b.ins().jump(blocks[target], &[]);
                    }
                }
                self.start_unreachable();
            }
            Op::Exit {
                exit,
                target,
                condition,
            } => self.exit(exit, target, condition),
            Op::Helper {
                helper,
                argument,
                out,
            } => self.helper(helper, argument, out),
        }
    }

    fn exit(&mut self, exit: Exit, target: Target, condition: Option<Varnode>) {
        let status = match exit {
            Exit::Branch => Status::Branch,
            Exit::Call => Status::Call,
            Exit::Return => Status::Return,
        };
        let next = condition.map(|condition| {
            let condition = self.read(condition);
            let taken = self.b.create_block();
            let next = self.b.create_block();
            self.b.ins().brif(condition, taken, &[], next, &[]);
            self.b.switch_to_block(taken);
            next
        });
        let target = match target {
            Target::Direct(address) => self.iconst(i64::from(address)),
            Target::Indirect(v) => self.read(v),
        };
        self.b.ins().store(
            flags(),
            target,
            self.frame,
            offset_of!(Frame, target) as i32,
        );
        self.leave(status);
        match next {
            Some(next) => self.b.switch_to_block(next),
            None => self.start_unreachable(),
        }
    }

    fn call_void(&mut self, signature: SigRef, callee: usize, args: &[Value]) -> Inst {
        let callee = self.b.ins().iconst(self.ptr, callee as i64);
        self.b.ins().call_indirect(signature, callee, args)
    }

    fn call(&mut self, signature: SigRef, callee: usize, args: &[Value]) -> Value {
        let call = self.call_void(signature, callee, args);
        self.b.inst_results(call)[0]
    }

    fn call_checked(&mut self, signature: SigRef, callee: usize, args: &[Value]) {
        let status = self.call(signature, callee, args);
        self.guard(status, Status::HelperFailed);
    }

    fn byte(&mut self, value: u8) -> Value {
        self.b.ins().iconst(types::I8, i64::from(value))
    }

    fn store_cpu16(&mut self, offset: u32, value: i64) {
        let value = self.b.ins().iconst(types::I16, value);
        self.b.ins().store(flags(), value, self.cpu, offset as i32);
    }

    fn helper(&mut self, helper: Helper, argument: Option<Varnode>, out: Option<Varnode>) {
        let cpu = self.config.cpu;
        let callbacks = &self.config.callbacks;
        match helper {
            Helper::X87Clear => {
                let status = self
                    .b
                    .ins()
                    .load(types::I16, flags(), self.cpu, cpu.fpu_sw as i32);
                let status = self
                    .b
                    .ins()
                    .band_imm(status, i64::from(!FPU_STATUS_CLEARED_BY_FNCLEX));
                self.b
                    .ins()
                    .store(flags(), status, self.cpu, cpu.fpu_sw as i32);
            }
            Helper::X87Empty { reset_top } => {
                self.store_cpu16(cpu.fpu_tag, i64::from(FPU_TAGS_EMPTY));
                if reset_top {
                    let top = self.b.ins().iconst(types::I32, 0);
                    self.store_cpu32(cpu.fpu_top, top);
                }
            }
            Helper::Rdtsc | Helper::Cpuid => {
                let callee = if matches!(helper, Helper::Rdtsc) {
                    callbacks.rdtsc
                } else {
                    callbacks.cpuid
                };
                self.call_void(self.signatures.cpu, callee, &[self.cpu]);
            }
            Helper::String(op) => {
                let args = [
                    self.frame,
                    self.byte(op.family.repr),
                    self.byte(op.width_log2),
                    self.byte(op.repeat.repr),
                ];
                self.call_checked(self.signatures.string, callbacks.string, &args);
            }
            Helper::X87(call) => self.x87(call, argument, out),
        }
    }

    fn x87(&mut self, call: X87Call, argument: Option<Varnode>, out: Option<Varnode>) {
        let argument = match argument {
            Some(argument) => {
                let addr = self.read(argument);
                self.guest_check(addr, call.memory_bytes());
                addr
            }
            None => self.iconst(0),
        };
        let callbacks = &self.config.callbacks;
        let specialized = call
            .specializable()
            .then(|| callbacks.x87_select(&call))
            .flatten();
        if let Some(function) = specialized {
            let operand = self
                .b
                .ins()
                .iconst(types::I32, i64::from(call.specialized_operand()));
            let args = [self.frame, argument, operand];
            self.call_checked(self.signatures.x87_specialized, function as usize, &args);
            return;
        }
        let slot = |b: &mut FunctionBuilder<'_>, size| {
            b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 3))
        };
        let call_slot = slot(self.b, size_of::<X87Call>() as u32);
        let fields = [
            (offset_of!(X87Call, kind), call.kind.repr),
            (offset_of!(X87Call, operand), call.operand),
            (offset_of!(X87Call, bits), call.bits),
            (offset_of!(X87Call, flags), call.flags),
            (offset_of!(X87Call, extra), call.extra),
        ];
        for (offset, value) in fields {
            let value = self.byte(value);
            self.b.ins().stack_store(value, call_slot, offset as i32);
        }
        let result_slot = slot(self.b, 8);
        let call_pointer = self.b.ins().stack_addr(self.ptr, call_slot, 0);
        let result = self.b.ins().stack_addr(self.ptr, result_slot, 0);
        let args = [self.frame, call_pointer, argument, result];
        let callee = self.config.callbacks.x87;
        self.call_checked(self.signatures.x87, callee, &args);
        if let Some(out) = out {
            let value = self.b.ins().stack_load(types::I64, result_slot, 0);
            self.write(out, value);
        }
    }

    fn truncate(&mut self, value: Value, size: u32) -> Value {
        if size >= 8 {
            value
        } else {
            self.b.ins().band_imm(value, mask(size) as i64)
        }
    }

    fn sign_extend(&mut self, value: Value, size: u32) -> Value {
        if size >= 8 {
            return value;
        }
        let shift = i64::from(64 - size * 8);
        let shifted = self.b.ins().ishl_imm(value, shift);
        self.b.ins().sshr_imm(shifted, shift)
    }

    fn flag(&mut self, condition: Value) -> Value {
        self.b.ins().uextend(types::I64, condition)
    }

    fn unary(&mut self, code: OpCode, a: Varnode) -> Value {
        let size = a.size;
        let a = self.read(a);
        match code {
            OpCode::Copy | OpCode::IntZExt => a,
            OpCode::IntSExt => self.sign_extend(a, size),
            OpCode::Int2Comp => self.b.ins().ineg(a),
            OpCode::IntNegate => self.b.ins().bnot(a),
            OpCode::BoolNegate => {
                let flipped = self.b.ins().bxor_imm(a, 1);
                self.b.ins().band_imm(flipped, 1)
            }
            OpCode::PopCount => self.b.ins().popcnt(a),
            OpCode::LzCount => {
                let zeros = self.b.ins().clz(a);
                self.b.ins().iadd_imm(zeros, -i64::from(64 - size * 8))
            }
            OpCode::FloatNeg => self.b.ins().bxor_imm(a, FLOAT_SIGN),
            OpCode::FloatAbs => self.b.ins().band_imm(a, !FLOAT_SIGN & mask(4) as i64),
            _ => unreachable!("decoded unary {code:?}"),
        }
    }

    fn binary(&mut self, code: OpCode, a: Varnode, b: Varnode) -> Value {
        let size = a.size;
        let (b_size, a, b) = (b.size, self.read(a), self.read(b));
        match code {
            OpCode::IntAdd => self.b.ins().iadd(a, b),
            OpCode::IntSub => self.b.ins().isub(a, b),
            OpCode::IntXor => self.b.ins().bxor(a, b),
            OpCode::IntAnd => self.b.ins().band(a, b),
            OpCode::IntOr => self.b.ins().bor(a, b),
            OpCode::IntMult => self.b.ins().imul(a, b),
            OpCode::BoolXor | OpCode::BoolAnd | OpCode::BoolOr => {
                let value = match code {
                    OpCode::BoolXor => self.b.ins().bxor(a, b),
                    OpCode::BoolAnd => self.b.ins().band(a, b),
                    _ => self.b.ins().bor(a, b),
                };
                self.b.ins().band_imm(value, 1)
            }
            OpCode::IntEqual | OpCode::IntNotEqual | OpCode::IntLess | OpCode::IntLessEqual => {
                let cc = match code {
                    OpCode::IntEqual => IntCC::Equal,
                    OpCode::IntNotEqual => IntCC::NotEqual,
                    OpCode::IntLess => IntCC::UnsignedLessThan,
                    _ => IntCC::UnsignedLessThanOrEqual,
                };
                let flag = self.b.ins().icmp(cc, a, b);
                self.flag(flag)
            }
            OpCode::IntSLess | OpCode::IntSLessEqual => {
                let cc = if code == OpCode::IntSLess {
                    IntCC::SignedLessThan
                } else {
                    IntCC::SignedLessThanOrEqual
                };
                let (a, b) = (self.sign_extend(a, size), self.sign_extend(b, size));
                let flag = self.b.ins().icmp(cc, a, b);
                self.flag(flag)
            }
            OpCode::IntCarry => {
                let sum = self.b.ins().iadd(a, b);
                let sum = self.truncate(sum, size);
                let flag = self.b.ins().icmp(IntCC::UnsignedLessThan, sum, a);
                self.flag(flag)
            }
            OpCode::IntSCarry | OpCode::IntSBorrow => {
                let (result, operands_differ) = if code == OpCode::IntSCarry {
                    let result = self.b.ins().iadd(a, b);
                    let same = self.b.ins().bxor(a, b);
                    (result, self.b.ins().bnot(same))
                } else {
                    (self.b.ins().isub(a, b), self.b.ins().bxor(a, b))
                };
                let result = self.truncate(result, size);
                let changed = self.b.ins().bxor(a, result);
                let overflow = self.b.ins().band(operands_differ, changed);
                let sign = self.b.ins().ushr_imm(overflow, i64::from(size * 8 - 1));
                self.b.ins().band_imm(sign, 1)
            }
            OpCode::IntLeft | OpCode::IntRight | OpCode::IntSRight => {
                let arithmetic = code == OpCode::IntSRight;
                let too_far = self.b.ins().icmp_imm(
                    IntCC::UnsignedGreaterThanOrEqual,
                    b,
                    i64::from(size * 8),
                );
                let input = if arithmetic {
                    self.sign_extend(a, size)
                } else {
                    a
                };
                let shifted = match code {
                    OpCode::IntLeft => self.b.ins().ishl(input, b),
                    OpCode::IntRight => self.b.ins().ushr(input, b),
                    _ => self.b.ins().sshr(input, b),
                };
                let fill = if arithmetic {
                    self.b.ins().sshr_imm(input, 63)
                } else {
                    self.iconst(0)
                };
                self.b.ins().select(too_far, fill, shifted)
            }
            OpCode::Piece => {
                let high = self.b.ins().ishl_imm(a, i64::from(b_size * 8));
                self.b.ins().bor(high, b)
            }
            OpCode::SubPiece => {
                let too_far = self
                    .b
                    .ins()
                    .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, b, 8);
                let bits = self.b.ins().ishl_imm(b, 3);
                let shifted = self.b.ins().ushr(a, bits);
                let zero = self.iconst(0);
                self.b.ins().select(too_far, zero, shifted)
            }
            _ => unreachable!("decoded binary {code:?}"),
        }
    }

    fn guard_signed_overflow(&mut self, n: Value, d: Value) {
        let min = self.b.ins().icmp_imm(IntCC::Equal, n, i64::MIN);
        let minus_one = self.b.ins().icmp_imm(IntCC::Equal, d, -1);
        let overflow = self.b.ins().band(min, minus_one);
        self.guard(overflow, Status::DivideOverflow);
    }

    fn guard_fits(&mut self, quotient: Value, size: u32) {
        let narrowed = self.sign_extend(quotient, size);
        let overflow = self.b.ins().icmp(IntCC::NotEqual, narrowed, quotient);
        self.guard(overflow, Status::DivideOverflow);
    }

    fn guard_nonzero(&mut self, divisor: Value) {
        let zero = self.b.ins().icmp_imm(IntCC::Equal, divisor, 0);
        self.guard(zero, Status::DivideByZero);
    }

    fn divide(&mut self, code: OpCode, a: Varnode, b: Varnode) -> Value {
        let size = a.size;
        let (a, b) = (self.read(a), self.read(b));
        self.guard_nonzero(b);
        match code {
            OpCode::IntDiv => {
                let quotient = self.b.ins().udiv(a, b);
                let high = self.b.ins().ushr_imm(quotient, i64::from(size * 4));
                let overflow = self.b.ins().icmp_imm(IntCC::NotEqual, high, 0);
                self.guard(overflow, Status::DivideOverflow);
                quotient
            }
            OpCode::IntRem => self.b.ins().urem(a, b),
            OpCode::IntSDiv => {
                let (n, d) = (self.sign_extend(a, size), self.sign_extend(b, size));
                if size == 8 {
                    self.guard_signed_overflow(n, d);
                }
                let quotient = self.b.ins().sdiv(n, d);
                self.guard_fits(quotient, size / 2);
                quotient
            }
            OpCode::IntSRem => {
                let (n, d) = (self.sign_extend(a, size), self.sign_extend(b, size));
                let minus_one = self.b.ins().icmp_imm(IntCC::Equal, d, -1);
                let one = self.iconst(1);
                let d = self.b.ins().select(minus_one, one, d);
                self.b.ins().srem(n, d)
            }
            _ => unreachable!("decoded division {code:?}"),
        }
    }

    fn divmod(&mut self, signed: bool, size: u32, a: Varnode, b: Varnode) -> Value {
        let (a, b) = (self.read(a), self.read(b));
        self.guard_nonzero(b);
        let (quotient, remainder) = if signed {
            let (n, d) = (self.sign_extend(a, size * 2), self.sign_extend(b, size * 2));
            if size * 2 == 8 {
                self.guard_signed_overflow(n, d);
            }
            let quotient = self.b.ins().sdiv(n, d);
            self.guard_fits(quotient, size);
            let remainder = self.b.ins().srem(n, d);
            (
                self.truncate(quotient, size),
                self.truncate(remainder, size),
            )
        } else {
            let quotient = self.b.ins().udiv(a, b);
            let overflow =
                self.b
                    .ins()
                    .icmp_imm(IntCC::UnsignedGreaterThan, quotient, mask(size) as i64);
            self.guard(overflow, Status::DivideOverflow);
            (quotient, self.b.ins().urem(a, b))
        };
        let remainder_shift = if size == 1 { 8 } else { 32 };
        let remainder = self.b.ins().ishl_imm(remainder, remainder_shift);
        self.b.ins().bor(quotient, remainder)
    }

    fn read(&mut self, v: Varnode) -> Value {
        match v.space {
            Space::Const => self.iconst((v.offset & mask(v.size)) as i64),
            Space::Ram => {
                let addr = self.iconst(v.offset as i64);
                self.guest_load(addr, v.size)
            }
            Space::Register => self.read_register(register(v), v.size),
            Space::Unique => self.read_unique(v),
        }
    }

    fn write(&mut self, v: Varnode, value: Value) {
        let value = self.truncate(value, v.size);
        match v.space {
            Space::Register => self.write_register(register(v), v.size, value),
            Space::Unique => self.write_unique(v, value),
            Space::Ram => {
                let addr = self.iconst(v.offset as i64);
                self.guest_store(addr, v.size, value);
            }
            Space::Const => unreachable!("decoded constant output"),
        }
    }

    fn read_unique(&mut self, v: Varnode) -> Value {
        let mut value = self.iconst(0);
        for i in 0..v.size {
            let offset = v.offset.wrapping_add(u64::from(i));
            let byte = match &self.uniques {
                Uniques::Static(values) => values[&offset],
                Uniques::Dynamic(variables) => {
                    let (byte, defined) = variables[&offset];
                    let defined = self.b.use_var(defined);
                    let missing = self.b.ins().icmp_imm(IntCC::Equal, defined, 0);
                    self.guard(missing, Status::UndefinedUnique);
                    self.b.use_var(byte)
                }
            };
            let part = if i == 0 {
                byte
            } else {
                self.b.ins().ishl_imm(byte, i64::from(i * 8))
            };
            value = self.b.ins().bor(value, part);
        }
        value
    }

    fn write_unique(&mut self, v: Varnode, value: Value) {
        for i in 0..v.size {
            let part = if i == 0 {
                value
            } else {
                self.b.ins().ushr_imm(value, i64::from(i * 8))
            };
            let byte = self.b.ins().band_imm(part, 0xff);
            let offset = v.offset.wrapping_add(u64::from(i));
            match &mut self.uniques {
                Uniques::Static(values) => {
                    values.insert(offset, byte);
                }
                Uniques::Dynamic(variables) => {
                    let (variable, defined) = variables[&offset];
                    self.b.def_var(variable, byte);
                    let yes = self.b.ins().iconst(types::I8, 1);
                    self.b.def_var(defined, yes);
                }
            }
        }
    }

    fn load_bytes(&mut self, base: Value, offset: u32, size: u32) -> Value {
        let mut value = None;
        for (at, ty) in pieces(size) {
            let part = self.b.ins().load(ty, flags(), base, (offset + at) as i32);
            let part = if ty == types::I64 {
                part
            } else {
                self.b.ins().uextend(types::I64, part)
            };
            let part = if at == 0 {
                part
            } else {
                self.b.ins().ishl_imm(part, i64::from(at * 8))
            };
            value = Some(match value {
                Some(value) => self.b.ins().bor(value, part),
                None => part,
            });
        }
        value.expect("non-empty access")
    }

    fn store_bytes(&mut self, base: Value, offset: u32, size: u32, value: Value) {
        for (at, ty) in pieces(size) {
            let part = if at == 0 {
                value
            } else {
                self.b.ins().ushr_imm(value, i64::from(at * 8))
            };
            let part = if ty == types::I64 {
                part
            } else {
                self.b.ins().ireduce(ty, part)
            };
            self.b
                .ins()
                .store(flags(), part, base, (offset + at) as i32);
        }
    }

    fn load_cpu32(&mut self, offset: u32) -> Value {
        self.b
            .ins()
            .load(types::I32, flags(), self.cpu, offset as i32)
    }

    fn store_cpu32(&mut self, offset: u32, value: Value) {
        self.b.ins().store(flags(), value, self.cpu, offset as i32);
    }

    fn eflags_misc_without_split(&mut self, clear: u32) -> Value {
        let misc = self.load_cpu32(self.config.cpu.eflags_misc);
        let misc = self
            .b
            .ins()
            .band_imm(misc, i64::from(!(EFLAGS_SPLIT_MASK | clear)));
        self.b.ins().bor_imm(misc, i64::from(EFLAGS_RESERVED_ONE))
    }

    fn read_register(&mut self, register: Register, size: u32) -> Value {
        let cpu = self.config.cpu;
        let value32 = match register {
            Register::Bytes { field, byte } => {
                return self.load_bytes(self.cpu, field.offset(&cpu) + byte, size)
            }
            Register::SplitFlag(index) => {
                let flag = self.load_cpu32(cpu.split_flags[index]);
                self.b.ins().band_imm(flag, 1)
            }
            Register::EflagsBit(bit) => {
                let misc = self.load_cpu32(cpu.eflags_misc);
                let misc = self.b.ins().bor_imm(misc, i64::from(EFLAGS_RESERVED_ONE));
                let shifted = self.b.ins().ushr_imm(misc, i64::from(bit));
                self.b.ins().band_imm(shifted, 1)
            }
            Register::Eflags => {
                let mut value = self.eflags_misc_without_split(0);
                for (offset, bit) in cpu.split_flags.into_iter().zip(SPLIT_FLAG_BITS) {
                    let flag = self.load_cpu32(offset);
                    let flag = self.b.ins().ishl_imm(flag, i64::from(bit));
                    value = self.b.ins().bor(value, flag);
                }
                if size == 2 {
                    self.b.ins().band_imm(value, LOW_HALF)
                } else {
                    value
                }
            }
        };
        self.b.ins().uextend(types::I64, value32)
    }

    fn write_register(&mut self, register: Register, size: u32, value: Value) {
        let cpu = self.config.cpu;
        if let Register::Bytes { field, byte } = register {
            self.store_bytes(self.cpu, field.offset(&cpu) + byte, size, value);
            return;
        }
        let value = self.b.ins().ireduce(types::I32, value);
        match register {
            Register::SplitFlag(index) => {
                let flag = self.b.ins().band_imm(value, 1);
                self.store_cpu32(cpu.split_flags[index], flag);
                let misc = self.eflags_misc_without_split(0);
                self.store_cpu32(cpu.eflags_misc, misc);
            }
            Register::EflagsBit(bit) => {
                let misc = self.eflags_misc_without_split(1 << bit);
                let flag = self.b.ins().band_imm(value, 1);
                let flag = self.b.ins().ishl_imm(flag, i64::from(bit));
                let misc = self.b.ins().bor(misc, flag);
                self.store_cpu32(cpu.eflags_misc, misc);
            }
            Register::Eflags => {
                let value = if size == 2 {
                    let misc = self.load_cpu32(cpu.eflags_misc);
                    let high = self.b.ins().band_imm(misc, !LOW_HALF);
                    self.b.ins().bor(high, value)
                } else {
                    value
                };
                let misc = self.b.ins().band_imm(value, i64::from(!EFLAGS_SPLIT_MASK));
                let misc = self.b.ins().bor_imm(misc, i64::from(EFLAGS_RESERVED_ONE));
                self.store_cpu32(cpu.eflags_misc, misc);
                for (offset, bit) in cpu.split_flags.into_iter().zip(SPLIT_FLAG_BITS) {
                    let flag = self.b.ins().ushr_imm(value, i64::from(bit));
                    let flag = self.b.ins().band_imm(flag, 1);
                    self.store_cpu32(offset, flag);
                }
            }
            Register::Bytes { .. } => unreachable!(),
        }
    }

    fn guest_check(&mut self, addr: Value, size: u32) {
        let wide = self
            .b
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThan, addr, i64::from(u32::MAX));
        self.guard(wide, Status::WideAddress);
        let null = self.b.ins().icmp_imm(
            IntCC::UnsignedLessThan,
            addr,
            i64::from(self.config.null_limit),
        );
        self.guard(null, Status::NullAccess);
        let outside = self.b.ins().icmp_imm(
            IntCC::UnsignedGreaterThan,
            addr,
            i64::from(self.config.guest_size - size),
        );
        self.guard(outside, Status::OutsideArena);
    }

    fn guest_pointer(&mut self, addr: Value, size: u32) -> Value {
        self.guest_check(addr, size);
        self.b.ins().iadd_imm(addr, self.config.guest_base as i64)
    }

    fn guest_load(&mut self, addr: Value, size: u32) -> Value {
        let pointer = self.guest_pointer(addr, size);
        self.load_bytes(pointer, 0, size)
    }

    fn guest_store(&mut self, addr: Value, size: u32, value: Value) {
        if !self.config.store_hooks {
            let pointer = self.guest_pointer(addr, size);
            self.store_bytes(pointer, 0, size, value);
            return;
        }
        self.guest_check(addr, size);
        let addr = self.b.ins().ireduce(types::I32, addr);
        let size = self.b.ins().iconst(types::I32, i64::from(size));
        let callee = self.config.callbacks.write_memory;
        self.call_void(self.signatures.write_memory, callee, &[addr, size, value]);
    }
}
