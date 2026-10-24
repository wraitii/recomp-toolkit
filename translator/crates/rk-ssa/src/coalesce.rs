use std::collections::BTreeMap;

use rk_core::contract::Field;
use rk_core::Lookup;
use rk_ir::{lanes, xmm_offset, Op, StateKey};

use crate::{canonical::rewrite_aliases, LaneGroup, LaneGroupLane, Ssa, ValueId};

fn channels(s: &Ssa) -> Vec<Vec<usize>> {
    let key_index: BTreeMap<StateKey, usize> = s
        .keys
        .iter()
        .enumerate()
        .map(|(index, key)| (*key, index))
        .collect();
    let mut channels = Vec::new();
    for field in Field::ALL {
        let keys: Vec<usize> = lanes(field)
            .iter()
            .filter_map(|key| key_index.get(key).copied())
            .collect();
        if keys.len() >= 2 {
            channels.push(keys);
        }
    }
    let mut xmm: BTreeMap<(usize, u8), Vec<usize>> = BTreeMap::new();
    for (index, key) in s.keys.iter().enumerate() {
        if let Some((reg, offset)) = xmm_offset(*key) {
            xmm.entry((reg, offset / 4)).or_default().push(index);
        }
    }
    for (_, mut keys) in xmm {
        keys.sort_by_key(|&index| xmm_offset(s.keys[index]).map_or(16, |(_, offset)| offset));
        if keys.len() == 4 {
            channels.push(keys);
        }
    }
    channels
}

