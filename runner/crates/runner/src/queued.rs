//! Queued prompts (docs/design/away-from-desk.md D-AD6, docs/protocol.md
//! "Queued prompts").
//!
//! A prompt the owner typed while this machine was offline waits at web as
//! ciphertext sealed to this runner's key. When the machine is back, web calls
//! `queued.deliver {id, sealed}`. The runner opens it under
//! `<machineId>|queued:<id>`, reads `{text, target, expiresAt}` from inside the
//! seal (so web cannot redirect or extend it), checks the target and the
//! expiry, delivers it once and answers. It never types into a pane with an
//! open decision: that answers `queued_busy` and web tries again later. A
//! chat with an open decision or a running turn queues it behind them, as
//! `chat.send` does.
//!
//! Exactly once: the ids delivered are kept (the last [`REMEMBERED`]) in
//! `<state>/queued-delivered.json`, so a resend after a lost answer or a
//! runner restart answers `duplicate` and delivers nothing.

use crate::api::{err, respond, ApiErr, ApiResult, ReplyTo};
use crate::daemon::Daemon;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use tmuxedo_protocol::queued::{context, valid_id, MAX_SEALED_BYTES, MAX_TEXT_BYTES, REMEMBERED};

pub const METHODS: &[&str] = &[tmuxedo_protocol::queued::METHOD_DELIVER];

/// The ids delivered so far (oldest first), persisted.
#[derive(Debug, Default)]
pub struct Delivered {
    path: Option<PathBuf>,
    ids: VecDeque<(String, Value)>,
}

impl Delivered {
    pub fn load(state_dir: &Path) -> Delivered {
        let path = state_dir.join("queued-delivered.json");
        let ids = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<(String, Value)>>(&b).ok())
            .unwrap_or_default();
        let mut d = Delivered {
            path: Some(path),
            ids: ids.into(),
        };
        d.trim();
        d
    }

    fn trim(&mut self) {
        while self.ids.len() > REMEMBERED {
            self.ids.pop_front();
        }
    }

    pub fn get(&self, id: &str) -> Option<&Value> {
        self.ids.iter().find(|(i, _)| i == id).map(|(_, v)| v)
    }

    fn add(&mut self, id: &str, answer: Value) {
        self.ids.push_back((id.to_string(), answer));
        self.trim();
        if let Some(p) = &self.path {
            let v: Vec<&(String, Value)> = self.ids.iter().collect();
            if let Ok(b) = serde_json::to_vec(&v) {
                let tmp = p.with_extension("json.tmp");
                if std::fs::write(&tmp, b).is_ok() {
                    let _ = std::fs::rename(&tmp, p);
                }
            }
        }
    }
}

/// What the seal holds.
#[derive(Debug, PartialEq)]
pub enum Target {
    Chat(String),
    Pane(String),
}

#[derive(Debug)]
pub struct Opened {
    pub text: String,
    pub target: Target,
    pub expires_at: Option<u64>,
}

/// Parse the opened plaintext `{v: 1, text, target: {chat} | {pane}, expiresAt?}`.
pub fn parse(plain: &[u8]) -> Result<Opened, ApiErr> {
    let bad = || err("queued_unreadable", "the queued prompt is not readable");
    let v: Value = serde_json::from_slice(plain).map_err(|_| bad())?;
    if v["v"].as_u64() != Some(1) {
        return Err(bad());
    }
    let text = v["text"].as_str().ok_or_else(bad)?.to_string();
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
        return Err(bad());
    }
    let t = &v["target"];
    let target = match (t["chat"].as_str(), t["pane"].as_str()) {
        (Some(c), None) if tmuxedo_protocol::chat::valid_session_id(c) => Target::Chat(c.to_string()),
        (None, Some(p)) if !p.is_empty() && p.len() <= 255 => Target::Pane(p.to_string()),
        _ => return Err(bad()),
    };
    let expires_at = match &v["expiresAt"] {
        Value::Null => None,
        x => Some(x.as_u64().ok_or_else(bad)?),
    };
    Ok(Opened {
        text,
        target,
        expires_at,
    })
}

impl Daemon {
    pub(crate) async fn queued_request(&mut self, method: &str, p: &Value, reply: ReplyTo) {
        let r = match method {
            tmuxedo_protocol::queued::METHOD_DELIVER => self.queued_deliver(p).await,
            m => Err(err("unknown_method", format!("no method {m}"))),
        };
        respond(reply, r, self);
    }

