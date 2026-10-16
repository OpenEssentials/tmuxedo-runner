//! Secret masking for chat events (docs/design/agent-chat.md §3.5).
//!
//! Every string in every event goes through [`Masker::value`] **before** it
//! gets a seq and is written to the log, so neither the log on disk, nor a
//! CHAT frame, nor a push body ever holds a raw secret the runner knows of.
//!
//! Two layers:
//!
//! 1. **Literals**: the values of secret-named variables in the chat child's
//!    environment (`ANTHROPIC_API_KEY`, `*_TOKEN`, `*_SECRET`, …; at least 8
//!    characters), and their base64 (standard, URL-safe, with and without
//!    padding) and percent-encoded forms. This is the Phase 14 masker's job
//!    for the values it knows.
//! 2. **Shapes**: tokens whose format says "credential" (provider prefixes
//!    such as `sk-ant-`, `ghp_`, `AKIA…`; JWTs; PEM private keys;
//!    `Authorization:` headers; `user:pass@` in URLs; `NAME=value` where the
//!    name says secret). Unlike the push-preview masker (web
//!    lib/secret-mask.ts), long hex or base64 runs are **not** masked: a chat
//!    shows commit hashes and checksums all the time.
//!
//! What the CLI sends to its own vendor is out of reach (the transcript
//! caveat, secrets-and-env.md §5.3).
//!
//! **Terminal control sequences** in what a tool took and gave back are
//! taken out first ([`strip_controls`], docs/design/chat-tools.md §15): a
//! tool's text is chosen by programs and pages the agent touched, and a
//! colour code in the middle of a secret would otherwise hide it from the
//! masker and leave it whole for whoever strips the code later.

use regex::{Captures, Regex};
use serde_json::Value;
use std::sync::LazyLock;

pub const REDACTED: &str = "***";
/// Shorter values are not masked (they would hit ordinary words).
pub const MIN_LITERAL: usize = 8;

/// A variable name that says its value is a secret.
static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(pass(word|wd|phrase)?|pwd|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|client[_-]?secret|credentials?|auth|cookie|session|dsn|signature)").unwrap()
});

/// Names the pattern catches that never hold a secret: the shell's working
/// folders (`pwd`) and X's cookie FILE path (`auth`). Masking them hid every
/// path under the runner's folder ("***/work/MIGRATION.md").
const NOT_SECRET: &[&str] = &[
    "PWD",
    "OLDPWD",
    "XAUTHORITY",
    "SESSION_MANAGER",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_CLASS",
    "XDG_SESSION_DESKTOP",
];

pub fn is_secret_name(name: &str) -> bool {
    !NOT_SECRET.contains(&name) && SECRET_NAME.is_match(name)
}

#[derive(Debug, Clone, Default)]
pub struct Masker {
    /// The values this chat's child can see (its secret-named environment).
    values: Vec<zeroize::Zeroizing<Vec<u8>>>,
    /// Those values in every encoding (the terminal masker's automaton,
    /// crate::secrets::mask, so both mask the same forms).
    set: std::sync::Arc<crate::secrets::mask::MaskSet>,
    /// Everything else the machine masks (vault deliveries, shell settings).
    pub machine: crate::secrets::Shared,
}

impl Masker {
    /// From name → value pairs: only secret-named values of at least
    /// [`MIN_LITERAL`] characters count.
    pub fn from_env<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Masker {
        let mut m = Masker::default();
        for (k, v) in vars {
            if is_secret_name(k) {
                m.add(v);
            }
        }
        m
    }

    /// Add one secret value (and its encodings).
    pub fn add(&mut self, value: &str) {
        let v = value.trim();
        if v.chars().count() < MIN_LITERAL || self.values.iter().any(|x| x.as_slice() == v.as_bytes()) {
            return;
        }
        self.values.push(zeroize::Zeroizing::new(v.as_bytes().to_vec()));
        self.set = std::sync::Arc::new(crate::secrets::mask::MaskSet::from_values(
            self.values.iter().map(|x| x.as_slice()),
        ));
    }

    pub fn literal_count(&self) -> usize {
        self.set.pattern_count()
    }

