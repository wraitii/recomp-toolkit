use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rk_core::Reason;
use rk_ir::{properties, Op};
use rk_ssa::{BlockId, Ssa, ValueId, DISPATCH_BLOCK};

use crate::scalar::{merge_shapes, parts, CarryShape, Scalar, Shape, INACTIVE};
use crate::types::Expr;

pub fn reset_op(op: Op) -> bool {
    matches!(
        op,
        Op::Div { .. }
            | Op::Idiv { .. }
            | Op::String(_)
            | Op::Call { .. }
            | Op::CallInd
            | Op::Seh(_)
            | Op::Slow(_)
            | Op::CallOther(_)
    )
}

pub(crate) fn external_exit(s: &Ssa, value: ValueId) -> bool {
    matches!(s.value(value).op, Op::ImportTail { .. })
        || matches!(s.value(value).op, Op::Branch { .. } | Op::CBranch { .. })
            && s.boundary(value).is_some()
}

fn transfer(
    block: BlockId,
    entry: &Shape,
    s: &Ssa,
    factory: &dyn Fn() -> Scalar,
    fold_pending: bool,
) -> Shape {
    let mut scalar = factory();
    match entry {
        Shape::Known(shape) => scalar.seed(shape, &BTreeMap::new()),
        _ => scalar.seed(
            &CarryShape {
                active: false,
                slots: Vec::new(),
                base: 0,
                low: 0,
                high: 0,
                top_unknown: false,
                status_dirty: false,
            },
            &BTreeMap::new(),
        ),
    }
    let Some(block) = s.block(block) else {
        return Shape::Unknown;
    };
    for &value in &block.ops {
        match s.value(value).op {
            Op::X87(op) => {
                scalar.apply(op, Some(Expr::External(value)), None);
            }
            op if reset_op(op) => scalar.reset(),
            _ if external_exit(s, value) => {
                scalar.flush();
            }
            _ => {}
        }
    }
    if fold_pending {
        scalar.fold_pending(&mut Vec::new());
    }
    scalar.snapshot()
}

pub fn analyze(s: &Ssa, factory: &dyn Fn() -> Scalar) -> Result<BTreeMap<BlockId, Shape>, Reason> {
    let mut succ: BTreeMap<BlockId, Vec<BlockId>> = BTreeMap::new();
    for (index, block) in s.blocks.iter().enumerate() {
        succ.entry(index as BlockId).or_default();
        for &pred in &block.preds {
            if pred != DISPATCH_BLOCK {
                let row = succ.entry(pred).or_default();
                if !row.contains(&(index as BlockId)) {
                    row.push(index as BlockId);
                }
            }
        }
    }
    let roots: Vec<BlockId> = s.entries.iter().map(|entry| entry.target).collect();
    let Some(&primary) = roots.first() else {
        return Ok(BTreeMap::new());
    };
    let mut entry: BTreeMap<BlockId, Shape> = s
        .blocks
        .iter()
        .enumerate()
        .map(|(index, _)| (index as BlockId, Shape::Unknown))
        .collect();
    entry.insert(primary, Shape::Known(INACTIVE));
    let mut resets: BTreeSet<BlockId> = roots.iter().copied().skip(1).collect();
    if s.blocks
        .get(primary as usize)
        .is_some_and(|block| block.preds.iter().any(|pred| *pred != DISPATCH_BLOCK))
    {
        resets.insert(primary);
    }
    for block in resets {
        entry.insert(block, Shape::Unsafe);
    }
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec![(primary, false)];
    while let Some((block, done)) = stack.pop() {
        if done {
            order.push(block);
            continue;
        }
        if !seen.insert(block) {
            continue;
        }
        stack.push((block, true));
        if let Some(targets) = succ.get(&block) {
            for &target in targets.iter().rev() {
                if !seen.contains(&target) {
                    stack.push((target, false));
                }
            }
        }
    }
    for index in 0..s.blocks.len() as BlockId {
        if !seen.contains(&index) {
            order.push(index);
        }
    }
    order.reverse();
    let mut exit: BTreeMap<BlockId, Shape> = BTreeMap::new();
    let mut work: VecDeque<BlockId> = order.into_iter().collect();
    let mut queued: BTreeSet<BlockId> = work.iter().copied().collect();
    let limit = 10 * s.blocks.len() * (s.blocks.len() + 10) + 1000;
    let mut iterations = 0usize;
    while let Some(block) = work.pop_front() {
        iterations += 1;
        if iterations > limit {
            return Err(Reason::UnsupportedX87 {
                detail: "carry analysis did not converge".into(),
            });
        }
        queued.remove(&block);
        let merged = if matches!(entry.get(&block), Some(Shape::Unsafe)) {
            Shape::Unsafe
        } else {
            let Some(block_ref) = s.block(block) else {
                continue;
            };
            let mut shapes = Vec::new();
            for &pred in &block_ref.preds {
                if pred == DISPATCH_BLOCK {
                    shapes.push(Shape::Known(INACTIVE));
                } else if let Some(shape) = exit.get(&pred) {
                    shapes.push(shape.clone());
                }
            }
            if shapes.is_empty() {
                continue;
            }
            if shapes.len() > 1 && shapes.iter().any(|shape| {
                matches!(shape, Shape::Known(shape) if shape.slots.iter().any(|(_, slot)| slot.pending))
            }) {
                return Err(Reason::UnsupportedX87 {
                    detail: format!("deferred x87 NaN reached a carried join B{block}"),
                });
            }
            merge_shapes(&shapes)
        };
        if entry.get(&block) == Some(&merged) && exit.contains_key(&block) {
            continue;
        }
        entry.insert(block, merged.clone());
        let linear = succ.get(&block).is_some_and(|targets| {
            targets.len() == 1
                && targets[0] == block + 1
                && !roots.contains(&targets[0])
                && s.block(targets[0]).is_some_and(|next| {
                    next.preds == vec![block] && next.start == s.blocks[block as usize].end
                })
                && !s.blocks[block as usize]
                    .ops
                    .iter()
                    .any(|value| properties(s.value(*value).op).terminator)
        });
        let result = transfer(block, &merged, s, factory, !linear);
        if exit.get(&block) != Some(&result) {
            exit.insert(block, result);
            if let Some(targets) = succ.get(&block) {
                for &target in targets {
                    if queued.insert(target) {
                        work.push_back(target);
                    }
                }
            }
        }
    }
    Ok(entry)
}

