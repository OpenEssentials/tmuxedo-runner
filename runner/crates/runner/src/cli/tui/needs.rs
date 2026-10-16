//! Needs you in the TUI (PLAN Phase 13, multi-agent-workflows.md §2.3): the
//! machine's `needs` frames (relayed by the gateway) kept per machine; the
//! sidebar shows how many items wait (`■ n`); `prefix !` (or `prefix N`,
//! which the prefix overlay never takes) opens the list, where
//!
//!   ↑↓ choose · a approve · d deny / cancel · r reply · 1–9 a choice ·
//!   Enter go to the pane · Esc close
//!
//! Every answer is `req needs.respond` over the control socket (the gateway
//! checks it is the machine's owner, never takes a client's `fresh_stepup`,
//! and audits it), so a high-risk approve is refused here with "approve it
//! on the web or in the app" (a fresh step-up). Items that only open the
//! terminal or a chat say so.

use super::app::{App, Machine, Mode};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tmuxedo_protocol::needs::{parse_item, NeedsYou};

/// One machine's open sets, by pane ("" = machine-level: chats, logins).
#[derive(Debug, Clone, Default)]
pub struct NeedsState {
    pub sets: BTreeMap<String, Vec<NeedsYou>>,
}

impl NeedsState {
    /// A frame from the machine's socket.
    pub fn observe(&mut self, v: &Value) {
        match v["t"].as_str() {
            Some("needs") => {
                let Some(pane) = v["paneId"].as_str() else { return };
                let items: Vec<NeedsYou> = v["items"]
                    .as_array()
                    .map(|a| a.iter().filter_map(parse_item).filter(|i| i.pane_id == pane).collect())
                    .unwrap_or_default();
                if items.is_empty() {
                    self.sets.remove(pane);
                } else {
                    self.sets.insert(pane.to_string(), items);
                }
            }
            // The gateway replays the current sets after welcome; a machine that went away has none.
            Some("welcome") => self.sets.clear(),
            Some("machine") if v["online"] == false => self.sets.clear(),
            _ => {}
        }
    }

    /// Every item, oldest first.
    pub fn items(&self) -> Vec<&NeedsYou> {
        let mut v: Vec<&NeedsYou> = self.sets.values().flatten().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        v
    }

    pub fn count(&self) -> usize {
        self.sets.values().map(Vec::len).sum()
    }
}

/// The sidebar's count for a machine: `■ 2`, or nothing.
pub fn badge(m: &Machine) -> Option<String> {
    let n = m.needs.count();
    (n > 0).then(|| format!("■ {n}"))
}

