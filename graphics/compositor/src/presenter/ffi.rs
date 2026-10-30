use crate::ffi::boundary;
use crate::presenter;

#[no_mangle]
pub unsafe extern "C" fn rc_mailbox_len(mailbox: *const presenter::Mailbox) -> usize {
    boundary(|| (*mailbox).len())
}
#[no_mangle]
pub unsafe extern "C" fn rc_mailbox_frame(
    mailbox: *const presenter::Mailbox,
    index: usize,
) -> *mut std::ffi::c_void {
    boundary(|| (*mailbox).frame(index))
}
#[no_mangle]
pub unsafe extern "C" fn rc_mailbox_enqueue(
    mailbox: *mut presenter::Mailbox,
    frame: presenter::FrameLease,
    viewport: i32,
) -> *mut std::ffi::c_void {
    boundary(|| (*mailbox).enqueue(frame, viewport))
}
#[no_mangle]
pub extern "C" fn rc_pacing_new() -> *mut presenter::Pacing {
    boundary(|| Box::into_raw(Box::new(presenter::Pacing::default())))
}
#[no_mangle]
pub unsafe extern "C" fn rc_pacing_free(p: *mut presenter::Pacing) {
    boundary(|| {
        if !p.is_null() {
            drop(Box::from_raw(p));
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_pacing_displayed(
    p: *mut presenter::Pacing,
    ts: f64,
    repeat: u8,
    gpu: f64,
    age: f64,
) {
    boundary(|| {
        if let Some(p) = p.as_mut() {
            p.displayed(ts, repeat != 0, gpu, age);
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_pacing_snapshot(
    p: *const presenter::Pacing,
    now: f64,
    drops: u64,
    intervals: *mut f64,
    count: *mut usize,
) -> presenter::PacingSnapshot {
    boundary(|| {
        let Some(p) = p.as_ref() else {
            return Default::default();
        };
        let (snapshot, values) = p.snapshot(now, drops);
        if let Some(count) = count.as_mut() {
            if !intervals.is_null() {
                std::ptr::copy_nonoverlapping(
                    values.as_ptr(),
                    intervals,
                    (*count).min(values.len()),
                );
            }
            *count = values.len();
        }
        snapshot
    })
}

#[no_mangle]
pub extern "C" fn rc_presenter_worker_start(
    callbacks: presenter::worker::WorkerCallbacks,
    offscreen: u8,
) -> *mut presenter::worker::Worker {
    boundary(
        || match presenter::worker::Worker::start(callbacks, offscreen != 0) {
            Ok(worker) => Box::into_raw(Box::new(worker)),
            Err(error) => {
                eprintln!("presenter: could not start worker: {error}");
                std::ptr::null_mut()
            }
        },
    )
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_worker_notify(worker: *const presenter::worker::Worker) {
    boundary(|| {
        if let Some(worker) = worker.as_ref() {
            worker.notify();
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_worker_stop(worker: *const presenter::worker::Worker) {
    boundary(|| {
        if let Some(worker) = worker.as_ref() {
            worker.stop();
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_worker_free(worker: *mut presenter::worker::Worker) {
    boundary(|| {
        if !worker.is_null() {
            drop(Box::from_raw(worker));
        }
    });
}

use presenter::lifecycle::{Completion, Counters, FrameSnapshot, FrameState, Retirement, Timeline};
#[no_mangle]
pub extern "C" fn rc_frame_state_new() -> *mut FrameState {
    boundary(|| Box::into_raw(Box::new(FrameState::default())))
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_state_free(state: *mut FrameState) {
    boundary(|| {
        if !state.is_null() {
            drop(Box::from_raw(state));
        }
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_snapshot(state: *const FrameState) -> FrameSnapshot {
    boundary(|| (*state).snapshot())
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_seal(state: *const FrameState, ts: f64, prefix: u8) {
    boundary(|| (*state).seal(ts, prefix != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_repeated(state: *const FrameState, sealed: f64) {
    boundary(|| (*state).repeated(sealed))
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_release(state: *const FrameState) -> u8 {
    boundary(|| u8::from((*state).release()))
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_command(state: *const FrameState, completion: Completion) -> u8 {
    boundary(|| u8::from((*state).command(completion)))
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_presented(state: *const FrameState, ts: f64, fake: u8) -> u8 {
    boundary(|| u8::from((*state).presented(ts, fake != 0)))
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_prefix_begin(state: *const FrameState) {
    boundary(|| (*state).prefix_begin())
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_prefix_complete(state: *const FrameState, success: u8) {
    boundary(|| (*state).prefix_complete(success != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_frame_synthetic_prefix_complete(state: *const FrameState) {
    boundary(|| (*state).synthetic_prefix_complete())
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_counters(timeline: *const Timeline) -> Counters {
    boundary(|| (*timeline).counters())
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_len(timeline: *const Timeline, retiring: u8) -> usize {
    boundary(|| (*timeline).len(retiring != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_frame(
    timeline: *const Timeline,
    retiring: u8,
    index: usize,
) -> *mut std::ffi::c_void {
    boundary(|| (*timeline).frame(retiring != 0, index))
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_defer(
    timeline: *mut Timeline,
    frame: presenter::FrameLease,
    count: u8,
) {
    boundary(|| (*timeline).defer(frame, count != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_mark_drop(timeline: *mut Timeline, state: *const FrameState) {
    boundary(|| (*timeline).mark_drop(&*state))
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_committed(timeline: *mut Timeline, state: *const FrameState) {
    boundary(|| (*timeline).committed(&*state))
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_retire(timeline: *mut Timeline, stopped: u8) -> Retirement {
    boundary(|| (*timeline).retire(stopped != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_cached(timeline: *const Timeline) -> *mut std::ffi::c_void {
    boundary(|| (*timeline).cached())
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_clear_cached(timeline: *mut Timeline) {
    boundary(|| (*timeline).clear_cached())
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_deadline(timeline: *const Timeline, now: f64) -> f64 {
    boundary(|| (*timeline).deadline(now))
}

#[no_mangle]
pub unsafe extern "C" fn rc_timeline_command_started(timeline: *mut Timeline) {
    boundary(|| (*timeline).command_started());
}
#[no_mangle]
pub unsafe extern "C" fn rc_timeline_command_finished(timeline: *mut Timeline) {
    boundary(|| (*timeline).command_finished());
}

use presenter::delivery::{Deliveries, Delivery};
use presenter::surface::{Backend, Surface, Transition};
#[no_mangle]
pub extern "C" fn rc_presenter_surface_new(backend: Backend, enabled: u8) -> *mut Surface {
    boundary(|| Box::into_raw(Box::new(Surface::new(backend, enabled != 0))))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_free(surface: *mut Surface) {
    boundary(|| {
        if !surface.is_null() {
            drop(Box::from_raw(surface));
        }
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_install(
    surface: *const Surface,
    native: *mut std::ffi::c_void,
    width: i32,
    height: i32,
    viewport: i32,
) {
    boundary(|| (*surface).install(native, width, height, viewport))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_resize(
    surface: *const Surface,
    width: i32,
    height: i32,
) {
    boundary(|| (*surface).resize(width, height))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_pending(surface: *const Surface) -> u8 {
    boundary(|| u8::from((*surface).pending()))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_viewport(surface: *const Surface) -> i32 {
    boundary(|| (*surface).viewport())
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_attached(surface: *const Surface) -> u8 {
    boundary(|| u8::from((*surface).attached()))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_apply(surface: *const Surface) -> Transition {
    boundary(|| (*surface).apply())
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_acquire(
    surface: *const Surface,
    suspended: u8,
) -> u64 {
    boundary(|| (*surface).acquire(suspended != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_refresh(
    surface: *const Surface,
    fallback: f64,
) -> f64 {
    boundary(|| (*surface).refresh(fallback))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_adopt(surface: *const Surface, chain: u64) {
    boundary(|| (*surface).adopt(chain))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_surface_close(surface: *const Surface) {
    boundary(|| (*surface).close())
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_deliveries_free(deliveries: *mut Deliveries) {
    boundary(|| {
        if !deliveries.is_null() {
            drop(Box::from_raw(deliveries));
        }
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_deliveries_enqueue(
    deliveries: *const Deliveries,
    delivery: Delivery,
) -> u8 {
    boundary(|| u8::from((*deliveries).enqueue(delivery)))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_deliveries_pending(deliveries: *const Deliveries) -> usize {
    boundary(|| deliveries.as_ref().map_or(0, Deliveries::pending))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_deliveries_stop(deliveries: *const Deliveries) {
    boundary(|| {
        if let Some(deliveries) = deliveries.as_ref() {
            deliveries.stop();
        }
    })
}
#[no_mangle]
pub extern "C" fn rc_presenter_deliveries_new() -> *mut Deliveries {
    boundary(|| match Deliveries::new() {
        Ok(deliveries) => Box::into_raw(Box::new(deliveries)),
        Err(error) => {
            eprintln!("presenter: could not start capture delivery: {error}");
            std::ptr::null_mut()
        }
    })
}

use presenter::turn::{Input as TurnInput, Presenter, Selection as TurnSelection};
#[no_mangle]
pub extern "C" fn rc_presenter_new() -> *mut Presenter {
    boundary(|| Box::into_raw(Box::new(Presenter::default())))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_free(p: *mut Presenter) {
    boundary(|| {
        if !p.is_null() {
            drop(Box::from_raw(p));
        }
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_mailbox(p: *mut Presenter) -> *mut presenter::Mailbox {
    boundary(|| &mut (*p).mailbox as *mut _)
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_timeline(p: *mut Presenter) -> *mut Timeline {
    boundary(|| &mut (*p).timeline as *mut _)
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_signal(p: *mut Presenter, ts: f64, pending: u8) {
    boundary(|| (*p).signal(ts, pending != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_ready(p: *const Presenter, input: TurnInput) -> u8 {
    boundary(|| u8::from((*p).ready(input)))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_poll_native(p: *const Presenter, input: TurnInput) -> u8 {
    boundary(|| u8::from((*p).poll_native(input)))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_check(p: *mut Presenter, input: TurnInput) -> TurnSelection {
    boundary(|| (*p).check(input))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_admit(p: *mut Presenter, input: TurnInput) -> TurnSelection {
    boundary(|| (*p).admit(input))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_run(
    callbacks: presenter::turn::Callbacks,
    surface: *const Surface,
    ts: f64,
    fake: u8,
    automatic: u8,
    offscreen: u8,
    suspended: u8,
) {
    boundary(|| {
        presenter::turn::run(
            callbacks,
            &*surface,
            ts,
            fake != 0,
            automatic != 0,
            offscreen != 0,
            suspended != 0,
        )
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_reject(
    p: *mut Presenter,
    frame: presenter::FrameLease,
    retry: u8,
) -> presenter::turn::Rejected {
    boundary(|| (*p).reject(frame, retry != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_transition(
    p: *mut Presenter,
    viewport: i32,
    changed: u8,
) -> presenter::MailboxSelection {
    boundary(|| (*p).transition(viewport, changed != 0))
}
#[no_mangle]
pub unsafe extern "C" fn rc_presenter_submit(
    surface: *const Surface,
    submission: presenter::turn::Submission,
) {
    boundary(|| presenter::turn::submit(&*surface, submission))
}

#[no_mangle]
pub unsafe extern "C" fn rc_presenter_shutdown(p: *mut Presenter, fake: u8) {
    boundary(|| (*p).shutdown(fake != 0))
}

#[no_mangle]
pub unsafe extern "C" fn rc_presenter_native_pending(p: *const Presenter) -> u8 {
    boundary(|| u8::from((*p).native_pending()))
}
