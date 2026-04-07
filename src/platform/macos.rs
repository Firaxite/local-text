// macOS renderer using a layer-hosting NSView + CGImage-per-frame.
//
// "Layer-hosting" means we call [view setLayer:layer] BEFORE
// [view setWantsLayer:YES].  Apple's docs: "AppKit refrains from
// interfering with the layer's contents."  This prevents AppKit from
// clearing or replacing our contents during live resize.
//
// CA caches the GPU texture it imports from a CALayer's `contents` pointer.
// Setting the same IOSurface pointer frame after frame is a no-op — CA
// composites the stale GPU texture even after we've written new pixels.
//
// Fix: wrap the IOSurface base address in a fresh CGImage each frame.
// CGImage is a tiny (~200-byte) metadata object; its pixel memory is shared
// with the IOSurface (no copy, no extra framebuffer).  Because the CGImage
// pointer is new every frame, CA is forced to re-import the texture from the
// current IOSurface pixels — eliminating the blank-window flicker.
//
// Pixel format: 0x00RRGGBB (little-endian u32), matching the 'BGRA' OSType.

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

// ── CoreGraphics C bindings ───────────────────────────────────────────────────

type CGColorSpaceRef    = *mut c_void;
type CGDataProviderRef  = *mut c_void;
type CGImageRef         = *mut c_void;

// kCGBitmapByteOrder32Little | kCGImageAlphaNoneSkipFirst = (2<<12)|4 = 8196
const CG_BITMAP_INFO: u32 = 8196;

// CGDataProviderDirectCallbacks: version=0, all callbacks null except getBytesAtPosition.
// We use the simpler "no-copy" provider that takes a raw pointer directly.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C-unwind" {
    fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;
    fn CGColorSpaceRelease(cs: CGColorSpaceRef);
    fn CGDataProviderCreateWithData(
        info:         *mut c_void,
        data:         *const c_void,
        size:         usize,
        release_data: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
    ) -> CGDataProviderRef;
    fn CGDataProviderRelease(provider: CGDataProviderRef);
    fn CGImageCreate(
        width:             usize,
        height:            usize,
        bits_per_component: usize,
        bits_per_pixel:    usize,
        bytes_per_row:     usize,
        color_space:       CGColorSpaceRef,
        bitmap_info:       u32,
        provider:          CGDataProviderRef,
        decode:            *const f64,   // NULL
        should_interpolate: bool,
        intent:            i32,         // kCGRenderingIntentDefault = 0
    ) -> CGImageRef;
    fn CGImageRelease(image: CGImageRef);
}

const CF_NUMBER_SINT32: i64 = 3;      // kCFNumberSInt32Type
const CF_STRING_ENC_UTF8: u32 = 0x0800_0100;

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    layer:      Retained<CALayer>,
    surface:    IOSurfaceRef,
    colorspace: CGColorSpaceRef,
    width:      u32,
    height:     u32,
}

unsafe impl Send for Renderer {}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            if !self.surface.is_null()    { CFRelease(self.surface.cast()); }
            if !self.colorspace.is_null() { CGColorSpaceRelease(self.colorspace); }
        }
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
        match window.window_handle().unwrap().as_raw() {
            RawWindowHandle::AppKit(h) => unsafe {
                let view: &NSObject = h.ns_view.cast().as_ref();
                let _: () = msg_send![view, setLayer: &*layer];
                let _: () = msg_send![view, setWantsLayer: Bool::YES];
            },
            _ => panic!("unsupported window handle type on macOS"),
        }

        let colorspace = unsafe { CGColorSpaceCreateDeviceRGB() };
        assert!(!colorspace.is_null(), "CGColorSpaceCreateDeviceRGB failed");

        Renderer { layer, surface: ptr::null_mut(), colorspace, width: 0, height: 0 }
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

        let base_addr;
        let bytes_per_row;
        unsafe {
            IOSurfaceLock(self.surface, 0, ptr::null_mut());
            base_addr = IOSurfaceGetBaseAddress(self.surface) as *mut u32;
            bytes_per_row = IOSurfaceGetBytesPerRow(self.surface);
            debug_assert_eq!(bytes_per_row / 4, w as usize);
            let pixels = std::slice::from_raw_parts_mut(base_addr, (w * h) as usize);
            draw(pixels, w, h);
            IOSurfaceUnlock(self.surface, 0, ptr::null_mut());
        }

        // Create a fresh CGImage wrapping the IOSurface pixel memory (no copy).
        // The new CGImage pointer forces CA to re-import the GPU texture every
        // frame, so the on-screen content always matches what we just painted.
        let cgimage = unsafe {
            let provider = CGDataProviderCreateWithData(
                ptr::null_mut(),
                base_addr.cast(),
                bytes_per_row * h as usize,
                None,
            );
            let img = CGImageCreate(
                w as usize, h as usize,
                8, 32,
                bytes_per_row,
                self.colorspace,
                CG_BITMAP_INFO,
                provider,
                ptr::null(),
                false,
                0,
            );
            CGDataProviderRelease(provider);
            img
        };
        assert!(!cgimage.is_null(), "CGImageCreate returned null");

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let any: &AnyObject = unsafe { &*(cgimage as *const AnyObject) };
        unsafe { self.layer.setContents(Some(any)) };
        CATransaction::commit();

        // CA holds its own reference; release ours.
        unsafe { CGImageRelease(cgimage) };
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
