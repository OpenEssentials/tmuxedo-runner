//! One gateway client socket per machine, as a task: ticket (with the CLI
//! token) → `/gw/client` → `hello` → frames both ways, until the app drops
//! it. On loss it reports `Down` and reconnects with backoff and a fresh
//! ticket (the app re-attaches with `from` when the next `welcome` comes);
//! close 4003 (revoked) or a refused ticket stops it for good.

use super::app::Out;
use crate::cli::attach::{dial, fetch_ticket};
use crate::cli::CliConfig;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_protocol::close::REVOKED;
use tmuxedo_protocol::DataFrame;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug)]
pub enum NetEvent {
    Machines(Result<Vec<Value>, String>),
    Text(String, Value),
    Data(String, DataFrame),
    /// A CHAT frame (binary kind 4) for the chat screen.
    Chat(String, tmuxedo_protocol::chat::ChatFrame),
    Down(String),
    Stopped(String, String),
    ControlRefused(String, String),
    /// `GET /api/machines/{id}/plugins/effective` answered (super::agent_settings).
    AgentSettings(String, Result<Value, String>),
    /// `GET /api/me/layouts` (the account's layout favourites).
    Favourites(Result<Value, String>),
}

/// What the event loop sends a machine's socket task.
#[derive(Debug)]
pub enum Cmd {
    Send(Out),
    Close,
}

pub struct Conn {
    pub control: bool,
    pub tx: UnboundedSender<Cmd>,
    pub task: tokio::task::JoinHandle<()>,
}

pub fn spawn(cfg: CliConfig, mid: String, control: bool, ev: UnboundedSender<NetEvent>) -> Conn {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(conn_task(cfg, mid, control, rx, ev));
    Conn { control, tx, task }
}

fn text(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

async fn conn_task(
    cfg: CliConfig,
    mid: String,
    control: bool,
    mut rx: UnboundedReceiver<Cmd>,
    ev: UnboundedSender<NetEvent>,
) {
    let scope = if control { "control" } else { "observe" };
    let mut backoff = Duration::from_millis(500);
    loop {
        let ws = match fetch_ticket(&cfg, &mid, scope).await {
            Ok((200, v)) => match v["ticket"].as_str() {
                Some(t) => dial(&cfg, t).await,
                None => Err("ticket missing".into()),
            },
            Ok((403, v)) if control => {
                let why = v["error"]["code"].as_str().unwrap_or("forbidden").to_string();
                let _ = ev.send(NetEvent::ControlRefused(mid, why));
                return;
            }
            Ok((code @ (401 | 403 | 404), v)) => {
                let why = v["error"]["message"].as_str().unwrap_or("refused").to_string();
                let _ = ev.send(NetEvent::Stopped(mid, format!("ticket refused ({code}): {why}")));
                return;
            }
            Ok((code, _)) => Err(format!("ticket: HTTP {code}")),
            Err(e) => Err(e),
        };
        let ws = match ws {
            Ok(ws) => ws,
            Err(_) => {
                if wait_or_close(&mut rx, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(15));
                continue;
            }
        };
        // Anything queued while down was for the old socket; the app re-attaches on `welcome`.
        while rx.try_recv().is_ok() {}
        let (mut sink, mut stream) = ws.split();
        let hello = json!({ "t": "hello", "client": { "kind": "tui", "version": crate::VERSION } });
        if sink.send(text(hello)).await.is_err() {
            let _ = ev.send(NetEvent::Down(mid.clone()));
            continue;
        }
        backoff = Duration::from_millis(500);
        let mut ping = tokio::time::interval(Duration::from_secs(25));
        ping.tick().await;
        let code = loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        match v["t"].as_str() {
                            Some("ping") => { let _ = sink.send(text(json!({ "t": "pong", "at": v["at"] }))).await; }
                            Some("pong") => {}
                            _ => { let _ = ev.send(NetEvent::Text(mid.clone(), v)); }
                        }
                    }
                    Some(Ok(Message::Binary(b))) => {
                        if let Some(f) = DataFrame::decode(&b) {
                            let _ = ev.send(NetEvent::Data(mid.clone(), f));
                        } else if let Some(f) = tmuxedo_protocol::chat::ChatFrame::decode(&b) {
                            let _ = ev.send(NetEvent::Chat(mid.clone(), f));
                        }
                    }
                    Some(Ok(Message::Close(c))) => break c.map(|c| u16::from(c.code)),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break None,
                },
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Send(Out::Text(v))) => { let _ = sink.send(text(v)).await; }
                    Some(Cmd::Send(Out::Data(f))) => {
                        if let Ok(b) = f.encode() {
                            let _ = sink.send(Message::Binary(b.into())).await;
                        }
                    }
                    Some(Cmd::Close) | None => {
                        let _ = sink.flush().await;
                        let _ = sink.close().await;
                        return;
                    }
                },
                _ = ping.tick() => {
                    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                    let _ = sink.send(text(json!({ "t": "ping", "at": at }))).await;
                }
            }
        };
        if code == Some(REVOKED) {
            let _ = ev.send(NetEvent::Stopped(mid, "revoked".into()));
            return;
        }
        let _ = ev.send(NetEvent::Down(mid.clone()));
        if wait_or_close(&mut rx, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
}

/// Sleep `d`, dropping queued sends; true if told to close meanwhile.
async fn wait_or_close(rx: &mut UnboundedReceiver<Cmd>, d: Duration) -> bool {
    let until = tokio::time::Instant::now() + d;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => return false,
            c = rx.recv() => match c {
                Some(Cmd::Close) | None => return true,
                Some(Cmd::Send(_)) => {}
            }
        }
    }
}
