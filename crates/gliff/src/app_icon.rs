//! The original Gliff artwork, embedded so the bare macOS binary needs no bundle
//! or installed icon theme. See README for the reproducible SVG rasterization.

#[cfg(any(target_os = "macos", test))]
const PNG: &[u8] = include_bytes!("../../../resources/gliff.png");

/// Called once from GTK startup, after GDK has created NSApplication.
#[cfg(target_os = "macos")]
pub fn install() -> Result<(), &'static str> {
    assert!(gtk4::is_initialized_main_thread());
    native::install()
}

#[cfg(not(target_os = "macos"))]
pub fn install() -> Result<(), &'static str> {
    Ok(())
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::{c_char, c_void};

    type Id = *mut c_void;
    type Sel = *mut c_void;

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> Id;
        fn CFRelease(object: Id);
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
    }

    pub(super) fn install() -> Result<(), &'static str> {
        // SAFETY: The caller enforces GTK's initialized main thread, where GDK
        // has already created NSApplication. Class/selector strings are static
        // C strings; every objc_msgSend cast matches the SDK method signature
        // (pointer arguments/returns or void, no structs/variadics). CFData is
        // toll-free bridged to NSData and copies our valid static PNG bytes.
        // CFDataCreate and alloc/init return owned objects, released on every
        // path; sharedApplication is borrowed. applicationIconImage is strong,
        // so NSApplication retains the image after our release. initWithData:
        // consumes the allocated receiver even when initialization returns nil.
        unsafe {
            let send: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_object: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let set_object: unsafe extern "C" fn(Id, Sel, Id) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let release: unsafe extern "C" fn(Id, Sel) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let app = send(
                objc_getClass(c"NSApplication".as_ptr()),
                sel_registerName(c"sharedApplication".as_ptr()),
            );
            if app.is_null() {
                return Err("NSApplication is unavailable");
            }
            let data = CFDataCreate(
                std::ptr::null(),
                super::PNG.as_ptr(),
                super::PNG.len() as isize,
            );
            if data.is_null() {
                return Err("could not allocate icon data");
            }
            let image = send_object(
                send(
                    objc_getClass(c"NSImage".as_ptr()),
                    sel_registerName(c"alloc".as_ptr()),
                ),
                sel_registerName(c"initWithData:".as_ptr()),
                data,
            );
            CFRelease(data);
            if image.is_null() {
                return Err("could not decode the embedded icon");
            }
            set_object(
                app,
                sel_registerName(c"setApplicationIconImage:".as_ptr()),
                image,
            );
            release(image, sel_registerName(c"release".as_ptr()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use gtk4::gdk_pixbuf::Pixbuf;
    use std::io::Cursor;

    #[test]
    fn embedded_icon_is_original_size_scaled_with_transparency() {
        // Pixbuf decoding does not initialize GTK or open a display.
        let icon = Pixbuf::from_read(Cursor::new(super::PNG)).expect("embedded PNG decodes");
        assert_eq!((icon.width(), icon.height()), (1024, 1024));
        assert!(icon.has_alpha());
        assert_eq!(icon.n_channels(), 4);
        let bytes = icon.read_pixel_bytes();
        let pixel = |x: usize, y: usize| {
            let start = y * icon.rowstride() as usize + x * 4;
            &bytes[start..start + 4]
        };
        for (x, y) in [(0, 0), (1023, 0), (0, 1023), (1023, 1023)] {
            assert_eq!(pixel(x, y)[3], 0, "rounded corners remain transparent");
        }
        assert_eq!(pixel(512, 100), [0x10, 0x10, 0x10, 255]);
        assert_eq!(pixel(240, 400), [0xf5, 0xf1, 0xe8, 255]);
    }
}
