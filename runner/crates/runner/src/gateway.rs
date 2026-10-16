//! The outbound WebSocket to the gateway: dial, heartbeat, reconnect.
//!
//! The connection task owns heartbeats (app-level `ping` every 20 s, answer
//! `pong`, drop the socket after 75 s of silence) and the backoff; the daemon
//! only sees frames, connects and disconnects. Close codes 4001 (replaced)
//! and 4003 (revoked) end the task for good; 4002 (unsupported version)
//! waits a long time before trying again.
//!
//! No circular wait (audit P2-5): the socket's reader and writer run
//! concurrently, so the writer always drains the daemon's outbound queue
//! while the reader waits to hand a frame to the daemon, and the daemon
//! never awaits a full outbound queue (it `try_send`s, keeps an ordered
//! overflow, and asks for a reset through [`GwLink::reset`] when the
//! socket is stalled; a reconnect resyncs everything from offsets).

use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tmuxedo_protocol::{close, SUBPROTOCOLS};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

pub const PING_EVERY: Duration = Duration::from_secs(20);
pub const SILENCE_LIMIT: Duration = Duration::from_secs(75);

/// The daemon's handle on one live connection.
#[derive(Debug, Clone)]
pub struct GwLink {
    /// Outbound frames (bounded; the daemon only `try_send`s).
    pub out: mpsc::Sender<Message>,
    /// Drop this connection now (stalled socket); the loop reconnects.
    pub reset: Arc<tokio::sync::Notify>,
}

/// Outbound queue length per connection.
pub const OUT_QUEUE: usize = 256;

#[derive(Debug)]
pub enum GwEvent {
    Connected(GwLink),
    Text(String),
    Binary(Vec<u8>),
    Disconnected {
        code: Option<u16>,
        reason: String,
    },
    /// 4001 or 4003: stop for good.
    Fatal(u16),
}

#[derive(Debug, Clone)]
pub struct GatewayOptions {
    pub url: String,
    pub token: String,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Wait after 4002 `unsupported_version`.
    pub unsupported_wait: Duration,
    /// Signs the gateway's `challenge` for a key-bound token (docs/protocol.md "Runner proof").
    pub signer: Option<crate::signer::SignerHandle>,
    /// The files worker: upload chunks (`file_put`, FILE frames) go straight there,
    /// waiting when it is full so the gateway feels the disk's pace (crate::files::worker).
    pub files: Option<mpsc::Sender<crate::files::worker::Msg>>,
}

/// The token's public lookup half (`tmr.<lookup>.<secret>`), which the proof names.
pub fn token_lookup(token: &str) -> Option<&str> {
    let mut parts = token.split('.');
    let (_, lookup, _) = (parts.next()?, parts.next()?, parts.next()?);
    (parts.next().is_none() && !lookup.is_empty()).then_some(lookup)
}

/// The answer to `{t:"challenge", nonce}`: `{t:"proof", sig}`, or why there is none.
pub fn answer_challenge(
    nonce: &str,
    token: &str,
    signer: Option<&crate::signer::SignerHandle>,
) -> Result<String, String> {
    if nonce.is_empty()
        || nonce.len() > 128
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("the gateway's challenge is malformed".into());
    }
    let signer = signer
        .ok_or("this machine's token is key-bound but no signer was given (start the runner from the tmuxedo app)")?;
    let lookup = token_lookup(token).ok_or("the runner token is malformed")?;
    let input = tmuxedo_protocol::runner_proof_input(nonce, lookup);
    let sig = signer.sign(input.as_bytes())?;
    Ok(serde_json::json!({ "t": "proof", "sig": sig }).to_string())
}

/// `Some(nonce)` when `text` is a gateway `challenge` frame.
pub fn challenge_nonce(text: &str) -> Option<String> {
    if !text.contains("\"challenge\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    (v.get("t")?.as_str()? == "challenge").then(|| v.get("nonce").and_then(|n| n.as_str()).unwrap_or("").to_string())
}

/// Exponential backoff with jitter, reset by a session that actually ran.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    cur: Duration,
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            cur: Duration::ZERO,
        }
    }
    /// The next wait without jitter: min, then doubling, capped.
    pub fn next_base(&mut self) -> Duration {
        self.cur = if self.cur.is_zero() {
            self.min
        } else {
            (self.cur * 2).min(self.max)
        };
        self.cur
    }
    /// The next wait with ±20% jitter.
    pub fn next_wait(&mut self) -> Duration {
        let base = self.next_base();
        let r = crate::util::random_bytes(2);
        let unit = u16::from_be_bytes([r[0], r[1]]) as f64 / u16::MAX as f64; // 0..1
        base.mul_f64(0.8 + 0.4 * unit)
    }
    pub fn reset(&mut self) {
        self.cur = Duration::ZERO;
    }
}

pub fn tls_connector() -> Connector {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Connector::Rustls(Arc::new(config))
}

