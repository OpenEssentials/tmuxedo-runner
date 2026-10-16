//! The `when` language of agent files (docs/design/agent-detection.md §6):
//! `"text"`, `/regex/`, `line /regex/`, `not`, `and`, `or` and parentheses.

use regex::Regex;
use std::cell::OnceCell;
use std::fmt;

/// Most terms (texts and regexes) one condition may have.
pub const MAX_TERMS: usize = 64;
/// Deepest parenthesis nesting.
pub const MAX_DEPTH: usize = 8;
/// Longest text or regex literal, in characters.
pub const MAX_LITERAL: usize = 1000;

/// A parsed condition.
#[derive(Debug, Clone)]
pub enum Cond {
    /// Case-insensitive substring; stored lower-cased.
    Text(String),
    /// Regex anywhere in the scope.
    Regex(Regex),
    /// Regex on at least one line.
    Line(Regex),
    Not(Box<Cond>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
}

/// Why a condition did not parse; `at` is a byte offset into the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub at: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at byte {}: {}", self.at, self.message)
    }
}

fn fail<T>(at: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
        at,
        message: message.into(),
    })
}

/// The text a condition is evaluated against; lower-cases at most once.
pub struct Subject<'a> {
    text: &'a str,
    lower: OnceCell<String>,
}

impl<'a> Subject<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            lower: OnceCell::new(),
        }
    }

    fn lower(&self) -> &str {
        self.lower.get_or_init(|| self.text.to_lowercase())
    }
}

impl Cond {
    pub fn parse(src: &str) -> Result<Cond, ParseError> {
        let tokens = lex(src)?;
        let mut p = Parser {
            tokens,
            pos: 0,
            terms: 0,
            depth: 0,
            end: src.len(),
        };
        let cond = p.or()?;
        match p.tokens.get(p.pos) {
            None => Ok(cond),
            Some((at, _)) => fail(*at, "unexpected input after the condition"),
        }
    }

    pub fn eval(&self, s: &Subject<'_>) -> bool {
        match self {
            Cond::Text(needle) => s.lower().contains(needle.as_str()),
            Cond::Regex(re) => re.is_match(s.text),
            Cond::Line(re) => s.text.lines().any(|l| re.is_match(l)),
            Cond::Not(c) => !c.eval(s),
            Cond::And(a, b) => a.eval(s) && b.eval(s),
            Cond::Or(a, b) => a.eval(s) || b.eval(s),
        }
    }
}

/// Canonical form: every `and`/`or`/`not` operand parenthesised when compound,
/// so `Cond::parse(&c.to_string())` gives the same tree back.
impl fmt::Display for Cond {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cond::Text(t) => {
                f.write_str("\"")?;
                for ch in t.chars() {
                    if ch == '"' || ch == '\\' {
                        f.write_str("\\")?;
                    }
                    write!(f, "{ch}")?;
                }
                f.write_str("\"")
            }
            Cond::Regex(re) => write_regex(f, re.as_str()),
            Cond::Line(re) => {
                f.write_str("line ")?;
                write_regex(f, re.as_str())
            }
            Cond::Not(c) => write!(f, "not {}", Operand(c)),
            Cond::And(a, b) => write!(f, "{} and {}", Operand(a), Operand(b)),
            Cond::Or(a, b) => write!(f, "{} or {}", Operand(a), Operand(b)),
        }
    }
}

struct Operand<'a>(&'a Cond);

impl fmt::Display for Operand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Cond::Not(_) | Cond::And(..) | Cond::Or(..) => write!(f, "({})", self.0),
            atom => write!(f, "{atom}"),
        }
    }
}

fn write_regex(f: &mut fmt::Formatter<'_>, pattern: &str) -> fmt::Result {
    f.write_str("/")?;
    // An unescaped `/` would end the literal; an escaped one (`\/`) already reads back as `/`.
    let mut escaped = false;
    for ch in pattern.chars() {
        if ch == '/' && !escaped {
            f.write_str("\\")?;
        }
        write!(f, "{ch}")?;
        escaped = !escaped && ch == '\\';
    }
    f.write_str("/")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Text(String),
    Regex(String),
    Line,
    Not,
    And,
    Or,
    Open,
    Close,
}

