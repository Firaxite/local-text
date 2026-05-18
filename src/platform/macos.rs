// macOS renderer: two backends behind a single `Renderer` enum.
//
// CPU backend — IOSurface + CALayer (existing path, zero extra RAM).
//   Pixels are CPU-written then presented via setContentsChanged / setContents.
//
// GPU backend — IOSurface + CAMetalLayer blit (adds ~66 MB at 4K).
//   CPU still writes pixels into an IOSurface; a Metal blit encoder copies the
//   result into a CAMetalLayer drawable whose present() is vsync-aligned by the
//   GPU scheduler.  No tearing regardless of how long the CPU write takes.
//
// Both backends share the same `render_frame(closure)` interface.
//
// CVDisplayLink fires at the display's native rate; the callback sets an
// AtomicBool that the main loop reads to decide when to call request_redraw().

use std::ffi::c_void;
use std::ptr;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{msg_send, MainThreadMarker};
use objc2_foundation::NSObject;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandQueue, MTLDevice, MTLDrawable,
    MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    MTLCreateSystemDefaultDevice,
};
use objc2_quartz_core::{CALayer, CAMetalLayer, CAMetalDrawable, CATransaction};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

// ── Raw IOSurface + CoreFoundation C bindings ─────────────────────────────────

type CFAllocatorRef  = *const c_void;
type CFDictionaryRef = *const c_void;
type IOSurfaceRef    = *mut c_void;

// Opaque stand-in for the C struct `__IOSurface` so that `*mut IOSurface`
// gets the correct objc2 Encode = `^{__IOSurface=}` rather than `^v`.
// Metal's -newTextureWithDescriptor:iosurface:plane: checks this encoding.
#[repr(C)]
struct IOSurface(c_void);

unsafe impl objc2::RefEncode for IOSurface {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("__IOSurface", &[]));
}

// 'BGRA' pixel format — matches 0x00RRGGBB u32 stored in little-endian memory.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C-unwind" {
    static kCFAllocatorDefault: CFAllocatorRef;
    fn CFRelease(cf: *const c_void);
    fn CFNumberCreate(alloc: CFAllocatorRef, the_type: i64, value_ptr: *const c_void) -> *mut c_void;
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, c_str: *const i8, encoding: u32) -> *mut c_void;
    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys:   *const *const c_void,
        values: *const *const c_void,
        count:  isize,
        key_cbs: *const c_void,
        val_cbs: *const c_void,
    ) -> CFDictionaryRef;
    static kCFTypeDictionaryKeyCallBacks:   c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C-unwind" {
    fn IOSurfaceCreate(properties: CFDictionaryRef) -> IOSurfaceRef;
    fn IOSurfaceLock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: IOSurfaceRef) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: IOSurfaceRef) -> usize;
}

const CF_NUMBER_SINT32: i64 = 3;
const CF_STRING_ENC_UTF8: u32 = 0x0800_0100;

// ── CVDisplayLink ─────────────────────────────────────────────────────────────