enum End {
    Closed(Option<u16>, String),
    DaemonGone,
}

/// Run until a fatal close code, or until the daemon drops its receiver.
pub async fn connection_loop(opts: GatewayOptions, tx: mpsc::Sender<GwEvent>) {
    let mut backoff = Backoff::new(opts.backoff_min, opts.backoff_max);
    loop {
        let started = Instant::now();
        let end = match connect_once(&opts, &tx).await {
            Ok(end) => end,
            Err(e) => End::Closed(None, e),
        };
        let (code, reason) = match end {
            End::DaemonGone => return,
            End::Closed(code, reason) => (code, reason),
        };
        if tx
            .send(GwEvent::Disconnected {
                code,
                reason: reason.clone(),
            })
            .await
            .is_err()
        {
            return;
        }
        match code {
            Some(c) if c == close::REPLACED || c == close::REVOKED => {
                if reason == "proof_failed" {
                    crate::log!("the gateway refused this machine's key proof; pair this machine again");
                }
                let _ = tx.send(GwEvent::Fatal(c)).await;
                return;
            }
            Some(c) if c == close::UNSUPPORTED_VERSION => {
                crate::log!(
                    "gateway refused protocol version ({reason}); waiting {:?}",
                    opts.unsupported_wait
                );
                tokio::time::sleep(opts.unsupported_wait).await;
                continue;
            }
            _ => {}
        }
        if started.elapsed() >= Duration::from_secs(60) {
            backoff.reset();
        }
        let wait = backoff.next_wait();
        crate::log!(
            "gateway disconnected (code {code:?}: {reason}); reconnecting in {:.1}s",
            wait.as_secs_f64()
        );
        tokio::time::sleep(wait).await;
    }
}

async fn connect_once(opts: &GatewayOptions, tx: &mpsc::Sender<GwEvent>) -> Result<End, String> {
    let mut req = opts.url.as_str().into_client_request().map_err(|e| e.to_string())?;
    let h = req.headers_mut();
    h.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", opts.token)).map_err(|e| e.to_string())?,
    );
    h.insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&SUBPROTOCOLS.join(", ")).map_err(|e| e.to_string())?,
    );
    h.insert(
        "User-Agent",
        HeaderValue::from_str(&format!("tmuxedo-runner/{}", crate::VERSION))
            .unwrap_or(HeaderValue::from_static("tmuxedo-runner")),
    );
    let connector = if opts.url.starts_with("wss://") {
        Some(tls_connector())
    } else {
        None
    };
    let (ws, _resp) = tokio::time::timeout(
        Duration::from_secs(20),
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, connector),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|e| e.to_string())?;
    crate::log!("connected to {}", opts.url);

    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(OUT_QUEUE);
    // Replies the connection makes itself (pong), ahead of daemon traffic.
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Message>();
    let reset = Arc::new(tokio::sync::Notify::new());
    let link = GwLink {
        out: out_tx,
        reset: reset.clone(),
    };
    if tx.send(GwEvent::Connected(link)).await.is_err() {
        return Ok(End::DaemonGone);
    }

    // Writer: control lane first, then daemon frames, plus the heartbeat.
    // A stalled socket blocks only this future.
    let writer = async {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            let m = tokio::select! {
                biased;
                Some(m) = ctl_rx.recv() => m,
                out = out_rx.recv() => match out {
                    Some(m) => m,
                    None => return End::DaemonGone,
                },
                _ = ping.tick() => {
                    Message::Text(tmuxedo_protocol::frames::RunnerFrame::Ping { at: crate::util::now_ms() }.to_json().into())
                }
            };
            let is_close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() {
                return End::Closed(None, "send failed".into());
            }
            if is_close {
                return End::Closed(Some(close::GOING_AWAY), "runner shutting down".into());
            }
        }
    };

    // Reader: frames to the daemon (awaiting it is fine: the daemon never
    // blocks on us), pings answered on the control lane, silence detection.
    let reader = async {
        loop {
            let msg = match tokio::time::timeout(SILENCE_LIMIT, stream.next()).await {
                Ok(m) => m,
                Err(_) => return End::Closed(None, "gateway silent".into()),
            };
            match msg {
                Some(Ok(Message::Text(t))) => {
                    let t = t.to_string();
                    // A key-bound token: prove possession of the key before anything else happens.
                    if let Some(nonce) = challenge_nonce(&t) {
                        let (token, signer) = (opts.token.clone(), opts.signer.clone());
                        let answer =
                            tokio::task::spawn_blocking(move || answer_challenge(&nonce, &token, signer.as_ref()))
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()));
                        match answer {
                            Ok(proof) => {
                                let _ = ctl_tx.send(Message::Text(proof.into()));
                                crate::log!("answered the gateway's key challenge");
                            }
                            // The gateway closes 4003 proof_failed; that stops us (never retried as bearer).
                            Err(e) => crate::log!("cannot answer the gateway's key challenge: {e}"),
                        }
                        continue;
                    }
                    // Upload control frames go to the files worker, in socket order with their bytes.
                    if let Some(files) = opts.files.as_ref().filter(|_| t.contains("\"file_put")) {
                        if let Some(c) = tmuxedo_protocol::files::FileCtl::parse(&t) {
                            if files.send(crate::files::worker::Msg::Ctl(c)).await.is_err() {
                                return End::DaemonGone;
                            }
                            continue;
                        }
                    }
                    if let tmuxedo_protocol::frames::Parsed::Frame(f) = tmuxedo_protocol::frames::parse_gateway(&t) {
                        if let tmuxedo_protocol::frames::GatewayFrame::Ping { .. } = f {
                            let pong = tmuxedo_protocol::frames::RunnerFrame::Pong {
                                at: crate::util::now_ms(),
                            }
                            .to_json();
                            let _ = ctl_tx.send(Message::Text(pong.into()));
                            continue;
                        }
                        if let tmuxedo_protocol::frames::GatewayFrame::Pong { .. } = f {
                            continue;
                        }
                    }
                    if tx.send(GwEvent::Text(t)).await.is_err() {
                        return End::DaemonGone;
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    if tmuxedo_protocol::files::is_file_frame(&b) {
                        if let (Some(files), Some(f)) = (&opts.files, tmuxedo_protocol::files::FileFrame::decode(&b)) {
                            if files.send(crate::files::worker::Msg::Data(f)).await.is_err() {
                                return End::DaemonGone;
                            }
                        }
                        continue;
                    }
                    if tx.send(GwEvent::Binary(b.to_vec())).await.is_err() {
                        return End::DaemonGone;
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = match frame {
                        Some(f) => (Some(u16::from(f.code)), f.reason.to_string()),
                        None => (None, "closed".into()),
                    };
                    return End::Closed(code, reason);
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return End::Closed(None, e.to_string()),
                None => return End::Closed(None, "eof".into()),
            }
        }
    };

    // Whichever ends first ends the connection (the other is dropped, which
    // closes the socket).
    Ok(tokio::select! {
        e = writer => e,
        e = reader => e,
        _ = reset.notified() => End::Closed(None, "outbound queue stalled".into()),
    })
}

