use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;
use rk_core::contract::Field;
use rk_core::memory::{Address, Base, Range};
use rk_core::Addr;
use rk_ir::{Op, StateKey};
use rk_ssa::address::{Addresses, Relations};
use rk_ssa::initialization::Initialization;
use rk_ssa::{CallEffect, CallFacts, CallSummary, Ssa, ValueId};
use serde_json::{json, Value};

use super::{BodyContext, Prepared};

const STEP_LIMIT: usize = 4096;
const CALL_LIMIT: usize = 1;
const BYTE_LIMIT: u32 = 256;

struct ReportFacts<'a> {
    base: &'a dyn CallFacts,
    cleanup: &'a BTreeMap<Addr, i32>,
}

impl CallFacts for ReportFacts<'_> {
    fn call(&self, site: Addr, target: Option<Addr>) -> CallEffect {
        self.base.call(site, target)
    }

    fn summary(&self, site: Addr, target: Option<Addr>) -> CallSummary<'_> {
        let mut result = self.base.summary(site, target);
        if result.cleanup.is_none() {
            result.cleanup = target.and_then(|target| self.cleanup.get(&target).copied());
        }
        result
    }
}

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
    initialized_reads: BTreeMap<(usize, usize), Initialization>,
    return_origins: BTreeMap<Field, Value>,
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
    visited: BTreeSet<(
        Addr,
        usize,
        usize,
        Address,
        [Address; 8],
        Vec<u32>,
        usize,
        usize,
    )>,
    calls: usize,
    completed_calls: usize,
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

fn address_origins(s: &Ssa, roots: &[ValueId], facts: &dyn CallFacts) -> Value {
    let origins = rk_ssa::origin::origins(s, roots, facts);
    json!({"value": origins.value, "operation": origins.operation,
        "inputs": origins.inputs.iter().map(|field| field.name()).collect::<BTreeSet<_>>(),
        "reloads": origins.reloads.iter().map(|reload| json!({"field": reload.field.name(), "site": format!("{:08x}", reload.site),
            "target": reload.target.map(|addr| format!("{addr:08x}")), "cleanup": reload.cleanup})).collect::<Vec<_>>(),
        "pointer_loads": origins.pointer_loads.iter().map(|addr| format!("{addr:08x}")).collect::<Vec<_>>(),
        "walk_limited": origins.walk_limited, "unresolved_operations": origins.unresolved_operations})
}

fn address_origin(s: &Ssa, root: ValueId, facts: &dyn CallFacts) -> Value {
    address_origins(s, &[root], facts)
}

