//! The signer behind a key-bound runner token (docs/protocol.md "Runner
//! proof", docs/design/mac-pairing.md §3.2).
//!
//! The runner never holds the private key. On a Mac it lives in the Secure
//! Enclave and only the tmuxedo app can use it; the app starts the runner with
//! one end of a socketpair as `TMUXEDO_SIGNER_FD` (no file on disk, nothing for
//! another process to connect to). Alternatively `TMUXEDO_SIGNER_SOCKET` names
//! a unix socket the app listens on (the isolated-user mode, where the runner
//! is not the app's child; the app checks the peer itself).
//!
//! Wire: one JSON line each way.
//!   → `{"op":"sign","input":"<standard base64 of the bytes>"}`
//!   ← `{"sig":"<base64url signature>"}` or `{"error":"<why>"}`
//!
//! The app signs ONLY runner-proof inputs naming its own token, so this is not
//! a general signing oracle. The fd is marked close-on-exec at once so no child
//! of the runner inherits it.

use base64::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long one signature may take (the app may be busy; the gateway waits 10 s).
pub const SIGN_TIMEOUT: Duration = Duration::from_secs(5);

pub trait Signer: Send + Sync {
    fn sign(&self, input: &[u8]) -> Result<String, String>;
    /// Any other request to the app (one JSON line each way): `confirm`, `confirm_poll`.
    fn call(&self, _req: &Value) -> Result<Value, String> {
        Err("this signer cannot ask the Mac".into())
    }
}

/// A cloneable, printable handle for options structs.
#[derive(Clone)]
pub struct SignerHandle(pub Arc<dyn Signer>);

impl std::fmt::Debug for SignerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignerHandle(..)")
    }
}

impl SignerHandle {
    pub fn sign(&self, input: &[u8]) -> Result<String, String> {
        self.0.sign(input)
    }
    pub fn call(&self, req: &Value) -> Result<Value, String> {
        self.0.call(req)
    }
}

fn request(input: &[u8]) -> String {
    let mut line =
        json!({ "op": "sign", "input": base64::engine::general_purpose::STANDARD.encode(input) }).to_string();
    line.push('\n');
    line
}

fn answer(line: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(line.trim()).map_err(|_| "signer answered junk".to_string())?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Err(format!("signer refused: {e}"));
    }
    match v.get("sig").and_then(Value::as_str) {
        Some(s)
            if !s.is_empty()
                && s.len() <= 200
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Ok(s.to_string())
        }
        _ => Err("signer answered without a signature".into()),
    }
}

fn exchange(stream: &mut UnixStream, input: &[u8]) -> Result<String, String> {
    answer(&round_trip(stream, &request(input))?)
}

/// One line out, one line back, within [`SIGN_TIMEOUT`].
fn round_trip(stream: &mut UnixStream, line_out: &str) -> Result<String, String> {
    stream.set_read_timeout(Some(SIGN_TIMEOUT)).map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(SIGN_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(line_out.as_bytes())
        .map_err(|e| format!("signer: {e}"))?;
    let mut r = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    let n = r.read_line(&mut line).map_err(|e| format!("signer: {e}"))?;
    if n == 0 {
        return Err("signer closed the connection".into());
    }
    Ok(line)
}

fn call_on(stream: &mut UnixStream, req: &Value) -> Result<Value, String> {
    let mut out = req.to_string();
    out.push('\n');
    let line = round_trip(stream, &out)?;
    let v: Value = serde_json::from_str(line.trim()).map_err(|_| "the app answered junk".to_string())?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Err(format!("the app refused: {e}"));
    }
    Ok(v)
}

/// An inherited, already-connected socket (one request at a time).
pub struct FdSigner {
    stream: Mutex<UnixStream>,
}

impl FdSigner {
    /// Take ownership of `fd` and make it close-on-exec.
    ///
    /// # Safety
    /// `fd` must be an open unix stream socket this process owns and nothing else uses.
    pub unsafe fn from_raw_fd(fd: i32) -> Result<Self, String> {
        use std::os::fd::FromRawFd;
        if fd < 0 || libc::fcntl(fd, libc::F_GETFD) < 0 {
            return Err(format!("TMUXEDO_SIGNER_FD {fd} is not an open descriptor"));
        }
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok(Self {
            stream: Mutex::new(UnixStream::from_raw_fd(fd)),
        })
    }
    pub fn from_stream(s: UnixStream) -> Self {
        Self { stream: Mutex::new(s) }
    }
}

impl Signer for FdSigner {
    fn sign(&self, input: &[u8]) -> Result<String, String> {
        let mut s = self.stream.lock().map_err(|_| "signer lock poisoned".to_string())?;
        exchange(&mut s, input)
    }
    fn call(&self, req: &Value) -> Result<Value, String> {
        let mut s = self.stream.lock().map_err(|_| "signer lock poisoned".to_string())?;
        call_on(&mut s, req)
    }
}