    /// Mask one string: this chat's literals, every secret the machine
    /// masks (vault deliveries, shell settings), then shapes.
    pub fn text(&self, s: &str) -> String {
        let out = self.set.redact_str(s, REDACTED);
        let out = self.machine.get().redact_str(&out, REDACTED);
        mask_shapes(&out)
    }

    /// Mask every string in a JSON value (keys of objects too), except the
    /// identifier fields the protocol needs verbatim.
    pub fn value(&self, v: &mut Value) {
        self.walk(v, None, 0);
    }

    fn walk(&self, v: &mut Value, key: Option<&str>, depth: usize) {
        match v {
            Value::String(s) => {
                if key.is_some_and(is_id_key) && PLAIN_ID.is_match(s) {
                    return;
                }
                let m = self.text(s);
                if m != *s {
                    *s = m;
                }
            }
            Value::Array(a) if depth < 64 => {
                for x in a {
                    self.walk(x, key, depth + 1);
                }
            }
            Value::Object(o) if depth < 64 => {
                let keys: Vec<String> = o.keys().cloned().collect();
                for k in keys {
                    let masked = self.text(&k);
                    let mut item = o.remove(&k).unwrap_or(Value::Null);
                    self.walk(&mut item, Some(&k), depth + 1);
                    o.insert(if masked == k { k } else { masked }, item);
                }
            }
            _ => {}
        }
    }
}

/// Text without terminal control sequences: CSI, OSC and the other escape
/// sequences go whole, and so does every control character except tab and
/// newline (a carriage return becomes a line break, so a progress bar reads
/// as lines instead of one glued line). What is left is plain text that no
/// terminal acts on. Characters that change the reading direction or have no
/// width are **kept**: a reader shows them as visible marks, so the line a
/// person reads is the line that ran.
///
/// A string sequence (OSC, DCS, …) that is never closed ends at the end of
/// its line: an unclosed one cannot swallow the rest of the output.
pub fn strip_controls(s: &str) -> std::borrow::Cow<'_, str> {
    let dirty = |c: char| (c.is_control() && c != '\n' && c != '\t') || ('\u{80}'..='\u{9f}').contains(&c);
    if !s.chars().any(dirty) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    // OSC, DCS, SOS, PM, APC: up to BEL or the string terminator, at most to the end of the line.
    fn skip_string(it: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        while let Some(&c) = it.peek() {
            match c {
                '\n' => return,
                '\u{7}' | '\u{9c}' => {
                    it.next();
                    return;
                }
                '\u{1b}' => {
                    it.next();
                    if it.peek() == Some(&'\\') {
                        it.next();
                    }
                    return;
                }
                _ => {
                    it.next();
                }
            }
        }
    }
    // CSI: parameters, intermediates, one final byte.
    fn skip_csi(it: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        while it.next_if(|c| ('\u{30}'..='\u{3f}').contains(c)).is_some() {}
        while it.next_if(|c| ('\u{20}'..='\u{2f}').contains(c)).is_some() {}
        it.next_if(|c| ('\u{40}'..='\u{7e}').contains(c));
    }
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.peek().copied() {
                Some('[') => {
                    it.next();
                    skip_csi(&mut it);
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    it.next();
                    skip_string(&mut it);
                }
                Some(n) if ('\u{20}'..='\u{2f}').contains(&n) => {
                    while it.next_if(|c| ('\u{20}'..='\u{2f}').contains(c)).is_some() {}
                    it.next_if(|c| ('\u{30}'..='\u{7e}').contains(c));
                }
                Some(n) if ('\u{30}'..='\u{7e}').contains(&n) => {
                    it.next();
                }
                _ => {}
            },
            '\u{9b}' => skip_csi(&mut it),
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_string(&mut it),
            '\r' => {
                if it.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\n' | '\t' => out.push(c),
            c if dirty(c) => {}
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// [`strip_controls`] on every string value of a JSON value (not on keys).
pub fn strip_value(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(t) = strip_controls(s) {
                *s = t;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(strip_value),
        Value::Object(o) => o.values_mut().for_each(strip_value),
        _ => {}
    }
}

/// Envelope and enum fields that are identifiers, kept when they look like one.
fn is_id_key(k: &str) -> bool {
    matches!(
        k,
        "type"
            | "at"
            | "sessionId"
            | "turnId"
            | "agentPath"
            | "messageId"
            | "toolId"
            | "id"
            | "kind"
            | "status"
            | "mode"
            | "plugin"
            | "login"
            | "change"
            | "source"
            | "risk"
            | "outcome"
            | "to"
            | "paneId"
            | "clientMsgId"
            | "expiresAt"
            | "code"
            | "resumedFrom"
            | "options"
            | "features"
    )
}

static PLAIN_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._:@/+-]{0,128}$").unwrap());

