use crate::fast_map::FastMap;
use std::borrow::Borrow;
use std::hash::Hash;

struct Entry<V> {
    value: V,
    used: u64,
    bytes: usize,
}

pub(crate) struct LruCache<K, V, const ENTRIES: usize, const BYTES: usize = { usize::MAX }> {
    entries: FastMap<K, Entry<V>>,
    clock: u64,
    bytes: usize,
}

impl<K, V, const ENTRIES: usize, const BYTES: usize> Default for LruCache<K, V, ENTRIES, BYTES> {
    fn default() -> Self {
        Self {
            entries: FastMap::default(),
            clock: 0,
            bytes: 0,
        }
    }
}

impl<K: Clone + Eq + Hash, V, const ENTRIES: usize, const BYTES: usize>
    LruCache<K, V, ENTRIES, BYTES>
{
    pub(crate) fn get<Q: Eq + Hash + ?Sized>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let entry = self.entries.get_mut(key)?;
        self.clock += 1;
        entry.used = self.clock;
        Some(&entry.value)
    }

    pub(crate) fn insert(&mut self, key: K, value: V, bytes: usize) -> usize {
        let bytes = bytes.saturating_add(std::mem::size_of::<(K, Entry<V>)>());
        if bytes > BYTES || ENTRIES == 0 {
            return 0;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes -= previous.bytes;
        }
        let mut evicted = 0;
        while self.entries.len() >= ENTRIES || bytes > BYTES - self.bytes {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .unwrap()
                .0
                .clone();
            self.bytes -= self.entries.remove(&oldest).unwrap().bytes;
            evicted += 1;
        }
        self.clock += 1;
        self.entries.insert(
            key,
            Entry {
                value,
                used: self.clock,
                bytes,
            },
        );
        self.bytes += bytes;
        evicted
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(key, entry)| (key, &entry.value))
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.clock = 0;
    }
}
