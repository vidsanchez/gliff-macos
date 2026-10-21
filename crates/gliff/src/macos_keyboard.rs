//! Small, read-only native snapshots, only called while this window/video is
//! focused. No event tap, background key stream, Accessibility API or injection.
//! GDK's flagsChanged classification uses aggregate flags, which cannot tell
//! which side was released when both sides of a modifier were held.

#[cfg(target_os = "macos")]
mod native {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, code: u16) -> bool;
        fn CGEventSourceFlagsState(state: i32) -> u64;
    }
    pub fn down(code: u32) -> bool {
        if code > 126 {
            return false;
        }
        // SAFETY: CoreGraphics takes a numeric state ID and keycode by value.
        // 0 = kCGEventSourceStateCombinedSessionState in the SDK CGEventTypes.h.
        unsafe { CGEventSourceKeyState(0, code as u16) }
    }
    pub fn caps() -> bool {
        // SAFETY: no pointers or ownership; read-only session flags snapshot.
        unsafe { CGEventSourceFlagsState(0) & (1 << 16) != 0 }
    }
}

#[cfg(target_os = "macos")]
pub use native::{caps, down};

// This workspace targets the Quartz backend. Other OSes can still compile/test
// the pure logic, but must not forward keyboard input with the wrong backend.
#[cfg(not(target_os = "macos"))]
pub fn down(_: u32) -> bool {
    false
}
#[cfg(not(target_os = "macos"))]
pub fn caps() -> bool {
    false
}

pub fn modifiers() -> [bool; 8] {
    crate::keyboard::MODIFIERS.map(down)
}
