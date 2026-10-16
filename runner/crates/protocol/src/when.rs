//! Plugin visibility conditions (`when`, docs/design/plugins.md §6.1) and
//! `{path}` text: the Rust twin of packages/protocol/src/when.ts. Both are
//! tested on packages/protocol/fixtures/when.json, so they cannot drift.
//!
//! ```text
//! expr  := or
//! or    := and (("or" | "||") and)*
//! and   := not (("and" | "&&") not)*
//! not   := ("not" | "!") not | cmp
//! cmp   := atom (("==" | "!=") atom | "not"? "in" list)?
//! atom  := string | number | true | false | null | path | "(" expr ")"
//! list  := "[" (literal ("," literal)*)? "]"
//! path  := ident ("." ident)*        ident = [a-z_][a-z0-9_]*
//! ```
//!
//! A missing path is null; a bare atom is true unless null, false, "" or 0;
//! comparisons are strict. Parsing reports junk; evaluation never panics.

use serde_json::Value;

pub const MAX_LEN: usize = 512;
pub const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Lit(Lit),
    Path(Vec<String>),
    Not(Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Eq(Box<Node>, Box<Node>),
    Ne(Box<Node>, Box<Node>),
    In(Box<Node>, Vec<Lit>, bool),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Num(f64),
    Id(String),
    Op(&'static str),
}

fn ident_start(c: u8) -> bool {
    c.is_ascii_lowercase() || c == b'_'
}
fn ident_char(c: u8) -> bool {
    ident_start(c) || c.is_ascii_digit()
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b = src.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' => i += 1,
            b'\'' | b'"' => {
                let mut s = Vec::new();
                let mut j = i + 1;
                while j < b.len() && b[j] != c {
                    if b[j] == b'\\' && j + 1 < b.len() {
                        s.push(b[j + 1]);
                        j += 2;
                        continue;
                    }
                    s.push(b[j]);
                    j += 1;
                }
                if j >= b.len() {
                    return Err("unterminated string".into());
                }
                out.push(Tok::Str(String::from_utf8_lossy(&s).into_owned()));
                i = j + 1;
            }
            _ if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j + 1 < b.len() && b[j] == b'.' && b[j + 1].is_ascii_digit() {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                let n: f64 = src[i..j].parse().map_err(|_| "bad number".to_string())?;
                out.push(Tok::Num(n));
                i = j;
            }
            _ if ident_start(c) => {
                let mut j = i;
                loop {
                    while j < b.len() && ident_char(b[j]) {
                        j += 1;
                    }
                    if j + 1 < b.len() && b[j] == b'.' && ident_start(b[j + 1]) {
                        j += 1;
                        continue;
                    }
                    break;
                }
                out.push(Tok::Id(src[i..j].to_string()));
                i = j;
            }
            _ => {
                let two = src.get(i..i + 2).unwrap_or("");
                let op: &'static str = match two {
                    "==" => "==",
                    "!=" => "!=",
                    "&&" => "&&",
                    "||" => "||",
                    _ => match c {
                        b'!' => "!",
                        b'(' => "(",
                        b')' => ")",
                        b'[' => "[",
                        b']' => "]",
                        b',' => ",",
                        _ => return Err(format!("unexpected {:?}", src[i..].chars().next().unwrap_or('?'))),
                    },
                };
                out.push(Tok::Op(op));
                i += op.len();
            }
        }
    }
    Ok(out)
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

