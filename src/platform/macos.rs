// macOS renderer using a layer-hosting NSView + IOSurface.
//
// "Layer-hosting" means we call [view setLayer:layer] BEFORE
// [view setWantsLayer:YES].  Apple's docs: "AppKit refrains from
// interfering with the layer's contents."  This is the key difference
// from a layer-backed view, where AppKit owns the layer and may clear
// or replace its contents during live resize.
//
// IOSurface is zero-copy between CPU and GPU: the GPU reads directly from
// the same physical pages we write to during lock/unlock.  No pixel data
// is ever copied.
//
// CA only re-composites a layer when one of its model properties changes.
// Setting `contents` to the same IOSurface pointer is a no-op.  We use
// the private-but-stable `setContentsChanged` message (used by WebKit for
// canvas/video) to tell CA the pixels have been updated in-place so it
// schedules a composite pass without touching any other property.
//
// When a resize creates a new surface (new pointer), we set `contents`
// directly — that forces CA to import the new surface.
//
// After live resize ends, CA's render server stops responding to
// `setContentsChanged` until a fresh `setContents` re-establishes the
// compositing connection.  We handle this with a `needs_reimport` flag:
// after any resize-triggered `setContents`, we force one additional surface
// recreation + `setContents` on the very next non-resize render.  That
// single recovery render wakes CA back up; subsequent renders revert to the
// cheaper `setContentsChanged` path.  The recovery render does NOT re-set
// the flag, preventing an infinite cycle.
//
// Pixel format: 0x00RRGGBB (little-endian u32), matching the 'BGRA' OSType.

macro_rules! dlog {
    ($($arg:tt)*) => {
        #[cfg(feature = "logging")]
        dlog!($($arg)*);
    }
}

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{msg_send, MainThreadMarker};
use objc2_foundation::NSObject;
use objc2_quartz_core::{CALayer, CATransaction};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

// ── Raw IOSurface + CoreFoundation C bindings ─────────────────────────────────

type CFAllocatorRef  = *const c_void;
type CFDictionaryRef = *const c_void;
type IOSurfaceRef    = *mut c_void;

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

const CF_NUMBER_SINT32: i64 = 3;      // kCFNumberSInt32Type
const CF_STRING_ENC_UTF8: u32 = 0x0800_0100;

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    layer:        Retained<CALayer>,
    surface:      IOSurfaceRef,
    width:        u32,
    height:       u32,
    view:         *mut AnyObject,   // NSView — owned by NSWindow, valid as long as the window lives
    frame_count:  u64,
    needs_reimport: bool,
}

unsafe impl Send for Renderer {}

impl Drop for Renderer {
    fn drop(&mut self) {
        if !self.surface.is_null() { unsafe { CFRelease(self.surface.cast()) }; }
    }
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Self {
        let _mtm = MainThreadMarker::new()
            .expect("Renderer must be created on the main thread");

        let layer = CALayer::new();
        layer.setGeometryFlipped(true);
        layer.setOpaque(true);
        // Scale so that 1 IOSurface pixel == 1 physical screen pixel.
        layer.setContentsScale(window.scale_factor());
        unsafe {
            use objc2_quartz_core::kCAGravityResize;
            layer.setContentsGravity(kCAGravityResize);
        }

        // Layer-hosting setup: assign our layer to the NSView BEFORE calling
        // setWantsLayer:YES.  AppKit only manages the frame (keeping it in sync
        // with the view bounds) and never touches the contents.
        let view_ptr = match window.window_handle().unwrap().as_raw() {
            RawWindowHandle::AppKit(h) => unsafe {
                let view: &NSObject = h.ns_view.cast().as_ref();
                let _: () = msg_send![view, setLayer: &*layer];
                let _: () = msg_send![view, setWantsLayer: Bool::YES];
                h.ns_view.as_ptr() as *mut AnyObject
            },
            _ => panic!("unsupported window handle type on macOS"),
        };

        Renderer { layer, surface: ptr::null_mut(), width: 0, height: 0, view: view_ptr, frame_count: 0, needs_reimport: false }
    }

