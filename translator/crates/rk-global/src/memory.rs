use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use rk_core::ir::Policy;
use rk_core::memory::{Address, Base, Effects, Range};
use rk_core::Addr;
use rk_ir::Op;
use rk_ssa::{BuildOptions, CallFacts};

use crate::callgraph::CallGraph;
use crate::GlobalInput;

#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub functions: BTreeMap<Addr, Effects>,
    pub sites: BTreeMap<Addr, Effects>,
}

struct Call {
    targets: Vec<Addr>,
    frame: Address,
}

struct Prepared {
    local: Effects,
    calls: Vec<Call>,
}

fn prepare(input: &GlobalInput<'_>, addr: Addr, facts: &dyn CallFacts) -> Option<Prepared> {
    let fir = input.analysis.ir.get(&addr)?.as_ref().ok()?;
    if !fir.entries.is_empty()
        || !fir.dynamic.is_empty()
        || !fir.seh.is_empty()
        || !fir.seh_handlers.is_empty()
        || !fir.seh_unwind_only.is_empty()
        || !fir.import_tails.is_empty()
    {
        return None;
    }
    let mut tail_indices = BTreeSet::new();
    for (&index, targets) in &fir.exits {
        let insn = fir.insns.get(index as usize)?;
        if !insn.mnemonic.eq_ignore_ascii_case("JMP")
            || targets.len() != 1
            || !fir.succ.get(index as usize)?.is_empty()
        {
            return None;
        }
        tail_indices.insert(index);
    }
    if !crate::cfg::closed(fir, &tail_indices) {
        return None;
    }
    let cg = rk_ir::lower(fir, &input.program.image, &rk_x87::Recipes).ok()?;
    let opts = BuildOptions {
        policy: Policy::Optimizing,
        call_facts: facts,
        ..BuildOptions::default()
    };
    let mut s = rk_ssa::build(&cg, &opts).ok()?;
    rk_ssa::canonicalize(&mut s, true);
    let accesses = rk_ssa::memory::accesses(&s, None);
    let mut addresses = rk_ssa::address::Addresses::new(&s);
    let mut local = Effects::default();
    let mut calls = Vec::new();
    let mut tails = BTreeSet::new();
    for block in &s.blocks {
        for &id in &block.ops {
            if s.value(id).op == Op::Return {
                local
                    .reads
                    .insert(Range::new(Address::exact(Base::Frame, 0), 4));
            }
            let target = match s.value(id).op {
                Op::Call { target } | Op::Tail { target } => Some(vec![target]),
                Op::CallInd => Some(
                    input
                        .indirect
                        .get(&s.value_addr[id as usize])
                        .cloned()
                        .unwrap_or_default(),
                ),
                Op::Branch { target }
                    if fir.exits.values().any(|targets| targets.contains(&target)) =>
                {
                    tails.insert(target);
                    Some(vec![target])
                }
                Op::ImportTail { .. } | Op::BranchInd | Op::Trap(_) | Op::Breakpoint => {
                    return None
                }
                _ => None,
            };
            if let Some(targets) = target {
                if let Op::Tail { target } = s.value(id).op {
                    tails.insert(target);
                }
                calls.push(Call {
                    targets,
                    frame: addresses.call_frame(id),
                });
                continue;
            }
            if let Some(access) = accesses.get(&id) {
                local.reads.opaque |= access.reads.opaque;
                local.writes.opaque |= access.writes.opaque;
                for &range in &access.reads.ranges {
                    local.reads.insert(range);
                }
                for &range in &access.writes.ranges {
                    local.writes.insert(range);
                }
            }
        }
    }
    if fir
        .exits
        .values()
        .flatten()
        .any(|target| !tails.contains(target))
    {
        return None;
    }
    Some(Prepared { local, calls })
}

pub fn analyze(
    input: &GlobalInput<'_>,
    excluded: &BTreeSet<Addr>,
    facts: &dyn CallFacts,
) -> Analysis {
    let prepared: BTreeMap<Addr, Prepared> = input
        .analysis
        .ir
        .par_iter()
        .filter(|(addr, _)| !excluded.contains(addr))
        .filter_map(|(&addr, _)| prepare(input, addr, facts).map(|body| (addr, body)))
        .collect();
    let graph = CallGraph::build(
        input.analysis,
        input.indirect,
        input.indirect_jumps,
        |addr| prepared.contains_key(&addr),
    );
    let functions = solve(&prepared, &graph);
    let sites = input
        .indirect
        .iter()
        .map(|(&site, targets)| {
            let mut effect = Effects::default();
            if targets.is_empty() {
                effect = Effects::unknown();
            }
            for target in targets {
                effect.join(
                    &functions
                        .get(target)
                        .cloned()
                        .unwrap_or_else(Effects::unknown),
                );
            }
            (site, effect)
        })
        .collect();
    Analysis { functions, sites }
}

