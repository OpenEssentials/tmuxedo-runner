//! Secret delivery on the runner (docs/design/secrets-and-env.md §3.3, §4;
//! docs/protocol.md "Secrets").
//!
//! - After every `welcome` the runner registers its X25519 public key
//!   (`secret_key`). Web answers with a `secrets` frame: the policy (names and
//!   bindings, never values) and the mask list (every value delivered to this
//!   machine in the last 30 days, sealed to the key).
//! - A mechanism that needs values asks with `secret_req {names, purpose}`;
//!   web checks the bindings, audits, and answers with the values sealed to
//!   this runner's key for this request. The gateway relays ciphertext only.
//! - Values live in memory ([`Cache`], zeroized on drop), 15 minutes after
//!   their last use; a policy change or revocation drops them. Offline, only
//!   what is cached is available (and, opted in per machine, a sealed copy on
//!   disk, [`offline`]).
//! - The runner checks the bindings too, before it asks: web is the
//!   authority, the local check stops a refused request early.

use super::seal::{self, RunnerKey};
use crate::api::{err, ApiErr};
use crate::daemon::Daemon;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// How long an unused value stays in memory.
pub const TTL: Duration = Duration::from_secs(15 * 60);
/// How long a request may wait for web.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Binding {
    Exec {
        #[serde(default)]
        programs: Vec<String>,
    },
    Mcp {
        plugin: String,
        server: String,
    },
    Agent {
        plugin: String,
    },
    Git {
        #[serde(default)]
        hosts: Vec<String>,
    },
    Pane,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Purpose {
    pub kind: String,
    pub plugin: Option<String>,
    pub pane_id: Option<String>,
    pub mcp: Option<String>,
    pub program: Option<String>,
    pub host: Option<String>,
}

impl Purpose {
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "kind": self.kind });
        for (k, f) in [
            ("plugin", &self.plugin),
            ("paneId", &self.pane_id),
            ("mcp", &self.mcp),
            ("program", &self.program),
            ("host", &self.host),
        ] {
            if let Some(x) = f {
                v[k] = json!(x);
            }
        }
        v
    }
}

impl Binding {
    pub fn allows(&self, p: &Purpose) -> bool {
        match (self, p.kind.as_str()) {
            (Binding::Exec { programs }, "exec") => p
                .program
                .as_deref()
                .is_some_and(|prog| super::exec::program_allowed(Path::new(prog), programs)),
            (Binding::Mcp { plugin, server }, "mcp") => {
                p.plugin.as_deref() == Some(plugin.as_str())
                    && (server == "*" || p.mcp.as_deref() == Some(server.as_str()))
            }
            (Binding::Agent { plugin }, "agent") => p.plugin.as_deref() == Some(plugin.as_str()),
            (Binding::Git { hosts }, "git") => p
                .host
                .as_deref()
                .is_some_and(|h| hosts.iter().any(|x| x.eq_ignore_ascii_case(h))),
            (Binding::Pane, "pane") => true,
            _ => false,
        }
    }
}

/// What web says this machine may use (names and bindings only).
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub names: BTreeMap<String, Vec<Binding>>,
    pub offline_cache: bool,
    pub ssh: Value,
    /// Plugins whose hook guard the user turned off.
    pub guard_off: Vec<String>,
}

