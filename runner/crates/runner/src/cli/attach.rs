//! `tmuxedo attach <machine>[:<pane>]`: one pane through the gateway.
//!
//! Gets a one-minute attach ticket (`POST /api/gateway/ticket`, `control` or
//! `observe`), connects to `/gw/client` with `tmuxedo.v1, aimux.v1, ticket.<t>`, and
//! passes one pane through in raw mode. Prefix `ctrl-b` then `d` detaches
//! (`ctrl-b ctrl-b` sends a literal ctrl-b). Local resizes become `resize`
//! frames; processed bytes are acknowledged (`ack`) for flow control; local
//! terminal query replies are filtered out (see `filter`). A dropped socket
//! reconnects with a fresh ticket and resumes from the last processed offset.
//!
//! Copy-on-select does not apply here: attach is a raw passthrough that
//! never enables mouse capture, so the local terminal does its own text
//! selection and copying (if the pane's program turns on mouse reporting,
//! hold Shift — or your terminal's override key — to select). The `tmuxedo`
//! TUI (`cli/tui`), which owns the mouse, implements copy-on-select itself.

use super::filter::ReplyFilter;
use super::{api, load_cli_config, machines, resolve_machine, CliConfig};
use crate::config::http_base;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::time::Duration;
use tmuxedo_protocol::{DataFrame, DataKind, SUBPROTOCOLS};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

pub const PREFIX: u8 = 0x02; // ctrl-b
pub const ACK_EVERY: u64 = 64 * 1024;

/// Raw mode for the life of the value (only when stdin is a terminal).
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> RawMode {
        // SAFETY: termios calls on fd 0 with a zeroed struct filled by tcgetattr.
        unsafe {
            if libc::isatty(0) != 1 {
                return RawMode(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawMode(None);
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            RawMode(Some(orig))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            // SAFETY: restoring the attributes read in enable().
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

fn term_size() -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ fills a winsize struct.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return Some((ws.ws_col, ws.ws_row));
        }
    }
    None
}

/// The client socket URL: `$TMUXEDO_GATEWAY_URL`, else `wss://<server>/gw/client`.
pub fn client_url(server: &str) -> String {
    if let Some(u) = tmuxedo_holder::env_var("GATEWAY_URL") {
        return u;
    }
    let base = http_base(server);
    match base.strip_prefix("https://") {
        Some(h) => format!("wss://{h}/gw/client"),
        None => format!("ws://{}/gw/client", base.trim_start_matches("http://")),
    }
}

/// A client socket to the gateway.
pub type ClientSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// `POST /api/gateway/ticket {machineId, scope}` with the CLI token → (status, body).
pub async fn fetch_ticket(cfg: &CliConfig, mid: &str, scope: &str) -> Result<(u16, Value), String> {
    let (cfg, body) = (cfg.clone(), json!({ "machineId": mid, "scope": scope }));
    tokio::task::spawn_blocking(move || api(&cfg, "POST", "/api/gateway/ticket", Some(body)))
        .await
        .map_err(|e| e.to_string())?
}

/// Open `/gw/client` with subprotocols `tmuxedo.v1, aimux.v1, ticket.<ticket>` (an older gateway selects `aimux.v1`).
pub async fn dial(cfg: &CliConfig, ticket: &str) -> Result<ClientSocket, String> {
    let url = client_url(&cfg.server);
    let mut req = url.as_str().into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!("{}, ticket.{ticket}", SUBPROTOCOLS.join(", "))).map_err(|e| e.to_string())?,
    );
    let connector = url.starts_with("wss://").then(crate::gateway::tls_connector);
    tokio_tungstenite::connect_async_tls_with_config(req, None, false, connector)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

pub fn run(target: &str, control: bool, open_urls: bool) -> Result<(), String> {
    let cfg = load_cli_config()?;
    let (machine, pane) = match target.split_once(':') {
        Some((m, p)) => (m.to_string(), Some(p.to_string())),
        None => (target.to_string(), None),
    };
    let m = resolve_machine(&machines(&cfg)?, &machine)?;
    let mid = m["id"].as_str().ok_or("machine without id")?.to_string();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let raw = RawMode::enable();
    let r = rt.block_on(session(cfg, mid, pane, control, open_urls));
    drop(raw);
    eprintln!("\r\n[tmuxedo: {}]", if r.is_ok() { "detached" } else { "disconnected" });
    r
}

