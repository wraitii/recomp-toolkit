use super::pipeline::{CompiledPipeline, PipelineRecipe, PipelineRecipeKey};
use crate::RenderError;
use crate::fast_map::FastMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub(crate) struct Limits {
    pub(crate) queued: usize,
    pub(crate) recipe_bytes: usize,
    pub(crate) ready: usize,
}

struct Job {
    key: PipelineRecipeKey,
    recipe: PipelineRecipe,
    queued: Instant,
}

enum Completion {
    Started(PipelineRecipeKey),
    Finished(PipelineRecipeKey, Result<CompiledPipeline, RenderError>),
}

enum Entry {
    Queued,
    Compiling,
    Ready(CompiledPipeline),
    Failed(RenderError),
}

struct Tracked {
    entry: Entry,
    bytes: usize,
    queued: Instant,
    first_frame: u64,
    fallback_draws: u64,
    selected: bool,
}

pub(crate) struct Compiler {
    epoch: u64,
    jobs: Option<mpsc::SyncSender<Job>>,
    completed: mpsc::Receiver<Completion>,
    stopped: Arc<AtomicBool>,
    entries: FastMap<PipelineRecipeKey, Tracked>,
    order: std::collections::VecDeque<PipelineRecipeKey>,
    bytes: usize,
    limits: Limits,
    queue_full: u64,
    memory_full: u64,
}

impl Compiler {
    pub(crate) fn new(device: wgpu::Device, limits: Limits) -> Result<Self, RenderError> {
        let delay_ms = match std::env::var("RECOMP_D3D9_ASYNC_DELAY_MS") {
            Ok(value) => value.parse::<u64>().map_err(|_| {
                RenderError::invalid(
                    "SpecializationWorker",
                    "RECOMP_D3D9_ASYNC_DELAY_MS must be an unsigned millisecond count",
                )
            })?,
            Err(std::env::VarError::NotPresent) => 0,
            Err(error) => {
                return Err(RenderError::invalid(
                    "SpecializationWorker",
                    error.to_string(),
                ));
            }
        };
        static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
        let epoch = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
        let (jobs, received) = mpsc::sync_channel::<Job>(limits.queued);
        let (completed, completions) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        std::thread::Builder::new()
            .name("shader-compiler".into())
            .spawn(move || {
                for job in received {
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if completed
                        .send(Completion::Started(job.key.clone()))
                        .is_err()
                    {
                        break;
                    }
                    let started = Instant::now();
                    eprintln!(
                        "[d3d9-async] compiling epoch={epoch} key={:?} queue_ms={:.3}",
                        job.key,
                        started.duration_since(job.queued).as_secs_f64() * 1000.0
                    );
                    let result = job.recipe.compile(&device);
                    eprintln!(
                        "[d3d9-async] completed epoch={epoch} key={:?} compile_ms={:.3} error={:?}",
                        job.key,
                        started.elapsed().as_secs_f64() * 1000.0,
                        result.as_ref().err()
                    );
                    if result.is_ok() && delay_ms != 0 {
                        eprintln!("[d3d9-async] debug-delay epoch={epoch} delay_ms={delay_ms}");
                        let started = Instant::now();
                        let delay = Duration::from_millis(delay_ms);
                        while started.elapsed() < delay && !worker_stopped.load(Ordering::Acquire) {
                            std::thread::sleep(
                                delay
                                    .saturating_sub(started.elapsed())
                                    .min(Duration::from_millis(50)),
                            );
                        }
                    }
                    if worker_stopped.load(Ordering::Acquire)
                        || completed
                            .send(Completion::Finished(job.key, result))
                            .is_err()
                    {
                        eprintln!("[d3d9-async] retired-completion epoch={epoch}");
                        break;
                    }
                }
            })
            .map_err(|error| RenderError::new("SpecializationWorker", error.to_string()))?;
        eprintln!(
            "[d3d9-async] prepare epoch={epoch} queued_limit={} recipe_bytes_limit={} ready_limit={}",
            limits.queued, limits.recipe_bytes, limits.ready
        );
        Ok(Self {
            epoch,
            jobs: Some(jobs),
            completed: completions,
            stopped,
            entries: FastMap::default(),
            order: Default::default(),
            bytes: 0,
            limits,
            queue_full: 0,
            memory_full: 0,
        })
    }

    pub(crate) fn end_frame(&mut self, frame: u64) {
        let epoch = self.epoch;
        let queued = self
            .entries
            .values()
            .filter(|t| matches!(t.entry, Entry::Queued))
            .count();
        let compiling = self
            .entries
            .values()
            .filter(|t| matches!(t.entry, Entry::Compiling))
            .count();
        let failed = self
            .entries
            .values()
            .filter(|t| matches!(t.entry, Entry::Failed(_)))
            .count();
        eprintln!(
            "[d3d9-async] frame epoch={epoch} frame={frame} queued={queued} compiling={compiling} ready={} failed={failed} recipe_bytes_estimate={} queue_full={} memory_full={}",
            self.order.len(),
            self.bytes,
            self.queue_full,
            self.memory_full
        );
        self.queue_full = 0;
        self.memory_full = 0;
    }