type CVDisplayLinkRef = *mut c_void;
type CVReturn = i32;
type CVTime = [i64; 4];  // opaque CVTimeStamp placeholder

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C-unwind" {
    fn CVDisplayLinkCreateWithActiveCGDisplays(link: *mut CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkSetOutputCallback(
        link: CVDisplayLinkRef,
        callback: unsafe extern "C" fn(
            CVDisplayLinkRef,
            *const CVTime, *const CVTime,
            i64,
            *mut i64,
            *mut c_void,
        ) -> CVReturn,
        ctx: *mut c_void,
    ) -> CVReturn;
    fn CVDisplayLinkStart(link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkStop(link: CVDisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkRelease(link: CVDisplayLinkRef);
}

unsafe extern "C" fn display_link_callback(
    _link: CVDisplayLinkRef,
    _now: *const CVTime, _out: *const CVTime,
    _flags: i64, _info: *mut i64,
    ctx: *mut c_void,
) -> CVReturn {
    // ctx is a raw pointer to a Box<dyn Fn()>
    let f = &*(ctx as *const Box<dyn Fn() + Send + Sync>);
    f();
    0
}

/// Drives a callback at the display's native refresh rate.
pub struct DisplayLink {
    link: CVDisplayLinkRef,
    // Keep the boxed closure alive for as long as the DisplayLink is alive.
    _ctx: Box<Box<dyn Fn() + Send + Sync>>,
}

unsafe impl Send for DisplayLink {}
unsafe impl Sync for DisplayLink {}

impl DisplayLink {
    pub fn new(on_vsync: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        let cb: Box<Box<dyn Fn() + Send + Sync>> = Box::new(Box::new(on_vsync));
        let ctx = Box::into_raw(cb);
        let mut link: CVDisplayLinkRef = ptr::null_mut();
        unsafe {
            let r = CVDisplayLinkCreateWithActiveCGDisplays(&mut link);
            if r != 0 || link.is_null() {
                drop(Box::from_raw(ctx)); // reclaim memory
                return None;
            }
            CVDisplayLinkSetOutputCallback(link, display_link_callback, ctx.cast());
            CVDisplayLinkStart(link);
        }
        // Rebuild the Box from the raw pointer so Drop can free it.
        let ctx_box = unsafe { Box::from_raw(ctx) };
        Some(DisplayLink { link, _ctx: ctx_box })
    }
}

impl Drop for DisplayLink {
    fn drop(&mut self) {
        unsafe {
            CVDisplayLinkStop(self.link);
            CVDisplayLinkRelease(self.link);
        }
    }
}

// ── CPU renderer ──────────────────────────────────────────────────────────────
// (original implementation — see module-level comment)

struct CpuRenderer {
    layer:         Retained<CALayer>,
    surfaces:      [IOSurfaceRef; 2],  // [0] and [1]; only [1] used when double_buffer=true
    back:          usize,              // index of the surface to write to next
    double_buffer: bool,               // when false back stays 0 (single-surface legacy mode)
    width:         u32,
    height:        u32,
    view:          *mut AnyObject,
}

unsafe impl Send for CpuRenderer {}

impl Drop for CpuRenderer {
    fn drop(&mut self) {
        for s in &self.surfaces {
            if !s.is_null() { unsafe { CFRelease(s.cast()) }; }
        }
    }
}

impl CpuRenderer {
    fn new(window: &Window, double_buffer: bool) -> Self {
        let layer = CALayer::new();
        layer.setGeometryFlipped(true);
        layer.setOpaque(true);
        layer.setContentsScale(window.scale_factor());
        unsafe {
            use objc2_quartz_core::kCAGravityResize;
            layer.setContentsGravity(kCAGravityResize);
        }

        let view_ptr = match window.window_handle().unwrap().as_raw() {
            RawWindowHandle::AppKit(h) => unsafe {
                let view: &NSObject = h.ns_view.cast().as_ref();
                let _: () = msg_send![view, setLayer: &*layer];
                let _: () = msg_send![view, setWantsLayer: Bool::YES];
                h.ns_view.as_ptr() as *mut AnyObject
            },
            _ => panic!("unsupported window handle"),
        };

        CpuRenderer { layer, surfaces: [ptr::null_mut(); 2], back: 0,
                      double_buffer, width: 0, height: 0, view: view_ptr }
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height { return; }
        self.width  = width;
        self.height = height;
        for s in &mut self.surfaces {
            if !s.is_null() { unsafe { CFRelease(s.cast()) }; *s = ptr::null_mut(); }
        }
        self.back = 0;
    }

    fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F) {
        let w = self.width;
        let h = self.height;
        if w == 0 || h == 0 { return; }

        // Re-attach layer if AppKit swapped it
        unsafe {
            let view_layer: *mut AnyObject = msg_send![self.view, layer];
            let our_layer:  *const c_void  = (&*self.layer as *const CALayer).cast();
            if view_layer as *const c_void != our_layer {
                let _: () = msg_send![self.view, setLayer:      &*self.layer];
                let _: () = msg_send![self.view, setWantsLayer:  Bool::YES];
            }
        }

        // Allocate the back surface on demand
        if self.surfaces[self.back].is_null() {
            self.surfaces[self.back] = create_surface(w, h);
        }

        let surf = self.surfaces[self.back];
        unsafe {
            IOSurfaceLock(surf, 0, ptr::null_mut());
            let bpr    = IOSurfaceGetBytesPerRow(surf);
            let stride = (bpr / 4) as u32; // u32s per row (>= w when 16-byte aligned)
            let base   = IOSurfaceGetBaseAddress(surf) as *mut u32;
            let pixels = std::slice::from_raw_parts_mut(base, (stride * h) as usize);
            draw(pixels, stride, h);
            IOSurfaceUnlock(surf, 0, ptr::null_mut());
        }

        // Present the freshly-written back surface.
        // Double-buffer: setContents alternates between two different surface pointers —
        //   CALayer sees a new pointer each frame and re-composites automatically.
        // Single-buffer: the pointer never changes, so we must call setContentsChanged
        //   to tell CALayer the pixel data was mutated in place.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let any: &AnyObject = unsafe { &*(surf as *const AnyObject) };
        unsafe { self.layer.setContents(Some(any)) };
        if !self.double_buffer {
            unsafe { let _: () = msg_send![&*self.layer, setContentsChanged]; }
        }
        CATransaction::commit();
        unsafe {
            use objc2::ClassType;
            let _: () = msg_send![CATransaction::class(), flush];
        }

        // Flip to the other surface for the next frame (double-buffer only)
        if self.double_buffer { self.back = 1 - self.back; }
    }

    fn set_double_buffer(&mut self, enabled: bool) {
        if enabled == self.double_buffer { return; }
        if !enabled {
            // Single-buffer mode always writes to surfaces[0]; always free surfaces[1].
            // When back==1: surfaces[1] is the write target — CA doesn't hold it, freed immediately.
            // When back==0: surfaces[1] is displayed — CA holds it; our CFRelease drops refcount
            //   to 1. CA releases it on the next frame when setContents(surfaces[0]) is called.
            if !self.surfaces[1].is_null() {
                unsafe { CFRelease(self.surfaces[1].cast()) };
                self.surfaces[1] = ptr::null_mut();
            }
            self.back = 0;
        }
        self.double_buffer = enabled;
        // When enabling, surfaces[1] is allocated lazily on the next render_frame call.
    }
}

// ── GPU renderer ──────────────────────────────────────────────────────────────
// IOSurface CPU write + CAMetalLayer blit for vsync-aligned, tear-free display.

struct GpuRenderer {
    device:         Retained<objc2::runtime::ProtocolObject<dyn MTLDevice>>,
    cmd_queue:      Retained<objc2::runtime::ProtocolObject<dyn MTLCommandQueue>>,
    metal_layer:    Retained<CAMetalLayer>,
    surface:        IOSurfaceRef,
    src_texture:    Option<Retained<objc2::runtime::ProtocolObject<dyn MTLTexture>>>,
    width:          u32,
    height:         u32,
    #[allow(dead_code)]
    view:           *mut AnyObject,  // kept alive; layer was attached in new()
}

unsafe impl Send for GpuRenderer {}

impl Drop for GpuRenderer {
    fn drop(&mut self) {
        if !self.surface.is_null() { unsafe { CFRelease(self.surface.cast()) }; }
    }
}

impl GpuRenderer {
    fn new(window: &Window, drawable_count: u8) -> Option<Self> {
        let device = MTLCreateSystemDefaultDevice()?;

        let cmd_queue = device.newCommandQueue()?;

        let metal_layer = CAMetalLayer::layer();
        metal_layer.setGeometryFlipped(true);
        metal_layer.setOpaque(true);
        unsafe {
            metal_layer.setDevice(Some(&*device));
            metal_layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            // framebufferOnly=false allows the blit encoder to write to the drawable texture
            let _: () = msg_send![&*metal_layer, setFramebufferOnly: Bool::NO];
            // Scale so 1 Metal pixel == 1 screen pixel
            let _: () = msg_send![&*metal_layer, setContentsScale: window.scale_factor()];
            // Drawable pool size: 2 saves ~33 MiB vs 3 at 4K. Apple requires 2–3.
            let count = (drawable_count as usize).clamp(2, 3);
            let _: () = msg_send![&*metal_layer, setMaximumDrawableCount: count];
        }

        let view_ptr = match window.window_handle().unwrap().as_raw() {
            RawWindowHandle::AppKit(h) => unsafe {
                let view: &NSObject = h.ns_view.cast().as_ref();
                // Cast CAMetalLayer to CALayer for setLayer: (it's a subclass)
                let layer_ref: &CALayer = &*((&*metal_layer as *const CAMetalLayer).cast::<CALayer>());
                let _: () = msg_send![view, setLayer: layer_ref];
                let _: () = msg_send![view, setWantsLayer: Bool::YES];
                h.ns_view.as_ptr() as *mut AnyObject
            },
            _ => return None,
        };

        Some(GpuRenderer {
            device,
            cmd_queue,
            metal_layer,
            surface:     ptr::null_mut(),
            src_texture: None,
            width:       0,
            height:      0,
            view:        view_ptr,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height { return; }
        self.width  = width;
        self.height = height;
        if !self.surface.is_null() {
            unsafe { CFRelease(self.surface.cast()) };
            self.surface = ptr::null_mut();
        }
        self.src_texture = None;

        // Update drawable size in physical pixels
        if width > 0 && height > 0 {
            // Use the typed CAMetalLayer::setDrawableSize via a CGSize struct.
            // We define CGSize locally with the correct Encode impl so msg_send works.
            unsafe { set_drawable_size(&self.metal_layer, width, height); }
        }
    }

    fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F) {
        let w = self.width;
        let h = self.height;
        if w == 0 || h == 0 { return; }

        // Allocate IOSurface if needed
        if self.surface.is_null() {
            self.surface = create_surface(w, h);
            self.src_texture = None; // will be created below
        }

        // CPU write into the IOSurface
        unsafe {
            IOSurfaceLock(self.surface, 0, ptr::null_mut());
            let bpr    = IOSurfaceGetBytesPerRow(self.surface);
            let stride = (bpr / 4) as u32; // u32s per row (>= w when 16-byte aligned)
            let base   = IOSurfaceGetBaseAddress(self.surface) as *mut u32;
            let pixels = std::slice::from_raw_parts_mut(base, (stride * h) as usize);
            draw(pixels, stride, h);
            IOSurfaceUnlock(self.surface, 0, ptr::null_mut());
        }

        // Create or reuse the IOSurface-backed Metal texture (zero-copy wrap)
        if self.src_texture.is_none() {
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    w as _,
                    h as _,
                    false,
                )
            };
            desc.setStorageMode(MTLStorageMode::Shared);
            desc.setUsage(MTLTextureUsage::ShaderRead);

            // Wrap IOSurface as a Metal texture (zero-copy, same physical memory).
            // Cast to *mut IOSurface so msg_send! encodes it as ^{__IOSurface=}
            // rather than ^v — Metal's runtime type check requires the former.
            let io_surf_ptr = self.surface as *mut IOSurface;
            let tex = unsafe {
                let tex: *mut AnyObject = msg_send![
                    &*self.device,
                    newTextureWithDescriptor: &*desc,
                    iosurface: io_surf_ptr,
                    plane: 0usize
                ];
                if tex.is_null() { return; }
                // +1 retain from `new*` method; wrap in Retained.
                // Safety: tex is a valid MTLTexture protocol object from Metal.
                Retained::from_raw(tex as *mut objc2::runtime::ProtocolObject<dyn MTLTexture>)
                    .expect("newTextureWithDescriptor:iosurface:plane: returned null")
            };
            self.src_texture = Some(tex);
        }

        let Some(src_tex) = &self.src_texture else { return };

        // Get the next drawable from CAMetalLayer
        let Some(drawable) = self.metal_layer.nextDrawable() else { return };
        let dst_tex = drawable.texture();

        // Blit src → drawable and present at vsync
        let Some(cmd_buf) = self.cmd_queue.commandBuffer() else { return };
        let Some(blit) = cmd_buf.blitCommandEncoder() else { return };

        use objc2_metal::{MTLOrigin, MTLSize};
        let origin = MTLOrigin { x: 0, y: 0, z: 0 };
        let size   = MTLSize   { width: w as _, height: h as _, depth: 1 };

        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                src_tex, 0, 0, origin, size,
                &dst_tex, 0, 0, origin,
            );
        }
        unsafe { let _: () = msg_send![&*blit, endEncoding]; }

        // presentDrawable schedules vsync-aligned presentation
        unsafe {
            let drawable_ref: &objc2::runtime::ProtocolObject<dyn MTLDrawable> =
                &*((&*drawable) as *const _ as *const _);
            cmd_buf.presentDrawable(drawable_ref);
        }
        cmd_buf.commit();
    }

    fn set_drawable_count(&mut self, count: u8) {
        let n = (count as usize).clamp(2, 3);
        unsafe { let _: () = msg_send![&*self.metal_layer, setMaximumDrawableCount: n]; }
    }
}

