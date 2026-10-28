use crate::address::Relations;
use rk_core::{
    memory::{Address, Base, Range},
    Addr,
};
use std::collections::{BTreeMap, BTreeSet};

pub struct Initialization {
    pub writes: Vec<Addr>,
    pub entry_return_address: bool,
}

pub enum Effect {
    Read(Range),
    Write(Range),
    Barrier,
}
pub struct Event {
    pub site: Addr,
    pub effect: Effect,
}
pub struct Block {
    pub events: Vec<Event>,
    pub next: Vec<usize>,
}
pub struct InitializationGraph<'a> {
    pub entry: usize,
    pub blocks: Vec<Block>,
    pub relations: &'a Relations,
    pub return_slot_proven: bool,
    pub step_limit: usize,
    pub byte_limit: u32,
}

pub fn initialized_reads(body: &InitializationGraph) -> BTreeMap<(usize, usize), Initialization> {
    let mut predecessors = vec![Vec::new(); body.blocks.len()];
    for (index, block) in body.blocks.iter().enumerate() {
        for &next in &block.next {
            predecessors[next].push(index);
        }
    }
    let prove = |block: usize, index: usize, read: Range| -> Option<Initialization> {
        if !matches!(
            read.address,
            Address::Known {
                base: Base::Frame | Base::Stack(_),
                span: 0,
                ..
            }
        ) || read.size == 0
            || read.size > body.byte_limit
        {
            return None;
        }
        let mut pending = vec![(block, index, (0..read.size).collect::<BTreeSet<_>>(), false)];
        let mut seen = BTreeSet::new();
        let mut active = BTreeSet::new();
        let mut writes = BTreeSet::new();
        let mut entry_return_address = false;
        while let Some((block, index, mut remaining, finished)) = pending.pop() {
            let key = (block, index, remaining.clone());
            if finished {
                active.remove(&key);
                seen.insert(key);
                continue;
            }
            if seen.contains(&key) {
                continue;
            }
            if seen.len() + active.len() >= body.step_limit || !active.insert(key) {
                return None;
            }
            pending.push((block, index, remaining.clone(), true));
            if index == 0 {
                if block == body.entry {
                    if body.return_slot_proven {
                        let slot = Range::new(Address::exact(Base::Frame, 0), 4);
                        let before = remaining.len();
                        remaining.retain(|&byte| {
                            body.relations
                                .offset_of(slot, Range::new(read.address.offset(byte), 1))
                                .is_none()
                        });
                        entry_return_address |= remaining.len() != before;
                        if remaining.is_empty() {
                            continue;
                        }
                    }
                    return None;
                }
                if predecessors[block].is_empty() {
                    return None;
                }
                pending.extend(predecessors[block].iter().map(|&pred| {
                    (
                        pred,
                        body.blocks[pred].events.len(),
                        remaining.clone(),
                        false,
                    )
                }));
                continue;
            }
            let event = &body.blocks[block].events[index - 1];
            match event.effect {
                Effect::Write(write) => {
                    let before = remaining.len();
                    remaining.retain(|&byte| {
                        body.relations
                            .offset_of(write, Range::new(read.address.offset(byte), 1))
                            .is_none()
                    });
                    if remaining.len() != before {
                        writes.insert(event.site);
                    }
                }
                Effect::Barrier => return None,
                Effect::Read(_) => {}
            }
            if !remaining.is_empty() {
                pending.push((block, index - 1, remaining, false));
            }
        }
        Some(Initialization {
            writes: writes.into_iter().collect(),
            entry_return_address,
        })
    };
    body.blocks
        .iter()
        .enumerate()
        .flat_map(|(block, body)| {
            body.events
                .iter()
                .enumerate()
                .filter_map(move |(index, event)| {
                    let Effect::Read(read) = event.effect else {
                        return None;
                    };
                    Some(((block, index), read))
                })
        })
        .filter_map(|(location, read)| {
            prove(location.0, location.1, read).map(|writes| (location, writes))
        })
        .collect()
}
