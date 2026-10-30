use std::ffi::c_void;
use std::sync::Mutex;

#[repr(C)]
pub struct Presented {
    pub(crate) context: *mut c_void,
    pub(crate) invoke: unsafe extern "C" fn(*mut c_void, f64),
    pub(crate) release: unsafe extern "C" fn(*mut c_void),
}

impl Drop for Presented {
    fn drop(&mut self) {
        unsafe { (self.release)(self.context) };
    }
}

#[repr(C)]
pub struct Backend {
    pub(crate) context: *mut c_void,
    pub(crate) create: unsafe extern "C" fn(*mut c_void, *mut c_void, i32, i32) -> u64,
    pub(crate) destroy: unsafe extern "C" fn(*mut c_void, u64),
    pub(crate) resize: unsafe extern "C" fn(*mut c_void, u64, i32, i32),
    pub(crate) acquire: unsafe extern "C" fn(*mut c_void, u64) -> u64,
    pub(crate) release: unsafe extern "C" fn(*mut c_void, u64, u64),
    pub(crate) present: unsafe extern "C" fn(*mut c_void, u64, u64, u64, f64, Presented),
    pub(crate) refresh: unsafe extern "C" fn(*mut c_void, u64) -> f64,
}

#[derive(Clone, Copy)]
struct Event {
    native: *mut c_void,
    width: i32,
    height: i32,
    viewport: i32,
    install: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Transition {
    pub installed: u8,
    pub changed: u8,
}

struct State {
    pending: Option<Event>,
    native: *mut c_void,
    viewport: i32,
    chain: u64,
    closed: bool,
}

pub struct Surface {
    backend: Backend,
    enabled: bool,
    state: Mutex<State>,
}

impl Surface {
    pub fn new(backend: Backend, enabled: bool) -> Self {
        Self {
            backend,
            enabled,
            state: Mutex::new(State {
                pending: None,
                native: std::ptr::null_mut(),
                viewport: -1,
                chain: 0,
                closed: false,
            }),
        }
    }

    pub fn install(&self, native: *mut c_void, width: i32, height: i32, viewport: i32) {
        let mut state = self.state.lock().unwrap();
        if !state.closed {
            state.pending = Some(Event {
                native,
                width,
                height,
                viewport,
                install: true,
            });
        }
    }

    pub fn resize(&self, width: i32, height: i32) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        let event = state.pending.get_or_insert(Event {
            native: std::ptr::null_mut(),
            width,
            height,
            viewport: -1,
            install: false,
        });
        event.width = width;
        event.height = height;
    }

    pub fn pending(&self) -> bool {
        self.state.lock().unwrap().pending.is_some()
    }

    pub fn viewport(&self) -> i32 {
        self.state.lock().unwrap().viewport
    }

    fn chain(&self) -> u64 {
        self.state.lock().unwrap().chain
    }

    pub fn attached(&self) -> bool {
        self.chain() != 0
    }

    pub fn apply(&self) -> Transition {
        let event = self.state.lock().unwrap().pending.take();
        let Some(event) = event else {
            return Transition::default();
        };
        if event.install {
            let (chain, changed) = {
                let mut state = self.state.lock().unwrap();
                let chain = std::mem::take(&mut state.chain);
                let changed = state.native != event.native;
                state.native = event.native;
                state.viewport = event.viewport;
                (chain, changed)
            };
            if chain != 0 {
                unsafe { (self.backend.destroy)(self.backend.context, chain) };
            }
            let chain = if self.enabled && !event.native.is_null() {
                unsafe {
                    (self.backend.create)(
                        self.backend.context,
                        event.native,
                        event.width,
                        event.height,
                    )
                }
            } else {
                0
            };
            self.state.lock().unwrap().chain = chain;
            Transition {
                installed: 1,
                changed: u8::from(changed),
            }
        } else {
            let chain = self.chain();
            if self.enabled && chain != 0 {
                unsafe {
                    (self.backend.resize)(self.backend.context, chain, event.width, event.height)
                };
            }
            Transition::default()
        }
    }

    pub fn acquire(&self, suspended: bool) -> u64 {
        let chain = self.chain();
        if suspended || chain == 0 {
            return 0;
        }
        unsafe { (self.backend.acquire)(self.backend.context, chain) }
    }

    pub fn release(&self, drawable: u64) {
        if drawable != 0 {
            let chain = self.chain();
            assert!(chain != 0, "presenter: drawable release without surface");
            unsafe { (self.backend.release)(self.backend.context, chain, drawable) };
        }
    }

    pub fn present(&self, command: u64, drawable: u64, duration: f64, callback: Presented) {
        let chain = self.chain();
        assert!(
            chain != 0 && drawable != 0,
            "presenter: presentation without drawable surface"
        );
        unsafe {
            (self.backend.present)(
                self.backend.context,
                chain,
                command,
                drawable,
                duration,
                callback,
            )
        };
    }

    pub fn refresh(&self, fallback: f64) -> f64 {
        let chain = self.chain();
        let period = if chain != 0 {
            unsafe { (self.backend.refresh)(self.backend.context, chain) }
        } else {
            fallback
        };
        if period > 0.0 && period.is_finite() {
            period
        } else {
            fallback
        }
    }

    pub fn adopt(&self, chain: u64) {
        let previous = {
            let mut state = self.state.lock().unwrap();
            if state.chain == chain {
                return;
            }
            std::mem::replace(&mut state.chain, chain)
        };
        if previous != 0 {
            unsafe { (self.backend.destroy)(self.backend.context, previous) };
        }
    }

    pub fn close(&self) {
        let chain = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.pending = None;
            std::mem::take(&mut state.chain)
        };
        if chain != 0 {
            unsafe { (self.backend.destroy)(self.backend.context, chain) };
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.close();
    }
}
