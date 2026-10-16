//! Login callback relay (docs/protocol.md "Login callback relay", handoff
//! work queue 3b).
//!
//! A CLI's browser login (Claude Code's `/login`, `codex login`) starts an
//! HTTP server on `localhost:<port>` and opens an OAuth authorize URL whose
//! `redirect_uri` points at it. In a pane the URL reaches the user's device
//! through the opener stand-ins (`open_url`), but the device cannot reach the
//! pane's `localhost`, so the redirect fails and the browser is left on
//! `http://localhost:<port>/callback?code=…&state=…`. The runner shares the
//! pane's network namespace, so it can deliver that address for the user:
//!
//! 1. [`detect`]: an `open_url` whose query has a loopback `redirect_uri`
//!    and a `state` is marked with `callback {host, port, path, state}` and
//!    remembered for the pane ([`Relay::record`]) for [`PENDING_TTL`].
//! 2. `login_callback {paneId, url}` (gateway frame): the pasted URL must
//!    match a remembered callback's scheme, host, port and path, carry its
//!    `state`, the port must be listening here, and the pane must be under
//!    its rate limit ([`Relay::check`]). The callback is then spent
//!    (single use) and [`deliver`] makes ONE `GET` to the loopback port, no
//!    redirects followed, [`GET_TIMEOUT`] in all.
//!
//! The URL's `code` is a credential: it is never logged or kept, only
//! forwarded once to the port that asked for it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

/// How long a callback from an `open_url` can be relayed.
pub const PENDING_TTL: Duration = Duration::from_secs(15 * 60);
/// The whole relayed request (connect, send, read) must finish in this.
pub const GET_TIMEOUT: Duration = Duration::from_secs(10);
/// At most this many `login_callback` attempts per pane per [`RATE_WINDOW`].
pub const RATE_MAX: usize = 6;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Callbacks remembered per pane (the newest win).
const MAX_PER_PANE: usize = 4;
const MAX_STATE: usize = 512;
const MAX_PATH: usize = 1024;
/// Response bytes read from the callback server (status line, headers, an excerpt).
const MAX_READ: usize = 64 * 1024;
const EXCERPT_CHARS: usize = 200;

/// A loopback OAuth redirect target, from an authorize URL's own query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Callback {
    /// `localhost` or `127.0.0.1` (lowercase), exactly as in `redirect_uri`.
    pub host: String,
    pub port: u16,
    /// The redirect path (no query), e.g. `/callback`, `/auth/callback`.
    pub path: String,
    /// The authorize URL's `state`; the pasted URL must carry the same.
    pub state: String,
}

impl Callback {
    pub fn to_json(&self) -> Value {
        json!({ "host": self.host, "port": self.port, "path": self.path, "state": self.state })
    }
}

/// Why a `login_callback` is refused (the `res` error code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not an http URL, or junk.
    InvalidUrl,
    /// Nothing to deliver for this pane (no login URL opened in the last 15 minutes).
    NoPending,
    /// Scheme, host, port or path differ from every callback of this pane.
    Mismatch,
    /// Right place, wrong (or no) `state`.
    StateMismatch,
    /// The callback is older than [`PENDING_TTL`].
    Expired,
    /// The callback was already delivered once.
    AlreadyUsed,
    /// Nothing listens on the callback port here (the CLI gave up or exited).
    NotListening,
    /// Too many attempts for this pane.
    RateLimited,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::InvalidUrl => "invalid_url",
            Refusal::NoPending => "no_pending",
            Refusal::Mismatch => "mismatch",
            Refusal::StateMismatch => "state_mismatch",
            Refusal::Expired => "expired",
            Refusal::AlreadyUsed => "already_used",
            Refusal::NotListening => "not_listening",
            Refusal::RateLimited => "rate_limited",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Refusal::InvalidUrl => "That is not the address of a local login callback.",
            Refusal::NoPending => {
                "No login is waiting in this pane (open the login link first; links last 15 minutes)."
            }
            Refusal::Mismatch => {
                "That address is not the one this pane's login is waiting on (host, port or path differ)."
            }
            Refusal::StateMismatch => "That address belongs to a different login attempt (its state does not match).",
            Refusal::Expired => "That login link is older than 15 minutes; start the login again.",
            Refusal::AlreadyUsed => "That login callback was already delivered.",
            Refusal::NotListening => "The login in this pane is no longer waiting (nothing listens on its port).",
            Refusal::RateLimited => "Too many attempts for this pane; wait a minute.",
        }
    }
}