type Rewrite = fn(&Captures) -> String;
type Rule = (Regex, Rewrite);

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let secret = r"(?:pass(?:word|wd|phrase)?|pwd|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|client[_-]?secret|credentials?|bearer|dsn)";
    let r: Vec<(String, Rewrite)> = vec![
        // PEM private keys (whole block; a cut-off block to the end).
        (
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)".into(),
            |_| REDACTED.into(),
        ),
        // Authorization-style headers.
        (
            r#"(?i)\b(authorization|proxy-authorization|x-api-key|x-auth-token|cookie)(\s*[:=]\s*)(?:(bearer|basic|token)\s+)?("[^"]*"|'[^']*'|[^\s'"]+)"#.into(),
            |c| {
                let scheme = c.get(3).map(|m| format!("{} ", m.as_str())).unwrap_or_default();
                format!("{}{}{scheme}{REDACTED}", &c[1], &c[2])
            },
        ),
        (r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}".into(), |c| format!("{} {REDACTED}", &c[1])),
        // user:password@ in URLs.
        (
            r"(?i)\b([a-z][a-z0-9+.-]*://)([^\s:/@]+):([^\s@/]+)@".into(),
            |c| format!("{}{}:{REDACTED}@", &c[1], &c[2]),
        ),
        // Well-known token formats.
        (
            r"\b(?:sk-(?:ant-|proj-|live-|test-)?[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{16,}|xox[abposr]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,}|(?:tmr|amr)\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}|(?:tmx|amx)_[A-Za-z0-9_-]{8,}|hvs\.[A-Za-z0-9_-]{16,}|npm_[A-Za-z0-9]{30,}|pypi-[A-Za-z0-9_-]{30,}|SG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}|whsec_[A-Za-z0-9]{16,}|rk_(?:live|test)_[A-Za-z0-9]{16,}|sk_(?:live|test)_[A-Za-z0-9]{16,})".into(),
            |_| REDACTED.into(),
        ),
        // JWTs.
        (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}".into(), |_| REDACTED.into()),
        // NAME=value / "name": "value" where the name says secret.
        (
            format!(r#"(?i)([A-Za-z0-9_.-]*{secret}[A-Za-z0-9_.-]*)(["']?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s'",;&|)]+)"#),
            |c| {
                let v = &c[3];
                // Already masked, or a scheme word (handled by the header rules).
                if v.contains(REDACTED) || v.trim_matches(['"', '\'']).is_empty() {
                    return c[0].to_string();
                }
                let lower = v.to_ascii_lowercase();
                if ["bearer", "basic", "token"].contains(&lower.as_str()) {
                    return c[0].to_string();
                }
                let rep = if v.starts_with('"') {
                    format!("\"{REDACTED}\"")
                } else if v.starts_with('\'') {
                    format!("'{REDACTED}'")
                } else {
                    REDACTED.into()
                };
                format!("{}{}{rep}", &c[1], &c[2])
            },
        ),
        // --password x / --api-key x.
        (
            format!(r#"(?i)(--?[A-Za-z0-9-]*{secret}[A-Za-z0-9-]*)(\s+)("[^"]*"|'[^']*'|[^\s'"-][^\s'"]*)"#),
            |c| format!("{}{}{REDACTED}", &c[1], &c[2]),
        ),
    ];
    r.into_iter()
        .map(|(re, f)| (Regex::new(&re).expect("mask rule compiles"), f))
        .collect()
});

