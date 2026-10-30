pub mod turn;
use std::collections::VecDeque;
use std::ffi::c_void;

pub mod delivery;
#[cfg(feature = "ffi")]
mod ffi;
pub mod lifecycle;
pub mod surface;
pub mod worker;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FrameInfo {
    pub viewport: u8,
    pub capture: u8,
    pub native: u8,
    pub readiness: u32,
    pub frame_id: u64,
}

#[repr(C)]
pub struct FrameLease {
    pub(crate) frame: *mut c_void,
    pub(crate) inspect: unsafe extern "C" fn(*mut c_void, u8) -> FrameInfo,
    pub(crate) release: unsafe extern "C" fn(*mut c_void),
    pub(crate) state: *mut lifecycle::FrameState,
    pub(crate) repeat: unsafe extern "C" fn(*mut c_void) -> FrameLease,
}

impl FrameLease {
    fn info(&self, readiness: bool) -> FrameInfo {
        unsafe { (self.inspect)(self.frame, u8::from(readiness)) }
    }

    fn transfer(self) -> *mut c_void {
        let frame = self.frame;
        std::mem::forget(self);
        frame
    }
}

impl Drop for FrameLease {
    fn drop(&mut self) {
        unsafe { (self.release)(self.frame) };
    }
}

#[repr(C)]
#[derive(Default)]
pub struct MailboxSelection {
    pub discarded: [*mut c_void; 2],
    pub readiness: [u32; 2],
}

#[derive(Default)]
pub struct Mailbox {
    frames: VecDeque<FrameLease>,
}

impl Mailbox {
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, index: usize) -> *mut c_void {
        self.frames
            .get(index)
            .map_or(std::ptr::null_mut(), |f| f.frame)
    }

    pub fn enqueue(&mut self, frame: FrameLease, viewport: i32) -> *mut c_void {
        let discarded = if self.frames.len() == 2 {
            let infos = self.infos(false);
            let Some(index) = evict(&infos[..2], viewport) else {
                return frame.transfer();
            };
            self.frames.remove(index).unwrap().transfer()
        } else {
            std::ptr::null_mut()
        };
        self.frames.push_back(frame);
        discarded
    }

    pub fn retry(&mut self, frame: FrameLease) -> *mut c_void {
        if self.frames.is_empty() {
            self.frames.push_front(frame);
            std::ptr::null_mut()
        } else {
            frame.transfer()
        }
    }

    fn infos(&self, readiness: bool) -> [FrameInfo; 2] {
        std::array::from_fn(|i| {
            self.frames
                .get(i)
                .map_or(FrameInfo::default(), |frame| frame.info(readiness))
        })
    }

    fn select_owned(&mut self, viewport: i32) -> (Option<FrameLease>, MailboxSelection) {
        let mut frame = None;
        let infos = self.infos(true);
        let (selected, drops) = select(&infos[..self.frames.len()], viewport);
        let mut result = MailboxSelection::default();
        let mut discarded = 0;
        for i in (0..self.frames.len()).rev() {
            if drops & (1 << i) != 0 {
                result.discarded[discarded] = self.frames.remove(i).unwrap().transfer();
                result.readiness[discarded] = infos[i].readiness;
                discarded += 1;
            } else if selected == Some(i) {
                frame = self.frames.remove(i);
            }
        }
        (frame, result)
    }

    pub fn cancel(&mut self, viewport: i32) -> MailboxSelection {
        let mut result = MailboxSelection::default();
        let mut discarded = 0;
        for i in (0..self.frames.len()).rev() {
            if viewport < 0 || (self.frames[i].info(false).viewport != 0) != (viewport != 0) {
                result.discarded[discarded] = self.frames.remove(i).unwrap().transfer();
                discarded += 1;
            }
        }
        result
    }
}

fn evict(frames: &[FrameInfo], viewport: i32) -> Option<usize> {
    if frames
        .first()
        .is_some_and(|f| f.native != 0 && (viewport < 0 || (f.viewport != 0) == (viewport != 0)))
    {
        frames
            .len()
            .checked_sub(1)
            .filter(|&i| i > 0 && frames[i].capture == 0)
    } else {
        frames.iter().position(|f| f.capture == 0)
    }
}

