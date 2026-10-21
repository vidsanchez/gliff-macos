//! Self-contained XKB text from the selected macOS layout exposed by GDK.
//! No xkbcommon library, XKB data files or compositor are required locally.
//! GDK macOS groups 0/1 are plain/Option, levels 0/1 plain/Shift. translate_key
//! supplies fallback for GDK's deduplicated entries and preserves dead keysyms.

use crate::keyboard::{OptionKey, Policy, KEYS};
use gtk4::glib::translate::IntoGlib;
use gtk4::{gdk, prelude::*};
use std::fmt::Write;

#[derive(Clone, Debug)]
pub struct LayoutKey {
    pub code: u32,
    pub symbols: [u32; 4],
    pub alphabetic: bool,
}

pub struct Source {
    pub text: String,
    policy: Policy,
}
impl Source {
    pub fn new(policy: Policy) -> Self {
        let mut s = Self {
            text: String::new(),
            policy,
        };
        s.refresh();
        s
    }
    /// Called before input, and on a UI timer to update idle connections too.
    /// Empty means unsupported backend: never silently use a US map for input.
    pub fn refresh(&mut self) -> bool {
        let text = collect()
            .map(|keys| build_xkb(&keys, self.policy))
            .unwrap_or_default();
        if text == self.text {
            return false;
        }
        self.text = text;
        true
    }
}

/// Identity for local release-hotkey matching only: no Shift, Caps or Option.
/// Remote input still uses the original physical code and generated keymap.
/// Returning None leaves reported special keys available as a safe fallback.
pub fn unmodified_keyval(code: u32) -> Option<gdk::Key> {
    let display = gdk::Display::default()?;
    if !display.backend().is_macos() {
        return None;
    }
    let (key, _, _, _) = display.translate_key(code, gdk::ModifierType::empty(), 0)?;
    (key != gdk::Key::VoidSymbol && key.into_glib() != 0).then_some(key)
}

fn collect() -> Option<Vec<LayoutKey>> {
    let display = gdk::Display::default()?;
    if !display.backend().is_macos() {
        return None;
    }
    let mut keys = Vec::new();
    for &(native, code) in KEYS {
        let mut symbols = [0; 4];
        let mut chars = [None; 4];
        for (index, symbol) in symbols.iter_mut().enumerate() {
            let state = if index % 2 == 1 {
                gdk::ModifierType::SHIFT_MASK
            } else {
                gdk::ModifierType::empty()
            };
            if let Some((key, _, _, _)) = display.translate_key(native, state, (index / 2) as i32) {
                chars[index] = key.to_unicode();
                let value = key.into_glib();
                *symbol = if value == 0x00ff_ffff { 0 } else { value };
            }
        }
        let alphabetic = match [chars[0], chars[1]] {
            [Some(a), Some(b)] => {
                a != b && a.is_alphabetic() && a.to_uppercase().to_string() == b.to_string()
            }
            _ => false,
        };
        keys.push(LayoutKey {
            code,
            symbols,
            alphabetic,
        });
    }
    // A real layout must at least translate A; do not claim success on empty data.
    keys.iter()
        .any(|k| k.code == 30 && k.symbols[0] != 0)
        .then_some(keys)
}

