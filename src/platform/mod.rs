// Platform-specific rendering backends.
// Each backend exposes `pub struct Renderer` with:
//   fn new(window: Arc<Window>) -> Self
//   fn resize(&mut self, width: u32, height: u32)
//   fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F)
//     where draw receives (pixels, width, height), stride == width (in u32 units)

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Renderer;

#[cfg(not(target_os = "macos"))]
mod fallback;
#[cfg(not(target_os = "macos"))]
pub use fallback::Renderer;
