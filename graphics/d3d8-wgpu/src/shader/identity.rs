use crate::fast_map::FastHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProgramKey {
    fingerprint: u64,
    words: Arc<[u32]>,
}

impl ProgramKey {
    pub(crate) fn byte_len(&self) -> usize {
        self.words.len() * 4
    }

    pub(crate) fn new(words: Arc<[u32]>) -> Self {
        let mut hasher = FastHasher::default();
        words.hash(&mut hasher);
        Self {
            fingerprint: hasher.finish(),
            words,
        }
    }
}

impl Hash for ProgramKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.fingerprint);
    }
}

impl fmt::Debug for ProgramKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgramKey")
            .field("fingerprint", &format_args!("{:016x}", self.fingerprint))
            .field("words", &self.words.len())
            .finish()
    }
}