/// One list line: where, what, and the keys it takes.
pub fn line(app: &App, it: &NeedsYou) -> String {
    let pane = if it.chat_id.is_some() {
        "chat".to_string()
    } else if it.pane_id.is_empty() {
        "machine".to_string()
    } else {
        app.topo()
            .and_then(|t| t.pane(&it.pane_id))
            .and_then(|p| {
                p.label
                    .clone()
                    .filter(|l| !l.is_empty())
                    .or_else(|| Some(p.title.clone()))
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| it.pane_id.clone())
    };
    let risk = if it.risk == "high" { " ⚠ high risk" } else { "" };
    format!("{} · {pane}: {}{risk}  [{}]", it.plugin, it.title, keys_of(it))
}

/// The keys an item takes in the list.
pub fn keys_of(it: &NeedsYou) -> String {
    let mut k = vec![];
    for r in &it.responses {
        match r.kind.as_str() {
            "approve" if it.risk == "high" => k.push("approve on web/app".to_string()),
            "approve" => k.push("a approve".into()),
            "deny" if it.kind == "question" => k.push("d cancel".into()),
            "deny" => k.push("d deny".into()),
            "reply" => k.push("r reply".into()),
            "choose" => {
                let opts = r.options.as_deref().unwrap_or(&[]);
                k.push(
                    opts.iter()
                        .map(|o| format!("{} {}", o.id, o.label))
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
            "action" => k.push(format!("s {}", r.label.to_lowercase())),
            "open" => k.push("enter open".into()),
            // A file request (`tmuxedo files request`): the file comes from the web, the app or `tmuxedo upload`.
            "upload" => k.push("upload on web/app".into()),
            "dismiss" if it.kind == "file" => k.push("x not now".into()),
            _ => {}
        }
    }
    k.join(" · ")
}

impl App {
    /// The selected machine's items, oldest first.
    pub fn needs_items(&self) -> Vec<NeedsYou> {
        self.machine()
            .map(|m| m.needs.items().into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// `prefix !` / `prefix N`.
    pub fn needs_open(&mut self) {
        if self.needs_items().is_empty() {
            self.note = Some("Nothing needs you on this machine.".into());
            return;
        }
        self.mode = Mode::Needs(0);
    }

    /// A key in the list.
    pub fn needs_key(&mut self, cursor: usize, k: KeyEvent) {
        let items = self.needs_items();
        if items.is_empty() {
            self.mode = Mode::Normal;
            return;
        }
        let cursor = cursor.min(items.len() - 1);
        let it = items[cursor].clone();
        self.mode = Mode::Needs(cursor);
        let by_kind = |kind: &str| it.responses.iter().find(|r| r.kind == kind).cloned();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Up => self.mode = Mode::Needs(cursor.saturating_sub(1)),
            KeyCode::Down => self.mode = Mode::Needs((cursor + 1).min(items.len() - 1)),
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if !it.pane_id.is_empty() {
                    self.focus_pane_anywhere(&it.pane_id);
                } else if let Some(c) = &it.chat_id {
                    // The chat screen (super::chat) answers it in place.
                    self.chat_open(c);
                }
            }
            KeyCode::Char('a') => match by_kind("approve") {
                Some(_) if it.risk == "high" => {
                    self.note =
                        Some("High risk: approve it on the web or in the app (it asks you to confirm it’s you).".into())
                }
                Some(r) => self.needs_send(&it, &r.id, None, None),
                None => self.note = Some("Nothing to approve here.".into()),
            },
            KeyCode::Char('d') => match by_kind("deny") {
                Some(r) => self.needs_send(&it, &r.id, None, None),
                None => self.note = Some("Nothing to deny here.".into()),
            },
            KeyCode::Char('x') => match by_kind("dismiss").filter(|_| it.kind == "file") {
                Some(r) => self.needs_send(&it, &r.id, None, None),
                None => self.note = Some("Nothing to turn down here.".into()),
            },
            KeyCode::Char('s') => {
                if let Some(r) = by_kind("action") {
                    self.needs_send(&it, &r.id, None, None);
                }
            }
            KeyCode::Char('r') => {
                if by_kind("reply").is_some() || (it.kind == "permission" && by_kind("deny").is_some()) {
                    self.mode = Mode::Reply(it.id.clone(), String::new());
                } else {
                    self.note = Some("This one takes no reply.".into());
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let choose = by_kind("choose");
                let id = c.to_string();
                match choose.filter(|r| r.options.as_ref().is_some_and(|o| o.iter().any(|x| x.id == id))) {
                    Some(r) => self.needs_send(&it, &r.id, Some(id), None),
                    None => self.note = Some(format!("{c} is not a choice here.")),
                }
            }
            _ => {}
        }
    }

    /// Typing a reply (`r`): Enter sends, Esc goes back to the list.
    pub fn reply_key(&mut self, item: String, mut buf: String, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => self.mode = Mode::Needs(0),
            KeyCode::Enter => {
                self.mode = Mode::Needs(0);
                let text = buf.trim().to_string();
                let Some(it) = self.needs_items().into_iter().find(|i| i.id == item) else {
                    self.note = Some("Already answered or expired.".into());
                    return;
                };
                if text.is_empty() {
                    return;
                }
                // A question's reply is typed in; a permission's goes with a deny.
                let r = it
                    .responses
                    .iter()
                    .find(|r| r.kind == "reply")
                    .or_else(|| it.responses.iter().find(|r| r.kind == "deny"))
                    .map(|r| r.id.clone());
                if let Some(r) = r {
                    self.needs_send(&it, &r, None, Some(text));
                }
            }
            KeyCode::Backspace => {
                buf.pop();
                self.mode = Mode::Reply(item, buf);
            }
            KeyCode::Char(c) if !c.is_control() && buf.chars().count() < 2000 => {
                buf.push(c);
                self.mode = Mode::Reply(item, buf);
            }
            _ => self.mode = Mode::Reply(item, buf),
        }
    }

    fn needs_send(&mut self, it: &NeedsYou, response: &str, option: Option<String>, text: Option<String>) {
        let mut p = json!({ "itemId": it.id, "responseId": response });
        if !it.pane_id.is_empty() {
            p["paneId"] = json!(it.pane_id);
        }
        if let Some(c) = &it.chat_id {
            p["chatId"] = json!(c);
        }
        if let Some(o) = option {
            p["optionId"] = json!(o);
        }
        if let Some(t) = text {
            p["text"] = json!(t);
        }
        p["by"] = json!("TUI");
        self.request_needs(p, it.title.clone());
    }

    /// The status line while typing a reply.
    pub fn reply_line(&self, item: &str, buf: &str) -> String {
        let what = self
            .needs_items()
            .into_iter()
            .find(|i| i.id == item)
            .map(|i| i.title)
            .unwrap_or_default();
        format!("reply to “{what}” (enter sends, esc back): {buf}▏")
    }
}

/// What the answer's outcome says on the status line.
pub fn outcome_note(what: &str, result: &Value) -> String {
    match result["outcome"].as_str() {
        Some("accepted") => format!("answered: {what}"),
        Some("stale") => format!("not sent: “{what}” was already answered, expired, or the screen changed"),
        _ => format!("sent: “{what}” (the machine did not confirm yet)"),
    }
}

#[cfg(test)]
mod tests {
    use super::keys_of;
    use tmuxedo_protocol::needs::NeedsYou;

    fn fixture(name: &str) -> NeedsYou {
        let f: serde_json::Value =
            serde_json::from_str(include_str!("../../../../../../packages/protocol/fixtures/needs.json")).unwrap();
        serde_json::from_value(f["items"]["valid"][name].clone()).unwrap()
    }

    #[test]
    fn a_file_request_says_where_to_upload_and_x_turns_it_down() {
        assert_eq!(keys_of(&fixture("file_request")), "upload on web/app · x not now");
        // Other items keep their keys; `x` is only a file request's.
        assert_eq!(keys_of(&fixture("permission_hook")), "a approve · d deny");
    }
}
