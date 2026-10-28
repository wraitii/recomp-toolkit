use crate::{CallFacts, Ssa, ValueId};
use rk_core::{contract::Field, Addr};
use rk_ir::Op;
use std::collections::{BTreeMap, BTreeSet};

pub struct Reload {
    pub field: Field,
    pub site: Addr,
    pub target: Option<Addr>,
    pub cleanup: Option<i32>,
}

pub struct Origins {
    pub value: Option<ValueId>,
    pub operation: Option<String>,
    pub inputs: BTreeSet<Field>,
    pub reloads: Vec<Reload>,
    pub pointer_loads: BTreeSet<Addr>,
    pub walk_limited: bool,
    pub unresolved_operations: BTreeSet<String>,
}

pub fn origins(s: &Ssa, roots: &[ValueId], facts: &dyn CallFacts) -> Origins {
    let mut pending = roots.to_vec();
    let mut seen = BTreeSet::new();
    let mut inputs = BTreeSet::new();
    let mut reloads = BTreeMap::new();
    let mut loads = BTreeSet::new();
    let mut unresolved = BTreeSet::new();
    let mut limited = false;
    while let Some(id) = pending.pop() {
        if seen.len() >= 256 {
            limited = true;
            break;
        }
        if !seen.insert(id) {
            continue;
        }
        match s.value(id).op {
            Op::Input(key) => {
                if let Some(field) = s.field(key) {
                    inputs.insert(field);
                } else {
                    unresolved.insert(format!("{:?}", s.value(id).op));
                }
            }
            Op::StateKeep(_) => pending.extend(s.args(id).get(1).copied()),
            Op::StateReload(key) => {
                if let Some(field) = s.field(key) {
                    let call = s.args(id)[0];
                    let site = s.value_addr[call as usize];
                    let target = match s.value(call).op {
                        Op::Call { target } => Some(target),
                        _ => None,
                    };
                    reloads.insert(
                        (field, site),
                        Reload {
                            field,
                            site,
                            target,
                            cleanup: facts.summary(site, target).cleanup,
                        },
                    );
                }
            }
            Op::P(rk_core::ir::OpCode::Load) => {
                loads.insert(s.value_addr[id as usize]);
            }
            Op::Const(_) | Op::Target(_) | Op::CcKind(_) => {}
            op if rk_ir::properties(op).pure && !s.args(id).is_empty() => {
                pending.extend(s.args(id))
            }
            op => {
                unresolved.insert(format!("{op:?}"));
            }
        }
    }
    Origins {
        value: roots.first().copied(),
        operation: roots.first().map(|&root| format!("{:?}", s.value(root).op)),
        inputs,
        reloads: reloads.into_values().collect(),
        pointer_loads: loads,
        walk_limited: limited,
        unresolved_operations: unresolved,
    }
}
