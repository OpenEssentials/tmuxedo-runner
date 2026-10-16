//! Key events → the bytes a terminal would send (xterm conventions), so a
//! key typed in the TUI reaches the focused pane as the program expects it.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

/// `ctrl-b`, the prefix.
pub fn is_prefix(k: &KeyEvent) -> bool {
    k.code == KeyCode::Char('b')
        && k.modifiers.contains(KeyModifiers::CONTROL)
        && !k.modifiers.contains(KeyModifiers::ALT)
}

fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Encode a key for a pane. `app_cursor`: the pane asked for application cursor keys (DECCKM).
pub fn encode_key(k: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let m = k.modifiers;
    let (shift, alt, ctrl) = (
        m.contains(KeyModifiers::SHIFT),
        m.contains(KeyModifiers::ALT),
        m.contains(KeyModifiers::CONTROL),
    );
    let modn = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let cursor = |c: char| -> Vec<u8> {
        if modn > 1 {
            format!("\x1b[1;{modn}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modn > 1 {
            format!("\x1b[{n};{modn}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let mut alt_prefix = alt;
    let out = match k.code {
        KeyCode::Char(c) if ctrl => vec![ctrl_byte(c)?],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => {
            alt_prefix = false;
            b"\x1b[Z".to_vec()
        }
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        code => {
            alt_prefix = false;
            match code {
                KeyCode::Up => cursor('A'),
                KeyCode::Down => cursor('B'),
                KeyCode::Right => cursor('C'),
                KeyCode::Left => cursor('D'),
                KeyCode::Home => cursor('H'),
                KeyCode::End => cursor('F'),
                KeyCode::Insert => tilde(2),
                KeyCode::Delete => tilde(3),
                KeyCode::PageUp => tilde(5),
                KeyCode::PageDown => tilde(6),
                KeyCode::F(n @ 1..=4) => {
                    let c = (b'P' + n - 1) as char;
                    if modn > 1 {
                        format!("\x1b[1;{modn}{c}").into_bytes()
                    } else {
                        format!("\x1bO{c}").into_bytes()
                    }
                }
                KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]),
                _ => return None,
            }
        }
    };
    Some(if alt_prefix { [&[0x1b][..], &out].concat() } else { out })
}

/// Most text sent to the local clipboard in one OSC 52 sequence (UTF-8
/// bytes before base64; terminals commonly accept ~100 KB).
pub const OSC52_MAX: usize = 100_000;

/// OSC 52 "set clipboard" for `text` (cut at a char boundary to
/// [`OSC52_MAX`]); inside tmux wrapped in its DCS passthrough. Returns the
/// bytes, the characters copied and whether the text was cut.
pub fn osc52(text: &str, in_tmux: bool) -> (Vec<u8>, usize, bool) {
    use base64::Engine;
    let mut end = text.len().min(OSC52_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &text[..end];
    let b64 = base64::engine::general_purpose::STANDARD.encode(cut.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x07");
    let out = if in_tmux {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    };
    (out.into_bytes(), cut.chars().count(), end < text.len())
}

/// Encode a mouse event for a pane that enabled mouse reporting (DECSET
/// 9/1000/1002/1003 → `mode`, 1005/1006 → `enc`), at 0-based cell (`col`,
/// `row`) inside the pane. None if this mode does not report the event (or
/// the position cannot be expressed in the legacy encoding).
pub fn encode_mouse(
    kind: MouseEventKind,
    mods: KeyModifiers,
    col: u16,
    row: u16,
    mode: vt100::MouseProtocolMode,
    enc: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
    let btn = |b: MouseButton| match b {
        MouseButton::Left => 0u32,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    // (button code, release?)
    let (mut code, release) = match kind {
        MouseEventKind::Down(b) => (btn(b), false),
        MouseEventKind::Up(b) if mode != M::Press => (btn(b), true),
        MouseEventKind::Drag(b) if matches!(mode, M::ButtonMotion | M::AnyMotion) => (btn(b) + 32, false),
        MouseEventKind::Moved if mode == M::AnyMotion => (3 + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        _ => return None,
    };
    if mode == M::None {
        return None;
    }
    if mode != M::Press {
        if mods.contains(KeyModifiers::SHIFT) {
            code += 4;
        }
        if mods.contains(KeyModifiers::ALT) {
            code += 8;
        }
        if mods.contains(KeyModifiers::CONTROL) {
            code += 16;
        }
    }
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    Some(match enc {
        E::Sgr => format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes(),
        E::Default | E::Utf8 => {
            // Legacy encodings cannot say which button was released.
            let code = if release { 3 + (code & !3) } else { code };
            let mut out = b"\x1b[M".to_vec();
            for v in [code + 32, x + 32, y + 32] {
                if enc == E::Default {
                    if v > 255 {
                        return None;
                    }
                    out.push(v as u8);
                } else {
                    let ch = char::from_u32(v)?;
                    let mut b = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                }
            }
            out
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sets_the_clipboard() {
        let (b, n, cut) = osc52("héllo", false);
        assert_eq!(b, b"\x1b]52;c;aMOpbGxv\x07");
        assert_eq!((n, cut), (5, false));
        let (b, ..) = osc52("hi", true);
        assert_eq!(
            b, b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\",
            "tmux passthrough doubles ESC"
        );
        let big = "é".repeat(OSC52_MAX); // 2 bytes each
        let (b, n, cut) = osc52(&big, false);
        assert!(cut && n == OSC52_MAX / 2);
        assert!(b.len() < OSC52_MAX * 4 / 3 + 16);
    }

    #[test]
    fn mouse_encodes_like_xterm() {
        use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
        let n = KeyModifiers::NONE;
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        // SGR (1006): 1-based, M press / m release, real button on release.
        assert_eq!(
            encode_mouse(down, n, 4, 2, M::PressRelease, E::Sgr).unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            encode_mouse(up, n, 4, 2, M::PressRelease, E::Sgr).unwrap(),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Right),
                KeyModifiers::CONTROL,
                0,
                0,
                M::PressRelease,
                E::Sgr
            )
            .unwrap(),
            b"\x1b[<18;1;1M"
        );
        assert_eq!(
            encode_mouse(MouseEventKind::ScrollUp, n, 9, 9, M::PressRelease, E::Sgr).unwrap(),
            b"\x1b[<64;10;10M"
        );
        // Motion only in 1002 (with a button) / 1003 (any).
        assert_eq!(encode_mouse(drag, n, 1, 1, M::PressRelease, E::Sgr), None);
        assert_eq!(
            encode_mouse(drag, n, 1, 1, M::ButtonMotion, E::Sgr).unwrap(),
            b"\x1b[<32;2;2M"
        );
        assert_eq!(
            encode_mouse(MouseEventKind::Moved, n, 1, 1, M::ButtonMotion, E::Sgr),
            None
        );
        assert_eq!(
            encode_mouse(MouseEventKind::Moved, n, 1, 1, M::AnyMotion, E::Sgr).unwrap(),
            b"\x1b[<35;2;2M"
        );
        // X10 (9): presses only, no modifiers.
        assert_eq!(encode_mouse(up, n, 0, 0, M::Press, E::Sgr), None);
        // Legacy encoding: CSI M + 32-offset bytes; release is button 3.
        assert_eq!(
            encode_mouse(down, n, 0, 0, M::PressRelease, E::Default).unwrap(),
            b"\x1b[M !!"
        );
        assert_eq!(
            encode_mouse(up, n, 0, 0, M::PressRelease, E::Default).unwrap(),
            b"\x1b[M#!!"
        );
        assert_eq!(
            encode_mouse(down, n, 300, 0, M::PressRelease, E::Default),
            None,
            "beyond 223 columns"
        );
        assert_eq!(
            encode_mouse(down, n, 300, 0, M::PressRelease, E::Utf8).unwrap(),
            [b"\x1b[M ".as_slice(), "\u{14d}".as_bytes(), b"!"].concat()
        );
        assert_eq!(encode_mouse(down, n, 0, 0, M::None, E::Sgr), None);
    }

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn keys_encode_like_xterm() {
        let n = KeyModifiers::NONE;
        assert_eq!(encode_key(&k(KeyCode::Char('x'), n), false).unwrap(), b"x");
        assert_eq!(encode_key(&k(KeyCode::Char('é'), n), false).unwrap(), "é".as_bytes());
        assert_eq!(
            encode_key(&k(KeyCode::Char('c'), KeyModifiers::CONTROL), false).unwrap(),
            [3]
        );
        assert_eq!(
            encode_key(&k(KeyCode::Char('x'), KeyModifiers::ALT), false).unwrap(),
            b"\x1bx"
        );
        assert_eq!(encode_key(&k(KeyCode::Enter, n), false).unwrap(), b"\r");
        assert_eq!(encode_key(&k(KeyCode::Backspace, n), false).unwrap(), [0x7f]);
        assert_eq!(encode_key(&k(KeyCode::Up, n), false).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&k(KeyCode::Up, n), true).unwrap(), b"\x1bOA");
        assert_eq!(
            encode_key(&k(KeyCode::Left, KeyModifiers::CONTROL), true).unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(encode_key(&k(KeyCode::PageDown, n), false).unwrap(), b"\x1b[6~");
        assert_eq!(encode_key(&k(KeyCode::F(1), n), false).unwrap(), b"\x1bOP");
        assert_eq!(encode_key(&k(KeyCode::F(5), n), false).unwrap(), b"\x1b[15~");
        assert!(is_prefix(&k(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(!is_prefix(&k(KeyCode::Char('b'), n)));
    }
}
