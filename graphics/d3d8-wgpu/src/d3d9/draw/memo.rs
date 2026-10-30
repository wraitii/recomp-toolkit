use super::*;

pub(super) struct Key<'a> {
    pub fingerprint: u64,
    pub interpreted_topology: Option<wgpu::PrimitiveTopology>,
    pub pipeline: Option<&'a wgpu::RenderPipeline>,
    pub ff_key: Option<[u32; 5]>,
    pub raw_inputs: u16,
    pub target: (wgpu::TextureFormat, u32),
    pub depth: Option<(wgpu::TextureFormat, u32)>,
    pub textures: &'a BoundTextures,
}

impl Key<'_> {
    fn matches(&self, memo: &DrawMemo) -> bool {
        memo.programs.is_some() == self.interpreted_topology.is_some()
            && self.interpreted_topology.is_none_or(|topology| {
                memo.pipeline
                    .as_ref()
                    .is_some_and(|(shape, _)| shape.1 == topology)
            })
            && self.pipeline.is_none_or(|compiled| {
                memo.pipeline
                    .as_ref()
                    .is_some_and(|(_, pipeline)| pipeline == compiled)
            })
            && memo.ff_key == self.ff_key
            && memo.raw_inputs == self.raw_inputs
            && memo.target == self.target
            && memo.depth == self.depth
    }
}

// A hit checks its memo out of its slot and returns it there, so the hot path never
// rehashes or shifts entries. Slots left empty by a draw that failed are skipped.
#[derive(Default)]
pub(super) struct Cache {
    entries: FastMap<u64, SmallVec<[(u64, Option<Box<DrawMemo>>); 1]>>,
    len: usize,
    clock: u64,
}

pub(super) struct Slot {
    fingerprint: u64,
    index: usize,
}

impl Cache {
    pub(super) fn take(&mut self, key: &Key<'_>) -> Option<(Box<DrawMemo>, Slot)> {
        if stats::enabled() {
            self.take_inner::<true>(key)
        } else {
            self.take_inner::<false>(key)
        }
    }

    fn take_inner<const COUNT: bool>(&mut self, key: &Key<'_>) -> Option<(Box<DrawMemo>, Slot)> {
        let mut examined = 0;
        let mut texture_rejects = 0;
        let mut found = None;
        if let Some(entries) = self.entries.get(&key.fingerprint) {
            for (index, (_, memo)) in entries.iter().enumerate().rev() {
                let Some(memo) = memo else { continue };
                if COUNT {
                    examined += 1;
                }
                if !key.matches(memo) {
                    continue;
                }
                if memo.textures == *key.textures {
                    found = Some(index);
                    break;
                }
                if COUNT {
                    texture_rejects += 1;
                }
            }
        }
        if COUNT {
            stats::record(|s| {
                s.memo_lookups += 1;
                s.memo_hits += u64::from(found.is_some());
                s.memo_misses += u64::from(found.is_none());
                s.memo_examined += examined;
                s.memo_fingerprint_matches += examined;
                s.memo_texture_rejects += texture_rejects;
            });
        }
        let index = found?;
        let memo = self.entries.get_mut(&key.fingerprint).unwrap()[index]
            .1
            .take()
            .unwrap();
        self.len -= 1;
        Some((
            memo,
            Slot {
                fingerprint: key.fingerprint,
                index,
            },
        ))
    }

    pub(super) fn put_back(&mut self, slot: Slot, memo: Box<DrawMemo>) -> bool {
        let entry = self
            .entries
            .get_mut(&slot.fingerprint)
            .and_then(|entries| entries.get_mut(slot.index))
            .filter(|entry| entry.1.is_none());
        let Some(entry) = entry else {
            return self.insert(memo);
        };
        self.clock += 1;
        *entry = (self.clock, Some(memo));
        self.len += 1;
        false
    }

    pub(super) fn insert(&mut self, memo: Box<DrawMemo>) -> bool {
        let evicted = self.len == DRAW_MEMOS;
        if evicted {
            let (fingerprint, index) = self
                .entries
                .iter()
                .flat_map(|(&fingerprint, entries)| {
                    entries
                        .iter()
                        .enumerate()
                        .filter(|(_, (_, memo))| memo.is_some())
                        .map(move |(index, (used, _))| (fingerprint, index, *used))
                })
                .min_by_key(|&(_, _, used)| used)
                .map(|(fingerprint, index, _)| (fingerprint, index))
                .unwrap();
            let entries = self.entries.get_mut(&fingerprint).unwrap();
            entries.remove(index);
            if entries.is_empty() {
                self.entries.remove(&fingerprint);
            }
            self.len -= 1;
        }
        self.clock += 1;
        self.entries
            .entry(memo.fingerprint)
            .or_default()
            .push((self.clock, Some(memo)));
        self.len += 1;
        evicted
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.len = 0;
        self.clock = 0;
    }
}