enum WordArg {
    Source(ValueId),
    Const(u64),
    Pack(Vec<ValueId>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reject {
    MissingPhi,
    Arity,
    Shape,
    SelfReference,
}

struct LaneSite {
    key: usize,
    phi: ValueId,
    args: Vec<ValueId>,
}

struct Candidate {
    block: usize,
    word_args: Vec<WordArg>,
    lanes: Vec<LaneSite>,
}

enum Piece {
    Word(ValueId),
    Const(u64),
    Other,
}

fn piece(s: &Ssa, arg: ValueId, lane: usize, width: usize) -> Piece {
    let value = s.value(arg);
    match value.op {
        Op::Byte(index) if value.size == 1 && index as usize == lane && value.args_len == 1 => {
            let source = s.args(arg)[0];
            if s.value(source).size as usize == width {
                Piece::Word(source)
            } else {
                Piece::Other
            }
        }
        Op::Const(data) if value.size == 1 => Piece::Const(data & 0xff),
        _ => Piece::Other,
    }
}

fn candidate(
    s: &Ssa,
    phi_by_key: &[BTreeMap<usize, ValueId>],
    block: usize,
    keys: &[usize],
    start: usize,
    width: usize,
    used: &[bool],
) -> Result<Candidate, Reject> {
    let Some(selected) = keys.get(start..start + width) else {
        return Err(Reject::Shape);
    };
    if selected.iter().any(|&key| used[key]) {
        return Err(Reject::MissingPhi);
    }
    let preds = s.blocks[block].preds.len();
    let mut phis = Vec::with_capacity(width);
    for &key in selected {
        let Some(&phi) = phi_by_key[block].get(&key) else {
            return Err(Reject::MissingPhi);
        };
        if s.value(phi).args_len as usize != preds {
            return Err(Reject::Arity);
        }
        phis.push((key, phi, s.args(phi).to_vec()));
    }
    let mut word_args = Vec::with_capacity(preds);
    for position in 0..preds {
        let raw: Vec<ValueId> = phis.iter().map(|(_, _, args)| args[position]).collect();
        let mut source = None;
        let mut constant = None;
        let mut all_word = true;
        let mut all_const = true;
        for (lane, arg) in raw.iter().copied().enumerate() {
            match piece(s, arg, lane, width) {
                Piece::Word(value) => {
                    all_const = false;
                    if source.is_some_and(|previous| previous != value) {
                        all_word = false;
                    } else {
                        source = Some(value);
                    }
                }
                Piece::Const(byte) => {
                    all_word = false;
                    if source.is_some() {
                        all_const = false;
                    } else {
                        constant = Some(constant.unwrap_or(0) | (byte << (8 * lane)));
                    }
                }
                Piece::Other => {
                    all_word = false;
                    all_const = false;
                }
            }
        }
        word_args.push(if all_const {
            WordArg::Const(constant.unwrap())
        } else if all_word {
            WordArg::Source(source.unwrap())
        } else {
            if raw
                .iter()
                .any(|arg| phis.iter().any(|(_, phi, _)| phi == arg))
            {
                return Err(Reject::SelfReference);
            }
            WordArg::Pack(raw)
        });
    }
    Ok(Candidate {
        block,
        word_args,
        lanes: phis
            .into_iter()
            .map(|(key, phi, args)| LaneSite { key, phi, args })
            .collect(),
    })
}

fn coalesce_roots(s: &mut Ssa) {
    let channels = channels(s);

    let mut alias: Lookup<ValueId, ValueId> = Lookup::default();
    for keys in &channels {
        if keys.len() != 4 {
            continue;
        }
        let Some(inputs) = keys
            .iter()
            .map(|&key| {
                s.dispatch
                    .loads
                    .iter()
                    .find(|(candidate, _)| *candidate == s.keys[key])
                    .map(|(_, value)| *value)
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if inputs.iter().enumerate().any(|(lane, value)| {
            !matches!(s.value(*value).op, Op::Input(key) if key == s.keys[keys[lane]])
        }) {
            continue;
        }
        let addr = s.value_addr[inputs[0] as usize];
        let word = s.new_value(Op::Input(s.keys[keys[0]]), 4, &[], addr);
        for (lane, value) in inputs.into_iter().enumerate() {
            let byte = s.new_value(Op::Byte(lane as u8), 1, &[word], addr);
            alias.insert(value, byte);
        }
    }

    let mut groups: BTreeMap<(ValueId, Vec<usize>), [Option<ValueId>; 4]> = BTreeMap::new();
    for (index, value) in s.values.iter().enumerate() {
        let id = index as ValueId;
        let key = match value.op {
            Op::StateReload(key) | Op::StateKeep(key) => key,
            _ => continue,
        };
        let Some((keys, lane)) = channels.iter().find_map(|keys| {
            keys.iter()
                .position(|&index| s.keys[index] == key)
                .map(|lane| (keys, lane))
        }) else {
            continue;
        };
        if lane >= 4 {
            continue;
        }
        let site = s.args(id)[0];
        groups.entry((site, keys.clone())).or_insert([None; 4])[lane] = Some(id);
    }

    for ((site, keys), ids) in groups {
        let Some(ids) = ids.iter().copied().collect::<Option<Vec<_>>>() else {
            continue;
        };
        let addr = s.value_addr[ids[0] as usize];
        let keep = matches!(s.value(ids[0]).op, Op::StateKeep(_));
        if ids
            .iter()
            .any(|id| matches!(s.value(*id).op, Op::StateKeep(_)) != keep)
        {
            continue;
        }
        let word = if keep {
            let olds: Vec<ValueId> = ids.iter().map(|id| s.args(*id)[1]).collect();
            let old = s.new_value(Op::Pack, 4, &olds, addr);
            s.new_value(Op::StateKeep(s.keys[keys[0]]), 4, &[site, old], addr)
        } else {
            s.new_value(Op::StateReload(s.keys[keys[0]]), 4, &[site], addr)
        };
        for (lane, id) in ids.into_iter().enumerate() {
            let byte = s.new_value(Op::Byte(lane as u8), 1, &[word], addr);
            alias.insert(id, byte);
        }
    }
    rewrite_aliases(s, &alias);
}

pub(crate) fn coalesce(s: &mut Ssa, roots: bool) {
    if roots {
        coalesce_roots(s);
    }
    let channels = channels(s);

    let mut phi_by_key: Vec<BTreeMap<usize, ValueId>> = vec![BTreeMap::new(); s.blocks.len()];
    for (block, phis) in s.blocks.iter().enumerate() {
        for &phi in &phis.phis {
            if let Some(&key) = s.phi_key.get(&phi) {
                phi_by_key[block].insert(key, phi);
            }
        }
    }

    let mut candidates = Vec::new();
    for block in 0..s.blocks.len() {
        let mut used = vec![false; s.keys.len()];
        for keys in &channels {
            if keys.len() == 4 {
                if let Ok(found) = candidate(s, &phi_by_key, block, keys, 0, 4, &used) {
                    for lane in &found.lanes {
                        used[lane.key] = true;
                    }
                    candidates.push(found);
                }
            }
            let starts: &[usize] = if keys.len() == 4 { &[0, 2] } else { &[0] };
            for &start in starts {
                if let Ok(found) = candidate(s, &phi_by_key, block, keys, start, 2, &used) {
                    for lane in &found.lanes {
                        used[lane.key] = true;
                    }
                    candidates.push(found);
                }
            }
        }
    }

    let mut alias: Lookup<ValueId, ValueId> = Lookup::default();
    let mut constants = BTreeMap::new();
    let mut packs: BTreeMap<Vec<ValueId>, ValueId> = BTreeMap::new();
    let mut word_phis: BTreeMap<(usize, u8, Vec<ValueId>), ValueId> = BTreeMap::new();
    for candidate in candidates {
        let width = candidate.lanes.len() as u8;
        let addr = s.value_addr[candidate.lanes[0].phi as usize];
        let mut args = Vec::with_capacity(candidate.word_args.len());
        for arg in candidate.word_args {
            args.push(match arg {
                WordArg::Source(value) => value,
                WordArg::Const(data) => s.constant(&mut constants, width, data),
                WordArg::Pack(values) => *packs
                    .entry(values.clone())
                    .or_insert_with(|| s.new_value(Op::Pack, width, &values, addr)),
            });
        }
        let key = (candidate.block, width, args.clone());
        let phi = match word_phis.get(&key) {
            Some(&phi) => phi,
            None => {
                let phi = s.new_value(Op::Phi, width, &args, addr);
                let position = candidate
                    .lanes
                    .iter()
                    .find_map(|lane| {
                        s.blocks[candidate.block]
                            .phis
                            .iter()
                            .position(|value| *value == lane.phi)
                    })
                    .unwrap_or(s.blocks[candidate.block].phis.len());
                s.blocks[candidate.block].phis.insert(position, phi);
                word_phis.insert(key, phi);
                phi
            }
        };

        let mut group = LaneGroup {
            phi,
            block: candidate.block as u32,
            lanes: Vec::new(),
        };
        for (lane, site) in candidate.lanes.into_iter().enumerate() {
            let value = s.new_value(
                Op::Byte(lane as u8),
                1,
                &[phi],
                s.value_addr[site.phi as usize],
            );
            alias.insert(site.phi, value);
            group.lanes.push(LaneGroupLane {
                key: site.key,
                value,
                args: site.args,
            });
        }
        s.lane_groups.push(group);
    }
    rewrite_aliases(s, &alias);
}