enum Input {
    Bytes(Vec<u8>),
    Eof,
}

async fn session(
    cfg: CliConfig,
    mid: String,
    mut pane: Option<String>,
    control: bool,
    open_urls: bool,
) -> Result<(), String> {
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Input>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = in_tx.send(Input::Eof);
                    return;
                }
                Ok(n) => {
                    if in_tx.send(Input::Bytes(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut winch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).map_err(|e| e.to_string())?;
    let mut filter = ReplyFilter::default();
    let mut processed: Option<u64> = None;
    let mut backoff = Duration::from_millis(500);
    let mut prefix = false;
    loop {
        let scope = if control { "control" } else { "observe" };
        let ticket = fetch_ticket(&cfg, &mid, scope).await?;
        if ticket.0 != 200 {
            return Err(format!("ticket refused ({}): {}", ticket.0, ticket.1));
        }
        let t = ticket.1["ticket"].as_str().ok_or("ticket missing")?.to_string();
        let ws = match dial(&cfg, &t).await {
            Ok(ws) => ws,
            Err(e) => {
                eprint!("\r\n[tmuxedo: {e}; retrying]\r\n");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(15));
                continue;
            }
        };
        let (mut sink, mut stream) = ws.split();
        let send = |v: Value| Message::Text(v.to_string().into());
        sink.send(send(
            json!({ "t": "hello", "client": { "kind": "tui", "version": crate::VERSION } }),
        ))
        .await
        .map_err(|e| e.to_string())?;
        // Wait for welcome (and topology, if we must pick a pane).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut welcomed = false;
        while !welcomed || pane.is_none() {
            let msg = tokio::time::timeout_at(deadline, stream.next())
                .await
                .map_err(|_| "no welcome/topology from the gateway (is the machine online?)".to_string())?;
            let Some(Ok(Message::Text(text))) = msg else {
                if matches!(msg, Some(Ok(_))) {
                    continue;
                }
                return Err("gateway closed the connection".into());
            };
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            match v["t"].as_str() {
                Some("welcome") => {
                    welcomed = true;
                    if v["machine"]["online"] == json!(false) {
                        eprint!("\r\n[tmuxedo: machine is offline; waiting]\r\n");
                    }
                }
                Some("topology") if pane.is_none() => {
                    pane = v["panes"][0]["id"].as_str().map(str::to_owned);
                }
                Some("error") => return Err(format!("{}: {}", v["code"], v["message"])),
                _ => {}
            }
        }
        let pane_id = pane.clone().unwrap_or_default();
        let mut attach = json!({ "t": "attach", "paneId": pane_id });
        if let Some(f) = processed {
            attach["from"] = json!(f);
        }
        sink.send(send(attach)).await.map_err(|e| e.to_string())?;
        if control {
            if let Some((c, r)) = term_size() {
                sink.send(send(json!({ "t": "resize", "paneId": pane_id, "cols": c, "rows": r })))
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        backoff = Duration::from_millis(500);
        let mut acked = processed.unwrap_or(0);
        let mut ack_tick = tokio::time::interval(Duration::from_millis(500));
        let mut out = std::io::stdout();
        let detached = loop {
            let flush_carry = filter.has_carry();
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Binary(b))) => {
                        let Some(f) = DataFrame::decode(&b) else { continue };
                        if f.pane_id != pane_id {
                            continue;
                        }
                        let (payload, end) = match f.kind {
                            DataKind::Snapshot => (&f.payload[..], f.offset),
                            DataKind::Output => {
                                let end = f.offset + f.payload.len() as u64;
                                match processed {
                                    Some(p) if end <= p => continue,
                                    Some(p) if f.offset < p => (&f.payload[(p - f.offset) as usize..], end),
                                    _ => (&f.payload[..], end),
                                }
                            }
                            DataKind::Input => continue,
                        };
                        filter.observe_output(payload);
                        let _ = out.write_all(payload);
                        let _ = out.flush();
                        processed = Some(end);
                        if end.saturating_sub(acked) >= ACK_EVERY {
                            acked = end;
                            let _ = sink.send(send(json!({ "t": "ack", "paneId": pane_id, "offset": end }))).await;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        match v["t"].as_str() {
                            Some("ping") => { let _ = sink.send(send(json!({ "t": "pong", "at": v["at"] }))).await; }
                            Some("error") => eprint!("\r\n[tmuxedo: {} {}]\r\n", v["code"], v["message"]),
                            Some("machine") if v["online"] == json!(false) => eprint!("\r\n[tmuxedo: machine went offline]\r\n"),
                            // A pane asked to open a web link (Claude's /login, …):
                            // a clickable line, or open it here with --open-urls.
                            Some("event") if v["type"] == "open_url" => {
                                let url = v["data"]["url"].as_str().unwrap_or("");
                                if crate::open_url::valid_url(url) {
                                    if open_urls && crate::open_url::open_locally(url).is_ok() {
                                        eprint!("\r\n[tmuxedo: opened {url}]\r\n");
                                    } else {
                                        eprint!("\r\n[tmuxedo: open {}]\r\n", crate::open_url::osc8(url));
                                    }
                                    // A login whose redirect goes to the machine's localhost.
                                    let cb = &v["data"]["callback"];
                                    if cb.is_object() {
                                        super::login_callback::remember(&mid, v["paneId"].as_str().unwrap_or(""), cb);
                                        eprint!("\r\n[tmuxedo: {}]\r\n", super::login_callback::hint(cb));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break false,
                    Some(Ok(_)) => {}
                },
                inp = in_rx.recv() => match inp {
                    Some(Input::Bytes(b)) => {
                        let mut send_bytes = vec![];
                        let mut detach = false;
                        for &c in &filter.filter_input(&b) {
                            if prefix {
                                prefix = false;
                                match c {
                                    b'd' => { detach = true; break; }
                                    PREFIX => send_bytes.push(PREFIX),
                                    other => { send_bytes.push(PREFIX); send_bytes.push(other); }
                                }
                            } else if c == PREFIX {
                                prefix = true;
                            } else {
                                send_bytes.push(c);
                            }
                        }
                        if control && !send_bytes.is_empty() {
                            let f = DataFrame { kind: DataKind::Input, gap: false, req: 0, pane_id: pane_id.clone(), offset: 0, payload: send_bytes };
                            if let Ok(enc) = f.encode() {
                                let _ = sink.send(Message::Binary(enc.into())).await;
                            }
                        }
                        if detach {
                            break true;
                        }
                    }
                    Some(Input::Eof) | None => break true,
                },
                _ = tokio::time::sleep(Duration::from_millis(30)), if flush_carry => {
                    let b = filter.flush();
                    if control && !b.is_empty() {
                        let f = DataFrame { kind: DataKind::Input, gap: false, req: 0, pane_id: pane_id.clone(), offset: 0, payload: b };
                        if let Ok(enc) = f.encode() {
                            let _ = sink.send(Message::Binary(enc.into())).await;
                        }
                    }
                }
                _ = winch.recv() => {
                    if control {
                        if let Some((c, r)) = term_size() {
                            let _ = sink.send(send(json!({ "t": "resize", "paneId": pane_id, "cols": c, "rows": r }))).await;
                        }
                    }
                }
                _ = ack_tick.tick() => {
                    if let Some(p) = processed.filter(|p| *p > acked) {
                        acked = p;
                        let _ = sink.send(send(json!({ "t": "ack", "paneId": pane_id, "offset": p }))).await;
                    }
                }
            }
        };
        if detached {
            let _ = sink.send(send(json!({ "t": "detach", "paneId": pane_id }))).await;
            let _ = sink.close().await;
            return Ok(());
        }
        eprint!("\r\n[tmuxedo: connection lost; reconnecting]\r\n");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
}
