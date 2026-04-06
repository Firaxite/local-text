// macOS renderer using a layer-hosting NSView.
//
// "Layer-hosting" means we call [view setLayer:layer] BEFORE
// [view setWantsLayer:YES].  Apple's docs: "AppKit refrains from
// interfering with the layer's contents."  This is the key difference
// from a layer-backed view, where AppKit owns the layer and may clear
// or replace its contents during live resize — the root cause of the
// blank-window flicker we were seeing.
//
// With layer-hosting:
//   • AppKit manages the layer's frame/bounds automatically (tied to the
//     NSView frame); we never call setFrame.
//   • We own the contents (IOSurface) entirely.
//   • One IOSurface per window size, recreated only on resize.
//
// Pixel format: 0x00RRGGBB (little-endian u32), matching the 'BGRA' OSType.
// CA needs a model property change to schedule a composite pass; we toggle
// zPosition by 1 ULP on alternating frames so there is always a change
// without any visual difference.

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
    layer:   Retained<CALayer>,
    surface: IOSurfaceRef,
    width:   u32,
    height:  u32,
    parity:  bool,
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
        // setWantsLayer:YES.  This transfers ownership of the layer to us;
        // AppKit only manages the frame (keeping it in sync with the view
        // bounds) and never touches the contents.
        match window.window_handle().unwrap().as_raw() {
            RawWindowHandle::AppKit(h) => unsafe {
                let view: &NSObject = h.ns_view.cast().as_ref();
                let _: () = msg_send![view, setLayer: &*layer];
                let _: () = msg_send![view, setWantsLayer: Bool::YES];
            },
            _ => panic!("unsupported window handle type on macOS"),
        }

        Renderer { layer, surface: ptr::null_mut(), width: 0, height: 0, parity: false }
    }

    /// Call whenever `WindowEvent::Resized` fires (physical pixel dimensions).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height { return; }
        self.width  = width;
        self.height = height;
        // Drop old surface; a fresh one sized to the new dimensions is
        // created on the next render_frame call.  AppKit keeps the layer
        // frame in sync with the view bounds automatically — no setFrame needed.
        if !self.surface.is_null() {
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

        if self.surface.is_null() {
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

        // CA needs a model-layer property change to schedule a composite pass;
        // a bare setContents with the same pointer is a no-op.  Toggle zPosition
        // by 1 ULP each frame so there is always a detectable change.
        // f64::from_bits(1) ≈ 5e-324; visually indistinguishable from 0.0.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let z = if self.parity { 0.0_f64 } else { f64::from_bits(1) };
        self.layer.setZPosition(z);
        self.parity = !self.parity;
        set_layer_contents(&self.layer, self.surface);
        CATransaction::commit();
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

fn set_layer_contents(layer: &CALayer, surface: IOSurfaceRef) {
    let any: &AnyObject = unsafe { &*(surface as *const AnyObject) };
    unsafe { layer.setContents(Some(any)) };
}
