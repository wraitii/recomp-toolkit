use std::collections::{BTreeMap, BTreeSet};

use rk_core::contract::{DirectArguments, Field, Fields};
use rk_core::Addr;
use rk_ir::{field_lane, lanes, Op};
use rk_ssa::ValueId;

use crate::PlannedFunction;

pub fn select_arguments(
    functions: &BTreeMap<Addr, &PlannedFunction>,
    targets: &BTreeSet<Addr>,
    configured: &BTreeMap<Addr, DirectArguments>,
) -> BTreeMap<Addr, DirectArguments> {
    let mut sites: BTreeMap<Addr, Vec<(&PlannedFunction, ValueId)>> = BTreeMap::new();
    for &function in functions.values() {
        for (index, value) in function.ssa.values.iter().enumerate() {
            if let Op::Call { target } = value.op {
                if targets.contains(&target) {
                    sites
                        .entry(target)
                        .or_default()
                        .push((function, index as ValueId));
                }
            }
        }
    }
    let mut selected = configured.clone();
    for (&target, calls) in &sites {
        if configured.contains_key(&target) {
            continue;
        }
        let Some(&callee) = functions.get(&target) else {
            continue;
        };
        let mut scores = Vec::new();
        for field in Fields::GPRS.iter().filter(|field| *field != Field::Esp) {
            let reads = callee.ssa.values.iter().enumerate().any(|(index, value)| {
                callee.live.contains(&(index as ValueId))
                    && matches!(value.op, Op::Input(key) if field_lane(key).is_some_and(|(input, _)| input == field))
            });
            if !reads {
                continue;
            }
            let return_store = callee.ssa.boundaries.iter().any(|boundary| {
                if callee.ssa.value(boundary.at).op != Op::Return {
                    return false;
                }
                let published = callee
                    .publish
                    .get(&boundary.at)
                    .map_or(&[][..], Vec::as_slice);
                !lanes(field).iter().any(|key| published.contains(key))
                    && callee
                        .ssa
                        .boundary(boundary.at)
                        .unwrap_or_default()
                        .iter()
                        .any(|&(key, value)| {
                            field_lane(key).is_some_and(|(output, _)| output == field)
                                && input_value(callee, value, field)
                        })
            });
            let stores = calls
                .iter()
                .filter(|(caller, value)| {
                    caller
                        .publish
                        .get(value)
                        .is_some_and(|keys| lanes(field).iter().all(|key| keys.contains(key)))
                })
                .count();
            let score = 2 * stores as isize
                - (calls.len() - stores) as isize
                - if return_store {
                    calls.len() as isize
                } else {
                    0
                };
            if stores >= 2 && score > 0 {
                scores.push((score, field));
            }
        }
        scores.sort_by_key(|&(score, field)| (std::cmp::Reverse(score), field));
        let registers: Fields = scores.into_iter().take(2).map(|(_, field)| field).collect();
        if !registers.is_empty() {
            selected.insert(
                target,
                DirectArguments {
                    registers,
                    st0: false,
                },
            );
        }
    }
    selected
}

fn input_value(function: &PlannedFunction, value: ValueId, field: Field) -> bool {
    let value = match function.ssa.value(value).op {
        Op::Byte(_) => function.ssa.args(value)[0],
        _ => value,
    };
    matches!(function.ssa.value(value).op, Op::Input(key) if field_lane(key).is_some_and(|(input, _)| input == field))
}
