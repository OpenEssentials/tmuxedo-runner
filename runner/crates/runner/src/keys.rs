//! tmux-style key names for `pane.send_keys` → bytes for the PTY.

pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    let fixed: Option<&[u8]> = match lower.as_str() {
        "enter" | "return" | "c-m" | "cr" => Some(b"\r"),
        "tab" | "c-i" => Some(b"\t"),
        "btab" | "s-tab" | "backtab" => Some(b"\x1b[Z"),
        "escape" | "esc" | "c-[" => Some(b"\x1b"),
        "backspace" | "bspace" | "bs" => Some(b"\x7f"),
        "space" | "spc" => Some(b" "),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" | "pgup" | "ppage" => Some(b"\x1b[5~"),
        "pagedown" | "pgdn" | "npage" => Some(b"\x1b[6~"),
        "delete" | "del" | "dc" => Some(b"\x1b[3~"),
        "insert" | "ins" | "ic" => Some(b"\x1b[2~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        "c-space" | "c-@" => Some(b"\x00"),
        _ => None,
    };
    if let Some(b) = fixed {
        return Some(b.to_vec());
    }
    if let Some(rest) = name.strip_prefix("M-").or_else(|| name.strip_prefix("m-")) {
        let mut v = vec![0x1b];
        v.extend(key_bytes(rest)?);
        return Some(v);
    }
    if let Some(rest) = name
        .strip_prefix("C-")
        .or_else(|| name.strip_prefix("c-"))
        .or_else(|| name.strip_prefix("Ctrl-"))
    {
        let mut chars = rest.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        let c = c.to_ascii_lowercase();
        return match c {
            'a'..='z' => Some(vec![(c as u8) & 0x1f]),
            '\\' => Some(vec![0x1c]),
            ']' => Some(vec![0x1d]),
            '^' => Some(vec![0x1e]),
            '_' => Some(vec![0x1f]),
            '?' => Some(vec![0x7f]),
            _ => None,
        };
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c.to_string().into_bytes()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(key_bytes("Enter").unwrap(), b"\r");
        assert_eq!(key_bytes("C-c").unwrap(), vec![3]);
        assert_eq!(key_bytes("M-x").unwrap(), b"\x1bx");
        assert_eq!(key_bytes("M-Enter").unwrap(), b"\x1b\r");
        assert_eq!(key_bytes("Up").unwrap(), b"\x1b[A");
        assert_eq!(key_bytes("F5").unwrap(), b"\x1b[15~");
        assert_eq!(key_bytes("q").unwrap(), b"q");
        assert_eq!(key_bytes("é").unwrap(), "é".as_bytes());
        assert!(key_bytes("NoSuchKey").is_none());
        assert!(key_bytes("C-xy").is_none());
    }
}
