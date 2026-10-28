use std::collections::{BTreeMap, VecDeque};

use rk_core::Lookup;
use rk_ir::Op;

use crate::{Ssa, ValueId};

pub(crate) trait Aliases {
    fn target(&self, value: ValueId) -> Option<ValueId>;
    fn is_empty(&self) -> bool;
}

impl Aliases for Lookup<ValueId, ValueId> {
    fn target(&self, value: ValueId) -> Option<ValueId> {
        self.get(&value).copied()
    }

    fn is_empty(&self) -> bool {
        Lookup::is_empty(self)
    }
}

#[derive(Default)]
struct DenseAliases {
    targets: Vec<ValueId>,
    len: usize,
}

impl DenseAliases {
    fn insert(&mut self, value: ValueId, target: ValueId) {
        if self.targets.len() <= value as usize {
            self.targets.resize(value as usize + 1, ValueId::MAX);
        }
        if self.targets[value as usize] == ValueId::MAX {
            self.len += 1;
        }
        self.targets[value as usize] = target;
    }
}

impl Aliases for DenseAliases {
    fn target(&self, value: ValueId) -> Option<ValueId> {
        self.targets
            .get(value as usize)
            .copied()
            .filter(|&target| target != ValueId::MAX)
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub(crate) fn rewrite_aliases(s: &mut Ssa, alias: &impl Aliases) {
    if alias.is_empty() {
        return;
    }
    for value in &mut s.values {
        let start = value.args_start as usize;
        let end = start + value.args_len as usize;
        for arg in &mut s.args[start..end] {
            *arg = resolve_alias(alias, *arg);
        }
    }
    for (_, value) in &mut s.state {
        *value = resolve_alias(alias, *value);
    }
    for (_, value) in &mut s.dispatch.loads {
        *value = resolve_alias(alias, *value);
    }
    for reads in &mut s.entry_reads {
        for (_, value) in reads {
            *value = resolve_alias(alias, *value);
        }
    }
    for state in &mut s.exit_state {
        for value in state.iter_mut().flatten() {
            *value = resolve_alias(alias, *value);
        }
    }
    for group in &mut s.lane_groups {
        group.phi = resolve_alias(alias, group.phi);
        for lane in &mut group.lanes {
            lane.value = resolve_alias(alias, lane.value);
            for arg in &mut lane.args {
                *arg = resolve_alias(alias, *arg);
            }
        }
    }
    for block in &mut s.blocks {
        block.phis.retain(|value| alias.target(*value).is_none());
        block.ops.retain(|value| alias.target(*value).is_none());
        for phi in &mut block.phis {
            *phi = resolve_alias(alias, *phi);
        }
        for value in &mut block.ops {
            *value = resolve_alias(alias, *value);
        }
    }
    for record in &mut s.flags {
        for value in record
            .op
            .iter_mut()
            .chain(record.kind_value.iter_mut())
            .chain(record.a.iter_mut())
            .chain(record.b.iter_mut())
            .chain(record.carry.iter_mut())
            .chain(record.result.iter_mut())
            .chain(record.flags.values_mut())
        {
            *value = resolve_alias(alias, *value);
        }
    }
}

pub(crate) fn resolve_alias(alias: &impl Aliases, mut value: ValueId) -> ValueId {
    while let Some(next) = alias.target(value) {
        if next == value {
            break;
        }
        value = next;
    }
    value
}

fn same_packed(s: &Ssa, alias: &DenseAliases, left: ValueId, right: ValueId) -> bool {
    let left = resolve_alias(alias, left);
    let right = resolve_alias(alias, right);
    if left == right {
        return true;
    }
    let a = &s.values[left as usize];
    let b = &s.values[right as usize];
    if a.op != Op::Pack || b.op != Op::Pack || a.size != b.size || a.args_len != b.args_len {
        return false;
    }
    let a_args = &s.args[a.args_start as usize..(a.args_start + a.args_len) as usize];
    let b_args = &s.args[b.args_start as usize..(b.args_start + b.args_len) as usize];
    a_args
        .iter()
        .zip(b_args)
        .all(|(&x, &y)| resolve_alias(alias, x) == resolve_alias(alias, y))
}

fn signed_sub_parts(
    s: &Ssa,
    alias: &DenseAliases,
    id: ValueId,
) -> Option<(ValueId, ValueId, ValueId)> {
    use rk_core::ir::OpCode as C;
    let value = &s.values[id as usize];
    if value.size != 1
        || !matches!(
            value.op,
            Op::P(C::IntXor | C::IntNotEqual | C::IntEqual | C::BoolXor)
        )
    {
        return None;
    }
    let args = &s.args[value.args_start as usize..(value.args_start + value.args_len) as usize];
    if args.len() != 2 {
        return None;
    }
    let left = &s.values[resolve_alias(alias, args[0]) as usize];
    let right = &s.values[resolve_alias(alias, args[1]) as usize];
    if left.size != 1 || right.size != 1 {
        return None;
    }
    for (sign, overflow) in [(left, right), (right, left)] {
        if sign.op != Op::P(C::IntSLess)
            || sign.args_len != 2
            || overflow.op != Op::P(C::IntSBorrow)
            || overflow.args_len != 2
        {
            continue;
        }
        let sign_args =
            &s.args[sign.args_start as usize..(sign.args_start + sign.args_len) as usize];
        let overflow_args = &s.args
            [overflow.args_start as usize..(overflow.args_start + overflow.args_len) as usize];
        let result = &s.values[resolve_alias(alias, sign_args[0]) as usize];
        let zero = &s.values[resolve_alias(alias, sign_args[1]) as usize];
        let a = resolve_alias(alias, overflow_args[0]);
        let b = resolve_alias(alias, overflow_args[1]);
        if result.op != Op::P(C::IntSub)
            || result.size == 0
            || result.size != s.values[a as usize].size
            || s.values[a as usize].size != s.values[b as usize].size
            || zero.op != Op::Const(0)
            || zero.size != result.size
        {
            continue;
        }
        let sub_args =
            &s.args[result.args_start as usize..(result.args_start + result.args_len) as usize];
        if sub_args.len() == 2
            && same_packed(s, alias, sub_args[0], a)
            && same_packed(s, alias, sub_args[1], b)
        {
            return Some((a, b, resolve_alias(alias, sign_args[0])));
        }
    }
    None
}

fn signed_sub_le(s: &Ssa, alias: &DenseAliases, id: ValueId) -> Option<(ValueId, ValueId)> {
    use rk_core::ir::OpCode as C;
    let value = &s.values[id as usize];
    if value.size != 1 || !matches!(value.op, Op::P(C::IntOr | C::BoolOr)) || value.args_len != 2 {
        return None;
    }
    let args = &s.args[value.args_start as usize..(value.args_start + value.args_len) as usize];
    for (relation, equal) in [(args[0], args[1]), (args[1], args[0])] {
        let relation = &s.values[resolve_alias(alias, relation) as usize];
        if relation.op != Op::P(C::IntSLess) || relation.size != 1 || relation.args_len != 2 {
            continue;
        }
        let relation_args = &s.args
            [relation.args_start as usize..(relation.args_start + relation.args_len) as usize];
        let a = resolve_alias(alias, relation_args[0]);
        let b = resolve_alias(alias, relation_args[1]);
        if s.values[a as usize].size == 0 || s.values[a as usize].size != s.values[b as usize].size
        {
            continue;
        }
        let equal = &s.values[resolve_alias(alias, equal) as usize];
        if equal.op != Op::P(C::IntEqual) || equal.size != 1 || equal.args_len != 2 {
            continue;
        }
        let equal_args =
            &s.args[equal.args_start as usize..(equal.args_start + equal.args_len) as usize];
        let left = &s.values[resolve_alias(alias, equal_args[0]) as usize];
        let right = &s.values[resolve_alias(alias, equal_args[1]) as usize];
        let (zero, result) = if left.op == Op::Const(0) {
            (left, resolve_alias(alias, equal_args[1]))
        } else if right.op == Op::Const(0) {
            (right, resolve_alias(alias, equal_args[0]))
        } else {
            continue;
        };
        let result_value = &s.values[result as usize];
        if zero.size != result_value.size
            || result_value.op != Op::P(C::IntSub)
            || result_value.size != s.values[a as usize].size
            || result_value.args_len != 2
        {
            continue;
        }
        let sub_args = &s.args[result_value.args_start as usize
            ..(result_value.args_start + result_value.args_len) as usize];
        if same_packed(s, alias, sub_args[0], a) && same_packed(s, alias, sub_args[1], b) {
            return Some((a, b));
        }
    }
    None
}

fn fold(s: &Ssa, code: rk_core::ir::OpCode, args: &[ValueId], result_size: u8) -> Option<u64> {
    use rk_core::ir::OpCode as C;
    if !(1..=8).contains(&result_size) || !(1..=2).contains(&args.len()) {
        return None;
    }
    let operand = |index: usize| {
        let value = s.value(args[index]);
        let Op::Const(data) = value.op else {
            return None;
        };
        (1..=8)
            .contains(&value.size)
            .then(|| (data & rk_core::mask(value.size as u32), value.size))
    };
    let (a, width_bytes) = operand(0)?;
    let (b, low_size) = if args.len() == 2 { operand(1)? } else { (0, 0) };
    let width = width_bytes as u32 * 8;
    let bits = rk_core::mask(width_bytes as u32);
    let signed = |value: u64| -> i64 {
        if width >= 64 {
            value as i64
        } else {
            ((value << (64 - width)) as i64) >> (64 - width)
        }
    };
    let (sa, sb) = (signed(a), signed(b));
    let result = match code {
        C::Copy => Some(a),
        C::IntZExt => Some(a),
        C::IntSExt => {
            let source = width;
            Some(if source >= 64 {
                a
            } else {
                (((a << (64 - source)) as i64) >> (64 - source)) as u64
            })
        }
        C::IntEqual => Some((a == b) as u64),
        C::IntNotEqual => Some((a != b) as u64),
        C::IntLess => Some((a < b) as u64),
        C::IntLessEqual => Some((a <= b) as u64),
        C::IntSLess => Some((sa < sb) as u64),
        C::IntSLessEqual => Some((sa <= sb) as u64),
        C::IntCarry => Some((a as u128 + b as u128 > bits as u128) as u64),
        C::IntSCarry => {
            let sum = a.wrapping_add(b) & bits;
            Some((((a ^ sum) & (b ^ sum)) >> (width.saturating_sub(1))) & 1)
        }
        C::IntSBorrow => {
            let difference = a.wrapping_sub(b) & bits;
            Some((((a ^ b) & (a ^ difference)) >> (width.saturating_sub(1))) & 1)
        }
        C::Int2Comp => Some(a.wrapping_neg()),
        C::IntNegate => Some(!a),
        C::IntAdd => Some(a.wrapping_add(b)),
        C::IntSub => Some(a.wrapping_sub(b)),
        C::IntMult => Some(a.wrapping_mul(b)),
        C::IntAnd | C::BoolAnd => Some(a & b),
        C::IntOr | C::BoolOr => Some(a | b),
        C::IntXor => Some(a ^ b),
        C::BoolXor => Some(((a != 0) ^ (b != 0)) as u64),
        C::BoolNegate => Some((a == 0) as u64),
        C::IntLeft => Some(if b >= width as u64 { 0 } else { a << b }),
        C::IntRight => Some(if b >= width as u64 { 0 } else { a >> b }),
        C::IntSRight => Some(if b >= width as u64 {
            if sa < 0 {
                bits
            } else {
                0
            }
        } else {
            ((sa >> b) as u64) & bits
        }),
        C::IntDiv => (b != 0).then(|| a / b),
        C::IntRem => (b != 0).then(|| a % b),
        C::IntSDiv => sa.checked_div(sb).map(|value| value as u64),
        C::IntSRem => sa.checked_rem(sb).map(|value| value as u64),
        C::PopCount => Some(a.count_ones() as u64),
        C::LzCount => Some(a.leading_zeros().saturating_sub(64 - width) as u64),
        C::SubPiece => Some(if b >= width_bytes as u64 {
            0
        } else {
            a >> (8 * b)
        }),
        C::Piece if low_size > 0 && width_bytes + low_size == result_size => {
            Some((a << (low_size as u32 * 8)) | b)
        }
        _ => None,
    };
    result.map(|data| data & rk_core::mask(result_size as u32))
}

enum Identity {
    Value(ValueId),
    Const(u64),
}

fn identity(s: &Ssa, code: rk_core::ir::OpCode, args: &[ValueId], size: u8) -> Option<Identity> {
    use rk_core::ir::OpCode as C;
    let [a, b] = args else { return None };
    let ca = match s.values[*a as usize].op {
        Op::Const(data) => Some(data),
        _ => None,
    };
    let cb = match s.values[*b as usize].op {
        Op::Const(data) => Some(data),
        _ => None,
    };
    if ca.is_some() && cb.is_some() {
        return None;
    }
    let mask = rk_core::mask(size as u32);
    let keep = |id: ValueId| (s.values[id as usize].size == size).then_some(Identity::Value(id));
    match code {
        C::IntAdd | C::IntSub if cb == Some(0) => keep(*a),
        C::IntMult if ca == Some(0) || cb == Some(0) => Some(Identity::Const(0)),
        C::IntMult if ca == Some(1) => keep(*b),
        C::IntMult if cb == Some(1) => keep(*a),
        C::IntAnd if ca == Some(0) || cb == Some(0) => Some(Identity::Const(0)),
        C::IntAnd if ca == Some(mask) => keep(*b),
        C::IntAnd if cb == Some(mask) => keep(*a),
        C::IntOr if ca == Some(0) => keep(*b),
        C::IntOr if cb == Some(0) => keep(*a),
        C::IntOr if ca == Some(mask) || cb == Some(mask) => Some(Identity::Const(mask)),
        C::IntXor if ca == Some(0) => keep(*b),
        C::IntXor if cb == Some(0) => keep(*a),
        C::IntLeft | C::IntRight if cb == Some(0) => keep(*a),
        _ => None,
    }
}

pub fn canonicalize(s: &mut Ssa, signed_sub: bool) {
    let mut cache = BTreeMap::new();
    for (index, value) in s.values.iter().enumerate() {
        if let Op::Const(data) = value.op {
            cache.entry((value.size, data)).or_insert(index as ValueId);
        }
    }
    let mut alias = DenseAliases::default();
    let count = s.values.len();
    let mut user_start = vec![0u32; count + 1];
    for value in &s.values {
        for &arg in &s.args[value.args_start as usize..(value.args_start + value.args_len) as usize]
        {
            if (arg as usize) < count {
                user_start[arg as usize + 1] += 1;
            }
        }
    }
    for index in 0..count {
        user_start[index + 1] += user_start[index];
    }
    let mut users = vec![0 as ValueId; user_start[count] as usize];
    let mut fill = user_start.clone();
    for (index, value) in s.values.iter().enumerate() {
        for &arg in &s.args[value.args_start as usize..(value.args_start + value.args_len) as usize]
        {
            if (arg as usize) < count {
                users[fill[arg as usize] as usize] = index as ValueId;
                fill[arg as usize] += 1;
            }
        }
    }
    let mut args = Vec::new();
    let mut queued = vec![false; s.values.len()];
    let mut work = VecDeque::new();
    for (index, item) in queued.iter_mut().enumerate() {
        *item = true;
        work.push_back(index as ValueId);
    }
    while let Some(id) = work.pop_front() {
        queued[id as usize] = false;
        if alias.target(id).is_some() {
            continue;
        }
        let value = s.values[id as usize];
        let start = value.args_start as usize;
        let end = start + value.args_len as usize;
        args.clear();
        for &arg in &s.args[start..end] {
            args.push(resolve_alias(&alias, arg));
        }
        let mut changed = false;
        for (slot, arg) in s.args[start..end].iter_mut().zip(args.iter().copied()) {
            if *slot != arg {
                *slot = arg;
                changed = true;
            }
        }
        let mut rewrote_predicate = false;
        if signed_sub && value.size == 1 {
            use rk_core::ir::OpCode as C;
            let rewritten = match value.op {
                Op::P(C::IntXor | C::IntNotEqual) => {
                    signed_sub_parts(s, &alias, id).map(|(a, b, _)| {
                        s.values[id as usize].op = Op::P(C::IntSLess);
                        (a, b)
                    })
                }
                Op::P(C::BoolXor) => signed_sub_parts(s, &alias, id).map(|(a, b, _)| {
                    s.values[id as usize].op = Op::P(C::IntSLess);
                    (a, b)
                }),
                Op::P(C::IntEqual) => signed_sub_parts(s, &alias, id).map(|(a, b, _)| {
                    s.values[id as usize].op = Op::P(C::IntSLessEqual);
                    (b, a)
                }),
                Op::P(C::IntOr | C::BoolOr) => signed_sub_le(s, &alias, id).map(|(a, b)| {
                    s.values[id as usize].op = Op::P(C::IntSLessEqual);
                    (a, b)
                }),
                _ => None,
            };
            if let Some((a, b)) = rewritten {
                let start = s.values[id as usize].args_start as usize;
                s.args[start] = a;
                s.args[start + 1] = b;
                changed = true;
                rewrote_predicate = true;
            }
        }
        let replacement = if rewrote_predicate {
            None
        } else {
            match value.op {
                Op::Phi => {
                    let mut distinct = Vec::new();
                    for &arg in &args {
                        if arg != id && !distinct.contains(&arg) {
                            distinct.push(arg);
                        }
                    }
                    (distinct.len() == 1).then(|| distinct[0])
                }
                Op::Byte(index) if args.len() == 1 => {
                    let source = &s.values[args[0] as usize];
                    if source.op == Op::Pack {
                        let source_args = &s.args[source.args_start as usize
                            ..(source.args_start + source.args_len) as usize];
                        source_args.get(index as usize).copied()
                    } else if let Op::Const(data) = source.op {
                        Some(s.constant(&mut cache, 1, data >> (8 * index)))
                    } else {
                        None
                    }
                }
                Op::Pack => {
                    let all_const = args
                        .iter()
                        .all(|&arg| matches!(s.values[arg as usize].op, Op::Const(_)));
                    if all_const {
                        let mut data = 0u64;
                        for (index, &arg) in args.iter().enumerate() {
                            if let Op::Const(byte) = s.values[arg as usize].op {
                                data |= (byte & 0xff) << (8 * index);
                            }
                        }
                        Some(s.constant(&mut cache, value.size, data))
                    } else if args.iter().enumerate().all(|(index, &arg)| {
                        let byte = &s.values[arg as usize];
                        byte.op == Op::Byte(index as u8)
                            && byte.args_len == 1
                            && resolve_alias(&alias, s.args[byte.args_start as usize])
                                == resolve_alias(&alias, s.args(args[0])[0])
                    }) {
                        let first = &s.values[args[0] as usize];
                        let source = resolve_alias(&alias, s.args[first.args_start as usize]);
                        (s.values[source as usize].size == value.size).then_some(source)
                    } else {
                        None
                    }
                }
                Op::P(code) => {
                    use rk_core::ir::OpCode as C;
                    match code {
                        C::Copy | C::IntZExt
                            if !args.is_empty()
                                && s.values[args[0] as usize].size == value.size =>
                        {
                            Some(args[0])
                        }
                        _ if args.len() == 2
                            && same_packed(s, &alias, args[0], args[1])
                            && matches!(
                                code,
                                C::IntEqual
                                    | C::IntLessEqual
                                    | C::IntSLessEqual
                                    | C::IntNotEqual
                                    | C::IntLess
                                    | C::IntSLess
                                    | C::IntXor
                                    | C::IntSub
                                    | C::IntSBorrow
                            ) =>
                        {
                            let result = match code {
                                C::IntEqual | C::IntLessEqual | C::IntSLessEqual => 1,
                                _ => 0,
                            };
                            Some(s.constant(&mut cache, value.size, result))
                        }
                        _ if args.len() == 2
                            && same_packed(s, &alias, args[0], args[1])
                            && s.values[args[0] as usize].size == value.size
                            && matches!(code, C::IntAnd | C::IntOr | C::BoolAnd | C::BoolOr) =>
                        {
                            Some(args[0])
                        }
                        _ if args.len() == 2 => match identity(s, code, &args, value.size) {
                            Some(Identity::Value(id)) => Some(id),
                            Some(Identity::Const(data)) => {
                                Some(s.constant(&mut cache, value.size, data))
                            }
                            None => fold(s, code, &args, value.size)
                                .map(|data| s.constant(&mut cache, value.size, data)),
                        },
                        _ => fold(s, code, &args, value.size)
                            .map(|data| s.constant(&mut cache, value.size, data)),
                    }
                }
                _ => None,
            }
        };
        if let Some(replacement) = replacement {
            let replacement = resolve_alias(&alias, replacement);
            if replacement != id {
                alias.insert(id, replacement);
                changed = true;
            }
        }
        if changed {
            let start = s.values[id as usize].args_start as usize;
            let end = start + s.values[id as usize].args_len as usize;
            let touched = s.args[start..end]
                .iter()
                .copied()
                .chain(std::iter::once(id));
            for arg in touched {
                if arg as usize >= count {
                    continue;
                }
                let arg_users = &users
                    [user_start[arg as usize] as usize..user_start[arg as usize + 1] as usize];
                for &user in arg_users {
                    if !queued[user as usize] {
                        queued[user as usize] = true;
                        work.push_back(user);
                    }
                }
            }
        }
    }
    rewrite_aliases(s, &alias);
}
