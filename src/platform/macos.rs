// macOS renderer: one persistent IOSurface per window size, set as a CALayer's
// contents.  Core Animation maps the IOSurface as a GPU texture (no per-frame
// RAM copy), giving us exactly one framebuffer in the process at steady state.
//
// Pixel format: 0x00RRGGBB (little-endian u32), matching the 'BGRA' OSType.
//
// IOSurface is created via the raw C API (IOSurface.framework) to avoid the
// objc2 feature-gate maze around `initWithProperties:`.  CALayer management
// uses the objc2-quartz-core bindings, which are straightforward.

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
// We declare only what we need; the framework links are pulled in by the
// objc2-io-surface crate (IOSurface) and objc2-core-foundation (CF).

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

// kCFNumberSInt32Type = 3
const CF_NUMBER_SINT32: i64 = 3;
// kCFStringEncodingUTF8 = 0x08000100
const CF_STRING_ENC_UTF8: u32 = 0x0800_0100;

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    layer:      Retained<CALayer>,
    root_layer: Retained<CALayer>,
    surface:    IOSurfaceRef,  // null when not yet allocated
    width:      u32,
    height:     u32,
    parity:     bool,          // toggles zPosition by 1 ULP each frame; see render_frame
}

// SAFETY: IOSurfaceRef is a CF object that is thread-safe for retain/release.
// All mutations happen on the main thread under the winit event loop.
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

        // ── Get the NSView and make it layer-backed ────────────────────────
        let root_layer: Retained<CALayer> = match window.window_handle().unwrap().as_raw() {
            RawWindowHandle::AppKit(h) => unsafe {
                let view: &NSObject = h.ns_view.cast().as_ref();
                let _: () = msg_send![view, setWantsLayer: Bool::YES];
                let layer: Option<Retained<CALayer>> = msg_send![view, layer];
                layer.expect("NSView has no layer after setWantsLayer:YES")
            },
            _ => panic!("unsupported window handle type on macOS"),
        };
        // ── Create a sublayer we fully control ────────────────────────────
        let layer = CALayer::new();
        layer.setGeometryFlipped(true);
        layer.setOpaque(true);

        // Match the root layer's scale factor so 1 surface pixel = 1 screen pixel.
        let scale = root_layer.contentsScale();
        layer.setContentsScale(scale);

        unsafe {
            use objc2_quartz_core::kCAGravityResize;
            layer.setContentsGravity(kCAGravityResize);
        }

        root_layer.addSublayer(&layer);
        layer.setFrame(root_layer.bounds());

        Renderer { layer, root_layer, surface: ptr::null_mut(), width: 0, height: 0, parity: false }
    }

    /// Call whenever `WindowEvent::Resized` fires (physical pixel dimensions).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height { return; }
        self.width  = width;
        self.height = height;
        // Drop old surface.  The layer frame *and* new surface are committed
        // together inside render_frame so there is never a moment where the
        // layer has the new frame but still displays the old (wrong-size) surface.
        if !self.surface.is_null() {
            unsafe { CFRelease(self.surface.cast()) };
            self.surface = ptr::null_mut();
        }
    }

    /// Lock the framebuffer, invoke `draw(pixels, width, height)`, unlock,
    /// then tell Core Animation to composite the updated surface.
    ///
    /// `pixels` is row-major, stride == width in u32 units, format 0x00RRGGBB.
    pub fn render_frame<F: FnOnce(&mut [u32], u32, u32)>(&mut self, draw: F) {
        let w = self.width;
        let h = self.height;
        if w == 0 || h == 0 { return; }

        // (Re-)create the IOSurface on first call or after a resize.
        let surface_is_new = self.surface.is_null();
        if surface_is_new {
            self.surface = create_surface(w, h);
        }

        unsafe {
            // Lock for CPU write (options = 0).
            IOSurfaceLock(self.surface, 0, ptr::null_mut());

            let base   = IOSurfaceGetBaseAddress(self.surface) as *mut u32;
            let stride = IOSurfaceGetBytesPerRow(self.surface) / 4;
            // We requested bytes_per_row = width*4, so stride should equal width.
            debug_assert_eq!(stride, w as usize);
            let pixels = std::slice::from_raw_parts_mut(base, (w * h) as usize);

            draw(pixels, w, h);

            IOSurfaceUnlock(self.surface, 0, ptr::null_mut());
        }

        // CA only re-composites when it sees a model property change.  A bare
        // setContents with the same pointer is a no-op; the pixel data update
        // isn't enough on its own.  We need to change *something* in every
        // transaction to trigger a composite pass.
        //
        // The old approach (ping-pong with a 1×1 sentinel) sent two separate
        // CATransaction commits per frame.  If they straddled a vsync boundary
        // — common during live resize when the compositor is under load — the
        // 1×1 sentinel surface was displayed for a full frame, appearing blank.
        //
        // Fix: single transaction per frame.  We toggle zPosition by 1 ULP
        // (0.0 ↔ 5e-324) on alternating frames.  CA always sees a property
        // change and schedules a composite, but the visual difference is
        // sub-atomic (< 1e-18 of a pixel at any real display size).
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if surface_is_new {
            // Also sync the layer frame atomically with the new surface so the
            // layer never shows the new (larger/smaller) frame with stale content.
            self.layer.setFrame(self.root_layer.bounds());
        }
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
        // Build a CFDictionary with IOSurface property keys/values.
        // All keys are CFString (kIOSurface*), all values are CFNumber (SInt32).
        let make_num = |v: u32| -> *mut c_void {
            let v32 = v as i32;
            CFNumberCreate(kCFAllocatorDefault, CF_NUMBER_SINT32, ptr::addr_of!(v32).cast())
        };
        let make_key = |s: &[u8]| -> *mut c_void {
            CFStringCreateWithCString(kCFAllocatorDefault, s.as_ptr().cast(), CF_STRING_ENC_UTF8)
        };

        // IOSurface property key names (null-terminated).
        let k_width    = make_key(b"IOSurfaceWidth\0");
        let k_height   = make_key(b"IOSurfaceHeight\0");
        let k_bpr      = make_key(b"IOSurfaceBytesPerRow\0");
        let k_bpe      = make_key(b"IOSurfaceBytesPerElement\0");
        let k_pixfmt   = make_key(b"IOSurfacePixelFormat\0");

        let v_width    = make_num(width);
        let v_height   = make_num(height);
        let v_bpr      = make_num(width * 4);
        let v_bpe      = make_num(4);
        let v_pixfmt   = make_num(PIXEL_FORMAT_BGRA);

        let keys:   [*const c_void; 5] = [k_width,  k_height,  k_bpr,  k_bpe,  k_pixfmt];
        let values: [*const c_void; 5] = [v_width,  v_height,  v_bpr,  v_bpe,  v_pixfmt];

        let dict = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            5,
            ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
            ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
        );

        let surface = IOSurfaceCreate(dict);

        // Release temporaries.
        for &k in &keys   { CFRelease(k); }
        for &v in &values { CFRelease(v); }
        CFRelease(dict.cast());

        assert!(!surface.is_null(), "IOSurfaceCreate returned null");
        surface
    }
}

fn set_layer_contents(layer: &CALayer, surface: IOSurfaceRef) {
    // IOSurface is toll-free bridged with ObjC — safe to cast to AnyObject.
    let any: &AnyObject = unsafe { &*(surface as *const AnyObject) };
    unsafe { layer.setContents(Some(any)) };
}
