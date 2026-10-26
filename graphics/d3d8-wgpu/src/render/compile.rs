use super::pipeline::{CompilationCache, CompiledPipeline, PipelineRecipe, PipelineRecipeKey};
use crate::RenderError;
use crate::fast_map::FastMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub(crate) struct Limits {
    pub(crate) queued: usize,
    pub(crate) recipe_bytes: usize,
}

struct Job {
    key: PipelineRecipeKey,
    recipe: PipelineRecipe,
    queued: Instant,
    frame: u64,
}

enum Completion {
    Started(PipelineRecipeKey),
    Finished(PipelineRecipe, Result<CompiledPipeline, RenderError>),
}

enum Entry {
    Queued,
    Compiling,
    Failed(RenderError),
}

struct Tracked {
    entry: Entry,
    bytes: usize,
    queued: Instant,
    first_frame: u64,
    fallback_draws: u64,
}

pub(crate) struct Compiler {
    epoch: u64,
    trace: bool,
    jobs: Option<mpsc::SyncSender<Job>>,
    completed: mpsc::Receiver<Completion>,
    stopped: Arc<AtomicBool>,
    entries: FastMap<PipelineRecipeKey, Tracked>,
    bytes: usize,
    limits: Limits,
    queue_full: u64,
    memory_full: u64,
}

impl Compiler {
    pub(crate) fn new(
        device: wgpu::Device,
        limits: Limits,
        trace: bool,
    ) -> Result<Self, RenderError> {
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
                let mut cache = CompilationCache::default();
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
                    if trace {
                        eprintln!(
                            "[d3d9-async] compiling epoch={epoch} recipe={:016x} frame={} queue_ms={:.3}",
                            job.key.fingerprint(), job.frame,
                            started.duration_since(job.queued).as_secs_f64() * 1000.0
                        );
                    }
                    let result = job.recipe.compile(&device, &mut cache);
                    if trace || result.is_err() {
                        eprintln!(
                            "[d3d9-async] completed epoch={epoch} recipe={:016x} frame={} ok={} compile_ms={:.3} error={:?}",
                            job.key.fingerprint(), job.frame, result.is_ok(),
                            started.elapsed().as_secs_f64() * 1000.0,
                            result.as_ref().err()
                        );
                    }
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
                            .send(Completion::Finished(job.recipe, result))
                            .is_err()
                    {
                        if trace {
                            eprintln!("[d3d9-async] retired-completion epoch={epoch}");
                        }
                        break;
                    }
                }
            })
            .map_err(|error| RenderError::new("SpecializationWorker", error.to_string()))?;
        if trace {
            eprintln!(
                "[d3d9-async] prepare epoch={epoch} queued_limit={} recipe_bytes_limit={}",
                limits.queued, limits.recipe_bytes
            );
        }
        Ok(Self {
            epoch,
            trace,
            jobs: Some(jobs),
            completed: completions,
            stopped,
            entries: FastMap::default(),
            bytes: 0,
            limits,
            queue_full: 0,
            memory_full: 0,
        })
    }

    pub(crate) fn end_frame(&mut self, frame: u64) {
        if !self.trace {
            self.queue_full = 0;
            self.memory_full = 0;
            return;
        }
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
        if self.trace {
            eprintln!(
                "[d3d9-async] frame epoch={epoch} frame={frame} queued={queued} compiling={compiling} failed={failed} recipe_bytes_estimate={} queue_full={} memory_full={}",
                self.bytes, self.queue_full, self.memory_full
            );
        }
        self.queue_full = 0;
        self.memory_full = 0;
    }

    pub(crate) fn publish(
        &mut self,
        frame: u64,
    ) -> Result<Vec<(PipelineRecipe, CompiledPipeline)>, RenderError> {
        let epoch = self.epoch;
        let mut ready = Vec::new();
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
                Completion::Finished(recipe, result) => {
                    let key = recipe.key();
                    let fatal = result
                        .as_ref()
                        .err()
                        .filter(|error| error.kind == crate::RenderErrorKind::OutOfMemory)
                        .cloned();
                    if let Some(mut tracked) = self.entries.remove(&key) {
                        self.bytes -= tracked.bytes;
                        if self.trace {
                            eprintln!(
                                "[d3d9-async] published epoch={epoch} recipe={:016x} frame={frame} requested_frame={} ok={} elapsed_ms={:.3} fallback_frames={} fallback_draws={}",
                                key.fingerprint(),
                                tracked.first_frame,
                                result.is_ok(),
                                tracked.queued.elapsed().as_secs_f64() * 1000.0,
                                frame.saturating_sub(tracked.first_frame),
                                tracked.fallback_draws
                            );
                        }
                        match result {
                            Ok(compiled) => ready.push((recipe, compiled)),
                            Err(error) => {
                                tracked.entry = Entry::Failed(error);
                                tracked.bytes = 0;
                                self.entries.insert(key, tracked);
                            }
                        }
                    }
                    if let Some(error) = fatal {
                        return Err(error);
                    }
                }
            }
        }
        Ok(ready)
    }

    pub(crate) fn request(
        &mut self,
        key: &PipelineRecipeKey,
        recipe: &PipelineRecipe,
        frame: u64,
    ) -> Result<(), RenderError> {
        let epoch = self.epoch;
        if let Some(tracked) = self.entries.get_mut(key) {
            return match &tracked.entry {
                Entry::Failed(error) if error.kind == crate::RenderErrorKind::OutOfMemory => {
                    Err(error.clone())
                }
                Entry::Failed(_) | Entry::Queued | Entry::Compiling => {
                    tracked.fallback_draws += 1;
                    Ok(())
                }
            };
        }
        let bytes = recipe.retained_bytes() * 2
            + std::mem::size_of::<Tracked>()
            + std::mem::size_of::<PipelineRecipeKey>();
        if bytes > self.limits.recipe_bytes.saturating_sub(self.bytes) {
            self.memory_full += 1;
            return Ok(());
        }
        let Some(jobs) = &self.jobs else {
            return Ok(());
        };
        let queued = Instant::now();
        match jobs.try_send(Job {
            key: key.clone(),
            recipe: recipe.clone(),
            queued,
            frame,
        }) {
            Ok(()) => {
                self.bytes += bytes;
                if self.trace {
                    eprintln!(
                        "[d3d9-async] queued epoch={epoch} recipe={:016x} frame={frame} entries={} recipe_bytes_estimate={}",
                        key.fingerprint(),
                        self.entries.len() + 1,
                        self.bytes
                    );
                }
                self.entries.insert(
                    key.clone(),
                    Tracked {
                        entry: Entry::Queued,
                        bytes,
                        queued,
                        first_frame: frame,
                        fallback_draws: 1,
                    },
                );
                Ok(())
            }
            Err(mpsc::TrySendError::Full(_)) => {
                self.queue_full += 1;
                Ok(())
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
        if self.trace {
            eprintln!(
                "[d3d9-async] retire epoch={} entries={}",
                self.epoch,
                self.entries.len()
            );
        }
        self.stopped.store(true, Ordering::Release);
        self.jobs = None;
    }
}
