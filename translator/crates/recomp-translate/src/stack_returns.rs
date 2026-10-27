use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use rayon::prelude::*;
use rk_core::contract::Field;
use rk_core::ir::Policy;
use rk_core::memory::{Address, Base, Range};
use rk_core::Addr;
use rk_ir::{Op, StateKey};
use rk_ssa::address::Addresses;
use rk_ssa::{BuildOptions, Ssa};
use serde_json::{json, Value};

use super::{BodyContext, Prepared};

const STEP_LIMIT: usize = 4096;
const CALL_LIMIT: usize = 1;
const BYTE_LIMIT: u32 = 256;

struct Store {
    site: Addr,
    key: StateKey,
    range: Range,
    returned_pointer: bool,
}

enum Effect {
    Read(Range),
    Write(Range),
    Call {
        target: Option<Addr>,
        frame: Address,
    },
    Return,
    Barrier(String),
}

struct Event {
    site: Addr,
    effect: Effect,
    esp: Address,
}

struct Block {
    events: Vec<Event>,
    next: Vec<usize>,
}

struct Body {
    entry: usize,
    blocks: Vec<Block>,
    stores: Vec<Store>,
    instructions: BTreeMap<Addr, String>,
    return_slot_proven: bool,
}

#[derive(Clone, Copy)]
struct Cursor {
    function: Addr,
    block: usize,
    index: usize,
    frame: Address,
    original: bool,
}

#[derive(Clone)]
struct Search {
    cursor: Cursor,
    resumes: Vec<Cursor>,
    remaining: BTreeSet<u32>,
    trace: Vec<Value>,
    visited: BTreeSet<(Addr, usize, usize, Address, Vec<u32>, usize)>,
    calls: usize,
}

fn describe(range: Range) -> Value {
    match range.address {
        Address::Known { base, offset, span } => json!({
            "base": format!("{base:?}"), "offset": offset as i32, "span": span, "size": range.size,
        }),
        Address::Unknown => json!({"base": "unknown", "size": range.size}),
    }
}

fn mapped(address: Address, cursor: Cursor) -> Address {
    match address {
        Address::Known {
            base: Base::Frame,
            offset,
            span,
        } => cursor.frame.offset(offset).bounded(0, span),
        Address::Known {
            base: Base::Absolute,
            ..
        } => address,
        _ if cursor.original => address,
        _ => Address::Unknown,
    }
}

fn stores(planned: &rk_plan::PlannedFunction) -> Vec<Store> {
    let s = &planned.ssa;
    let frame = s
        .frame
        .map_or(Address::Unknown, |_| Address::exact(Base::Frame, 0));
    let mut result = Vec::new();
    for boundary in &s.boundaries {
        if s.value(boundary.at).op != Op::Return {
            continue;
        }
        for &key in planned.publish.get(&boundary.at).into_iter().flatten() {
            let StateKey::Frame { offset, aligned } = key else {
                continue;
            };
            let Some(&(_, value)) = s
                .boundary(boundary.at)
                .unwrap_or_default()
                .iter()
                .find(|&&(k, _)| k == key)
            else {
                continue;
            };
            let base = if aligned {
                s.aligned.map_or(Address::Unknown, |(start, mask)| {
                    frame.offset(start).bounded(!mask, 0)
                })
            } else {
                frame
            };
            result.push(Store {
                site: s.value_addr[boundary.at as usize],
                key,
                range: Range::new(base.offset(offset as u32), u32::from(s.value(value).size)),
                returned_pointer: false,
            });
        }
    }
    result
}

