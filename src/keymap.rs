// AUTO-GENERATED from docs/reference/keycodes.json (linux column = Windows VK codes).
// Maps the Windows Virtual-Key code the Electron app sends to a US-layout X11 keysym.
// keysym -> XTEST keycode is resolved at runtime from the server keyboard mapping.
/// Returns the US X11 keysym for a Windows VK code, or None if unmapped.
pub fn vk_to_keysym(vk: u32) -> Option<u32> {
    Some(match vk {
        8 => 0xff08,   // backspace
        9 => 0xff09,   // tab
        13 => 0xff0d,  // enter
        19 => 0xff7f,  // num_lock
        20 => 0xffe5,  // caps_lock
        27 => 0xff1b,  // esc
        32 => 0x0020,  // space
        33 => 0xff55,  // page_up
        34 => 0xff56,  // page_down
        35 => 0xff57,  // end
        36 => 0xff50,  // home
        37 => 0xff51,  // left
        38 => 0xff52,  // up
        39 => 0xff53,  // right
        40 => 0xff54,  // down
        42 => 0xff61,  // print
        44 => 0xff61,  // print_screen
        45 => 0xff63,  // insert
        46 => 0xffff,  // delete
        48 => 0x0030,  // zero
        49 => 0x0031,  // one
        50 => 0x0032,  // two
        51 => 0x0033,  // three
        52 => 0x0034,  // four
        53 => 0x0035,  // five
        54 => 0x0036,  // six
        55 => 0x0037,  // seven
        56 => 0x0038,  // eight
        57 => 0x0039,  // nine
        65 => 0x0061,  // a
        66 => 0x0062,  // b
        67 => 0x0063,  // c
        68 => 0x0064,  // d
        69 => 0x0065,  // e
        70 => 0x0066,  // f
        71 => 0x0067,  // g
        72 => 0x0068,  // h
        73 => 0x0069,  // i
        74 => 0x006a,  // j
        75 => 0x006b,  // k
        76 => 0x006c,  // l
        77 => 0x006d,  // m
        78 => 0x006e,  // n
        79 => 0x006f,  // o
        80 => 0x0070,  // p
        81 => 0x0071,  // q
        82 => 0x0072,  // r
        83 => 0x0073,  // s
        84 => 0x0074,  // t
        85 => 0x0075,  // u
        86 => 0x0076,  // v
        87 => 0x0077,  // w
        88 => 0x0078,  // x
        89 => 0x0079,  // y
        90 => 0x007a,  // z
        91 => 0xffeb,  // win
        92 => 0xffec,  // win_r
        96 => 0xffb0,  // num_pad_zero
        97 => 0xffb1,  // num_pad_one
        98 => 0xffb2,  // num_pad_two
        99 => 0xffb3,  // num_pad_three
        100 => 0xffb4, // num_pad_four
        101 => 0xffb5, // num_pad_five
        102 => 0xffb6, // num_pad_six
        103 => 0xffb7, // num_pad_seven
        104 => 0xffb8, // num_pad_eight
        105 => 0xffb9, // num_pad_nine
        106 => 0xffaa, // multiply
        107 => 0xffab, // add
        108 => 0xffad, // subtract
        110 => 0xffae, // decimal
        111 => 0xffaf, // divide
        112 => 0xffbe, // f1
        113 => 0xffbf, // f2
        114 => 0xffc0, // f3
        115 => 0xffc1, // f4
        116 => 0xffc2, // f5
        117 => 0xffc3, // f6
        118 => 0xffc4, // f7
        119 => 0xffc5, // f8
        120 => 0xffc6, // f9
        121 => 0xffc7, // f10
        122 => 0xffc8, // f11
        123 => 0xffc9, // f12
        124 => 0xffca, // f13
        125 => 0xffcb, // f14
        126 => 0xffcc, // f15
        127 => 0xffcd, // f16
        128 => 0xffce, // f17
        129 => 0xffcf, // f18
        130 => 0xffd0, // f19
        131 => 0xffd1, // f20
        132 => 0xffd2, // f21
        133 => 0xffd3, // f22
        134 => 0xffd4, // f23
        135 => 0xffd5, // f24
        160 => 0xffe1, // shift
        161 => 0xffe2, // shift_r
        162 => 0xffe3, // ctrl
        163 => 0xffe4, // ctrl_r
        164 => 0xffe9, // alt
        165 => 0xffea, // alt_r
        186 => 0x003b, // semicolon
        187 => 0x003d, // equals
        188 => 0x002c, // comma
        189 => 0x002d, // minus
        190 => 0x002e, // period
        191 => 0x002f, // forwardSlash
        192 => 0x0060, // backTick
        219 => 0x005b, // squareBracketOpen
        220 => 0x005c, // backSlash
        221 => 0x005d, // squareBracketClose
        222 => 0x0027, // apostrophe
        _ => return None,
    })
}