// ── URL pieces (the same rules as packages/protocol/src/login-callback.ts) ──

/// `%XX` and `+` (form encoding) decoded; None for broken escapes or non-UTF-8.
fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The first value of `name` in a raw query (decoded).
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (decode(k)? == name).then(|| decode(v)).flatten()
    })
}

/// A URL split into its parts. Only what the relay needs; no normalisation
/// beyond lowercasing the scheme and host.
#[derive(Debug, PartialEq, Eq)]
struct Parts<'a> {
    scheme: String,
    host: String,
    /// The explicit port, or the scheme's default.
    port: u16,
    path: &'a str,
    /// Raw (still encoded), without `?`; "" when absent.
    query: &'a str,
}

fn split(url: &str) -> Option<Parts<'_>> {
    if url.is_empty()
        || url.len() > crate::open_url::MAX_URL
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let rest = rest.split('#').next().unwrap_or("");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') || h.ends_with(']') => {
            if p.is_empty() || p.len() > 5 || !p.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let n: u32 = p.parse().ok()?;
            if n == 0 || n > 65535 {
                return None;
            }
            (h, n as u16)
        }
        _ => (authority, default),
    };
    if host.is_empty() {
        return None;
    }
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, q),
        None => (tail, ""),
    };
    Some(Parts {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
        path: if path.is_empty() { "/" } else { path },
        query,
    })
}

fn loopback_host(h: &str) -> bool {
    h == "localhost" || h == "127.0.0.1"
}

/// The loopback callback an OAuth/OIDC authorize URL asks for, or None.
///
/// The URL must carry `redirect_uri` = `http://localhost:<port>/…` or
/// `http://127.0.0.1:<port>/…` (an explicit port; no userinfo), a non-empty
/// `state`, and `response_type` or `client_id` (so a random link that merely
/// mentions a localhost address is not taken for a login).
pub fn detect(url: &str) -> Option<Callback> {
    let u = split(url)?;
    let redirect = query_param(u.query, "redirect_uri")?;
    let state = query_param(u.query, "state").filter(|s| !s.is_empty() && s.len() <= MAX_STATE)?;
    if query_param(u.query, "response_type").is_none() && query_param(u.query, "client_id").is_none() {
        return None;
    }
    // An explicit port: `http://localhost/cb` would mean port 80, never a CLI's.
    let authority = redirect.split_once("://")?.1.split(['/', '?', '#']).next()?;
    if !authority.contains(':') {
        return None;
    }
    let r = split(&redirect)?;
    if r.scheme != "http" || !loopback_host(&r.host) || !r.path.starts_with('/') || r.path.len() > MAX_PATH {
        return None;
    }
    Some(Callback {
        host: r.host,
        port: r.port,
        path: r.path.to_string(),
        state,
    })
}

/// Does `pasted` (the browser's final address) belong to `cb`? Ok(raw query).
pub fn matches<'a>(cb: &Callback, pasted: &'a str) -> Result<&'a str, Refusal> {
    let u = split(pasted).ok_or(Refusal::InvalidUrl)?;
    if u.scheme != "http" || u.host != cb.host || u.port != cb.port || u.path != cb.path {
        return Err(Refusal::Mismatch);
    }
    match query_param(u.query, "state") {
        Some(s) if s == cb.state => Ok(u.query),
        _ => Err(Refusal::StateMismatch),
    }
}

// ── the pending set ─────────────────────────────────────────────────────

struct Pending {
    cb: Callback,
    at: Instant,
    used: bool,
}

/// What may be delivered: the callback (for the Host header) and the raw query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub callback: Callback,
    pub query: String,
}

