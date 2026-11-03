// Fallback renderer for non-macOS platforms (Windows, Linux).
// Uses softbuffer: allocates one pixel buffer per frame, hands it to the OS
// compositor.  Memory characteristics differ from the IOSurface backend;
// see platform/macos.rs for the low-overhead path.
//
// The public API mirrors platform/macos.rs so main.rs compiles unchanged on every
// platform. The GPU/CPU distinction and the double-buffer / drawable-count knobs
// are macOS-only concepts; here they are accepted and ignored (softbuffer always
// presents through the platform compositor, and we never allocate an extra
// framebuffer of our own).

use std::num::NonZeroU32;
use std::sync::Arc;
use winit::window::Window;

pub struct Renderer {
    _ctx:  softbuffer::Context<Arc<Window>>,
    surf:  softbuffer::Surface<Arc<Window>, Arc<Window>>,
    width:  u32,
    height: u32,
}

impl Renderer {
    fn new(window: Arc<Window>) -> Self {
        let ctx  = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surf = softbuffer::Surface::new(&ctx, window).expect("softbuffer surface");
        Renderer { _ctx: ctx, surf, width: 0, height: 0 }
    }

    /// CPU renderer. `double_buffer` is a macOS IOSurface option; ignored here.
    pub fn new_cpu(window: &Arc<Window>, _double_buffer: bool) -> Self {
        Self::new(Arc::clone(window))
    }

    /// There is no separate GPU path outside macOS; this is the same softbuffer renderer.
    pub fn new_gpu(window: &Arc<Window>, _drawable_count: u8) -> Self {
        Self::new(Arc::clone(window))
    }

    pub fn is_gpu(&self) -> bool { false }

    pub fn set_cpu_double_buffer(&mut self, _enabled: bool) {}

    pub fn set_gpu_drawable_count(&mut self, _count: u8) {}

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width  = width;
        self.height = height;
    }

    pub fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F) {
        let w = self.width;
        let h = self.height;
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else { return };
        if self.surf.resize(nw, nh).is_err() { return; }
        let Ok(mut raw) = self.surf.buffer_mut() else { return };
        draw(&mut raw, w, h);
        let _ = raw.present();
    }
}

/// Display-refresh callback driver. Not available outside macOS: `new` returns
/// None and the main loop falls back to `request_redraw` (see `apply_vsync_setting`).
pub struct DisplayLink;

impl DisplayLink {
    pub fn new(_on_vsync: impl Fn() + Send + Sync + 'static) -> Option<Self> { None }
}
