use recomp_gpu::{wgpu, SharedContext};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct SurfaceFrame {
    frame: Option<wgpu::SurfaceTexture>,
    active: Arc<AtomicBool>,
    _surface: Arc<wgpu::Surface<'static>>,
}

impl Drop for SurfaceFrame {
    fn drop(&mut self) {
        drop(self.frame.take());
        self.active.store(false, Ordering::Release);
    }
}

pub struct SurfacePresenter {
    context: Arc<SharedContext>,
    surface: Arc<wgpu::Surface<'static>>,
    config: wgpu::SurfaceConfiguration,
    frame: Option<wgpu::SurfaceTexture>,
    suspended: bool,
    active: Arc<AtomicBool>,
    pending_size: Option<(u32, u32)>,
    cancelled: Arc<AtomicBool>,
}

impl SurfacePresenter {
    pub fn new(
        context: Arc<SharedContext>,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        if context.loss.is_lost() {
            return Err("surface: physical device lost".into());
        }
        let caps = surface.get_capabilities(&context.adapter);
        let mut config = surface
            .get_default_config(&context.adapter, width.max(1), height.max(1))
            .ok_or("surface: adapter does not support presentation")?;
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST;
        if !caps.usages.contains(usage) {
            return Err("surface: adapter does not support drawable copy destinations".into());
        }
        config.usage = usage;
        config.format = caps
            .formats
            .iter()
            .copied()
            .find(|format| *format == wgpu::TextureFormat::Bgra8Unorm)
            .unwrap_or(config.format);
        if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            config.present_mode = wgpu::PresentMode::Fifo;
        }
        let suspended = width == 0 || height == 0;
        if !suspended {
            surface.configure(&context.device, &config);
        }
        Ok(Self {
            context,
            surface: Arc::new(surface),
            config,
            frame: None,
            suspended,
            active: Arc::new(AtomicBool::new(false)),
            pending_size: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }
    pub fn cancelled_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }
    pub fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.frame = None;
        self.pending_size = None;
    }
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), &'static str> {
        if self.context.loss.is_lost() {
            return Err("surface: physical device lost");
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.frame.is_some() || self.active.load(Ordering::Acquire) {
            self.pending_size = Some((width, height));
            return Ok(());
        }
        self.pending_size = None;
        self.suspended = width == 0 || height == 0;
        if !self.suspended {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.context.device, &self.config);
        }
        Ok(())
    }
    pub fn finish(&mut self) {
        if self.frame.is_some() || self.active.load(Ordering::Acquire) {
            return;
        }
        if let Some((width, height)) = self.pending_size.take() {
            let _ = self.resize(width, height);
        }
    }
    pub fn acquire(&mut self) -> Result<Option<wgpu::Texture>, String> {
        if self.context.loss.is_lost() {
            return Err("surface: physical device lost".into());
        }
        self.finish();
        if self.cancelled.load(Ordering::Acquire)
            || self.suspended
            || self.active.load(Ordering::Acquire)
        {
            return Ok(None);
        }
        if self.frame.is_some() {
            return Err("surface: drawable already acquired".into());
        }
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                let texture = frame.texture.clone();
                self.frame = Some(frame);
                Ok(Some(texture))
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.context.device, &self.config);
                Ok(None)
            }
            wgpu::CurrentSurfaceTexture::Lost => Err("surface: lost".into()),
            wgpu::CurrentSurfaceTexture::Validation => {
                Err("surface: acquisition validation failed".into())
            }
        }
    }
    pub fn release(&mut self) {
        self.frame = None;
        self.finish();
    }
    pub fn present(&mut self, mut frame: SurfaceFrame) -> Result<(), &'static str> {
        if !Arc::ptr_eq(&self.active, &frame.active) {
            return Err("surface: frame belongs to another presenter");
        }
        if self.context.loss.is_lost() {
            return Err("surface: physical device lost");
        }
        let texture = frame
            .frame
            .take()
            .ok_or("surface: frame already consumed")?;
        if self.cancelled.load(Ordering::Acquire) {
            drop(texture);
        } else {
            self.context.queue.present(texture);
        }
        drop(frame);
        self.finish();
        Ok(())
    }
    pub fn take_frame(&mut self) -> Option<SurfaceFrame> {
        let frame = self.frame.take()?;
        self.active.store(true, Ordering::Release);
        Some(SurfaceFrame {
            frame: Some(frame),
            active: Arc::clone(&self.active),
            _surface: Arc::clone(&self.surface),
        })
    }
}