fn body(s: &Ssa, mut stores: Vec<Store>) -> Result<Body> {
    anyhow::ensure!(
        s.entries.len() == 1 && s.entries[0].addr == s.addr,
        "multiple-or-interior-entries"
    );
    let mut addresses = Addresses::with_parameters(s);
    for boundary in &s.boundaries {
        if s.value(boundary.at).op != Op::Return {
            continue;
        }
        let returned_pointer = Field::ALL
            .into_iter()
            .take(8)
            .filter(|&field| field != Field::Esp)
            .any(|field| {
                matches!(addresses.state(boundary.at, field), Address::Known {
                base: Base::Frame, offset, ..
            } if (offset as i32) < 0)
            });
        if returned_pointer {
            for store in &mut stores {
                if store.site == s.value_addr[boundary.at as usize] {
                    store.returned_pointer = true;
                }
            }
        }
    }
    let accesses = rk_ssa::memory::accesses_with_addresses(s, &mut addresses, None);
    let mut blocks: Vec<Block> = s
        .blocks
        .iter()
        .map(|_| Block {
            events: Vec::new(),
            next: Vec::new(),
        })
        .collect();
    for (index, block) in s.blocks.iter().enumerate() {
        for &pred in &block.preds {
            if pred != rk_ssa::DISPATCH_BLOCK {
                blocks[pred as usize].next.push(index);
            }
        }
        for &id in &block.ops {
            let op = s.value(id).op;
            let effect = match op {
                Op::Call { target } => Some(Effect::Call {
                    target: Some(target),
                    frame: addresses.call_frame(id),
                }),
                Op::CallInd => Some(Effect::Call {
                    target: None,
                    frame: addresses.call_frame(id),
                }),
                Op::Return => Some(Effect::Return),
                Op::Branch { .. } | Op::CBranch { .. } if s.boundary(id).is_some() => {
                    Some(Effect::Barrier("external-branch".into()))
                }
                Op::BranchInd
                | Op::Tail { .. }
                | Op::ImportTail { .. }
                | Op::Seh(_)
                | Op::Trap(_)
                | Op::Breakpoint => Some(Effect::Barrier(format!("opaque-control-flow:{op:?}"))),
                _ if rk_ir::properties(op).opaque => {
                    Some(Effect::Barrier(format!("opaque-operation:{op:?}")))
                }
                _ => accesses.get(&id).map(|access| match access.location {
                    Some(range) if access.reads.opaque || !access.reads.ranges.is_empty() => {
                        Effect::Read(range)
                    }
                    Some(range) => Effect::Write(range),
                    None => Effect::Barrier("unmodelled-memory-effect".into()),
                }),
            };
            if let Some(effect) = effect {
                blocks[index].events.push(Event {
                    site: s.value_addr[id as usize],
                    effect,
                    esp: addresses.state(id, Field::Esp),
                });
            }
        }
    }
    let entry = s.entries[0].target as usize;
    anyhow::ensure!(entry < blocks.len(), "no-entry-block");
    Ok(Body {
        entry,
        blocks,
        stores,
        instructions: BTreeMap::new(),
        return_slot_proven: false,
    })
}

fn terminal(reason: &str, search: &Search, site: Addr, detail: Value) -> Value {
    json!({
        "reason": reason, "function": format!("{:08x}", search.cursor.function),
        "site": format!("{site:08x}"), "remaining_bytes": search.remaining,
        "call_depth": search.resumes.len(), "trace": search.trace, "detail": detail,
    })
}

