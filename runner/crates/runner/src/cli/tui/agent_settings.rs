//! `prefix S`: what each agent gets on this machine (Phase 17, PLAN box 4;
//! docs/design/plugin-settings-ux.md §5 "TUI"). Read only: the effective
//! settings from `GET /api/machines/{id}/plugins/effective` (the same API
//! token and server the TUI already uses for the machine list), drawn over
//! the panes, one block per agent:
//!
//!   Claude Code
//!     Starts with: Model: sonnet · 2 extra arguments
//!     ! Dangerous: Never ask for permission
//!     MCP servers: github, postgres · 1 off
//!
//! Then the add-ons on the machine (Phase 17b; the `addons` list of the same
//! response, absent from an older server), each with where it sends data:
//!
//!   Add-ons on this machine
//!     Example telemetry: sends data to telemetry.example.com (workspace)
//!
//! `e` opens Settings → Plugins on the web, where everything is changed;
//! `r` loads again; ↑↓ scroll; Esc closes. A project can differ from the
//! machine: the view says so and leaves projects to the web page.
//!
//! Names come from the person's own items and are shown as text with
//! control characters replaced. Secrets never reach this view: the API
//! sends item names and launch values only.

use super::app::{App, Mode};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use serde_json::Value;

/// Settings the runner does not take from here yet (the web form does not draw them either).
const NOT_SHOWN: [&str; 2] = ["extras", "keystroke"];
/// Kinds in the order the web page lists them.
const KINDS: [(&str, &str); 5] = [
    ("mcp", "MCP servers"),
    ("instructions", "Instructions"),
    ("skill", "Skills"),
    ("subagent", "Sub-agents"),
    ("command", "Commands"),
];

#[derive(Debug, Clone, PartialEq)]
pub enum Load {
    Loading,
    Ready(Value),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub machine_id: String,
    pub machine: String,
    pub load: Load,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Heading,
    Plain,
    Danger,
    Muted,
}

/// The API path for one machine (no project: projects are on the web page).
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
    format!("/api/machines/{enc}/plugins/effective")
}

/// Typed text as one safe line: control characters become spaces.
fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn schema(plugin: &str) -> Value {
    crate::plugin_gen::settings_schema(plugin)
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null)
}

/// The agent's name ("Claude Code"), else its id.
pub fn agent_label(plugin: &str) -> String {
    schema(plugin)["title"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| clean(plugin))
}

/// Launch values in a few words (the web's `launchSummary`), and the dangerous switches that are on.
pub fn launch_summary(plugin: &str, values: &Value) -> (String, Vec<String>) {
    let s = schema(plugin);
    let mut parts = vec![];
    let mut danger = vec![];
    let Some(props) = s["properties"].as_object() else {
        return (String::new(), danger);
    };
    for (key, node) in props {
        if NOT_SHOWN.contains(&key.as_str()) {
            continue;
        }
        let title = node["title"].as_str().unwrap_or(key);
        let v = &values[key.as_str()];
        match node["type"].as_str() {
            Some("boolean") if v == &Value::Bool(true) => {
                if node["x-dangerous"].is_string() {
                    danger.push(title.to_string());
                } else {
                    parts.push(title.to_string());
                }
            }
            Some("array") => {
                let n = v.as_array().map_or(0, Vec::len);
                if n > 0 {
                    parts.push(format!("{n} extra {}", if n == 1 { "argument" } else { "arguments" }));
                }
            }
            Some("string") | Some("integer") | Some("number") => {
                let t = match v {
                    Value::String(x) if !x.is_empty() => clean(x),
                    Value::Number(x) => x.to_string(),
                    _ => continue,
                };
                parts.push(format!("{title}: {}", t.chars().take(40).collect::<String>()));
            }
            _ => {}
        }
    }
    (parts.join(" · "), danger)
}