fn return_origins(
    s: &Ssa,
    facts: &dyn CallFacts,
    addresses: &mut Addresses<'_>,
    complete: bool,
) -> BTreeMap<Field, Value> {
    Field::ALL.into_iter().take(8).filter(|&field| field != Field::Esp).map(|field| {
        let mut returns = Vec::new();
        let mut independent = complete;
        for boundary in &s.boundaries {
            if s.value(boundary.at).op != Op::Return {
                continue;
            }
            let state = s.boundary(boundary.at).unwrap_or_default();
            let roots: Vec<_> = rk_ir::lanes(field).iter().filter_map(|key| state.iter().find(|(candidate, _)| candidate == key).map(|&(_, value)| value)).collect();
            let origin = address_origins(s, &roots, facts);
            let address = addresses.state(boundary.at, field);
            let stack_address = matches!(address, Address::Known { base: Base::Frame | Base::Stack(_) | Base::Argument(_), .. });
            let mut local_load = false;
            for &id in s.blocks.iter().flat_map(|block| &block.ops) {
                if !matches!(s.value(id).op, Op::P(rk_core::ir::OpCode::Load))
                    || !origin["pointer_loads"].as_array().is_some_and(|loads| loads.contains(&json!(format!("{:08x}", s.value_addr[id as usize])))) {
                    continue;
                }
                let source = addresses.get(s.args(id)[1]);
                let source_origin = address_origin(s, s.args(id)[1], facts);
                let argument = matches!(addresses.relations.bounds(source), Address::Known { base: Base::Frame, offset, span: 0 } if (offset as i32) >= 4);
                let stack_source = matches!(source, Address::Known { base: Base::Frame | Base::Stack(_), .. })
                    || source_origin["inputs"].as_array().is_none_or(|inputs| inputs.contains(&json!("ESP")));
                local_load |= !argument && (stack_source || source_origin["walk_limited"] != false
                    || source_origin["unresolved_operations"].as_array().is_none_or(|operations| !operations.is_empty())
                    || source_origin["reloads"].as_array().is_none_or(|reloads| !reloads.is_empty()));
            }
            let proved = roots.len() == 4 && !stack_address && !local_load && origin["walk_limited"] == false
                && origin["unresolved_operations"].as_array().is_some_and(Vec::is_empty)
                && origin["inputs"].as_array().is_some_and(|inputs| !inputs.contains(&json!("ESP")))
                && origin["reloads"].as_array().is_some_and(Vec::is_empty);
            independent &= proved;
            returns.push(json!({"site": format!("{:08x}", s.value_addr[boundary.at as usize]), "address": describe(Range::new(address, 4)),
                "origin": origin, "local_or_unresolved_pointer_load": local_load, "stack_independent": proved}));
        }
        (field, json!({"stack_independent": independent && !returns.is_empty(), "complete_control_flow": complete, "returns": returns}))
    }).collect()
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

pub(super) fn omission_plan(
    planned: &rk_plan::PlannedFunction,
    summary: &rk_ssa::memory::ReturnStackSummary,
) -> BTreeMap<ValueId, Vec<StateKey>> {
    let mut result = BTreeMap::new();
    if planned.ssa.return_slot.is_err() {
        return result;
    }
    for store in stores(planned) {
        if !summary.eligible(store.site, store.range) {
            continue;
        }
        if let Some(boundary) = planned.ssa.boundaries.iter().find(|boundary| {
            planned.ssa.value(boundary.at).op == Op::Return
                && planned.ssa.value_addr[boundary.at as usize] == store.site
        }) {
            result
                .entry(boundary.at)
                .or_insert_with(Vec::new)
                .push(store.key);
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
    let complete = blocks.iter().all(|block| {
        !block
            .events
            .iter()
            .any(|event| matches!(event.effect, Effect::Barrier(_)))
            && (!block.next.is_empty()
                || block
                    .events
                    .last()
                    .is_some_and(|event| matches!(event.effect, Effect::Return)))
    });
    let return_origins = return_origins(s, facts, &mut addresses, complete);
    Ok(Body {
        entry,
        blocks,
        stores: Vec::new(),
        instructions: BTreeMap::new(),
        return_slot_proven: false,
        address_work: addresses.work_usage(),
        forwarded_reads: Vec::new(),
        initialized_reads: BTreeMap::new(),
        return_origins,
        relations: addresses.relations,
        returned_pointer_sites,
        frame: rk_ssa::memory::Frame::new(s, Some(facts)),
    })
}

fn initialized_reads(body: &Body) -> BTreeMap<(usize, usize), Initialization> {
    use rk_ssa::initialization as init;
    let graph = init::InitializationGraph {
        entry: body.entry,
        relations: &body.relations,
        return_slot_proven: body.return_slot_proven,
        step_limit: STEP_LIMIT,
        byte_limit: BYTE_LIMIT,
        blocks: body
            .blocks
            .iter()
            .map(|block| init::Block {
                next: block.next.clone(),
                events: block
                    .events
                    .iter()
                    .map(|event| init::Event {
                        site: event.site,
                        effect: match event.effect {
                            Effect::Read(range) => init::Effect::Read(range),
                            Effect::Write(range) => init::Effect::Write(range),
                            _ => init::Effect::Barrier,
                        },
                    })
                    .collect(),
            })
            .collect(),
    };
    init::initialized_reads(&graph)
}

fn nonstack_reload(
    reload: &Value,
    cursor: Cursor,
    current: &Body,
    bodies: &BTreeMap<Addr, Body>,
) -> bool {
    let Some(field) = reload["field"].as_str().and_then(Field::from_name) else {
        return false;
    };
    let Some(target) = reload["target"].as_str().and_then(rk_core::parse_hex) else {
        return false;
    };
    let Some(summary) = bodies
        .get(&target)
        .filter(|body| body.return_slot_proven)
        .and_then(|body| body.return_origins.get(&field))
    else {
        return false;
    };
    if summary["stack_independent"] != true {
        return false;
    }
    let Some(site) = reload["site"].as_str().and_then(rk_core::parse_hex) else {
        return false;
    };
    let Some(registers) = current
        .blocks
        .iter()
        .flat_map(|block| &block.events)
        .find_map(|event| {
            if event.site != site {
                return None;
            }
            match &event.effect {
                Effect::Call {
                    target: Some(callee),
                    registers,
                    ..
                } if *callee == target => Some(registers),
                _ => None,
            }
        })
    else {
        return false;
    };
    summary["returns"].as_array().is_some_and(|returns| {
        returns.iter().all(|result| {
            result["origin"]["inputs"].as_array().is_some_and(|inputs| {
                inputs.iter().all(|input| {
                    let Some(field) = input.as_str().and_then(Field::from_name) else {
                        return false;
                    };
                    let Some(index) = Field::ALL
                        .iter()
                        .take(8)
                        .position(|&candidate| candidate == field)
                    else {
                        return true;
                    };
                    !matches!(
                        mapped_in(registers[index], cursor, current),
                        Address::Unknown
                            | Address::Known {
                                base: Base::Frame | Base::Stack(_),
                                ..
                            }
                    )
                })
            })
        })
    })
}

fn inferred_cleanup(body: &Body) -> Option<i32> {
    if !body.return_slot_proven
        || body
            .blocks
            .iter()
            .flat_map(|block| &block.events)
            .any(|event| matches!(event.effect, Effect::Barrier(_)))
    {
        return None;
    }
    let mut cleanup = None;
    for block in &body.blocks {
        if block.next.is_empty()
            && !block
                .events
                .last()
                .is_some_and(|event| matches!(event.effect, Effect::Return))
        {
            return None;
        }
        for event in &block.events {
            if !matches!(event.effect, Effect::Return) {
                continue;
            }
            let Address::Known {
                base: Base::Frame,
                offset,
                span: 0,
            } = body.relations.bounds(event.esp)
            else {
                return None;
            };
            let value = (offset as i32).checked_sub(4)?;
            if cleanup.is_some_and(|prior| prior != value) {
                return None;
            }
            cleanup = Some(value);
        }
    }
    cleanup
}

fn return_gap(body: &Body, store: &Store) -> Option<(i64, i64)> {
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
            let latest_end =
                i64::from(offset as i32) + i64::from(span) + i64::from(store.range.size);
            let earliest_end = i64::from(offset as i32) + i64::from(store.range.size);
            let top = i64::from(top as i32);
            (latest_end <= top).then_some((top - latest_end, top - earliest_end))
        }
        _ => None,
    }
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
        "completed_analyzed_calls": search.completed_calls,
    })
}

fn in_image(range: Range, image: (u32, u32)) -> bool {
    matches!(range.address, Address::Known { base: Base::Absolute, offset, span }
        if offset >= image.0 && u64::from(offset) + u64::from(span) + u64::from(range.size) <= u64::from(image.1))
}

fn frontier_category(terminal: &Value, range: Range) -> String {
    let reason = terminal["reason"].as_str().unwrap_or("unknown");
    let detail = &terminal["detail"];
    let origin = &detail["address_origin"];
    let reloads: BTreeSet<_> = origin["reloads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reload| reload["field"].as_str())
        .collect();
    let class = detail["blocker_class"].as_str().unwrap_or("unknown");
    let cause = match reason {
        "stack-read-may-alias" if reloads.contains("ESP") => {
            if origin["reloads"].as_array().is_some_and(|reloads| {
                reloads
                    .iter()
                    .any(|reload| reload["field"] == "ESP" && reload["cleanup"].is_null())
            }) {
                "esp-reload-missing-cleanup".into()
            } else {
                "esp-reload-known-cleanup".into()
            }
        }
        "stack-read-may-alias" if class == "bounded-stack-ranges-overlap" => {
            let bounded_slot = matches!(range.address, Address::Known { span, .. } if span != 0);
            let bounded_read = detail["read"]["span"]
                .as_u64()
                .is_some_and(|span| span != 0);
            let expanded_read = detail["raw_read"]["span"] == 0 && bounded_read;
            match (bounded_slot, bounded_read) {
                (true, false) => "slot-position-range",
                (false, true) if expanded_read => "mapped-read-range",
                (true, true) if expanded_read => "slot-and-mapped-read-ranges",
                (false, true) => "read-address-range",
                (true, true) => "slot-and-read-ranges",
                _ => "stack-range-overlap",
            }
            .into()
        }
        "read-origin-call-reload" => reloads.into_iter().collect::<Vec<_>>().join("+"),
        "read-origin-walk-limit" if !reloads.is_empty() => "register-reloads-seen".into(),
        "read-origin-walk-limit"
            if origin["pointer_loads"]
                .as_array()
                .is_some_and(|loads| !loads.is_empty()) =>
        {
            "pointer-loads-seen".into()
        }
        "read-origin-walk-limit" => "no-root-recovered".into(),
        "stack-read-may-alias"
        | "pointer-read-slot-not-below-esp"
        | "pointer-read-stack-position-unknown"
        | "callee-register-binding-unknown" => {
            if detail["raw_read"]["base"] == "Frame" && detail["read"]["base"] != "Frame" {
                "callee-frame-relationship-lost".into()
            } else {
                class.into()
            }
        }
        "call-depth-limit" => match (
            detail["target"].is_string(),
            detail["next_callee_has_stack_writes"] == true,
        ) {
            (false, _) => "indirect-target",
            (true, true) => "available-target-with-stack-writes",
            _ => "other-direct-target",
        }
        .into(),
        _ => return reason.into(),
    };
    format!("{reason}/{cause}")
}

fn query_counts(rows: &[&Value]) -> Value {
    let calls: BTreeSet<_> = rows.iter().filter_map(|row| row["call"].as_str()).collect();
    let slots: BTreeSet<_> = rows
        .iter()
        .map(|row| {
            (
                row["call"].as_str(),
                row["callee"].as_str(),
                row["slot"].as_str(),
            )
        })
        .collect();
    json!({"queries": rows.len(), "call_sites": calls.len(), "call_slots": slots.len()})
}

fn unresolved_progress(rows: &[&Value]) -> BTreeMap<&'static str, Value> {
    [
        "unread-through-analyzed-call",
        "partial-overwrite-every-path",
        "no-additional-progress-evidence",
    ]
    .into_iter()
    .map(|name| {
        let matching: Vec<_> = rows
            .iter()
            .copied()
            .filter(|row| {
                let evidence = &row["stack_evidence"];
                let through_call = evidence["unread_through_analyzed_call"] == true;
                let partial = evidence["partial_overwrite"]["every_path_at_least_one_byte"] == true;
                evidence["outcome"] == "unresolved"
                    && match name {
                        "unread-through-analyzed-call" => through_call,
                        "partial-overwrite-every-path" => partial,
                        _ => !through_call && !partial,
                    }
            })
            .collect();
        (name, query_counts(&matching))
    })
    .collect()
}

