//! Encoding mouse events as terminal escape sequences: SGR (DECSET 1006)
//! when the app asked for it, else the original X10 form.

use crate::shell::{GridPoint, MouseModeFlags};

#[derive(Clone, Copy, Debug)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug)]
pub enum MouseEventKind {
    Press,
    Release,
    Motion,
}

/// Encode a mouse event as the bytes to send to the shell.
/// Returns None if the current mouse mode shouldn't forward this event.
pub fn encode_mouse_event(
    mode: MouseModeFlags,
    kind: MouseEventKind,
    button: MouseButton,
    point: GridPoint,
    shift: bool,
    ctrl: bool,
    alt: bool,
) -> Option<Vec<u8>> {
    if !mode.any_active() {
        return None;
    }

    if matches!(kind, MouseEventKind::Motion) && !mode.report_drag && !mode.report_motion {
        return None;
    }

    let mut button_code: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    // The X10 form has no release suffix: a release is button 3, whichever
    // button it was.
    if !mode.sgr && matches!(kind, MouseEventKind::Release) {
        button_code = 3;
    }

    if matches!(kind, MouseEventKind::Motion) {
        button_code += 32;
    }
    if shift {
        button_code += 4;
    }
    if alt {
        button_code += 8;
    }
    if ctrl {
        button_code += 16;
    }

    // Both forms count cells from 1.
    let col = point.column.saturating_add(1);
    let row = (point.line + 1).max(1) as usize;

    if !mode.sgr {
        return x10_report(button_code, col, row);
    }

    let suffix = if matches!(kind, MouseEventKind::Release) {
        'm'
    } else {
        'M'
    };

    let s = format!("\x1b[<{};{};{}{}", button_code, col, row, suffix);
    Some(s.into_bytes())
}

/// `ESC [ M` and three bytes, each value plus 32. A byte holds 255 at most,
/// so cells past column or row 223 cannot be reported.
fn x10_report(button_code: u32, col: usize, row: usize) -> Option<Vec<u8>> {
    const MAX_CELL: usize = 223;
    if col > MAX_CELL || row > MAX_CELL {
        return None;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        (button_code + 32) as u8,
        (col + 32) as u8,
        (row + 32) as u8,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLICK: MouseModeFlags = MouseModeFlags {
        report_click: true,
        report_drag: false,
        report_motion: false,
        sgr: false,
    };
    const CLICK_SGR: MouseModeFlags = MouseModeFlags { sgr: true, ..CLICK };

    fn encode(mode: MouseModeFlags, kind: MouseEventKind, button: MouseButton) -> Option<Vec<u8>> {
        let point = GridPoint { line: 4, column: 9 };
        encode_mouse_event(mode, kind, button, point, false, false, false)
    }

    #[test]
    fn nothing_is_sent_while_mouse_mode_is_off() {
        let off = MouseModeFlags::default();
        assert_eq!(encode(off, MouseEventKind::Press, MouseButton::Left), None);
        assert_eq!(
            encode(
                MouseModeFlags { sgr: true, ..off },
                MouseEventKind::Press,
                MouseButton::Left
            ),
            None
        );
    }

    #[test]
    fn sgr_mode_sends_sgr_reports() {
        assert_eq!(
            encode(CLICK_SGR, MouseEventKind::Press, MouseButton::Left),
            Some(b"\x1b[<0;10;5M".to_vec())
        );
        assert_eq!(
            encode(CLICK_SGR, MouseEventKind::Release, MouseButton::Right),
            Some(b"\x1b[<2;10;5m".to_vec())
        );
    }

    #[test]
    fn without_sgr_mode_reports_use_the_x10_form() {
        assert_eq!(
            encode(CLICK, MouseEventKind::Press, MouseButton::Left),
            Some(vec![0x1b, b'[', b'M', 32, 42, 37])
        );
        assert_eq!(
            encode(CLICK, MouseEventKind::Release, MouseButton::Left),
            Some(vec![0x1b, b'[', b'M', 35, 42, 37])
        );
        assert_eq!(
            encode(CLICK, MouseEventKind::Press, MouseButton::WheelDown),
            Some(vec![0x1b, b'[', b'M', 97, 42, 37])
        );
    }

    #[test]
    fn the_x10_form_drops_cells_it_cannot_encode() {
        let far = GridPoint {
            line: 4,
            column: 223,
        };
        let report = encode_mouse_event(
            CLICK,
            MouseEventKind::Press,
            MouseButton::Left,
            far,
            false,
            false,
            false,
        );
        assert_eq!(report, None);
    }

    #[test]
    fn motion_needs_a_drag_or_motion_mode() {
        assert_eq!(
            encode(CLICK_SGR, MouseEventKind::Motion, MouseButton::Left),
            None
        );
        let drag = MouseModeFlags {
            report_drag: true,
            ..CLICK_SGR
        };
        assert_eq!(
            encode(drag, MouseEventKind::Motion, MouseButton::Left),
            Some(b"\x1b[<32;10;5M".to_vec())
        );
    }
}
