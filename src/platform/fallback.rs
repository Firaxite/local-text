// Fallback renderer for non-macOS platforms (Windows, Linux).
// Uses softbuffer: allocates one pixel buffer per frame, hands it to the OS
// compositor.  Memory characteristics differ from the IOSurface backend;
// see platform/macos.rs for the low-overhead path.

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
    pub fn new(window: Arc<Window>) -> Self {
        let ctx  = softbuffer::Context::new(window.clone()).unwrap();
        let surf = softbuffer::Surface::new(&ctx, window).unwrap();
        Renderer { _ctx: ctx, surf, width: 0, height: 0 }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width  = width;
        self.height = height;
    }

    pub fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F) {
        let w = self.width;
        let h = self.height;
        if w == 0 || h == 0 { return; }

        let _ = self.surf.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap());
        let Ok(mut raw) = self.surf.buffer_mut() else { return };
        draw(&mut raw, w, h);
        raw.present().unwrap();
    }
}
