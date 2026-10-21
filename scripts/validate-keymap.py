"""Optional dev-only XKB compiler/state/compose validation, never client runtime.

First export fixtures with GLIFF_TEST_KEYMAP_DIR=/tmp/gliff-keymap-fixtures
cargo test --offline -p gliff keymap::tests::export_xkb_fixtures.
Usage: python3 scripts/validate-keymap.py /absolute/libxkbcommon.dylib /tmp/gliff-keymap-fixtures
The caller supplies an existing library; this script installs/downloads nothing.
"""
import ctypes as c
from pathlib import Path
import sys

lib = c.CDLL(sys.argv[1])
P, U, I = c.c_void_p, c.c_uint32, c.c_int

def fn(name, result, *args):
    f = getattr(lib, name)
    f.restype, f.argtypes = result, args
    return f

context_new = fn("xkb_context_new", P, I)
compile_map = fn("xkb_keymap_new_from_string", P, P, c.c_char_p, I, I)
state_new = fn("xkb_state_new", P, P)
update = fn("xkb_state_update_key", I, P, U, I)
symbol = fn("xkb_state_key_get_one_sym", U, P, U)
keysym_from_name = fn("xkb_keysym_from_name", U, c.c_char_p, I)
consumed = fn("xkb_state_key_get_consumed_mods2", U, P, U, I)
serialize_mods = fn("xkb_state_serialize_mods", U, P, I)
state_unref = fn("xkb_state_unref", None, P)
map_unref = fn("xkb_keymap_unref", None, P)
context_unref = fn("xkb_context_unref", None, P)
compose_table = fn("xkb_compose_table_new_from_buffer", P, P, c.c_char_p, c.c_size_t, c.c_char_p, I, I)
compose_new = fn("xkb_compose_state_new", P, P, I)
compose_feed = fn("xkb_compose_state_feed", I, P, U)
compose_utf8 = fn("xkb_compose_state_get_utf8", I, P, c.c_char_p, c.c_size_t)
compose_state_unref = fn("xkb_compose_state_unref", None, P)
compose_table_unref = fn("xkb_compose_table_unref", None, P)

ctx = context_new(1)  # No default includes: prove maps are self-contained.
assert ctx
try:
    for mode in ("alt", "altgr"):
        text = (Path(sys.argv[2]) / f"{mode}.xkb").read_bytes()
        keymap = compile_map(ctx, text, 1, 0)
        assert keymap, f"invalid XKB {mode} map"
        state = state_new(keymap)
        assert state
        try:
            def key(evdev, pressed):
                update(state, evdev + 8, int(pressed))
            def sym(evdev):
                return symbol(state, evdev + 8)
            # XKB mode (0), not GTK mode: unchanged keys must not consume
            # Shift/Option, otherwise apps lose Shift+Left/Shift+F1 shortcuts.
            key(42, True)
            for evdev, expected in ((105, 0xff51), (59, 0xffbe)):
                assert sym(evdev) == expected
                mask = consumed(state, evdev + 8, 0)
                depressed = serialize_mods(state, 1)
                assert depressed & 1, "Shift must be depressed"
                assert not mask & 1, f"{mode}: key {evdev} consumed Shift: {mask}"
            key(56, True)
            for evdev in (105, 59):
                assert consumed(state, evdev + 8, 0) == 0, f"{mode}: unchanged key consumes modifiers"
            key(56, False)
            key(42, False)
            # Hyprland 0.56.2 resolveKeycode: case-insensitive symbol lookup
            # followed by an unmodified state scan in the active group. These
            # fixture C/V levels came from read-only Spanish-ISO layout metadata,
            # NOT a capture of events or an exported live remote keymap.
            for name, evdev in ((b'C', 46), (b'V', 47)):
                target = keysym_from_name(name, 1)
                resolved = [kc for kc in range(8, 256) if symbol(state, kc) == target]
                assert evdev + 8 in resolved, f'{mode}: Hyprland-style resolver cannot find {name!r}'
                assert sym(evdev) == name.lower()[0]
                key(29, True)
                key(42, True)
                assert sym(evdev) == name[0]
                key(42, False)
                key(29, False)
            # The installed Omarchy terminal bindings use Ctrl+Insert and
            # Shift+Insert, not C/V. Resolve the exact target symbol first.
            target = keysym_from_name(b'Insert', 1)
            resolved = [kc for kc in range(8, 256) if symbol(state, kc) == target]
            assert 118 in resolved, f'{mode}: Hyprland-style resolver cannot find Insert at evdev 110'
            for modifier, mask in ((29, 4), (42, 1)):
                key(modifier, True)
                assert sym(110) == target, f'{mode}: shortcut changed Insert symbol'
                assert serialize_mods(state, 1) & mask, f'{mode}: shortcut modifier not depressed'
                assert consumed(state, 118, 0) == 0, f'{mode}: Insert consumed shortcut modifiers'
                key(modifier, False)
            assert sym(30) == ord('a')
            key(42, True)
            assert sym(30) == ord('A')
            key(42, False)
            key(56, True)
            assert sym(30) == (0xe6 if mode == 'altgr' else ord('a'))
            key(42, True)
            assert sym(30) == (0xc6 if mode == 'altgr' else ord('A'))
            key(42, False)
            key(56, False)
            assert sym(39) == ord('ñ')
            assert sym(40) == 0xfe51  # dead_acute, not literal apostrophe
            key(58, True)
            key(58, False)
            assert sym(30) == ord('A')
            assert sym(39) == ord('Ñ')
            key(58, True)
            key(58, False)
            assert sym(30) == ord('a')
            # Exercise real XKB compose state, without requiring X11 locale data.
            rules = '<dead_acute> <a> : "á" U00E1\n'.encode()
            table = compose_table(ctx, rules, len(rules), b'C.UTF-8', 1, 0)
            assert table
            composition = compose_new(table, 0)
            assert composition
            try:
                compose_feed(composition, sym(40))
                compose_feed(composition, sym(30))
                output = c.create_string_buffer(16)
                compose_utf8(composition, output, len(output))
                assert output.value.decode() == 'á'
            finally:
                compose_state_unref(composition)
                compose_table_unref(table)
        finally:
            state_unref(state)
            map_unref(keymap)
        print(f'OK: {mode}: XKB compilation, Shift, Option, Caps Lock, ñ, dead-key composition, unconsumed Shift+Left/Shift+F1, C/V/Insert resolution and Ctrl+Insert/Shift+Insert (Hyprland 0.56.2 algorithm)')
finally:
    context_unref(ctx)