impl Policy {
    pub fn parse(v: &Value) -> Policy {
        let mut names = BTreeMap::new();
        for n in v["names"].as_array().into_iter().flatten() {
            let Some(name) = n["name"].as_str() else { continue };
            let b: Vec<Binding> = n["bindings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| serde_json::from_value(b.clone()).ok())
                .collect();
            names.insert(name.to_string(), b);
        }
        Policy {
            names,
            offline_cache: v["offlineCache"].as_bool().unwrap_or(false),
            ssh: v["ssh"].clone(),
            guard_off: v["guardOff"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(String::from))
                .collect(),
        }
    }

    pub fn allows(&self, name: &str, p: &Purpose) -> bool {
        self.names.get(name).is_some_and(|b| b.iter().any(|b| b.allows(p)))
    }

    /// Names bound with `pred`.
    pub fn bound(&self, pred: impl Fn(&Binding) -> bool) -> Vec<String> {
        self.names
            .iter()
            .filter(|(_, b)| b.iter().any(&pred))
            .map(|(n, _)| n.clone())
            .collect()
    }
}

pub struct Cached {
    pub value: Zeroizing<Vec<u8>>,
    pub version: u64,
    pub used: Instant,
}

pub type Values = Vec<(String, Zeroizing<Vec<u8>>)>;
pub type Waiter = Box<dyn FnOnce(&mut Daemon, Result<Values, ApiErr>) + Send>;

struct Pending {
    names: Vec<String>,
    since: Instant,
    waiters: Vec<Waiter>,
}

#[derive(Default)]
pub struct Rt {
    pub key: Option<RunnerKey>,
    pub machine: String,
    pub policy: Policy,
    pub cache: BTreeMap<String, Cached>,
    pending: HashMap<String, Pending>,
    next: u64,
    /// Where the opt-in offline copy lives.
    pub offline_path: Option<PathBuf>,
    /// Agent transcripts scanned so far, and the (transcript, name) hits reported.
    pub transcripts: super::guard::Transcripts,
    pub exposed: std::collections::HashSet<(String, String)>,
}

impl Rt {
    pub fn new(state_dir: &Path) -> Rt {
        let key = match RunnerKey::load_or_create(&state_dir.join("secret-key")) {
            Ok(k) => Some(k),
            Err(e) => {
                crate::log!("secrets: no runner key ({e}); secrets are unavailable on this machine");
                None
            }
        };
        Rt {
            key,
            offline_path: Some(state_dir.join("secrets-offline.sealed")),
            ..Rt::default()
        }
    }

    fn cached(&mut self, names: &[String]) -> Option<Values> {
        let now = Instant::now();
        let mut out = vec![];
        for n in names {
            let c = self.cache.get_mut(n)?;
            c.used = now;
            out.push((n.clone(), c.value.clone()));
        }
        Some(out)
    }
}

/// The opt-in offline copy (BYO machines, D-8b): the cache sealed to the
/// runner's own key, so the file alone (a copied disk image without the key
/// file) does not open. On a stolen laptop with both files it does: that is
/// why it is off by default and per machine.
pub mod offline {
    use super::*;

    pub fn save(rt: &Rt) {
        let (Some(path), Some(key)) = (&rt.offline_path, &rt.key) else {
            return;
        };
        if !rt.policy.offline_cache {
            let _ = std::fs::remove_file(path);
            return;
        }
        let mut m = serde_json::Map::new();
        for (n, c) in &rt.cache {
            m.insert(
                n.clone(),
                json!({"v": String::from_utf8_lossy(&c.value), "version": c.version}),
            );
        }
        let plain = Zeroizing::new(Value::Object(m).to_string().into_bytes());
        if let Ok(s) = seal::seal(&key.public_b64(), &plain, "offline-cache") {
            let _ = crate::config::write_private(path, s.as_bytes());
        }
    }

    pub fn load(rt: &mut Rt) {
        let (Some(path), Some(key)) = (&rt.offline_path, &rt.key) else {
            return;
        };
        let Ok(s) = std::fs::read_to_string(path) else { return };
        let Ok(plain) = key.open(s.trim(), "offline-cache") else {
            return;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&plain) else {
            return;
        };
        let now = Instant::now();
        for (n, c) in v.as_object().into_iter().flatten() {
            if let Some(val) = c["v"].as_str() {
                rt.cache.insert(
                    n.clone(),
                    Cached {
                        value: Zeroizing::new(val.as_bytes().to_vec()),
                        version: c["version"].as_u64().unwrap_or(0),
                        used: now,
                    },
                );
            }
        }
    }
}

impl Daemon {
    /// `welcome`: register the key (every connection; web stores it when new).
    pub(crate) fn secrets_welcomed(&mut self, machine_id: &str) {
        self.delivery.machine = machine_id.to_string();
        let Some(k) = &self.delivery.key else { return };
        let f = json!({"t": "secret_key", "id": "key", "alg": seal::ALG, "publicKey": k.public_b64()});
        self.queue_msg(tokio_tungstenite::tungstenite::Message::Text(f.to_string().into()));
    }

