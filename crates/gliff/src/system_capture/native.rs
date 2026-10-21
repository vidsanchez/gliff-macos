//! All native tap resources are confined to their owning thread. AX only reads
//! the foreground APPLICATION's PID, never UI elements, text or credentials.
use super::{Core, Decision, ForegroundCheck, InitError, InitStage, Raw, Reason};
use std::{
    ffi::c_void,
    ptr,
    sync::{atomic::Ordering, Arc},
};
type P = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        location: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(P, u32, P, P) -> P,
        user: P,
    ) -> P;
    fn CGEventTapEnable(tap: P, enabled: bool);
    fn CGEventTapIsEnabled(tap: P) -> bool;
    fn CGEventGetIntegerValueField(event: P, field: u32) -> i64;
    fn CGEventGetFlags(event: P) -> u64;
    fn CGEventGetTimestamp(event: P) -> u64;
    fn CGPreflightListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
}
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: P) -> u8;
    static kAXTrustedCheckOptionPrompt: P;
    fn AXUIElementCreateSystemWide() -> P;
    fn AXUIElementSetMessagingTimeout(element: P, seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(element: P, attribute: P, value: *mut P) -> i32;
    fn AXUIElementGetPid(element: P, pid: *mut i32) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: P;
    static kCFBooleanTrue: P;
    fn CFDictionaryCreate(
        allocator: P,
        keys: *const P,
        values: *const P,
        count: isize,
        key_callbacks: P,
        value_callbacks: P,
    ) -> P;
    fn CFStringCreateWithCString(allocator: P, string: *const i8, encoding: u32) -> P;
    fn CFMachPortCreateRunLoopSource(allocator: P, port: P, order: isize) -> P;
    fn CFMachPortInvalidate(port: P);
    fn CFRunLoopGetCurrent() -> P;
    fn CFRunLoopAddSource(loop_: P, source: P, mode: P);
    fn CFRunLoopRemoveSource(loop_: P, source: P, mode: P);
    fn CFRunLoopRunInMode(mode: P, seconds: f64, return_after_source: bool) -> i32;
    fn CFRelease(object: P);
}