fn lex(src: &str) -> Result<Vec<(usize, Tok)>, ParseError> {
    let mut out = vec![];
    let mut it = src.char_indices().peekable();
    while let Some((at, ch)) = it.next() {
        match ch {
            c if c.is_whitespace() => {}
            '(' => out.push((at, Tok::Open)),
            ')' => out.push((at, Tok::Close)),
            '"' => {
                let mut text = String::new();
                loop {
                    match it.next() {
                        None => return fail(at, "unterminated text"),
                        Some((_, '"')) => break,
                        Some((esc, '\\')) => match it.next() {
                            Some((_, c @ ('"' | '\\'))) => text.push(c),
                            _ => return fail(esc, "only \\\" and \\\\ may be escaped in text"),
                        },
                        Some((_, c)) => text.push(c),
                    }
                }
                literal_ok(at, &text, "text")?;
                out.push((at, Tok::Text(text)));
            }
            '/' => {
                let mut re = String::new();
                loop {
                    match it.next() {
                        None => return fail(at, "unterminated regex"),
                        Some((_, '/')) => break,
                        Some((_, '\\')) => match it.next() {
                            None => return fail(at, "unterminated regex"),
                            Some((_, '/')) => re.push('/'),
                            Some((_, c)) => {
                                re.push('\\');
                                re.push(c);
                            }
                        },
                        Some((_, c)) => re.push(c),
                    }
                }
                literal_ok(at, &re, "regex")?;
                out.push((at, Tok::Regex(re)));
            }
            c if c.is_alphabetic() => {
                let mut word = String::from(c);
                while let Some((_, c)) = it.next_if(|(_, c)| c.is_alphanumeric() || *c == '_') {
                    word.push(c);
                }
                out.push((
                    at,
                    match word.as_str() {
                        "line" => Tok::Line,
                        "not" => Tok::Not,
                        "and" => Tok::And,
                        "or" => Tok::Or,
                        _ => return fail(at, format!("unknown word {word:?} (expected line, not, and, or)")),
                    },
                ));
            }
            other => return fail(at, format!("unexpected character {other:?}")),
        }
    }
    Ok(out)
}

fn literal_ok(at: usize, value: &str, what: &str) -> Result<(), ParseError> {
    let n = value.chars().count();
    if n == 0 {
        return fail(at, format!("empty {what}"));
    }
    if n > MAX_LITERAL {
        return fail(at, format!("{what} is {n} characters, the limit is {MAX_LITERAL}"));
    }
    Ok(())
}

struct Parser {
    tokens: Vec<(usize, Tok)>,
    pos: usize,
    terms: usize,
    depth: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|(_, t)| t)
    }

    fn at(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.end, |(at, _)| *at)
    }

    fn or(&mut self) -> Result<Cond, ParseError> {
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            left = Cond::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Cond, ParseError> {
        let mut left = self.unary()?;
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            left = Cond::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Cond, ParseError> {
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            return Ok(Cond::Not(Box::new(self.unary()?)));
        }
        self.atom()
    }

    fn term(&mut self, at: usize) -> Result<(), ParseError> {
        self.terms += 1;
        if self.terms > MAX_TERMS {
            return fail(at, format!("more than {MAX_TERMS} terms"));
        }
        Ok(())
    }

    fn regex(at: usize, pattern: &str) -> Result<Regex, ParseError> {
        Regex::new(pattern).or_else(|e| fail(at, format!("bad regex: {e}")))
    }

    fn atom(&mut self) -> Result<Cond, ParseError> {
        let at = self.at();
        let Some((_, tok)) = self.tokens.get(self.pos).cloned() else {
            return fail(at, "expected a term, found the end");
        };
        self.pos += 1;
        match tok {
            Tok::Text(t) => {
                self.term(at)?;
                Ok(Cond::Text(t.to_lowercase()))
            }
            Tok::Regex(r) => {
                self.term(at)?;
                Ok(Cond::Regex(Self::regex(at, &r)?))
            }
            Tok::Line => {
                let re_at = self.at();
                let Some(Tok::Regex(r)) = self.peek().cloned() else {
                    return fail(re_at, "line must be followed by a /regex/");
                };
                self.pos += 1;
                self.term(at)?;
                Ok(Cond::Line(Self::regex(re_at, &r)?))
            }
            Tok::Open => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return fail(at, format!("parentheses nest deeper than {MAX_DEPTH}"));
                }
                let inner = self.or()?;
                if self.peek() != Some(&Tok::Close) {
                    return fail(self.at(), "missing )");
                }
                self.pos += 1;
                self.depth -= 1;
                Ok(inner)
            }
            other => fail(at, format!("expected a term, found {}", describe(&other))),
        }
    }
}