/// Callbacks recorded from `open_url`, per pane, and per-pane attempt times.
#[derive(Default)]
pub struct Relay {
    pending: HashMap<String, Vec<Pending>>,
    attempts: HashMap<String, Vec<Instant>>,
}

impl Relay {
    /// Remember `cb` for `pane` (the same callback again refreshes it).
    pub fn record(&mut self, pane: &str, cb: Callback, now: Instant) {
        self.prune(now);
        let list = self.pending.entry(pane.to_string()).or_default();
        list.retain(|p| p.cb != cb);
        list.push(Pending {
            cb,
            at: now,
            used: false,
        });
        if list.len() > MAX_PER_PANE {
            list.remove(0);
        }
    }

    /// Validate a pasted URL against `pane`'s callbacks and spend the match.
    /// `listening(port)` says whether the port is open here (checked last,
    /// after everything that needs no system call).
    pub fn check(
        &mut self,
        pane: &str,
        pasted: &str,
        now: Instant,
        listening: impl FnOnce(u16) -> bool,
    ) -> Result<Accepted, Refusal> {
        let tries = self.attempts.entry(pane.to_string()).or_default();
        tries.retain(|t| now.saturating_duration_since(*t) < RATE_WINDOW);
        if tries.len() >= RATE_MAX {
            return Err(Refusal::RateLimited);
        }
        tries.push(now);
        let u = split(pasted).ok_or(Refusal::InvalidUrl)?;
        if u.scheme != "http" || !loopback_host(&u.host) {
            return Err(Refusal::InvalidUrl);
        }
        let list = self
            .pending
            .get_mut(pane)
            .filter(|l| !l.is_empty())
            .ok_or(Refusal::NoPending)?;
        // Same place first (scheme, host, port, path), then the state.
        let mut place = false;
        let mut hit = None;
        for (i, p) in list.iter().enumerate() {
            match matches(&p.cb, pasted) {
                Ok(_) => hit = Some(i),
                Err(Refusal::StateMismatch) => place = true,
                Err(_) => {}
            }
        }
        let Some(i) = hit else {
            return Err(if place {
                Refusal::StateMismatch
            } else {
                Refusal::Mismatch
            });
        };
        let p = &list[i];
        if p.used {
            return Err(Refusal::AlreadyUsed);
        }
        if now.saturating_duration_since(p.at) >= PENDING_TTL {
            return Err(Refusal::Expired);
        }
        if !listening(p.cb.port) {
            return Err(Refusal::NotListening);
        }
        list[i].used = true;
        Ok(Accepted {
            callback: list[i].cb.clone(),
            query: u.query.to_string(),
        })
    }

    /// Forget a pane (closed).
    pub fn forget(&mut self, pane: &str) {
        self.pending.remove(pane);
        self.attempts.remove(pane);
    }

    /// Drop callbacks (and spent ones' tombstones) past their life.
    fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, l| {
            l.retain(|p| now.saturating_duration_since(p.at) < PENDING_TTL * 2);
            !l.is_empty()
        });
        self.attempts
            .retain(|_, t| t.iter().any(|t| now.saturating_duration_since(*t) < RATE_WINDOW));
    }
}

// ── is the port listening; the one GET ─────────────────────────────────

/// Where a listener for `port` answers on this machine: 127.0.0.1 when an
/// IPv4 socket listens on 127.0.0.1 or 0.0.0.0, else ::1 when an IPv6 one
/// listens on ::1 or :: (Node's `localhost` may bind either). Linux reads
/// `/proc/net/tcp{,6}`; elsewhere a connect probe to 127.0.0.1 then ::1.
pub fn listening_addr(port: u16) -> Option<SocketAddr> {
    if cfg!(target_os = "linux") {
        if let (Ok(v4), v6) = (
            std::fs::read_to_string("/proc/net/tcp"),
            std::fs::read_to_string("/proc/net/tcp6").unwrap_or_default(),
        ) {
            return listening_in(&v4, &v6, port);
        }
    }
    [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)]
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .find(|a| std::net::TcpStream::connect_timeout(a, Duration::from_millis(300)).is_ok())
}

