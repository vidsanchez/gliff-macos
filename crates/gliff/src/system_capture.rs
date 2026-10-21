//! Opt-in session keyboard filter. Pure policy tests never create an event tap.

use crate::{
    clipboard::ToWorker,
    keyboard::{self, Policy, Shared},
    ClientMsg, OutSender,
};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod native;

pub const INPUT_LIMIT: usize = crate::input_queue::NORMAL_LIMIT;
pub const SHIFT: u64 = 1 << 17;
pub const CONTROL: u64 = 1 << 18;
pub const OPTION: u64 = 1 << 19;
pub const COMMAND: u64 = 1 << 20;
const MOD_MASK: u64 = SHIFT | CONTROL | OPTION | COMMAND;
const LEASE_MS: u64 = 150;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitStage {
    AxTrust,
    AxSystemWide,
    AxAttribute,
    AxTimeout,
    EventTap,
    RunLoop,
    RunLoopSource,
    ThreadSpawn,
    ThreadPanic,
    StartupTimeout,
    Clock,
    PermissionOptions,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitError {
    pub stage: InitStage,
    pub code: Option<i32>,
}
impl InitError {
    pub fn new(stage: InitStage, code: Option<i32>) -> Self {
        Self { stage, code }
    }
}
impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.stage)?;
        if let Some(code) = self.code {
            write!(f, " (code={code})")?;
        }
        Ok(())
    }
}
/// Only the exclusive CLI action supplies true. A false path cannot execute
/// the supplied native prompt function; one invocation can request at most once.
pub fn explicit_permission_request<T>(explicit: bool, request: impl FnOnce() -> T) -> Option<T> {
    explicit.then(request)
}
pub fn request_capture_permission() -> Result<bool, InitError> {
    #[cfg(target_os = "macos")]
    {
        native::log_diagnostics();
        native::request_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(InitError::new(InitStage::AxTrust, None))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Idle = 0,
    Emergency = 1,
    Focus = 2,
    Stale = 3,
    Queue = 4,
    Lock = 5,
    Disabled = 6,
    Permission = 7,
    Panic = 8,
    Create = 9,
}
impl Reason {
    pub fn from_u8(n: u8) -> Self {
        match n {
            1 => Self::Emergency,
            2 => Self::Focus,
            3 => Self::Stale,
            4 => Self::Queue,
            5 => Self::Lock,
            6 => Self::Disabled,
            7 => Self::Permission,
            8 => Self::Panic,
            9 => Self::Create,
            _ => Self::Idle,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Pass,
    Suppress,
}

/// Last result of the existing AX foreground check; never includes another PID
/// or key data. The callback publishes only an atomic, and GTK logs the notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundCheck {
    NotChecked,
    SelfProcess,
    OtherProcess,
    QueryError(i32),
    MissingApplication,
    PidError(i32),
    PermissionLost,
}
impl ForegroundCheck {
    fn encode(self) -> u64 {
        let (kind, code) = match self {
            Self::NotChecked => (0, 0),
            Self::SelfProcess => (1, 0),
            Self::OtherProcess => (2, 0),
            Self::QueryError(code) => (3, code),
            Self::MissingApplication => (4, 0),
            Self::PidError(code) => (5, code),
            Self::PermissionLost => (6, 0),
        };
        kind | ((code as u32 as u64) << 32)
    }
    fn decode(value: u64) -> Self {
        let code = (value >> 32) as u32 as i32;
        match value as u32 {
            1 => Self::SelfProcess,
            2 => Self::OtherProcess,
            3 => Self::QueryError(code),
            4 => Self::MissingApplication,
            5 => Self::PidError(code),
            6 => Self::PermissionLost,
            _ => Self::NotChecked,
        }
    }
}
pub struct Raw {
    pub code: u32,
    pub flags: u64,
    pub repeat: bool,
    pub modifiers: [bool; 8],
    pub timestamp: u64,
}
#[derive(Default)]
pub struct Hotkey {
    pub codes: Vec<u32>,
    pub modifiers: u64,
    pub double: bool,
    last: Option<Instant>,
}
impl Hotkey {
    pub fn new(codes: Vec<u32>, modifiers: u64, double: bool) -> Self {
        Self {
            codes,
            modifiers,
            double,
            last: None,
        }
    }
    fn matches(&mut self, raw: &Raw, fresh: bool) -> bool {
        if !fresh || raw.repeat || !self.codes.contains(&raw.code) {
            return false;
        }
        if !self.double {
            return raw.flags & MOD_MASK == self.modifiers;
        }
        let now = Instant::now();
        if self
            .last
            .is_some_and(|old| now.duration_since(old) <= Duration::from_millis(400))
        {
            self.last = None;
            true
        } else {
            self.last = Some(now);
            false
        }
    }
}
pub struct Target {
    tx: OutSender,
    state: Shared,
    epoch: u64,
    armed_at: u64,
    policy: Policy,
    hotkey: Hotkey,
    local: keyboard::LocalKeys,
    modifiers: [bool; 8],
}
impl Target {
    pub fn new(
        tx: OutSender,
        state: Shared,
        policy: Policy,
        hotkey: Hotkey,
        held: Vec<u32>,
    ) -> Self {
        let mut local = keyboard::LocalKeys::default();
        local.capture(held);
        let held_modifiers = state.borrow().held_native();
        let modifiers = keyboard::MODIFIERS.map(|code| held_modifiers.contains(&code));
        Self {
            tx,
            state,
            epoch: 0,
            armed_at: 0,
            policy,
            hotkey,
            local,
            modifiers,
        }
    }
    fn send(&self, events: Vec<keyboard::Event>) -> bool {
        events.into_iter().all(|e| {
            self.tx
                .send(ToWorker::Send(ClientMsg::Key {
                    keycode: e.code,
                    pressed: e.pressed,
                }))
                .is_ok()
        })
    }
}

/// Aggregate flags belong to THIS event; a current-state snapshot may already
/// describe a later key-up. Device-side flags (IOLLEvent.h) take precedence;
/// otherwise retain known sides, then a coherent snapshot, then a left-side
/// fallback. Never infer an absent aggregate modifier from a future snapshot.
fn event_modifiers(kind: u32, raw: &Raw, mut previous: [bool; 8]) -> [bool; 8] {
    for (flag, left, right, left_bit, right_bit) in [
        (COMMAND, 1, 0, 0x8, 0x10),
        (SHIFT, 2, 5, 0x2, 0x4),
        (OPTION, 3, 6, 0x20, 0x40),
        (CONTROL, 4, 7, 0x1, 0x2000),
    ] {
        if raw.flags & flag == 0 {
            previous[left] = false;
            previous[right] = false;
            continue;
        }
        if raw.flags & (left_bit | right_bit) != 0 {
            previous[left] = raw.flags & left_bit != 0;
            previous[right] = raw.flags & right_bit != 0;
        } else if kind == 12
            && [keyboard::MODIFIERS[left], keyboard::MODIFIERS[right]].contains(&raw.code)
        {
            let side = if raw.code == keyboard::MODIFIERS[left] {
                left
            } else {
                right
            };
            let other = if side == left { right } else { left };
            previous[side] = !previous[side] || !previous[other];
        } else if !previous[left] && !previous[right] {
            previous[left] = raw.modifiers[left];
            previous[right] = raw.modifiers[right];
            if !previous[left] && !previous[right] {
                previous[left] = true;
            }
        }
    }
    previous
}

/// Observer metadata is separate from the callback's mutable Target. Observers
/// never lock Target; the callback never locks Owner. Only arm/cleanup acquire
/// both, always Target -> Owner. Epoch/channel validity remains live, not cached.
struct Owner {
    state: Shared,
    tx: OutSender,
    epoch: u64,
}
impl Owner {
    fn valid(&self) -> bool {
        self.state.native_valid(self.epoch) && !self.tx.is_closed()
    }
}

pub struct Core {
    requested: bool,
    start: Instant,
    armed: AtomicBool,
    ready: AtomicBool,
    initializing: AtomicBool,
    init_error: Mutex<Option<InitError>>,
    enabled: AtomicBool,
    stop: AtomicBool,
    deadline: AtomicU64,
    reason: AtomicU8,
    notice: AtomicBool,
    foreground_check: AtomicU64,
    owner: Mutex<Option<Owner>>,
    target: Mutex<Option<Target>>,
}
impl Core {
    fn new(requested: bool) -> Self {
        Self {
            requested,
            start: Instant::now(),
            armed: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            initializing: AtomicBool::new(requested),
            init_error: Mutex::new(None),
            enabled: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            deadline: AtomicU64::new(0),
            reason: AtomicU8::new(0),
            notice: AtomicBool::new(false),
            foreground_check: AtomicU64::new(0),
            owner: Mutex::new(None),
            target: Mutex::new(None),
        }
    }
    fn init_failed(&self, error: InitError) {
        self.fail(Reason::Create);
        *self.init_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        self.reason.store(Reason::Create as u8, Ordering::Release);
        self.initializing.store(false, Ordering::Release);
        self.ready.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        tracing::warn!(stage=%error, "Native keyboard initialization failed; GTK fallback");
    }
    fn publish_ready(&self) -> bool {
        if self.stop.load(Ordering::Acquire) {
            return false;
        }
        self.ready.store(true, Ordering::Release);
        self.initializing.store(false, Ordering::Release);
        tracing::info!("Native keyboard tap ready but disarmed; explicit video click required");
        true
    }
    fn poll_startup(&self) {
        if self.initializing.load(Ordering::Acquire)
            && self.start.elapsed() > Duration::from_secs(5)
        {
            self.init_failed(InitError::new(InitStage::StartupTimeout, None));
        }
    }
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + 1
    }
    fn fail(&self, reason: Reason) {
        // This atomic happens BEFORE taking any lock or touching a channel.
        // Even a stuck UI/network/lock cannot keep intercepting after escape.
        if self.armed.swap(false, Ordering::AcqRel) {
            self.reason.store(reason as u8, Ordering::Release);
            self.notice.store(true, Ordering::Release);
        }
    }
    fn arm(&self, mut next: Target, timestamp: u64) -> bool {
        if !self.requested
            || self.stop.load(Ordering::Acquire)
            || !self.ready.load(Ordering::Acquire)
            || next.tx.is_closed()
        {
            return false;
        }
        if self.owns(&next.state, &next.tx) {
            return true;
        }
        if timestamp == 0 {
            self.fail(Reason::Create);
            *self.init_error.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(InitError::new(InitStage::Clock, None));
            self.reason.store(Reason::Create as u8, Ordering::Release);
            tracing::warn!("Native keyboard arm failed: Clock returned zero; GTK fallback");
            self.cleanup();
            return false;
        }
        self.fail(Reason::Focus);
        self.cleanup();
        let Ok(mut target) = self.target.try_lock() else {
            return false;
        };
        if target.is_some() {
            return false;
        }
        let Ok(mut owner) = self.owner.try_lock() else {
            return false;
        };
        *self.init_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
        next.armed_at = timestamp;
        next.epoch = next.state.arm_native();
        *owner = Some(Owner {
            state: next.state.clone(),
            tx: next.tx.clone(),
            epoch: next.epoch,
        });
        *target = Some(next);
        self.reason.store(0, Ordering::Release);
        self.notice.store(false, Ordering::Release);
        self.foreground_check.store(0, Ordering::Release);
        self.deadline
            .store(self.now() + LEASE_MS, Ordering::Release);
        self.armed.store(true, Ordering::Release);
        true
    }
    fn owns(&self, state: &Shared, tx: &OutSender) -> bool {
        self.armed.load(Ordering::Acquire)
            && self.now() <= self.deadline.load(Ordering::Acquire)
            && self.owner.lock().ok().is_some_and(|owner| {
                owner
                    .as_ref()
                    .is_some_and(|t| t.state.same(state) && t.tx.same_channel(tx) && t.valid())
            })
    }
    fn heartbeat(&self, state: &Shared) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        if self.now() > self.deadline.load(Ordering::Acquire) {
            self.fail(Reason::Stale);
            return;
        }
        // A busy observer is not a failed event. Do not extend the lease unless
        // this tick validates the owner; the watchdog still bounds a stuck UI.
        let Ok(owner) = self.owner.try_lock() else {
            return;
        };
        if owner
            .as_ref()
            .is_some_and(|t| t.state.same(state) && t.valid())
        {
            self.deadline
                .store(self.now() + LEASE_MS, Ordering::Release);
        } else {
            self.fail(Reason::Focus);
        }
    }
    fn watchdog(&self) {
        if self.armed.load(Ordering::Acquire) && self.now() > self.deadline.load(Ordering::Acquire)
        {
            self.fail(Reason::Stale);
        }
        if self.armed.load(Ordering::Acquire) {
            if let Ok(owner) = self.owner.try_lock() {
                if !owner.as_ref().is_some_and(Owner::valid) {
                    self.fail(Reason::Focus);
                }
            }
        }
        self.cleanup();
    }
    fn cleanup(&self) {
        if self.armed.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut target) = self.target.try_lock() else {
            return;
        };
        let Some(t) = target.as_ref() else {
            return;
        };
        t.state.disarm_native();
        let Ok(mut owner) = self.owner.try_lock() else {
            return;
        };
        let Some(mut state) = t.state.try_borrow_mut() else {
            return;
        };
        // Release-only messages may exceed INPUT_LIMIT by at most the bounded
        // set of held physical keys. Never wait for queue space or network.
        let events = state.release_all();
        let _ = t.send(events);
        drop(state);
        *target = None;
        *owner = None;
    }
    fn route(
        &self,
        kind: u32,
        foreground: impl FnOnce() -> bool,
        read: impl FnOnce() -> Raw,
    ) -> Decision {
        if kind >= u32::MAX - 1 {
            self.fail(Reason::Disabled);
            return Decision::Pass;
        }
        if !matches!(kind, 10..=12) || !self.armed.load(Ordering::Acquire) {
            return Decision::Pass;
        }
        if self.now() > self.deadline.load(Ordering::Acquire) {
            self.fail(Reason::Stale);
            return Decision::Pass;
        }
        // Native foreground/permission verification precedes copying key data.
        if !foreground() {
            self.fail(Reason::Focus);
            return Decision::Pass;
        }
        let Ok(mut target) = self.target.try_lock() else {
            self.fail(Reason::Lock);
            return Decision::Pass;
        };
        let Some(t) = target.as_mut() else {
            self.fail(Reason::Focus);
            return Decision::Pass;
        };
        if !t.state.native_valid(t.epoch) || t.tx.is_closed() {
            self.fail(Reason::Focus);
            return Decision::Pass;
        }
        let raw = read();
        if kind == 10 && raw.code == 53 && raw.flags & SHIFT != 0 {
            self.fail(Reason::Emergency);
            return Decision::Suppress;
        }
        if raw.timestamp < t.armed_at {
            self.fail(Reason::Stale);
            return Decision::Pass;
        }
        if keyboard::quartz_to_evdev(raw.code, t.policy).is_none() {
            return Decision::Pass;
        }
        let fresh = if kind == 10 {
            t.local.press(raw.code)
        } else {
            false
        };
        if kind == 10 && t.hotkey.matches(&raw, fresh) {
            self.fail(Reason::Emergency);
            return Decision::Suppress;
        }
        // This queue feeds a bounded network writer (256), so a slow writer
        // eventually applies real pressure here instead of growing forever.
        if t.tx.len() >= INPUT_LIMIT {
            self.fail(Reason::Queue);
            return Decision::Pass;
        }
        let Some(mut state) = t.state.try_borrow_mut() else {
            self.fail(Reason::Lock);
            return Decision::Pass;
        };
        if !self.armed.load(Ordering::Acquire) || !t.state.native_valid(t.epoch) {
            return Decision::Pass;
        }
        t.modifiers = event_modifiers(kind, &raw, t.modifiers);
        let mut events = state.sync_modifiers(t.modifiers, t.policy);
        events.extend(state.sync_caps(raw.flags & (1 << 16) != 0));
        if !keyboard::MODIFIERS.contains(&raw.code) && raw.code != 57 {
            if kind == 10 && fresh && !raw.repeat {
                events.extend(state.press(raw.code, t.policy));
            }
            if kind == 11 {
                t.local.release(raw.code);
                events.extend(state.release(raw.code));
            }
        }
        if t.tx.len() + events.len() > INPUT_LIMIT {
            self.fail(Reason::Queue);
            return Decision::Pass;
        }
        if !t.send(events) {
            self.fail(Reason::Focus);
            return Decision::Pass;
        }
        Decision::Suppress
    }
}