const RESERVED: &[&str] = &["true", "false", "null", "and", "or", "not", "in"];

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }
    fn is_op(&self, v: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(o)) if *o == v)
    }
    fn is_word(&self, v: &str) -> bool {
        matches!(self.peek(), Some(Tok::Id(w)) if w == v)
    }
    fn expect(&mut self, v: &str) -> Result<(), String> {
        if !self.is_op(v) {
            return Err(format!("expected {v}"));
        }
        self.i += 1;
        Ok(())
    }
    fn expr(&mut self, d: usize) -> Result<Node, String> {
        if d > MAX_DEPTH {
            return Err("too deeply nested".into());
        }
        let mut a = self.and(d)?;
        while self.is_word("or") || self.is_op("||") {
            self.i += 1;
            a = Node::Or(Box::new(a), Box::new(self.and(d)?));
        }
        Ok(a)
    }
    fn and(&mut self, d: usize) -> Result<Node, String> {
        let mut a = self.not(d)?;
        while self.is_word("and") || self.is_op("&&") {
            self.i += 1;
            a = Node::And(Box::new(a), Box::new(self.not(d)?));
        }
        Ok(a)
    }
    fn not(&mut self, d: usize) -> Result<Node, String> {
        if self.is_word("not") || self.is_op("!") {
            if d + 1 > MAX_DEPTH {
                return Err("too deeply nested".into());
            }
            self.i += 1;
            return Ok(Node::Not(Box::new(self.not(d + 1)?)));
        }
        self.cmp(d)
    }
    fn cmp(&mut self, d: usize) -> Result<Node, String> {
        let a = self.atom(d)?;
        if self.is_op("==") || self.is_op("!=") {
            let eq = self.is_op("==");
            self.i += 1;
            let b = self.atom(d)?;
            return Ok(if eq {
                Node::Eq(Box::new(a), Box::new(b))
            } else {
                Node::Ne(Box::new(a), Box::new(b))
            });
        }
        if self.is_word("in") {
            self.i += 1;
            return Ok(Node::In(Box::new(a), self.list()?, false));
        }
        if self.is_word("not") && matches!(self.t.get(self.i + 1), Some(Tok::Id(w)) if w == "in") {
            self.i += 2;
            return Ok(Node::In(Box::new(a), self.list()?, true));
        }
        Ok(a)
    }
    fn literal(&mut self) -> Result<Lit, String> {
        let l = match self.peek() {
            None => return Err("unexpected end".into()),
            Some(Tok::Str(s)) => Lit::Str(s.clone()),
            Some(Tok::Num(n)) => Lit::Num(*n),
            Some(Tok::Id(w)) if w == "true" => Lit::Bool(true),
            Some(Tok::Id(w)) if w == "false" => Lit::Bool(false),
            Some(Tok::Id(w)) if w == "null" => Lit::Null,
            _ => return Err("expected a literal".into()),
        };
        self.i += 1;
        Ok(l)
    }
    fn list(&mut self) -> Result<Vec<Lit>, String> {
        self.expect("[")?;
        let mut out = vec![];
        if self.is_op("]") {
            self.i += 1;
            return Ok(out);
        }
        loop {
            out.push(self.literal()?);
            if self.is_op("]") {
                self.i += 1;
                return Ok(out);
            }
            self.expect(",")?;
        }
    }
    fn atom(&mut self, d: usize) -> Result<Node, String> {
        match self.peek() {
            None => Err("unexpected end".into()),
            Some(Tok::Op("(")) => {
                self.i += 1;
                let e = self.expr(d + 1)?;
                self.expect(")")?;
                Ok(e)
            }
            Some(Tok::Id(w)) if !RESERVED.contains(&w.as_str()) => {
                let p = w.split('.').map(String::from).collect();
                self.i += 1;
                Ok(Node::Path(p))
            }
            _ => Ok(Node::Lit(self.literal()?)),
        }
    }
}

/// Parse a condition, or say why not.
pub fn parse(src: &str) -> Result<Node, String> {
    if src.len() > MAX_LEN {
        return Err("too long".into());
    }
    let t = lex(src)?;
    if t.is_empty() {
        return Err("empty".into());
    }
    let mut p = P { t, i: 0 };
    let n = p.expr(0)?;
    if p.i < p.t.len() {
        return Err("trailing input".into());
    }
    Ok(n)
}

/// The context value at a path (missing, object or array → null).
pub fn lookup(ctx: &Value, path: &[String]) -> Lit {
    let mut v = ctx;
    for k in path {
        match v.as_object().and_then(|o| o.get(k)) {
            Some(x) => v = x,
            None => return Lit::Null,
        }
    }
    match v {
        Value::String(s) => Lit::Str(s.clone()),
        Value::Number(n) => n.as_f64().map_or(Lit::Null, Lit::Num),
        Value::Bool(b) => Lit::Bool(*b),
        _ => Lit::Null,
    }
}

fn truthy(v: &Lit) -> bool {
    !matches!(v, Lit::Null | Lit::Bool(false)) && *v != Lit::Str(String::new()) && *v != Lit::Num(0.0)
}