unsafe extern "C" {
    fn clock_gettime_nsec_np(clock: u32) -> u64;
}
/// Quartz/NX timestamps are nanoseconds of absolute uptime. Apple IOHIDSystem
/// converts its absolute ts to ns for NXEvent.payload.time; Apple documents
/// CLOCK_UPTIME_RAW as the nanosecond equivalent of mach_absolute_time (no sleep).
/// Synthetic CGEvent constructors have timestamp=0 and MUST NOT be used as a clock.
pub fn arm_timestamp() -> u64 {
    // SAFETY: SDK _time.h CLOCK_UPTIME_RAW=8, no event observation/creation.
    unsafe { clock_gettime_nsec_np(8) }
}
pub fn log_diagnostics() {
    // SAFETY: read-only preflight APIs take no arguments and request nothing;
    // no tap and no event data. These booleans are diagnostics,
    // NOT a requirement to grant every permission or proof that a tap can open.
    unsafe {
        tracing::info!(pid=std::process::id(), executable=%std::env::current_exe().map(|p|p.display().to_string()).unwrap_or_else(|_|"unknown".into()),
            ax_trusted=trusted(), listen_preflight=CGPreflightListenEventAccess(), post_preflight=CGPreflightPostEventAccess(),
            uptime_clock_valid=arm_timestamp()!=0, "Native capture preflight (TCC identity may be attributed to terminal)");
    }
}
pub fn request_permission() -> Result<bool, InitError> {
    // SAFETY: static CFString/CFBoolean keys and values outlive this dictionary;
    // null callbacks are valid and avoid retaining these static objects. This
    // API only prompts on the explicit CLI path; it never grants permission.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            ptr::null_mut(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        if options.is_null() {
            return Err(InitError::new(InitStage::PermissionOptions, None));
        }
        let trusted = AXIsProcessTrustedWithOptions(options) != 0;
        CFRelease(options);
        Ok(trusted) // prompt is asynchronous; false does NOT mean prompt failed
    }
}
pub fn trusted() -> bool {
    // SAFETY: read-only preflight; unlike request APIs, never displays a prompt.
    unsafe { AXIsProcessTrusted() != 0 }
}
struct Context {
    core: Arc<Core>,
    ax: P,
    attribute: P,
    pid: i32,
}
impl Context {
    fn foreground(&self) -> bool {
        if !trusted() {
            self.core
                .foreground_check
                .store(ForegroundCheck::PermissionLost.encode(), Ordering::Release);
            self.core.fail(Reason::Permission);
            return false;
        }
        // SAFETY: AX refs belong to this thread and live through the runloop.
        // Only AXFocusedApplication is queried. The configured 5ms timeout
        // bounds IPC; any API failure or missing PID returns fail-open.
        unsafe {
            let mut app = ptr::null_mut();
            let error = AXUIElementCopyAttributeValue(self.ax, self.attribute, &mut app);
            if error != 0 {
                self.core.foreground_check.store(
                    ForegroundCheck::QueryError(error).encode(),
                    Ordering::Release,
                );
                return false;
            }
            if app.is_null() {
                self.core.foreground_check.store(
                    ForegroundCheck::MissingApplication.encode(),
                    Ordering::Release,
                );
                return false;
            }
            let mut pid = 0;
            let result = AXUIElementGetPid(app, &mut pid);
            CFRelease(app);
            let check = if result != 0 {
                ForegroundCheck::PidError(result)
            } else if pid == self.pid {
                ForegroundCheck::SelfProcess
            } else {
                ForegroundCheck::OtherProcess
            };
            self.core
                .foreground_check
                .store(check.encode(), Ordering::Release);
            check == ForegroundCheck::SelfProcess
        }
    }
}
unsafe extern "C" fn callback(_proxy: P, kind: u32, event: P, user: P) -> P {
    if user.is_null() {
        return event;
    }
    // SAFETY: user points to the live boxed Context for this source. No event
    // pointer is retained/copied or passed to GTK; it is borrowed only here.
    let ctx = unsafe { &*(user as *const Context) };
    if event.is_null() && kind < u32::MAX - 1 {
        ctx.core.fail(Reason::Panic);
        return event;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.core.route(
            kind,
            || ctx.foreground(),
            || {
                // Core invokes this closure ONLY after armed/fresh lease, native
                // foreground, owner epoch and live consumer checks have succeeded.
                // SAFETY: keyboard events supply these numeric fields per SDK.
                unsafe {
                    Raw {
                        code: CGEventGetIntegerValueField(event, 9) as u32,
                        flags: CGEventGetFlags(event),
                        repeat: CGEventGetIntegerValueField(event, 8) != 0,
                        modifiers: crate::macos_keyboard::modifiers(),
                        timestamp: CGEventGetTimestamp(event),
                    }
                }
            },
        )
    }));
    match result {
        Ok(Decision::Suppress) => ptr::null_mut(),
        Ok(Decision::Pass) => event,
        Err(_) => {
            ctx.core.fail(Reason::Panic);
            event
        }
    }
}
struct Resources {
    tap: P,
    source: P,
    loop_: P,
    ax: P,
    attribute: P,
}
impl Drop for Resources {
    fn drop(&mut self) {
        // SAFETY: each object was created on and is released on this thread;
        // invalidate/remove callbacks before their boxed user context is freed.
        unsafe {
            if !self.tap.is_null() {
                CGEventTapEnable(self.tap, false);
                CFMachPortInvalidate(self.tap);
            }
            if !self.source.is_null() {
                CFRunLoopRemoveSource(self.loop_, self.source, kCFRunLoopDefaultMode);
                CFRelease(self.source);
            }
            for p in [self.tap, self.ax, self.attribute] {
                if !p.is_null() {
                    CFRelease(p);
                }
            }
        }
    }
}
pub fn run(core: Arc<Core>) {
    // SAFETY: FFI signatures follow CGEvent.h/AXUIElement.h/CoreFoundation SDK.
    // Only this thread owns the native runloop/resources; no AppKit calls.
    unsafe {
        let mut resources = Resources {
            tap: ptr::null_mut(),
            source: ptr::null_mut(),
            loop_: CFRunLoopGetCurrent(),
            ax: AXUIElementCreateSystemWide(),
            attribute: CFStringCreateWithCString(
                ptr::null_mut(),
                c"AXFocusedApplication".as_ptr(),
                0x08000100,
            ),
        };
        if resources.loop_.is_null() {
            core.init_failed(InitError::new(InitStage::RunLoop, None));
            return;
        }
        if resources.ax.is_null() {
            core.init_failed(InitError::new(InitStage::AxSystemWide, None));
            return;
        }
        if resources.attribute.is_null() {
            core.init_failed(InitError::new(InitStage::AxAttribute, None));
            return;
        }
        let ax_error = AXUIElementSetMessagingTimeout(resources.ax, 0.005);
        if ax_error != 0 {
            core.init_failed(InitError::new(InitStage::AxTimeout, Some(ax_error)));
            return;
        }
        let mut context = Box::new(Context {
            core: core.clone(),
            ax: resources.ax,
            attribute: resources.attribute,
            pid: std::process::id() as i32,
        });
        // SESSION (1), head insertion (0), active/default filter (0). Never HID
        // and never root. Mask contains keyboard events only, not mouse/scroll.
        resources.tap = CGEventTapCreate(
            1,
            0,
            0,
            (1 << 10) | (1 << 11) | (1 << 12),
            callback,
            (&mut *context as *mut Context).cast(),
        );
        if resources.tap.is_null() {
            core.init_failed(InitError::new(InitStage::EventTap, None));
            return;
        }
        CGEventTapEnable(resources.tap, false);
        resources.source = CFMachPortCreateRunLoopSource(ptr::null_mut(), resources.tap, 0);
        if resources.source.is_null() {
            core.init_failed(InitError::new(InitStage::RunLoopSource, None));
            return;
        }
        CFRunLoopAddSource(resources.loop_, resources.source, kCFRunLoopDefaultMode);
        if !core.publish_ready() {
            drop(resources);
            drop(context);
            return;
        }
        let mut enabled = false;
        while !core.stop.load(Ordering::Acquire) {
            if core.armed.load(Ordering::Acquire) && !trusted() {
                core.fail(Reason::Permission);
            }
            core.watchdog();
            let desired = core.armed.load(Ordering::Acquire);
            if enabled && desired && !CGEventTapIsEnabled(resources.tap) {
                core.fail(Reason::Disabled);
            }
            let desired = core.armed.load(Ordering::Acquire);
            if desired != enabled {
                CGEventTapEnable(resources.tap, desired);
                enabled = desired;
                core.enabled.store(enabled, Ordering::Release);
            }
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.025, true);
        }
        core.fail(Reason::Focus);
        core.cleanup();
        core.ready.store(false, Ordering::Release);
        core.enabled.store(false, Ordering::Release);
        // Drop resources before context: no callback can outlive userInfo.
        drop(resources);
        drop(context);
    }
}