/// A socket the app listens on; one connection per signature.
pub struct SocketSigner {
    path: PathBuf,
}

impl SocketSigner {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl SocketSigner {
    fn connect(&self) -> Result<UnixStream, String> {
        UnixStream::connect(&self.path).map_err(|e| format!("signer {}: {e}", self.path.display()))
    }
}

impl Signer for SocketSigner {
    fn sign(&self, input: &[u8]) -> Result<String, String> {
        exchange(&mut self.connect()?, input)
    }
    fn call(&self, req: &Value) -> Result<Value, String> {
        call_on(&mut self.connect()?, req)
    }
}

/// How a question to the Mac ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    Allowed,
    Denied,
    TimedOut,
    /// The app could not be asked (too old, gone, refused the request): treated as denied.
    Failed(String),
}

/// Ask the person at the Mac (docs/design/mac-isolation.md §3). Each exchange is short (a
/// ticket, then polls), so a signature the gateway needs meanwhile is never stuck behind it.
///   → `{"op":"confirm","pane":…,"what":…,"timeout":<s>}`  ← `{"ticket":"…"}`
///   → `{"op":"confirm_poll","ticket":"…"}`               ← `{"state":"pending|allow|deny|timeout"}`
pub fn confirm(s: &SignerHandle, pane: &str, what: &str, timeout: Duration, poll: Duration) -> Confirmed {
    let v = match s.call(&json!({ "op": "confirm", "pane": pane, "what": what, "timeout": timeout.as_secs() })) {
        Ok(v) => v,
        Err(e) => return Confirmed::Failed(e),
    };
    let Some(ticket) = v.get("ticket").and_then(Value::as_str).map(str::to_string) else {
        return Confirmed::Failed("the app answered without a ticket".into());
    };
    // The app times the question out itself; this is the backstop if it never says so.
    let deadline = std::time::Instant::now() + timeout + Duration::from_secs(5);
    loop {
        std::thread::sleep(poll);
        if std::time::Instant::now() > deadline {
            return Confirmed::TimedOut;
        }
        match s.call(&json!({ "op": "confirm_poll", "ticket": ticket })) {
            Ok(v) => match v.get("state").and_then(Value::as_str) {
                Some("allow") => return Confirmed::Allowed,
                Some("deny") => return Confirmed::Denied,
                Some("timeout") => return Confirmed::TimedOut,
                Some("pending") => {}
                _ => return Confirmed::Failed("the app answered an unknown state".into()),
            },
            Err(e) => return Confirmed::Failed(e),
        }
    }
}