/// [`listening_addr`] over the text of `/proc/net/tcp` and `/proc/net/tcp6`.
pub fn listening_in(tcp: &str, tcp6: &str, port: u16) -> Option<SocketAddr> {
    // Rows: `sl local_address rem_address st …`; addresses are hex, `ADDR:PORT`; st 0A = LISTEN.
    let rows = |text: &str| -> Vec<String> {
        text.lines()
            .skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                let (addr, p) = f.get(1)?.split_once(':')?;
                (f.get(3) == Some(&"0A") && u16::from_str_radix(p, 16).ok()? == port).then(|| addr.to_ascii_uppercase())
            })
            .collect()
    };
    // 127.0.0.1 little-endian; 0.0.0.0. IPv6 ::1 and :: in /proc's word order.
    if rows(tcp).iter().any(|a| a == "0100007F" || a == "00000000") {
        return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    let v6 = rows(tcp6);
    if v6
        .iter()
        .any(|a| a == "00000000000000000000000001000000" || a == "00000000000000000000000000000000")
    {
        return Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port));
    }
    // An IPv4-mapped 127.0.0.1 / 0.0.0.0 listener on a dual-stack socket.
    if v6.iter().any(|a| a == "0000000000000000FFFF00000100007F") {
        return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    None
}

/// What the callback server answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub status: u16,
    /// A short, masked excerpt of the body (errors only).
    pub excerpt: Option<String>,
}

impl Delivered {
    /// 2xx and 3xx: the CLI took the code (both Claude and Codex redirect to a success page).
    pub fn ok(&self) -> bool {
        (200..400).contains(&self.status)
    }
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "ok": self.ok(), "status": self.status });
        if let Some(e) = &self.excerpt {
            v["excerpt"] = json!(e);
        }
        v
    }
}

/// One `GET <path>?<query>` to `addr`, `Host: <host>:<port>` (what the
/// browser would have sent), `Connection: close`, no redirects followed,
/// [`GET_TIMEOUT`] for everything. Err: a short reason.
pub async fn deliver(addr: SocketAddr, a: &Accepted) -> Result<Delivered, String> {
    match tokio::time::timeout(GET_TIMEOUT, get(addr, a)).await {
        Ok(r) => r,
        Err(_) => Err("timeout".into()),
    }
}

async fn get(addr: SocketAddr, a: &Accepted) -> Result<Delivered, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "not_listening".to_string(),
        _ => format!("connect: {e}"),
    })?;
    let target = if a.query.is_empty() {
        a.callback.path.clone()
    } else {
        format!("{}?{}", a.callback.path, a.query)
    };
    let req = format!(
        "GET {target} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: tmuxedo-runner/{}\r\nAccept: text/html,*/*;q=0.8\r\nConnection: close\r\n\r\n",
        a.callback.host,
        a.callback.port,
        crate::VERSION
    );
    s.write_all(req.as_bytes()).await.map_err(|e| format!("send: {e}"))?;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).await.map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() >= MAX_READ {
            break;
        }
        // Enough to answer once the headers and an excerpt's worth of body are in.
        if let Some(h) = find(&buf, b"\r\n\r\n") {
            if buf.len() >= h + 4 + 2048 {
                break;
            }
        }
    }
    parse_response(&buf, &[a.query.as_str(), a.callback.state.as_str()])
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Status and (for errors) a masked excerpt of an HTTP/1.x response.
pub fn parse_response(buf: &[u8], secrets: &[&str]) -> Result<Delivered, String> {
    let line_end = find(buf, b"\r\n").ok_or("no HTTP response")?;
    let line = std::str::from_utf8(&buf[..line_end]).map_err(|_| "no HTTP response")?;
    let mut parts = line.split_whitespace();
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err("no HTTP response".into());
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .filter(|s| (100..600).contains(s))
        .ok_or("no HTTP status")?;
    let excerpt = (status >= 400).then(|| {
        let body = find(buf, b"\r\n\r\n").map(|h| &buf[h + 4..]).unwrap_or(&[]);
        let head = String::from_utf8_lossy(&buf[..find(buf, b"\r\n\r\n").unwrap_or(0)]).to_ascii_lowercase();
        let body = if head.contains("transfer-encoding: chunked") {
            dechunk(body)
        } else {
            body.to_vec()
        };
        mask_excerpt(&String::from_utf8_lossy(&body), secrets)
    });
    Ok(Delivered { status, excerpt })
}