fn investigate(bodies: &BTreeMap<Addr, Body>, start: Cursor, range: Range) -> Vec<Value> {
    let Address::Known { span, .. } = range.address else {
        return vec![json!({"reason": "unknown-call-frame"})];
    };
    let Some(width) = span
        .checked_add(range.size)
        .filter(|&width| width <= BYTE_LIMIT)
    else {
        return vec![json!({"reason": "byte-budget", "range": describe(range)})];
    };
    let mut pending = vec![Search {
        cursor: start,
        resumes: Vec::new(),
        remaining: (0..width).collect(),
        trace: Vec::new(),
        visited: BTreeSet::new(),
        calls: 0,
    }];
    let mut terminals = Vec::new();
    let mut steps = 0;
    while let Some(mut search) = pending.pop() {
        steps += 1;
        if steps > STEP_LIMIT {
            terminals.push(terminal(
                "step-budget",
                &search,
                0,
                json!({"pending_paths": pending.len() + 1}),
            ));
            break;
        }
        let cursor = search.cursor;
        let key = (
            cursor.function,
            cursor.block,
            cursor.index,
            cursor.frame,
            search.remaining.iter().copied().collect(),
            search.resumes.len(),
        );
        if !search.visited.insert(key) {
            terminals.push(terminal("loop", &search, 0, Value::Null));
            continue;
        }
        let current = &bodies[&cursor.function];
        let block = &current.blocks[cursor.block];
        let Some(event) = block.events.get(cursor.index) else {
            if block.next.is_empty() {
                terminals.push(terminal("external-continuation", &search, 0, Value::Null));
            } else {
                for &next in &block.next {
                    let mut next_search = search.clone();
                    next_search.cursor.block = next;
                    next_search.cursor.index = 0;
                    pending.push(next_search);
                }
            }
            continue;
        };
        search.cursor.index += 1;
        let byte = |index: u32| {
            let Address::Known { base, offset, .. } = range.address.offset(index) else {
                unreachable!()
            };
            Range::new(Address::exact(base, offset), 1)
        };
        match &event.effect {
            Effect::Read(read) => {
                let read = Range::new(mapped(read.address, cursor), read.size);
                let touched: Vec<_> = search
                    .remaining
                    .iter()
                    .copied()
                    .filter(|&i| !read.disjoint(byte(i)))
                    .collect();
                if !touched.is_empty() {
                    let definite = touched.iter().any(|&i| read.offset_of(byte(i)).is_some());
                    let below_esp = touched.iter().any(|&i| {
                        match (mapped(event.esp, cursor), byte(i).address) {
                            (
                                Address::Known {
                                    base,
                                    offset,
                                    span: 0,
                                },
                                Address::Known {
                                    base: own,
                                    offset: at,
                                    ..
                                },
                            ) if base == own => (at.wrapping_sub(offset) as i32) < 0,
                            _ => false,
                        }
                    });
                    terminals.push(terminal(if definite { "read-before-overwrite" } else { "read-may-alias" }, &search, event.site,
                        json!({"read": describe(read), "touched_bytes": touched, "below_esp": below_esp,
                               "partially_overwritten": search.remaining.len() < width as usize})));
                    continue;
                }
            }
            Effect::Write(write) => {
                let write = Range::new(mapped(write.address, cursor), write.size);
                let guaranteed = match write.address {
                    Address::Known { base, offset, span } if write.size > span => Some(Range::new(
                        Address::exact(base, offset.wrapping_add(span)),
                        write.size - span,
                    )),
                    _ => None,
                };
                if let Some(guaranteed) = guaranteed {
                    let before = search.remaining.len();
                    search
                        .remaining
                        .retain(|&i| guaranteed.offset_of(byte(i)).is_none());
                    if before != search.remaining.len() {
                        search.trace.push(json!({"function": format!("{:08x}", cursor.function), "site": format!("{:08x}", event.site),
                            "write": describe(write), "bytes_killed": before - search.remaining.len()}));
                    }
                    if search.remaining.is_empty() {
                        terminals.push(terminal(
                            "overwritten",
                            &search,
                            event.site,
                            json!({"entered_calls": search.calls}),
                        ));
                        continue;
                    }
                }
            }
            Effect::Call { target, frame } => {
                let next = target.and_then(|target| bodies.get(&target).map(|body| (target, body)));
                let uses_stack = next.is_some_and(|(_, body)| {
                    body.blocks.iter().flat_map(|b| &b.events).any(|event| {
                        matches!(
                            event.effect,
                            Effect::Write(Range {
                                address: Address::Known {
                                    base: Base::Frame,
                                    ..
                                },
                                ..
                            })
                        )
                    })
                });
                let reason = if search.resumes.len() >= CALL_LIMIT {
                    Some("call-depth-limit")
                } else if next.is_none() {
                    Some(if target.is_some() {
                        "callee-unavailable"
                    } else {
                        "indirect-call"
                    })
                } else {
                    None
                };
                if let Some(reason) = reason {
                    terminals.push(terminal(
                        reason,
                        &search,
                        event.site,
                        json!({"target": target.map(|target| format!("{target:08x}")),
                        "next_callee_has_stack_writes": uses_stack,
                        "assume_dead_after_stack_call_candidate": uses_stack}),
                    ));
                    continue;
                }
                let (target, callee) = next.unwrap();
                search.trace.push(json!({"call": format!("{:08x}", event.site), "caller": format!("{:08x}", cursor.function), "callee": format!("{target:08x}")}));
                search.resumes.push(search.cursor);
                search.calls += 1;
                search.cursor = Cursor {
                    function: target,
                    block: callee.entry,
                    index: 0,
                    frame: mapped(*frame, cursor),
                    original: false,
                };
            }
            Effect::Return => {
                if !search.resumes.is_empty() && !current.return_slot_proven {
                    terminals.push(terminal(
                        "unproven-callee-return",
                        &search,
                        event.site,
                        Value::Null,
                    ));
                    continue;
                }
                if let Some(resume) = search.resumes.pop() {
                    search.cursor = resume;
                } else {
                    terminals.push(terminal("caller-return", &search, event.site,
                        json!({"caller_unwind_not_analyzed": true, "assume_dead_after_caller_return_candidate": true})));
                    continue;
                }
            }
            Effect::Barrier(reason) => {
                terminals.push(terminal(reason, &search, event.site, Value::Null));
                continue;
            }
        }
        pending.push(search);
    }
    terminals
}