/// The signer this process was given, if any: `TMUXEDO_SIGNER_FD`, else `TMUXEDO_SIGNER_SOCKET`.
/// The variables are removed from the environment so no child sees them.
pub fn from_env() -> Result<Option<SignerHandle>, String> {
    let fd = std::env::var("TMUXEDO_SIGNER_FD").ok().filter(|s| !s.is_empty());
    let sock = std::env::var_os("TMUXEDO_SIGNER_SOCKET").filter(|s| !s.is_empty());
    std::env::remove_var("TMUXEDO_SIGNER_FD");
    std::env::remove_var("TMUXEDO_SIGNER_SOCKET");
    if let Some(fd) = fd {
        let n: i32 = fd
            .parse()
            .map_err(|_| format!("TMUXEDO_SIGNER_FD is not a number: {fd}"))?;
        // SAFETY: the parent handed us this descriptor for exactly this purpose.
        let s = unsafe { FdSigner::from_raw_fd(n)? };
        return Ok(Some(SignerHandle(Arc::new(s))));
    }
    Ok(sock.map(|p| SignerHandle(Arc::new(SocketSigner::new(PathBuf::from(p))))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    /// A fake app: answers each request line with `reply(input bytes)`.
    fn fake_app(reply: impl Fn(&[u8]) -> String + Send + 'static) -> UnixStream {
        let (ours, theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut r = BufReader::new(theirs.try_clone().unwrap());
            let mut w = theirs;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let v: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["op"], "sign");
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(v["input"].as_str().unwrap())
                    .unwrap();
                let _ = w.write_all(format!("{}\n", reply(&bytes)).as_bytes());
            }
        });
        ours
    }

    #[test]
    fn signs_through_the_socket_one_line_each_way() {
        let s = FdSigner::from_stream(fake_app(|b| json!({ "sig": format!("sig_{}", b.len()) }).to_string()));
        assert_eq!(s.sign(b"abc").unwrap(), "sig_3");
        assert_eq!(s.sign(b"tmuxedo-runner-proof-v1\nn\nl").unwrap(), "sig_27");
    }

    #[test]
    fn refusals_junk_and_hangups_are_errors() {
        let s = FdSigner::from_stream(fake_app(|_| json!({ "error": "not a runner proof" }).to_string()));
        assert!(s.sign(b"x").unwrap_err().contains("not a runner proof"));
        let s = FdSigner::from_stream(fake_app(|_| "nonsense".into()));
        assert!(s.sign(b"x").is_err());
        let s = FdSigner::from_stream(fake_app(|_| json!({ "sig": "has spaces in it" }).to_string()));
        assert!(s.sign(b"x").is_err());
        let s = FdSigner::from_stream(fake_app(|_| json!({}).to_string()));
        assert!(s.sign(b"x").is_err());
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        assert!(FdSigner::from_stream(ours).sign(b"x").is_err());
    }

    #[test]
    fn a_silent_app_times_out() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let t = std::time::Instant::now();
        assert!(FdSigner::from_stream(ours).sign(b"x").is_err());
        assert!(t.elapsed() >= SIGN_TIMEOUT - Duration::from_millis(100));
    }

    #[test]
    fn the_fd_is_taken_close_on_exec_and_bad_fds_refused() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let fd = ours.into_raw_fd();
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let s = unsafe { FdSigner::from_raw_fd(fd) }.unwrap();
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC
        );
        drop(s);
        assert!(unsafe { FdSigner::from_raw_fd(9999) }.is_err());
        assert!(unsafe { FdSigner::from_raw_fd(-1) }.is_err());
    }

    /// A fake app that answers `confirm` with a ticket and `confirm_poll` with `states` in turn.
    fn confirming_app(states: Vec<&'static str>) -> SignerHandle {
        let (ours, theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut r = BufReader::new(theirs.try_clone().unwrap());
            let mut w = theirs;
            let mut states = states.into_iter();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let v: Value = serde_json::from_str(&line).unwrap_or_default();
                let out = match v["op"].as_str() {
                    Some("confirm") if v["pane"] == "w1:p1" && v["what"] == "typing" => json!({ "ticket": "t1" }),
                    Some("confirm_poll") if v["ticket"] == "t1" => {
                        json!({ "state": states.next().unwrap_or("pending") })
                    }
                    _ => json!({ "error": "bad request" }),
                };
                let _ = w.write_all(format!("{out}\n").as_bytes());
            }
        });
        SignerHandle(Arc::new(FdSigner::from_stream(ours)))
    }

    #[test]
    fn confirm_asks_then_polls_until_the_mac_answers() {
        let ms = Duration::from_millis(5);
        let t = Duration::from_secs(30);
        let ask = |states| confirm(&confirming_app(states), "w1:p1", "typing", t, ms);
        assert_eq!(ask(vec!["pending", "allow"]), Confirmed::Allowed);
        assert_eq!(ask(vec!["deny"]), Confirmed::Denied);
        assert_eq!(ask(vec!["pending", "timeout"]), Confirmed::TimedOut);
        assert!(matches!(ask(vec!["maybe"]), Confirmed::Failed(_)));
    }

    #[test]
    fn confirm_fails_closed_without_an_app_that_can_ask() {
        let s = Duration::from_secs(1);
        // An app that does not know the request (an older one): `bad request`.
        let r = confirm(&confirming_app(vec![]), "other-pane", "typing", s, s);
        assert!(
            matches!(r, Confirmed::Failed(ref e) if e.contains("bad request")),
            "{r:?}"
        );
        // A signer with no `call` at all.
        struct SignOnly;
        impl Signer for SignOnly {
            fn sign(&self, _: &[u8]) -> Result<String, String> {
                Ok("x".into())
            }
        }
        assert!(matches!(
            confirm(&SignerHandle(Arc::new(SignOnly)), "p", "typing", s, s),
            Confirmed::Failed(_)
        ));
    }

    /// The app never answers `allow`/`deny`: the runner's own backstop denies.
    #[test]
    fn confirm_has_a_backstop_deadline() {
        let t = std::time::Instant::now();
        let r = confirm(
            &confirming_app(vec![]),
            "w1:p1",
            "typing",
            Duration::from_secs(0),
            Duration::from_millis(200),
        );
        assert_eq!(r, Confirmed::TimedOut);
        assert!(t.elapsed() >= Duration::from_secs(5));
    }

    #[test]
    fn socket_signer_connects_per_request() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("signer.sock");
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for c in l.incoming().take(2) {
                let c = c.unwrap();
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut w = c;
                w.write_all(b"{\"sig\":\"ok_sig\"}\n").unwrap();
            }
        });
        let s = SocketSigner::new(path.clone());
        assert_eq!(s.sign(b"a").unwrap(), "ok_sig");
        assert_eq!(s.sign(b"b").unwrap(), "ok_sig");
        assert!(SocketSigner::new(d.path().join("nope.sock")).sign(b"a").is_err());
    }
}
