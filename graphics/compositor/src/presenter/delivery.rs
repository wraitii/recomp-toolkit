use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle, ThreadId};

#[repr(C)]
pub struct Delivery {
    pub(crate) context: *mut c_void,
    pub(crate) run: unsafe extern "C" fn(*mut c_void),
    pub(crate) release: unsafe extern "C" fn(*mut c_void),
}

unsafe impl Send for Delivery {}

impl Drop for Delivery {
    fn drop(&mut self) {
        unsafe { (self.release)(self.context) };
    }
}

#[derive(Default)]
struct State {
    queued: VecDeque<Delivery>,
    running: bool,
    closed: bool,
    exited: bool,
    thread: Option<ThreadId>,
}

#[derive(Default)]
struct Signal {
    state: Mutex<State>,
    wake: Condvar,
}

pub struct Deliveries {
    signal: Arc<Signal>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Deliveries {
    pub fn new() -> std::io::Result<Self> {
        let signal = Arc::new(Signal::default());
        let worker = signal.clone();
        let thread = thread::Builder::new()
            .name("presenter-capture".into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&worker))).is_err()
                {
                    eprintln!("presenter: capture delivery worker panicked");
                    std::process::abort();
                }
            })?;
        Ok(Self {
            signal,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn enqueue(&self, delivery: Delivery) -> bool {
        let mut state = self.signal.state.lock().unwrap();
        if state.closed {
            drop(state);
            drop(delivery);
            return false;
        }
        state.queued.push_back(delivery);
        self.signal.wake.notify_one();
        true
    }

    pub fn pending(&self) -> usize {
        let state = self.signal.state.lock().unwrap();
        state.queued.len() + usize::from(state.running)
    }

    pub fn stop(&self) {
        let mut state = self.signal.state.lock().unwrap();
        state.closed = true;
        self.signal.wake.notify_all();
        if state.thread == Some(thread::current().id()) {
            let cancelled = std::mem::take(&mut state.queued);
            drop(state);
            drop(cancelled);
            return;
        }
        while !state.exited {
            state = self.signal.wake.wait(state).unwrap();
        }
        drop(state);
        let thread = self.thread.lock().unwrap().take();
        if let Some(thread) = thread {
            if thread.join().is_err() {
                eprintln!("presenter: capture delivery join failed");
                std::process::abort();
            }
        }
    }
}

impl Drop for Deliveries {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(signal: &Signal) {
    let mut state = signal.state.lock().unwrap();
    state.thread = Some(thread::current().id());
    loop {
        if let Some(delivery) = state.queued.pop_front() {
            state.running = true;
            drop(state);
            unsafe { (delivery.run)(delivery.context) };
            drop(delivery);
            state = signal.state.lock().unwrap();
            state.running = false;
        } else if state.closed {
            state.exited = true;
            signal.wake.notify_all();
            return;
        } else {
            state = signal.wake.wait(state).unwrap();
        }
    }
}
