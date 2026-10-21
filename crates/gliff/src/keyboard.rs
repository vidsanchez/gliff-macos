//! Pure Quartz → evdev mapping and balanced remote-key lifecycle.
//! No hooks, GTK state, or native queries live in this module.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Command {
    #[default]
    Super,
    Control,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OptionKey {
    #[default]
    Alt,
    Altgr,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    pub command: Command,
    pub option: OptionKey,
}

// Apple's HIToolbox/Events.h kVK_* constants to Linux input-event-codes.h.
// Quartz code 0 is A (not an unknown event); ISO section is a separate key.
// Fn itself is handled by macOS, not a regular key delivered by GDK.
pub const KEYS: &[(u32, u32)] = &[
    (0, 30),
    (1, 31),
    (2, 32),
    (3, 33),
    (4, 35),
    (5, 34),
    (6, 44),
    (7, 45),
    (8, 46),
    (9, 47),
    (10, 86),
    (11, 48),
    (12, 16),
    (13, 17),
    (14, 18),
    (15, 19),
    (16, 21),
    (17, 20),
    (18, 2),
    (19, 3),
    (20, 4),
    (21, 5),
    (22, 7),
    (23, 6),
    (24, 13),
    (25, 10),
    (26, 8),
    (27, 12),
    (28, 9),
    (29, 11),
    (30, 27),
    (31, 24),
    (32, 22),
    (33, 26),
    (34, 23),
    (35, 25),
    (36, 28),
    (37, 38),
    (38, 36),
    (39, 40),
    (40, 37),
    (41, 39),
    (42, 43),
    (43, 51),
    (44, 53),
    (45, 49),
    (46, 50),
    (47, 52),
    (48, 15),
    (49, 57),
    (50, 41),
    (51, 14),
    (53, 1),
    (54, 126),
    (55, 125),
    (56, 42),
    (57, 58),
    (58, 56),
    (59, 29),
    (60, 54),
    (61, 100),
    (62, 97),
    (64, 187),
    (65, 83),
    (67, 55),
    (69, 78),
    (71, 69),
    (72, 115),
    (73, 114),
    (74, 113),
    (75, 98),
    (76, 96),
    (78, 74),
    (79, 188),
    (80, 189),
    (81, 117),
    (82, 82),
    (83, 79),
    (84, 80),
    (85, 81),
    (86, 75),
    (87, 76),
    (88, 77),
    (89, 71),
    (90, 190),
    (91, 72),
    (92, 73),
    (96, 63),
    (97, 64),
    (98, 65),
    (99, 61),
    (100, 66),
    (101, 67),
    (103, 87),
    (105, 183),
    (106, 186),
    (107, 184),
    (109, 68),
    (111, 88),
    (113, 185),
    (114, 110),
    (115, 102),
    (116, 104),
    (117, 111),
    (118, 62),
    (119, 107),
    (120, 60),
    (121, 109),
    (122, 59),
    (123, 105),
    (124, 106),
    (125, 108),
    (126, 103),
];

/// Stable order for left/right Command, Shift, Option and Control snapshots.
pub const MODIFIERS: [u32; 8] = [54, 55, 56, 58, 59, 60, 61, 62];

pub fn quartz_to_evdev(code: u32, policy: Policy) -> Option<u32> {
    if policy.command == Command::Control {
        match code {
            55 => return Some(29),
            54 => return Some(97),
            _ => {}
        }
    }
    KEYS.iter()
        .find_map(|&(mac, evdev)| (mac == code).then_some(evdev))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub code: u32,
    pub pressed: bool,
}
impl Event {
    pub fn down(code: u32) -> Self {
        Self {
            code,
            pressed: true,
        }
    }
    pub fn up(code: u32) -> Self {
        Self {
            code,
            pressed: false,
        }
    }
}