pub fn assert_covers(join: &Shape, live: &Shape, where_: &str) -> Result<(), Reason> {
    match (join, live) {
        (Shape::Unsafe, _) => Ok(()),
        (_, Shape::Unknown) => Ok(()),
        (Shape::Unknown, _) => Ok(()),
        (Shape::Known(_), Shape::Unsafe) => Err(Reason::UnsupportedX87 {
            detail: format!("{where_}: live x87 state is unknown"),
        }),
        (Shape::Known(join), Shape::Known(live)) => {
            if !live.active {
                if join.active {
                    return Err(Reason::UnsupportedX87 {
                        detail: format!("{where_}: carried x87 edge expected active state"),
                    });
                }
                return Ok(());
            }
            if !join.active {
                return Err(Reason::UnsupportedX87 {
                    detail: format!("{where_}: live x87 state at an inactive join"),
                });
            }
            let planned: BTreeMap<i32, &crate::scalar::SlotShape> = join
                .slots
                .iter()
                .map(|(position, slot)| (*position, slot))
                .collect();
            for (position, slot) in &live.slots {
                let Some(planned) = planned.get(position) else {
                    if slot.dirty == 0 {
                        continue;
                    }
                    return Err(Reason::UnsupportedX87 {
                        detail: format!(
                            "{where_}: live x87 position {position} absent from join shape"
                        ),
                    });
                };
                if slot.dirty & !planned.dirty != 0 {
                    return Err(Reason::UnsupportedX87 {
                        detail: format!("{where_}: live x87 parts beyond join shape at {position}"),
                    });
                }
                if planned.narrow && !slot.narrow {
                    return Err(Reason::UnsupportedX87 {
                        detail: format!(
                            "{where_}: binary32 join at {position} from unproven operands"
                        ),
                    });
                }
                if slot.pending != planned.pending {
                    return Err(Reason::UnsupportedX87 {
                        detail: format!(
                            "{where_}: deferred x87 NaN differs from join at {position}"
                        ),
                    });
                }
                if planned.quiet && !slot.quiet {
                    return Err(Reason::UnsupportedX87 {
                        detail: format!(
                            "{where_}: quiet NaN join at {position} from unproven operands"
                        ),
                    });
                }
                for part in parts(slot.parts & planned.parts) {
                    let agreed = planned
                        .consts
                        .iter()
                        .find(|(name, _)| *name == part)
                        .map(|(_, value)| value);
                    let live = slot
                        .consts
                        .iter()
                        .find(|(name, _)| *name == part)
                        .map(|(_, value)| value);
                    if let Some(agreed) = agreed {
                        if live != Some(agreed) {
                            return Err(Reason::UnsupportedX87 {
                                detail: format!(
                                    "{where_}: x87 {part} at {position} is not the joined constant"
                                ),
                            });
                        }
                    }
                }
            }
            if live.low < join.low || live.high > join.high || live.base < join.base {
                return Err(Reason::UnsupportedX87 {
                    detail: format!(
                        "{where_}: live x87 window [{},{}]/{} exceeds join [{},{}]/{}",
                        live.low, live.high, live.base, join.low, join.high, join.base
                    ),
                });
            }
            if (live.top_unknown || live.base != join.base) && !join.top_unknown {
                return Err(Reason::UnsupportedX87 {
                    detail: format!("{where_}: live x87 published TOP differs from join"),
                });
            }
            if live.status_dirty && !join.status_dirty {
                return Err(Reason::UnsupportedX87 {
                    detail: format!("{where_}: live x87 status publication not planned"),
                });
            }
            Ok(())
        }
    }
}
