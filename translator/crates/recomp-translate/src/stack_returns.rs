use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use rk_core::contract::Field;
use rk_core::memory::{Address, Base, Range};
use rk_core::Addr;
use rk_ir::{Op, StateKey};
use rk_ssa::address::{Addresses, Relations};
use rk_ssa::{CallFacts, Ssa, ValueId};
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
    private_frame: bool,
    return_position: &'static str,
}

enum Effect {
    Read(Range),
    Write(Range),
    Call {
        target: Option<Addr>,
        frame: Address,
        registers: [Address; 8],
    },
    Return,
    Barrier(String),
}

struct Event {
    site: Addr,
    effect: Effect,
    esp: Address,
    address_origin: Value,
}

struct Block {
    events: Vec<Event>,
    next: Vec<usize>,
}

pub(super) struct Body {
    entry: usize,
    blocks: Vec<Block>,
    stores: Vec<Store>,
    instructions: BTreeMap<Addr, String>,
    return_slot_proven: bool,
    address_work: (usize, bool),
    forwarded_reads: Vec<Value>,
    relations: Relations,
    returned_pointer_sites: BTreeSet<Addr>,
    frame: rk_ssa::memory::Frame,
}

#[derive(Clone, Copy)]
struct Cursor {
    function: Addr,
    block: usize,
    index: usize,
    frame: Address,
    original: bool,
    registers: [Address; 8],
}

#[derive(Clone)]
struct Search {
    cursor: Cursor,
    resumes: Vec<Cursor>,
    remaining: BTreeSet<u32>,
    trace: Vec<Value>,
    visited: BTreeSet<(Addr, usize, usize, Address, [Address; 8], Vec<u32>, usize)>,
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
        Address::Known {
            base: Base::Parameter(index),
            offset,
            span,
        } => cursor
            .registers
            .get(index as usize)
            .copied()
            .unwrap_or(Address::Unknown)
            .offset(offset)
            .bounded(0, span),
        _ if cursor.original => address,
        _ => Address::Unknown,
    }
}

fn mapped_in(address: Address, cursor: Cursor, body: &Body) -> Address {
    let address = if cursor.original {
        address
    } else {
        body.relations.bounds(address)
    };
    mapped(address, cursor)
}

fn address_origin(s: &Ssa, root: ValueId, facts: &dyn CallFacts) -> Value {
    let mut pending = vec![root];
    let mut seen = BTreeSet::new();
    let mut inputs = BTreeSet::new();
    let mut reloads = BTreeMap::new();
    let mut loads = BTreeSet::new();
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
                    inputs.insert(field.name());
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
                    reloads.insert((field, site), json!({"field": field.name(), "site": format!("{site:08x}"),
                        "target": target.map(|target| format!("{target:08x}")), "cleanup": facts.summary(site, target).cleanup}));
                }
            }
            Op::P(rk_core::ir::OpCode::Load) => {
                loads.insert(format!("{:08x}", s.value_addr[id as usize]));
            }
            op if rk_ir::properties(op).pure => pending.extend(s.args(id)),
            _ => {}
        }
    }
    json!({"value": root, "operation": format!("{:?}", s.value(root).op),
        "inputs": inputs, "reloads": reloads.into_values().collect::<Vec<_>>(),
        "pointer_loads": loads, "walk_limited": limited})
}

