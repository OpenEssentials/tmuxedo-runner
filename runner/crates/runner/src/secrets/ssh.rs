//! The runner's ssh-agent (docs/design/secrets-and-env.md §6a): the agent
//! can *use* a key but never *read* it. No key files in `~/.ssh`: panes
//! whose scope binds a key get `SSH_AUTH_SOCK` pointing at this agent.
//!
//! - **Keys** come from the policy web sends (`secrets.policy.ssh`): a
//!   `machine` key was generated on this machine (Secure Enclave through the
//!   Mac app when it is there, else sealed on disk to the runner's own key);
//!   a `vault` key is fetched sealed (`secret_req` kind `ssh`) and held in
//!   memory for its lifetime (default 15 minutes), its material added to the
//!   mask set.
//! - **Who**: the caller's pane (`SO_PEERCRED` → session) must be in the
//!   key's scope (all machines, this machine, or a project) and, when the key
//!   names agents, run one of them.
//! - **Where**: a key with destinations signs a login only after a verified
//!   `session-bind@openssh.com` whose host key `known_hosts` lists for one of
//!   them, for the very session being signed, and never for a forwarded
//!   agent. Commit signing (`SSHSIG`) only when the key allows it.
//! - **Confirm each use**: a durable `ssh_confirm` event (push) and the
//!   signature waits for a `decision` with its prompt id; deny or 60 s
//!   without an answer refuses. Adding, removing and locking keys from a
//!   pane are refused.

use super::sshwire::{self as w, Bind, PrivateKey, Reader};
use crate::daemon::Daemon;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// The confirmation wait: [`CONFIRM_TIMEOUT`], or `TMUXEDO_SSH_CONFIRM_TIMEOUT_MS` (tests).
pub fn confirm_timeout() -> Duration {
    tmuxedo_holder::env_var("SSH_CONFIRM_TIMEOUT_MS")
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(CONFIRM_TIMEOUT)
}
pub const DEFAULT_LIFETIME: Duration = Duration::from_secs(15 * 60);

pub fn socket_path(runner_socket: &Path) -> PathBuf {
    runner_socket.with_file_name("ssh-agent.sock")
}

/// One key as the policy describes it (never private material).
#[derive(Debug, Clone, PartialEq)]
pub struct KeySpec {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub blob: Vec<u8>,
    pub line: String,
    pub destinations: Vec<String>,
    pub confirm: String,
    pub lifetime: Option<Duration>,
    pub scope: String,
    pub scope_ref: String,
    pub plugins: Vec<String>,
    pub signing: bool,
    pub hardware: Option<String>,
}

impl KeySpec {
    pub fn parse(v: &Value) -> Option<KeySpec> {
        let line = v["publicKey"].as_str()?.to_string();
        let strs = |k: &str| -> Vec<String> {
            v[k].as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        };
        Some(KeySpec {
            id: v["id"].as_str()?.to_string(),
            name: v["name"].as_str().unwrap_or("key").to_string(),
            kind: v["kind"].as_str().unwrap_or("vault").to_string(),
            blob: w::blob_of_line(&line)?,
            line,
            destinations: strs("destinations"),
            confirm: v["confirm"].as_str().unwrap_or("never").to_string(),
            lifetime: v["lifetimeMinutes"].as_u64().map(|m| Duration::from_secs(m * 60)),
            scope: v["scope"].as_str().unwrap_or("user").to_string(),
            scope_ref: v["scopeRef"].as_str().unwrap_or("").to_string(),
            plugins: strs("plugins"),
            signing: v["signing"].as_bool().unwrap_or(false),
            hardware: v["hardware"].as_str().map(String::from),
        })
    }
}

/// From the agent socket to the daemon.
pub enum Ask {
    Identities {
        pid: i32,
        reply: std::sync::mpsc::Sender<Vec<(Vec<u8>, String)>>,
    },
    Sign {
        pid: i32,
        blob: Vec<u8>,
        data: Vec<u8>,
        bind: Option<Bind>,
        reply: std::sync::mpsc::Sender<Option<Vec<u8>>>,
    },
}

struct Loaded {
    key: PrivateKey,
    until: Instant,
}

struct Pending {
    key_id: String,
    data: Vec<u8>,
    deadline: Instant,
    reply: std::sync::mpsc::Sender<Option<Vec<u8>>>,
}

