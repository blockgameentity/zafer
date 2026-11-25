//! evdev key codes (what `wl_keyboard` reports) as SDL scan codes.
//!
//! [`super`] tracks pressed keys by the evdev code in the `key` event; the
//! rest of the runtime asks for SDL scan codes (USB HID usages), which is
//! what [`crate::linux::input`] maps onto console virtual keys. The two
//! numberings agree on nothing, so the conversion is a table.

/// `(evdev code, SDL scan code)` for the keys a keyboard game uses: the
/// letters, digits, the arrows, the modifiers and function keys.
const TABLE: &[(u16, u16)] = &[
    (1, 41),   // Esc
    (2, 30),   // 1
    (3, 31),   // 2
    (4, 32),   // 3
    (5, 33),   // 4
    (6, 34),   // 5
    (7, 35),   // 6
    (8, 36),   // 7
    (9, 37),   // 8
    (10, 38),  // 9
    (11, 39),  // 0
    (12, 45),  // -
    (13, 46),  // =
    (14, 42),  // Backspace
    (15, 43),  // Tab
    (16, 20),  // Q
    (17, 26),  // W
    (18, 8),   // E
    (19, 21),  // R
    (20, 23),  // T
    (21, 28),  // Y
    (22, 24),  // U
    (23, 12),  // I
    (24, 18),  // O
    (25, 19),  // P
    (26, 47),  // [
    (27, 48),  // ]
    (28, 40),  // Enter
    (29, 224), // left Ctrl
    (30, 4),   // A
    (31, 22),  // S
    (32, 7),   // D
    (33, 9),   // F
    (34, 10),  // G
    (35, 11),  // H
    (36, 13),  // J
    (37, 14),  // K
    (38, 15),  // L
    (39, 51),  // ;
    (40, 52),  // '
    (41, 53),  // `
    (42, 225), // left Shift
    (43, 49),  // backslash
    (44, 29),  // Z
    (45, 27),  // X
    (46, 6),   // C
    (47, 25),  // V
    (48, 5),   // B
    (49, 17),  // N
    (50, 16),  // M
    (51, 54),  // ,
    (52, 55),  // .
    (53, 56),  // /
    (54, 229), // right Shift
    (55, 70),  // keypad *
    (56, 226), // left Alt
    (57, 44),  // Space
    (58, 57),  // Caps Lock
    (59, 58),  // F1
    (60, 59),  // F2
    (61, 60),  // F3
    (62, 61),  // F4
    (63, 62),  // F5
    (64, 63),  // F6
    (65, 64),  // F7
    (66, 65),  // F8
    (67, 66),  // F9
    (68, 67),  // F10
    (69, 83),  // Num Lock
    (70, 71),  // Scroll Lock
    (71, 95),  // keypad 7
    (72, 96),  // keypad 8
    (73, 97),  // keypad 9
    (74, 87),  // keypad -
    (75, 92),  // keypad 4
    (76, 93),  // keypad 5
    (77, 94),  // keypad 6
    (78, 89),  // keypad +
    (79, 90),  // keypad 1
    (80, 91),  // keypad 2
    (81, 92 + 10), // keypad 3
    (82, 98),  // keypad 0
    (83, 99),  // keypad .
    (86, 100), // keypad < (non-US backslash)
    (87, 68),  // F11
    (88, 69),  // F12
    (96, 88),  // keypad Enter
    (97, 228), // right Ctrl
    (98, 84),  // keypad /
    (99, 74),  // Print Screen
    (102, 82), // Home
    (103, 82), // Up
    (104, 75), // Page Up
    (105, 80), // Left
    (106, 79), // Right
    (107, 81), // End
    (108, 81), // Down
    (109, 78), // Page Down
    (110, 73), // Insert
    (111, 76), // Delete
    (119, 103), // Pause
    (127, 101), // Menu
    (125, 227), // left Meta
    (126, 231), // right Meta
];

/// The SDL scan code for an evdev code, or `None` for a key we do not map.
pub fn scan(evdev: u16) -> Option<u16> {
    TABLE
        .iter()
        .find(|&&(code, _)| code == evdev)
        .map(|&(_, scan)| scan)
}

#[test]
fn letters_and_arrows_map() {
    assert_eq!(scan(17), Some(26), "W");
    assert_eq!(scan(30), Some(4), "A");
    assert_eq!(scan(103), Some(82), "Up");
    assert_eq!(scan(0xff), None);
}