fn alias_class(
    raw: Range,
    read: Range,
    target: Range,
    cursor: Cursor,
    origin: &Value,
) -> &'static str {
    if !cursor.original
        && matches!(
            raw.address,
            Address::Known {
                base: Base::Parameter(_),
                ..
            }
        )
    {
        return if read.address == Address::Unknown {
            "callee-register-binding-unresolved"
        } else {
            "callee-register-binding-may-alias"
        };
    }
    if !cursor.original
        && matches!(
            raw.address,
            Address::Known {
                base: Base::Argument(_),
                ..
            }
        )
    {
        return "callee-stack-argument-not-substituted";
    }
    if !cursor.original && raw.address != Address::Unknown && read.address == Address::Unknown {
        return "callee-local-root-not-mapped";
    }
    if matches!(
        read.address,
        Address::Known {
            base: Base::Absolute,
            ..
        }
    ) {
        return "absolute-versus-symbolic-stack";
    }
    if let (
        Address::Known { base, span, .. },
        Address::Known {
            base: own,
            span: own_span,
            ..
        },
    ) = (read.address, target.address)
    {
        if base == own && (span != 0 || own_span != 0) {
            return "bounded-stack-ranges-overlap";
        }
    }
    if origin["reloads"]
        .as_array()
        .is_some_and(|reloads| reloads.iter().any(|reload| reload["field"] == "ESP"))
    {
        return "stack-state-reloaded-after-call";
    }
    if origin["reloads"]
        .as_array()
        .is_some_and(|reloads| !reloads.is_empty())
    {
        return "call-returned-register-origin";
    }
    if matches!(
        read.address,
        Address::Known {
            base: Base::Parameter(_) | Base::Argument(_),
            ..
        }
    ) {
        return "incoming-pointer-versus-stack";
    }
    if origin["inputs"]
        .as_array()
        .is_some_and(|inputs| inputs.iter().any(|input| input == "ESP"))
        && origin["pointer_loads"]
            .as_array()
            .is_some_and(Vec::is_empty)
    {
        return "unresolved-stack-expression";
    }
    if origin["pointer_loads"]
        .as_array()
        .is_some_and(|loads| !loads.is_empty())
    {
        return "loaded-pointer-origin";
    }
    "unresolved-symbolic-address"
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
                private_frame: false,
                return_position: "unknown",
            });
        }
    }
    result
}

fn body(s: &Ssa, facts: &dyn CallFacts) -> Result<Body> {
    anyhow::ensure!(
        s.entries.len() == 1 && s.entries[0].addr == s.addr,
        "multiple-or-interior-entries"
    );
    let mut addresses = Addresses::local_with_parameters(s);
    let mut call_addresses = Addresses::local_with_parameters(s);
    let mut returned_pointer_sites = BTreeSet::new();
    for boundary in &s.boundaries {
        if s.value(boundary.at).op != Op::Return {
            continue;
        }
        let returned_pointer = Field::ALL
            .into_iter()
            .take(8)
            .filter(|&field| field != Field::Esp)
            .any(|field| {
                let address = addresses.state(boundary.at, field);
                matches!(addresses.relations.bounds(address), Address::Known {
                base: Base::Frame, offset, ..
            } if (offset as i32) < 0)
            });
        if returned_pointer {
            returned_pointer_sites.insert(s.value_addr[boundary.at as usize]);
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
                    registers: std::array::from_fn(|index| {
                        call_addresses.state(id, Field::ALL[index])
                    }),
                }),
                Op::CallInd => Some(Effect::Call {
                    target: None,
                    frame: addresses.call_frame(id),
                    registers: std::array::from_fn(|index| {
                        call_addresses.state(id, Field::ALL[index])
                    }),
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
                let origin = if matches!(effect, Effect::Read(_)) {
                    address_origin(s, s.args(id)[1], facts)
                } else {
                    Value::Null
                };
                blocks[index].events.push(Event {
                    site: s.value_addr[id as usize],
                    effect,
                    esp: addresses.state(id, Field::Esp),
                    address_origin: origin,
                });
            }
        }
    }
    let entry = s.entries[0].target as usize;
    anyhow::ensure!(entry < blocks.len(), "no-entry-block");
    Ok(Body {
        entry,
        blocks,
        stores: Vec::new(),
        instructions: BTreeMap::new(),
        return_slot_proven: false,
        address_work: addresses.work_usage(),
        forwarded_reads: Vec::new(),
        relations: addresses.relations,
        returned_pointer_sites,
        frame: rk_ssa::memory::Frame::new(s, Some(facts)),
    })
}

fn return_position(body: &Body, store: &Store) -> &'static str {
    let esp = body
        .blocks
        .iter()
        .flat_map(|block| &block.events)
        .find(|event| event.site == store.site && matches!(event.effect, Effect::Return))
        .map(|event| body.relations.bounds(event.esp));
    match (body.relations.bounds(store.range.address), esp) {
        (
            Address::Known { base, offset, span },
            Some(Address::Known {
                base: own,
                offset: top,
                span: 0,
            }),
        ) if base == own => {
            if i64::from(offset as i32) + i64::from(span) + i64::from(store.range.size)
                <= i64::from(top as i32)
            {
                "below-esp"
            } else {
                "not-wholly-below-esp"
            }
        }
        _ => "unknown",
    }
}

