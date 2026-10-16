//! The chat screen's glue to the app (super::chat): opening a chat, its
//! frames, its requests, and chat ⇄ terminal. Everything goes over the
//! machine's gateway client socket the TUI already holds: `chat_attach` /
//! `chat_detach`, CHAT frames, and `req chat.*` like any other method. The
//! gateway removes `fresh_stepup` from every socket request, so a high-risk
//! allow is never sent from here (docs/protocol.md "Agent chat").
//!
//! Keys, behind the prefix: `C` opens the focused pane's agent as a chat
//! (`chat.handoff {to: chat, pane_id}`), and in a chat hands it back to a
//! terminal (`chat.handoff {to: terminal}`; when the chat is already in a
//! terminal it brings it back); `q` leaves the chat screen (the chat goes on).

use super::app::{App, Out};
use super::chat::{ChatScreen, Cmd, Link};
use ratatui::crossterm::event::KeyEvent;
use serde_json::{json, Value};
use tmuxedo_protocol::chat::ChatFrame;

#[derive(Debug, Clone)]
pub enum ChatReq {
    /// `chat.handoff {to: chat, pane_id}`: open the result.
    Open,
    /// `chat.handoff {to: terminal}`: go to the pane.
    Terminal,
    /// `chat.handoff {to: chat, session_id}`.
    Back,
    Send,
    Interrupt,
    /// The decision's id and title.
    Respond(String, String),
}

