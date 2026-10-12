use gpui::{Keystroke, Modifiers};

use crate::ui::keyboard_layout;

/// Bytes to send to the shell for a key press, or `None` when the key is left
/// to the OS text input or to an app shortcut.
///
/// `is_option_meta` only matters on macOS: Option+char either types the
/// character the layout composes (false) or acts as Meta (true).
///
/// `is_app_cursor` is the app's cursor key mode (DECCKM): when set, plain
/// arrows, Home and End go out as `ESC O x` instead of `ESC [ x`.
pub fn keystroke_to_bytes(
    keystroke: &Keystroke,
    is_option_meta: bool,
    is_app_cursor: bool,
) -> Option<Vec<u8>> {
    keystroke_to_bytes_on(
        keystroke,
        is_option_meta,
        is_app_cursor,
        cfg!(target_os = "macos"),
    )
}

/// Arrow keys for a wheel turn on the alternate screen, where there is no
/// scrollback: a pager or editor without mouse mode scrolls by them. Positive
/// `lines` is Up.
pub fn wheel_arrow_bytes(lines: i32, is_app_cursor: bool) -> Vec<u8> {
    let letter = if lines > 0 { b'A' } else { b'B' };
    cursor_key_seq(letter, 1, is_app_cursor).repeat(lines.unsigned_abs() as usize)
}

fn keystroke_to_bytes_on(
    keystroke: &Keystroke,
    is_option_meta: bool,
    is_app_cursor: bool,
    is_macos: bool,
) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;

    if is_macos {
        if let Some(bytes) = macos_editing_seq(keystroke.key.as_str(), m) {
            return Some(bytes.to_vec());
        }
    }

    // Platform key (Cmd/Win) without ctrl: let OS handle (Cmd+Q, Cmd+W, etc.)
    if m.platform && !m.control {
        return None;
    }

    // Control codes go by physical key, but gpui names the key after the
    // character the active layout prints: Ctrl+C on ЙЦУКЕН arrives as "С".
    // Resolve it back so the shell still gets its control byte.
    let mut buffer = [0u8; 4];
    let control_key = match keyboard_layout::us_layout_key(&keystroke.key) {
        Some(us) => &*us.encode_utf8(&mut buffer),
        None => keystroke.key.as_str(),
    };

    // Ctrl+Shift+{C,V,A,F} are chords handled as TerminalView actions (copy /
    // paste / select-all / search). Don't also forward them to the shell, or
    // Ctrl+Shift+C would send SIGINT (0x03) on top of copying.
    if m.control && m.shift && matches!(control_key, "c" | "v" | "a" | "f") {
        return None;
    }

    // Special keys always get intercepted regardless of modifiers
    if let Some(bytes) = special_key_seq(keystroke.key.as_str(), m, is_app_cursor) {
        return Some(bytes);
    }

    if m.control && !m.alt {
        return ctrl_to_bytes(control_key);
    }

    if m.alt && !m.control {
        return match (is_macos, is_option_meta) {
            // Left to the OS, Option+Space types a no-break space, which the
            // shell takes as part of a command name. It is easy to hit right
            // after Option+7 for "|" on a German layout.
            (true, false) if keystroke.key == "space" => Some(b" ".to_vec()),
            // The composed character (@ [ ] { } | ~ \ on many layouts) arrives
            // through the OS text input, dead keys included.
            (true, false) => None,
            (true, true) => macos_meta_seq(control_key, m.shift),
            (false, _) => alt_meta_seq(keystroke),
        };
    }

    // Printable keys (plain char, shift+char) are left to the OS text input.
    None
}

/// The line-editing chords macOS users expect, as the readline / zsh emacs
/// keys every shell binds. The xterm forms (`ESC[1;3D` for Option+Left) are
/// unbound in stock zsh and bash, which print `;3D` instead of moving.
fn macos_editing_seq(key: &str, m: &Modifiers) -> Option<&'static [u8]> {
    if m.control || m.shift || m.function {
        return None;
    }
    Some(match (m.alt, m.platform, key) {
        (true, false, "left") => b"\x1bb",
        (true, false, "right") => b"\x1bf",
        (true, false, "backspace") => b"\x1b\x7f",
        (true, false, "delete") => b"\x1bd",
        (false, true, "left") => b"\x01",
        (false, true, "right") => b"\x05",
        (false, true, "backspace") => b"\x15",
        _ => return None,
    })
}