// ── Public Renderer (wraps either backend) ────────────────────────────────────

enum RendererImpl {
    Cpu(CpuRenderer),
    Gpu(GpuRenderer),
}

pub struct Renderer(RendererImpl);

impl Renderer {
    pub fn new_cpu(window: &Window, double_buffer: bool) -> Self {
        let _mtm = MainThreadMarker::new()
            .expect("Renderer must be created on the main thread");
        Renderer(RendererImpl::Cpu(CpuRenderer::new(window, double_buffer)))
    }

    pub fn new_gpu(window: &Window, drawable_count: u8) -> Self {
        let _mtm = MainThreadMarker::new()
            .expect("Renderer must be created on the main thread");
        if let Some(g) = GpuRenderer::new(window, drawable_count) {
            Renderer(RendererImpl::Gpu(g))
        } else {
            // Fallback to CPU if Metal isn't available
            Renderer(RendererImpl::Cpu(CpuRenderer::new(window, true)))
        }
    }

    pub fn is_gpu(&self) -> bool { matches!(self.0, RendererImpl::Gpu(_)) }

    pub fn set_cpu_double_buffer(&mut self, enabled: bool) {
        if let RendererImpl::Cpu(r) = &mut self.0 { r.set_double_buffer(enabled); }
    }

    pub fn set_gpu_drawable_count(&mut self, count: u8) {
        if let RendererImpl::Gpu(r) = &mut self.0 { r.set_drawable_count(count); }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        match &mut self.0 {
            RendererImpl::Cpu(r) => r.resize(width, height),
            RendererImpl::Gpu(r) => r.resize(width, height),
        }
    }