/// No includes: the remote only needs its XKB compiler, not a local Linux layout.
/// Explicit modifier actions avoid depending on remote compatibility includes.
pub fn build_xkb(keys: &[LayoutKey], policy: Policy) -> String {
    let mut out =
        String::from("xkb_keymap {\nxkb_keycodes \"mac\" { minimum = 8; maximum = 255;\n");
    for &(_, code) in KEYS {
        writeln!(out, "<K{code:03}> = {};", code + 8).unwrap();
    }
    out.push_str("};\nxkb_types \"mac\" {\n");
    out.push_str("type \"ONE_LEVEL\" { modifiers = None; map[None] = Level1; };\n");
    out.push_str(
        "type \"TWO_LEVEL\" { modifiers = Shift; map[None] = Level1; map[Shift] = Level2; };\n",
    );
    out.push_str("type \"ALPHABETIC\" { modifiers = Shift+Lock; map[None] = Level1; map[Shift] = Level2; map[Lock] = Level2; map[Shift+Lock] = Level1; };\n");
    out.push_str("type \"KEYPAD\" { modifiers = None; map[None] = Level1; };\n");
    out.push_str("type \"MAC\" { modifiers = Shift+Mod5; map[None] = Level1; map[Shift] = Level2; map[Mod5] = Level3; map[Shift+Mod5] = Level4; };\n");
    out.push_str("type \"MAC_ALPHA\" { modifiers = Shift+Lock+Mod5; map[None] = Level1; map[Shift] = Level2; map[Lock] = Level2; map[Shift+Lock] = Level1; map[Mod5] = Level3; map[Shift+Mod5] = Level4; map[Lock+Mod5] = Level3; map[Shift+Lock+Mod5] = Level4; };\n");
    out.push_str(
        "};\nxkb_compatibility \"mac\" {};\nxkb_symbols \"mac\" { name[Group1] = \"macOS\";\n",
    );
    // Quartz Help (114) already maps to Linux KEY_INSERT (110) on the wire.
    // GDK may report Help/NoSymbol or omit it on compact Mac layouts. Remote
    // compositor bindings still need to resolve Insert (e.g. Ctrl+Insert and
    // Shift+Insert), even without a physical Insert key. Define its canonical
    // Linux symbol independently of GDK and do not consume shortcut modifiers.
    out.push_str(
        "key <K110> { type[Group1] = \"ONE_LEVEL\", symbols[Group1] = [Insert], repeat = Yes };\n",
    );
    for key in keys.iter().filter(|key| key.code != 110) {
        let (symbol, modifier, action) = match key.code {
            42 => (Some("Shift_L"), "Shift", "SetMods"),
            54 => (Some("Shift_R"), "Shift", "SetMods"),
            29 => (Some("Control_L"), "Control", "SetMods"),
            97 => (Some("Control_R"), "Control", "SetMods"),
            125 => (Some("Super_L"), "Mod4", "SetMods"),
            126 => (Some("Super_R"), "Mod4", "SetMods"),
            56 => {
                if policy.option == OptionKey::Altgr {
                    (Some("ISO_Level3_Shift"), "Mod5", "SetMods")
                } else {
                    (Some("Alt_L"), "Mod1", "SetMods")
                }
            }
            100 => {
                if policy.option == OptionKey::Altgr {
                    (Some("ISO_Level3_Shift"), "Mod5", "SetMods")
                } else {
                    (Some("Alt_R"), "Mod1", "SetMods")
                }
            }
            58 => (Some("Caps_Lock"), "Lock", "LockMods"),
            _ => (None, "", ""),
        };
        if let Some(symbol) = symbol {
            writeln!(out,"key <K{:03}> {{ type[Group1] = \"ONE_LEVEL\", symbols[Group1] = [{symbol}], actions[Group1] = [{action}(modifiers={modifier})], repeat = No }};",key.code).unwrap();
            writeln!(out, "modifier_map {modifier} {{ <K{:03}> }};", key.code).unwrap();
        } else {
            let identical = key.symbols.iter().all(|&s| s == key.symbols[0]);
            let kind = if identical {
                "ONE_LEVEL"
            } else if key.alphabetic {
                "MAC_ALPHA"
            } else {
                "MAC"
            };
            let symbols = if identical {
                format!("0x{:x}", key.symbols[0])
            } else {
                key.symbols
                    .iter()
                    .map(|s| format!("0x{s:x}"))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            // A four-level type would consume Shift/Mod5 in XKB mode even
            // when they do not change Left/F1. Keep those shortcut modifiers.
            writeln!(out,"key <K{:03}> {{ type[Group1] = \"{kind}\", symbols[Group1] = [{symbols}], repeat = Yes }};",key.code).unwrap();
        }
    }
    out.push_str("};\n};\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<LayoutKey> {
        KEYS.iter()
            .map(|&(_, code)| LayoutKey {
                code,
                symbols: match code {
                    105 => [0xff51; 4],                 // Left, unaffected by Shift or Option
                    59 => [0xffbe; 4],                  // F1, unaffected by Shift or Option
                    30 => [0x61, 0x41, 0xe6, 0xc6],     // US A and Option symbols
                    39 => [0xf1, 0xd1, 0x7e, 0xfe53],   // Spanish ñ and dead tilde
                    40 => [0xfe51, 0xfe57, 0x7b, 0x7d], // Spanish dead acute/diaeresis
                    // Spanish ISO C/V metadata checked read-only through the
                    // same Carbon UCKeyTranslate call used by GTK 4.24.1.
                    46 => ['c', 'C', '©', ' '].map(|c| gdk::unicode_to_keyval(c as u32)),
                    47 => ['v', 'V', '√', '◊'].map(|c| gdk::unicode_to_keyval(c as u32)),
                    _ => [0; 4],
                },
                alphabetic: matches!(code, 30 | 39 | 46 | 47),
            })
            .collect()
    }
    #[test]
    fn generated_map_preserves_spanish_and_us_four_levels_and_dead_keys() {
        let map = build_xkb(&fixture(), Policy::default());
        assert!(map.contains("[0x61,0x41,0xe6,0xc6]"));
        assert!(map.contains("[0xf1,0xd1,0x7e,0xfe53]"));
        assert!(map.contains("[0xfe51,0xfe57,0x7b,0x7d]"));
        assert!(map.contains("<K030> = 38;"));
        assert!(!map.contains("include"));
        assert!(map.contains("LockMods(modifiers=Lock)"));
    }
    #[test]
    fn linux_insert_is_always_present_even_if_gdk_reports_help_no_symbol_or_no_entry() {
        assert_eq!(
            crate::keyboard::quartz_to_evdev(114, Policy::default()),
            Some(110)
        );
        let mut expected = None;
        for symbols in [Some([0xff6a; 4]), Some([0; 4]), None] {
            let mut keys = fixture();
            keys.retain(|key| key.code != 110);
            if let Some(symbols) = symbols {
                keys.push(LayoutKey {
                    code: 110,
                    symbols,
                    alphabetic: false,
                });
            }
            for option in [OptionKey::Alt, OptionKey::Altgr] {
                let policy = Policy {
                    option,
                    ..Policy::default()
                };
                let map = build_xkb(&keys, policy);
                let rows: Vec<_> = map
                    .lines()
                    .filter(|line| line.starts_with("key <K110>"))
                    .collect();
                assert_eq!(rows.len(), 1, "Insert must be defined exactly once");
                assert!(
                    rows[0].contains("[Insert]"),
                    "Linux KEY_INSERT must not inherit GDK Help/NoSymbol"
                );
                assert!(
                    rows[0].contains("\"ONE_LEVEL\""),
                    "Shift+Insert must not consume Shift"
                );
                assert!(map.contains("<K110> = 118;"));
                if option == OptionKey::Alt {
                    if let Some(expected) = &expected {
                        assert_eq!(&map, expected);
                    } else {
                        expected = Some(map);
                    }
                }
            }
        }
    }
    #[test]
    fn option_policies_change_both_modifier_action_and_symbols() {
        let keys = fixture();
        let alt = build_xkb(&keys, Policy::default());
        let altgr = build_xkb(
            &keys,
            Policy {
                option: OptionKey::Altgr,
                ..Policy::default()
            },
        );
        assert!(alt.contains("[Alt_L]"));
        assert!(alt.contains("SetMods(modifiers=Mod1)"));
        assert!(!alt.contains("[ISO_Level3_Shift]"));
        assert!(altgr.contains("[ISO_Level3_Shift]"));
        assert!(altgr.contains("SetMods(modifiers=Mod5)"));
    }
    #[test]
    fn export_xkb_fixtures_for_optional_native_validator() {
        if let Some(dir) = std::env::var_os("GLIFF_TEST_KEYMAP_DIR") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            for option in [OptionKey::Alt, OptionKey::Altgr] {
                let name = if option == OptionKey::Alt {
                    "alt"
                } else {
                    "altgr"
                };
                std::fs::write(
                    dir.join(format!("{name}.xkb")),
                    build_xkb(
                        &fixture(),
                        Policy {
                            option,
                            ..Policy::default()
                        },
                    ),
                )
                .unwrap();
            }
        }
    }
    #[test]
    fn navigation_and_function_keys_use_a_modifier_free_type() {
        let map = build_xkb(&fixture(), Policy::default());
        for code in [105, 59] {
            let row = map
                .lines()
                .find(|line| line.starts_with(&format!("key <K{code:03}>")))
                .unwrap();
            assert!(row.contains("\"ONE_LEVEL\""), "{row}");
        }
    }
}