#[derive(Default)]
pub struct Rt {
    pub keys: Vec<KeySpec>,
    loaded: HashMap<String, Loaded>,
    pending: HashMap<String, Pending>,
    next: u64,
    /// Where machine keys live (sealed to the runner key).
    pub dir: Option<PathBuf>,
}

impl Rt {
    pub fn new(dir: PathBuf) -> Rt {
        Rt {
            dir: Some(dir),
            ..Rt::default()
        }
    }
}

/// The `known_hosts` files OpenSSH reads.
fn known_hosts_text(home: &str) -> String {
    let mut s = std::fs::read_to_string(Path::new(home).join(".ssh/known_hosts")).unwrap_or_default();
    s.push('\n');
    s.push_str(&std::fs::read_to_string("/etc/ssh/ssh_known_hosts").unwrap_or_default());
    s
}

/// May `key` sign `data` (with `bind`)? Err = why not (logged, never sent).
pub fn allowed(key: &KeySpec, data: &[u8], bind: Option<&Bind>, known_hosts: &str) -> Result<String, String> {
    if w::is_sshsig(data) {
        return if key.signing {
            Ok("commit signing".into())
        } else {
            Err("not allowed for signing".into())
        };
    }
    let Some(sid) = w::userauth_session(data) else {
        return Err("not a login or a commit signature".into());
    };
    if bind.is_some_and(|b| b.forwarding) {
        return Err("agent forwarding is refused".into());
    }
    if key.destinations.is_empty() {
        return Ok(bind.map(|_| "a host").unwrap_or("a host").into());
    }
    let Some(b) = bind.filter(|b| b.session_id == sid) else {
        return Err("no verified host for this login (OpenSSH 8.9 or newer binds sessions)".into());
    };
    for d in &key.destinations {
        let (keys, revoked) = w::known_keys(known_hosts, d);
        if revoked.contains(&b.host_key) {
            return Err(format!("{d}'s key is revoked"));
        }
        if keys.contains(&b.host_key) {
            return Ok(d.clone());
        }
    }
    Err(format!("the host is not one of {}", key.destinations.join(", ")))
}

// ── the socket ────────────────────────────────────────────────────────────

pub fn serve(path: &Path, ask: tokio::sync::mpsc::UnboundedSender<Ask>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new().name("ssh-agent".into()).spawn(move || {
        for s in l.incoming().flatten() {
            let ask = ask.clone();
            let _ = std::thread::Builder::new()
                .name("ssh-agent-conn".into())
                .spawn(move || connection(s, ask));
        }
    })?;
    Ok(())
}

fn read_msg(s: &mut UnixStream) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).ok()?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > 256 * 1024 {
        return None;
    }
    let mut body = vec![0u8; n];
    s.read_exact(&mut body).ok()?;
    Some(body)
}

fn connection(mut s: UnixStream, ask: tokio::sync::mpsc::UnboundedSender<Ask>) {
    let Some(pid) = super::exec::peer_pid(s.as_raw_fd()) else {
        return;
    };
    let mut bind: Option<Bind> = None;
    while let Some(msg) = read_msg(&mut s) {
        let mut r = Reader::new(&msg);
        let reply = match r.u8() {
            Some(w::REQUEST_IDENTITIES) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = ask.send(Ask::Identities { pid, reply: tx });
                let ids = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
                let mut out = vec![w::IDENTITIES_ANSWER];
                out.extend_from_slice(&(ids.len() as u32).to_be_bytes());
                for (blob, comment) in ids {
                    w::put_string(&mut out, &blob);
                    w::put_string(&mut out, comment.as_bytes());
                }
                out
            }
            Some(w::SIGN_REQUEST) => {
                let (Some(blob), Some(data)) = (r.string(), r.string()) else {
                    let _ = s.write_all(&w::frame(&[w::FAILURE]));
                    continue;
                };
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = ask.send(Ask::Sign {
                    pid,
                    blob: blob.to_vec(),
                    data: data.to_vec(),
                    bind: bind.clone(),
                    reply: tx,
                });
                match rx
                    .recv_timeout(CONFIRM_TIMEOUT + Duration::from_secs(30))
                    .ok()
                    .flatten()
                {
                    Some(sig) => {
                        let mut out = vec![w::SIGN_RESPONSE];
                        w::put_string(&mut out, &sig);
                        out
                    }
                    None => vec![w::FAILURE],
                }
            }
            Some(w::EXTENSION) => match r.string() {
                Some(b"session-bind@openssh.com") => match w::session_bind(r.rest()) {
                    Some(b) => {
                        bind = Some(b);
                        vec![w::SUCCESS]
                    }
                    None => vec![w::FAILURE],
                },
                _ => vec![w::FAILURE],
            },
            // add / remove / lock / anything else: keys are managed by tmuxedo only.
            _ => vec![w::FAILURE],
        };
        if s.write_all(&w::frame(&reply)).is_err() {
            return;
        }
    }
}