fn select(frames: &[FrameInfo], viewport: i32) -> (Option<usize>, u64) {
    let eligible = |f: &FrameInfo| viewport < 0 || (f.viewport != 0) == (viewport != 0);
    if frames.last().is_some_and(|f| !eligible(f)) {
        return (None, 0);
    }
    if let Some(i) = frames.iter().position(|f| f.capture != 0 && eligible(f)) {
        return match frames[i].readiness {
            0 => (None, 0),
            1 => (
                Some(i),
                frames[..i]
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.capture == 0)
                    .fold(0, |mask, (j, _)| mask | (1 << j)),
            ),
            _ => (None, 1 << i),
        };
    }
    let mut drops = 0;
    for i in (0..frames.len()).rev() {
        let f = &frames[i];
        if !eligible(f) || f.readiness == 0 {
            continue;
        }
        if f.readiness == 1 {
            for (j, older) in frames[..i].iter().enumerate() {
                if older.capture == 0 {
                    drops |= 1 << j;
                }
            }
            return (Some(i), drops);
        }
        drops |= 1 << i;
    }
    (None, drops)
}

#[derive(Default)]
pub struct Pacing {
    events: VecDeque<(f64, f64, f64, bool)>,
    intervals: VecDeque<f64>,
    first: Option<f64>,
    last_new: Option<f64>,
    unique: u64,
    repeated: u64,
}
#[repr(C)]
#[derive(Default)]
pub struct PacingSnapshot {
    pub new_fps: f64,
    pub display_fps: f64,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub gpu_ms: f64,
    pub age_ms: f64,
    pub repeat_percent: f64,
    pub new_frames: u64,
    pub repeats: u64,
    pub drops: u64,
}
impl Pacing {
    pub fn displayed(&mut self, ts: f64, repeat: bool, gpu: f64, age: f64) {
        if !ts.is_finite() || ts < 0.0 || self.events.back().is_some_and(|e| ts < e.0) {
            return;
        }
        self.first.get_or_insert(ts);
        if repeat {
            self.repeated += 1;
        } else {
            self.unique += 1;
            if let Some(last) = self.last_new {
                if ts > last {
                    self.intervals.push_back((ts - last) * 1000.0);
                    if self.intervals.len() > 120 {
                        self.intervals.pop_front();
                    }
                }
            }
            self.last_new = Some(ts);
        }
        self.events
            .push_back((ts, gpu.max(0.0), age.max(0.0), repeat));
        while self.events.len() > 1024 || self.events.front().is_some_and(|e| e.0 < ts - 2.0) {
            self.events.pop_front();
        }
    }
    pub fn snapshot(&self, now: f64, drops: u64) -> (PacingSnapshot, Vec<f64>) {
        let mut s = PacingSnapshot {
            new_frames: self.unique,
            repeats: self.repeated,
            drops,
            ..Default::default()
        };
        let mut n = 0;
        let mut r = 0;
        for e in &self.events {
            if e.0 > now - 2.0 && e.0 <= now {
                n += 1;
                r += usize::from(e.3);
                s.gpu_ms += e.1;
                s.age_ms += e.2;
            }
        }
        let span = self
            .first
            .map_or(0.0, |first| (now - first).max(0.0).min(2.0));
        if span >= 0.25 {
            s.new_fps = (n - r) as f64 / span;
            s.display_fps = n as f64 / span;
        }
        if n > 0 {
            s.gpu_ms /= n as f64;
            s.age_ms /= n as f64;
            s.repeat_percent = 100.0 * r as f64 / n as f64;
        }
        let values = if self.last_new.is_some_and(|last| now - last < 2.0) {
            self.intervals.iter().copied().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if !values.is_empty() {
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            s.median_ms = sorted[sorted.len() / 2];
            s.p95_ms = sorted[((sorted.len() as f64 * 0.95).ceil() as usize) - 1];
        }
        (s, values)
    }
}
