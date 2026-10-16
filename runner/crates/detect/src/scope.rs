//! Scopes: which slice of the input a signal looks at
//! (docs/design/agent-detection.md §5).

use crate::Input;
use std::fmt;

/// Largest `last N` / `first N`.
pub const MAX_COUNT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Screen,
    Title,
    Progress,
    Last(usize),
    First(usize),
    BelowRule,
    Box,
    AboveBox,
    AboveBoxLine,
    AfterComposer,
    BeforeComposer,
    UnlessComposer,
}

impl Scope {
    pub fn parse(s: &str) -> Result<Scope, String> {
        let fixed = match s {
            "screen" => Some(Scope::Screen),
            "title" => Some(Scope::Title),
            "progress" => Some(Scope::Progress),
            "below-rule" => Some(Scope::BelowRule),
            "box" => Some(Scope::Box),
            "above-box" => Some(Scope::AboveBox),
            "above-box-line" => Some(Scope::AboveBoxLine),
            "after-composer" => Some(Scope::AfterComposer),
            "before-composer" => Some(Scope::BeforeComposer),
            "unless-composer" => Some(Scope::UnlessComposer),
            _ => None,
        };
        if let Some(scope) = fixed {
            return Ok(scope);
        }
        let (make, count): (fn(usize) -> Scope, &str) = if let Some(n) = s.strip_prefix("last ") {
            (Scope::Last, n)
        } else if let Some(n) = s.strip_prefix("first ") {
            (Scope::First, n)
        } else {
            return Err(format!("unknown scope {s:?}"));
        };
        match count.parse::<usize>() {
            Ok(n) if (1..=MAX_COUNT).contains(&n) && count.bytes().all(|b| b.is_ascii_digit()) => Ok(make(n)),
            _ => Err(format!("scope {s:?}: the count must be a number from 1 to {MAX_COUNT}")),
        }
    }

    /// Whether the scope reads the screen (not the title or progress).
    pub fn on_screen(self) -> bool {
        !matches!(self, Scope::Title | Scope::Progress)
    }

    pub fn slice<'a>(self, input: &Input<'a>) -> &'a str {
        let s = input.screen;
        match self {
            Scope::Screen => s,
            Scope::Title => input.title,
            Scope::Progress => input.progress,
            Scope::Last(n) => last(s, n),
            Scope::First(n) => first(s, n),
            Scope::BelowRule => below_rule(s),
            Scope::Box => prompt_box(s).map_or("", |(_, body)| body),
            Scope::AboveBox => above_box(s),
            Scope::AboveBoxLine => above_box(s).lines().rev().find(|l| !blank(l)).unwrap_or(""),
            Scope::AfterComposer => after_composer(s),
            Scope::BeforeComposer => live_composer(s).map_or(s, |l| &s[..l.start]),
            Scope::UnlessComposer => live_composer(s).map_or(s, |_| ""),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Screen => f.write_str("screen"),
            Scope::Title => f.write_str("title"),
            Scope::Progress => f.write_str("progress"),
            Scope::Last(n) => write!(f, "last {n}"),
            Scope::First(n) => write!(f, "first {n}"),
            Scope::BelowRule => f.write_str("below-rule"),
            Scope::Box => f.write_str("box"),
            Scope::AboveBox => f.write_str("above-box"),
            Scope::AboveBoxLine => f.write_str("above-box-line"),
            Scope::AfterComposer => f.write_str("after-composer"),
            Scope::BeforeComposer => f.write_str("before-composer"),
            Scope::UnlessComposer => f.write_str("unless-composer"),
        }
    }
}

/// One screen line: `text` without its line break (and without a CR before it);
/// `start..next` covers the line and its break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Line<'a> {
    pub text: &'a str,
    pub start: usize,
    pub next: usize,
}

pub(crate) fn lines(s: &str) -> Vec<Line<'_>> {
    let mut out = vec![];
    let mut start = 0;
    for raw in s.split_inclusive('\n') {
        let next = start + raw.len();
        let text = raw.strip_suffix('\n').unwrap_or(raw);
        let text = text.strip_suffix('\r').unwrap_or(text);
        out.push(Line { text, start, next });
        start = next;
    }
    out
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn last(s: &str, n: usize) -> &str {
    let from = lines(s).into_iter().rev().filter(|l| !blank(l.text)).take(n).last();
    from.map_or("", |l| &s[l.start..])
}

