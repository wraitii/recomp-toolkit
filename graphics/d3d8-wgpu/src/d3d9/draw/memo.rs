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

#[derive(Default)]
pub(super) struct Cache {
    entries: FastMap<u64, SmallVec<[(u64, Box<DrawMemo>); 1]>>,
    len: usize,
    clock: u64,
}

impl Cache {
    pub(super) fn take(&mut self, key: &Key<'_>) -> Option<Box<DrawMemo>> {
        if stats::enabled() {
            self.take_inner::<true>(key)
        } else {
            self.take_inner::<false>(key)
        }
    }

    fn take_inner<const COUNT: bool>(&mut self, key: &Key<'_>) -> Option<Box<DrawMemo>> {
        let mut examined = 0;
        let mut texture_rejects = 0;
        let mut found = None;
        if let Some(entries) = self.entries.get(&key.fingerprint) {
            for (index, (_, memo)) in entries.iter().enumerate().rev() {
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
        found.map(|index| self.remove(key.fingerprint, index))
    }

    fn remove(&mut self, fingerprint: u64, index: usize) -> Box<DrawMemo> {
        let entries = self.entries.get_mut(&fingerprint).unwrap();
        let (_, memo) = entries.remove(index);
        if entries.is_empty() {
            self.entries.remove(&fingerprint);
        }
        self.len -= 1;
        memo
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
                        .map(move |(index, (used, _))| (fingerprint, index, *used))
                })
                .min_by_key(|&(_, _, used)| used)
                .map(|(fingerprint, index, _)| (fingerprint, index))
                .unwrap();
            self.remove(fingerprint, index);
        }
        self.clock += 1;
        self.entries
            .entry(memo.fingerprint)
            .or_default()
            .push((self.clock, memo));
        self.len += 1;
        evicted
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.len = 0;
        self.clock = 0;
    }
}
