use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Debug, Default)]
pub(super) struct Counters {
    pub draws: u64,
    pub binding_refreshes: u64,
    pub binding_refreshes_unchanged: u64,
    pub effect_pass_binding_refreshes: u64,
    pub effect_pass_binding_refreshes_unchanged: u64,
    pub memo_lookups: u64,
    pub memo_hits: u64,
    pub memo_misses: u64,
    pub memo_examined: u64,
    pub memo_fingerprint_matches: u64,
    pub memo_texture_rejects: u64,
    pub memo_evictions: u64,
    pub index_hits: u64,
    pub index_misses: u64,
    pub index_new_buffers: u64,
    pub index_scanned: u64,
    pub index_cleanup_scans: u64,
    pub index_cleanup_entries: u64,
    pub index_cleanup_removed: u64,
    pub index_buffer_evictions: u64,
    pub index_range_evictions: u64,
    pub native_layout_hits: u64,
    pub native_layout_misses: u64,
    pub native_layout_evictions: u64,
    pub texture_hits: u64,
    pub texture_cold_misses: u64,
    pub texture_changed_misses: u64,
    pub texture_epoch_invalidations: u64,
    pub texture_invalidated_slots: u64,
    pub effect_begins: u64,
    pub effect_begin_passes: u64,
    pub effect_commits: u64,
    pub effect_setters: u64,
    pub effect_texture_setters: u64,
    pub ctab_parses: u64,
    pub ctab_input_bytes: u64,
    pub saved_key_builds: u64,
    pub saved_ctab_parses: u64,
    pub constant_updates: u64,
    pub constant_registers: u64,
    pub constant_temp_bytes: u64,
    pub buffer_uploads: u64,
    pub buffer_upload_bytes: u64,
    pub buffer_snapshot_copies: u64,
    pub buffer_weak_detaches: u64,
    pub index_buffer_uploads: u64,
    pub index_buffer_snapshot_copies: u64,
    pub index_buffer_weak_detaches: u64,
}

struct Statistics {
    counters: Counters,
    dirty: bool,
    frames: u64,
    total_frames: u64,
    started: Instant,
}

impl Statistics {
    fn report(&mut self) {
        eprintln!(
            "[d3d9-stats] schema=1 thread={:?} frame={} frames={} elapsed_ms={:.3} {:?}",
            std::thread::current().id(),
            self.total_frames,
            self.frames,
            self.started.elapsed().as_secs_f64() * 1000.0,
            self.counters
        );
        self.counters = Counters::default();
        self.dirty = false;
        self.frames = 0;
        self.started = Instant::now();
    }
}

impl Drop for Statistics {
    fn drop(&mut self) {
        if self.frames != 0 || self.dirty {
            self.report();
        }
    }
}

thread_local! {
    static STATISTICS: RefCell<Statistics> = RefCell::new(Statistics {
        counters: Counters::default(),
        dirty: false,
        frames: 0,
        total_frames: 0,
        started: Instant::now(),
    });
}

#[inline]
pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RECOMP_D3D9_STATS").is_ok_and(|v| v == "1"))
}

#[inline]
pub(super) fn record(update: impl FnOnce(&mut Counters)) {
    if enabled() {
        STATISTICS.with_borrow_mut(|s| {
            update(&mut s.counters);
            s.dirty = true;
        });
    }
}

pub(super) fn end_frame() {
    if enabled() {
        STATISTICS.with_borrow_mut(|s| {
            s.frames += 1;
            s.total_frames += 1;
            if s.frames == 120 {
                s.report();
            }
        });
    }
}
