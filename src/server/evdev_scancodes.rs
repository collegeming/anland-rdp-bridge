//! RDP (PS/2 Set 1) scancode → Linux evdev keycode mapping.
//!
//! `mstsc` sends raw Set 1 scancodes (+ a separate `extended` flag) over the
//! RDP input channel. The anland bridge KEY payload expects **evdev keycodes**
//! (the ANiri consumer adds +8 and feeds them to smithay/libinput as xkb
//! codes). Pure data + a pure lookup — no FFI, no shared state.
//!
//! ## Why a table is load-bearing
//!
//! Set 1 scancodes and evdev keycodes coincide for the entire main typing
//! block (both derive from the same physical-layout numbering: `A`=30, `T`=20,
//! `F1`=59, …), so a server that forwards the scancode as-is *appears* to
//! work for typing and most `Ctrl` combos. But every special key diverges:
//!
//! - Left/right Windows (GUI) 0x5B/0x5C = 91/92 → evdev `KEY_LEFTMETA`/`KEY_RIGHTMETA`
//!   = **125/126**. Forwarded raw, 91 is `KEY_HIRAGANA` — so niri's `Mod+T`
//!   (Win+T) never fires because the compositor never sees a Meta press.
//!   Mapped in BOTH the normal and extended tables: clients disagree on the
//!   extended flag for the Win keys (lamco's mapper and mstsc handle the
//!   E0-prefixed form), so only one variant would drop real Win presses.
//! - Right Ctrl/Alt are extended scancodes (0x1D/0x38 + extended flag) that
//!   raw-forwarding collapses onto their left halves.
//! - Navigation/Insert/Delete/PrintScreen are extended keys with unrelated
//!   evdev values.
//!
//! Values are from `linux/input-event-codes.h` (checked against Arch's
//! `/usr/include/linux/input-event-codes.h`).
//!
//! ## Non-US layouts need no per-key translation
//!
//! RDP carries *scancodes*, not characters. The remote session's own xkb
//! layout translates evdev keycode → keysym in the compositor, so a German /
//! French / … remote layout produces the right characters with no server-side
//! work — this is standard RDP semantics (a Windows RDP server behaves the
//! same way). Only the physical key *positions* matter here, and the ISO
//! extra key (0x56 → `KEY_102ND`) is covered. The macOS fork needed a
//! client-layout translation because it streams a *local* macOS session; the
//! anland streaming model does not.

/// Sentinel for "no mapping" — well outside the evdev keycode range.
const NONE: u32 = u32::MAX;

/// Evdev keycodes of the lock keys, used by the `Synchronize` lock-state
/// reconciliation in the input handler.
pub(crate) const KEY_CAPSLOCK: u32 = 58;
pub(crate) const KEY_NUMLOCK: u32 = 69;
pub(crate) const KEY_SCROLLLOCK: u32 = 70;

/// Remote lock state (CapsLock/NumLock/ScrollLock) as tracked by the input
/// handler. Mirrors the client's reported state so a later `Synchronize`
/// only toggles keys that actually differ.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LockState {
    pub num_lock: bool,
    pub caps_lock: bool,
    pub scroll_lock: bool,
}

/// Compute which lock keys need a press+release toggle to bring the remote
/// session from `tracked` up to the requested state, updating `tracked` in
/// place. Pure — the caller injects each returned keycode (down then up) over
/// the bridge, which toggles the remote xkb lock state. The bridge only
/// carries key events, so lock state is *toggled*, not set — the same
/// reconciliation model xrdp uses.
pub(crate) fn lock_toggles(
    num_lock: bool,
    caps_lock: bool,
    scroll_lock: bool,
    tracked: &mut LockState,
) -> Vec<u32> {
    let want = LockState {
        num_lock,
        caps_lock,
        scroll_lock,
    };
    let mut toggles = Vec::new();
    if want.caps_lock != tracked.caps_lock {
        toggles.push(KEY_CAPSLOCK);
    }
    if want.num_lock != tracked.num_lock {
        toggles.push(KEY_NUMLOCK);
    }
    if want.scroll_lock != tracked.scroll_lock {
        toggles.push(KEY_SCROLLLOCK);
    }
    *tracked = want;
    toggles
}

/// PS/2 Set 1 scancode → evdev keycode. `extended` is the RDP keyboard-event
/// extended flag. Returns `None` for unmapped scancodes (non-US layouts, media
/// keys, and IME need follow-on work).
pub(crate) fn scancode_to_evdev(scancode: u8, extended: bool) -> Option<u32> {
    let entry = if extended {
        SCANCODE_EXTENDED[scancode as usize]
    } else {
        SCANCODE_NORMAL[scancode as usize]
    };
    if entry == NONE {
        None
    } else {
        Some(entry)
    }
}

const SCANCODE_NORMAL: [u32; 256] = build_scancode_normal();
const SCANCODE_EXTENDED: [u32; 256] = build_scancode_extended();

