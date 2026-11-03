// Platform-specific rendering backends.
// Each backend exposes the same API (main.rs compiles unchanged on every platform):
//   pub struct Renderer with
//     fn new_cpu(window, double_buffer: bool) -> Self
//     fn new_gpu(window, drawable_count: u8) -> Self
//     fn is_gpu(&self) -> bool
//     fn set_cpu_double_buffer(&mut self, bool) / fn set_gpu_drawable_count(&mut self, u8)
//     fn resize(&mut self, width: u32, height: u32)
//     fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F)
//       where draw receives (pixels, stride, height) — stride >= width, in u32 units
//   pub struct DisplayLink with fn new(on_vsync) -> Option<Self>
//     (None where unsupported; the main loop then falls back to request_redraw)

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{DisplayLink, Renderer};

#[cfg(not(target_os = "macos"))]
mod fallback;
#[cfg(not(target_os = "macos"))]
pub use fallback::{DisplayLink, Renderer};
