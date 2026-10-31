use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};
use rk_core::isa::{Reg, Shape, Step};
use rk_core::Addr;

#[derive(Clone, Copy, Debug)]
enum SimpleOp {
    None,
    Reg(Register),
    Mem(Mem),
    Imm(u32),
}

#[derive(Clone, Copy, Debug)]
struct Mem {
    base: Register,
    index: Register,
    scale: u32,
    displacement: Addr,
    size: u32,
}

struct Decoded {
    insn: Instruction,
    size: u8,
    ops: [SimpleOp; 4],
    op_count: u8,
    immediate: Option<(u8, Addr)>,
    displacement: Option<(u8, Addr)>,
    padding: bool,
}

pub(crate) fn shape(at: Addr, bytes: &[u8]) -> Option<Shape> {
    Some(decode(at, bytes)?.shape())
}

pub(crate) fn signature(at: Addr, bytes: &[u8]) -> Option<String> {
    let d = decode(at, bytes)?;
    Some(format!(
        "{:?}:{:?}:{}:{:?}:{:?}:{}",
        d.insn.mnemonic(),
        d.ops,
        d.op_count,
        d.immediate,
        d.displacement,
        d.padding
    ))
}

fn decode(at: Addr, bytes: &[u8]) -> Option<Decoded> {
    let first = decode_one(bytes, at, None)?;
    Some(
        match (first.insn.mnemonic() == Mnemonic::Wait)
            .then(|| decode_one(&bytes[1..], at + 1, Some(1)))
            .flatten()
            .filter(|next| {
                format!("{:?}", next.insn.mnemonic())
                    .to_lowercase()
                    .starts_with("fn")
            }) {
            Some(next) => next,
            None => first,
        },
    )
}

fn full(register: Register) -> Reg {
    register.full_register() as Reg
}

impl Decoded {
    fn shape(&self) -> Shape {
        let mnemonic = self.insn.mnemonic();
        let step = match (mnemonic, self.ops[0], self.ops[1]) {
            (Mnemonic::Movzx | Mnemonic::Mov, SimpleOp::Reg(dest), SimpleOp::Mem(m))
                if m.size == 1 && m.scale <= 1 =>
            {
                Step::IndexLoad {
                    dest: full(dest),
                    source: match (m.base, m.index) {
                        (base, Register::None) if base != Register::None => Some(full(base)),
                        (Register::None, index) if index != Register::None => Some(full(index)),
                        _ => None,
                    },
                    table: m.displacement,
                }
            }
            (Mnemonic::Cmp, SimpleOp::Reg(left), SimpleOp::Imm(right)) => Step::Compare {
                reg: full(left),
                bound: right,
            },
            (Mnemonic::Jae | Mnemonic::Jb, _, _) => Step::Exclusive,
            (Mnemonic::Ja | Mnemonic::Jbe, _, _) => Step::Inclusive,
            _ => Step::Other,
        };
        let written = match self.ops[0] {
            SimpleOp::Reg(r)
                if !matches!(mnemonic, Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Push) =>
            {
                Some(full(r))
            }
            _ => None,
        };
        let slot = match self.ops[0] {
            SimpleOp::Mem(m) if m.base == Register::None && m.index == Register::None => {
                Some(m.displacement)
            }
            _ => None,
        };
        let table_jump = match self.ops[0] {
            SimpleOp::Mem(m)
                if self.op_count == 1
                    && m.base == Register::None
                    && m.index != Register::None
                    && m.scale == 4
                    && m.size == 4 =>
            {
                Some((full(m.index), m.displacement))
            }
            _ => None,
        };
        Shape {
            length: self.size,
            step,
            written,
            pointer: matches!(mnemonic, Mnemonic::Push | Mnemonic::Mov)
                .then_some(self.immediate)
                .flatten()
                .map(|(_, value)| value),
            slot,
            table_jump,
            immediate: self.immediate,
            displacement: self.displacement,
            counter: matches!(
                mnemonic,
                Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne
            ),
            padding: self.padding,
        }
    }
}

fn decode_one(bytes: &[u8], at: Addr, folded: Option<u8>) -> Option<Decoded> {
    let mut decoder = Decoder::with_ip(32, bytes, at, DecoderOptions::NONE);
    let mut insn = Instruction::default();
    decoder.decode_out(&mut insn);
    if insn.is_invalid() || insn.len() == 0 || insn.len() > bytes.len() {
        return None;
    }
    let base = folded.unwrap_or(0);
    let prefix = if folded.is_some() { 1u8 } else { 0u8 };
    let size = insn.len() as u8 + prefix;
    let mut ops = [SimpleOp::None; 4];
    let op_count = insn.op_count().min(4) as u8;
    for (index, op) in ops.iter_mut().enumerate().take(op_count as usize) {
        *op = match insn.op_kind(index as u32) {
            OpKind::Register => SimpleOp::Reg(insn.op_register(index as u32)),
            OpKind::Memory => SimpleOp::Mem(Mem {
                base: insn.memory_base(),
                index: insn.memory_index(),
                scale: insn.memory_index_scale(),
                displacement: Addr::from(insn.memory_displacement64() as u32),
                size: insn.memory_size().size() as u32,
            }),
            OpKind::Immediate8
            | OpKind::Immediate8_2nd
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => SimpleOp::Imm(insn.immediate(index as u32) as u32),
            _ => SimpleOp::None,
        };
    }
    let constant = decoder.get_constant_offsets(&insn);
    let immediate = (constant.immediate_size() == 4).then(|| {
        (
            constant.immediate_offset().wrapping_add(base as usize) as u8,
            Addr::from(insn.immediate(0) as u32),
        )
    });
    let displacement = (constant.displacement_size() == 4).then(|| {
        (
            constant.displacement_offset().wrapping_add(base as usize) as u8,
            Addr::from(insn.memory_displacement64() as u32),
        )
    });
    let padding = bytes[..insn.len()].iter().all(|&b| b == 0x90 || b == 0xcc)
        || is_alignment_padding(&insn, &ops);
    Some(Decoded {
        insn,
        size,
        ops,
        op_count,
        immediate,
        displacement,
        padding,
    })
}

fn is_alignment_padding(insn: &Instruction, ops: &[SimpleOp; 4]) -> bool {
    if matches!(insn.mnemonic(), Mnemonic::Nop) {
        return true;
    }
    match (ops[0], ops[1]) {
        (SimpleOp::Reg(a), SimpleOp::Reg(b)) => {
            a == b && matches!(insn.mnemonic(), Mnemonic::Mov | Mnemonic::Xchg)
        }
        (SimpleOp::Reg(a), SimpleOp::Mem(m)) => {
            matches!(insn.mnemonic(), Mnemonic::Lea)
                && a == m.base
                && m.index == Register::None
                && m.displacement == 0
        }
        _ => false,
    }
}