// ── the daemon side ───────────────────────────────────────────────────────

impl Daemon {
    /// Does `k` apply to `pane_id` (scope, and the agent when the key names some)?
    fn ssh_applies(&self, k: &KeySpec, pane_id: &str, check_agent: bool) -> bool {
        let scope_ok = match k.scope.as_str() {
            "machine" => k.scope_ref == self.delivery.machine,
            "project" => self
                .state
                .pane(pane_id)
                .and_then(|p| {
                    self.projects.project_for(
                        Path::new(&p.pane.cwd),
                        &self.delivery.machine,
                        &std::collections::BTreeMap::from([(k.scope_ref.clone(), Default::default())]),
                    )
                })
                .is_some(),
            _ => true,
        };
        if !scope_ok {
            return false;
        }
        if !check_agent || k.plugins.is_empty() {
            return true;
        }
        let kind = self
            .panes
            .get(pane_id)
            .and_then(|rt| rt.track.reported_kind.clone().or_else(|| rt.track.process_kind.clone()))
            .or_else(|| self.state.pane(pane_id).and_then(|p| p.meta.kind.clone()));
        kind.is_some_and(|kind| {
            k.plugins.iter().any(|p| {
                *p == kind
                    || self
                        .plugins
                        .active()
                        .any(|x| x.id() == p.as_str() && x.agent_label() == kind.as_str())
            })
        })
    }

    /// The pane gets `SSH_AUTH_SOCK` (and commit signing) when a key applies.
    pub(crate) fn ssh_pane_env(&self, pane_id: &str, env: &mut std::collections::BTreeMap<String, String>) {
        let keys: Vec<&KeySpec> = self
            .ssh
            .keys
            .iter()
            .filter(|k| self.ssh_applies(k, pane_id, false))
            .collect();
        if keys.is_empty() {
            env.remove("SSH_AUTH_SOCK");
            return;
        }
        env.insert(
            "SSH_AUTH_SOCK".into(),
            socket_path(&self.opts.runner_socket).display().to_string(),
        );
        if let Some(k) = keys.iter().find(|k| k.signing) {
            let n: usize = env.get("GIT_CONFIG_COUNT").and_then(|n| n.parse().ok()).unwrap_or(0);
            let entries = [
                ("gpg.format", "ssh".to_string()),
                ("user.signingkey", format!("key::{}", k.line)),
                ("commit.gpgsign", "true".to_string()),
                ("tag.gpgsign", "true".to_string()),
            ];
            for (i, (key, val)) in entries.iter().enumerate() {
                env.insert(format!("GIT_CONFIG_KEY_{}", n + i), key.to_string());
                env.insert(format!("GIT_CONFIG_VALUE_{}", n + i), val.clone());
            }
            env.insert("GIT_CONFIG_COUNT".into(), (n + entries.len()).to_string());
        }
    }

    /// `secrets.policy.ssh` changed.
    pub(crate) fn ssh_policy_changed(&mut self) {
        let next: Vec<KeySpec> = self
            .delivery
            .policy
            .ssh
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(KeySpec::parse)
            .collect();
        let ids: Vec<&str> = next.iter().map(|k| k.id.as_str()).collect();
        self.ssh.loaded.retain(|id, _| ids.contains(&id.as_str()));
        if next != self.ssh.keys {
            crate::log!("ssh agent: {} keys", next.len());
        }
        self.ssh.keys = next;
    }

    /// `secrets.revoked` names `ssh:<id>`: the key is gone (a machine key's sealed file too).
    pub(crate) fn ssh_revoked(&mut self, id: &str) {
        self.ssh.loaded.remove(id);
        self.ssh.keys.retain(|k| k.id != id);
        if let Some(d) = &self.ssh.dir {
            let _ = std::fs::remove_file(d.join(format!("{id}.sealed")));
        }
        self.secrets.remove(super::Source::SshKey, &format!("ssh:{id}"));
        crate::log!("ssh agent: key {id} revoked");
    }

