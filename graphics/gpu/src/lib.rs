use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Clone, Default)]
pub struct PhysicalLoss {
    state: Arc<LossState>,
}
#[derive(Default)]
struct LossState {
    lost: AtomicBool,
    reason: Mutex<Option<String>>,
}
impl PhysicalLoss {
    pub fn same_context(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
    pub fn mark(&self, reason: String) {
        if let Ok(mut slot) = self.state.reason.lock() {
            *slot = Some(reason);
        }
        self.state.lost.store(true, Ordering::Release);
    }
    pub fn is_lost(&self) -> bool {
        self.state.lost.load(Ordering::Acquire)
    }
    pub fn reason(&self) -> Option<String> {
        self.state.reason.lock().ok().and_then(|slot| slot.clone())
    }
}

pub use wgpu;

/// Shared physical device and queue. A replacement is a new context with a new generation.
pub struct SharedContext {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_info: wgpu::AdapterInfo,
    pub loss: PhysicalLoss,
    pub generation: u64,
}

impl SharedContext {
    /// Wrap caller-owned handles without installing callbacks. The caller must ensure
    /// the device and queue came from this adapter, import each device only once,
    /// and serialize direct queue access with clients. Its device-loss callback must
    /// mark `loss`; retain that token when installing or changing the callback.
    pub fn new(
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Result<Self, &'static str> {
        if !adapter.features().contains(device.features())
            || !device.limits().check_limits(&adapter.limits())
        {
            return Err("device capabilities exceed adapter capabilities");
        }
        let adapter_info = adapter.get_info();
        static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = NEXT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .map_err(|_| "GPU context generations exhausted")?;
        Ok(Self {
            adapter,
            device,
            queue,
            adapter_info,
            loss: PhysicalLoss::default(),
            generation,
        })
    }
}
