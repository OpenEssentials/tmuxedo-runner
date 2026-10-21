//! Keep-awake and Wake-on-LAN in the terminal client (docs/protocol.md
//! "Keep-awake and Wake-on-LAN"), the same facts the web card and the phone
//! show: `GET /api/machines` says per machine how it can be woken
//! (`wake.relay.name`, or why not) and whether it is being kept awake for
//! working agents. The sidebar shows ◐ on a machine kept awake; an offline
//! machine says "prefix W wakes it via <relay>"; `prefix W` posts
//! `POST /api/v1/machines/{id}/wake` in the background and the status line
//! says what happened, in the web card's words.

use super::app::{App, Machine};
use serde_json::Value;

/// The web's line when nothing can wake it (lib/wake-offer.ts NO_RELAY_TEXT).
pub const NO_RELAY_TEXT: &str = "Can't wake it: no machine on its network is online.";

/// The relay `GET /api/machines` names for an offline own computer.
pub fn wake_via(it: &Value) -> Option<String> {
    it["wake"]["relay"]["name"]
        .as_str()
        .map(clean)
        .filter(|s| !s.is_empty())
}

/// Nothing can wake it now (`wake.block == "no_relay"`).
pub fn wake_blocked(it: &Value) -> bool {
    it["wake"]["block"].as_str() == Some("no_relay")
}

/// Agents working on a machine kept awake for them (lib/wake-offer.ts
/// keptAwakeAgents): on, available, online and at least one working.
pub fn kept_awake(it: &Value) -> Option<u64> {
    let on = it["online"].as_bool() == Some(true)
        && it["keepAwake"].as_bool() == Some(true)
        && it["keepAwakeAvailable"].as_bool() == Some(true);
    let n = it["agentsWorking"].as_u64().unwrap_or(0);
    (on && n > 0).then_some(n)
}

/// The sidebar mark, or none.
pub fn badge(m: &Machine) -> Option<String> {
    if !m.online {
        return None;
    }
    m.kept_awake.map(|n| format!("◐{n}"))
}

/// What the panes area says while the machine is offline.
pub fn offline_text(m: &Machine) -> String {
    match (&m.wake_via, m.wake_blocked) {
        (Some(via), _) => format!("machine offline: prefix W wakes it via {via}"),
        (None, true) => format!("machine offline: {NO_RELAY_TEXT}"),
        (None, false) => "machine offline: waiting for it".to_string(),
    }
}

/// `POST` path for waking a machine.
pub fn path(machine_id: &str) -> String {
    let enc: String = machine_id
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("/api/v1/machines/{enc}/wake")
}

/// A server-given name as one safe line.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

impl App {
    /// `prefix W`: wake the selected machine, if it is offline and something can.
    pub fn wake_open(&mut self) {
        let Some(m) = self.machine().cloned() else { return };
        let note = if m.online {
            format!("{} is already online", m.name)
        } else if let Some(via) = &m.wake_via {
            self.wake_out.push(m.id.clone());
            format!("Sending the wake signal via {via}…")
        } else if m.wake_blocked {
            format!("{NO_RELAY_TEXT} A wake relay can be set up in its settings on the web.")
        } else {
            format!("{} can't be woken from here yet", m.name)
        };
        self.note = Some(note);
    }

    /// The wake request answered: `Ok(relay name)` or the error to show.
    pub fn on_wake(&mut self, mid: &str, r: Result<String, String>) {
        let name = self
            .machines
            .iter()
            .find(|m| m.id == mid)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| mid.to_string());
        self.note = Some(match r {
            Ok(via) => format!("Wake signal sent via {}… waiting for {name}", clean(&via)),
            Err(e) => format!("{name}: {}", clean(&e)),
        });
    }
}