    pub(crate) fn on_ssh_ask(&mut self, a: Ask) {
        match a {
            Ask::Identities { pid, reply } => {
                let ids = match self.pane_of_pid(pid) {
                    Some(pane) => self
                        .ssh
                        .keys
                        .iter()
                        .filter(|k| self.ssh_applies(k, &pane, true))
                        .map(|k| (k.blob.clone(), format!("{} (tmuxedo)", k.name)))
                        .collect(),
                    None => vec![],
                };
                let _ = reply.send(ids);
            }
            Ask::Sign {
                pid,
                blob,
                data,
                bind,
                reply,
            } => {
                let Some(pane) = self.pane_of_pid(pid) else {
                    let _ = reply.send(None);
                    return;
                };
                let Some(key) = self
                    .ssh
                    .keys
                    .iter()
                    .find(|k| k.blob == blob && self.ssh_applies(k, &pane, true))
                    .cloned()
                else {
                    let _ = reply.send(None);
                    return;
                };
                let what = match allowed(&key, &data, bind.as_ref(), &known_hosts_text(&self.opts.home)) {
                    Ok(w) => w,
                    Err(why) => {
                        crate::log!("ssh agent: {} refused in {pane}: {why}", key.name);
                        let _ = reply.send(None);
                        return;
                    }
                };
                if key.confirm != "never" {
                    self.ssh.next += 1;
                    let prompt = format!("ssh-{}-{}", self.ssh.next, crate::util::random_id());
                    self.queue_event(
                        &pane,
                        "ssh_confirm",
                        json!({
                            "promptId": prompt, "keyId": key.id, "key": key.name, "use": what,
                            "fingerprint": w::fingerprint(&key.blob), "stepUp": key.confirm == "each_stepup",
                        }),
                    );
                    self.ssh.pending.insert(
                        prompt,
                        Pending {
                            key_id: key.id.clone(),
                            data,
                            deadline: Instant::now() + confirm_timeout(),
                            reply,
                        },
                    );
                    self.ssh_flush = true;
                    return;
                }
                self.ssh_sign(&key, &pane, data, reply);
            }
        }
    }

    /// A `decision` whose prompt id is an ssh confirmation.
    pub(crate) fn ssh_decision(&mut self, pane_id: &str, prompt: &str, allow: bool) -> bool {
        let Some(p) = self.ssh.pending.remove(prompt) else {
            return prompt.starts_with("ssh-");
        };
        let key = self.ssh.keys.iter().find(|k| k.id == p.key_id).cloned();
        match (allow, key) {
            (true, Some(k)) => {
                crate::log!("ssh agent: {} approved for one use", k.name);
                self.ssh_sign(&k, pane_id, p.data, p.reply);
            }
            _ => {
                crate::log!("ssh agent: a confirmation was denied");
                let _ = p.reply.send(None);
            }
        }
        true
    }

    /// Every few seconds: confirmations nobody answered are refused; expired keys unloaded.
    pub(crate) fn ssh_tick(&mut self) {
        let now = Instant::now();
        let late: Vec<String> = self
            .ssh
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for id in late {
            if let Some(p) = self.ssh.pending.remove(&id) {
                crate::log!("ssh agent: a confirmation timed out; refused");
                let _ = p.reply.send(None);
            }
        }
        self.ssh.loaded.retain(|_, l| l.until > now);
    }