fn value(n: &Node, ctx: &Value) -> Lit {
    match n {
        Node::Lit(l) => l.clone(),
        Node::Path(p) => lookup(ctx, p),
        other => Lit::Bool(test(other, ctx)),
    }
}

/// Evaluate a parsed condition.
pub fn test(n: &Node, ctx: &Value) -> bool {
    match n {
        Node::Lit(_) | Node::Path(_) => truthy(&value(n, ctx)),
        Node::Not(a) => !test(a, ctx),
        Node::And(a, b) => test(a, ctx) && test(b, ctx),
        Node::Or(a, b) => test(a, ctx) || test(b, ctx),
        Node::Eq(a, b) => value(a, ctx) == value(b, ctx),
        Node::Ne(a, b) => value(a, ctx) != value(b, ctx),
        Node::In(a, list, neg) => {
            let v = value(a, ctx);
            list.contains(&v) != *neg
        }
    }
}

/// No condition is true; one that does not parse is false (never shown).
pub fn eval(src: Option<&str>, ctx: &Value) -> bool {
    match src {
        None | Some("") => true,
        Some(s) => parse(s).is_ok_and(|n| test(&n, ctx)),
    }
}

/// Fill `{a.b}` placeholders from the context ("" when missing).
pub fn render_text(text: &str, ctx: &Value) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let path_ok = |p: &str| {
            !p.is_empty()
                && p.split('.').all(|seg| {
                    let b = seg.as_bytes();
                    !b.is_empty() && ident_start(b[0]) && b.iter().all(|c| ident_char(*c))
                })
        };
        match after.find('}') {
            Some(close) if path_ok(&after[..close]) => {
                let path: Vec<String> = after[..close].split('.').map(String::from).collect();
                match lookup(ctx, &path) {
                    Lit::Str(s) => out.push_str(&s),
                    Lit::Num(n) => out.push_str(&num_text(n)),
                    Lit::Bool(b) => out.push_str(if b { "true" } else { "false" }),
                    Lit::Null => {}
                }
                rest = &after[close + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A number as JavaScript's `String(n)` writes the common cases (3, 3.5, -1).
fn num_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/protocol/fixtures/when.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn shared_fixtures_cases() {
        let f = fx();
        let cases = f["cases"].as_array().unwrap();
        assert!(cases.len() >= 30);
        for c in cases {
            let e = c["expr"].as_str().unwrap();
            assert!(parse(e).is_ok(), "{e}: {:?}", parse(e));
            assert_eq!(eval(Some(e), &f["ctx"]), c["want"].as_bool().unwrap(), "{e}");
        }
    }

    #[test]
    fn shared_fixtures_invalid_and_limits() {
        let f = fx();
        for e in f["invalid"].as_array().unwrap() {
            let e = e.as_str().unwrap();
            assert!(parse(e).is_err(), "{e} parsed");
            assert!(!eval(Some(e), &f["ctx"]), "{e}");
        }
        for e in f["empty_is_true"].as_array().unwrap() {
            assert!(eval(e.as_str(), &f["ctx"]));
        }
        let n = f["too_long"].as_u64().unwrap() as usize;
        assert_eq!(n, MAX_LEN + 1);
        assert!(parse(&"x".repeat(n)).is_err());
        assert!(parse(&"x".repeat(MAX_LEN)).is_ok());
    }

    #[test]
    fn shared_fixtures_text() {
        let f = fx();
        for c in f["text"].as_array().unwrap() {
            assert_eq!(
                render_text(c["text"].as_str().unwrap(), &f["ctx"]),
                c["want"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn junk_contexts_never_panic() {
        for ctx in [
            Value::Null,
            serde_json::json!(3),
            serde_json::json!([]),
            serde_json::json!({"auth": []}),
            serde_json::json!({"auth": {"state": {"deep": 1}}}),
        ] {
            assert!(!eval(Some("auth.state == 'ok'"), &ctx));
            assert!(eval(Some("auth.state == null"), &ctx));
            assert_eq!(render_text("{auth.state}", &ctx), "");
        }
        assert_eq!(render_text("a { b {c", &Value::Null), "a { b {c");
        assert_eq!(render_text("{é}", &Value::Null), "{é}");
    }
}