/// The rows of the view, with their tone.
pub fn rows(view: &View) -> Vec<(Tone, String)> {
    let mut out = vec![];
    let v = match &view.load {
        Load::Loading => return vec![(Tone::Muted, "Loading…".into())],
        Load::Failed(why) => {
            return vec![
                (Tone::Danger, format!("Could not load: {}", clean(why))),
                (Tone::Muted, "r tries again · e opens the web page".into()),
            ]
        }
        Load::Ready(v) => v,
    };
    for p in v["plugins"].as_array().into_iter().flatten() {
        let plugin = p["plugin"].as_str().unwrap_or("");
        let label = agent_label(plugin);
        out.push((Tone::Heading, label.clone()));
        let (text, danger) = launch_summary(plugin, &p["values"]);
        let decisions: Vec<&Value> = p["decisions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["shadowedBy"].is_null())
            .collect();
        if text.is_empty() && danger.is_empty() && decisions.is_empty() {
            out.push((
                Tone::Muted,
                format!("  Nothing added: {label} starts with only its own setup."),
            ));
            continue;
        }
        out.push((
            Tone::Plain,
            if text.is_empty() {
                "  Starts as it would on its own.".into()
            } else {
                format!("  Starts with: {text}")
            },
        ));
        for d in danger {
            out.push((Tone::Danger, format!("  ! Dangerous: {d}")));
        }
        for (kind, title) in KINDS {
            let of: Vec<&Value> = decisions.iter().copied().filter(|d| d["kind"] == kind).collect();
            if of.is_empty() {
                continue;
            }
            let on: Vec<String> = of
                .iter()
                .filter(|d| d["enabled"] == true)
                .map(|d| clean(d["name"].as_str().unwrap_or("?")))
                .collect();
            let off = of.len() - on.len();
            let names = if on.is_empty() {
                "none on".to_string()
            } else {
                on.join(", ")
            };
            let mut line = format!("  {title}: {names}");
            if off > 0 {
                line.push_str(&format!(" · {off} off"));
            }
            out.push((Tone::Plain, line));
        }
    }
    let addons: Vec<&Value> = v["addons"].as_array().into_iter().flatten().collect();
    if !addons.is_empty() {
        out.push((Tone::Heading, "Add-ons on this machine".into()));
        for a in addons {
            let title = clean(a["title"].as_str().or(a["name"].as_str()).unwrap_or("?"));
            let hosts: Vec<String> = a["sendsTo"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|o| clean(o.split_once("://").map_or(o, |(_, rest)| rest).trim_end_matches('/')))
                .collect();
            let sends = if hosts.is_empty() {
                "sends nothing off the machine".to_string()
            } else {
                format!("sends data to {}", hosts.join(", "))
            };
            let from = if a["from"] == "workspace" {
                "workspace"
            } else {
                "this machine"
            };
            out.push((Tone::Plain, format!("  {title}: {sends} ({from})")));
        }
    }
    out.push((Tone::Muted, String::new()));
    out.push((
        Tone::Muted,
        "A project can change these. Press e to see or change them on the web.".into(),
    ));
    out
}

impl App {
    /// Open the view for the selected machine and ask for its settings.
    pub fn agent_settings_open(&mut self) {
        let Some(m) = self.machine() else {
            self.note = Some("No machine selected.".into());
            return;
        };
        let (id, name) = (m.id.clone(), m.name.clone());
        self.agent_settings = Some(View {
            machine_id: id.clone(),
            machine: name,
            load: Load::Loading,
        });
        self.agent_settings_out.push(id);
        self.mode = Mode::AgentSettings(0);
    }

    /// The answer of `GET …/plugins/effective` (from the event loop's thread).
    pub fn on_agent_settings(&mut self, machine_id: &str, r: Result<Value, String>) {
        if let Some(v) = self.agent_settings.as_mut().filter(|v| v.machine_id == machine_id) {
            v.load = match r {
                Ok(x) => Load::Ready(x),
                Err(e) => Load::Failed(e),
            };
        }
    }

    /// A key while the view is open; `top` is the first row shown.
    pub fn agent_settings_key(&mut self, top: u16, k: KeyEvent) {
        let last = self
            .agent_settings
            .as_ref()
            .map_or(0, |v| rows(v).len())
            .saturating_sub(1) as u16;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                self.agent_settings = None;
            }
            KeyCode::Char('e') => {
                self.mode = Mode::Normal;
                self.agent_settings = None;
                if self.web_url.is_empty() {
                    self.note = Some("Open Settings → Plugins on the web to change these.".into());
                } else {
                    self.open_urls
                        .push(format!("{}/settings/plugins", self.web_url.trim_end_matches('/')));
                    self.note = Some("Opening Settings → Plugins in your browser.".into());
                }
            }
            KeyCode::Char('r') => {
                if let Some(v) = self.agent_settings.as_mut() {
                    v.load = Load::Loading;
                    self.agent_settings_out.push(v.machine_id.clone());
                }
            }
            KeyCode::Up => self.mode = Mode::AgentSettings(top.saturating_sub(1)),
            KeyCode::Down => self.mode = Mode::AgentSettings((top + 1).min(last)),
            KeyCode::PageUp => self.mode = Mode::AgentSettings(top.saturating_sub(10)),
            KeyCode::PageDown => self.mode = Mode::AgentSettings((top + 10).min(last)),
            _ => {}
        }
    }
}
