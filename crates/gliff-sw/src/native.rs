use crate::{annexb::Parameters, Error, Result};

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{ffi::c_void, ptr::NonNull};
    unsafe extern "C" {
        fn gliff_vt_create(
            sps: *const u8,
            sps_len: usize,
            pps: *const u8,
            pps_len: usize,
            width: u32,
            height: u32,
            result: *mut *mut c_void,
        ) -> i32;
        fn gliff_vt_decode(
            decoder: *mut c_void,
            avcc: *const u8,
            length: usize,
            pixels: *mut u8,
            capacity: usize,
        ) -> i32;
        fn gliff_vt_destroy(decoder: *mut c_void);
    }
    // Deliberately !Send/!Sync: its native session belongs to the existing Worker.
    pub struct Native {
        pointer: NonNull<c_void>,
        _owner: std::marker::PhantomData<std::rc::Rc<()>>,
    }
    fn status(code: i32) -> Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(Error::Native(code))
        }
    }
    impl Native {
        pub fn new(parameters: &Parameters, size: (u32, u32)) -> Result<Self> {
            let mut pointer = std::ptr::null_mut();
            // SAFETY: slices live through this synchronous SDK-backed C function;
            // it owns its created CF objects and copies the parameter-set data.
            status(unsafe {
                gliff_vt_create(
                    parameters.sps.as_ptr(),
                    parameters.sps.len(),
                    parameters.pps.as_ptr(),
                    parameters.pps.len(),
                    size.0,
                    size.1,
                    &mut pointer,
                )
            })?;
            Ok(Self {
                pointer: NonNull::new(pointer).ok_or(Error::Native(-200001))?,
                _owner: std::marker::PhantomData,
            })
        }
        pub fn decode(&mut self, avcc: &[u8], pixels: &mut [u8]) -> Result<()> {
            // SAFETY: session is live and exclusively accessed; flags=0 in C
            // completes all writes/callbacks before these borrowed slices expire.
            status(unsafe {
                gliff_vt_decode(
                    self.pointer.as_ptr(),
                    avcc.as_ptr(),
                    avcc.len(),
                    pixels.as_mut_ptr(),
                    pixels.len(),
                )
            })
        }
    }
    impl Drop for Native {
        fn drop(&mut self) {
            // SAFETY: owns this unique live session; no decode/callback is in flight.
            unsafe {
                gliff_vt_destroy(self.pointer.as_ptr());
            }
        }
    }
    #[cfg(test)]
    mod tests {
        unsafe extern "C" {
            fn gliff_copy_bgra(
                src: *const u8,
                src_len: usize,
                stride: usize,
                source_width: usize,
                source_height: usize,
                x: usize,
                y: usize,
                width: usize,
                height: usize,
                dst: *mut u8,
                capacity: usize,
            ) -> i32;
        }
        #[test]
        fn stride_crop_bounds_and_capacity() {
            let src: Vec<u8> = (0..48).collect();
            let mut dst = [0u8; 16];
            // SAFETY: both arrays outlive each synchronous checked copy; invalid
            // dimensions/capacities must fail before touching the destination.
            unsafe {
                assert_eq!(
                    gliff_copy_bgra(src.as_ptr(), 48, 16, 3, 3, 1, 1, 2, 2, dst.as_mut_ptr(), 16),
                    0
                );
                assert_eq!(
                    dst,
                    [20, 21, 22, 23, 24, 25, 26, 27, 36, 37, 38, 39, 40, 41, 42, 43]
                );
                for (len, stride, width, height, x, y, capacity) in [
                    (47, 16, 3, 3, 1, 1, 16),
                    (48, 8, 3, 3, 1, 1, 16),
                    (48, usize::MAX, 3, 3, 1, 1, 16),
                    (48, 16, usize::MAX, 3, 1, 1, 16),
                    (48, 16, 3, usize::MAX, 1, 1, 16),
                    (48, 16, 3, 3, 3, 1, 16),
                    (48, 16, 3, 3, 1, 3, 16),
                    (48, 16, 3, 3, 1, 1, 15),
                ] {
                    assert_ne!(
                        gliff_copy_bgra(
                            src.as_ptr(),
                            len,
                            stride,
                            width,
                            height,
                            x,
                            y,
                            2,
                            2,
                            dst.as_mut_ptr(),
                            capacity
                        ),
                        0
                    );
                }
            }
        }
    }
}
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;
    pub struct Native;
    impl Native {
        pub fn new(_: &Parameters, _: (u32, u32)) -> Result<Self> {
            Err(Error::Unsupported("VideoToolbox requires macOS".into()))
        }
        pub fn decode(&mut self, _: &[u8], _: &mut [u8]) -> Result<()> {
            Err(Error::Unsupported("VideoToolbox requires macOS".into()))
        }
    }
}
pub use platform::Native;