    pub(crate) fn publish(&mut self) -> Result<bool, RenderError> {
        let epoch = self.epoch;
        let mut changed = false;
        loop {
            let completion = match self.completed.try_recv() {
                Ok(completion) => completion,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(RenderError::new(
                        "SpecializationWorker",
                        "compiler worker exited",
                    ));
                }
            };
            match completion {
                Completion::Started(key) => {
                    if let Some(tracked) = self.entries.get_mut(&key) {
                        tracked.entry = Entry::Compiling;
                    }
                }
                Completion::Finished(key, result) => {
                    let fatal = result
                        .as_ref()
                        .err()
                        .filter(|error| error.kind == crate::RenderErrorKind::OutOfMemory)
                        .cloned();
                    if let Some(tracked) = self.entries.get_mut(&key) {
                        tracked.entry = match result {
                            Ok(compiled) => Entry::Ready(compiled),
                            Err(error) => Entry::Failed(error),
                        };
                        if matches!(tracked.entry, Entry::Ready(_)) {
                            self.order.push_back(key.clone());
                        }
                        eprintln!(
                            "[d3d9-async] published epoch={epoch} key={key:?} elapsed_ms={:.3}",
                            tracked.queued.elapsed().as_secs_f64() * 1000.0
                        );
                        changed = true;
                    }
                    if let Some(error) = fatal {
                        return Err(error);
                    }
                }
            }
        }
        while self.order.len() > self.limits.ready {
            if let Some(key) = self.order.pop_front() {
                if let Some(tracked) = self.entries.remove(&key) {
                    self.bytes -= tracked.bytes;
                    eprintln!("[d3d9-async] evicted epoch={epoch} key={key:?}");
                }
            }
        }
        Ok(changed)
    }

    pub(crate) fn request(
        &mut self,
        recipe: PipelineRecipe,
        frame: u64,
    ) -> Result<Option<CompiledPipeline>, RenderError> {
        let epoch = self.epoch;
        let key = recipe.key();
        if let Some(tracked) = self.entries.get_mut(&key) {
            if matches!(tracked.entry, Entry::Ready(_)) {
                self.order.retain(|existing| existing != &key);
                self.order.push_back(key.clone());
            }
            return match &tracked.entry {
                Entry::Ready(compiled) => {
                    if !tracked.selected {
                        eprintln!(
                            "[d3d9-async] selected epoch={epoch} key={key:?} fallback_ms={:.3} fallback_frames={} fallback_draws={}",
                            tracked.queued.elapsed().as_secs_f64() * 1000.0,
                            frame.saturating_sub(tracked.first_frame),
                            tracked.fallback_draws
                        );
                        tracked.selected = true;
                    }
                    Ok(Some(compiled.clone()))
                }
                Entry::Failed(error) if error.kind == crate::RenderErrorKind::OutOfMemory => {
                    Err(error.clone())
                }
                Entry::Failed(_) | Entry::Queued | Entry::Compiling => {
                    tracked.fallback_draws += 1;
                    Ok(None)
                }
            };
        }
        let bytes = recipe.retained_bytes() * 2
            + std::mem::size_of::<Tracked>()
            + std::mem::size_of::<PipelineRecipeKey>();
        if bytes > self.limits.recipe_bytes.saturating_sub(self.bytes) {
            self.memory_full += 1;
            return Ok(None);
        }
        let Some(jobs) = &self.jobs else {
            return Ok(None);
        };
        let queued = Instant::now();
        match jobs.try_send(Job {
            key: key.clone(),
            recipe,
            queued,
        }) {
            Ok(()) => {
                self.bytes += bytes;
                eprintln!(
                    "[d3d9-async] queued epoch={epoch} key={key:?} entries={} recipe_bytes_estimate={}",
                    self.entries.len() + 1,
                    self.bytes
                );
                self.entries.insert(
                    key,
                    Tracked {
                        entry: Entry::Queued,
                        bytes,
                        queued,
                        first_frame: frame,
                        fallback_draws: 1,
                        selected: false,
                    },
                );
                Ok(None)
            }
            Err(mpsc::TrySendError::Full(_)) => {
                self.queue_full += 1;
                Ok(None)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => Err(RenderError::new(
                "SpecializationWorker",
                "compiler worker exited",
            )),
        }
    }
}

impl Drop for Compiler {
    fn drop(&mut self) {
        eprintln!(
            "[d3d9-async] retire epoch={} entries={}",
            self.epoch,
            self.entries.len()
        );
        self.stopped.store(true, Ordering::Release);
        self.jobs = None;
    }
}
