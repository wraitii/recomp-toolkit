use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Default)]
pub struct WorkerState {
    pub now: f64,
    pub period: f64,
    pub acknowledgement: f64,
    pub ready: u8,
    pub native_pending: u8,
    pub poll_native: u8,
}

#[repr(C)]
pub struct WorkerCallbacks {
    pub(crate) context: *mut c_void,
    pub(crate) state: unsafe extern "C" fn(*mut c_void) -> WorkerState,
    pub(crate) tick: unsafe extern "C" fn(*mut c_void, f64, u8),
    pub(crate) release: unsafe extern "C" fn(*mut c_void),
}

unsafe impl Send for WorkerCallbacks {}

impl Drop for WorkerCallbacks {
    fn drop(&mut self) {
        unsafe { (self.release)(self.context) };
    }
}

#[derive(Default)]
struct WakeState {
    generation: u64,
    stop: bool,
}

#[derive(Default)]
struct Signal {
    state: Mutex<WakeState>,
    wake: Condvar,
}

pub struct Worker {
    signal: Arc<Signal>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Worker {
    pub fn start(callbacks: WorkerCallbacks, offscreen: bool) -> std::io::Result<Self> {
        let signal = Arc::new(Signal::default());
        let worker_signal = signal.clone();
        let thread = thread::Builder::new()
            .name("presenter".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&callbacks, &worker_signal, offscreen);
                }))
                .is_err()
                {
                    eprintln!("presenter: worker panicked");
                    std::process::abort();
                }
            })?;
        Ok(Self {
            signal,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn notify(&self) {
        let mut state = self.signal.state.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        self.signal.wake.notify_one();
    }

    pub fn stop(&self) {
        {
            let mut state = self.signal.state.lock().unwrap();
            state.stop = true;
            self.signal.wake.notify_all();
        }
        if let Some(thread) = self.thread.lock().unwrap().take() {
            if thread.join().is_err() {
                eprintln!("presenter: worker join failed");
                std::process::abort();
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(callbacks: &WorkerCallbacks, signal: &Signal, offscreen: bool) {
    let start = Instant::now();
    let origin = unsafe { (callbacks.state)(callbacks.context) }.now;
    let mut synthetic_index = 0u64;
    let mut next_refresh = None;
    loop {
        let generation = {
            let state = signal.state.lock().unwrap();
            if state.stop {
                return;
            }
            state.generation
        };
        let state = unsafe { (callbacks.state)(callbacks.context) };
        let period = if state.period.is_finite() && state.period > 0.0 {
            state.period
        } else {
            1.0 / 60.0
        };
        let (delay, synthetic_ts) = if offscreen {
            if state.native_pending != 0 {
                (0.001, None)
            } else {
                synthetic_index =
                    (synthetic_index + 1).max((start.elapsed().as_secs_f64() * 120.0) as u64 + 1);
                let elapsed = synthetic_index as f64 / 120.0;
                (
                    (elapsed - start.elapsed().as_secs_f64()).max(0.0),
                    Some(origin + elapsed),
                )
            }
        } else {
            let refresh =
                next_refresh.get_or_insert_with(|| (state.now / period).floor() * period + period);
            let mut delay = (*refresh - state.now).max(0.0).min(0.016);
            if state.acknowledgement >= 0.0 {
                delay = delay.min((state.acknowledgement - state.now).max(0.0));
            }
            if state.poll_native != 0 {
                delay = delay.min(0.001);
            } else if state.ready != 0 {
                delay = 0.0;
            }
            (delay, None)
        };
        let mut guard = signal.state.lock().unwrap();
        if offscreen && synthetic_ts.is_some() {
            guard = signal
                .wake
                .wait_timeout_while(guard, Duration::from_secs_f64(delay), |s| !s.stop)
                .unwrap()
                .0;
        } else if guard.generation == generation && !guard.stop && delay > 0.0 {
            guard = signal
                .wake
                .wait_timeout_while(guard, Duration::from_secs_f64(delay), |s| {
                    !s.stop && s.generation == generation
                })
                .unwrap()
                .0;
        }
        if guard.stop {
            return;
        }
        drop(guard);
        let state = unsafe { (callbacks.state)(callbacks.context) };
        let refresh = !offscreen && next_refresh.is_some_and(|next| state.now >= next);
        if refresh {
            next_refresh = Some((state.now / period).floor() * period + period);
        }
        unsafe {
            (callbacks.tick)(
                callbacks.context,
                synthetic_ts.unwrap_or(state.now),
                u8::from(refresh),
            );
        }
    }
}