/// Best effort: the chunk payloads that arrived.
fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    while let Some(e) = find(b, b"\r\n") {
        let size = std::str::from_utf8(&b[..e])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok());
        let Some(n) = size.filter(|n| *n > 0) else { break };
        let data = &b[e + 2..];
        out.extend_from_slice(&data[..n.min(data.len())]);
        if data.len() < n + 2 {
            break;
        }
        b = &data[n + 2..];
    }
    out
}

/// Tags stripped, whitespace collapsed, the request's own values and
/// anything secret-shaped masked, cut to [`EXCERPT_CHARS`].
pub fn mask_excerpt(body: &str, secrets: &[&str]) -> String {
    use std::sync::OnceLock;
    static TAGS: OnceLock<regex::Regex> = OnceLock::new();
    static SECRETY: OnceLock<regex::Regex> = OnceLock::new();
    static OPAQUE: OnceLock<regex::Regex> = OnceLock::new();
    let tags = TAGS.get_or_init(|| regex::Regex::new(r"(?s)<(script|style)\b.*?</(script|style)>|<[^>]*>").unwrap());
    let secrety = SECRETY.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(code|state|token|access_token|refresh_token|id_token|secret|password|key)(\s*[=:]\s*)[^\s&,;]+",
        )
        .unwrap()
    });
    let opaque = OPAQUE.get_or_init(|| regex::Regex::new(r"[A-Za-z0-9_\-.~+/=]{20,}").unwrap());
    let mut t = tags.replace_all(body, " ").into_owned();
    for s in secrets.iter().filter(|s| s.len() >= 4) {
        t = t.replace(*s, "***");
        for kv in s.split('&') {
            if let Some((_, v)) = kv.split_once('=') {
                if v.len() >= 4 {
                    t = t.replace(v, "***");
                    if let Some(d) = decode(v) {
                        t = t.replace(&d, "***");
                    }
                }
            }
        }
    }
    let t = secrety.replace_all(&t, "$1$2***");
    let t = opaque.replace_all(&t, "***");
    let flat: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > EXCERPT_CHARS {
        format!("{}…", flat.chars().take(EXCERPT_CHARS - 1).collect::<String>())
    } else {
        flat
    }
}

/// `(host, port, path)` of an `http://` URL (what a callback is matched on), or None.
pub fn place_of(url: &str) -> Option<(String, u16, String)> {
    split(url)
        .filter(|u| u.scheme == "http")
        .map(|u| (u.host, u.port, u.path.to_string()))
}

