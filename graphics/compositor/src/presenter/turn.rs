use super::{lifecycle::Timeline, Mailbox};
use std::ffi::c_void;

pub struct Presenter {
    pub mailbox: Mailbox,
    pub timeline: Timeline,
    tick_pending: bool,
    pacing_permit: bool,
    retry_wait: bool,
    last_link_tick: f64,
    window_started: Option<f64>,
}

impl Default for Presenter {
    fn default() -> Self {
        Self {
            mailbox: Mailbox::default(),
            timeline: Timeline::default(),
            tick_pending: false,
            pacing_permit: true,
            retry_wait: false,
            last_link_tick: -1.0,
            window_started: None,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Input {
    pub ts: f64,
    pub period: f64,
    pub flight_limit: usize,
    pub capture_backlog: usize,
    pub viewport: i32,
    pub offscreen: u8,
    pub fake: u8,
    pub duration_pacing: u8,
    pub minimum_duration: u8,
    pub surface_pending: u8,
}

#[repr(u32)]
enum Fault {
    DisplayStalled = 2,
    DisplayFallback = 4,
    GpuTimeout = 16,
}

#[repr(u8)]
enum Failure {
    Encoding = 0,
    NoDrawable = 1,
}

#[repr(C)]
#[derive(Default)]
pub struct Selection {
    pub frame: *mut c_void,
    pub discarded: [*mut c_void; 2],
    pub readiness: [u32; 2],
    pub fallback: [*mut c_void; 3],
    pub faults: u32,
}

impl Input {
    fn cpu_paced(self) -> bool {
        self.offscreen == 0
            && self.fake == 0
            && self.duration_pacing != 0
            && self.minimum_duration == 0
    }
}

impl Presenter {
    pub fn signal(&mut self, ts: f64, pending: bool) {
        self.last_link_tick = ts;
        self.tick_pending = pending;
        self.pacing_permit = true;
        self.retry_wait = false;
    }

    fn capacity(&self, input: Input) -> bool {
        !self.retry_wait
            && self.timeline.len(false) < input.flight_limit
            && input.capture_backlog < input.flight_limit
            && (!input.cpu_paced() || self.pacing_permit)
    }

    pub fn ready(&self, input: Input) -> bool {
        self.tick_pending
            || input.surface_pending != 0
            || (self.capacity(input) && self.mailbox.len() != 0)
    }

    pub fn poll_native(&self, input: Input) -> bool {
        self.native_pending()
            && self.capacity(input)
            && !self.tick_pending
            && input.surface_pending == 0
    }

    pub fn native_pending(&self) -> bool {
        (0..self.mailbox.len()).any(|i| self.mailbox.frames[i].info(false).native != 0)
    }

    pub fn surface_changed(&mut self) {
        self.timeline.clear_cached();
        self.pacing_permit = true;
        self.retry_wait = false;
    }

    pub fn check(&mut self, input: Input) -> Selection {
        self.tick_pending = false;
        let mut result = Selection::default();
        if input.offscreen == 0 {
            let started = *self.window_started.get_or_insert(input.ts);
            if (self.timeline.len(false) != 0 || self.mailbox.len() != 0)
                && input.ts - started.max(self.last_link_tick) >= 1.0
            {
                result.faults |= Fault::DisplayStalled as u32;
            }
            for i in 0..self.timeline.len(false) {
                let frame = &self.timeline.flights[i];
                if frame.state().fallback(input.ts) {
                    result.faults |= Fault::DisplayFallback as u32;
                    result.fallback[i] = frame.frame;
                }
                let status = frame.state().snapshot();
                if status.gpu == 0 && input.ts - status.submitted_ts >= 1.0 {
                    result.faults |= Fault::GpuTimeout as u32;
                }
            }
        }
        result
    }

    pub fn admit(&mut self, input: Input) -> Selection {
        let mut result = Selection::default();
        if !self.capacity(input) {
            return result;
        }
        let frame = if self.mailbox.len() == 0 {
            let Some(frame) = self.timeline.repeat_owned(input.viewport) else {
                return result;
            };
            frame
        } else {
            let (frame, selection) = self.mailbox.select_owned(input.viewport);
            result.discarded = selection.discarded;
            result.readiness = selection.readiness;
            let Some(frame) = frame else {
                return result;
            };
            frame
        };
        result.frame = frame.frame;
        self.timeline.submit(
            frame,
            input.ts,
            (input.flight_limit + 2) as f64 * input.period,
        );
        if input.cpu_paced() {
            self.pacing_permit = false;
        }
        result
    }
}

#[repr(C)]
pub struct Encoding {
    pub width: i32,
    pub height: i32,
    pub drawable_width: i32,
    pub drawable_height: i32,
    pub x: i32,
    pub y: i32,
    pub format: u32,
    pub drawable_format: u32,
    pub previous_width: i32,
    pub previous_height: i32,
    pub previous_format: u32,
    pub previous: u8,
    pub repeat: u8,
    pub placed: u8,
}

#[repr(C)]
pub struct Callbacks {
    pub context: *mut c_void,
    pub transition: unsafe extern "C" fn(*mut c_void, super::surface::Transition),
    pub select: unsafe extern "C" fn(*mut c_void, f64) -> *mut c_void,
    pub prefix: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8,
    pub prepare: unsafe extern "C" fn(*mut c_void, *mut c_void),
    pub describe: unsafe extern "C" fn(*mut c_void, *mut c_void, u64) -> Encoding,
    pub begin: unsafe extern "C" fn(*mut c_void, *mut c_void, u8) -> u64,
    pub encode: unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u32),
    pub commit: unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u64),
    pub complete: unsafe extern "C" fn(*mut c_void, *mut c_void, f64, u8),
    pub failed: unsafe extern "C" fn(*mut c_void, *mut c_void, u8),
    pub release: unsafe extern "C" fn(*mut c_void),
}

#[repr(u32)]
enum Operation {
    Compose,
    Copy,
    Place,
    Fit,
    Overlays,
}

struct Turn<'a> {
    callbacks: &'a Callbacks,
    frame: *mut c_void,
}
impl Drop for Turn<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.callbacks.release)(self.frame);
        }
    }
}