    pub fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F) {
        match &mut self.0 {
            RendererImpl::Cpu(r) => r.render_frame(draw),
            RendererImpl::Gpu(r) => r.render_frame(draw),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// CGSize with the Encode impl needed for msg_send (matches CGSize struct encoding).
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize { width: f64, height: f64 }
unsafe impl objc2::Encode for CGSize {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
        "CGSize",
        &[f64::ENCODING, f64::ENCODING],
    );
}
unsafe impl objc2::RefEncode for CGSize {
    const ENCODING_REF: objc2::Encoding = objc2::Encoding::Pointer(&<Self as objc2::Encode>::ENCODING);
}

unsafe fn set_drawable_size(layer: &CAMetalLayer, w: u32, h: u32) {
    let size = CGSize { width: w as f64, height: h as f64 };
    let _: () = msg_send![layer, setDrawableSize: size];
}

fn create_surface(width: u32, height: u32) -> IOSurfaceRef {
    unsafe {
        let make_num = |v: u32| -> *mut c_void {
            let v32 = v as i32;
            CFNumberCreate(kCFAllocatorDefault, CF_NUMBER_SINT32, ptr::addr_of!(v32).cast())
        };
        let make_key = |s: &[u8]| -> *mut c_void {
            CFStringCreateWithCString(kCFAllocatorDefault, s.as_ptr().cast(), CF_STRING_ENC_UTF8)
        };

        let k_width  = make_key(b"IOSurfaceWidth\0");
        let k_height = make_key(b"IOSurfaceHeight\0");
        let k_bpr    = make_key(b"IOSurfaceBytesPerRow\0");
        let k_bpe    = make_key(b"IOSurfaceBytesPerElement\0");
        let k_pixfmt = make_key(b"IOSurfacePixelFormat\0");

        let v_width  = make_num(width);
        let v_height = make_num(height);
        // Metal requires bytesPerRow aligned to 16 bytes.
        let bpr = (width * 4 + 15) & !15;
        let v_bpr    = make_num(bpr);
        let v_bpe    = make_num(4);
        let v_pixfmt = make_num(PIXEL_FORMAT_BGRA);

        let keys:   [*const c_void; 5] = [k_width, k_height, k_bpr, k_bpe, k_pixfmt];
        let values: [*const c_void; 5] = [v_width, v_height, v_bpr, v_bpe, v_pixfmt];

        let dict = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(), values.as_ptr(), 5,
            ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
            ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
        );
        let surface = IOSurfaceCreate(dict);
        for &k in &keys   { CFRelease(k); }
        for &v in &values { CFRelease(v); }
        CFRelease(dict.cast());
        assert!(!surface.is_null(), "IOSurfaceCreate returned null");
        surface
    }
}