/// Owns the native runloop thread. No native APIs run when requested=false.
pub struct Driver {
    core: Arc<Core>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Driver {
    pub fn new(requested: bool) -> Self {
        let core = Arc::new(Core::new(requested));
        let mut driver = Self { core, thread: None };
        #[cfg(target_os = "macos")]
        if requested {
            native::log_diagnostics();
            if !native::trusted() {
                driver
                    .core
                    .init_failed(InitError::new(InitStage::AxTrust, None));
            } else {
                let core = driver.core.clone();
                match std::thread::Builder::new()
                    .name("gliff-keyboard-tap".into())
                    .spawn(move || {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            native::run(core.clone())
                        }))
                        .is_err()
                        {
                            core.init_failed(InitError::new(InitStage::ThreadPanic, None));
                        }
                    }) {
                    Ok(thread) => driver.thread = Some(thread),
                    Err(error) => driver
                        .core
                        .init_failed(InitError::new(InitStage::ThreadSpawn, error.raw_os_error())),
                }
            }
        }
        driver
    }
    pub fn arm(&self, target: Target) -> bool {
        self.core.poll_startup();
        if !self.core.requested || !self.core.ready.load(Ordering::Acquire) || target.tx.is_closed()
        {
            return false;
        }
        #[cfg(target_os = "macos")]
        let timestamp = native::arm_timestamp();
        #[cfg(not(target_os = "macos"))]
        let timestamp = 0;
        self.core.arm(target, timestamp)
    }
    pub fn owns(&self, state: &Shared, tx: &OutSender) -> bool {
        self.core.owns(state, tx)
    }
    pub fn heartbeat(&self, state: &Shared) {
        self.core.heartbeat(state);
    }
    pub fn disarm(&self) {
        self.core.fail(Reason::Focus);
        self.core.cleanup();
    }
    pub fn active(&self) -> bool {
        self.core.enabled.load(Ordering::Acquire)
            && self.core.armed.load(Ordering::Acquire)
            && self.core.now() <= self.core.deadline.load(Ordering::Acquire)
            && self
                .core
                .owner
                .lock()
                .ok()
                .is_some_and(|owner| owner.as_ref().is_some_and(Owner::valid))
    }
    pub fn notice(&self) -> Option<Reason> {
        self.core
            .notice
            .swap(false, Ordering::AcqRel)
            .then(|| Reason::from_u8(self.core.reason.load(Ordering::Acquire)))
    }
    pub fn foreground_check(&self) -> ForegroundCheck {
        ForegroundCheck::decode(self.core.foreground_check.load(Ordering::Acquire))
    }
    pub fn status(&self) -> String {
        if !self.core.requested {
            return "Keyboard: normal GTK — system shortcuts stay local".into();
        }
        self.core.poll_startup();
        if let Some(error) = *self
            .core
            .init_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            let action = if error.stage == InitStage::AxTrust {
                "run --request-capture-permissions explicitly, approve in Settings, then restart"
            } else {
                "see CLI stage/preflight diagnostic; GTK normal"
            };
            return format!("Native capture unavailable: {error} — {action}");
        }
        if self.core.initializing.load(Ordering::Acquire) {
            return "Native keyboard starting (not capturing); wait, then click video".into();
        }
        if self.active() {
            return "Native keyboard active — Shift+Esc ALWAYS releases locally".into();
        }
        match Reason::from_u8(self.core.reason.load(Ordering::Acquire)) {
            Reason::Permission => {
                "Native capture permission lost — GTK normal; approve manually then restart"
            }
            Reason::Create => "Native capture unavailable — see CLI diagnostic; GTK normal",
            Reason::Idle => {
                "Keyboard: GTK normal — click connected video to arm native capture"
            }
            Reason::Focus => "Native capture released: Focus — GTK normal; click connected video to arm native capture",
            Reason::Emergency => "Native capture released: Emergency — GTK normal; click video explicitly to retry",
            Reason::Stale => "Native capture released: Stale lease/event — GTK normal; click video explicitly to retry",
            Reason::Queue => "Native capture released: Queue pressure — GTK normal; click video explicitly to retry",
            Reason::Lock => "Native capture released: Lock unavailable — GTK normal; click video explicitly to retry",
            Reason::Disabled => "Native capture released: Disabled tap — GTK normal; click video explicitly to retry",
            Reason::Panic => "Native capture released: Panic — GTK normal; click video explicitly to retry",
        }
        .into()
    }
    pub fn shutdown(&mut self) {
        self.core.fail(Reason::Focus);
        self.core.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // Never wait on UI for an OS initializer/callback that may not
            // return. Dropping an unfinished JoinHandle detaches ONLY the
            // handle: the thread retains its Arc/Core, Context and native
            // resources, and releases them itself when the native call returns.
            // publish_ready/arm both honor stop, so late init cannot capture.
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
        self.core.cleanup();
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn housekeeping_and_observation_do_not_contend_with_event_target() {
        for operation in 0..4 {
            let (core, tx, _rx, state) = armed();
            core.enabled.store(true, Ordering::Release);
            core.deadline.store(u64::MAX, Ordering::Release);
            let target = core.target.lock().unwrap();
            let worker = core.clone();
            let driver = Driver {
                core: core.clone(),
                thread: None,
            };
            let (done, result) = std::sync::mpsc::channel();
            let observed = std::thread::scope(|scope| {
                let driver = &driver;
                let thread = scope.spawn(move || {
                    let ok = match operation {
                        0 => {
                            worker.heartbeat(&state);
                            worker.armed.load(Ordering::Acquire)
                        }
                        1 => {
                            worker.watchdog();
                            worker.armed.load(Ordering::Acquire)
                        }
                        2 => driver.active() && driver.status().contains("Native keyboard active"),
                        _ => driver.owns(&state, &tx),
                    };
                    done.send(ok).unwrap();
                });
                let observed = result.recv_timeout(Duration::from_secs(2));
                drop(target); // always unblock a RED implementation before asserting
                thread.join().unwrap();
                observed
            });
            assert!(observed.unwrap(), "operation {operation}");
            assert!(core.armed.load(Ordering::Acquire));
        }
    }

    #[test]
    fn expired_lease_disarms_even_when_event_target_is_busy_and_never_rearms() {
        let (core, _tx, _rx, state) = armed();
        let target = core.target.lock().unwrap();
        core.deadline.store(0, Ordering::Release);
        core.watchdog();
        assert!(!core.armed.load(Ordering::Acquire));
        assert_eq!(core.reason.load(Ordering::Acquire), Reason::Stale as u8);
        core.heartbeat(&state);
        assert!(!core.armed.load(Ordering::Acquire));
        drop(target);
    }

    #[test]
    fn observer_invalidations_and_late_previous_owner_never_refresh_a_new_lease() {
        for closed in [false, true] {
            let (core, tx, rx, state) = armed();
            if closed {
                drop(rx);
            } else {
                state.disarm_native();
            }
            core.heartbeat(&state);
            assert!(!core.armed.load(Ordering::Acquire));
            assert!(!core.owns(&state, &tx));
        }
        let (core, _tx, _rx, old) = armed();
        core.fail(Reason::Focus);
        core.cleanup();
        let next = Shared::default();
        let (tx, _rx) = crate::input_queue::channel();
        assert!(core.arm(
            Target::new(
                tx.clone(),
                next.clone(),
                Policy::default(),
                Hotkey::default(),
                vec![]
            ),
            200
        ));
        core.heartbeat(&old);
        assert!(!core.armed.load(Ordering::Acquire));
        assert!(!core.owns(&next, &tx));
    }

    #[test]
    fn busy_observer_skips_ticks_without_renewing_and_cannot_block_expiration() {
        let (core, _tx, _rx, state) = armed();
        let owner = core.owner.lock().unwrap();
        let deadline = u64::MAX - 1;
        core.deadline.store(deadline, Ordering::Release);
        core.heartbeat(&state);
        core.watchdog();
        assert!(core.armed.load(Ordering::Acquire));
        assert_eq!(core.deadline.load(Ordering::Acquire), deadline);
        core.deadline.store(0, Ordering::Release);
        core.watchdog();
        assert!(!core.armed.load(Ordering::Acquire));
        assert_eq!(core.reason.load(Ordering::Acquire), Reason::Stale as u8);
        drop(owner);
        core.heartbeat(&state);
        assert!(!core.armed.load(Ordering::Acquire));
    }

    #[test]
    fn callback_and_emergency_never_take_observer_lock() {
        let (core, _tx, _rx, _state) = armed();
        let owner = core.owner.lock().unwrap();
        assert_eq!(core.route(10, || true, || raw(0, 0)), Decision::Suppress);
        assert_eq!(
            core.route(10, || true, || raw(53, SHIFT)),
            Decision::Suppress
        );
        assert!(!core.armed.load(Ordering::Acquire));
        assert_eq!(core.reason.load(Ordering::Acquire), Reason::Emergency as u8);
        drop(owner);
    }

    #[test]
    fn expired_heartbeat_cannot_extend_a_lease_without_watchdog() {
        let (core, _tx, _rx, state) = armed();
        core.deadline.store(0, Ordering::Release);
        core.heartbeat(&state);
        assert!(!core.armed.load(Ordering::Acquire));
        assert_eq!(core.deadline.load(Ordering::Acquire), 0);
        assert_eq!(core.reason.load(Ordering::Acquire), Reason::Stale as u8);
    }

    #[test]
    fn foreground_diagnostic_preserves_codes_resets_on_arm_and_notice_is_once() {
        for check in [
            ForegroundCheck::NotChecked,
            ForegroundCheck::SelfProcess,
            ForegroundCheck::OtherProcess,
            ForegroundCheck::QueryError(-25204),
            ForegroundCheck::MissingApplication,
            ForegroundCheck::PidError(-25202),
            ForegroundCheck::PermissionLost,
        ] {
            assert_eq!(ForegroundCheck::decode(check.encode()), check);
        }
        let (core, tx, _rx, state) = armed();
        let driver = Driver {
            core: core.clone(),
            thread: None,
        };
        core.foreground_check.store(
            ForegroundCheck::QueryError(-25204).encode(),
            Ordering::Release,
        );
        core.fail(Reason::Focus);
        core.fail(Reason::Lock); // later cleanup/disarm cannot overwrite cause
        driver.disarm(); // cleanup's Focus must preserve the first AX failure
        assert_eq!(driver.notice(), Some(Reason::Focus));
        assert_eq!(driver.notice(), None);
        assert_eq!(
            driver.foreground_check(),
            ForegroundCheck::QueryError(-25204)
        );
        assert!(driver.status().contains("Focus"));
        core.cleanup();
        assert!(core.arm(
            Target::new(tx, state, Policy::default(), Hotkey::default(), vec![]),
            200
        ));
        assert_eq!(driver.foreground_check(), ForegroundCheck::NotChecked);
    }

    #[test]
    fn status_names_each_latched_release_without_key_data() {
        for reason in [
            Reason::Focus,
            Reason::Emergency,
            Reason::Stale,
            Reason::Queue,
            Reason::Lock,
            Reason::Disabled,
            Reason::Panic,
        ] {
            let (core, _tx, _rx, _state) = armed();
            let driver = Driver {
                core: core.clone(),
                thread: None,
            };
            core.fail(reason);
            assert!(driver.status().contains(&format!("{reason:?}")));
        }
    }

    #[test]
    fn repeated_arm_preserves_held_click_modifiers_and_fifo() {
        for (code, flag, expected) in [(56, SHIFT, 42), (59, CONTROL, 29), (55, COMMAND, 125)] {
            let (core, tx, mut rx, state) = armed();
            let mut event = raw(code, flag);
            let index = keyboard::MODIFIERS.iter().position(|&n| n == code).unwrap();
            event.modifiers[index] = true;
            assert_eq!(core.route(12, || true, || event), Decision::Suppress);
            for _ in 0..2 {
                assert!(core.arm(
                    Target::new(
                        tx.clone(),
                        state.clone(),
                        Policy::default(),
                        Hotkey::default(),
                        vec![]
                    ),
                    200
                ));
                tx.send(ToWorker::Send(ClientMsg::PointerButton {
                    button: 272,
                    pressed: true,
                }))
                .unwrap();
            }
            assert!(
                matches!(rx.try_recv().unwrap(), ToWorker::Send(ClientMsg::Key { keycode, pressed:true }) if keycode==expected)
            );
            for _ in 0..2 {
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    ToWorker::Send(ClientMsg::PointerButton { pressed: true, .. })
                ));
            }
            assert!(
                rx.try_recv().is_err(),
                "rearming same owner must not release modifier"
            );
        }
    }
    #[test]
    fn event_flags_override_a_snapshot_that_has_already_released_command_and_caps() {
        for command in [keyboard::Command::Super, keyboard::Command::Control] {
            for (device, expected) in [
                (
                    8,
                    if command == keyboard::Command::Super {
                        125
                    } else {
                        29
                    },
                ),
                (
                    16,
                    if command == keyboard::Command::Super {
                        126
                    } else {
                        97
                    },
                ),
            ] {
                let (core, _tx, mut rx, _state) = armed();
                core.target.lock().unwrap().as_mut().unwrap().policy.command = command;
                assert_eq!(
                    core.route(10, || true, || raw(49, COMMAND | device)),
                    Decision::Suppress
                );
                assert!(
                    matches!(rx.try_recv().unwrap(), ToWorker::Send(ClientMsg::Key { keycode, pressed:true }) if keycode==expected)
                );
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    ToWorker::Send(ClientMsg::Key {
                        keycode: 57,
                        pressed: true
                    })
                ));
                assert!(rx.try_recv().is_err());
                assert_eq!(core.route(11, || true, || raw(49, 0)), Decision::Suppress);
            }
        }
        let (core, _, mut rx, _) = armed();
        assert_eq!(
            core.route(12, || true, || raw(57, 1 << 16)),
            Decision::Suppress
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::Key {
                keycode: 58,
                pressed: true
            })
        ));
    }
    #[test]
    fn every_modifier_side_obeys_event_bits_and_policy_despite_incoherent_snapshot() {
        // Independent hardware/evdev expectations, not derived from the table
        // under test. Option AltGr changes XKB actions, not wire keycodes.
        let cases = [
            (55, COMMAND, 0x8, 125, 29),
            (54, COMMAND, 0x10, 126, 97),
            (56, SHIFT, 0x2, 42, 42),
            (60, SHIFT, 0x4, 54, 54),
            (58, OPTION, 0x20, 56, 56),
            (61, OPTION, 0x40, 100, 100),
            (59, CONTROL, 0x1, 29, 29),
            (62, CONTROL, 0x2000, 97, 97),
        ];
        for command in [keyboard::Command::Super, keyboard::Command::Control] {
            for option in [keyboard::OptionKey::Alt, keyboard::OptionKey::Altgr] {
                for (native, aggregate, device, super_code, control_code) in cases {
                    let expected = if command == keyboard::Command::Control {
                        control_code
                    } else {
                        super_code
                    };
                    let (core, _tx, mut rx, _state) = armed();
                    core.target.lock().unwrap().as_mut().unwrap().policy =
                        Policy { command, option };
                    let mut event = raw(native, aggregate | device);
                    // The instantaneous snapshot disagrees on EVERY side.
                    event.modifiers = [true; 8];
                    assert_eq!(core.route(12, || true, || event), Decision::Suppress);
                    assert!(
                        matches!(rx.try_recv().unwrap(), ToWorker::Send(ClientMsg::Key {keycode,pressed:true}) if keycode==expected),
                        "native={native}, command={command:?}, option={option:?}"
                    );
                    assert!(rx.try_recv().is_err());
                    let mut released = raw(native, 0);
                    released.modifiers = [true; 8];
                    assert_eq!(core.route(12, || true, || released), Decision::Suppress);
                    assert!(
                        matches!(rx.try_recv().unwrap(), ToWorker::Send(ClientMsg::Key {keycode,pressed:false}) if keycode==expected)
                    );
                    assert!(rx.try_recv().is_err());
                }
            }
        }
    }
    #[test]
    fn aggregate_flags_with_no_coherent_side_use_fallback_and_preserve_known_side() {
        let mut event = raw(49, COMMAND);
        event.modifiers[4] = true; // inconsistent current Control snapshot
        let fallback = event_modifiers(10, &event, [false; 8]);
        assert!(fallback[1]);
        assert!(!fallback[4]);
        let mut right = [false; 8];
        right[0] = true;
        assert_eq!(event_modifiers(10, &event, right), right);
        let cleared = event_modifiers(11, &raw(49, 0), right);
        assert_eq!(cleared, [false; 8]);
        // Without device-side bits, flagsChanged toggles a known side while
        // an aggregate Command remains held by the other side.
        let mut both = right;
        both[1] = true;
        let after = event_modifiers(12, &raw(55, COMMAND), both);
        assert!(after[0]);
        assert!(!after[1]);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn arm_clock_is_nonzero_and_monotonic_without_capture_or_permission() {
        let first = native::arm_timestamp();
        let second = native::arm_timestamp();
        assert!(first > 0, "synthetic CGEvent timestamps are not a clock");
        assert!(second >= first);
    }
    #[test]
    fn init_errors_report_each_stage_and_ax_code_without_event_data() {
        for stage in [
            InitStage::AxTrust,
            InitStage::AxSystemWide,
            InitStage::AxAttribute,
            InitStage::AxTimeout,
            InitStage::EventTap,
            InitStage::RunLoop,
            InitStage::RunLoopSource,
            InitStage::ThreadSpawn,
            InitStage::ThreadPanic,
            InitStage::StartupTimeout,
            InitStage::PermissionOptions,
        ] {
            let core = Arc::new(Core::new(true));
            let code = (stage == InitStage::AxTimeout).then_some(-25201);
            core.init_failed(InitError::new(stage, code));
            let driver = Driver {
                core: core.clone(),
                thread: None,
            };
            let status = driver.status();
            assert!(status.contains(&format!("{stage:?}")), "{status}");
            if code.is_some() {
                assert!(status.contains("-25201"));
            }
            assert!(
                !core.publish_ready(),
                "failed startup cannot later enable tap"
            );
            assert!(!driver.active());
            assert_eq!(
                core.route(10, || panic!("not queried"), || panic!("not read")),
                Decision::Pass
            );
        }
    }
    #[test]
    fn startup_wait_and_timeout_are_not_mislabeled_as_tap_creation() {
        let mut core = Core::new(true);
        core.start = Instant::now() - Duration::from_secs(6);
        let driver = Driver {
            core: Arc::new(core),
            thread: None,
        };
        assert!(driver.status().contains("StartupTimeout"));
        let driver = Driver {
            core: Arc::new(Core::new(true)),
            thread: None,
        };
        assert!(driver.status().contains("starting"));
        assert!(driver.core.publish_ready());
        assert!(driver.status().contains("click connected video"));
    }
    #[test]
    fn zero_arm_clock_is_a_clock_failure_not_tap_init_failure() {
        let (core, tx, _rx, state) = armed();
        core.fail(Reason::Focus);
        core.cleanup();
        assert!(!core.arm(
            Target::new(tx, state, Policy::default(), Hotkey::default(), vec![]),
            0
        ));
        let driver = Driver { core, thread: None };
        assert!(driver.status().contains("Clock"));
        assert!(!driver.status().contains("EventTap"));
    }
    #[test]
    fn permission_prompt_is_explicit_once_and_never_automatic() {
        assert_eq!(
            explicit_permission_request(false, || panic!("no automatic prompts")),
            None::<bool>
        );
        let count = std::cell::Cell::new(0);
        let current = explicit_permission_request(true, || {
            count.set(count.get() + 1);
            false
        });
        assert_eq!(current, Some(false)); // async prompt is NOT a synchronous grant
        assert_eq!(count.get(), 1);
    }
    #[test]
    fn shutdown_never_waits_for_blocked_init_and_late_thread_cleans_without_activating() {
        let mut core = Core::new(true);
        core.start = Instant::now() - Duration::from_secs(6);
        let core = Arc::new(core);
        let owner = core.clone();
        let cleaned = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(AtomicBool::new(false));
        let cleanup_flag = cleaned.clone();
        let activation_flag = activated.clone();
        let (release, blocked) = std::sync::mpsc::channel();
        let (native_done, native_finished) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            // Model native resources/context exclusively owned by this thread.
            struct Resources(Arc<AtomicBool>);
            impl Drop for Resources {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let resources = Resources(cleanup_flag);
            blocked.recv().unwrap();
            activation_flag.store(owner.publish_ready(), Ordering::Release);
            drop(resources);
            native_done.send(()).unwrap();
        });
        let mut driver = Driver {
            core,
            thread: Some(thread),
        };
        assert!(driver.status().contains("StartupTimeout"));
        let (returned, done) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            driver.shutdown();
            drop(driver);
            returned.send(()).unwrap();
        });
        let returned_before_release = done.recv_timeout(Duration::from_millis(100)).is_ok();
        let cleaned_before_release = cleaned.load(Ordering::Acquire);
        // Always unblock/join the test worker even on RED; never hang the suite.
        release.send(()).unwrap();
        native_finished
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        shutdown.join().unwrap();
        assert!(
            returned_before_release,
            "UI shutdown waited for blocked initialization"
        );
        assert!(
            !cleaned_before_release,
            "UI freed resources still owned by blocked thread"
        );
        assert!(cleaned.load(Ordering::Acquire));
        assert!(
            !activated.load(Ordering::Acquire),
            "late initializer activated after stop/timeout"
        );
    }
    #[test]
    fn outside_capture_does_not_read_key_data() {
        let core = Core::new(true);
        assert_eq!(
            core.route(10, || true, || panic!("must not read outside capture")),
            Decision::Pass
        );
    }
    #[test]
    fn emergency_is_independent_of_configurable_hotkey_and_queue_capacity() {
        let (core, tx, mut rx, state) = armed();
        state.borrow_mut().press(55, Policy::default());
        for _ in 0..INPUT_LIMIT {
            tx.send(ToWorker::Zoom(1)).unwrap();
        }
        assert_eq!(
            core.route(10, || true, || raw(53, SHIFT)),
            Decision::Suppress
        );
        assert!(!core.armed.load(Ordering::Acquire));
        core.cleanup();
        while let Ok(m) = rx.try_recv() {
            if matches!(
                m,
                ToWorker::Send(ClientMsg::Key {
                    keycode: 125,
                    pressed: false
                })
            ) {
                return;
            }
        }
        panic!("owner's release must be queued without waiting for network/UI");
    }
    #[test]
    fn stale_foreground_lease_generation_closed_and_full_channel_fail_open() {
        for failure in 0..5 {
            let (core, tx, rx, state) = armed();
            match failure {
                0 => core.deadline.store(0, Ordering::Release),
                1 => state.disarm_native(),
                2 => drop(rx),
                3 => {
                    for _ in 0..INPUT_LIMIT {
                        tx.send(ToWorker::Zoom(1)).unwrap();
                    }
                }
                _ => {}
            }
            assert_eq!(
                core.route(10, || failure != 4, || raw(0, 0)),
                Decision::Pass
            );
            assert!(!core.armed.load(Ordering::Acquire));
            // A heartbeat never silently re-arms a failed capture.
            core.heartbeat(&state);
            assert!(!core.armed.load(Ordering::Acquire));
        }
    }
    #[test]
    fn successful_forward_has_one_down_one_up_and_filters_repeat() {
        let (core, _tx, mut rx, _state) = armed();
        assert_eq!(core.route(10, || true, || raw(0, 0)), Decision::Suppress);
        let mut repeat = raw(0, 0);
        repeat.repeat = true;
        assert_eq!(core.route(10, || true, || repeat), Decision::Suppress);
        assert_eq!(core.route(11, || true, || raw(0, 0)), Decision::Suppress);
        let mut events = Vec::new();
        while let Ok(ToWorker::Send(ClientMsg::Key { keycode, pressed })) = rx.try_recv() {
            events.push((keycode, pressed));
        }
        assert_eq!(events, [(30, true), (30, false)]);
    }
    #[test]
    fn disabled_and_mutex_contention_are_latched_fail_open() {
        let (core, _tx, _rx, state) = armed();
        let guard = state.borrow_mut();
        assert_eq!(core.route(10, || true, || raw(0, 0)), Decision::Pass);
        drop(guard);
        core.cleanup();
        assert!(!core.armed.load(Ordering::Acquire));
        let (core, _, _, _) = armed();
        assert_eq!(
            core.route(
                u32::MAX - 1,
                || panic!("not queried"),
                || panic!("not read")
            ),
            Decision::Pass
        );
        assert!(!core.armed.load(Ordering::Acquire));
    }
    #[test]
    fn timestamp_boundary_rejects_prearm_events_and_zero_clock() {
        let (core, tx, mut rx, state) = armed();
        let mut old = raw(0, 0);
        old.timestamp = 99;
        assert_eq!(core.route(10, || true, || old), Decision::Pass);
        assert!(rx.try_recv().is_err());
        assert!(!core.armed.load(Ordering::Acquire));
        core.cleanup();
        assert!(!core.arm(
            Target::new(
                tx.clone(),
                state.clone(),
                Policy::default(),
                Hotkey::default(),
                vec![]
            ),
            0
        ));
        assert!(core.arm(
            Target::new(
                tx,
                state.clone(),
                Policy::default(),
                Hotkey::default(),
                vec![]
            ),
            200
        ));
        let mut boundary = raw(0, 0);
        boundary.timestamp = 200;
        assert_eq!(core.route(10, || true, || boundary), Decision::Suppress);
        // A layout/focus epoch invalidation also rejects already queued events.
        state.disarm_native();
        assert_eq!(core.route(11, || true, || raw(0, 0)), Decision::Pass);
    }
    #[test]
    fn opt_out_and_unavailable_tap_never_arm_or_read() {
        let driver = Driver::new(false); // no native permission or tap APIs
        assert!(driver.status().contains("normal GTK"));
        let core = Core::new(true); // permission/create failure keeps ready=false
        let (tx, _rx) = crate::input_queue::channel();
        assert!(!core.arm(
            Target::new(
                tx,
                Shared::default(),
                Policy::default(),
                Hotkey::default(),
                vec![]
            ),
            100
        ));
        assert_eq!(
            core.route(
                10,
                || panic!("foreground not queried"),
                || panic!("not read")
            ),
            Decision::Pass
        );
    }
    #[test]
    fn emergency_disarms_even_with_keyboard_lock_busy_then_releases_owner() {
        let (core, _tx, mut rx, state) = armed();
        state.borrow_mut().press(0, Policy::default());
        let guard = state.borrow_mut();
        assert_eq!(
            core.route(10, || true, || raw(53, SHIFT)),
            Decision::Suppress
        );
        assert!(!core.armed.load(Ordering::Acquire));
        core.cleanup(); // best effort: never waits for this held lock
        drop(guard);
        core.cleanup();
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::Key {
                keycode: 30,
                pressed: false
            })
        ));
    }
    #[test]
    fn switching_owner_releases_only_old_connection() {
        let (core, _tx, mut old_rx, old) = armed();
        assert_eq!(core.route(10, || true, || raw(0, 0)), Decision::Suppress);
        let (new_tx, mut new_rx) = crate::input_queue::channel();
        let new = Shared::default();
        assert!(core.arm(
            Target::new(
                new_tx,
                new.clone(),
                Policy::default(),
                Hotkey::default(),
                vec![]
            ),
            200
        ));
        assert!(!old.native_valid(0));
        assert!(matches!(
            old_rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::Key { pressed: true, .. })
        ));
        assert!(matches!(
            old_rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::Key { pressed: false, .. })
        ));
        assert!(new_rx.try_recv().is_err());
        assert_eq!(core.route(10, || true, || raw(1, 0)), Decision::Suppress);
        assert!(matches!(
            new_rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::Key {
                keycode: 31,
                pressed: true
            })
        ));
    }
    #[test]
    fn native_hotkey_repeat_is_not_a_double_tap_and_command_space_is_single() {
        let mut h = Hotkey::new(vec![53], 0, true);
        assert!(!h.matches(&raw(53, 0), true));
        let mut repeat = raw(53, 0);
        repeat.repeat = true;
        assert!(!h.matches(&repeat, true));
        assert!(!h.matches(&raw(53, 0), false));
        assert!(h.matches(&raw(53, 0), true));
        let (core, _tx, mut rx, _state) = armed();
        let mut space = raw(49, COMMAND);
        space.modifiers[1] = true;
        assert_eq!(core.route(10, || true, || space), Decision::Suppress);
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::Key {
                keycode: 125,
                pressed: true
            })
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            ToWorker::Send(ClientMsg::Key {
                keycode: 57,
                pressed: true
            })
        ));
        assert!(rx.try_recv().is_err());
    }
    fn raw(code: u32, flags: u64) -> Raw {
        Raw {
            code,
            flags,
            repeat: false,
            modifiers: [false; 8],
            timestamp: u64::MAX,
        }
    }
    fn armed() -> (Arc<Core>, OutSender, crate::input_queue::Receiver, Shared) {
        let core = Arc::new(Core::new(true));
        assert!(core.publish_ready());
        let (tx, rx) = crate::input_queue::channel();
        let state = Shared::default();
        assert!(core.arm(
            Target::new(
                tx.clone(),
                state.clone(),
                Policy::default(),
                Hotkey::default(),
                vec![]
            ),
            100
        ));
        (core, tx, rx, state)
    }
}
