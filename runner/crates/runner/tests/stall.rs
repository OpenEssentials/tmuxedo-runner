//! Audit P2-5: a stalled gateway cannot wedge the runner. The gateway
//! socket (with a tiny receive buffer, so it fills quickly) stops being
//! read while both directions are flooded: the gateway sends thousands of
//! `req`s whose replies are large (`server.agent_manifests`, ~45 KB each),
//! and an attached pane streams output. The daemon must keep
//! serving the local API and hooks throughout (its main loop also runs the
//! watchdog and detection), reset the stalled connection itself, and
//! reconnect and resync.

#![allow(clippy::result_large_err)]

mod common;

use common::{options, start_holder, H};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tmuxedo_runner::daemon;
use tokio::net::{TcpListener, TcpSocket};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const S: Duration = Duration::from_secs(60);
/// A wedge never answers; a busy debug build may take a few seconds.
const ANSWER: Duration = Duration::from_secs(10);

type Ws = WebSocketStream<tokio::net::TcpStream>;

fn listener() -> TcpListener {
    let s = TcpSocket::new_v4().unwrap();
    // Inherited by accepted sockets: the peer's window stays small.
    s.set_recv_buffer_size(4096).unwrap();
    s.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    s.listen(8).unwrap()
}

async fn accept(l: &TcpListener) -> Ws {
    let (tcp, _) = tokio::time::timeout(S, l.accept())
        .await
        .expect("runner connects")
        .unwrap();
    let cb = |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
              mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
        resp.headers_mut().insert(
            "sec-websocket-protocol",
            tokio_tungstenite::tungstenite::http::HeaderValue::from_static(tmuxedo_protocol::SUBPROTOCOL),
        );
        Ok(resp)
    };
    tokio_tungstenite::accept_hdr_async(tcp, cb).await.unwrap()
}

async fn text(ws: &mut Ws, pred: impl Fn(&Value) -> bool) -> Value {
    let until = Instant::now() + S;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, ws.next()).await.expect("frame in time") {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if pred(&v) {
                    return v;
                }
            }
            Some(Ok(_)) => {}
            other => panic!("socket ended: {other:?}"),
        }
    }
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

async fn handshake(ws: &mut Ws) {
    text(ws, |v| v["t"] == "hello").await;
    send(ws, json!({"t": "welcome", "machineId": "m1", "userId": "u1"})).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_gateway_cannot_wedge_the_runner() {
    let dir = common::tempdir();
    let holder = start_holder(dir.path());
    let l = listener();
    let url = format!("ws://{}/gw/runner", l.local_addr().unwrap());
    let mut o = options(dir.path(), &url, holder);
    // Tight limits so the stall is seen within seconds even in a debug build
    // (production: 10 s / 8 MiB); the mechanism is the same.
    o.gw_stall = Duration::from_secs(1);
    o.gw_overflow_max = 256 * 1024;
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c1 = accept(&l).await;
    handshake(&mut c1).await;
    text(&mut c1, |v| v["t"] == "topology").await;
    let h = H::attach_to(&sock).await;
    h.wait_alive("w1:p1").await;

    // Live output on an attached pane (a modest, steady stream).
    h.ok(
        "pane.send_text",
        json!({"pane_id": "w1:p1", "text": "while :; do echo tmuxedo-stall-$((i=i+1)); sleep 0.01; done\n"}),
    )
    .await;
    send(&mut c1, json!({"t": "attach", "req": 1, "paneId": "w1:p1"})).await;
    // Inbound flood; from now on nothing reads c1.
    let flood = tokio::spawn(async move {
        for i in 0..3000 {
            let req = json!({"t": "req", "id": format!("r{i}"), "method": "server.agent_manifests", "params": {}});
            if tokio::time::timeout(Duration::from_secs(5), c1.send(Message::Text(req.to_string().into())))
                .await
                .is_err()
            {
                break; // the runner stopped reading us too: fine
            }
        }
        c1 // kept open, never read
    });

    // The daemon stays responsive the whole time.
    let t0 = Instant::now();
    let mut checks = 0;
    while t0.elapsed() < Duration::from_secs(12) {
        let r = tokio::time::timeout(ANSWER, h.raw("ping", json!({}))).await;
        assert!(
            r.is_ok_and(|v| v["result"]["type"] == "pong"),
            "local API stalled after {checks} checks"
        );
        let hook = json!({"paneId": "w1:p1", "event": {"hook_event_name": "Notification", "session_id": "s", "notification_type": "other"}});
        let r = tokio::time::timeout(ANSWER, h.raw("hook.event", hook)).await;
        assert!(r.is_ok(), "hook stalled after {checks} checks");
        checks += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(checks >= 5, "only {checks} checks");

    // The runner dropped the stalled socket by itself and reconnects.
    let mut c2 = accept(&l).await;
    handshake(&mut c2).await;
    let topo = text(&mut c2, |v| v["t"] == "topology").await;
    assert_eq!(topo["panes"][0]["id"], "w1:p1");
    send(
        &mut c2,
        json!({"t": "req", "id": "after", "method": "ping", "params": {}}),
    )
    .await;
    let res = text(&mut c2, |v| v["t"] == "res" && v["id"] == "after").await;
    assert_eq!(res["result"]["type"], "pong");
    h.ok("pane.send_keys", json!({"pane_id": "w1:p1", "keys": ["C-c"]}))
        .await;
    let _c1 = flood.await.unwrap();
    let _ = c2.close(None).await;
    task.abort();
}