fn terminal(reason: &str, search: &Search, site: Addr, detail: Value) -> Value {
    json!({
        "reason": reason, "function": format!("{:08x}", search.cursor.function),
        "site": format!("{site:08x}"), "remaining_bytes": search.remaining,
        "call_depth": search.resumes.len(), "trace": search.trace, "detail": detail,
    })
}

fn in_image(range: Range, image: (u32, u32)) -> bool {
    matches!(range.address, Address::Known { base: Base::Absolute, offset, span }
        if offset >= image.0 && u64::from(offset) + u64::from(span) + u64::from(range.size) <= u64::from(image.1))
}

fn investigate(
    bodies: &BTreeMap<Addr, Body>,
    start: Cursor,
    range: Range,
    image_guard: Option<(u32, u32)>,
    private_evidence: bool,
) -> Vec<Value> {
    if image_guard.is_some_and(|image| in_image(range, image)) {
        return vec![json!({"reason": "image-guard-impossible"})];
    }
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
    let relations = &bodies[&start.function].relations;
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
            cursor.registers,
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
        let stack_position =
            |esp: Address| match (relations.bounds(range.address), relations.bounds(esp)) {
                (
                    Address::Known { base, offset, span },
                    Address::Known {
                        base: own,
                        offset: top,
                        span: 0,
                    },
                ) if base == own => Some(
                    i64::from(offset as i32) + i64::from(span) + i64::from(range.size)
                        <= i64::from(top as i32),
                ),
                _ => None,
            };
        match &event.effect {
            Effect::Read(read) => {
                let raw = *read;
                let read = Range::new(mapped_in(read.address, cursor, current), read.size);
                if image_guard.is_some_and(|image| in_image(read, image)) {
                    search.trace.push(json!({"guard": "omitted-slot-outside-image", "function": format!("{:08x}", cursor.function),
                        "site": format!("{:08x}", event.site), "read": describe(read)}));
                    pending.push(search);
                    continue;
                }
                let touched: Vec<_> = search
                    .remaining
                    .iter()
                    .copied()
                    .filter(|&i| !relations.disjoint(read, byte(i)))
                    .collect();
                if !touched.is_empty() {
                    let definite = touched
                        .iter()
                        .any(|&i| relations.offset_of(read, byte(i)).is_some());
                    let stack_derived = matches!(
                        read.address,
                        Address::Known {
                            base: Base::Frame | Base::Stack(_),
                            ..
                        }
                    ) || event.address_origin["inputs"].as_array().is_none_or(
                        |inputs| {
                            inputs.iter().any(|input| {
                                input == "ESP"
                                    || (!cursor.original
                                        && Field::ALL.iter().take(8).enumerate().any(
                                            |(index, field)| {
                                                input == field.name()
                                                    && matches!(
                                                        cursor.registers[index],
                                                        Address::Known {
                                                            base: Base::Frame | Base::Stack(_),
                                                            ..
                                                        }
                                                    )
                                            },
                                        ))
                            })
                        },
                    ) || event.address_origin["reloads"].as_array().is_none_or(
                        |reloads| reloads.iter().any(|reload| reload["field"] == "ESP"),
                    );
                    let walk_limited = event.address_origin["walk_limited"] != false;
                    let register_reloaded = event.address_origin["reloads"]
                        .as_array()
                        .is_none_or(|reloads| !reloads.is_empty());
                    let unknown_binding = !cursor.original
                        && event.address_origin["inputs"]
                            .as_array()
                            .is_none_or(|inputs| {
                                inputs.iter().any(|input| {
                                    Field::ALL.iter().take(8).enumerate().any(|(index, field)| {
                                        input == field.name()
                                            && cursor.registers[index] == Address::Unknown
                                    })
                                })
                            });
                    let unknown_origin = walk_limited || register_reloaded || unknown_binding;
                    if private_evidence
                        && !definite
                        && !stack_derived
                        && !unknown_origin
                        && stack_position(mapped_in(event.esp, cursor, current)) == Some(true)
                    {
                        search.trace.push(json!({"assumption": "unrelated-pointer-does-not-read-returned-private-frame-below-esp", "function": format!("{:08x}", cursor.function), "site": format!("{:08x}", event.site), "read": describe(read)}));
                        pending.push(search);
                        continue;
                    }
                    let below_esp = touched.iter().any(|&i| {
                        match (mapped_in(event.esp, cursor, current), byte(i).address) {
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
                    let reason = if definite {
                        "read-before-overwrite"
                    } else if private_evidence && stack_derived {
                        "stack-read-may-alias"
                    } else if private_evidence && walk_limited {
                        "read-origin-walk-limit"
                    } else if private_evidence && register_reloaded {
                        "read-origin-call-reload"
                    } else if private_evidence && unknown_binding {
                        "callee-register-binding-unknown"
                    } else if private_evidence
                        && stack_position(mapped_in(event.esp, cursor, current)) == Some(false)
                    {
                        "pointer-read-slot-not-below-esp"
                    } else if private_evidence {
                        "pointer-read-stack-position-unknown"
                    } else {
                        "read-may-alias"
                    };
                    terminals.push(terminal(reason, &search, event.site,
                        json!({"read": describe(read), "raw_read": describe(raw),
                               "blocker_class": alias_class(raw, read, range, cursor, &event.address_origin),
                               "address_origin": event.address_origin,
                               "address_work": {"steps": current.address_work.0, "limit_exceeded": current.address_work.1},
                               "whole_slot_below_esp": stack_position(mapped_in(event.esp, cursor, current)),
                               "touched_bytes": touched, "below_esp": below_esp,
                               "partially_overwritten": search.remaining.len() < width as usize})));
                    continue;
                }
            }
            Effect::Write(write) => {
                let write = Range::new(mapped_in(write.address, cursor, current), write.size);
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
                        .retain(|&i| relations.offset_of(guaranteed, byte(i)).is_none());
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
            Effect::Call {
                target,
                frame,
                registers,
            } => {
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
                    frame: mapped_in(*frame, cursor, current),
                    original: false,
                    registers: registers.map(|address| mapped_in(address, cursor, current)),
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
                    if private_evidence {
                        let call = bodies[&resume.function].blocks[resume.block]
                            .events
                            .get(resume.index - 1)
                            .map(|event| format!("{:08x}", event.site));
                        search.trace.push(json!({"completed_call": call, "caller": format!("{:08x}", resume.function), "callee": format!("{:08x}", cursor.function), "return": format!("{:08x}", event.site), "remaining_bytes": search.remaining}));
                    }
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

pub(super) fn capture(s: &mut Ssa, facts: &dyn CallFacts) -> Result<Body> {
    let reads = |s: &Ssa| -> BTreeSet<ValueId> {
        s.blocks.iter().flat_map(|block| &block.ops).copied().filter(|&id| {
            matches!(s.value(id).op, Op::P(rk_core::ir::OpCode::Load))
                || matches!(s.value(id).op, Op::X87(op) if op.memory() == Some(rk_ir::x87::MemoryEffect::Read))
        }).collect()
    };
    let writes = |s: &Ssa| -> BTreeSet<ValueId> {
        s.blocks.iter().flat_map(|block| &block.ops).copied().filter(|&id| {
            matches!(s.value(id).op, Op::P(rk_core::ir::OpCode::Store))
                || matches!(s.value(id).op, Op::X87(op) if op.memory() == Some(rk_ir::x87::MemoryEffect::Write))
        }).collect()
    };
    let before_reads = reads(s);
    let before_writes = writes(s);
    rk_ssa::forward(s, Some(facts));
    anyhow::ensure!(
        before_writes.is_subset(&writes(s)),
        "report forwarding removed a guest store"
    );
    let remaining_reads = reads(s);
    let forwarded = before_reads
        .difference(&remaining_reads)
        .map(|&id| {
            json!({
                "site": format!("{:08x}", s.value_addr[id as usize]), "value": id,
            })
        })
        .collect();
    let mut result = body(s, facts)?;
    result.forwarded_reads = forwarded;
    Ok(result)
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
            let (addr, ir, planned, view) = match prepared {
                Prepared::Retained {
                    addr,
                    ir,
                    planned,
                    view,
                } => (addr, ir, planned, view),
                Prepared::Emitted((addr, _, reason, _)) => {
                    return Err((addr, format!("planning-unavailable:{reason:?}")))
                }
            };
            if opaque.contains(&addr) {
                return Err((addr, "native-replacement".into()));
            }
            let result = (|| -> Result<Body> {
                let candidates = stores(&planned);
                let mut result = view.context("pre-promotion stack view unavailable")??;
                result.stores = candidates
                    .into_iter()
                    .map(|mut store| {
                        store.returned_pointer =
                            result.returned_pointer_sites.contains(&store.site);
                        store.private_frame = result.frame.unexposed(store.range);
                        store.return_position = return_position(&result, &store);
                        store
                    })
                    .collect();
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
    let mut guarded_calls = BTreeSet::new();
    let mut guarded_slots = BTreeSet::new();
    let mut guarded_queries = 0;
    let image = (ctx.program.image.base, ctx.program.image.end());
    for (&caller, body) in &bodies {
        for (block, item) in body.blocks.iter().enumerate() {
            for (index, event) in item.events.iter().enumerate() {
                indirect_calls +=
                    usize::from(matches!(event.effect, Effect::Call { target: None, .. }));
                let Effect::Call {
                    target: Some(target),
                    frame,
                    ..
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
                        registers: std::array::from_fn(|index| {
                            Address::exact(Base::Parameter(index as u8), 0)
                        }),
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
                        investigate(&bodies, cursor, range, None, false)
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
                    let guarded = if outcome == "unresolved"
                        && terminals.iter().any(|terminal| {
                            terminal["reason"] == "read-may-alias"
                                && terminal["detail"]["read"]["base"] == "Absolute"
                        }) {
                        let terminals = investigate(&bodies, cursor, range, Some(image), false);
                        let candidate = terminals
                            .iter()
                            .all(|terminal| terminal["reason"] == "overwritten");
                        if candidate {
                            guarded_queries += 1;
                            guarded_calls.insert(event.site);
                            guarded_slots.insert((event.site, target, store.key));
                        }
                        json!({"outcome": if candidate { "guarded-overwrite-candidate" } else { "unresolved" },
                            "required_guard": "actual omitted slot lies completely outside the loaded image",
                            "image_start": format!("{:08x}", image.0), "image_end": format!("{:08x}", image.1), "terminals": terminals})
                    } else {
                        Value::Null
                    };
                    let evidence = if store.return_position != "below-esp" {
                        json!({"outcome": "outside-scope", "reason": store.return_position})
                    } else if !store.private_frame {
                        json!({"outcome": "unresolved", "terminals": [{"reason": "private-frame-not-proven"}]})
                    } else if store.returned_pointer {
                        json!({"outcome": "unresolved", "terminals": [{"reason": "returned-frame-pointer"}]})
                    } else {
                        let mut frontier = investigate(&bodies, cursor, range, None, true);
                        for terminal in &mut frontier {
                            let function =
                                terminal["function"].as_str().and_then(rk_core::parse_hex);
                            let site = terminal["site"].as_str().and_then(rk_core::parse_hex);
                            if let Some(text) = function.zip(site).and_then(|(function, site)| {
                                bodies.get(&function)?.instructions.get(&site)
                            }) {
                                terminal["instruction"] = json!(text);
                            }
                        }
                        let outcome = if frontier
                            .iter()
                            .any(|terminal| terminal["reason"] == "read-before-overwrite")
                        {
                            "definite-read-before-overwrite"
                        } else if !frontier.is_empty()
                            && frontier
                                .iter()
                                .all(|terminal| terminal["reason"] == "overwritten")
                        {
                            "overwritten-before-read"
                        } else if !frontier.is_empty()
                            && frontier.iter().all(|terminal| {
                                terminal["reason"] == "overwritten"
                                    || terminal["reason"] == "caller-return"
                            })
                        {
                            "unread-at-caller-return"
                        } else {
                            "unresolved"
                        };
                        let reached_call = |terminal: &Value| {
                            terminal["trace"].as_array().is_some_and(|trace| {
                                trace
                                    .iter()
                                    .any(|step| step.get("completed_call").is_some())
                            })
                        };
                        let unread_through_call = frontier.iter().any(reached_call)
                            && frontier.iter().all(|terminal| {
                                terminal["reason"] != "step-budget"
                                    && (terminal["reason"] == "overwritten"
                                        || reached_call(terminal))
                            });
                        json!({"outcome": outcome, "unread_through_analyzed_call": unread_through_call, "terminals": frontier})
                    };
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
                        "outcome": outcome, "terminals": terminals, "slot_outside_image": guarded, "private_frame": store.private_frame, "return_position": store.return_position, "stack_evidence": evidence}));
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
            "returned_frame_pointer": store.returned_pointer, "private_frame": store.private_frame, "return_position": store.return_position,
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
    let classes: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| row["terminals"].as_array().into_iter().flatten())
        .filter(|terminal| terminal["reason"] == "read-may-alias")
        .filter_map(|terminal| terminal["detail"]["blocker_class"].as_str())
        .collect();
    let blockers: BTreeMap<_, _> = classes.into_iter().map(|class| {
        let matching = |terminal: &&Value| terminal["reason"] == "read-may-alias" && terminal["detail"]["blocker_class"] == class;
        let queries: Vec<_> = rows.iter().filter(|row| row["terminals"].as_array().unwrap().iter().any(|terminal| matching(&terminal))).collect();
        let instructions: BTreeSet<_> = queries.iter().flat_map(|row| row["terminals"].as_array().unwrap().iter().filter(matching))
            .map(|terminal| (terminal["function"].as_str(), terminal["site"].as_str())).collect();
        let calls: BTreeSet<_> = queries.iter().map(|row| row["call"].as_str()).collect();
        let sole: Vec<_> = queries.iter().filter(|row| row["terminals"].as_array().unwrap().iter()
            .all(|terminal| terminal["reason"] == "overwritten" || matching(&terminal))).collect();
        let examples: Vec<_> = queries.iter().take(3).map(|row| json!({"caller": row["caller"], "call": row["call"],
            "callee": row["callee"], "return": row["return"], "slot": row["slot"]})).collect();
        (class, json!({"queries": queries.len(), "call_sites": calls.len(), "blocking_instructions": instructions.len(),
            "sole_blocker_queries": sole.len(), "examples": examples}))
    }).collect();
    let evidence_outcomes: BTreeMap<_, _> = ["definite-read-before-overwrite", "overwritten-before-read", "unread-at-caller-return", "unresolved", "outside-scope"].into_iter().map(|outcome| {
        let matching: Vec<_> = rows.iter().filter(|row| row["stack_evidence"]["outcome"] == outcome).collect();
        let calls: BTreeSet<_> = matching.iter().filter_map(|row| row["call"].as_str()).collect();
        let slots: BTreeSet<_> = matching.iter().map(|row| (row["call"].as_str(), row["callee"].as_str(), row["slot"].as_str())).collect();
        (outcome, json!({"queries": matching.len(), "call_sites": calls.len(), "call_slots": slots.len()}))
    }).collect();
    let frontier_reasons: BTreeSet<_> = rows
        .iter()
        .filter(|row| row["stack_evidence"]["outcome"] == "unresolved")
        .flat_map(|row| {
            row["stack_evidence"]["terminals"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|terminal| terminal["reason"].as_str())
        .filter(|reason| *reason != "overwritten" && *reason != "caller-return")
        .collect();
    let evidence_frontiers: BTreeMap<_, _> = frontier_reasons
        .into_iter()
        .map(|reason| {
            let matching: Vec<_> = rows
                .iter()
                .filter(|row| {
                    row["stack_evidence"]["outcome"] == "unresolved"
                        && row["stack_evidence"]["terminals"]
                            .as_array()
                            .is_some_and(|terminals| {
                                terminals
                                    .iter()
                                    .any(|terminal| terminal["reason"] == reason)
                            })
                })
                .collect();
            let calls: BTreeSet<_> = matching
                .iter()
                .filter_map(|row| row["call"].as_str())
                .collect();
            (
                reason,
                json!({"queries": matching.len(), "call_sites": calls.len()}),
            )
        })
        .collect();
    let through_call: Vec<_> = rows
        .iter()
        .filter(|row| row["stack_evidence"]["unread_through_analyzed_call"] == true)
        .collect();
    let through_call_sites: BTreeSet<_> = through_call
        .iter()
        .filter_map(|row| row["call"].as_str())
        .collect();
    let through_call_slots: BTreeSet<_> = through_call
        .iter()
        .map(|row| {
            (
                row["call"].as_str(),
                row["callee"].as_str(),
                row["slot"].as_str(),
            )
        })
        .collect();
    let through_call_outcomes: BTreeMap<_, _> = [
        "definite-read-before-overwrite",
        "overwritten-before-read",
        "unread-at-caller-return",
        "unresolved",
    ]
    .into_iter()
    .map(|outcome| {
        (
            outcome,
            through_call
                .iter()
                .filter(|row| row["stack_evidence"]["outcome"] == outcome)
                .count(),
        )
    })
    .collect();
    let forwarded: BTreeMap<_, _> = bodies
        .iter()
        .filter(|(_, body)| !body.forwarded_reads.is_empty())
        .map(|(&function, body)| (format!("{function:08x}"), &body.forwarded_reads))
        .collect();
    let report = json!({"format": "recomp-stack-return-report-v1", "read_only": true,
        "executable_sha256": ctx.program.sha256, "image_base": format!("{:08x}", ctx.program.image.base), "image_size": ctx.program.image.size,
        "scope": "Normal continuations of ordinary planned return publications; pre-promotion SSA uses per-function production assumptions and retains every original store. Production forwarding removes reads proven to consume prior writes. Stack evidence uses private-frame exposure and general address relations. Candidates require runtime observer guards and return/entry contracts before optimization. Indirect and external callers prevent global elimination from these counts alone.",
        "limits": {"steps_per_query": STEP_LIMIT, "nested_calls": CALL_LIMIT, "caller_unwinds": 0, "bytes_per_query": BYTE_LIMIT},
        "assumptions": {"private_stack": ctx.assume_private_stack, "stack_pointer_bases": ctx.assume_stack_pointer_bases,
            "dead_after_stack_call": false, "dead_after_caller_returns": false},
        "coverage": {"functions": bodies.len(), "return_publications": publications, "sites": site_coverage,
            "indirect_call_sites_not_queried": indirect_calls, "unavailable_functions": unavailable},
        "candidate_call_sites": candidate_calls.len(), "candidate_call_slots": candidate_slots.len(),
        "additional_candidates_with_slot_outside_image_guard": {"queries": guarded_queries, "call_sites": guarded_calls.len(), "call_slots": guarded_slots.len()},
        "stack_evidence_counts": evidence_outcomes,
        "unread_through_analyzed_call": {"queries": through_call.len(), "call_sites": through_call_sites.len(), "call_slots": through_call_slots.len(), "later_outcomes": through_call_outcomes, "scope": "Every explored path overwrites all tracked bytes or completes a subsequent analyzed call with bytes surviving and no stack read; at least one path completes such a call. Later reads or unresolved behavior remain possible. Query counts overlap the main outcome categories."},
        "stack_evidence_unresolved_frontiers": evidence_frontiers,
        "stack_evidence_frontier_counting": "Each unresolved query is counted once per terminal reason; queries can have multiple reasons; overwritten and caller-return paths are not counted as unresolved frontiers.",
        "stack_evidence_scope": "Only publications proved wholly below restored ESP. Production private-stack excludes unrelated pointer reads of unexposed returned slots while below current ESP. Definite reads take precedence over unresolved paths. Unread-at-caller-return requires all paths to overwrite or reach the first caller return unread; it does not prove the bytes dead afterward. No emission changes.",
        "assumption_only_upper_bounds": assumption_bounds,
        "assumption_scope": "Stack-call bounds use syntactic stack writes, not proof that every path writes. First-caller-return is more aggressive than a two- or three-caller cutoff; those are not measured.",
        "blocker_classes": blockers,
        "initialized_reads_forwarded": forwarded,
        "blocker_counting": "Queries can contain several classes. Sole-blocker counts describe current terminal frontiers, not proofs that bypassing one class would finish a query.",
        "publications": inventory, "counts": counts, "rows": rows});
    std::fs::write(path, serde_json::to_string_pretty(&report)? + "\n")?;
    eprintln!("stack-return report: {} functions, {publications} publications, {} call/publication queries; stack evidence {:?}", bodies.len(), report["rows"].as_array().unwrap().len(), report["stack_evidence_counts"]);
    Ok(())
}
