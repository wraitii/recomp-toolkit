use super::gpu::D3d9Gpu;
use crate::RenderError;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

pub(crate) const D3DQUERYTYPE_EVENT: u32 = 8;
pub(crate) const D3DQUERYTYPE_OCCLUSION: u32 = 9;

const OCCLUSION_SLOTS: u32 = 4096;
const OCCLUSION_STRIDE: u64 = 8;

pub(crate) enum QueryPoll {
    Pending,
    Ready(u32),
}

struct Readback {
    buffer: wgpu::Buffer,
    mapped: Arc<OnceLock<bool>>,
    areas: Vec<u64>,
    boolean: bool,
}

impl Readback {
    fn count(&self) -> Result<u32, RenderError> {
        let data = self
            .buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|error| RenderError::new("GetData", error.to_string()))?;
        let total: u64 = data
            .chunks_exact(OCCLUSION_STRIDE as usize)
            .zip(&self.areas)
            .map(|(bytes, &area)| {
                let mut word = [0u8; 8];
                word.copy_from_slice(bytes);
                let samples = u64::from_le_bytes(word);
                match (self.boolean, samples) {
                    (true, 0) => 0,
                    (true, _) => area,
                    (false, _) => samples,
                }
            })
            .sum();
        Ok(total.min(u64::from(u32::MAX)) as u32)
    }
}

enum Occlusion {
    Idle,
    Recording {
        areas: Vec<u64>,
        chunks: Vec<Readback>,
    },
    Pending(Vec<Readback>),
    Done(u32),
}

struct QueryRecord {
    kind: u32,
    generation: u64,
    done: Arc<AtomicU64>,
    native_completion: bool,
    occlusion: Occlusion,
}

struct OcclusionPool {
    set: wgpu::QuerySet,
    boolean: bool,
    active: Option<u32>,
}

impl OcclusionPool {
    // The resolve and copy are queued before subsequent draws. The guest can
    // keep the result buffer alive without holding any slots in the query set.
    fn resolve(&self, gpu: &D3d9Gpu, areas: &mut Vec<u64>) -> Result<Readback, RenderError> {
        let count = areas.len() as u32;
        let size = u64::from(count) * OCCLUSION_STRIDE;
        let readback = gpu.validate("ResolveOcclusion", || {
            let resolve = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("d3d9-occlusion-resolve"),
                size,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("d3d9-occlusion-readback"),
                size,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.record(|encoder, _| {
                encoder.resolve_query_set(&self.set, 0..count, &resolve, 0);
                encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, size);
            });
            Ok(readback)
        })?;
        let mapped = Arc::new(OnceLock::new());
        let map_target = readback.clone();
        let flag = Arc::clone(&mapped);
        let physical = gpu.loss().physical_loss();
        gpu.submit_then(move |_| {
            if physical.is_lost() {
                let _ = flag.set(false);
                return;
            }
            map_target.map_async(wgpu::MapMode::Read, .., move |result| {
                let _ = flag.set(result.is_ok());
            });
        });
        Ok(Readback {
            buffer: readback,
            mapped,
            areas: std::mem::take(areas),
            boolean: self.boolean,
        })
    }
}

pub(crate) struct D3d9Queries {
    next_handle: u32,
    next_generation: u64,
    records: BTreeMap<u32, QueryRecord>,
    occlusion: Option<OcclusionPool>,
}

impl D3d9Queries {
    pub fn new() -> Self {
        Self {
            next_handle: 1,
            next_generation: 1,
            records: BTreeMap::new(),
            occlusion: None,
        }
    }