/// One instance belongs to exactly one connection generation, never the tab UI.
/// Keep native identities as well as wire codes: Command→Ctrl can alias Control.
#[derive(Default)]
pub struct Keyboard {
    pressed: BTreeMap<u32, u32>,
    caps: bool,
}
impl Keyboard {
    pub fn press(&mut self, native: u32, policy: Policy) -> Vec<Event> {
        if native == 57 || self.pressed.contains_key(&native) {
            return vec![];
        }
        let Some(code) = quartz_to_evdev(native, policy) else {
            return vec![];
        };
        let already_down = self.pressed.values().any(|&c| c == code);
        self.pressed.insert(native, code);
        if already_down {
            vec![]
        } else {
            vec![Event::down(code)]
        }
    }
    pub fn release(&mut self, native: u32) -> Vec<Event> {
        let Some(code) = self.pressed.remove(&native) else {
            return vec![];
        };
        if self.pressed.values().any(|&c| c == code) {
            vec![]
        } else {
            vec![Event::up(code)]
        }
    }
    pub fn release_all(&mut self) -> Vec<Event> {
        let codes: BTreeSet<_> = std::mem::take(&mut self.pressed).into_values().collect();
        codes.into_iter().map(Event::up).collect()
    }
    pub fn sync_modifiers(&mut self, down: [bool; 8], policy: Policy) -> Vec<Event> {
        let mut events = Vec::new();
        // Add before remove: moving between aliased Command and Control must
        // not release/repress Ctrl while the other physical source is held.
        for (native, held) in MODIFIERS.into_iter().zip(down) {
            if held {
                events.extend(self.press(native, policy));
            }
        }
        for (native, held) in MODIFIERS.into_iter().zip(down) {
            if !held {
                events.extend(self.release(native));
            }
        }
        events
    }
    pub fn sync_caps(&mut self, locked: bool) -> Vec<Event> {
        if self.caps == locked {
            return vec![];
        }
        self.caps = locked;
        vec![Event::down(58), Event::up(58)]
    }
    pub fn held_native(&self) -> Vec<u32> {
        self.pressed.keys().copied().collect()
    }
}

/// Tracks local presses even after the video releases focus. Autorepeat must
/// neither generate another remote down nor count as another hotkey tap.
#[derive(Default)]
pub struct LocalKeys(BTreeSet<u32>);
impl LocalKeys {
    pub fn press(&mut self, native: u32) -> bool {
        self.0.insert(native)
    }
    pub fn release(&mut self, native: u32) {
        self.0.remove(&native);
    }
    pub fn held(&self) -> Vec<u32> {
        self.0.iter().copied().collect()
    }
    pub fn capture(&mut self, held: impl IntoIterator<Item = u32>) {
        self.0 = held.into_iter().collect();
    }
}