    fn ssh_sign(&mut self, key: &KeySpec, pane: &str, data: Vec<u8>, reply: std::sync::mpsc::Sender<Option<Vec<u8>>>) {
        if let Some(l) = self.ssh.loaded.get(&key.id) {
            let _ = reply.send(l.key.sign(&data).ok());
            return;
        }
        let lifetime = key.lifetime.unwrap_or(DEFAULT_LIFETIME);
        match key.kind.as_str() {
            "machine" => {
                if matches!(key.hardware.as_deref(), Some("secure_enclave" | "mac_keychain")) {
                    let _ = reply.send(self.ssh_enclave_sign(key, &data));
                    return;
                }
                match self.ssh_machine_key(&key.id) {
                    Some(k) => {
                        let sig = k.sign(&data).ok();
                        self.ssh.loaded.insert(
                            key.id.clone(),
                            Loaded {
                                key: k,
                                until: Instant::now() + lifetime,
                            },
                        );
                        let _ = reply.send(sig);
                    }
                    None => {
                        crate::log!("ssh agent: machine key {} is not on this machine", key.name);
                        let _ = reply.send(None);
                    }
                }
            }
            _ => {
                let id = key.id.clone();
                let purpose = super::delivery::Purpose {
                    kind: "ssh".into(),
                    pane_id: Some(pane.to_string()),
                    ..Default::default()
                };
                self.secret_want_unchecked(
                    vec![format!("ssh:{id}")],
                    purpose,
                    Box::new(move |d, r| {
                        let k = r.ok().and_then(|v| {
                            use base64::Engine;
                            let der = Zeroizing::new(
                                base64::engine::general_purpose::STANDARD
                                    .decode(v.first()?.1.as_slice())
                                    .ok()?,
                            );
                            let k = PrivateKey::from_pkcs8(&der).ok()?;
                            // Its material never shows in a pane or a frame.
                            let b64 = Zeroizing::new(
                                base64::engine::general_purpose::STANDARD
                                    .encode(der.as_slice())
                                    .into_bytes(),
                            );
                            let grew = d.secrets.insert(super::Source::SshKey, &format!("ssh:{id}"), b64);
                            d.mask_changed(grew);
                            Some(k)
                        });
                        match k {
                            Some(k) => {
                                let _ = reply.send(k.sign(&data).ok());
                                d.ssh.loaded.insert(
                                    id,
                                    Loaded {
                                        key: k,
                                        until: Instant::now() + lifetime,
                                    },
                                );
                            }
                            None => {
                                crate::log!("ssh agent: vault key {id} unavailable");
                                let _ = reply.send(None);
                            }
                        }
                    }),
                );
            }
        }
    }

    fn ssh_machine_key(&self, id: &str) -> Option<PrivateKey> {
        let path = self.ssh.dir.as_ref()?.join(format!("{id}.sealed"));
        let sealed = std::fs::read_to_string(path).ok()?;
        let der = self
            .delivery
            .key
            .as_ref()?
            .open(sealed.trim(), &format!("ssh-key:{id}"))
            .ok()?;
        PrivateKey::from_pkcs8(&der).ok()
    }

    fn ssh_enclave_sign(&self, key: &KeySpec, data: &[u8]) -> Option<Vec<u8>> {
        use base64::Engine;
        let s = self.opts.signer.as_ref()?;
        let r = s
            .0
            .call(&json!({"op": "ssh_sign", "keyId": key.id, "data": base64::engine::general_purpose::STANDARD.encode(data)}))
            .ok()?;
        base64::engine::general_purpose::STANDARD
            .decode(r["sig"].as_str()?)
            .ok()
    }