    pub fn create(&mut self, gpu: &D3d9Gpu, kind: u32) -> Result<u32, RenderError> {
        match kind {
            D3DQUERYTYPE_EVENT => {}
            D3DQUERYTYPE_OCCLUSION => self.ensure_occlusion_pool(gpu)?,
            other => {
                return Err(RenderError::new(
                    "CreateQuery",
                    format!("D3DQUERYTYPE {other} is not implemented"),
                ));
            }
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        self.records.insert(
            handle,
            QueryRecord {
                kind,
                generation: 0,
                done: Arc::new(AtomicU64::new(0)),
                native_completion: false,
                occlusion: Occlusion::Idle,
            },
        );
        Ok(handle)
    }

    fn ensure_occlusion_pool(&mut self, gpu: &D3d9Gpu) -> Result<(), RenderError> {
        if self.occlusion.is_some() {
            return Ok(());
        }
        let set = gpu.validate("CreateQuery", || {
            Ok(gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("d3d9-occlusion"),
                ty: wgpu::QueryType::Occlusion,
                count: OCCLUSION_SLOTS,
            }))
        })?;
        self.occlusion = Some(OcclusionPool {
            set,
            boolean: cfg!(target_vendor = "apple"),
            active: None,
        });
        Ok(())
    }

    pub fn destroy(&mut self, handle: u32) -> Result<(), RenderError> {
        self.records
            .remove(&handle)
            .ok_or_else(|| RenderError::invalid("DestroyQuery", "unknown query handle"))?;
        if let Some(pool) = self.occlusion.as_mut() {
            if pool.active == Some(handle) {
                pool.active = None;
            }
        }
        Ok(())
    }

    pub fn query_type(&self, handle: u32) -> Result<u32, RenderError> {
        self.records
            .get(&handle)
            .map(|record| record.kind)
            .ok_or_else(|| RenderError::invalid("GetType", "unknown query handle"))
    }

    pub fn data_size(kind: u32) -> Result<u32, RenderError> {
        match kind {
            D3DQUERYTYPE_EVENT | D3DQUERYTYPE_OCCLUSION => Ok(4),
            other => Err(RenderError::new(
                "GetDataSize",
                format!("D3DQUERYTYPE {other} is not implemented"),
            )),
        }
    }

    pub fn issue(&mut self, gpu: &D3d9Gpu, handle: u32, begin: bool) -> Result<(), RenderError> {
        if gpu.is_blocked() {
            return Err(RenderError::new("Issue", "the device is not operational"));
        }
        let kind = self.kind(handle, "Issue")?;
        match kind {
            D3DQUERYTYPE_EVENT => self.issue_event(gpu, handle, begin),
            D3DQUERYTYPE_OCCLUSION if begin => self.begin_occlusion(gpu, handle),
            D3DQUERYTYPE_OCCLUSION => self.end_occlusion(gpu, handle),
            _ => Err(RenderError::new("Issue", "query type is not implemented")),
        }
    }

    fn kind(&self, handle: u32, operation: &'static str) -> Result<u32, RenderError> {
        self.records
            .get(&handle)
            .map(|record| record.kind)
            .ok_or_else(|| RenderError::invalid(operation, "unknown query handle"))
    }

    fn issue_event(&mut self, gpu: &D3d9Gpu, handle: u32, begin: bool) -> Result<(), RenderError> {
        if begin {
            return Err(RenderError::invalid(
                "Issue",
                "D3DISSUE_BEGIN is invalid for event queries",
            ));
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let record = self
            .records
            .get_mut(&handle)
            .ok_or_else(|| RenderError::invalid("Issue", "unknown query handle"))?;
        let done = Arc::clone(&record.done);
        let native_done = Arc::clone(&done);
        let loss = gpu.loss().clone();
        record.native_completion = gpu.submit_native_completion(move |success| {
            if success {
                native_done.fetch_max(generation, Ordering::AcqRel);
            } else {
                loss.mark_physical("Metal event-query command buffer failed".to_string());
            }
        })?;
        record.generation = generation;
        if record.native_completion {
            return Ok(());
        }
        let physical = gpu.loss().physical_loss();
        gpu.submit_then(move |queue| {
            if physical.is_lost() {
                return;
            }
            queue.submit(std::iter::empty());
            queue.on_submitted_work_done(move || {
                done.fetch_max(generation, Ordering::AcqRel);
            });
        });
        Ok(())
    }

    fn begin_occlusion(&mut self, gpu: &D3d9Gpu, handle: u32) -> Result<(), RenderError> {
        self.ensure_occlusion_pool(gpu)?;
        let pool = self
            .occlusion
            .as_mut()
            .ok_or_else(|| RenderError::invalid("Issue", "occlusion query pool is missing"))?;
        if pool.active.is_some() {
            return Err(RenderError::new(
                "Issue",
                "nested occlusion queries are not implemented",
            ));
        }
        let record = self
            .records
            .get_mut(&handle)
            .ok_or_else(|| RenderError::invalid("Issue", "unknown query handle"))?;
        record.occlusion = Occlusion::Recording {
            areas: Vec::new(),
            chunks: Vec::new(),
        };
        pool.active = Some(handle);
        Ok(())
    }

    fn end_occlusion(&mut self, gpu: &D3d9Gpu, handle: u32) -> Result<(), RenderError> {
        let pool = self
            .occlusion
            .as_mut()
            .ok_or_else(|| RenderError::invalid("Issue", "occlusion query pool is missing"))?;
        if pool.active != Some(handle) {
            return Err(RenderError::invalid(
                "Issue",
                "D3DISSUE_END without a matching D3DISSUE_BEGIN",
            ));
        }
        let record = self
            .records
            .get_mut(&handle)
            .ok_or_else(|| RenderError::invalid("Issue", "unknown query handle"))?;
        let Occlusion::Recording { areas, chunks } = &mut record.occlusion else {
            return Err(RenderError::invalid(
                "Issue",
                "occlusion query is not recording",
            ));
        };
        if !areas.is_empty() {
            chunks.push(pool.resolve(gpu, areas)?);
        }
        record.occlusion = if chunks.is_empty() {
            Occlusion::Done(0)
        } else {
            Occlusion::Pending(std::mem::take(chunks))
        };
        pool.active = None;
        Ok(())
    }

    pub(crate) fn open_segment(
        &mut self,
        gpu: &D3d9Gpu,
        area: u64,
    ) -> Result<Option<u32>, RenderError> {
        let Some(pool) = self.occlusion.as_ref() else {
            return Ok(None);
        };
        let Some(handle) = pool.active else {
            return Ok(None);
        };
        let Some(Occlusion::Recording { areas, chunks }) = self
            .records
            .get_mut(&handle)
            .map(|record| &mut record.occlusion)
        else {
            return Err(RenderError::invalid(
                "DrawFlush",
                "occlusion query is not recording",
            ));
        };
        if areas.len() == OCCLUSION_SLOTS as usize {
            chunks.push(pool.resolve(gpu, areas)?);
        }
        let index = areas.len() as u32;
        areas.push(area);
        Ok(Some(index))
    }

    pub(crate) fn occlusion_set(&self) -> Option<&wgpu::QuerySet> {
        self.occlusion.as_ref().map(|pool| &pool.set)
    }

    pub fn get_data(
        &mut self,
        gpu: &D3d9Gpu,
        handle: u32,
        flush: bool,
    ) -> Result<QueryPoll, RenderError> {
        if gpu.is_blocked() {
            return Err(RenderError::new("GetData", "the device is not operational"));
        }
        let kind = self.kind(handle, "GetData")?;
        match kind {
            D3DQUERYTYPE_EVENT => self.get_event(gpu, handle, flush),
            D3DQUERYTYPE_OCCLUSION => self.get_occlusion(gpu, handle, flush),
            _ => Err(RenderError::new("GetData", "query type is not implemented")),
        }
    }

    fn get_event(
        &mut self,
        gpu: &D3d9Gpu,
        handle: u32,
        flush: bool,
    ) -> Result<QueryPoll, RenderError> {
        let (generation, done, native_completion) = {
            let record = self
                .records
                .get(&handle)
                .ok_or_else(|| RenderError::invalid("GetData", "unknown query handle"))?;
            (record.generation, &record.done, record.native_completion)
        };
        if native_completion {
            return Ok(if done.load(Ordering::Acquire) >= generation {
                QueryPoll::Ready(1)
            } else {
                QueryPoll::Pending
            });
        }
        if let Err(error) = gpu.device.poll(wgpu::PollType::Poll) {
            return Err(RenderError::new("GetData", error.to_string()));
        }
        if generation == 0 || done.load(Ordering::Acquire) >= generation {
            return Ok(QueryPoll::Ready(1));
        }
        if flush {
            gpu.request_flush();
        }
        Ok(QueryPoll::Pending)
    }

    fn get_occlusion(
        &mut self,
        gpu: &D3d9Gpu,
        handle: u32,
        flush: bool,
    ) -> Result<QueryPoll, RenderError> {
        let record = self
            .records
            .get(&handle)
            .ok_or_else(|| RenderError::invalid("GetData", "unknown query handle"))?;
        match record.occlusion {
            Occlusion::Idle => return Ok(QueryPoll::Ready(0)),
            Occlusion::Recording { .. } => return Ok(QueryPoll::Pending),
            Occlusion::Done(value) => return Ok(QueryPoll::Ready(value)),
            Occlusion::Pending(_) => {}
        }
        if flush {
            gpu.queue().submit(std::iter::empty());
        }
        if let Err(error) = gpu.device.poll(wgpu::PollType::Poll) {
            return Err(RenderError::new("GetData", error.to_string()));
        }
        let record = self
            .records
            .get_mut(&handle)
            .ok_or_else(|| RenderError::invalid("GetData", "unknown query handle"))?;
        let Occlusion::Pending(chunks) = &record.occlusion else {
            return Ok(QueryPoll::Pending);
        };
        for chunk in chunks {
            match chunk.mapped.get() {
                None => return Ok(QueryPoll::Pending),
                Some(false) => {
                    return Err(RenderError::new(
                        "GetData",
                        "occlusion readback buffer failed to map",
                    ));
                }
                Some(true) => {}
            }
        }
        let value = chunks.iter().try_fold(0u32, |total, chunk| {
            chunk.count().map(|count| total.saturating_add(count))
        })?;
        for chunk in chunks {
            chunk.buffer.unmap();
        }
        record.occlusion = Occlusion::Done(value);
        Ok(QueryPoll::Ready(value))
    }

    pub fn reset(&mut self, _gpu: &D3d9Gpu) {
        for record in self.records.values_mut() {
            record.generation = 0;
            record.occlusion = Occlusion::Idle;
        }
        self.occlusion = None;
    }
}