/// Session-owned state shared with the optional native callback. The callback
/// only uses try_lock; GTK can retain the existing borrow-style interface.
#[derive(Clone, Default)]
pub struct Shared(std::sync::Arc<SharedInner>);
#[derive(Default)]
struct SharedInner {
    state: std::sync::Mutex<Keyboard>,
    allowed: std::sync::atomic::AtomicBool,
    epoch: std::sync::atomic::AtomicU64,
}
impl Shared {
    pub fn borrow_mut(&self) -> std::sync::MutexGuard<'_, Keyboard> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn borrow(&self) -> std::sync::MutexGuard<'_, Keyboard> {
        self.borrow_mut()
    }
    pub fn try_borrow_mut(&self) -> Option<std::sync::MutexGuard<'_, Keyboard>> {
        match self.0.state.try_lock() {
            Ok(g) => Some(g),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
    pub fn disarm_native(&self) {
        use std::sync::atomic::Ordering;
        self.0.allowed.store(false, Ordering::Release);
        self.0.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn arm_native(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.0.allowed.store(true, Ordering::Release);
        self.0.epoch.load(Ordering::Acquire)
    }
    pub fn native_valid(&self, epoch: u64) -> bool {
        use std::sync::atomic::Ordering;
        self.0.allowed.load(Ordering::Acquire) && self.0.epoch.load(Ordering::Acquire) == epoch
    }
    pub fn same(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quartz_codes_have_no_x11_offset_and_cover_iso_navigation_and_keypad() {
        for (mac, linux) in [
            (0, 30),
            (10, 86),
            (18, 2),
            (24, 13),
            (36, 28),
            (39, 40),
            (41, 39),
            (48, 15),
            (50, 41),
            (51, 14),
            (53, 1),
            (54, 126),
            (55, 125),
            (56, 42),
            (58, 56),
            (59, 29),
            (60, 54),
            (61, 100),
            (62, 97),
            (76, 96),
            (82, 82),
            (92, 73),
            (122, 59),
            (90, 190),
            (123, 105),
            (117, 111),
        ] {
            assert_eq!(
                quartz_to_evdev(mac, Policy::default()),
                Some(linux),
                "Quartz {mac}"
            );
        }
        assert_eq!(quartz_to_evdev(127, Policy::default()), None);
        let mut evdev = std::collections::BTreeSet::new();
        for &(native, code) in KEYS {
            assert_eq!(quartz_to_evdev(native, Policy::default()), Some(code));
            assert!(evdev.insert(code), "duplicate mapping for {native}");
        }
    }

    #[test]
    fn repeats_and_orphan_releases_do_not_generate_remote_events() {
        let mut k = Keyboard::default();
        assert_eq!(k.press(0, Policy::default()), vec![Event::down(30)]);
        assert!(k.press(0, Policy::default()).is_empty());
        assert_eq!(k.release(0), vec![Event::up(30)]);
        assert!(k.release(0).is_empty());
        assert!(k.release(13).is_empty());
    }

    #[test]
    fn command_control_alias_uses_reference_counts() {
        let p = Policy {
            command: Command::Control,
            ..Policy::default()
        };
        let mut k = Keyboard::default();
        assert_eq!(k.press(59, p), vec![Event::down(29)]);
        assert!(k.press(55, p).is_empty());
        assert!(k.release(55).is_empty());
        assert_eq!(k.release(59), vec![Event::up(29)]);
    }

    #[test]
    fn focus_loss_session_switch_and_reconnect_release_exact_owner_once() {
        let mut first = Keyboard::default();
        let mut second = Keyboard::default();
        first.press(56, Policy::default());
        first.press(0, Policy::default());
        let released = first.release_all();
        assert!(released.contains(&Event::up(42)));
        assert!(released.contains(&Event::up(30)));
        assert_eq!(released.len(), 2);
        assert!(first.release_all().is_empty());
        assert!(second.release(0).is_empty());
        assert_eq!(second.press(0, Policy::default()), vec![Event::down(30)]);
        assert_eq!(second.release_all(), vec![Event::up(30)]);
    }

    #[test]
    fn snapshot_reconciles_both_sides_and_modifiers_pressed_outside_video() {
        let mut k = Keyboard::default();
        let mut held = [false; 8];
        held[2] = true; // left Shift
        held[5] = true; // right Shift
        assert_eq!(k.sync_modifiers(held, Policy::default()).len(), 2);
        assert!(k.sync_modifiers(held, Policy::default()).is_empty());
        held[2] = false;
        assert_eq!(
            k.sync_modifiers(held, Policy::default()),
            vec![Event::up(42)]
        );
        assert_eq!(
            k.sync_modifiers([false; 8], Policy::default()),
            vec![Event::up(54)]
        );
    }

    #[test]
    fn caps_lock_is_a_pulse_only_when_the_observed_lock_changes() {
        let mut k = Keyboard::default();
        assert_eq!(k.sync_caps(true), vec![Event::down(58), Event::up(58)]);
        assert!(k.sync_caps(true).is_empty());
        k.release_all();
        assert!(k.sync_caps(true).is_empty());
        assert_eq!(k.sync_caps(false), vec![Event::down(58), Event::up(58)]);
    }

    #[test]
    fn local_physical_tracking_filters_repeated_hotkey_presses() {
        let mut local = LocalKeys::default();
        assert!(local.press(53));
        assert!(!local.press(53)); // must not run double-tap matching again
        local.release(53);
        assert!(local.press(53));
    }
    #[test]
    fn entering_with_a_key_already_held_blocks_its_autorepeat_until_release() {
        let mut keys = LocalKeys::default();
        keys.capture([0, 56]);
        assert!(!keys.press(0));
        keys.release(0);
        assert!(keys.press(0));
    }

    #[test]
    fn modifier_alias_transition_does_not_release_another_held_control() {
        let p = Policy {
            command: Command::Control,
            ..Policy::default()
        };
        let mut k = Keyboard::default();
        let mut cmd = [false; 8];
        cmd[1] = true;
        assert_eq!(k.sync_modifiers(cmd, p), vec![Event::down(29)]);
        let mut ctrl = [false; 8];
        ctrl[4] = true;
        assert!(k.sync_modifiers(ctrl, p).is_empty());
        assert_eq!(k.release_all(), vec![Event::up(29)]);
    }
}
