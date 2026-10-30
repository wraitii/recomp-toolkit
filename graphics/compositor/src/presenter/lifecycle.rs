use super::FrameLease;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FrameSnapshot {
    pub sealed_ts: f64,
    pub submitted_ts: f64,
    pub gpu_ts: f64,
    pub presented_ts: f64,
    pub acknowledgement_deadline: f64,
    pub gpu_ms: f64,
    pub pending_prefixes: u32,
    pub gpu: u8,
    pub shown: u8,
    pub released: u8,
    pub dropped: u8,
    pub success: u8,
    pub repeat: u8,
    pub completion_fallback: u8,
    pub prefix_done: u8,
}

#[repr(C)]
pub struct Completion {
    pub ts: f64,
    pub period: f64,
    pub gpu_ms: f64,
    pub success: u8,
    pub offscreen: u8,
    pub stopped: u8,
    pub completion_only: u8,
}

pub struct FrameState {
    snapshot: Mutex<FrameSnapshot>,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(FrameSnapshot {
                sealed_ts: 0.0,
                submitted_ts: 0.0,
                gpu_ts: 0.0,
                presented_ts: 0.0,
                acknowledgement_deadline: 0.0,
                gpu_ms: 0.0,
                pending_prefixes: 0,
                gpu: 0,
                shown: 0,
                released: 0,
                dropped: 0,
                success: 1,
                repeat: 0,
                completion_fallback: 0,
                prefix_done: 1,
            }),
        }
    }
}

impl FrameState {
    pub fn snapshot(&self) -> FrameSnapshot {
        *self.snapshot.lock().unwrap()
    }

    pub fn seal(&self, ts: f64, prefix_pending: bool) {
        let mut s = self.snapshot.lock().unwrap();
        s.sealed_ts = ts;
        if prefix_pending {
            s.prefix_done = 0;
        }
    }

    pub fn repeated(&self, sealed: f64) {
        let mut s = self.snapshot.lock().unwrap();
        s.repeat = 1;
        s.sealed_ts = sealed;
    }

    pub fn submit(&self, ts: f64, grace: f64) {
        let mut s = self.snapshot.lock().unwrap();
        s.submitted_ts = ts;
        s.acknowledgement_deadline = ts + grace;
    }

    pub fn drop_frame(&self, gpu_complete: bool) {
        let mut s = self.snapshot.lock().unwrap();
        s.dropped = 1;
        if gpu_complete {
            s.gpu = 1;
        }
    }

    pub fn release(&self) -> bool {
        let mut s = self.snapshot.lock().unwrap();
        let fresh = s.released == 0;
        s.released = 1;
        fresh
    }

    pub fn fallback(&self, ts: f64) -> bool {
        let mut s = self.snapshot.lock().unwrap();
        if s.released != 0
            || s.dropped != 0
            || s.shown != 0
            || s.gpu == 0
            || s.success == 0
            || ts < s.acknowledgement_deadline
        {
            return false;
        }
        s.shown = 1;
        s.completion_fallback = 1;
        s.presented_ts = s.gpu_ts;
        true
    }

    pub fn command(&self, completion: Completion) -> bool {
        let mut s = self.snapshot.lock().unwrap();
        if s.released != 0 {
            return false;
        }
        s.gpu = 1;
        s.success &= u8::from(completion.success != 0);
        s.gpu_ts = completion.ts;
        s.gpu_ms = completion.gpu_ms.max(0.0);
        s.acknowledgement_deadline = s
            .acknowledgement_deadline
            .max(completion.ts + 2.0 * completion.period);
        if completion.offscreen != 0
            || completion.success == 0
            || completion.stopped != 0
            || completion.completion_only != 0
        {
            s.shown = 1;
            s.presented_ts = completion.ts;
            if completion.offscreen == 0
                && completion.success != 0
                && completion.stopped == 0
                && completion.completion_only != 0
            {
                s.completion_fallback = 1;
            }
        }
        true
    }

    pub fn presented(&self, ts: f64, fake: bool) -> bool {
        let mut s = self.snapshot.lock().unwrap();
        if (!fake && !(ts > 0.0)) || s.released != 0 || s.dropped != 0 || s.shown != 0 {
            return false;
        }
        s.shown = 1;
        s.presented_ts = ts;
        true
    }

    pub fn prefix_begin(&self) {
        let mut s = self.snapshot.lock().unwrap();
        s.prefix_done = 0;
        s.pending_prefixes += 1;
    }

    pub fn prefix_complete(&self, success: bool) {
        let mut s = self.snapshot.lock().unwrap();
        assert!(
            s.pending_prefixes > 0,
            "presenter: prefix completion without submission"
        );
        s.pending_prefixes -= 1;
        s.prefix_done = u8::from(s.pending_prefixes == 0);
        s.success &= u8::from(success);
    }

    pub fn synthetic_prefix_complete(&self) {
        self.snapshot.lock().unwrap().prefix_done = 1;
    }