fn describe(t: &Tok) -> &'static str {
    match t {
        Tok::Not => "not",
        Tok::And => "and",
        Tok::Or => "or",
        Tok::Close => ")",
        // Terms and `(` never reach here: `atom` accepts them.
        Tok::Text(_) | Tok::Regex(_) | Tok::Line | Tok::Open => "a term",
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))] // the tests themselves are not product code
mod tests {
    use super::*;

    fn eval(src: &str, text: &str) -> bool {
        Cond::parse(src).unwrap().eval(&Subject::new(text))
    }

    fn err(src: &str) -> ParseError {
        Cond::parse(src).unwrap_err()
    }

    #[test]
    fn text_ignores_case_both_ways() {
        assert!(eval(r#""Esc To Cancel""#, "press ESC to cancel"));
        assert!(!eval(r#""esc to cancel""#, "esc to stop"));
        assert!(eval(r#""Äpfel""#, "ÄPFEL"));
    }

    #[test]
    fn regex_is_case_sensitive_and_spans_lines() {
        assert!(eval("/Working/", "x\nWorking"));
        assert!(!eval("/Working/", "working"));
        assert!(eval("/(?i)working/", "WORKING"));
        assert!(eval(r"/a\nb/", "a\nb"));
    }

    #[test]
    fn line_regex_needs_one_line() {
        assert!(eval("line /^b$/", "a\nb\nc"));
        assert!(!eval("line /^a b$/", "a\nb"));
        assert!(eval(r"line /^x$/", "x\r\ny"), "a trailing CR is not part of the line");
        assert!(!eval("line /./", ""));
    }

    #[test]
    fn operators_short_circuit_and_combine() {
        assert!(eval(r#""a" and "b""#, "ab"));
        assert!(!eval(r#""a" and "b""#, "a"));
        assert!(!eval(r#""z" and "b""#, "b"));
        assert!(eval(r#""a" or "b""#, "b"));
        assert!(eval(r#""a" or "b""#, "a"));
        assert!(!eval(r#""a" or "b""#, "c"));
        assert!(eval(r#"not "a""#, "b"));
        assert!(!eval(r#"not "a""#, "a"));
        assert!(eval(r#"not not "a""#, "a"));
    }

    #[test]
    fn precedence_is_not_then_and_then_or() {
        // a or (b and c)
        assert!(eval(r#""a" or "b" and "c""#, "a"));
        assert!(!eval(r#""a" or "b" and "c""#, "b"));
        // (not a) and b
        assert!(eval(r#"not "a" and "b""#, "b"));
        assert!(!eval(r#"not "a" and "b""#, "ab"));
        // grouping overrides
        assert!(!eval(r#"("a" or "b") and "c""#, "a"));
        assert!(eval(r#"("a" or "b") and "c""#, "bc"));
    }

    #[test]
    fn literals_and_escapes() {
        assert!(eval(r#""say \"hi\"""#, r#"they say "hi""#));
        assert!(eval(r#""back\\slash""#, r"back\slash"));
        assert!(eval(r"/a\/b/", "a/b"));
        assert!(eval(r"/\d+/", "42"));
        assert!(eval(r"/\\/", r"x\y"));
        assert!(eval("\n\t\"a\"\n", "a"), "whitespace and newlines separate tokens");
        assert!(eval(r#"line/a/"#, "a"), "no space needed after line");
    }

    #[test]
    fn every_parse_error() {
        assert_eq!(err("").message, "expected a term, found the end");
        assert_eq!(err(r#""a" and"#).at, 7);
        assert_eq!(err(r#""a"#).message, "unterminated text");
        assert_eq!(err(r#""a\n""#).message, "only \\\" and \\\\ may be escaped in text");
        assert_eq!(err(r#""a\"#).message, "only \\\" and \\\\ may be escaped in text");
        assert_eq!(err("/a").message, "unterminated regex");
        assert_eq!(err(r"/a\").message, "unterminated regex");
        assert_eq!(err(r#""""#).message, "empty text");
        assert_eq!(err("//").message, "empty regex");
        assert!(err("/(/").message.starts_with("bad regex"));
        assert!(err("line /(/").message.starts_with("bad regex"));
        assert_eq!(err("line \"a\"").message, "line must be followed by a /regex/");
        assert_eq!(err("line").message, "line must be followed by a /regex/");
        assert_eq!(
            err(r#"maybe "a""#).message,
            "unknown word \"maybe\" (expected line, not, and, or)"
        );
        assert_eq!(err("&").message, "unexpected character '&'");
        assert_eq!(err(r#"("a""#).message, "missing )");
        assert_eq!(err(r#""a" "b""#).message, "unexpected input after the condition");
        assert_eq!(err(r#""a" )"#).at, 4);
        assert_eq!(err("and").message, "expected a term, found and");
        assert_eq!(err("or").message, "expected a term, found or");
        assert_eq!(err("not not").message, "expected a term, found the end");
        assert_eq!(err("not )").message, "expected a term, found )");
        assert_eq!(err("( not )").message, "expected a term, found )");
        assert_eq!(err(r#""a" and not"#).message, "expected a term, found the end");
        assert_eq!(format!("{}", err("&")), "at byte 0: unexpected character '&'");
    }

    #[test]
    fn limits() {
        let ok = vec![r#""a""#; MAX_TERMS].join(" or ");
        assert!(Cond::parse(&ok).is_ok());
        let too_many = vec![r#""a""#; MAX_TERMS + 1].join(" or ");
        assert_eq!(err(&too_many).message, format!("more than {MAX_TERMS} terms"));
        let regexes = vec!["/a/"; MAX_TERMS + 1].join(" or ");
        assert!(err(&regexes).message.starts_with("more than"));
        let lines = vec!["line /a/"; MAX_TERMS + 1].join(" or ");
        assert!(err(&lines).message.starts_with("more than"));

        let deep = format!("{}\"a\"{}", "(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH));
        assert!(Cond::parse(&deep).is_ok());
        let deeper = format!("{}\"a\"{}", "(".repeat(MAX_DEPTH + 1), ")".repeat(MAX_DEPTH + 1));
        assert!(err(&deeper).message.starts_with("parentheses nest deeper"));
        // Depth is nesting, not the count of groups.
        let siblings = [r#"("a")"#; MAX_DEPTH + 2].join(" and ");
        assert!(Cond::parse(&siblings).is_ok());

        let long = format!("\"{}\"", "x".repeat(MAX_LITERAL));
        assert!(Cond::parse(&long).is_ok());
        let longer = format!("\"{}\"", "é".repeat(MAX_LITERAL + 1));
        assert!(err(&longer).message.contains("the limit is"));
        let long_re = format!("/{}/", "x".repeat(MAX_LITERAL + 1));
        assert!(err(&long_re).message.contains("regex is"));
    }

    #[test]
    fn display_is_canonical_and_reparses() {
        let src = r#"not "A\"b" and (line /x\/y/ or /\d\\/) or not (not "c")"#;
        let c = Cond::parse(src).unwrap();
        let shown = c.to_string();
        assert_eq!(
            shown,
            r#"((not "a\"b") and (line /x\/y/ or /\d\\/)) or (not (not "c"))"#
        );
        assert_eq!(Cond::parse(&shown).unwrap().to_string(), shown);
        assert_eq!(Cond::parse(r#""q\\""#).unwrap().to_string(), r#""q\\""#);
    }

    #[test]
    fn regex_display_escapes_only_bare_slashes() {
        assert_eq!(Cond::Regex(Regex::new(r"a\\/").unwrap()).to_string(), r"/a\\\//");
        // `\/` (built directly; the parser turns it into `/`) is already escaped.
        let c = Cond::Line(Regex::new(r"a\/b").unwrap());
        assert_eq!(c.to_string(), r"line /a\/b/");
        assert!(Cond::parse(&c.to_string()).unwrap().eval(&Subject::new("a/b")));
    }

    #[test]
    fn describe_names_non_terms() {
        assert_eq!(describe(&Tok::Text("a".into())), "a term");
        assert_eq!(describe(&Tok::Regex("a".into())), "a term");
        assert_eq!(describe(&Tok::Line), "a term");
        assert_eq!(describe(&Tok::Open), "a term");
        // `not` is taken by `unary` before `atom` sees it; described all the same.
        assert_eq!(describe(&Tok::Not), "not");
    }
}