/// Modifier flag name (as sent by the app) -> X11 modifier keysym.
pub fn flag_to_keysym(flag: &str) -> Option<u32> {
    Some(match flag {
        "Shift" => 0xffe1,
        "Control" => 0xffe3,
        "Alt" => 0xffe9,
        "Command" | "Meta" | "Super" => 0xffeb, // Super_L (rare on non-mac)
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// evdev / uinput mapping (Wayland backend).
//
// The Wayland backend injects via /dev/uinput, which speaks Linux kernel
// `KEY_*` codes from <linux/input-event-codes.h> — NOT X11 keysyms. These are
// physical-key codes on a US layout, so the same VK the app sends maps to the
// US-positioned evdev key. (Non-US layouts would shift the produced character,
// same caveat the X11 keysym->keycode path has.)
// ---------------------------------------------------------------------------

/// Returns the Linux evdev `KEY_*` code for a Windows VK code, or None if unmapped.
pub fn vk_to_evdev(vk: u32) -> Option<u16> {
    Some(match vk {
        8 => 14,    // backspace
        9 => 15,    // tab
        13 => 28,   // enter
        19 => 69,   // num_lock
        20 => 58,   // caps_lock
        27 => 1,    // esc
        32 => 57,   // space
        33 => 104,  // page_up
        34 => 109,  // page_down
        35 => 107,  // end
        36 => 102,  // home
        37 => 105,  // left
        38 => 103,  // up
        39 => 106,  // right
        40 => 108,  // down
        44 => 99,   // print_screen (SYSRQ)
        45 => 110,  // insert
        46 => 111,  // delete
        48 => 11,   // 0
        49 => 2,    // 1
        50 => 3,    // 2
        51 => 4,    // 3
        52 => 5,    // 4
        53 => 6,    // 5
        54 => 7,    // 6
        55 => 8,    // 7
        56 => 9,    // 8
        57 => 10,   // 9
        65 => 30,   // a
        66 => 48,   // b
        67 => 46,   // c
        68 => 32,   // d
        69 => 18,   // e
        70 => 33,   // f
        71 => 34,   // g
        72 => 35,   // h
        73 => 23,   // i
        74 => 36,   // j
        75 => 37,   // k
        76 => 38,   // l
        77 => 50,   // m
        78 => 49,   // n
        79 => 24,   // o
        80 => 25,   // p
        81 => 16,   // q
        82 => 19,   // r
        83 => 31,   // s
        84 => 20,   // t
        85 => 22,   // u
        86 => 47,   // v
        87 => 17,   // w
        88 => 45,   // x
        89 => 21,   // y
        90 => 44,   // z
        91 => 125,  // win -> left meta
        92 => 126,  // win_r -> right meta
        96 => 82,   // num_pad_zero
        97 => 79,   // num_pad_one
        98 => 80,   // num_pad_two
        99 => 81,   // num_pad_three
        100 => 75,  // num_pad_four
        101 => 76,  // num_pad_five
        102 => 77,  // num_pad_six
        103 => 71,  // num_pad_seven
        104 => 72,  // num_pad_eight
        105 => 73,  // num_pad_nine
        106 => 55,  // multiply (KPASTERISK)
        107 => 78,  // add (KPPLUS)
        108 => 74,  // subtract (KPMINUS)
        110 => 83,  // decimal (KPDOT)
        111 => 98,  // divide (KPSLASH)
        112 => 59,  // f1
        113 => 60,  // f2
        114 => 61,  // f3
        115 => 62,  // f4
        116 => 63,  // f5
        117 => 64,  // f6
        118 => 65,  // f7
        119 => 66,  // f8
        120 => 67,  // f9
        121 => 68,  // f10
        122 => 87,  // f11
        123 => 88,  // f12
        124 => 183, // f13
        125 => 184, // f14
        126 => 185, // f15
        127 => 186, // f16
        128 => 187, // f17
        129 => 188, // f18
        130 => 189, // f19
        131 => 190, // f20
        132 => 191, // f21
        133 => 192, // f22
        134 => 193, // f23
        135 => 194, // f24
        160 => 42,  // shift
        161 => 54,  // shift_r
        162 => 29,  // ctrl
        163 => 97,  // ctrl_r
        164 => 56,  // alt
        165 => 100, // alt_r
        186 => 39,  // semicolon
        187 => 13,  // equals
        188 => 51,  // comma
        189 => 12,  // minus
        190 => 52,  // period
        191 => 53,  // forwardSlash
        192 => 41,  // backTick (grave)
        219 => 26,  // squareBracketOpen
        220 => 43,  // backSlash
        221 => 27,  // squareBracketClose
        222 => 40,  // apostrophe
        _ => return None,
    })
}

/// Returns the Windows VK code for a Linux evdev `KEY_*` code, or None if
/// unmapped. The inverse of [`vk_to_evdev`] — used by the global key monitor to
/// translate physical key events back into the VK codes the app's keyboard
/// service expects (left/right-specific modifiers: e.g. left Ctrl = 162, left
/// Meta/Win = 91), matching what the Windows low-level hook reports.
pub fn evdev_to_vk(code: u16) -> Option<u32> {
    Some(match code {
        1 => 27,    // esc
        2 => 49,    // 1
        3 => 50,    // 2
        4 => 51,    // 3
        5 => 52,    // 4
        6 => 53,    // 5
        7 => 54,    // 6
        8 => 55,    // 7
        9 => 56,    // 8
        10 => 57,   // 9
        11 => 48,   // 0
        12 => 189,  // minus
        13 => 187,  // equals
        14 => 8,    // backspace
        15 => 9,    // tab
        16 => 81,   // q
        17 => 87,   // w
        18 => 69,   // e
        19 => 82,   // r
        20 => 84,   // t
        21 => 89,   // y
        22 => 85,   // u
        23 => 73,   // i
        24 => 79,   // o
        25 => 80,   // p
        26 => 219,  // squareBracketOpen
        27 => 221,  // squareBracketClose
        28 => 13,   // enter
        29 => 162,  // left ctrl
        30 => 65,   // a
        31 => 83,   // s
        32 => 68,   // d
        33 => 70,   // f
        34 => 71,   // g
        35 => 72,   // h
        36 => 74,   // j
        37 => 75,   // k
        38 => 76,   // l
        39 => 186,  // semicolon
        40 => 222,  // apostrophe
        41 => 192,  // backTick (grave)
        42 => 160,  // left shift
        43 => 220,  // backSlash
        44 => 90,   // z
        45 => 88,   // x
        46 => 67,   // c
        47 => 86,   // v
        48 => 66,   // b
        49 => 78,   // n
        50 => 77,   // m
        51 => 188,  // comma
        52 => 190,  // period
        53 => 191,  // forwardSlash
        54 => 161,  // right shift
        55 => 106,  // KPASTERISK (multiply)
        56 => 164,  // left alt
        57 => 32,   // space
        58 => 20,   // caps_lock
        59 => 112,  // f1
        60 => 113,  // f2
        61 => 114,  // f3
        62 => 115,  // f4
        63 => 116,  // f5
        64 => 117,  // f6
        65 => 118,  // f7
        66 => 119,  // f8
        67 => 120,  // f9
        68 => 121,  // f10
        69 => 19,   // num_lock
        71 => 103,  // num_pad_seven
        72 => 104,  // num_pad_eight
        73 => 105,  // num_pad_nine
        74 => 108,  // KPMINUS (subtract)
        75 => 100,  // num_pad_four
        76 => 101,  // num_pad_five
        77 => 102,  // num_pad_six
        78 => 107,  // KPPLUS (add)
        79 => 97,   // num_pad_one
        80 => 98,   // num_pad_two
        81 => 99,   // num_pad_three
        82 => 96,   // num_pad_zero
        83 => 110,  // KPDOT (decimal)
        87 => 122,  // f11
        88 => 123,  // f12
        97 => 163,  // right ctrl
        98 => 111,  // KPSLASH (divide)
        99 => 44,   // print_screen (SYSRQ)
        100 => 165, // right alt
        102 => 36,  // home
        103 => 38,  // up
        104 => 33,  // page_up
        105 => 37,  // left
        106 => 39,  // right
        107 => 35,  // end
        108 => 40,  // down
        109 => 34,  // page_down
        110 => 45,  // insert
        111 => 46,  // delete
        125 => 91,  // left meta (win)
        126 => 92,  // right meta (win_r)
        183 => 124, // f13
        184 => 125, // f14
        185 => 126, // f15
        186 => 127, // f16
        187 => 128, // f17
        188 => 129, // f18
        189 => 130, // f19
        190 => 131, // f20
        191 => 132, // f21
        192 => 133, // f22
        193 => 134, // f23
        194 => 135, // f24
        _ => return None,
    })
}

/// Modifier flag name (as sent by the app) -> Linux evdev `KEY_*` code.
pub fn flag_to_evdev(flag: &str) -> Option<u16> {
    Some(match flag {
        "Shift" => 42,                       // KEY_LEFTSHIFT
        "Control" => 29,                     // KEY_LEFTCTRL
        "Alt" => 56,                         // KEY_LEFTALT
        "Command" | "Meta" | "Super" => 125, // KEY_LEFTMETA
        _ => return None,
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    /// `evdev_to_vk` must be the exact inverse of `vk_to_evdev`: every VK the app
    /// can send round-trips through evdev and back to the same VK. Guards against
    /// the two tables drifting apart (which would silently break shortcut
    /// matching — the monitor would report codes the app never stored).
    #[test]
    fn evdev_vk_roundtrip() {
        for vk in 0u32..=255 {
            if let Some(code) = vk_to_evdev(vk) {
                assert_eq!(
                    evdev_to_vk(code),
                    Some(vk),
                    "VK {vk} -> evdev {code} did not round-trip back to {vk}"
                );
            }
        }
    }

    /// The left-modifier VK codes the app's keycode constants use on non-mac
    /// (ctrl=162, shift=160, alt=164, win/meta=91) — the Ctrl+Meta default lives
    /// here, so these must map correctly or push-to-talk can never match.
    #[test]
    fn left_modifiers_map_to_app_vk_codes() {
        assert_eq!(evdev_to_vk(29), Some(162)); // left ctrl
        assert_eq!(evdev_to_vk(42), Some(160)); // left shift
        assert_eq!(evdev_to_vk(56), Some(164)); // left alt
        assert_eq!(evdev_to_vk(125), Some(91)); // left meta (win)
    }
}