fn cutoff_evidence(frontier: &[Value], caller_return: bool, contradicted: bool) -> Value {
    let overwritten = !frontier.is_empty()
        && frontier
            .iter()
            .all(|terminal| terminal["reason"] == "overwritten");
    let cutoff = !frontier.is_empty()
        && frontier.iter().all(|terminal| {
            terminal["reason"] == "overwritten"
                || terminal["reason"] == "assumed-dead-after-unread-call"
                || (caller_return && terminal["reason"] == "caller-return")
        });
    json!({"outcome": if overwritten { "overwritten-before-read" } else if cutoff && !contradicted { "candidate-under-assumption" } else { "unresolved" },
        "cutoff_reached_on_every_path": cutoff, "contradicted_by_continued_evidence": contradicted})
}

fn hypothesis_counts(rows: &[&Value], scenario: Option<&str>) -> Value {
    let result = |row: &Value| {
        let evidence = &row["stack_evidence"];
        scenario.map_or(&evidence["after_one_unread_call"], |name| {
            &evidence["assumption_scenarios"][name]
        })["outcome"]
            .as_str()
            .unwrap_or("outside-scope")
            .to_owned()
    };
    let mut outcomes: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    let mut calls: BTreeMap<&str, bool> = BTreeMap::new();
    for &row in rows {
        let outcome = result(row);
        let eligible =
            outcome == "candidate-under-assumption" || outcome == "overwritten-before-read";
        calls
            .entry(row["call"].as_str().unwrap())
            .and_modify(|all| *all &= eligible)
            .or_insert(eligible);
        outcomes.entry(outcome).or_default().push(row);
    }
    json!({"call_sites_all_covered_queries_qualify": calls.values().filter(|&&all| all).count(),
        "outcomes": outcomes.into_iter().map(|(name, rows)| {
            let mut counts = query_counts(&rows);
            counts["continued_evidence_outcomes"] = json!(["overwritten-before-read", "unread-at-caller-return", "unresolved", "definite-read-before-overwrite"].into_iter().map(|outcome| {
                let matching: Vec<_> = rows.iter().copied().filter(|row| row["stack_evidence"]["outcome"] == outcome).collect();
                (outcome, query_counts(&matching))
            }).collect::<BTreeMap<_, _>>());
            (name, counts)
        }).collect::<BTreeMap<_, _>>()})
}

fn evidence_breakdown(rows: &[&Value]) -> Value {
    let mut outcomes: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    let mut scenarios = BTreeSet::new();
    let mut categories: BTreeMap<&str, Vec<(&Value, &Value)>> = BTreeMap::new();
    for &row in rows {
        let evidence = &row["stack_evidence"];
        outcomes
            .entry(evidence["outcome"].as_str().unwrap())
            .or_default()
            .push(row);
        scenarios.extend(
            evidence["assumption_scenarios"]
                .as_object()
                .into_iter()
                .flat_map(|scenarios| scenarios.keys().map(String::as_str)),
        );
        if evidence["outcome"] != "unresolved" {
            continue;
        }
        for terminal in evidence["terminals"].as_array().into_iter().flatten() {
            if let Some(category) = terminal["tracking_category"].as_str() {
                categories
                    .entry(category)
                    .or_default()
                    .push((row, terminal));
            }
        }
    }
    let frontiers: BTreeMap<_, _> = categories.into_iter().map(|(category, entries)| {
        let mut matching: Vec<_> = entries.iter().map(|&(row, _)| row).collect();
        matching.dedup_by(|a, b| std::ptr::eq(*a, *b));
        let sole = matching.iter().filter(|row| row["stack_evidence"]["terminals"].as_array().is_some_and(|terminals| {
            terminals.iter().all(|terminal| terminal["reason"] == "overwritten" || terminal["reason"] == "caller-return"
                || terminal["tracking_category"] == category)
        })).count();
        let mut instructions = BTreeSet::new();
        let mut examples = Vec::new();
        for &(row, terminal) in &entries {
            if instructions.insert((terminal["function"].as_str(), terminal["site"].as_str())) && examples.len() < 3 {
                examples.push(json!({"caller": row["caller"], "call": row["call"], "return": row["return"],
                    "function": terminal["function"], "site": terminal["site"], "instruction": terminal["instruction"],
                    "detail": terminal["detail"]}));
            }
        }
        let positions: BTreeMap<_, _> = [("below-esp", Value::Bool(true)), ("not-wholly-below-esp", Value::Bool(false)), ("unknown", Value::Null)]
            .into_iter().map(|(name, position)| {
                let mut rows: Vec<_> = entries.iter().filter(|(_, terminal)| terminal["detail"]["whole_slot_below_esp"] == position)
                    .map(|&(row, _)| row).collect();
                rows.dedup_by(|a, b| std::ptr::eq(*a, *b));
                (name, query_counts(&rows))
            }).collect();
        let mut counts = query_counts(&matching);
        counts["terminal_paths"] = json!(entries.iter().map(|(_, terminal)| terminal["equivalent_paths"].as_u64().unwrap_or(1)).sum::<u64>());
        counts["terminal_states"] = json!(entries.len());
        counts["blocking_instructions"] = json!(instructions.len());
        counts["sole_frontier_queries"] = json!(sole);
        counts["stack_positions"] = json!(positions);
        counts["unresolved_progress"] = json!(unresolved_progress(&matching));
        counts["examples"] = json!(examples);
        (category, counts)
    }).collect();
    json!({"outcomes": outcomes.into_iter().map(|(name, rows)| (name, query_counts(&rows))).collect::<BTreeMap<_, _>>(),
        "after_one_unread_call": hypothesis_counts(rows, None),
        "assumption_scenarios": scenarios.into_iter().map(|name| (name, hypothesis_counts(rows, Some(name)))).collect::<BTreeMap<_, _>>(),
        "unresolved_progress": unresolved_progress(rows),
        "frontiers": frontiers})
}