pub(super) fn write(
    ctx: &BodyContext<'_>,
    prepared: Vec<Prepared>,
    selected: &BTreeSet<Addr>,
    opaque: &BTreeSet<Addr>,
    path: &Path,
) -> Result<()> {
    let results: Vec<_> = prepared
        .into_par_iter()
        .map(|prepared| {
            let (addr, ir, planned) = match prepared {
                Prepared::Retained { addr, ir, planned } => (addr, ir, planned),
                Prepared::Emitted((addr, _, reason, _)) => {
                    return Err((addr, format!("planning-unavailable:{reason:?}")))
                }
            };
            if opaque.contains(&addr) {
                return Err((addr, "native-replacement".into()));
            }
            let result = (|| -> Result<Body> {
                let candidates = stores(&planned);
                let cg = rk_ir::lower(&ir, &ctx.program.image, &rk_x87::Recipes)?;
                let mut original = rk_ssa::build(
                    &cg,
                    &BuildOptions {
                        policy: Policy::Optimizing,
                        call_facts: &ctx.facts,
                        ..Default::default()
                    },
                )
                .map_err(rk_core::Error::Diagnostic)?;
                rk_ssa::canonicalize(&mut original, true);
                let mut result = body(&original, candidates)?;
                result.return_slot_proven = planned.ssa.return_slot.is_ok();
                result.instructions = ir
                    .insns
                    .iter()
                    .map(|insn| (insn.addr, insn.text.clone()))
                    .collect();
                Ok(result)
            })();
            result
                .map(|body| (addr, body))
                .map_err(|error| (addr, error.to_string()))
        })
        .collect();
    let mut bodies = BTreeMap::new();
    let mut unavailable = Vec::new();
    for result in results {
        match result {
            Ok((addr, body)) => {
                bodies.insert(addr, body);
            }
            Err((addr, reason)) => {
                unavailable.push(json!({"function": format!("{addr:08x}"), "reason": reason}))
            }
        }
    }
    let mut rows = Vec::new();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut site_coverage = BTreeMap::<String, usize>::new();
    let mut candidate_calls = BTreeSet::new();
    let mut candidate_slots = BTreeSet::new();
    let mut queried_stores = BTreeSet::new();
    let mut indirect_calls = 0;
    for (&caller, body) in &bodies {
        for (block, item) in body.blocks.iter().enumerate() {
            for (index, event) in item.events.iter().enumerate() {
                indirect_calls +=
                    usize::from(matches!(event.effect, Effect::Call { target: None, .. }));
                let Effect::Call {
                    target: Some(target),
                    frame,
                } = event.effect
                else {
                    continue;
                };
                if !selected.is_empty() && !selected.contains(&target) {
                    continue;
                }
                let Some(callee) = bodies.get(&target) else {
                    continue;
                };
                if callee.stores.is_empty() {
                    continue;
                }
                *site_coverage.entry("direct_call_sites".into()).or_default() += 1;
                for store in &callee.stores {
                    queried_stores.insert((target, store.site, store.key));
                    let cursor = Cursor {
                        function: caller,
                        block,
                        index: index + 1,
                        frame: Address::exact(Base::Frame, 0),
                        original: true,
                    };
                    let range = Range::new(
                        mapped(
                            store.range.address,
                            Cursor {
                                frame,
                                original: false,
                                ..cursor
                            },
                        ),
                        store.range.size,
                    );
                    let mut terminals = if store.returned_pointer {
                        vec![
                            json!({"reason": "returned-frame-pointer", "function": format!("{target:08x}"), "site": format!("{:08x}", store.site)}),
                        ]
                    } else {
                        investigate(&bodies, cursor, range)
                    };
                    for diagnostic in &mut terminals {
                        let function = diagnostic["function"].as_str().and_then(rk_core::parse_hex);
                        let site = diagnostic["site"].as_str().and_then(rk_core::parse_hex);
                        if let Some(text) = function.zip(site).and_then(|(function, site)| {
                            bodies.get(&function)?.instructions.get(&site)
                        }) {
                            diagnostic["instruction"] = json!(text);
                        }
                    }
                    let outcome = if terminals.iter().all(|t| t["reason"] == "overwritten") {
                        "overwrite-candidate"
                    } else if terminals
                        .iter()
                        .any(|t| t["reason"] == "read-before-overwrite")
                    {
                        "observed"
                    } else {
                        "unresolved"
                    };
                    if outcome == "overwrite-candidate" {
                        candidate_calls.insert(event.site);
                        candidate_slots.insert((event.site, target, store.key));
                    }
                    *counts.entry(outcome.into()).or_default() += 1;
                    for reason in terminals
                        .iter()
                        .filter_map(|t| t["reason"].as_str())
                        .collect::<BTreeSet<_>>()
                    {
                        *counts.entry(format!("terminal:{reason}")).or_default() += 1;
                    }
                    rows.push(json!({"caller": format!("{caller:08x}"), "call": format!("{:08x}", event.site),
                        "callee": format!("{target:08x}"), "return": format!("{:08x}", store.site),
                        "slot": format!("{:?}", store.key), "callee_range": describe(store.range),
                        "caller_range": describe(range), "existing_direct_variant": ctx.direct_targets.contains(&target),
                        "origin_return_slot_proven": callee.return_slot_proven,
                        "outcome": outcome, "terminals": terminals}));
                }
            }
        }
    }
    let publications: usize = bodies.values().map(|body| body.stores.len()).sum();
    let queried_stores = &queried_stores;
    let inventory: Vec<_> = bodies
        .iter()
        .flat_map(|(&function, body)| {
            body.stores.iter().map(move |store| {
        json!({"function": format!("{function:08x}"), "return": format!("{:08x}", store.site),
            "slot": format!("{:?}", store.key), "range": describe(store.range),
            "returned_frame_pointer": store.returned_pointer,
            "queried": queried_stores.contains(&(function, store.site, store.key))})
    })
        })
        .collect();
    let assumption_bounds: BTreeMap<_, _> = [
        (
            "stop_at_next_stack_call",
            "assume_dead_after_stack_call_candidate",
        ),
        (
            "stop_at_first_caller_return",
            "assume_dead_after_caller_return_candidate",
        ),
    ]
    .into_iter()
    .map(|(name, field)| {
        let count = rows
            .iter()
            .filter(|row| {
                row["outcome"] == "unresolved"
                    && row["terminals"].as_array().is_some_and(|terminals| {
                        terminals.iter().all(|terminal| {
                            terminal["reason"] == "overwritten" || terminal["detail"][field] == true
                        })
                    })
            })
            .count();
        (name, count)
    })
    .collect();
    let report = json!({"format": "recomp-stack-return-report-v1", "read_only": true,
        "executable_sha256": ctx.program.sha256, "image_base": format!("{:08x}", ctx.program.image.base),
        "scope": "Normal continuations of ordinary planned return publications; original unpromoted SSA supplies effects. Candidates require runtime observer guards and return/entry contracts before optimization. Indirect and external callers prevent global elimination from these counts alone.",
        "limits": {"steps_per_query": STEP_LIMIT, "nested_calls": CALL_LIMIT, "caller_unwinds": 0, "bytes_per_query": BYTE_LIMIT},
        "assumptions": {"private_stack": ctx.assume_private_stack, "stack_pointer_bases": ctx.assume_stack_pointer_bases,
            "dead_after_stack_call": false, "dead_after_caller_returns": false},
        "coverage": {"functions": bodies.len(), "return_publications": publications, "sites": site_coverage,
            "indirect_call_sites_not_queried": indirect_calls, "unavailable_functions": unavailable},
        "candidate_call_sites": candidate_calls.len(), "candidate_call_slots": candidate_slots.len(),
        "assumption_only_upper_bounds": assumption_bounds,
        "assumption_scope": "Stack-call bounds use syntactic stack writes, not proof that every path writes. First-caller-return is more aggressive than a two- or three-caller cutoff; those are not measured.",
        "publications": inventory, "counts": counts, "rows": rows});
    std::fs::write(path, serde_json::to_string_pretty(&report)? + "\n")?;
    eprintln!("stack-return report: {} functions, {publications} publications, {} call/publication queries; {:?}", bodies.len(), report["rows"].as_array().unwrap().len(), report["counts"]);
    Ok(())
}
