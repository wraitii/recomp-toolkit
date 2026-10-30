use recomp_compositor::{
    Compositor, Element, Input, OUTPUT_DRAWS, Source, gpu::GpuCompositor, surface::SurfacePresenter,
};
use recomp_gpu::{SharedContext, wgpu};
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

struct Host {
    window: Arc<Window>,
    surface: SurfacePresenter,
    context: Arc<SharedContext>,
    compositor: GpuCompositor,
    policy: Compositor,
    image: wgpu::TextureView,
}
impl Host {
    fn new(window: Arc<Window>) -> Result<Self, Box<dyn std::error::Error>> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(Arc::clone(&window))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;
        let context = Arc::new(SharedContext::new(adapter, device, queue)?);
        let loss = context.loss.clone();
        context
            .device
            .set_device_lost_callback(move |reason, message| {
                loss.mark(format!("{reason:?}: {message}"))
            });
        let size = window.inner_size();
        let surface =
            SurfacePresenter::new(Arc::clone(&context), surface, size.width, size.height)?;
        let format = surface.format();
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("independent host UI"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        context.queue.write_texture(
            texture.as_image_copy(),
            &[50, 190, 235, 220],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let image = texture.create_view(&Default::default());
        let compositor = GpuCompositor::new(Arc::clone(&context), format)?;
        Ok(Self {
            window,
            surface,
            context,
            compositor,
            policy: Compositor::default(),
            image,
        })
    }
    fn resize(&mut self, width: u32, height: u32) {
        if let Err(error) = self.surface.resize(width, height) {
            eprintln!("compositor host: {error}");
        }
        self.window.request_redraw();
    }
    fn draw(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(texture) = self.surface.acquire()? else {
            return Ok(());
        };
        let view = texture.create_view(&Default::default());
        let (width, height) = self.surface.size();
        let input = Input {
            cls: 0,
            guest_w: 640,
            guest_h: 480,
            drawable_w: width as i32,
            drawable_h: height as i32,
            ..Default::default()
        };
        let element = Element {
            id: 1,
            x: 80,
            y: 80,
            w: 480,
            h: 320,
            ..Default::default()
        };
        let plan = self.policy.build(&input, &[element], OUTPUT_DRAWS);
        let mut encoder = self
            .context
            .device
            .create_command_encoder(&Default::default());
        self.compositor
            .encode(&mut encoder, &view, [width, height], &plan.draws, |draw| {
                matches!(draw.source, Source::Ui)
                    .then(|| (self.image.clone(), wgpu::FilterMode::Nearest))
            })?;
        self.context.queue.submit([encoder.finish()]);
        let frame = self
            .surface
            .take_frame()
            .ok_or("surface: missing drawable")?;
        self.surface.present(frame)?;
        Ok(())
    }
}
#[derive(Default)]
struct App {
    host: Option<Host>,
    failed: bool,
    frames: Option<u32>,
    presented: u32,
}
impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.host.is_some() {
            return;
        }
        let result = event_loop
            .create_window(Window::default_attributes().with_title("Independent compositor host"))
            .map(Arc::new)
            .map_err(|e| e.to_string())
            .and_then(|window| Host::new(window).map_err(|e| e.to_string()));
        match result {
            Ok(host) => {
                host.window.request_redraw();
                self.host = Some(host);
            }
            Err(error) => {
                eprintln!("compositor host: {error}");
                self.failed = true;
                event_loop.exit();
            }
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => host.resize(size.width, size.height),
            WindowEvent::RedrawRequested => {
                if let Err(error) = host.draw() {
                    eprintln!("compositor host: {error}");
                    self.failed = true;
                    event_loop.exit();
                } else {
                    self.presented += 1;
                    if self.frames.is_some_and(|count| self.presented >= count) {
                        event_loop.exit();
                    } else if self.frames.is_some() {
                        host.window.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let frames = match args.as_slice() {
        [] => None,
        [flag, count] if flag == "--frames" => {
            let count: u32 = count.parse()?;
            if count == 0 {
                return Err("--frames must be positive".into());
            }
            Some(count)
        }
        _ => return Err("usage: compositor-host [--frames N]".into()),
    };
    let event_loop = EventLoop::new()?;
    let mut app = App {
        frames,
        ..Default::default()
    };
    event_loop.run_app(&mut app)?;
    if app.failed {
        return Err("compositor host failed".into());
    }
    Ok(())
}
