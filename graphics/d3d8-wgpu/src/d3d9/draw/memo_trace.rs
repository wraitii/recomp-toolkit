use super::*;

const CAPACITIES: [usize; 7] = [16, 32, 64, 128, 256, 512, 1024];
const KINDS: [&str; 3] = ["full", "no_resources", "no_bindings"];

pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("RECOMP_D3D9_TRACE_MEMOS").is_ok_and(|value| value == "1"))
}

pub(super) fn keys(
    state: &D3d9State,
    textures: &BoundTextures,
    qualifiers: &impl Hash,
) -> [u64; 3] {
    let mut hashes: [FastHasher; 3] = std::array::from_fn(|_| FastHasher::default());
    for ((hash, state), kind) in hashes
        .iter_mut()
        .zip(state.memo_trace_fingerprints())
        .zip(0..3)
    {
        state.hash(hash);
        qualifiers.hash(hash);
        textures.len().hash(hash);
        for texture in textures {
            (
                texture.stage,
                texture.format,
                texture.depth,
                texture.cube,
                texture.volume,
            )
                .hash(hash);
            if kind < 2 {
                (texture.width, texture.height, texture.lod, texture.levels).hash(hash);
            }
            if kind == 0 {
                texture.texture.hash(hash);
                texture.view.hash(hash);
            }
        }
    }
    hashes.map(|hash| hash.finish())
}

#[derive(Default)]
struct Reuse {
    recent: Vec<u64>,
    hits: [u64; 7],
    miss_hits: [u64; 7],
}

impl Reuse {
    fn record(&mut self, key: u64, actual_miss: bool) {
        if let Some(index) = self.recent.iter().rposition(|&previous| previous == key) {
            let distance = self.recent.len() - index;
            for (bucket, capacity) in CAPACITIES.into_iter().enumerate() {
                if distance <= capacity {
                    self.hits[bucket] += 1;
                    self.miss_hits[bucket] += u64::from(actual_miss);
                }
            }
            self.recent.remove(index);
        } else if self.recent.len() == CAPACITIES[6] {
            self.recent.remove(0);
        }
        self.recent.push(key);
    }
}

#[derive(Default)]
pub(super) struct Capture {
    reuse: [Reuse; 3],
    draws: u64,
    actual_misses: u64,
    frames: u64,
    clears: u64,
    last_frame: u64,
}

impl Capture {
    pub(super) fn record(&mut self, keys: [u64; 3], hit: bool) {
        self.draws += 1;
        self.actual_misses += u64::from(!hit);
        for (reuse, key) in self.reuse.iter_mut().zip(keys) {
            reuse.record(key, !hit);
        }
    }

    pub(super) fn clear(&mut self) {
        if enabled() {
            for reuse in &mut self.reuse {
                reuse.recent.clear();
            }
            self.clears += 1;
        }
    }

    pub(super) fn end_frame(&mut self, frame: u64) {
        if enabled() {
            self.last_frame = frame;
            self.frames += 1;
            if self.frames == 120 {
                self.report();
            }
        }
    }

    fn report(&mut self) {
        for (reuse, kind) in self.reuse.iter_mut().zip(KINDS) {
            eprintln!(
                "[d3d9-memos] schema=1 thread={:?} frame={} frames={} draws={} actual_misses={} clears={} kind={kind} capacities={CAPACITIES:?} hits={:?} actual_miss_hits={:?}",
                std::thread::current().id(),
                self.last_frame + 1,
                self.frames,
                self.draws,
                self.actual_misses,
                self.clears,
                reuse.hits,
                reuse.miss_hits,
            );
            reuse.hits.fill(0);
            reuse.miss_hits.fill(0);
        }
        self.draws = 0;
        self.actual_misses = 0;
        self.frames = 0;
        self.clears = 0;
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if self.draws != 0 {
            self.report();
        }
    }
}