impl App {
    /// The clock for running timers.
    pub fn chat_clock(&self) -> Option<u64> {
        self.chat_now.or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as u64)
        })
    }

    /// Show chat `sid` of the selected machine and start watching it.
    pub fn chat_open(&mut self, sid: &str) {
        let Some(mid) = self.machine().map(|m| m.id.clone()) else {
            return;
        };
        if self.chat.as_ref().is_some_and(|c| c.sid == sid && c.mid == mid) {
            return;
        }
        self.chat_close();
        let mut c = ChatScreen::new(&mid, sid, self.chat_color);
        c.detail = self.chat_detail.clone();
        self.out.push((mid, Out::Text(c.attach_frame())));
        self.chat = Some(c);
        self.mode = super::app::Mode::Normal;
    }

    /// Leave the chat screen; the chat goes on.
    pub fn chat_close(&mut self) {
        if let Some(c) = self.chat.take() {
            self.out
                .push((c.mid, Out::Text(json!({ "t": "chat_detach", "sessionId": c.sid }))));
        }
    }

    /// After a reconnect: attach again from the event after the last one held.
    pub fn chat_relink(&mut self, mid: &str) {
        if let Some(c) = self.chat.as_mut().filter(|c| c.mid == mid) {
            c.link = Link::Attaching;
            let f = c.attach_frame();
            self.out.push((mid.to_string(), Out::Text(f)));
        }
    }

    pub fn on_chat_attached(&mut self, mid: &str, v: &Value) {
        if let Some(c) = self
            .chat
            .as_mut()
            .filter(|c| c.mid == mid && v["sessionId"] == c.sid.as_str())
        {
            c.on_attached(v);
        }
    }

    pub fn on_chat(&mut self, mid: &str, f: &ChatFrame) {
        if let Some(c) = self.chat.as_mut().filter(|c| c.mid == mid) {
            c.on_frame(f);
        }
    }

    /// `prefix C`.
    pub fn chat_toggle(&mut self) {
        if let Some(c) = &self.chat {
            let sid = c.sid.clone();
            if c.state.in_terminal.is_some() {
                self.request_chat(
                    "chat.handoff",
                    json!({ "to": "chat", "session_id": sid }),
                    ChatReq::Back,
                );
            } else {
                self.request_chat(
                    "chat.handoff",
                    json!({ "to": "terminal", "session_id": sid }),
                    ChatReq::Terminal,
                );
            }
            return;
        }
        let Some(pane) = self.focus.clone() else {
            self.note = Some("no pane to open as a chat".into());
            return;
        };
        let offer = self
            .topo()
            .and_then(|t| t.pane(&pane))
            .and_then(|p| p.agent.as_ref())
            .map(|a| a.chat.clone());
        match offer {
            Some(Some(_)) => self.request_chat("chat.handoff", json!({ "to": "chat", "pane_id": pane }), ChatReq::Open),
            Some(None) => self.note = Some("this agent cannot open as a chat (its plugin or login has no chat)".into()),
            None => self.note = Some("no agent in this pane".into()),
        }
    }

    pub fn chat_key(&mut self, k: KeyEvent) {
        let now = self.chat_clock();
        let Some(c) = self.chat.as_mut() else { return };
        let cmds = c.on_key(k, now);
        for cmd in cmds {
            self.chat_cmd(cmd);
        }
    }

    fn chat_cmd(&mut self, cmd: Cmd) {
        let Some(c) = &self.chat else { return };
        let sid = c.sid.clone();
        if self.machine().map(|m| &m.id) != Some(&c.mid) {
            self.note = Some("select the chat's machine first".into());
            return;
        }
        match cmd {
            Cmd::Send(text) => {
                let id = format!("tui-{}-{}", c.state.last_seq, text.len());
                self.request_chat(
                    "chat.send",
                    json!({ "session_id": sid, "text": text, "client_msg_id": id }),
                    ChatReq::Send,
                );
            }
            Cmd::Interrupt => {
                self.request_chat("chat.interrupt", json!({ "session_id": sid }), ChatReq::Interrupt);
                self.note = Some("stopping…".into());
            }
            Cmd::Respond { need, response } => {
                let title = c.state.needs.get(&need).map(|n| n.title.clone()).unwrap_or_default();
                self.request_chat(
                    "chat.respond",
                    json!({ "session_id": sid, "need_id": need, "response": response }),
                    ChatReq::Respond(need, title),
                );
            }
            Cmd::Copy(text) => {
                let (b, n, cut) = super::keys::osc52(&text, self.in_tmux);
                self.local_out.push(b);
                self.note = Some(format!("copied {n} chars{}", if cut { " (cut)" } else { "" }));
            }
        }
        if !self.can_control() {
            // `request` said "needs control"; an answer typed here is not lost.
            if let Some(c) = self.chat.as_mut() {
                c.answered.clear();
            }
        }
    }

    pub(super) fn on_chat_res(&mut self, req: ChatReq, v: &Value) {
        let err = v.get("error").filter(|e| !e.is_null());
        if let Some(e) = err {
            let code = e["code"].as_str().unwrap_or("error");
            let msg = super::chat_model::safe_line(e["message"].as_str().unwrap_or(""), 200);
            self.note = Some(match (&req, code) {
                (ChatReq::Respond(..), "stepup_required") => "high risk: answer this on the web or phone".into(),
                (ChatReq::Terminal, "turn_running") => "available when the turn ends (Esc stops it)".into(),
                (ChatReq::Terminal, "chat_not_started") => {
                    "send a first message before opening it in a terminal".into()
                }
                (ChatReq::Open | ChatReq::Back, "agent_busy") => "available when the agent finishes in its pane".into(),
                (ChatReq::Send, "chat_in_terminal") => "this chat is in a terminal: prefix C brings it back".into(),
                _ => format!("{code}: {msg}"),
            });
            if let (ChatReq::Respond(id, _), Some(c)) = (&req, self.chat.as_mut()) {
                c.answered.retain(|x| x != id);
            }
            return;
        }
        let r = &v["result"];
        match req {
            ChatReq::Open => match r["session"]["id"].as_str() {
                Some(sid) => self.chat_open(sid),
                None => self.note = Some("the runner did not name the chat".into()),
            },
            ChatReq::Terminal => {
                let pane = r["paneId"].as_str().map(str::to_owned);
                self.chat_close();
                self.note = Some("the chat continues in a terminal pane (prefix C brings it back)".into());
                // Focused when its workspace reaches the topology.
                if let Some(p) = pane {
                    self.focus_pane_anywhere(&p);
                }
            }
            ChatReq::Back => self.note = Some("back in the chat".into()),
            ChatReq::Send if r["queued"] == true => self.note = Some("queued: sent when this turn ends".into()),
            ChatReq::Respond(_, title) if r["outcome"] == "stale" => {
                self.note = Some(format!(
                    "“{}” was already answered or expired",
                    super::chat_model::safe_line(&title, 60)
                ))
            }
            _ => {}
        }
    }
}