fn solve(prepared: &BTreeMap<Addr, Prepared>, graph: &CallGraph) -> BTreeMap<Addr, Effects> {
    let mut functions = BTreeMap::new();
    for members in &graph.components {
        let mut current: BTreeMap<Addr, Effects> = members
            .iter()
            .map(|&node| {
                let addr = graph.nodes[node as usize];
                (addr, prepared[&addr].local.clone())
            })
            .collect();
        let mut converged = false;
        for _ in 0..128 {
            let next: BTreeMap<Addr, Effects> = current
                .keys()
                .map(|&addr| {
                    let body = &prepared[&addr];
                    let mut effects = body.local.clone();
                    for call in &body.calls {
                        if call.targets.is_empty() {
                            effects.join(&Effects::unknown());
                        }
                        for target in &call.targets {
                            let callee = current.get(target).or_else(|| functions.get(target));
                            effects.join(
                                &callee
                                    .cloned()
                                    .unwrap_or_else(Effects::unknown)
                                    .in_frame(call.frame),
                            );
                        }
                    }
                    (addr, effects)
                })
                .collect();
            converged = next == current;
            current = next;
            if converged {
                break;
            }
        }
        if !converged {
            for effects in current.values_mut() {
                *effects = Effects::unknown();
            }
        }
        functions.extend(current);
    }
    functions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(address: Address) -> Effects {
        let mut effects = Effects::default();
        effects.writes.insert(Range::new(address, 4));
        effects
    }

    #[test]
    fn summaries_translate_each_call_frame() {
        let graph = CallGraph::from_succ(vec![0x1000, 0x2000], vec![vec![1], vec![]]);
        let prepared = BTreeMap::from([
            (
                0x1000,
                Prepared {
                    local: Effects::default(),
                    calls: vec![
                        Call {
                            targets: vec![0x2000],
                            frame: Address::exact(Base::Frame, (-20i32) as u32),
                        },
                        Call {
                            targets: vec![0x2000],
                            frame: Address::exact(Base::Frame, (-40i32) as u32),
                        },
                    ],
                },
            ),
            (
                0x2000,
                Prepared {
                    local: write(Address::exact(Base::Frame, 8)),
                    calls: Vec::new(),
                },
            ),
        ]);
        let summaries = solve(&prepared, &graph);
        let writes = &summaries[&0x1000].writes;
        assert!(!writes.opaque);
        for offset in [-12i32, -32] {
            assert!(writes
                .ranges
                .contains(&Range::new(Address::exact(Base::Frame, offset as u32), 4)));
        }
        assert_eq!(writes.ranges.len(), 2);
    }

    #[test]
    fn recursive_static_effects_reach_a_fixed_point() {
        let graph = CallGraph::from_succ(vec![0x1000, 0x2000], vec![vec![1], vec![0]]);
        let prepared = BTreeMap::from([
            (
                0x1000,
                Prepared {
                    local: write(Address::exact(Base::Absolute, 0x500000)),
                    calls: vec![Call {
                        targets: vec![0x2000],
                        frame: Address::Unknown,
                    }],
                },
            ),
            (
                0x2000,
                Prepared {
                    local: write(Address::exact(Base::Absolute, 0x500004)),
                    calls: vec![Call {
                        targets: vec![0x1000],
                        frame: Address::Unknown,
                    }],
                },
            ),
        ]);
        let summaries = solve(&prepared, &graph);
        assert_eq!(summaries[&0x1000], summaries[&0x2000]);
        assert_eq!(summaries[&0x1000].writes.ranges.len(), 2);
        assert!(!summaries[&0x1000].writes.opaque);
    }

    #[test]
    fn unbounded_recursive_frames_and_missing_targets_stay_opaque() {
        let graph = CallGraph::from_succ(vec![0x1000], vec![vec![0]]);
        let prepared = BTreeMap::from([(
            0x1000,
            Prepared {
                local: write(Address::exact(Base::Frame, 8)),
                calls: vec![Call {
                    targets: vec![0x1000],
                    frame: Address::exact(Base::Frame, (-8i32) as u32),
                }],
            },
        )]);
        assert_eq!(solve(&prepared, &graph)[&0x1000], Effects::unknown());
        let prepared = BTreeMap::from([(
            0x1000,
            Prepared {
                local: Effects::default(),
                calls: vec![Call {
                    targets: vec![0x3000],
                    frame: Address::Unknown,
                }],
            },
        )]);
        assert_eq!(solve(&prepared, &graph)[&0x1000], Effects::unknown());
    }
}