/// Option as Meta on macOS: ESC plus the key without Option applied. gpui's
/// `key_char` holds the composed character there (Option+B is "∫"), so it must
/// not be used.
fn macos_meta_seq(key: &str, is_shift: bool) -> Option<Vec<u8>> {
    if key == "space" {
        return Some(b"\x1b ".to_vec());
    }
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // gpui keeps letters lower case and reports Shift as a modifier.
    let c = if is_shift { c.to_ascii_uppercase() } else { c };
    let mut bytes = vec![0x1b];
    bytes.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
    Some(bytes)
}

/// Alt as Meta on Windows and Linux: prefix any printable char with ESC.
fn alt_meta_seq(keystroke: &Keystroke) -> Option<Vec<u8>> {
    if let Some(kc) = &keystroke.key_char {
        return Some([b"\x1b", kc.as_bytes()].concat());
    }
    let k = keystroke.key.as_str();
    (k.len() == 1).then(|| [b"\x1b", k.as_bytes()].concat())
}

fn modifier_code(m: &Modifiers) -> u8 {
    1 + m.shift as u8 + m.alt as u8 * 2 + m.control as u8 * 4
}

/// Arrows, Home and End. A modified key keeps the CSI `1;mod` form in both
/// cursor key modes, as xterm does.
fn cursor_key_seq(letter: u8, mc: u8, is_app_cursor: bool) -> Vec<u8> {
    if mc > 1 {
        return format!("\x1b[1;{}{}", mc, letter as char).into_bytes();
    }
    let introducer = if is_app_cursor { b'O' } else { b'[' };
    vec![0x1b, introducer, letter]
}

fn tilde_seq(n: u8, mc: u8) -> Vec<u8> {
    if mc > 1 {
        format!("\x1b[{};{}~", n, mc).into_bytes()
    } else {
        format!("\x1b[{}~", n).into_bytes()
    }
}

/// F1 to F4. A modified key moves from SS3 to the CSI `1;mod` form, as in
/// xterm.
fn ss3_function_seq(letter: u8, mc: u8) -> Vec<u8> {
    if mc > 1 {
        return format!("\x1b[1;{}{}", mc, letter as char).into_bytes();
    }
    vec![0x1b, b'O', letter]
}

fn special_key_seq(key: &str, m: &Modifiers, is_app_cursor: bool) -> Option<Vec<u8>> {
    let mc = modifier_code(m);
    Some(match key {
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "enter" | "return" => b"\r".to_vec(),
        "escape" => b"\x1b".to_vec(),
        // Alt+Backspace deletes a word back in readline and zsh.
        "backspace" if m.alt => b"\x1b\x7f".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "up" => cursor_key_seq(b'A', mc, is_app_cursor),
        "down" => cursor_key_seq(b'B', mc, is_app_cursor),
        "right" => cursor_key_seq(b'C', mc, is_app_cursor),
        "left" => cursor_key_seq(b'D', mc, is_app_cursor),
        "home" => cursor_key_seq(b'H', mc, is_app_cursor),
        "end" => cursor_key_seq(b'F', mc, is_app_cursor),
        "pageup" => tilde_seq(5, mc),
        "pagedown" => tilde_seq(6, mc),
        "insert" => tilde_seq(2, mc),
        "delete" => tilde_seq(3, mc),
        "f1" => ss3_function_seq(b'P', mc),
        "f2" => ss3_function_seq(b'Q', mc),
        "f3" => ss3_function_seq(b'R', mc),
        "f4" => ss3_function_seq(b'S', mc),
        "f5" => tilde_seq(15, mc),
        // f6 skips 16 (historical VT convention)
        "f6" => tilde_seq(17, mc),
        "f7" => tilde_seq(18, mc),
        "f8" => tilde_seq(19, mc),
        "f9" => tilde_seq(20, mc),
        "f10" => tilde_seq(21, mc),
        // f11 skips 22 (historical VT convention)
        "f11" => tilde_seq(23, mc),
        "f12" => tilde_seq(24, mc),
        _ => return None,
    })
}