const fn build_scancode_normal() -> [u32; 256] {
    let mut t = [NONE; 256];
    t[0x01] = 1; // Esc → KEY_ESC
    t[0x02] = 2; // 1 → KEY_1
    t[0x03] = 3; // 2 → KEY_2
    t[0x04] = 4; // 3 → KEY_3
    t[0x05] = 5; // 4 → KEY_4
    t[0x06] = 6; // 5 → KEY_5
    t[0x07] = 7; // 6 → KEY_6
    t[0x08] = 8; // 7 → KEY_7
    t[0x09] = 9; // 8 → KEY_8
    t[0x0A] = 10; // 9 → KEY_9
    t[0x0B] = 11; // 0 → KEY_0
    t[0x0C] = 12; // - → KEY_MINUS
    t[0x0D] = 13; // = → KEY_EQUAL
    t[0x0E] = 14; // Backspace → KEY_BACKSPACE
    t[0x0F] = 15; // Tab → KEY_TAB
    t[0x10] = 16; // Q → KEY_Q
    t[0x11] = 17; // W → KEY_W
    t[0x12] = 18; // E → KEY_E
    t[0x13] = 19; // R → KEY_R
    t[0x14] = 20; // T → KEY_T
    t[0x15] = 21; // Y → KEY_Y
    t[0x16] = 22; // U → KEY_U
    t[0x17] = 23; // I → KEY_I
    t[0x18] = 24; // O → KEY_O
    t[0x19] = 25; // P → KEY_P
    t[0x1A] = 26; // [ → KEY_LEFTBRACE
    t[0x1B] = 27; // ] → KEY_RIGHTBRACE
    t[0x1C] = 28; // Enter → KEY_ENTER
    t[0x1D] = 29; // Left Ctrl → KEY_LEFTCTRL
    t[0x1E] = 30; // A → KEY_A
    t[0x1F] = 31; // S → KEY_S
    t[0x20] = 32; // D → KEY_D
    t[0x21] = 33; // F → KEY_F
    t[0x22] = 34; // G → KEY_G
    t[0x23] = 35; // H → KEY_H
    t[0x24] = 36; // J → KEY_J
    t[0x25] = 37; // K → KEY_K
    t[0x26] = 38; // L → KEY_L
    t[0x27] = 39; // ; → KEY_SEMICOLON
    t[0x28] = 40; // ' → KEY_APOSTROPHE
    t[0x29] = 41; // ` → KEY_GRAVE
    t[0x2A] = 42; // Left Shift → KEY_LEFTSHIFT
    t[0x2B] = 43; // \ → KEY_BACKSLASH
    t[0x2C] = 44; // Z → KEY_Z
    t[0x2D] = 45; // X → KEY_X
    t[0x2E] = 46; // C → KEY_C
    t[0x2F] = 47; // V → KEY_V
    t[0x30] = 48; // B → KEY_B
    t[0x31] = 49; // N → KEY_N
    t[0x32] = 50; // M → KEY_M
    t[0x33] = 51; // , → KEY_COMMA
    t[0x34] = 52; // . → KEY_DOT
    t[0x35] = 53; // / → KEY_SLASH
    t[0x36] = 54; // Right Shift → KEY_RIGHTSHIFT
    t[0x37] = 55; // numpad * → KEY_KPASTERISK
    t[0x38] = 56; // Left Alt → KEY_LEFTALT
    t[0x39] = 57; // Space → KEY_SPACE
    t[0x3A] = 58; // CapsLock → KEY_CAPSLOCK
    t[0x3B] = 59; // F1 → KEY_F1
    t[0x3C] = 60; // F2 → KEY_F2
    t[0x3D] = 61; // F3 → KEY_F3
    t[0x3E] = 62; // F4 → KEY_F4
    t[0x3F] = 63; // F5 → KEY_F5
    t[0x40] = 64; // F6 → KEY_F6
    t[0x41] = 65; // F7 → KEY_F7
    t[0x42] = 66; // F8 → KEY_F8
    t[0x43] = 67; // F9 → KEY_F9
    t[0x44] = 68; // F10 → KEY_F10
    t[0x45] = 69; // NumLock → KEY_NUMLOCK
    t[0x46] = 70; // ScrollLock → KEY_SCROLLLOCK
    t[0x47] = 71; // numpad 7 → KEY_KP7
    t[0x48] = 72; // numpad 8 → KEY_KP8
    t[0x49] = 73; // numpad 9 → KEY_KP9
    t[0x4A] = 74; // numpad - → KEY_KPMINUS
    t[0x4B] = 75; // numpad 4 → KEY_KP4
    t[0x4C] = 76; // numpad 5 → KEY_KP5
    t[0x4D] = 77; // numpad 6 → KEY_KP6
    t[0x4E] = 78; // numpad + → KEY_KPPLUS
    t[0x4F] = 79; // numpad 1 → KEY_KP1
    t[0x50] = 80; // numpad 2 → KEY_KP2
    t[0x51] = 81; // numpad 3 → KEY_KP3
    t[0x52] = 82; // numpad 0 → KEY_KP0
    t[0x53] = 83; // numpad . → KEY_KPDOT
    t[0x56] = 86; // ISO extra key → KEY_102ND
    t[0x57] = 87; // F11 → KEY_F11
    t[0x58] = 88; // F12 → KEY_F12
    t[0x5B] = 125; // Left Win/GUI → KEY_LEFTMETA
    t[0x5C] = 126; // Right Win/GUI → KEY_RIGHTMETA
    t
}