/// A close frame with the given code (used for orderly shutdown).
pub fn close_message(code: u16, reason: &str) -> Message {
    Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: CloseCode::from(code),
        reason: reason.to_string().into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(&'static str);
    impl crate::signer::Signer for Fixed {
        fn sign(&self, input: &[u8]) -> Result<String, String> {
            assert!(input.starts_with(b"tmuxedo-runner-proof-v1\n"));
            Ok(self.0.to_string())
        }
    }

    #[test]
    fn challenge_frames_are_recognised_and_answered_through_the_signer() {
        assert_eq!(
            challenge_nonce(r#"{"t":"challenge","nonce":"abc_-1"}"#).as_deref(),
            Some("abc_-1")
        );
        assert_eq!(challenge_nonce(r#"{"t":"welcome","machineId":"m","userId":"u"}"#), None);
        assert_eq!(
            challenge_nonce(r#"{"t":"req","id":"1","method":"challenge","params":{}}"#),
            None
        );
        assert_eq!(challenge_nonce("not json \"challenge\""), None);
        let h = crate::signer::SignerHandle(std::sync::Arc::new(Fixed("SIG")));
        let tok = "tmr.LOOKUP.secret";
        let proof: serde_json::Value =
            serde_json::from_str(&answer_challenge("n0nce", tok, Some(&h)).unwrap()).unwrap();
        assert_eq!(proof, serde_json::json!({"t": "proof", "sig": "SIG"}));
        assert!(answer_challenge("n0nce", tok, None).unwrap_err().contains("no signer"));
        assert!(answer_challenge("bad nonce!", tok, Some(&h)).is_err());
        assert!(answer_challenge("", tok, Some(&h)).is_err());
        assert!(answer_challenge("n", "garbage", Some(&h)).is_err());
        assert_eq!(token_lookup("tmr.abc.def"), Some("abc"));
        assert_eq!(token_lookup("tmr..def"), None);
        assert_eq!(token_lookup("tmr.a.b.c"), None);
    }

    #[test]
    fn backoff_doubles_caps_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
        let seq: Vec<u64> = (0..7).map(|_| b.next_base().as_secs()).collect();
        assert_eq!(seq, vec![2, 4, 8, 16, 32, 60, 60]);
        b.reset();
        assert_eq!(b.next_base().as_secs(), 2);
        for _ in 0..50 {
            let j = b.next_wait();
            assert!(j >= Duration::from_secs_f64(0.8 * b.cur.as_secs_f64()) - Duration::from_millis(1));
            assert!(j <= Duration::from_secs_f64(1.2 * b.cur.as_secs_f64()) + Duration::from_millis(1));
        }
    }
}