    pub fn shutdown(&self, fake: bool) {
        let mut s = self.snapshot.lock().unwrap();
        s.shown = 1;
        if fake {
            s.gpu = 1;
            s.prefix_done = 1;
        }
    }

    fn ready(&self) -> bool {
        let s = self.snapshot.lock().unwrap();
        s.gpu != 0 && s.prefix_done != 0 && s.pending_prefixes == 0
    }
}

#[repr(C)]
#[derive(Default)]
pub struct Retirement {
    pub frame: *mut c_void,
    pub publish: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Counters {
    pub unique: u64,
    pub repeats: u64,
    pub drops: u64,
    pub pending_commands: u64,
}

#[derive(Default)]
pub struct Timeline {
    pub(super) flights: VecDeque<FrameLease>,
    retiring: VecDeque<FrameLease>,
    cached: Option<FrameLease>,
    counters: Counters,
}

impl FrameLease {
    pub(super) fn state(&self) -> &FrameState {
        unsafe { &*self.state }
    }

    fn repeated(&self) -> Self {
        unsafe { (self.repeat)(self.frame) }
    }
}

impl Timeline {
    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn len(&self, retiring: bool) -> usize {
        if retiring {
            self.retiring.len()
        } else {
            self.flights.len()
        }
    }

    pub fn frame(&self, retiring: bool, index: usize) -> *mut c_void {
        let queue = if retiring {
            &self.retiring
        } else {
            &self.flights
        };
        queue.get(index).map_or(std::ptr::null_mut(), |f| f.frame)
    }

    pub fn submit(&mut self, frame: FrameLease, ts: f64, grace: f64) {
        frame.state().submit(ts, grace);
        self.flights.push_back(frame);
    }

    pub fn withdraw(&mut self, state: *const FrameState) {
        if let Some(index) = self
            .flights
            .iter()
            .position(|f| std::ptr::eq(f.state, state))
        {
            self.flights.remove(index);
        }
    }

    pub fn defer(&mut self, frame: FrameLease, count: bool) {
        if count {
            self.counters.drops += 1;
        }
        frame.state().drop_frame(true);
        self.retiring.push_back(frame);
    }

    pub fn mark_drop(&mut self, state: &FrameState) {
        if state.snapshot().dropped == 0 {
            self.counters.drops += 1;
            state.drop_frame(false);
        }
    }

    pub fn committed(&mut self, state: &FrameState) {
        if state.snapshot().repeat != 0 {
            self.counters.repeats += 1;
        }
    }

    pub fn command_started(&mut self) {
        self.counters.pending_commands += 1;
    }

    pub fn command_finished(&mut self) {
        assert!(
            self.counters.pending_commands > 0,
            "presenter: command completion without submission"
        );
        self.counters.pending_commands -= 1;
    }

    pub fn cancel(&mut self) {
        while let Some(frame) = self.flights.pop_front() {
            self.counters.drops += 1;
            frame.state().drop_frame(false);
            self.retiring.push_back(frame);
        }
    }

    pub fn shutdown(&mut self, fake: bool) {
        for frame in &mut self.flights {
            frame.state().shutdown(fake);
        }
        if fake {
            for frame in &mut self.retiring {
                frame.state().synthetic_prefix_complete();
            }
        }
    }

    pub fn retire(&mut self, stopped: bool) -> Retirement {
        if let Some(index) = self.retiring.iter().position(|f| f.state().ready()) {
            return Retirement {
                frame: self.retiring.remove(index).unwrap().transfer(),
                publish: 0,
            };
        }
        if !self
            .flights
            .front()
            .is_some_and(|f| f.state().ready() && f.state().snapshot().shown != 0)
        {
            return Retirement::default();
        }
        let frame = self.flights.pop_front().unwrap();
        let publish = !stopped && frame.state().snapshot().success != 0;
        if publish {
            if frame.state().snapshot().repeat == 0 {
                self.counters.unique += 1;
            }
            self.cached = Some(frame.repeated());
        }
        Retirement {
            frame: frame.transfer(),
            publish: u8::from(publish),
        }
    }

    pub fn cached(&self) -> *mut c_void {
        self.cached
            .as_ref()
            .map_or(std::ptr::null_mut(), |f| f.frame)
    }

    pub(super) fn repeat_owned(&self, viewport: i32) -> Option<FrameLease> {
        if !self.flights.is_empty() {
            return None;
        }
        let Some(frame) = &self.cached else {
            return None;
        };
        let info = frame.info(false);
        if info.frame_id == 0 || (viewport >= 0 && (info.viewport != 0) != (viewport != 0)) {
            return None;
        }
        Some(frame.repeated())
    }

    pub fn clear_cached(&mut self) {
        self.cached = None;
    }

    pub fn deadline(&self, now: f64) -> f64 {
        self.flights
            .iter()
            .filter_map(|frame| {
                let s = frame.state().snapshot();
                (s.shown == 0 && (s.gpu != 0 || now < s.acknowledgement_deadline))
                    .then_some(s.acknowledgement_deadline)
            })
            .min_by(f64::total_cmp)
            .unwrap_or(-1.0)
    }
}
