use std::{
    ffi::c_void,
    sync::{Arc, atomic::AtomicBool},
};

pub(super) struct SurfaceState {
    presenter: recomp_compositor::surface::SurfacePresenter,
    pub(super) drawable: Option<u64>,
    pub(super) cancelled: Arc<AtomicBool>,
}

impl SurfaceState {
    #[cfg(target_os = "macos")]
    unsafe fn create_surface(
        instance: &wgpu::Instance,
        native_surface: *mut c_void,
    ) -> Result<wgpu::Surface<'static>, String> {
        if native_surface.is_null() {
            return Err("surface: null CoreAnimation layer".into());
        }
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                native_surface,
            ))
        }
        .map_err(|e| format!("surface: creation failed: {e}"))?;
        Ok(surface)
    }

    #[cfg(not(target_os = "macos"))]
    unsafe fn create_surface(
        instance: &wgpu::Instance,
        native_surface: *mut c_void,
    ) -> Result<wgpu::Surface<'static>, String> {
        use raw_window_handle::{
            RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
            Win32WindowHandle, WindowsDisplayHandle, XlibDisplayHandle, XlibWindowHandle,
        };
        use std::num::NonZeroIsize;
        use std::ptr::NonNull;
        #[repr(C)]
        struct WindowSurface {
            kind: u32,
            window: *mut c_void,
            display: *mut c_void,
            id: u64,
            screen: i32,
        }
        let desc = unsafe { native_surface.cast::<WindowSurface>().as_ref() }
            .ok_or("surface: null SDL window descriptor")?;
        let (display, window) = match desc.kind {
            1 => {
                let hwnd = NonZeroIsize::new(desc.window as isize).ok_or("surface: null HWND")?;
                let mut window = Win32WindowHandle::new(hwnd);
                window.hinstance = NonZeroIsize::new(desc.display as isize);
                (
                    RawDisplayHandle::Windows(WindowsDisplayHandle::new()),
                    RawWindowHandle::Win32(window),
                )
            }
            2 => {
                let display = NonNull::new(desc.display).ok_or("surface: null Wayland display")?;
                let window = NonNull::new(desc.window).ok_or("surface: null Wayland surface")?;
                (
                    RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
                    RawWindowHandle::Wayland(WaylandWindowHandle::new(window)),
                )
            }
            3 => {
                let display = NonNull::new(desc.display).ok_or("surface: null X11 display")?;
                let window = desc
                    .id
                    .try_into()
                    .map_err(|_| "surface: invalid X11 window")?;
                (
                    RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(display), desc.screen)),
                    RawWindowHandle::Xlib(XlibWindowHandle::new(window)),
                )
            }
            _ => return Err("surface: unsupported SDL window handle".into()),
        };
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(display),
                raw_window_handle: window,
            })
        }
        .map_err(|e| format!("surface: creation failed: {e}"))?;
        Ok(surface)
    }

    pub(super) unsafe fn new(
        instance: &wgpu::Instance,
        context: Arc<recomp_gpu::SharedContext>,
        native_surface: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let surface = unsafe { Self::create_surface(instance, native_surface) }?;
        let presenter =
            recomp_compositor::surface::SurfacePresenter::new(context, surface, width, height)?;
        let cancelled = presenter.cancelled_token();
        Ok(Self {
            presenter,
            drawable: None,
            cancelled,
        })
    }

    pub(super) fn format(&self) -> wgpu::TextureFormat {
        self.presenter.format()
    }
    pub(super) fn resize(&mut self, _device: &wgpu::Device, width: u32, height: u32) {
        if let Err(error) = self.presenter.resize(width, height) {
            eprintln!("host GPU surface resize: {error}");
        }
    }
    pub(super) fn finish(&mut self, _device: &wgpu::Device) {
        self.presenter.finish();
    }
    pub(super) fn acquire(
        &mut self,
        _device: &wgpu::Device,
    ) -> Result<Option<wgpu::Texture>, String> {
        self.presenter.acquire()
    }
    pub(super) fn present(&mut self, frame: recomp_compositor::surface::SurfaceFrame) {
        if let Err(error) = self.presenter.present(frame) {
            eprintln!("host GPU surface present: {error}");
        }
    }
    pub(super) fn cancel(&mut self) {
        self.presenter.cancel();
    }
    pub(super) fn release(&mut self) {
        self.presenter.release();
    }
    pub(super) fn take_frame(&mut self) -> Option<recomp_compositor::surface::SurfaceFrame> {
        self.drawable = None;
        self.presenter.take_frame()
    }
}