fn first(s: &str, n: usize) -> &str {
    let to = lines(s).into_iter().filter(|l| !blank(l.text)).take(n).last();
    to.map_or("", |l| &s[..l.next])
}

/// `─` rule lines: only `─`, or at least three `─` then anything (a titled rule).
pub(crate) fn is_rule(line: &str) -> bool {
    let t = line.trim();
    let dashes = t.chars().take_while(|&c| c == '─').count();
    dashes >= 3 || (dashes > 0 && dashes == t.chars().count())
}

fn below_rule(s: &str) -> &str {
    lines(s)
        .into_iter()
        .rev()
        .find(|l| is_rule(l.text))
        .map_or(s, |l| &s[l.next..])
}

/// The prompt box, between the last two rule lines: (byte offset of its top rule, its body).
fn prompt_box(s: &str) -> Option<(usize, &str)> {
    let ls = lines(s);
    let bottom = ls.iter().rposition(|l| is_rule(l.text))?;
    let top = ls[..bottom].iter().rposition(|l| is_rule(l.text))?;
    Some((ls[top].start, &s[ls[top].next..ls[bottom].start]))
}

fn above_box(s: &str) -> &str {
    prompt_box(s).map_or(s, |(top, _)| &s[..top])
}

fn is_composer(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn is_reply_marker(line: &str) -> bool {
    line.starts_with(['•', '■', '✗', '✓'])
}

fn after_composer(s: &str) -> &str {
    lines(s)
        .into_iter()
        .rev()
        .find(|l| is_composer(l.text))
        .map_or(s, |l| &s[l.next..])
}

/// The last composer line, when no reply marker follows it.
fn live_composer(s: &str) -> Option<Line<'_>> {
    let ls = lines(s);
    let i = ls.iter().rposition(|l| is_composer(l.text))?;
    (!ls[i + 1..].iter().any(|l| is_reply_marker(l.text))).then_some(ls[i])
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod tests {
    use super::*;

    fn at(scope: &str, screen: &str) -> String {
        Scope::parse(scope)
            .unwrap()
            .slice(&Input {
                screen,
                title: "T",
                progress: "P",
            })
            .to_string()
    }

    #[test]
    fn parse_and_display_round_trip() {
        for s in [
            "screen",
            "title",
            "progress",
            "last 1",
            "last 500",
            "first 7",
            "below-rule",
            "box",
            "above-box",
            "above-box-line",
            "after-composer",
            "before-composer",
            "unless-composer",
        ] {
            assert_eq!(Scope::parse(s).unwrap().to_string(), s);
        }
        for bad in [
            "", "whole", "last", "last 0", "last 501", "last -1", "last +3", "first x", "last  2", "LAST 2",
        ] {
            assert!(Scope::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Scope::parse("nope").unwrap_err(), "unknown scope \"nope\"");
        assert_eq!(
            Scope::parse("first 0").unwrap_err(),
            "scope \"first 0\": the count must be a number from 1 to 500"
        );
    }

    #[test]
    fn on_screen() {
        assert!(!Scope::Title.on_screen());
        assert!(!Scope::Progress.on_screen());
        assert!(Scope::Screen.on_screen());
        assert!(Scope::Last(1).on_screen());
    }

    #[test]
    fn title_progress_and_screen() {
        assert_eq!(at("title", "x"), "T");
        assert_eq!(at("progress", "x"), "P");
        assert_eq!(at("screen", "a\nb"), "a\nb");
    }

    #[test]
    fn lines_track_offsets_crs_and_trailing_breaks() {
        let ls = lines("a\r\nbc\n\nd");
        let texts: Vec<_> = ls.iter().map(|l| l.text).collect();
        assert_eq!(texts, ["a", "bc", "", "d"]);
        assert_eq!((ls[1].start, ls[1].next), (3, 6));
        assert_eq!(lines("x\n").len(), 1);
        assert!(lines("").is_empty());
    }

    #[test]
    fn last_counts_non_blank_lines_from_the_bottom() {
        let s = "one\n\ntwo\n  \nthree\n\n";
        assert_eq!(at("last 1", s), "three\n\n");
        assert_eq!(at("last 2", s), "two\n  \nthree\n\n");
        assert_eq!(at("last 9", s), s, "fewer lines: from the first non-blank one");
        assert_eq!(at("last 9", "\n\nx"), "x");
        assert_eq!(at("last 3", "\n \n"), "");
        assert_eq!(at("last 1", "dup\ndup"), "dup", "the bottom occurrence");
    }

    #[test]
    fn first_counts_non_blank_lines_from_the_top() {
        let s = "\none\n\ntwo\nthree";
        assert_eq!(at("first 1", s), "\none\n");
        assert_eq!(at("first 2", s), "\none\n\ntwo\n");
        assert_eq!(at("first 3", s), s);
        assert_eq!(at("first 9", s), s);
        assert_eq!(at("first 2", "a\nb\n\n"), "a\nb\n");
        assert_eq!(at("first 1", " \n"), "");
    }

    #[test]
    fn rules() {
        assert!(is_rule("─"));
        assert!(is_rule("  ──────  "));
        assert!(is_rule("─── Plan ───"));
        assert!(!is_rule("── x"), "two dashes then text is not a rule");
        assert!(!is_rule("x ───"));
        assert!(!is_rule(""));
        assert!(!is_rule("   "));
        assert!(!is_rule("-----"), "ASCII dashes are not box rules");
    }

    #[test]
    fn below_rule_is_after_the_last_rule() {
        assert_eq!(at("below-rule", "a\n───\nb\n─────\nc\nd"), "c\nd");
        assert_eq!(at("below-rule", "a\n───"), "");
        assert_eq!(at("below-rule", "no rules"), "no rules");
    }

    #[test]
    fn prompt_box_scopes() {
        let s = "reply\n\n──────\n❯ hi\n  more\n──────\n  footer";
        assert_eq!(at("box", s), "❯ hi\n  more\n");
        assert_eq!(at("above-box", s), "reply\n\n");
        assert_eq!(at("above-box-line", s), "reply");
        // Three rules: the box is between the last two.
        let s3 = "──\nold\n──\n❯ x\n──";
        assert_eq!(at("box", s3), "❯ x\n");
        assert_eq!(at("above-box", s3), "──\nold\n");
        // Only one rule: no box.
        assert_eq!(at("box", "a\n──\n❯"), "");
        assert_eq!(at("above-box", "a\n──\n❯"), "a\n──\n❯");
        assert_eq!(at("above-box-line", "a\n──\n❯"), "❯");
        assert_eq!(at("above-box-line", "──\n──\n"), "");
        assert_eq!(at("box", "──\n──"), "");
    }

    #[test]
    fn prompt_box_body_stops_at_the_next_rule() {
        // top rule, body, a rule, more body: the body ends at the first rule below the top.
        assert_eq!(prompt_box("t\n──\nbody\n──\n"), Some((2, "body\n")));
    }

    #[test]
    fn composer_scopes() {
        let live = "• done\n› next\n  tail";
        assert_eq!(at("after-composer", live), "  tail");
        assert_eq!(at("before-composer", live), "• done\n");
        assert_eq!(at("unless-composer", live), "");
        let stale = "› asked\n• answered";
        assert_eq!(at("after-composer", stale), "• answered");
        assert_eq!(at("before-composer", stale), stale);
        assert_eq!(at("unless-composer", stale), stale);
        let none = "nothing\n›x";
        assert_eq!(at("after-composer", none), none);
        assert_eq!(at("before-composer", none), none);
        assert_eq!(at("unless-composer", none), none);
        assert_eq!(at("before-composer", "a\n›"), "a\n");
        for m in ["■ x", "✗ x", "✓ x"] {
            assert_eq!(at("unless-composer", &format!("› q\n{m}")), format!("› q\n{m}"));
        }
        assert!(!is_composer(" › indented"));
    }
}