/// The host of a URL for logs (never the path or query).
pub fn host_of(url: &str) -> String {
    split(url)
        .map(|u| format!("{}:{}", u.host, u.port))
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = "https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference&code_challenge=abc&code_challenge_method=S256&state=st4te-Claude";
    const CODEX: &str = "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&code_challenge=x&code_challenge_method=S256&state=codexState123&scope=openid+profile";

    fn cb(host: &str, port: u16, path: &str, state: &str) -> Callback {
        Callback {
            host: host.into(),
            port,
            path: path.into(),
            state: state.into(),
        }
    }

    #[test]
    fn detects_claude_and_codex_shapes() {
        assert_eq!(
            detect(CLAUDE),
            Some(cb("localhost", 54545, "/callback", "st4te-Claude"))
        );
        assert_eq!(
            detect(CODEX),
            Some(cb("127.0.0.1", 1455, "/auth/callback", "codexState123"))
        );
    }

    #[test]
    fn ignores_what_is_not_a_loopback_login() {
        for u in [
            "https://example.com/",
            "https://x/authorize?client_id=a&redirect_uri=https%3A%2F%2Fapp.example%2Fcb&state=s",
            "https://x/authorize?client_id=a&redirect_uri=http%3A%2F%2Flocalhost%2Fcb&state=s", // no port
            "https://x/authorize?client_id=a&redirect_uri=http%3A%2F%2Flocalhost%3A99999%2Fcb&state=s",
            "https://x/authorize?client_id=a&redirect_uri=http%3A%2F%2Fevil.com%3A80%2Fcb&state=s",
            "https://x/authorize?client_id=a&redirect_uri=http%3A%2F%2Fu%40localhost%3A80%2Fcb&state=s",
            "https://x/authorize?client_id=a&redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcb", // no state
            "https://x/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcb&state=s", // no client_id/response_type
            "https://x/authorize?client_id=a&redirect_uri=https%3A%2F%2Flocalhost%3A8080%2Fcb&state=s", // https
        ] {
            assert_eq!(detect(u), None, "{u}");
        }
    }

    #[test]
    fn a_pasted_url_must_match_place_and_state() {
        let c = cb("localhost", 54545, "/callback", "S1");
        assert_eq!(
            matches(&c, "http://localhost:54545/callback?code=abc&state=S1"),
            Ok("code=abc&state=S1")
        );
        assert_eq!(
            matches(&c, "HTTP://LOCALHOST:54545/callback?state=S1&code=abc#x"),
            Ok("state=S1&code=abc")
        );
        assert_eq!(
            matches(&c, "http://localhost:54545/callback?code=abc&state=S2"),
            Err(Refusal::StateMismatch)
        );
        assert_eq!(
            matches(&c, "http://localhost:54545/callback?code=abc"),
            Err(Refusal::StateMismatch)
        );
        assert_eq!(
            matches(&c, "http://localhost:54546/callback?code=abc&state=S1"),
            Err(Refusal::Mismatch)
        );
        assert_eq!(
            matches(&c, "http://localhost:54545/other?code=abc&state=S1"),
            Err(Refusal::Mismatch)
        );
        assert_eq!(
            matches(&c, "http://127.0.0.1:54545/callback?code=abc&state=S1"),
            Err(Refusal::Mismatch)
        );
        assert_eq!(
            matches(&c, "https://localhost:54545/callback?code=abc&state=S1"),
            Err(Refusal::Mismatch)
        );
        assert_eq!(matches(&c, "not a url"), Err(Refusal::InvalidUrl));
    }

    #[test]
    fn relay_accepts_once_and_names_every_refusal() {
        let t0 = Instant::now();
        let mut r = Relay::default();
        let up = |_| true;
        let ok = "http://127.0.0.1:1455/auth/callback?code=c0de&state=codexState123";
        assert_eq!(r.check("w1:p1", ok, t0, up), Err(Refusal::NoPending));
        r.record("w1:p1", detect(CODEX).unwrap(), t0);
        assert_eq!(r.check("w1:p2", ok, t0, up), Err(Refusal::NoPending), "another pane");
        assert_eq!(
            r.check(
                "w1:p1",
                "http://127.0.0.1:1455/auth/callback?code=c0de&state=nope",
                t0,
                up
            ),
            Err(Refusal::StateMismatch)
        );
        assert_eq!(
            r.check(
                "w1:p1",
                "http://127.0.0.1:1456/auth/callback?code=c0de&state=codexState123",
                t0,
                up
            ),
            Err(Refusal::Mismatch)
        );
        assert_eq!(
            r.check(
                "w1:p1",
                "http://127.0.0.1:1455/callback?code=c0de&state=codexState123",
                t0,
                up
            ),
            Err(Refusal::Mismatch)
        );
        assert_eq!(
            r.check(
                "w1:p1",
                "https://evil.example/auth/callback?state=codexState123",
                t0,
                up
            ),
            Err(Refusal::InvalidUrl)
        );
        assert_eq!(r.check("w1:p1", ok, t0, |_| false), Err(Refusal::NotListening));
        // (a minute later: the attempts above used up this minute's allowance)
        let t1 = t0 + RATE_WINDOW;
        let a = r.check("w1:p1", ok, t1, up).unwrap();
        assert_eq!(a.query, "code=c0de&state=codexState123");
        assert_eq!(a.callback.port, 1455);
        assert_eq!(r.check("w1:p1", ok, t1, up), Err(Refusal::AlreadyUsed), "single use");
    }

    #[test]
    fn relay_expires_and_rate_limits() {
        let t0 = Instant::now();
        let mut r = Relay::default();
        r.record("p", detect(CLAUDE).unwrap(), t0);
        let ok = "http://localhost:54545/callback?code=x&state=st4te-Claude";
        assert_eq!(r.check("p", ok, t0 + PENDING_TTL, |_| true), Err(Refusal::Expired));
        // A fresh open_url of the same login refreshes it.
        let t1 = t0 + PENDING_TTL;
        r.record("p", detect(CLAUDE).unwrap(), t1);
        // (the expired attempt above counts too: same instant)
        for _ in 0..RATE_MAX - 1 {
            assert_eq!(
                r.check("p", "http://localhost:54545/callback?state=bad", t1, |_| true),
                Err(Refusal::StateMismatch)
            );
        }
        assert_eq!(r.check("p", ok, t1, |_| true), Err(Refusal::RateLimited));
        assert!(r.check("p", ok, t1 + RATE_WINDOW, |_| true).is_ok());
    }

    #[test]
    fn proc_net_tcp_listeners() {
        let head = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let v4 = format!("{head}   0: 0100007F:05AF 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1\n   1: 0100007F:05B0 0100007F:9999 01 0 0 0 0 0 0\n");
        assert_eq!(listening_in(&v4, "", 1455), Some("127.0.0.1:1455".parse().unwrap()));
        assert_eq!(listening_in(&v4, "", 1456), None, "established, not listening");
        let v6 = format!(
            "{head}   0: 00000000000000000000000001000000:D531 00000000000000000000000000000000:0000 0A 0 0 0 0 0 0\n"
        );
        assert_eq!(listening_in(head, &v6, 54577), Some("[::1]:54577".parse().unwrap()));
    }

    #[test]
    fn responses_and_masked_excerpts() {
        let ok = parse_response(b"HTTP/1.1 302 Found\r\nLocation: https://x/success\r\n\r\n", &[]).unwrap();
        assert!(ok.ok());
        assert_eq!(ok.excerpt, None);
        let bad = parse_response(
            b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<html><body><h1>State mismatch</h1><p>code=c0deSecretValue state=abcd</p><p>token eyJhbGciOiJIUzI1NiJ9xxxxxxxxxxxx</p></body></html>",
            &["code=c0deSecretValue&state=abcd"],
        )
        .unwrap();
        assert_eq!(bad.status, 400);
        let e = bad.excerpt.unwrap();
        assert!(e.contains("State mismatch"), "{e}");
        assert!(
            !e.contains("c0deSecretValue") && !e.contains("eyJ") && !e.contains('<'),
            "{e}"
        );
        let chunked = parse_response(
            b"HTTP/1.1 500 Oops\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            &[],
        )
        .unwrap();
        assert_eq!(chunked.excerpt.as_deref(), Some("hello world"));
        assert!(parse_response(b"garbage", &[]).is_err());
        assert_eq!(mask_excerpt(&"a ".repeat(500), &[]).chars().count(), EXCERPT_CHARS);
    }

    /// The shared golden fixtures (packages/protocol/fixtures/login-callback.json).
    #[test]
    fn golden_fixtures() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/protocol/fixtures/login-callback.json"
        );
        let f: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for c in f["detect"].as_array().unwrap() {
            let got = detect(c["url"].as_str().unwrap()).map(|c| c.to_json());
            assert_eq!(got.unwrap_or(Value::Null), c["callback"], "detect {}", c["name"]);
        }
        for c in f["match"].as_array().unwrap() {
            let cb: Callback = Callback {
                host: c["callback"]["host"].as_str().unwrap().into(),
                port: c["callback"]["port"].as_u64().unwrap() as u16,
                path: c["callback"]["path"].as_str().unwrap().into(),
                state: c["callback"]["state"].as_str().unwrap().into(),
            };
            let got = match matches(&cb, c["pasted"].as_str().unwrap()) {
                Ok(_) => "ok",
                Err(r) => r.code(),
            };
            assert_eq!(got, c["outcome"], "match {}", c["name"]);
        }
    }
}