pub unsafe fn run(
    callbacks: Callbacks,
    surface: &super::surface::Surface,
    ts: f64,
    fake: bool,
    automatic: bool,
    offscreen: bool,
    suspended: bool,
) {
    if !ts.is_finite() || ts < 0.0 {
        return;
    }
    let context = callbacks.context;
    (callbacks.transition)(context, surface.apply());
    let frame = (callbacks.select)(context, ts);
    if frame.is_null() {
        return;
    }
    let turn = Turn {
        callbacks: &callbacks,
        frame,
    };
    if (callbacks.prefix)(context, frame) == 0 {
        (callbacks.failed)(context, frame, Failure::Encoding as u8);
        return;
    }
    if fake {
        (callbacks.prepare)(context, frame);
        (callbacks.complete)(context, frame, ts, u8::from(automatic));
        return;
    }
    let drawable = surface.acquire(offscreen || suspended);
    if !offscreen && drawable == 0 {
        (callbacks.failed)(context, frame, Failure::NoDrawable as u8);
        return;
    }
    let encoding = (callbacks.describe)(context, frame, drawable);
    let reuse = encoding.repeat != 0
        && encoding.previous != 0
        && encoding.width == encoding.previous_width
        && encoding.height == encoding.previous_height
        && encoding.format == encoding.previous_format;
    let command = (callbacks.begin)(context, frame, u8::from(reuse));
    if command == 0 {
        if drawable != 0 {
            surface.release(drawable);
        }
        (callbacks.failed)(context, frame, Failure::Encoding as u8);
        return;
    }
    (callbacks.prepare)(context, frame);
    if !reuse {
        (callbacks.encode)(context, frame, drawable, Operation::Compose as u32);
    }
    if drawable != 0 {
        let operation = if encoding.format == encoding.drawable_format
            && encoding.width == encoding.drawable_width
            && encoding.height == encoding.drawable_height
        {
            Operation::Copy
        } else if encoding.format == encoding.drawable_format
            && encoding.placed != 0
            && encoding.x + encoding.width <= encoding.drawable_width
            && encoding.y + encoding.height <= encoding.drawable_height
        {
            Operation::Place
        } else {
            Operation::Fit
        };
        (callbacks.encode)(context, frame, drawable, operation as u32);
        (callbacks.encode)(context, frame, drawable, Operation::Overlays as u32);
    }
    (callbacks.commit)(context, frame, drawable, command);
    drop(turn);
}

#[repr(C)]
pub struct Rejected {
    pub frame: *mut c_void,
    pub release: u8,
}
impl Presenter {
    pub fn reject(&mut self, frame: super::FrameLease, retry: bool) -> Rejected {
        self.timeline.withdraw(frame.state);
        self.retry_wait |= retry;
        let release = retry && frame.state().snapshot().repeat != 0;
        let frame = if retry && !release {
            self.mailbox.retry(frame)
        } else {
            frame.transfer()
        };
        Rejected {
            frame,
            release: u8::from(release),
        }
    }

    pub fn shutdown(&mut self, fake: bool) {
        for frame in self.mailbox.frames.drain(..) {
            self.timeline.defer(frame, false);
        }
        self.timeline.shutdown(fake);
    }

    pub fn transition(&mut self, viewport: i32, changed: bool) -> super::MailboxSelection {
        self.timeline.cancel();
        if changed {
            self.surface_changed();
            if viewport >= 0 {
                return self.mailbox.cancel(viewport);
            }
        }
        super::MailboxSelection::default()
    }
}

#[repr(C)]
pub struct Submission {
    pub context: *mut c_void,
    pub command: u64,
    pub drawable: u64,
    pub period: f64,
    pub duration_pacing: u8,
    pub arm: unsafe extern "C" fn(*mut c_void),
    pub commit: unsafe extern "C" fn(*mut c_void),
    pub presented: super::surface::Presented,
}

pub unsafe fn submit(surface: &super::surface::Surface, submission: Submission) {
    (submission.arm)(submission.context);
    if submission.drawable != 0 {
        let duration = if submission.duration_pacing != 0 {
            submission.period
        } else {
            0.0
        };
        surface.present(
            submission.command,
            submission.drawable,
            duration,
            submission.presented,
        );
    }
    (submission.commit)(submission.context);
}