    /// A `secrets` frame.
    pub(crate) fn on_secrets_frame(
        &mut self,
        mask: Option<String>,
        policy: Option<Value>,
        revoked: Option<Vec<String>>,
    ) {
        if let Some(p) = policy {
            let next = Policy::parse(&p);
            // Bindings changed: what is cached was fetched under the old ones.
            let changed = next.names != self.delivery.policy.names;
            self.delivery.policy = next;
            if changed {
                self.delivery.cache.clear();
            }
            if self.delivery.policy.offline_cache && self.delivery.cache.is_empty() {
                offline::load(&mut self.delivery);
            }
            offline::save(&self.delivery);
            crate::log!(
                "secrets: policy has {} names{}",
                self.delivery.policy.names.len(),
                if self.delivery.policy.offline_cache {
                    " (offline copy on)"
                } else {
                    ""
                }
            );
            self.prefetch_pane_secrets();
            self.secrets_patch_launch();
            self.ssh_policy_changed();
            // The generated skill lists the names (crate::skill "secrets").
            if self.sync_skills() && self.shim_dir.is_some() {
                let args = self.launch.claude_args.clone();
                let _ = self.plugins.write_shims(&self.opts.state_dir, &args);
                self.secrets_patch_launch();
            }
        }
        for n in revoked.unwrap_or_default() {
            if let Some(id) = n.strip_prefix("ssh:") {
                self.ssh_revoked(id);
                continue;
            }
            self.delivery.exposed.retain(|(_, x)| *x != n);
            self.delivery.cache.remove(&n);
            offline::save(&self.delivery);
        }
        if let (Some(m), Some(k)) = (mask, &self.delivery.key) {
            let ctx = seal::mask_context(&self.delivery.machine);
            match k
                .open(&m, &ctx)
                .ok()
                .and_then(|p| serde_json::from_slice::<Value>(&p).ok())
            {
                Some(v) => {
                    // Entries are `{name, value}` (or a bare value, named by position).
                    let vals: Vec<(String, Zeroizing<Vec<u8>>)> = v["values"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .filter_map(|(i, x)| {
                            let (name, val) = match x {
                                Value::String(s) => (format!("#{i}"), s.as_str()),
                                o => (format!("{}@m{i}", o["name"].as_str()?), o["value"].as_str()?),
                            };
                            Some((name, Zeroizing::new(val.as_bytes().to_vec())))
                        })
                        .collect();
                    let n = vals.len();
                    let grew = self.secrets.replace_source(super::Source::Vault, vals);
                    crate::log!("secrets: masking {n} delivered values");
                    self.mask_changed(grew);
                }
                None => crate::log!("secrets: the mask list does not open (key changed?)"),
            }
        }
    }

    /// The answer to a `secret_req` (or the key registration).
    pub(crate) fn on_secret_res(
        &mut self,
        id: String,
        result: Option<Value>,
        error: Option<tmuxedo_protocol::frames::ErrorBody>,
    ) {
        let Some(p) = self.delivery.pending.remove(&id) else {
            if let Some(e) = error {
                crate::log!("secrets: {id}: {} {}", e.code, e.message.unwrap_or_default());
            }
            return;
        };
        let res = match (result, error) {
            (_, Some(e)) => Err(err(&e.code, e.message.unwrap_or_else(|| "refused".into()))),
            (Some(r), None) => self.open_answer(&id, &p.names, &r),
            (None, None) => Err(err("bad_answer", "empty answer")),
        };
        for w in p.waiters {
            w(self, res.clone());
        }
    }

    fn open_answer(&mut self, id: &str, names: &[String], r: &Value) -> Result<Values, ApiErr> {
        let Some(k) = &self.delivery.key else {
            return Err(err("no_key", "this runner has no secret key"));
        };
        let ctx = seal::answer_context(&self.delivery.machine, id);
        let plain = r["sealed"]
            .as_str()
            .and_then(|s| k.open(s, &ctx).ok())
            .ok_or_else(|| err("bad_answer", "the answer does not open"))?;
        let v: Value = serde_json::from_slice(&plain).map_err(|_| err("bad_answer", "not JSON"))?;
        let now = Instant::now();
        let mut out = vec![];
        let mut grew = false;
        for n in names {
            let Some(val) = v["values"][n].as_str() else { continue };
            let version = v["versions"][n].as_u64().unwrap_or(0);
            let z = Zeroizing::new(val.as_bytes().to_vec());
            grew |= self
                .secrets
                .insert(super::Source::Delivered, &format!("{n}@{version}"), z.clone());
            self.delivery.cache.insert(
                n.clone(),
                Cached {
                    value: z.clone(),
                    version,
                    used: now,
                },
            );
            out.push((n.clone(), z));
        }
        self.mask_changed(grew);
        offline::save(&self.delivery);
        let missing: Vec<&String> = names.iter().filter(|n| !out.iter().any(|(m, _)| m == *n)).collect();
        if !missing.is_empty() {
            let why = r["denied"]
                .as_array()
                .filter(|d| !d.is_empty())
                .map(|_| "not allowed for this use")
                .unwrap_or("no such secret");
            return Err(err(
                "secret_unavailable",
                format!(
                    "{}: {why}",
                    missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        Ok(out)
    }

    /// Get `names` for `purpose`: from memory, else from web; `w` gets them
    /// (or why not). The local binding check comes first.
    /// As [`Daemon::secret_want`] for names the policy lists elsewhere (ssh
    /// keys: `ssh:<id>`); web checks them as always.
    pub(crate) fn secret_want_unchecked(&mut self, names: Vec<String>, purpose: Purpose, w: Waiter) {
        debug_assert_eq!(purpose.kind, "ssh");
        self.secret_want(names, purpose, w)
    }

    pub(crate) fn secret_want(&mut self, names: Vec<String>, purpose: Purpose, w: Waiter) {
        let refused: Vec<&String> = names
            .iter()
            .filter(|n| !self.delivery.policy.allows(n, &purpose))
            .collect();
        if !refused.is_empty() && purpose.kind != "ssh" {
            let msg = format!(
                "{} not allowed for {}{} (Settings → Secrets → who may receive it)",
                refused.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                purpose.kind,
                purpose.program.as_ref().map(|p| format!(" {p}")).unwrap_or_default()
            );
            return w(self, Err(err("not_allowed", msg)));
        }
        if let Some(v) = self.delivery.cached(&names) {
            return w(self, Ok(v));
        }
        if self.delivery.key.is_none() {
            return w(self, Err(err("no_key", "this runner has no secret key")));
        }
        if !self.gateway_ready() {
            return w(
                self,
                Err(err(
                    "offline",
                    format!("{} unavailable: machine offline", names.join(", ")),
                )),
            );
        }
        self.delivery.next += 1;
        let id = format!("s{}", self.delivery.next);
        let f = json!({"t": "secret_req", "id": id, "names": names, "purpose": purpose.to_json()});
        self.queue_msg(tokio_tungstenite::tungstenite::Message::Text(f.to_string().into()));
        self.delivery.pending.insert(
            id,
            Pending {
                names,
                since: Instant::now(),
                waiters: vec![w],
            },
        );
    }

    /// Every few seconds: expire values and stuck requests.
    pub(crate) fn secrets_tick(&mut self) {
        let now = Instant::now();
        let before = self.delivery.cache.len();
        let pinned = self.delivery.policy.bound(|b| matches!(b, Binding::Pane));
        self.delivery
            .cache
            .retain(|n, c| pinned.contains(n) || now.duration_since(c.used) < TTL);
        if self.delivery.cache.len() != before {
            offline::save(&self.delivery);
        }
        let stuck: Vec<String> = self
            .delivery
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.since) > REQUEST_TIMEOUT)
            .map(|(k, _)| k.clone())
            .collect();
        for id in stuck {
            if let Some(p) = self.delivery.pending.remove(&id) {
                for w in p.waiters {
                    w(self, Err(err("timeout", "tmuxedo did not answer in time")));
                }
            }
        }
    }

    /// Drop everything (the gateway connection is gone: answers in flight never come).
    pub(crate) fn secrets_disconnected(&mut self) {
        let ids: Vec<String> = self.delivery.pending.keys().cloned().collect();
        for id in ids {
            if let Some(p) = self.delivery.pending.remove(&id) {
                for w in p.waiters {
                    w(self, Err(err("offline", "machine offline")));
                }
            }
        }
    }

    /// Pane-bound secrets go into the spawn env of new panes, so they are
    /// fetched as soon as the policy names them (and kept while it does).
    fn prefetch_pane_secrets(&mut self) {
        let names: Vec<String> = self
            .delivery
            .policy
            .bound(|b| matches!(b, Binding::Pane))
            .into_iter()
            .filter(|n| !self.delivery.cache.contains_key(n))
            .collect();
        if names.is_empty() {
            return;
        }
        let p = Purpose {
            kind: "pane".into(),
            ..Default::default()
        };
        self.secret_want(
            names,
            p,
            Box::new(|_, r| {
                if let Err(e) = r {
                    crate::log!("secrets: pane env: {}", e.message);
                }
            }),
        );
    }

    /// Values for a new pane's environment (pane-bound, cached).
    pub(crate) fn pane_secret_env(&self) -> Vec<(String, String)> {
        self.delivery
            .policy
            .bound(|b| matches!(b, Binding::Pane))
            .into_iter()
            .filter_map(|n| {
                let c = self.delivery.cache.get(&n)?;
                Some((n, String::from_utf8_lossy(&c.value).into_owned()))
            })
            .collect()
    }

    /// The pane a process belongs to: its session leader is the pane's
    /// process (the holder starts each pane in its own session).
    pub(crate) fn pane_of_pid(&self, pid: i32) -> Option<String> {
        // SAFETY: getsid only reads.
        let sid = unsafe { libc::getsid(pid) };
        if sid <= 0 {
            return None;
        }
        self.panes
            .iter()
            .find(|(_, rt)| rt.alive && rt.pid as i32 == sid)
            .map(|(id, _)| id.clone())
            .or_else(|| self.pane_by_ancestry(pid))
    }

    /// A process that started its own session (a daemonising tool): walk its parents.
    fn pane_by_ancestry(&self, pid: i32) -> Option<String> {
        let mut p = pid;
        for _ in 0..64 {
            if let Some((id, _)) = self.panes.iter().find(|(_, rt)| rt.alive && rt.pid as i32 == p) {
                return Some(id.clone());
            }
            p = parent_pid(p)?;
            if p <= 1 {
                return None;
            }
        }
        None
    }

    /// `secret exec` / the MCP wrapper (secrets/exec.rs).
    pub(crate) fn on_exec_ask(&mut self, a: super::exec::ExecAsk) {
        let Some(pane) = self.pane_of_pid(a.pid) else {
            let _ = a.reply.send(Err((
                "forbidden".into(),
                "secret exec runs only inside a tmuxedo pane".into(),
            )));
            return;
        };
        let purpose = match &a.mcp {
            Some((plugin, server)) => Purpose {
                kind: "mcp".into(),
                plugin: Some(plugin.clone()),
                mcp: Some(server.clone()),
                pane_id: Some(pane),
                program: Some(a.program.clone()),
                ..Default::default()
            },
            None => Purpose {
                kind: "exec".into(),
                pane_id: Some(pane),
                program: Some(a.program.clone()),
                ..Default::default()
            },
        };
        let reply = a.reply;
        self.secret_want(
            a.names,
            purpose,
            Box::new(move |_, r| {
                let _ = reply.send(r.map_err(|e| (e.code, e.message)));
            }),
        );
    }

    /// Local API: `secret.list`, `secret.git_credential`, `secret.agent_env`.
    pub(crate) fn secret_local(&mut self, method: &str, params: &Value, peer: Option<i32>, reply: crate::api::ReplyTo) {
        let pane = peer.and_then(|p| self.pane_of_pid(p));
        match method {
            "secret.list" => {
                let names: Vec<Value> = self
                    .delivery
                    .policy
                    .names
                    .iter()
                    .map(|(n, b)| json!({"name": n, "uses": b.iter().map(binding_label).collect::<Vec<_>>()}))
                    .collect();
                crate::api::respond(reply, Ok(json!({"secrets": names})), self);
            }
            "secret.git_credential" | "secret.agent_env" => {
                let Some(pane) = pane else {
                    return crate::api::respond(reply, Err(err("forbidden", "only from a tmuxedo pane")), self);
                };
                let (names, purpose) = if method == "secret.git_credential" {
                    let host = params["host"].as_str().unwrap_or("").to_ascii_lowercase();
                    let p = Purpose {
                        kind: "git".into(),
                        host: Some(host),
                        pane_id: Some(pane),
                        ..Default::default()
                    };
                    let names: Vec<String> = self
                        .delivery
                        .policy
                        .names
                        .keys()
                        .filter(|n| self.delivery.policy.allows(n, &p))
                        .take(1)
                        .cloned()
                        .collect();
                    (names, p)
                } else {
                    let plugin = params["plugin"].as_str().unwrap_or("").to_string();
                    let p = Purpose {
                        kind: "agent".into(),
                        plugin: Some(plugin),
                        pane_id: Some(pane),
                        ..Default::default()
                    };
                    // `names`: only these (an add-on's, crate::addon_gen); the
                    // same binding decides either way.
                    let only: Option<Vec<&str>> = params["names"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect());
                    let names: Vec<String> = self
                        .delivery
                        .policy
                        .names
                        .keys()
                        .filter(|n| only.as_ref().is_none_or(|o| o.contains(&n.as_str())))
                        .filter(|n| self.delivery.policy.allows(n, &p))
                        .cloned()
                        .collect();
                    (names, p)
                };
                if names.is_empty() {
                    return crate::api::respond(reply, Ok(json!({"values": {}})), self);
                }
                self.secret_want(
                    names,
                    purpose,
                    Box::new(move |d, r| {
                        let res = r.map(|vals| {
                            let m: serde_json::Map<String, Value> = vals
                                .iter()
                                .map(|(n, v)| (n.clone(), Value::String(String::from_utf8_lossy(v).into_owned())))
                                .collect();
                            json!({"values": m})
                        });
                        crate::api::respond(reply, res, d);
                    }),
                );
            }
            _ => crate::api::respond(reply, Err(err("unknown_method", method)), self),
        }
    }
}

impl Daemon {
    /// The hook guard: a denial for `command`, or None. Only while the
    /// machine has secrets, and not for a plugin the user turned it off for.
    pub(crate) fn secret_guard(&mut self, plugin: &str, pane_id: &str, command: &str) -> Option<Value> {
        let p = &self.delivery.policy;
        if p.names.is_empty() || p.guard_off.iter().any(|x| x == plugin) {
            return None;
        }
        let names: Vec<String> = p.names.keys().cloned().collect();
        let why = super::guard::check(command, &names)?;
        crate::log!("secret guard: denied a {plugin} command in {pane_id} ({why})");
        Some(super::guard::deny_reply(&why, &names))
    }

    /// PostToolUse: the tool's output with every known secret masked, as the
    /// hook's `updatedToolOutput` (Claude Code; decisions/0005). None when
    /// there is nothing to mask (an identity rewrite could clobber another hook's).
    pub(crate) fn post_tool_rewrite(&self, ev: &Value) -> Option<Value> {
        if ev["hook_event_name"].as_str() != Some("PostToolUse") {
            return None;
        }
        let mut out = ev.get("tool_response")?.clone();
        if !super::mask_json(&self.secrets.set(), &mut out) {
            return None;
        }
        Some(json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedToolOutput": out}}))
    }

    /// After a turn: the transcript's new bytes, checked for every secret value.
    pub(crate) fn scan_transcript(&mut self, pane_id: &str, plugin: &str, kind: &str, ev: &Value) -> bool {
        let Some(path) = ev["transcript_path"].as_str().filter(|p| p.starts_with('/')) else {
            return false;
        };
        if self.secrets.set().is_empty() {
            return false;
        }
        let Some(tail) = self.delivery.transcripts.tail(path) else {
            return false;
        };
        let mut raised = false;
        for name in self.secrets.names_in(&tail) {
            if name.starts_with('#') || !self.delivery.exposed.insert((path.to_string(), name.clone())) {
                continue;
            }
            crate::log!("secret {name} appeared in the {plugin} transcript of {pane_id}");
            self.queue_event(
                pane_id,
                "secret_exposed",
                json!({ "name": name, "plugin": plugin, "kind": kind, "transcript": path }),
            );
            raised = true;
        }
        raised
    }

    /// Generated launch files follow the policy: the guard's PreToolUse hook
    /// while the machine has secrets, and the agent-env line in a plugin's
    /// shim while a secret is bound to that agent.
    pub(crate) fn secrets_patch_launch(&mut self) {
        let guard = !self.delivery.policy.names.is_empty();
        let dir = self.opts.state_dir.clone();
        let exe = self.opts.hook_exe.clone();
        let mut changed = vec![];
        let mut guard_changed = false;
        for p in self.plugins.active() {
            let id = p.id().to_string();
            let on = guard && !self.delivery.policy.guard_off.contains(&id);
            let gen = crate::plugin::gen_paths(p.as_ref(), &dir);
            for (name, g) in &p.manifest().generate {
                if g.builder == "hook-settings" {
                    if let Some(path) = gen.get(name) {
                        if super::guard::patch_settings(path, on).unwrap_or(false) {
                            guard_changed = true;
                            changed.push(format!("{id} guard {}", if on { "on" } else { "off" }));
                        }
                    }
                }
            }
            if let Some(l) = &p.manifest().launch {
                let agent_env = self
                    .delivery
                    .policy
                    .names
                    .values()
                    .flatten()
                    .any(|b| matches!(b, Binding::Agent { plugin } if *plugin == id));
                let shim = dir.join("shim").join(&l.shim);
                if super::guard::patch_shim(&shim, &exe, &id, agent_env).unwrap_or(false) {
                    changed.push(format!("{id} agent env {}", if agent_env { "on" } else { "off" }));
                }
            }
        }
        if !changed.is_empty() {
            crate::log!("secrets: {}", changed.join(", "));
        }
        // An add-on's settings copy (crate::addon_gen) follows the host's.
        if guard_changed {
            self.sync_plugin_set();
        }
    }
}

pub fn binding_label(b: &Binding) -> String {
    match b {
        Binding::Exec { programs } => format!("secret exec: {}", programs.join(", ")),
        Binding::Mcp { plugin, server } => format!("MCP {server} ({plugin})"),
        Binding::Agent { plugin } => format!("{plugin} agent env"),
        Binding::Git { hosts } => format!("git credential: {}", hosts.join(", ")),
        Binding::Pane => "pane env".into(),
    }
}

/// The parent of `pid`, from the OS.
pub fn parent_pid(pid: i32) -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &s[s.rfind(')')? + 2..];
        rest.split_whitespace().nth(1)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_allow_only_their_use() {
        let pol = Policy::parse(&json!({"names": [
            {"name": "GH_TOKEN", "bindings": [{"kind": "exec", "programs": ["sh"]}, {"kind": "git", "hosts": ["github.com"]}]},
            {"name": "OPENAI_API_KEY", "bindings": [{"kind": "agent", "plugin": "codex"}]},
            {"name": "LINEAR", "bindings": [{"kind": "mcp", "plugin": "claude", "server": "linear"}]},
            {"name": "BAD", "bindings": [{"kind": "nonsense"}]},
        ], "offlineCache": true}));
        assert!(pol.offline_cache);
        let sh = super::super::exec::resolve_program("sh", "/bin:/usr/bin", Path::new("/")).unwrap();
        let exec = |p: &Path| Purpose {
            kind: "exec".into(),
            program: Some(p.display().to_string()),
            ..Default::default()
        };
        let sh_name = sh.file_name().unwrap().to_str().unwrap().to_string();
        let pol2 =
            Policy::parse(&json!({"names": [{"name": "X", "bindings": [{"kind": "exec", "programs": [sh_name]}]}]}));
        assert!(pol2.allows("X", &exec(&sh)));
        assert!(!pol2.allows("X", &exec(Path::new("/usr/bin/env"))));
        let git = |h: &str| Purpose {
            kind: "git".into(),
            host: Some(h.into()),
            ..Default::default()
        };
        assert!(pol.allows("GH_TOKEN", &git("GitHub.com")));
        assert!(!pol.allows("GH_TOKEN", &git("gitlab.com")));
        let agent = |p: &str| Purpose {
            kind: "agent".into(),
            plugin: Some(p.into()),
            ..Default::default()
        };
        assert!(pol.allows("OPENAI_API_KEY", &agent("codex")));
        assert!(!pol.allows("OPENAI_API_KEY", &agent("claude")));
        assert!(!pol.allows("GH_TOKEN", &agent("claude")));
        let mcp = |s: &str| Purpose {
            kind: "mcp".into(),
            plugin: Some("claude".into()),
            mcp: Some(s.into()),
            ..Default::default()
        };
        assert!(pol.allows("LINEAR", &mcp("linear")));
        assert!(!pol.allows("LINEAR", &mcp("github")));
        assert!(!pol.allows("BAD", &agent("claude")));
        assert!(!pol.allows("MISSING", &agent("codex")));
    }
}