    /// `ssh.generate {keyId, name}` (web, through the gateway): a new key held
    /// by this machine. The Mac app's Secure Enclave when it is there, else
    /// Ed25519 sealed to the runner key in the state directory. Returns the
    /// public half only.
    pub(crate) fn ssh_generate(&mut self, p: &Value) -> crate::api::ApiResult {
        use crate::api::err;
        let id = p["keyId"]
            .as_str()
            .filter(|s| {
                !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            })
            .ok_or_else(|| err("invalid_params", "keyId"))?;
        let name = p["name"]
            .as_str()
            .unwrap_or("tmuxedo")
            .chars()
            .take(64)
            .collect::<String>();
        let comment = format!("{name} (tmuxedo, {})", crate::util::hostname());
        if let Some(s) = &self.opts.signer {
            if let Ok(r) = s.0.call(&json!({"op": "ssh_create", "keyId": id})) {
                if let Some(line) = r["publicKey"].as_str() {
                    if let Some(blob) = w::blob_of_line(line) {
                        return Ok(
                            json!({"publicKey": w::authorized_line(&blob, &comment), "fingerprint": w::fingerprint(&blob), "hardware": if r["hardware"] == "software" { "mac_keychain" } else { "secure_enclave" }}),
                        );
                    }
                }
            }
        }
        let key = self
            .delivery
            .key
            .as_ref()
            .ok_or_else(|| err("no_key", "this runner has no key to seal with"))?;
        let dir = self
            .ssh
            .dir
            .clone()
            .ok_or_else(|| err("internal_error", "no key folder"))?;
        let (der, k) = PrivateKey::generate_ed25519().map_err(|e| err("internal_error", e))?;
        let sealed = super::seal::seal(&key.public_b64(), &der, &format!("ssh-key:{id}"))
            .map_err(|e| err("internal_error", e))?;
        std::fs::create_dir_all(&dir).map_err(|e| err("internal_error", e.to_string()))?;
        crate::config::write_private(&dir.join(format!("{id}.sealed")), sealed.as_bytes())
            .map_err(|e| err("internal_error", e))?;
        let blob = k.blob();
        let tpm = Path::new("/dev/tpmrm0").exists();
        crate::log!("ssh agent: generated key {name} ({})", w::fingerprint(&blob));
        Ok(json!({
            "publicKey": w::authorized_line(&blob, &comment),
            "fingerprint": w::fingerprint(&blob),
            "hardware": "software",
            "tpmPresent": tpm,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dest: &[&str], signing: bool) -> KeySpec {
        let (_, k) = PrivateKey::generate_ed25519().unwrap();
        KeySpec {
            id: "k1".into(),
            name: "GitHub".into(),
            kind: "vault".into(),
            blob: k.blob(),
            line: w::authorized_line(&k.blob(), ""),
            destinations: dest.iter().map(|s| s.to_string()).collect(),
            confirm: "never".into(),
            lifetime: None,
            scope: "user".into(),
            scope_ref: String::new(),
            plugins: vec![],
            signing,
            hardware: None,
        }
    }

    fn login(sid: &[u8]) -> Vec<u8> {
        let mut d = vec![];
        w::put_string(&mut d, sid);
        d.push(50);
        d
    }

    #[test]
    fn destinations_need_a_verified_bind_for_this_session() {
        let (_, host_a) = PrivateKey::generate_ed25519().unwrap();
        let (_, host_b) = PrivateKey::generate_ed25519().unwrap();
        let kh = format!(
            "[127.0.0.1]:2201 {}\n[127.0.0.1]:2202 {}\n",
            w::authorized_line(&host_a.blob(), ""),
            w::authorized_line(&host_b.blob(), "")
        );
        let k = spec(&["[127.0.0.1]:2201"], false);
        let bind = |h: &PrivateKey, sid: &[u8], fwd: bool| Bind {
            host_key: h.blob(),
            session_id: sid.to_vec(),
            forwarding: fwd,
        };
        assert_eq!(
            allowed(&k, &login(b"s1"), Some(&bind(&host_a, b"s1", false)), &kh).unwrap(),
            "[127.0.0.1]:2201"
        );
        assert!(
            allowed(&k, &login(b"s1"), Some(&bind(&host_b, b"s1", false)), &kh).is_err(),
            "host B"
        );
        assert!(
            allowed(&k, &login(b"s2"), Some(&bind(&host_a, b"s1", false)), &kh).is_err(),
            "another session"
        );
        assert!(allowed(&k, &login(b"s1"), None, &kh).is_err(), "no bind");
        assert!(
            allowed(&k, &login(b"s1"), Some(&bind(&host_a, b"s1", true)), &kh).is_err(),
            "forwarded"
        );
        assert!(allowed(&k, b"SSHSIG...", None, &kh).is_err(), "not a signing key");
        assert!(allowed(&spec(&[], true), b"SSHSIG...", None, &kh).is_ok());
        assert!(allowed(&spec(&[], false), b"arbitrary", None, &kh).is_err());
        assert!(
            allowed(&spec(&[], false), &login(b"s9"), None, &kh).is_ok(),
            "no destinations: any host"
        );
    }

    #[test]
    fn policy_entries_parse() {
        let (_, k) = PrivateKey::generate_ed25519().unwrap();
        let v = json!({"id":"k9","name":"deploy","kind":"machine","publicKey": w::authorized_line(&k.blob(), "x"),
            "destinations":["github.com"],"confirm":"each_stepup","lifetimeMinutes":30,"scope":"project","scopeRef":"github.com/o/r",
            "plugins":["claude"],"signing":true,"hardware":"software"});
        let s = KeySpec::parse(&v).unwrap();
        assert_eq!(s.blob, k.blob());
        assert_eq!(s.lifetime, Some(Duration::from_secs(1800)));
        assert_eq!(
            (s.confirm.as_str(), s.signing, s.scope.as_str()),
            ("each_stepup", true, "project")
        );
        assert!(KeySpec::parse(&json!({"id":"x"})).is_none());
    }
}