const fn build_scancode_extended() -> [u32; 256] {
    let mut t = [NONE; 256];
    t[0x1C] = 96; // numpad Enter → KEY_KPENTER
    t[0x1D] = 97; // Right Ctrl → KEY_RIGHTCTRL
    t[0x35] = 98; // numpad / → KEY_KPSLASH
    t[0x37] = 99; // PrintScreen → KEY_SYSRQ
    t[0x38] = 100; // Right Alt → KEY_RIGHTALT
    t[0x47] = 102; // Home → KEY_HOME
    t[0x48] = 103; // Up → KEY_UP
    t[0x49] = 104; // PageUp → KEY_PAGEUP
    t[0x4B] = 105; // Left → KEY_LEFT
    t[0x4D] = 106; // Right → KEY_RIGHT
    t[0x4F] = 107; // End → KEY_END
    t[0x50] = 108; // Down → KEY_DOWN
    t[0x51] = 109; // PageDown → KEY_PAGEDOWN
    t[0x52] = 110; // Insert → KEY_INSERT
    t[0x53] = 111; // Delete → KEY_DELETE
    t[0x5B] = 125; // Left Win/GUI → KEY_LEFTMETA (some clients send it extended)
    t[0x5C] = 126; // Right Win/GUI → KEY_RIGHTMETA
    t[0x5D] = 139; // Apps / context-menu → KEY_MENU
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_typing_block_keeps_the_identity_mapping() {
        // Set 1 == evdev for the whole main block; regression-guard it so a
        // typo can't silently break typing.
        for scancode in [0x01, 0x0F, 0x14, 0x1E, 0x39, 0x3B, 0x45, 0x57, 0x58] {
            assert_eq!(
                scancode_to_evdev(scancode, false),
                Some(u32::from(scancode)),
                "scancode 0x{scancode:02X} should map to its own keycode"
            );
        }
    }

    #[test]
    fn meta_keys_map_to_evdev_not_raw_scancode() {
        // Win+T (niri Mod+T) dies on a raw-forwarding server: 0x5B/0x5C must
        // become KEY_LEFTMETA/KEY_RIGHTMETA, not 91/92 (KEY_HIRAGANA/…).
        assert_eq!(scancode_to_evdev(0x5B, false), Some(125));
        assert_eq!(scancode_to_evdev(0x5C, false), Some(126));
        // Clients differ on the extended flag for the Win keys (mstsc /
        // lamco's mapper both handle the E0 5B form); both variants must
        // translate or a real Win press gets silently dropped.
        assert_eq!(scancode_to_evdev(0x5B, true), Some(125));
        assert_eq!(scancode_to_evdev(0x5C, true), Some(126));
    }

    #[test]
    fn extended_keys_do_not_collapse_onto_their_left_halves() {
        assert_eq!(scancode_to_evdev(0x1D, false), Some(29)); // Left Ctrl
        assert_eq!(scancode_to_evdev(0x1D, true), Some(97)); // Right Ctrl
        assert_eq!(scancode_to_evdev(0x38, false), Some(56)); // Left Alt
        assert_eq!(scancode_to_evdev(0x38, true), Some(100)); // Right Alt
        assert_eq!(scancode_to_evdev(0x48, false), Some(72)); // numpad 8
        assert_eq!(scancode_to_evdev(0x48, true), Some(103)); // Up arrow
        assert_eq!(scancode_to_evdev(0x5D, true), Some(139)); // Apps → KEY_MENU
    }

    #[test]
    fn unmapped_scancodes_return_none() {
        assert_eq!(scancode_to_evdev(0xFF, false), None);
        assert_eq!(scancode_to_evdev(0x00, true), None);
    }

    #[test]
    fn lock_toggles_only_keys_that_differ_and_updates_tracked() {
        let mut tracked = LockState::default();
        // Fresh remote (all off), client reports CapsLock on → toggle caps.
        assert_eq!(lock_toggles(false, true, false, &mut tracked), vec![58]);
        assert_eq!(tracked, LockState { caps_lock: true, ..Default::default() });

        // In sync → no toggles, no churn.
        assert!(lock_toggles(false, true, false, &mut tracked).is_empty());

        // Client now reports all off → toggle caps off, num on.
        assert_eq!(lock_toggles(true, false, false, &mut tracked), vec![58, 69]);
        assert_eq!(tracked, LockState { num_lock: true, ..Default::default() });
    }
}