fn investigate(
    bodies: &BTreeMap<Addr, Body>,
    start: Cursor,
    range: Range,
    image_guard: Option<(u32, u32)>,
    private_evidence: bool,
    stop_after_unread_calls: usize,
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
        completed_calls: 0,
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
            search.completed_calls.min(stop_after_unread_calls),
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
                if private_evidence && !cursor.original {
                    if let Some(initialization) =
                        current.initialized_reads.get(&(cursor.block, cursor.index))
                    {
                        search.trace.push(json!({"proof": "callee-read-initialized", "function": format!("{:08x}", cursor.function),
                            "site": format!("{:08x}", event.site), "read": describe(raw), "entry_return_address_written_by_call": initialization.entry_return_address,
                            "writes": initialization.writes.iter().map(|site| format!("{site:08x}")).collect::<Vec<_>>()}));
                        pending.push(search);
                        continue;
                    }
                }
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
                    let definite = (0..=span).all(|start| {
                        (start..start + range.size).any(|index| {
                            search.remaining.contains(&index)
                                && relations.offset_of(read, byte(index)).is_some()
                        })
                    });
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
                    let resolved_reloads: Vec<_> = event.address_origin["reloads"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|reload| {
                            private_evidence && nonstack_reload(reload, cursor, current, bodies)
                        })
                        .cloned()
                        .collect();
                    let register_reloaded =
                        event.address_origin["reloads"]
                            .as_array()
                            .is_none_or(|reloads| {
                                reloads
                                    .iter()
                                    .any(|reload| !resolved_reloads.contains(reload))
                            });
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
                        && matches!(
                            read.address,
                            Address::Known {
                                base: Base::Absolute,
                                ..
                            }
                        )
                    {
                        search.trace.push(json!({"assumption": "private-stack-excludes-fixed-address-read", "function": format!("{:08x}", cursor.function), "site": format!("{:08x}", event.site), "read": describe(read)}));
                        pending.push(search);
                        continue;
                    }
                    if private_evidence
                        && !definite
                        && !stack_derived
                        && !unknown_origin
                        && matches!(
                            read.address,
                            Address::Known {
                                base: Base::Parameter(_) | Base::Argument(_),
                                ..
                            }
                        )
                    {
                        search.trace.push(json!({"assumption": "private-stack-excludes-incoming-pointer-read", "function": format!("{:08x}", cursor.function), "site": format!("{:08x}", event.site), "read": describe(read)}));
                        pending.push(search);
                        continue;
                    }
                    if private_evidence
                        && !definite
                        && !stack_derived
                        && !unknown_origin
                        && stack_position(mapped_in(event.esp, cursor, current)) == Some(true)
                    {
                        search.trace.push(json!({"assumption": "unrelated-pointer-does-not-read-returned-private-frame-below-esp", "function": format!("{:08x}", cursor.function), "site": format!("{:08x}", event.site), "read": describe(read), "resolved_return_origins": resolved_reloads}));
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
                               "resolved_return_origins": resolved_reloads,
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
                    search.completed_calls += 1;
                    if private_evidence {
                        let call = bodies[&resume.function].blocks[resume.block]
                            .events
                            .get(resume.index - 1)
                            .map(|event| format!("{:08x}", event.site));
                        search.trace.push(json!({"completed_call": call, "caller": format!("{:08x}", resume.function), "callee": format!("{:08x}", cursor.function), "return": format!("{:08x}", event.site), "remaining_bytes": search.remaining}));
                        if stop_after_unread_calls != 0
                            && search.completed_calls >= stop_after_unread_calls
                        {
                            terminals.push(terminal("assumed-dead-after-unread-call", &search, event.site,
                                json!({"assumption": "dead-after-additional-unread-calls", "minimum_completed_calls": stop_after_unread_calls, "proved_overwritten": false})));
                            continue;
                        }
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
    if !private_evidence {
        return terminals;
    }
    let mut grouped = BTreeMap::<String, Value>::new();
    for mut terminal in terminals {
        let key = json!({"reason": terminal["reason"], "function": terminal["function"], "site": terminal["site"],
            "remaining_bytes": terminal["remaining_bytes"], "call_depth": terminal["call_depth"],
            "detail": terminal["detail"], "completed_analyzed_calls": terminal["completed_analyzed_calls"]}).to_string();
        if let Some(prior) = grouped.get_mut(&key) {
            prior["equivalent_paths"] = json!(prior["equivalent_paths"].as_u64().unwrap() + 1);
        } else {
            terminal["equivalent_paths"] = json!(1);
            grouped.insert(key, terminal);
        }
    }
    grouped.into_values().collect()
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
    let summary_started = Instant::now();
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
                .map(|body| (addr, body, ir))
                .map_err(|error| (addr, error.to_string()))
        })
        .collect();
    let mut bodies = BTreeMap::new();
    let mut irs = BTreeMap::new();
    let mut unavailable = Vec::new();
    for result in results {
        match result {
            Ok((addr, body, ir)) => {
                bodies.insert(addr, body);
                irs.insert(addr, ir);
            }
            Err((addr, reason)) => {
                unavailable.push(json!({"function": format!("{addr:08x}"), "reason": reason}))
            }
        }
    }
    let recovered_cleanup: BTreeMap<_, _> = bodies
        .iter()
        .filter_map(|(&addr, body)| inferred_cleanup(body).map(|cleanup| (addr, cleanup)))
        .collect();
    let mut cleanup_fallback_sites = BTreeSet::new();
    for (&caller, body) in &bodies {
        for event in body.blocks.iter().flat_map(|block| &block.events) {
            if let Effect::Call {
                target: Some(target),
                ..
            } = event.effect
            {
                if recovered_cleanup.contains_key(&target)
                    && ctx
                        .facts
                        .summary(event.site, Some(target))
                        .cleanup
                        .is_none()
                {
                    cleanup_fallback_sites.insert((caller, event.site, target));
                }
            }
        }
    }
    let cleanup_fallback_callers: BTreeSet<_> = cleanup_fallback_sites
        .iter()
        .map(|&(caller, _, _)| caller)
        .collect();
    let facts = ReportFacts {
        base: &ctx.facts,
        cleanup: &recovered_cleanup,
    };
    let refreshed: Vec<_> = bodies
        .into_par_iter()
        .map(|(addr, mut prior)| {
            if !cleanup_fallback_callers.contains(&addr) {
                return (addr, prior, false);
            }
            let Some(ir) = irs.get(&addr) else {
                return (addr, prior, false);
            };
            let attempt = (|| -> rk_core::Result<Option<anyhow::Result<Body>>> {
                let cg = rk_ir::lower_with(
                    ir,
                    &ctx.program.image,
                    &rk_x87::Recipes,
                    ctx.reloc.is_some(),
                )?;
                let mut view = None;
                let mut options = ctx.plan_options(addr);
                options.call_facts = &facts;
                rk_plan::plan_function_with_view(
                    &cg,
                    &options,
                    Some(&mut |ssa| view = Some(capture(ssa, &facts))),
                )?;
                Ok(view)
            })();
            if let Ok(Some(Ok(mut fresh))) = attempt {
                fresh.stores = std::mem::take(&mut prior.stores)
                    .into_iter()
                    .map(|mut store| {
                        store.returned_pointer = fresh.returned_pointer_sites.contains(&store.site);
                        store.private_frame = fresh.frame.unexposed(store.range);
                        store.return_position = return_position(&fresh, &store);
                        store
                    })
                    .collect();
                fresh.return_slot_proven = prior.return_slot_proven;
                fresh.instructions = prior.instructions;
                (addr, fresh, true)
            } else {
                (addr, prior, false)
            }
        })
        .collect();
    let refreshed_count = refreshed
        .iter()
        .filter(|(_, _, refreshed)| *refreshed)
        .count();
    let bodies: BTreeMap<_, _> = refreshed
        .into_iter()
        .map(|(addr, mut body, _)| {
            body.initialized_reads = initialized_reads(&body);
            (addr, body)
        })
        .collect();
    let summary_seconds = summary_started.elapsed().as_secs_f64();
    let search_started = Instant::now();
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
                        investigate(&bodies, cursor, range, None, false, 0)
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
                        let terminals = investigate(&bodies, cursor, range, Some(image), false, 0);
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
                    let mut evidence = if store.return_position != "below-esp" {
                        json!({"outcome": "outside-scope", "reason": store.return_position})
                    } else if !store.private_frame {
                        json!({"outcome": "unresolved", "terminals": [{"reason": "private-frame-not-proven"}]})
                    } else if store.returned_pointer {
                        json!({"outcome": "unresolved", "terminals": [{"reason": "returned-frame-pointer"}]})
                    } else {
                        let mut frontier = investigate(&bodies, cursor, range, None, true, 0);
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
                        let progress = |terminal: &Value| -> Option<(usize, usize)> {
                            let Address::Known { span, .. } = range.address else {
                                return None;
                            };
                            let width = span.checked_add(range.size)?;
                            if width > BYTE_LIMIT || range.size == 0 {
                                return None;
                            }
                            let remaining = terminal["remaining_bytes"].as_array()?;
                            let mut alive = BTreeSet::new();
                            for byte in remaining {
                                let byte = u32::try_from(byte.as_u64()?).ok()?;
                                if byte >= width || !alive.insert(byte) {
                                    return None;
                                }
                            }
                            let minimum = (0..=span)
                                .map(|start| {
                                    (start..start + range.size)
                                        .filter(|byte| !alive.contains(byte))
                                        .count()
                                })
                                .min()?;
                            Some((minimum, alive.len()))
                        };
                        let progresses: Vec<_> = frontier.iter().map(progress).collect();
                        let no_definite_read = outcome != "definite-read-before-overwrite";
                        let some_partial = no_definite_read
                            && progresses
                                .iter()
                                .flatten()
                                .any(|&(written, remaining)| written > 0 && remaining > 0);
                        let all_touched = some_partial
                            && frontier
                                .iter()
                                .zip(&progresses)
                                .all(|(terminal, progress)| {
                                    terminal["reason"] != "step-budget"
                                        && progress.is_some_and(|(written, _)| written > 0)
                                });
                        let min_all_paths = all_touched
                            .then(|| {
                                progresses
                                    .iter()
                                    .flatten()
                                    .map(|&(written, _)| written)
                                    .min()
                            })
                            .flatten();
                        for (terminal, progress) in frontier.iter_mut().zip(&progresses) {
                            if let Some((written, remaining)) = progress {
                                terminal["guaranteed_overwritten_bytes"] = json!(written);
                                terminal["possible_remaining_bytes"] = json!(remaining);
                            }
                        }
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
                        json!({"outcome": outcome, "unread_through_analyzed_call": unread_through_call,
                            "partial_overwrite": {"some_paths": some_partial, "every_path_at_least_one_byte": all_touched, "minimum_bytes_every_path": min_all_paths}, "terminals": frontier})
                    };
                    let eligible = store.return_position == "below-esp"
                        && store.private_frame
                        && !store.returned_pointer;
                    let contradicted = evidence["outcome"] == "definite-read-before-overwrite";
                    let mut scenarios = BTreeMap::new();
                    for (name, calls) in [
                        ("after-one-unread-call", 1),
                        ("after-two-unread-calls", 2),
                        ("after-three-unread-calls", 3),
                    ] {
                        let mut result = if eligible {
                            let frontier = investigate(&bodies, cursor, range, None, true, calls);
                            let mut result = cutoff_evidence(&frontier, false, contradicted);
                            result["terminals"] = json!(frontier);
                            result
                        } else {
                            json!({"outcome": "outside-scope"})
                        };
                        if calls == 1 {
                            let combined = if let Some(frontier) = result["terminals"].as_array() {
                                cutoff_evidence(frontier, true, contradicted)
                            } else {
                                json!({"outcome": "outside-scope"})
                            };
                            scenarios.insert("after-one-unread-call-or-caller-return", combined);
                            evidence["after_one_unread_call"] = result.clone();
                            result.as_object_mut().unwrap().remove("terminals");
                            result["witness_source"] =
                                json!("stack_evidence.after_one_unread_call.terminals");
                        }
                        scenarios.insert(name, result);
                    }
                    let mut caller_return = if eligible {
                        cutoff_evidence(
                            evidence["terminals"].as_array().unwrap(),
                            true,
                            contradicted,
                        )
                    } else {
                        json!({"outcome": "outside-scope"})
                    };
                    caller_return["witness_source"] = json!("stack_evidence.terminals");
                    scenarios.insert("at-caller-return", caller_return);
                    scenarios
                        .get_mut("after-one-unread-call-or-caller-return")
                        .unwrap()["witness_source"] =
                        json!("stack_evidence.after_one_unread_call.terminals");
                    evidence["assumption_scenarios"] = json!(scenarios);
                    if evidence["outcome"] == "unresolved" {
                        if let Some(terminals) = evidence["terminals"].as_array_mut() {
                            for terminal in terminals {
                                if terminal["reason"] != "overwritten"
                                    && terminal["reason"] != "caller-return"
                                {
                                    terminal["tracking_category"] =
                                        json!(frontier_category(terminal, range));
                                }
                            }
                        }
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
                        "production_call_cleanup": ctx.facts.summary(event.site, Some(target)).cleanup,
                        "report_call_cleanup": facts.summary(event.site, Some(target)).cleanup,
                        "gap_below_restored_esp": return_gap(callee, store).map(|(min, max)| json!({"minimum_bytes": min, "maximum_bytes": max})),
                        "outcome": outcome, "terminals": terminals, "slot_outside_image": guarded, "private_frame": store.private_frame, "return_position": store.return_position, "stack_evidence": evidence}));
                }
            }
        }
    }
    let search_seconds = search_started.elapsed().as_secs_f64();
    let reporting_started = Instant::now();
    let publications: usize = bodies.values().map(|body| body.stores.len()).sum();
    let queried_stores = &queried_stores;
    let inventory: Vec<_> = bodies
        .iter()
        .flat_map(|(&function, body)| {
            body.stores.iter().map(move |store| {
        json!({"function": format!("{function:08x}"), "return": format!("{:08x}", store.site),
            "slot": format!("{:?}", store.key), "range": describe(store.range),
            "returned_frame_pointer": store.returned_pointer, "private_frame": store.private_frame, "return_position": store.return_position, "gap_below_restored_esp": return_gap(body, store).map(|(min, max)| json!({"minimum_bytes": min, "maximum_bytes": max})),
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
    let partial_summary: BTreeMap<_, _> = ["some_paths", "every_path_at_least_one_byte"].into_iter().map(|category| {
        let matching: Vec<_> = rows.iter().filter(|row| row["stack_evidence"]["partial_overwrite"][category] == true).collect();
        let calls: BTreeSet<_> = matching.iter().filter_map(|row| row["call"].as_str()).collect();
        let slots: BTreeSet<_> = matching.iter().map(|row| (row["call"].as_str(), row["callee"].as_str(), row["slot"].as_str())).collect();
        let outcomes: BTreeMap<_, _> = ["overwritten-before-read", "unread-at-caller-return", "unresolved"].into_iter().map(|outcome| {
            (outcome, matching.iter().filter(|row| row["stack_evidence"]["outcome"] == outcome).count())
        }).collect();
        (category, json!({"queries": matching.len(), "call_sites": calls.len(), "call_slots": slots.len(), "outcomes": outcomes}))
    }).collect();
    let forwarded: BTreeMap<_, _> = bodies
        .iter()
        .filter(|(_, body)| !body.forwarded_reads.is_empty())
        .map(|(&function, body)| (format!("{function:08x}"), &body.forwarded_reads))
        .collect();
    let mut callee_rows: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for row in &rows {
        callee_rows
            .entry(row["callee"].as_str().unwrap())
            .or_default()
            .push(row);
    }
    let breakdown = evidence_breakdown(&rows.iter().collect::<Vec<_>>());
    let by_callee: BTreeMap<_, _> = callee_rows
        .into_iter()
        .map(|(callee, rows)| (callee, evidence_breakdown(&rows)))
        .collect();
    let mut used_origins = BTreeSet::new();
    for row in &rows {
        for terminal in row["stack_evidence"]["terminals"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                row["stack_evidence"]["after_one_unread_call"]["terminals"]
                    .as_array()
                    .into_iter()
                    .flatten(),
            )
            .chain(
                row["stack_evidence"]["assumption_scenarios"]
                    .as_object()
                    .into_iter()
                    .flat_map(|scenarios| scenarios.values())
                    .flat_map(|scenario| scenario["terminals"].as_array().into_iter().flatten()),
            )
        {
            let reloads = terminal["detail"]["address_origin"]["reloads"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(
                    terminal["trace"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|step| {
                            step["resolved_return_origins"]
                                .as_array()
                                .into_iter()
                                .flatten()
                        }),
                );
            for reload in reloads {
                if let Some(key) = reload["target"]
                    .as_str()
                    .and_then(rk_core::parse_hex)
                    .zip(reload["field"].as_str().and_then(Field::from_name))
                {
                    used_origins.insert(key);
                }
            }
        }
    }
    let mut origins: BTreeMap<String, BTreeMap<&str, &Value>> = BTreeMap::new();
    for (target, field) in used_origins {
        if let Some(summary) = bodies
            .get(&target)
            .and_then(|body| body.return_origins.get(&field))
        {
            origins
                .entry(format!("{target:08x}"))
                .or_default()
                .insert(field.name(), summary);
        }
    }
    let function_plans: BTreeMap<_, _> = bodies.iter().filter(|(_, body)| !body.stores.is_empty()).map(|(&addr, body)| {
        let function = format!("{addr:08x}");
        let queries: Vec<_> = rows.iter().filter(|row| row["callee"] == function).collect();
        let overwritten = queries.iter().filter(|row| row["stack_evidence"]["outcome"] == "overwritten-before-read").count();
        let counterexamples: Vec<_> = queries.iter().filter(|row| row["stack_evidence"]["outcome"] == "definite-read-before-overwrite")
            .map(|row| json!({"caller": row["caller"], "call": row["call"], "return": row["return"], "slot": row["slot"]})).collect();
        let supported = queries.iter().filter(|row| matches!(row["stack_evidence"]["after_one_unread_call"]["outcome"].as_str(), Some("overwritten-before-read" | "candidate-under-assumption"))).count();
        let unresolved_support: BTreeMap<_, _> = [("supports-cutoff", true), ("does-not-support-cutoff", false)].into_iter().map(|(name, supports)| {
            let matching: Vec<_> = queries.iter().copied().filter(|row| row["stack_evidence"]["outcome"] == "unresolved"
                && (row["stack_evidence"]["after_one_unread_call"]["outcome"] == "candidate-under-assumption") == supports).collect();
            let mut frontiers = BTreeMap::<&str, usize>::new();
            for row in &matching {
                let categories: BTreeSet<_> = row["stack_evidence"]["terminals"].as_array().into_iter().flatten()
                    .filter_map(|terminal| terminal["tracking_category"].as_str()).collect();
                for category in categories { *frontiers.entry(category).or_default() += 1; }
            }
            (name, json!({"queries": matching.len(), "frontier_queries": frontiers, "progress": unresolved_progress(&matching)}))
        }).collect();
        let eligible = body.stores.iter().filter(|store| body.return_slot_proven && store.private_frame && !store.returned_pointer && store.return_position == "below-esp").count();
        let unqueried = body.stores.iter().filter(|store| !queried_stores.contains(&(addr, store.site, store.key))).count();
        let mut calls: BTreeMap<(&str, &str), Vec<&Value>> = BTreeMap::new();
        for row in &queries { calls.entry((row["caller"].as_str().unwrap(), row["call"].as_str().unwrap())).or_default().push(row); }
        let complete_calls: Vec<_> = calls.values().filter(|rows| rows.len() == body.stores.len() && rows.iter().all(|row| row["stack_evidence"]["outcome"] == "overwritten-before-read"))
            .map(|rows| json!({"caller": rows[0]["caller"], "call": rows[0]["call"]})).collect();
        (function.clone(), json!({"return_publications": body.stores.len(), "eligible_publications": eligible,
            "queries": queries.len(), "queried_call_sites": calls.len(), "unqueried_publications": unqueried,
            "overwrite_queries": overwritten, "supporting_queries": supported, "counterexample_queries": counterexamples.len(), "counterexamples": counterexamples,
            "complete_overwrite_call_sites": complete_calls, "function_wide_proven": false,
            "all_queried_publications_proven": !queries.is_empty() && overwritten == queries.len() && unqueried == 0,
            "heuristic_evidence": by_callee.get(function.as_str()), "unresolved_support": unresolved_support,
            "support_rule": "complete overwrite or dead after one additional completed unread call, every path; definite continued reads reject",
            "support_fraction": (!queries.is_empty()).then(|| supported as f64 / queries.len() as f64),
            "nomination_requires": ["normal-c-lifetime", "explicit-function-acceptance", "private-stack", "observer-fallback"],
            "unqueried_callers": "open: indirect, unavailable, external and tail callers are not closed by this report"}))
    }).collect();
    let mut report = json!({"format": "recomp-stack-return-report-v1", "read_only": true,
        "inputs": ctx.return_stack_inputs, "function_plans": function_plans,
        "proof_coverage": "Complete overwrite is a proof only for the listed call/return/slot queries under the recorded stack assumptions. No percentage or minimum sample size applies. Unresolved or unqueried queries block extending that proof. Function-wide consumption generalizes to unqueried callers under an explicitly accepted normal-C lifetime assumption; this report does not close the caller set. Support counts nominate, never prove; any definite read rejects nomination. No threshold is selected.",
        "executable_sha256": ctx.program.sha256, "image_base": format!("{:08x}", ctx.program.image.base), "image_size": ctx.program.image.size,
        "scope": "Normal continuations of ordinary planned return publications; pre-promotion SSA uses per-function production assumptions and retains every original store. Production forwarding removes reads proven to consume prior writes. Stack evidence uses private-frame exposure and general address relations. Candidates require runtime observer guards and return/entry contracts before optimization. Indirect and external callers prevent global elimination from these counts alone.",
        "limits": {"steps_per_query": STEP_LIMIT, "nested_calls": CALL_LIMIT, "caller_unwinds": 0, "bytes_per_query": BYTE_LIMIT},
        "assumptions": {"private_stack": ctx.assume_private_stack, "stack_pointer_bases": ctx.assume_stack_pointer_bases,
            "dead_after_stack_call": false, "dead_after_caller_returns": false},
        "coverage": {"functions": bodies.len(), "return_publications": publications, "sites": site_coverage,
            "indirect_call_sites_not_queried": indirect_calls, "unavailable_functions": unavailable},
        "candidate_call_sites": candidate_calls.len(), "candidate_call_slots": candidate_slots.len(),
        "additional_candidates_with_slot_outside_image_guard": {"queries": guarded_queries, "call_sites": guarded_calls.len(), "call_slots": guarded_slots.len()},
        "report_cleanup_recovery": {"callee_proofs": recovered_cleanup.len(), "fallback_call_sites": cleanup_fallback_sites.len(), "fallback_targets": cleanup_fallback_sites.iter().map(|&(_, _, target)| target).collect::<BTreeSet<_>>().len(), "affected_callers": cleanup_fallback_callers.len(), "refreshed_functions": refreshed_count,
            "cleanup": recovered_cleanup.iter().map(|(addr, cleanup)| (format!("{addr:08x}"), *cleanup)).collect::<BTreeMap<_, _>>(),
            "scope": "Report-only fallback when production call cleanup is unavailable. Each callee has a proven return slot, no opaque/external exits, and a single exact entry-ESP-relative restored ESP on every return. Existing per-site cleanup takes precedence; native replacements are excluded."},
        "stack_evidence_counts": evidence_outcomes,
        "report_local_initialization": {"functions": bodies.values().filter(|body| !body.initialized_reads.is_empty()).count(),
            "proved_reads": bodies.values().map(|body| body.initialized_reads.len()).sum::<usize>(),
            "scope": "Read-specific byte coverage before mapping callee addresses. Every predecessor path must write every read byte in the same invocation. Calls, opaque effects, entry without full coverage, cycles and budgets block a proof. A proved return-slot contract supplies the four entry return-address bytes written by CALL. Applied only inside subsequent analyzed callees, never to the original caller prefix. Stored values need not be recoverable; x87 stores count. Writes and local read coordinates remain correlated even when their mapped bounds overlap old slots. Skipping an initialized read does not prove the entire old slot overwritten. Trace witnesses list covering writes."},
        "report_return_origins": {"functions": origins,
            "scope": "Report-only return-register provenance under private-stack and stack-pointer-bases. Every recorded return must avoid direct stack-address construction, unresolved register reloads and local/unresolved pointer loads. Incoming argument values and ordinary memory loads retain private-stack unrelated-pointer semantics. Direct incoming register dependencies are checked against the producing call's bindings; known stack or unknown bindings block exclusion. This is pointer-origin evidence under the existing assumptions, not a numeric-address disjointness proof. Native/unavailable bodies and unresolved origins stay conservative. Witnesses cover summaries referenced by retained query frontiers and representative traces."},
        "after_one_unread_call_hypothesis": {"name": "dead-after-one-additional-unread-call", "enabled_for_emission": false,
            "scope": "DIVERGENCE(original): Assume an initially unexposed, unreturned slot wholly below restored ESP dead once one subsequent analyzed call returns without reading any surviving old bytes. Callee reads proved initialized by same-invocation writes cannot consume old bytes. Every path must fully overwrite the slot or complete that additional call unread; syntactic stack writes, a caller return alone, opaque calls and analysis cutoffs do not qualify. Evaluated in a separate search stopping at the first qualifying return; ordinary stack evidence continues afterward. Any definite read found by continued evidence rejects the candidate. Candidate-under-assumption is not proof of overwrite or permanent death. Per-query witnesses and global/per-callee counts live under after_one_unread_call."},
        "stack_lifetime_assumption_scenarios": {"enabled_for_emission": false,
            "scenarios": {"after-one-unread-call": {"completed_calls": 1}, "after-two-unread-calls": {"completed_calls": 2},
                "after-three-unread-calls": {"completed_calls": 3}, "at-caller-return": {"caller_returns": 1},
                "after-one-unread-call-or-caller-return": {"completed_calls": 1, "caller_returns": 1, "combine": "either cutoff on each path"}},
            "scope": "DIVERGENCE(original): Independent, overlapping hypothetical death cutoffs; a query or call site can qualify under several scenarios. One/two/three count completed sequential analyzed calls after the origin returns, not nested depth or entered calls. Every path must fully overwrite surviving bytes or reach that scenario's cutoff unread. The combined scenario permits different cutoffs on different paths. Caller-return scenarios reuse the continued evidence search; one-call/combined share witnesses; two/three-call scenarios search independently with the same nesting and step limits. Continued evidence remains separate and any definite read rejects candidates in every scenario. Counts distinguish proof from assumption and show continued outcomes, including additional unresolved candidates. call_sites_all_covered_queries_qualify requires every queried return/slot at that site to be proven overwritten or qualify under that assumption; ordinary call_sites counts need only one matching query. No claim about unqueried returns, indirect callers or permanent death is added."},
        "partial_overwrite_evidence": {"counts": partial_summary, "scope": "Some paths proves at least one actual slot byte overwritten while bytes might survive; every path requires at least one actual slot byte overwritten on all paths, allowing complete overwrites. Aligned slots use the minimum overlap over every possible slot position. Queries with a definite read before complete overwrite are excluded. Possible reads and later effects remain unresolved. Counts overlap other categories."},
        "unread_through_analyzed_call": {"queries": through_call.len(), "call_sites": through_call_sites.len(), "call_slots": through_call_slots.len(), "later_outcomes": through_call_outcomes, "scope": "Every explored path overwrites all tracked bytes or completes a subsequent analyzed call with bytes surviving and no read of surviving old values; at least one path completes such a call. Later reads or unresolved behavior remain possible. Query counts overlap the main outcome categories."},
        "stack_evidence_unresolved_frontiers": evidence_frontiers,
        "stack_evidence_breakdown": breakdown,
        "stack_evidence_by_callee": by_callee,
        "stack_evidence_breakdown_scope": "Frontier categories refine terminal reasons without changing the search or outcomes. Query/call counts overlap across categories and stack positions; call-site counts mean at least one query, not every return/slot. Sole-frontier queries may also overwrite or return unread. Examples use distinct blocking instructions. Missing stack positions are unknown. Unresolved progress signals overlap: completing an analyzed call unread or overwriting some actual bytes on every path is bounded evidence only; surviving bytes and later reads remain unresolved. No-additional-progress-evidence is absence of those signals, not proof of a live slot. Register reloads, lost frame relationships, loops, opaque operations and search limits alone provide no additional evidence of death. A bounded slot/read overlap is a possible read, not a definite read. Syntactic stack writes in a depth-limited target are not proof of overwrite. Ordinary evidence retains the report assumptions. The separate after_one_unread_call result explicitly evaluates an additional death assumption; no emission changes.",
        "stack_evidence_path_counting": "Equivalent stack-evidence endpoints are grouped by reason, function, site, call depth, remaining bytes, detail and the completed analyzed-call count. Each retains one representative trace and an equivalent_paths count. Category terminal_paths sums those counts; terminal_states counts retained representatives. Query outcomes and all-path progress use all endpoint groups. Ordinary overwrite analysis is not grouped.",
        "stack_evidence_frontier_counting": "Each unresolved query is counted once per terminal reason; queries can have multiple reasons; overwritten and caller-return paths are not counted as unresolved frontiers.",
        "stack_evidence_scope": "Only publications proved wholly below restored ESP. Production private-stack excludes unrelated pointer reads of unexposed returned slots while below current ESP. Definite reads take precedence over unresolved paths. Unread-at-caller-return requires all paths to overwrite or reach the first caller return unread; it does not prove the bytes dead afterward. No emission changes.",
        "assumption_only_upper_bounds": assumption_bounds,
        "assumption_scope": "Stack-call bounds use syntactic stack writes, not proof that every path writes. First-caller-return is more aggressive than a two- or three-caller cutoff; those are not measured.",
        "blocker_classes": blockers,
        "initialized_reads_forwarded": forwarded,
        "blocker_counting": "Queries can contain several classes. Sole-blocker counts describe current terminal frontiers, not proofs that bypassing one class would finish a query.",
        "publications": inventory, "counts": counts, "rows": rows});
    report["timing_seconds"] = json!({"summary_refresh_and_initialization": summary_seconds,
        "continuation_searches_and_witnesses": search_seconds, "report_aggregation": reporting_started.elapsed().as_secs_f64(),
        "scope": "Initial SSA capture is included in planning, measured separately; searches include witness JSON construction. Serialization and disk write are logged separately."});
    let serialization_started = Instant::now();
    let nominations = rk_core::facts::StackLifetimeNominations {
        format: rk_core::facts::STACK_LIFETIME_FORMAT.into(),
        inputs: report["inputs"].clone(),
        coverage: report["proof_coverage"].as_str().unwrap().into(),
        functions: serde_json::from_value(report["function_plans"].clone())?,
    };
    std::fs::write(
        path.with_extension("facts.json"),
        serde_json::to_string_pretty(&nominations)? + "\n",
    )?;
    std::fs::write(path, serde_json::to_string_pretty(&report)? + "\n")?;
    eprintln!("stack-return phases: summary={summary_seconds:.3}s searches={search_seconds:.3}s aggregation={}s serialization/write={:.3}s", report["timing_seconds"]["report_aggregation"], serialization_started.elapsed().as_secs_f64());
    eprintln!("stack-return report: {} functions, {publications} publications, {} call/publication queries; stack evidence {:?}", bodies.len(), report["rows"].as_array().unwrap().len(), report["stack_evidence_counts"]);
    Ok(())
}