fn ctrl_to_bytes(key: &str) -> Option<Vec<u8>> {
    if key.len() != 1 {
        return None;
    }
    let c = key.chars().next().expect("length checked above");
    match c {
        'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
        'A'..='Z' => Some(vec![c as u8 - b'A' + 1]),
        ' ' | '@' => Some(vec![0x00]),
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' => Some(vec![0x1f]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    fn plain(key: &str) -> Keystroke {
        Keystroke {
            modifiers: Modifiers::none(),
            key: key.to_string(),
            key_char: if key.len() == 1 {
                Some(key.to_string())
            } else {
                None
            },
        }
    }

    fn with_mods(key: &str, mods: Modifiers) -> Keystroke {
        Keystroke {
            modifiers: mods,
            key: key.to_string(),
            key_char: None,
        }
    }

    #[test]
    fn ctrl_a_to_0x01() {
        let ks = with_mods(
            "a",
            Modifiers {
                control: true,
                ..Modifiers::none()
            },
        );
        assert_eq!(keystroke_to_bytes(&ks, false, false), Some(vec![0x01]));
    }

    #[test]
    fn tab_to_0x09() {
        assert_eq!(
            keystroke_to_bytes(&plain("tab"), false, false),
            Some(vec![0x09])
        );
    }

    #[test]
    fn enter_to_0x0d() {
        assert_eq!(
            keystroke_to_bytes(&plain("enter"), false, false),
            Some(vec![0x0d])
        );
    }

    #[test]
    fn up_without_mods() {
        assert_eq!(
            keystroke_to_bytes(&plain("up"), false, false),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn ctrl_up() {
        let ks = with_mods(
            "up",
            Modifiers {
                control: true,
                ..Modifiers::none()
            },
        );
        assert_eq!(
            keystroke_to_bytes(&ks, false, false),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn app_cursor_mode_sends_plain_arrows_home_and_end_as_ss3() {
        for (key, expected) in [
            ("up", b"\x1bOA"),
            ("down", b"\x1bOB"),
            ("right", b"\x1bOC"),
            ("left", b"\x1bOD"),
            ("home", b"\x1bOH"),
            ("end", b"\x1bOF"),
        ] {
            assert_eq!(
                keystroke_to_bytes_on(&plain(key), false, true, false),
                Some(expected.to_vec()),
                "key {key}"
            );
        }
    }

    #[test]
    fn home_and_end_use_csi_outside_app_cursor_mode() {
        assert_eq!(
            keystroke_to_bytes(&plain("home"), false, false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&plain("end"), false, false),
            Some(b"\x1b[F".to_vec())
        );
    }

    #[test]
    fn app_cursor_mode_keeps_modified_keys_in_csi_form() {
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::none()
        };
        let shift = Modifiers {
            shift: true,
            ..Modifiers::none()
        };
        assert_eq!(
            keystroke_to_bytes_on(&with_mods("up", ctrl), false, true, false),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes_on(&with_mods("home", shift), false, true, false),
            Some(b"\x1b[1;2H".to_vec())
        );
    }

    #[test]
    fn app_cursor_mode_leaves_other_keys_alone() {
        assert_eq!(
            keystroke_to_bytes_on(&plain("pageup"), false, true, false),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes_on(&plain("tab"), false, true, false),
            Some(vec![0x09])
        );
        // The macOS word-movement chords are not cursor keys.
        assert_eq!(
            keystroke_to_bytes_on(&with_mods("left", ALT), false, true, true),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn wheel_arrows_repeat_one_key_per_line() {
        assert_eq!(wheel_arrow_bytes(2, false), b"\x1b[A\x1b[A".to_vec());
        assert_eq!(wheel_arrow_bytes(-3, false), b"\x1b[B\x1b[B\x1b[B".to_vec());
        assert_eq!(wheel_arrow_bytes(1, true), b"\x1bOA".to_vec());
        assert_eq!(wheel_arrow_bytes(-1, true), b"\x1bOB".to_vec());
        assert!(wheel_arrow_bytes(0, false).is_empty());
    }

    #[test]
    fn plain_a_is_none() {
        assert_eq!(keystroke_to_bytes(&plain("a"), false, false), None);
    }

    #[test]
    fn cmd_c_is_none() {
        let ks = with_mods(
            "c",
            Modifiers {
                platform: true,
                ..Modifiers::none()
            },
        );
        assert_eq!(keystroke_to_bytes(&ks, false, false), None);
    }

    #[test]
    fn ctrl_shift_c_is_none() {
        // The clipboard copy chord must not also send SIGINT to the shell.
        let ks = with_mods(
            "c",
            Modifiers {
                control: true,
                shift: true,
                ..Modifiers::none()
            },
        );
        assert_eq!(keystroke_to_bytes(&ks, false, false), None);
    }

    // With the Russian layout active, gpui names the C key after the character
    // it prints: upper case on Windows, lower case on macOS. Both must still
    // interrupt the running command.
    #[test]
    fn ctrl_c_on_the_russian_layout_still_interrupts() {
        for key in ["\u{0421}", "\u{0441}"] {
            let ks = with_mods(
                key,
                Modifiers {
                    control: true,
                    ..Modifiers::none()
                },
            );
            assert_eq!(
                keystroke_to_bytes(&ks, false, false),
                Some(vec![0x03]),
                "key {key}"
            );
        }
    }

    #[test]
    fn ctrl_shift_c_on_the_russian_layout_is_none() {
        let ks = with_mods(
            "\u{0421}",
            Modifiers {
                control: true,
                shift: true,
                ..Modifiers::none()
            },
        );
        assert_eq!(keystroke_to_bytes(&ks, false, false), None);
    }

    #[test]
    fn ctrl_shift_v_is_none() {
        let ks = with_mods(
            "v",
            Modifiers {
                control: true,
                shift: true,
                ..Modifiers::none()
            },
        );
        assert_eq!(keystroke_to_bytes(&ks, false, false), None);
    }

    #[test]
    fn ctrl_c_still_sigint() {
        // Plain Ctrl+C (no shift) must still interrupt.
        let ks = with_mods(
            "c",
            Modifiers {
                control: true,
                ..Modifiers::none()
            },
        );
        assert_eq!(keystroke_to_bytes(&ks, false, false), Some(vec![0x03]));
    }

    const ALT: Modifiers = Modifiers {
        alt: true,
        control: false,
        shift: false,
        platform: false,
        function: false,
    };
    const CMD: Modifiers = Modifiers {
        platform: true,
        control: false,
        shift: false,
        alt: false,
        function: false,
    };

    fn on_macos(ks: &Keystroke, is_option_meta: bool) -> Option<Vec<u8>> {
        keystroke_to_bytes_on(ks, is_option_meta, false, true)
    }

    /// What gpui reports on macOS for Option+key: `key` without Option,
    /// `key_char` with it.
    fn option_char(key: &str, composed: &str) -> Keystroke {
        Keystroke {
            modifiers: ALT,
            key: key.to_string(),
            key_char: Some(composed.to_string()),
        }
    }

    #[test]
    fn macos_option_arrows_move_by_word() {
        for is_option_meta in [false, true] {
            assert_eq!(
                on_macos(&with_mods("left", ALT), is_option_meta),
                Some(b"\x1bb".to_vec())
            );
            assert_eq!(
                on_macos(&with_mods("right", ALT), is_option_meta),
                Some(b"\x1bf".to_vec())
            );
        }
    }

    #[test]
    fn macos_option_backspace_and_delete_remove_a_word() {
        assert_eq!(
            on_macos(&with_mods("backspace", ALT), false),
            Some(b"\x1b\x7f".to_vec())
        );
        assert_eq!(
            on_macos(&with_mods("delete", ALT), false),
            Some(b"\x1bd".to_vec())
        );
    }

    #[test]
    fn macos_cmd_arrows_and_backspace_edit_the_line() {
        assert_eq!(on_macos(&with_mods("left", CMD), false), Some(vec![0x01]));
        assert_eq!(on_macos(&with_mods("right", CMD), false), Some(vec![0x05]));
        assert_eq!(
            on_macos(&with_mods("backspace", CMD), false),
            Some(vec![0x15])
        );
    }

    #[test]
    fn macos_other_cmd_chords_stay_with_the_app() {
        for key in ["c", "up", "down", "delete", "w"] {
            assert_eq!(on_macos(&with_mods(key, CMD), false), None, "key {key}");
        }
        let cmd_shift = Modifiers { shift: true, ..CMD };
        assert_eq!(on_macos(&with_mods("left", cmd_shift), false), None);
    }

    #[test]
    fn macos_option_shift_arrow_keeps_the_xterm_form() {
        let ks = with_mods("left", Modifiers { shift: true, ..ALT });
        assert_eq!(on_macos(&ks, false), Some(b"\x1b[1;4D".to_vec()));
    }

    #[test]
    fn macos_plain_backspace_and_arrows_are_unchanged() {
        assert_eq!(on_macos(&plain("backspace"), false), Some(vec![0x7f]));
        assert_eq!(on_macos(&plain("left"), false), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn macos_option_char_is_left_to_the_text_input_by_default() {
        // German Option+L types "@", US Option+B types "∫", Option+N is a dead key.
        assert_eq!(on_macos(&option_char("l", "@"), false), None);
        assert_eq!(on_macos(&option_char("b", "\u{222b}"), false), None);
        assert_eq!(on_macos(&option_char("n", "\u{2dc}"), false), None);
    }

    #[test]
    fn macos_option_space_types_a_plain_space_by_default() {
        assert_eq!(
            on_macos(&option_char("space", " "), false),
            Some(b" ".to_vec())
        );
        let ks = Keystroke {
            modifiers: Modifiers { shift: true, ..ALT },
            ..option_char("space", " ")
        };
        assert_eq!(on_macos(&ks, false), Some(b" ".to_vec()));
    }

    #[test]
    fn macos_option_as_meta_sends_escape_and_the_unmodified_key() {
        assert_eq!(
            on_macos(&option_char("b", "\u{222b}"), true),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            on_macos(&option_char("l", "@"), true),
            Some(b"\x1bl".to_vec())
        );
        assert_eq!(
            on_macos(&option_char(".", "\u{2026}"), true),
            Some(b"\x1b.".to_vec())
        );
        assert_eq!(
            on_macos(&option_char("space", " "), true),
            Some(b"\x1b ".to_vec())
        );
        // Option+N is a dead key: gpui reports the accent alone as key_char.
        assert_eq!(
            on_macos(&option_char("n", "\u{2dc}"), true),
            Some(b"\x1bn".to_vec())
        );
        // gpui folds Shift into a symbol key and clears the modifier.
        assert_eq!(
            on_macos(&option_char("!", "\u{2044}"), true),
            Some(b"\x1b!".to_vec())
        );
    }

    #[test]
    fn macos_option_as_meta_honours_shift() {
        let ks = with_mods("b", Modifiers { shift: true, ..ALT });
        assert_eq!(on_macos(&ks, true), Some(b"\x1bB".to_vec()));
    }

    #[test]
    fn macos_option_as_meta_on_the_russian_layout_uses_the_physical_key() {
        assert_eq!(
            on_macos(&option_char("\u{0438}", "\u{0438}"), true),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn macos_ctrl_option_char_is_none() {
        let ks = with_mods(
            "b",
            Modifiers {
                control: true,
                ..ALT
            },
        );
        assert_eq!(on_macos(&ks, true), None);
        assert_eq!(on_macos(&ks, false), None);
    }

    #[test]
    fn alt_stays_meta_off_macos() {
        let alt_b = Keystroke {
            modifiers: ALT,
            key: "b".into(),
            key_char: Some("b".into()),
        };
        for is_option_meta in [false, true] {
            assert_eq!(
                keystroke_to_bytes_on(&alt_b, is_option_meta, false, false),
                Some(b"\x1bb".to_vec())
            );
            assert_eq!(
                keystroke_to_bytes_on(&with_mods("left", ALT), is_option_meta, false, false),
                Some(b"\x1b[1;3D".to_vec())
            );
            assert_eq!(
                keystroke_to_bytes_on(&with_mods("backspace", ALT), is_option_meta, false, false),
                Some(b"\x1b\x7f".to_vec())
            );
            assert_eq!(
                keystroke_to_bytes_on(&with_mods("left", CMD), is_option_meta, false, false),
                None
            );
        }
    }

    #[test]
    fn plain_function_keys_keep_their_short_form() {
        assert_eq!(
            keystroke_to_bytes(&plain("f1"), false, false),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&plain("f4"), false, false),
            Some(b"\x1bOS".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&plain("f5"), false, false),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&plain("f12"), false, false),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn modified_function_keys_carry_the_modifier() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::none()
        };
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::none()
        };
        assert_eq!(
            keystroke_to_bytes(&with_mods("f1", ctrl), false, false),
            Some(b"\x1b[1;5P".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&with_mods("f5", shift), false, false),
            Some(b"\x1b[15;2~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&with_mods("f12", ctrl), false, false),
            Some(b"\x1b[24;5~".to_vec())
        );
    }
}