    async fn queued_deliver(&mut self, p: &Value) -> ApiResult {
        let id = p["id"]
            .as_str()
            .filter(|i| valid_id(i))
            .ok_or_else(|| err("invalid_params", "id is required"))?;
        let sealed = p["sealed"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= MAX_SEALED_BYTES)
            .ok_or_else(|| err("invalid_params", "sealed is required"))?;
        if let Some(first) = self.queued.get(id) {
            let mut a = first.clone();
            a["duplicate"] = json!(true);
            return Ok(a);
        }
        let key = self
            .delivery
            .key
            .as_ref()
            .ok_or_else(|| err("queued_unreadable", "this machine has no runner key"))?;
        let plain = key
            .open(sealed, &context(&self.delivery.machine, id))
            .map_err(|_| err("queued_unreadable", "the queued prompt does not open on this machine"))?;
        let q = parse(&plain)?;
        if q.expires_at.is_some_and(|e| crate::util::now_ms() > e) {
            return Err(err(
                "queued_expired",
                "the queued prompt ran out before the machine was back",
            ));
        }
        let answer = match &q.target {
            Target::Chat(sid) => {
                if !self.chats.sessions.contains_key(sid) {
                    return Err(err("session_ended", "the chat is gone"));
                }
                let r = self.chat_send_queued(sid, &q.text, &format!("q:{id}")).await;
                let r = r.map_err(|e| match e.code.as_str() {
                    "chat_not_found" | "chat_closed" | "chat_in_terminal" => err("session_ended", e.message),
                    _ => e,
                })?;
                json!({ "type": "queued_delivered", "id": id, "to": "chat", "seq": r["seq"], "queued": r["queued"] })
            }
            Target::Pane(pane) => {
                if self.state.pane(pane).is_none() {
                    return Err(err("session_ended", "the pane is gone"));
                }
                if self.needs.open.get(pane.as_str()).is_some_and(|v| !v.is_empty())
                    || self.prompts.values().any(|h| h.pane_id == *pane)
                {
                    return Err(err(
                        "queued_busy",
                        "the agent is waiting for a decision; the prompt waits too",
                    ));
                }
                let hid = self
                    .hid(pane)
                    .map_err(|_| err("session_ended", "the pane has no running process"))?;
                let mut bytes = q.text.into_bytes();
                bytes.push(b'\r');
                self.holder.input(&hid, &bytes);
                json!({ "type": "queued_delivered", "id": id, "to": "pane" })
            }
        };
        let mut stored = answer.clone();
        stored["at"] = json!(crate::util::iso_now());
        self.queued.add(id, stored.clone());
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_only_the_known_shape() {
        let ok = parse(br#"{"v":1,"text":"hi","target":{"chat":"abcdefgh12"},"expiresAt":5}"#).unwrap();
        assert_eq!(ok.target, Target::Chat("abcdefgh12".into()));
        assert_eq!(ok.expires_at, Some(5));
        let pane = parse(br#"{"v":1,"text":"ls","target":{"pane":"w1:p1"}}"#).unwrap();
        assert_eq!(pane.target, Target::Pane("w1:p1".into()));
        for bad in [
            &br#"{"v":2,"text":"hi","target":{"chat":"abcdefgh12"}}"#[..],
            br#"{"v":1,"text":"  ","target":{"chat":"abcdefgh12"}}"#,
            br#"{"v":1,"text":"hi","target":{"chat":"abcdefgh12","pane":"w1:p1"}}"#,
            br#"{"v":1,"text":"hi","target":{"chat":"bad id"}}"#,
            br#"{"v":1,"text":"hi","target":{"pane":""}}"#,
            br#"{"v":1,"text":"hi","target":{"pane":"w1:p1"},"expiresAt":"soon"}"#,
            b"junk",
        ] {
            assert_eq!(parse(bad).unwrap_err().code, "queued_unreadable");
        }
        let big = format!(
            r#"{{"v":1,"text":"{}","target":{{"pane":"p"}}}}"#,
            "a".repeat(MAX_TEXT_BYTES + 1)
        );
        assert!(parse(big.as_bytes()).is_err());
    }

    #[test]
    fn delivered_ids_are_capped_and_survive_a_reload() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Delivered::load(d.path());
        for i in 0..REMEMBERED + 5 {
            a.add(&format!("id{i:08}"), json!({ "n": i }));
        }
        let b = Delivered::load(d.path());
        assert_eq!(b.ids.len(), REMEMBERED);
        assert!(b.get("id00000000").is_none(), "the oldest are forgotten");
        assert_eq!(b.get(&format!("id{:08}", REMEMBERED + 4)).unwrap()["n"], REMEMBERED + 4);
    }
}