    /// Call whenever `WindowEvent::Resized` fires (physical pixel dimensions).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height {
            dlog!("[resize] resize() called but dimensions unchanged ({}x{}), skipping", width, height);
            return;
        }
        dlog!("[resize] Renderer::resize {}x{} -> {}x{}", self.width, self.height, width, height);
        self.width  = width;
        self.height = height;
        // Drop old surface; a fresh one sized to the new dimensions is
        // created on the next render_frame call.  AppKit keeps the layer
        // frame in sync with the view bounds automatically — no setFrame needed.
        if !self.surface.is_null() {
            dlog!("[resize] Releasing old IOSurface");
            unsafe { CFRelease(self.surface.cast()) };
            self.surface = ptr::null_mut();
        }
    }

    /// Lock the framebuffer, invoke `draw(pixels, width, height)`, unlock,
    /// then tell Core Animation to composite the updated surface.
    ///
    /// `pixels` is row-major, stride == width, format 0x00RRGGBB.
    pub fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F) {
        let w = self.width;
        let h = self.height;
        if w == 0 || h == 0 { return; }

        // Guard: if AppKit replaced the view's layer after live resize (possible in
        // layer-hosting mode), our stored layer is orphaned.  Re-attach it so CA
        // composites our content again.
        unsafe {
            let view_layer: *mut AnyObject = msg_send![self.view, layer];
            let our_layer:  *const c_void  = (&*self.layer as *const CALayer).cast();
            if view_layer as *const c_void != our_layer {
                dlog!("[render] layer detached (view.layer={:p} self.layer={:p}) — reattaching",
                          view_layer, our_layer);
                let _: () = msg_send![self.view, setLayer:     &*self.layer];
                let _: () = msg_send![self.view, setWantsLayer: Bool::YES];
            }
        }

        // Recovery: after a resize-triggered setContents, CA stops responding to
        // setContentsChanged until a fresh setContents re-establishes the connection.
        // Force surface recreation on the very next non-resize render.
        let is_recovery_render = self.needs_reimport && !self.surface.is_null();
        if is_recovery_render {
            dlog!("[render] needs_reimport: dropping surface to force setContents on frame {}", self.frame_count);
            unsafe { CFRelease(self.surface.cast()) };
            self.surface = ptr::null_mut();
        }
        self.needs_reimport = false;

        let new_surface = self.surface.is_null();
        if new_surface {
            dlog!("[resize] Allocating new IOSurface {}x{}", w, h);
            self.surface = create_surface(w, h);
        }

        unsafe {
            IOSurfaceLock(self.surface, 0, ptr::null_mut());
            let base   = IOSurfaceGetBaseAddress(self.surface) as *mut u32;
            let stride = IOSurfaceGetBytesPerRow(self.surface) / 4;
            debug_assert_eq!(stride, w as usize);
            let pixels = std::slice::from_raw_parts_mut(base, (w * h) as usize);
            draw(pixels, w, h);
            IOSurfaceUnlock(self.surface, 0, ptr::null_mut());
        }

        self.frame_count += 1;
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if new_surface {
            // New pointer — CA must import the surface fresh.
            dlog!("[render] frame {} setContents (new surface {:p}) recovery={}", self.frame_count, self.surface, is_recovery_render);
            if !is_recovery_render {
                self.needs_reimport = true;  // schedule one recovery render after this
            }
            let any: &AnyObject = unsafe { &*(self.surface as *const AnyObject) };
            unsafe { self.layer.setContents(Some(any)) };
        } else {
            // Same pointer — pixels were updated in-place.
            dlog!("[render] frame {} setContentsChanged", self.frame_count);
            unsafe { let _: () = msg_send![&*self.layer, setContentsChanged]; }
        }
        CATransaction::commit();

        // Force CA to submit the implicit transaction to the render server immediately,
        // rather than waiting for the run-loop iteration to end.  After live resize,
        // the run-loop end flush may not fire in time (or at all) for our timer-driven
        // renders, leaving committed transactions invisible until the next resize event.
        unsafe {
            use objc2::ClassType;
            let _: () = msg_send![CATransaction::class(), flush];
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
        let v_bpr    = make_num(width * 4);
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
