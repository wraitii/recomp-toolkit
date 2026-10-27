use super::*;

#[derive(Default)]
pub(super) struct Cache {
    entries: FastMap<u64, SmallVec<[(u64, Box<DrawMemo>); 1]>>,
    len: usize,
    clock: u64,
}

impl Cache {
    pub(super) fn take(
        &mut self,
        fingerprint: u64,
        mut matches: impl FnMut(&DrawMemo) -> bool,
    ) -> Option<Box<DrawMemo>> {
        let entries = self.entries.get(&fingerprint)?;
        let index = entries.iter().rposition(|(_, memo)| matches(memo))?;
        Some(self.remove(fingerprint, index))
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