/// Shape rules only (no literals).
pub fn mask_shapes(s: &str) -> String {
    let mut out = std::borrow::Cow::Borrowed(s);
    for (re, f) in RULES.iter() {
        if re.is_match(&out) {
            out = std::borrow::Cow::Owned(re.replace_all(&out, |c: &Captures| f(c)).into_owned());
        }
    }
    out.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    const KEY: &str = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";

    #[test]
    fn working_folders_are_not_secrets() {
        let m = Masker::from_env([
            ("PWD", "/home/dev"),
            ("OLDPWD", "/home/dev/work"),
            ("XAUTHORITY", "/home/dev/.Xauthority"),
            ("DB_PWD", "hunter2hunter2"),
        ]);
        assert_eq!(m.text("/home/dev/work/MIGRATION.md"), "/home/dev/work/MIGRATION.md");
        assert_eq!(m.text("pw hunter2hunter2"), "pw ***");
    }

    #[test]
    fn literals_and_their_encodings() {
        let m = Masker::from_env([("ANTHROPIC_API_KEY", "plainvalue-123456"), ("HOME", "/home/someone/x")]);
        let b64 = base64::engine::general_purpose::STANDARD.encode("plainvalue-123456");
        let url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("plainvalue-123456");
        for s in [
            "key=plainvalue-123456!".to_string(),
            format!("echo {b64}"),
            format!("x {url} y"),
            "q=plainvalue%2D123456".replace("%2D", "-"),
        ] {
            let out = m.text(&s);
            assert!(!out.contains("plainvalue-123456"), "{out}");
            assert!(!out.contains(&b64), "{out}");
        }
        // Not secret-named: kept.
        assert_eq!(m.text("/home/someone/x"), "/home/someone/x");
        // Too short to mask.
        let short = Masker::from_env([("TOKEN", "abc")]);
        assert_eq!(short.literal_count(), 0);
    }

    #[test]
    fn shapes() {
        let cases = [
            (format!("export ANTHROPIC_API_KEY={KEY}"), KEY.to_string()),
            (
                "Authorization: Bearer abcdefghijklmnop1234".into(),
                "abcdefghijklmnop1234".into(),
            ),
            (
                "curl https://bob:hunter2hunter2@example.com/x".into(),
                "hunter2hunter2".into(),
            ),
            (
                "token ghp_0123456789abcdefghijABCDEFGHIJ".into(),
                "ghp_0123456789abcdefghij".into(),
            ),
            ("aws AKIAABCDEFGHIJKLMNOP".into(), "AKIAABCDEFGHIJKLMNOP".into()),
            (
                "jwt eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0NTY3.SflKxwRJSMeKKF2QT4fw".into(),
                "SflKxwRJSMeKKF2QT4fw".into(),
            ),
            ("{\"password\": \"correct horse\"}".into(), "correct horse".into()),
            ("mysql --password s3cr3tpass".into(), "s3cr3tpass".into()),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----"
                    .into(),
                "b3BlbnNzaC1rZXktdjEAAAAA".into(),
            ),
        ];
        for (input, secret) in cases {
            let out = mask_shapes(&input);
            assert!(!out.contains(&secret), "{input:?} → {out:?}");
            assert!(out.contains(REDACTED), "{out}");
        }
        // Ordinary chat text and hashes survive.
        for keep in [
            "commit 3f2a9c1d4e5b6a7980f1e2d3c4b5a69788776655 fixed it",
            "sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "The password field is required.",
            "Run `npm test` in ~/src/shop",
            "token budget: 1200",
        ] {
            assert_eq!(mask_shapes(keep), keep);
        }
    }

    #[test]
    fn control_sequences_are_taken_out() {
        let c = |s: &str| strip_controls(s).into_owned();
        // Plain text is not copied.
        assert!(matches!(
            strip_controls("a\tb\nc é 日本"),
            std::borrow::Cow::Borrowed(_)
        ));
        // Colours, cursor moves, erases, private modes.
        assert_eq!(
            c("\u{1b}[31mred\u{1b}[0m \u{1b}[2J\u{1b}[H\u{1b}[?25lx\u{1b}[1;2Hy"),
            "red xy"
        );
        // A window title and a hyperlink (OSC), closed by BEL or by ST.
        assert_eq!(c("a\u{1b}]0;✓ done\u{7}b"), "ab");
        assert_eq!(
            c("\u{1b}]8;;https://evil.example\u{1b}\\click\u{1b}]8;;\u{1b}\\ here"),
            "click here"
        );
        // DCS, APC, PM, SOS.
        assert_eq!(
            c("a\u{1b}Pq#0;2;0;0;0\u{1b}\\b\u{1b}_x\u{7}c\u{1b}^y\u{7}d\u{1b}Xz\u{7}e"),
            "abcde"
        );
        // An unclosed string sequence ends with its line.
        assert_eq!(c("a\u{1b}]0;never closed\nnext line"), "a\nnext line");
        // Charset selection, save and restore, reset, a lone escape at the end.
        assert_eq!(c("\u{1b}(Ba\u{1b}7b\u{1b}8c\u{1b}cd\u{1b}"), "abcd");
        // The 8-bit forms.
        assert_eq!(c("a\u{9b}31mb\u{9d}0;t\u{9c}c\u{85}d"), "abcd");
        // Carriage returns: a Windows line end is a line end, an overwrite is a new line.
        assert_eq!(c("one\r\ntwo\r 50%\r100%\n"), "one\ntwo\n 50%\n100%\n");
        // Bell, backspace, NUL, DEL go; tab and newline stay.
        assert_eq!(c("a\u{7}b\u{8}c\u{0}d\u{7f}e\tf\ng"), "abcde\tf\ng");
        // Direction and zero-width characters are the reader's to mark: kept.
        assert_eq!(c("\u{1b}[0ma\u{202e}b\u{200b}c"), "a\u{202e}b\u{200b}c");
        // Never a panic, whatever the bytes.
        for junk in [
            "\u{1b}[",
            "\u{1b}[1;",
            "\u{1b}]",
            "\u{1b}P",
            "\u{9b}",
            "\u{1b}\u{1b}[",
            "\u{1b} ",
        ] {
            assert_eq!(c(junk), "", "{junk:?}");
        }
        let mut v =
            json!({ "output": "\u{1b}[1mbold\u{1b}[0m", "input": { "command": ["a\u{7}"], "n": 3 }, "k\u{7}": "x" });
        strip_value(&mut v);
        assert_eq!(
            v,
            json!({ "output": "bold", "input": { "command": ["a"], "n": 3 }, "k\u{7}": "x" })
        );
    }

    /// Stripping comes first: a secret cut in two by a colour code is whole
    /// again once the code is gone, and the masker must see it whole.
    #[test]
    fn a_secret_split_by_a_control_sequence_is_still_masked() {
        let m = Masker::from_env([("MY_TOKEN", "tok-0123456789")]);
        let b64 = base64::engine::general_purpose::STANDARD.encode("tok-0123456789");
        let (a, b) = b64.split_at(6);
        for hidden in [
            "tok-01\u{1b}[31m23456789".to_string(),
            "tok-\u{1b}]0;x\u{7}0123456789".to_string(),
            "tok-01234\u{8}56789".to_string(),
            format!("{a}\u{1b}[0m{b}"),
            format!("sk-ant-\u{1b}[1mapi03-{}", &KEY[13..]),
        ] {
            // Masked as it came, the secret survives for whoever strips the code afterwards.
            let late = strip_controls(&m.text(&hidden)).into_owned();
            assert!(
                late.contains("tok-0123456789") || late.contains(&b64) || late.contains(KEY),
                "the case proves nothing: {hidden:?}"
            );
            let out = m.text(&strip_controls(&hidden));
            assert!(out.contains(REDACTED), "{out}");
            for raw in ["tok-0123456789", b64.as_str(), KEY] {
                assert!(!out.contains(raw), "{out}");
            }
        }
    }

    #[test]
    fn json_values_keep_ids_and_mask_the_rest() {
        let m = Masker::from_env([("MY_TOKEN", "tok-0123456789")]);
        let mut v = json!({
            "type": "tool.done", "sessionId": "c-0123456789", "toolId": "toolu_01ABCdef",
            "output": "got tok-0123456789 and more",
            "input": {"command": "echo tok-0123456789", "tok-0123456789": 1},
            "list": ["x tok-0123456789"],
        });
        m.value(&mut v);
        let s = v.to_string();
        assert!(!s.contains("tok-0123456789"), "{s}");
        assert_eq!(v["toolId"], "toolu_01ABCdef");
        assert_eq!(v["sessionId"], "c-0123456789");
        assert_eq!(v["output"], "got *** and more");
        assert!(v["input"].get("***").is_some());
    }
}
