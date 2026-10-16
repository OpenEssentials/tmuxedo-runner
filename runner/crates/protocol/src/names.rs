//! Names people give workspaces, tabs and panes (docs/protocol.md "Names").
//! The same rules as `packages/protocol/src/names.ts`; both are checked
//! against `packages/protocol/fixtures/names.json`.
//!
//! 1. Whitespace of any kind is a plain space.
//! 2. C0/C1 controls, DEL, bidi overrides/isolates/marks, zero width space
//!    and the BOM are removed (ZWJ and variation selectors stay, for emoji).
//! 3. Runs of spaces become one; the ends are trimmed.
//! 4. At most [`NAME_MAX`] code points, trimmed again.
//!
//! An empty result means "the default name": see [`default_workspace_name`]
//! and [`default_tab_name`]; a pane has none (its title is shown).

/// The longest name, in Unicode code points.
pub const NAME_MAX: usize = 64;

/// Unicode White_Space minus the C1 NEL (which is removed).
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\u{09}'..='\u{0d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

fn is_dropped(c: char) -> bool {
    matches!(
        c,
        '\u{00}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{200b}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// A name as stored (see the module rules). May be empty.
pub fn clean_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(NAME_MAX * 4));
    let mut count = 0usize;
    let mut space = false;
    for c in raw.chars() {
        if is_space(c) {
            space = count > 0;
            continue;
        }
        if is_dropped(c) {
            continue;
        }
        if space {
            if count + 1 >= NAME_MAX {
                break;
            }
            out.push(' ');
            count += 1;
            space = false;
        }
        if count >= NAME_MAX {
            break;
        }
        out.push(c);
        count += 1;
    }
    out.trim_end_matches(' ').to_string()
}

/// `w<n>` → `default` for n = 1, else `n`; anything else is kept as is.
pub fn default_workspace_name(id: &str) -> String {
    match id.strip_prefix('w').and_then(|n| n.parse::<u64>().ok()) {
        Some(1) => "default".into(),
        Some(n) => n.to_string(),
        None => id.into(),
    }
}

/// `w<n>:t<m>` → `m`; anything else is kept as is.
pub fn default_tab_name(id: &str) -> String {
    id.rsplit_once(":t")
        .and_then(|(_, m)| m.parse::<u64>().ok())
        .map(|m| m.to_string())
        .unwrap_or_else(|| id.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_at_the_cut_is_not_kept() {
        let s = format!("{} xyz", "a".repeat(63));
        assert_eq!(clean_name(&s), "a".repeat(63));
    }

    #[test]
    fn defaults() {
        assert_eq!(default_workspace_name("w1"), "default");
        assert_eq!(default_workspace_name("w9"), "9");
        assert_eq!(default_workspace_name("x"), "x");
        assert_eq!(default_tab_name("w2:t7"), "7");
        assert_eq!(default_tab_name("x"), "x");
    }
}
