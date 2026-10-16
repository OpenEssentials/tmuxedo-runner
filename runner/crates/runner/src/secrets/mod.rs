//! Secrets on the machine (docs/design/secrets-and-env.md §3–§5).
//!
//! - [`mask`]: the streaming masker every pane's bytes pass through.
//! - [`Secrets`]: the literal values this runner masks (vault deliveries of
//!   the last 30 days as web lists them, live deliveries, and secret-named
//!   variables in the user's shell settings). Values live in memory only,
//!   zeroized on drop. The current [`mask::MaskSet`] is shared ([`Shared`])
//!   with every pane, the chat masker and the outgoing text frames, so all
//!   of them mask the same values (one set per daemon, never per process:
//!   tests run several daemons in one process).

pub mod delivery;
pub mod exec;
pub mod guard;
pub mod mask;
pub mod seal;
pub mod ssh;
pub mod sshwire;

use mask::MaskSet;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use zeroize::Zeroizing;

/// A daemon's current mask set, readable from anywhere it is handed to.
#[derive(Debug, Clone, Default)]
pub struct Shared(Arc<RwLock<Arc<MaskSet>>>);

impl Shared {
    pub fn get(&self) -> Arc<MaskSet> {
        self.0.read().map(|g| g.clone()).unwrap_or_default()
    }

    fn put(&self, set: Arc<MaskSet>) {
        if let Ok(mut g) = self.0.write() {
            *g = set;
        }
    }
}

/// Mask every string in a JSON value (object keys too) with `set`.
pub fn mask_json(set: &MaskSet, v: &mut Value) -> bool {
    fn walk(set: &MaskSet, v: &mut Value, depth: usize) -> bool {
        match v {
            Value::String(s) if set.contains(s.as_bytes()) => {
                *s = set.mask_str(s);
                true
            }
            Value::Array(a) if depth < 128 => a.iter_mut().fold(false, |acc, x| walk(set, x, depth + 1) | acc),
            Value::Object(o) if depth < 128 => {
                let mut changed = false;
                let keys: Vec<String> = o.keys().cloned().collect();
                for k in keys {
                    let mut item = o.remove(&k).unwrap_or(Value::Null);
                    changed |= walk(set, &mut item, depth + 1);
                    let key = if set.contains(k.as_bytes()) {
                        changed = true;
                        set.mask_str(&k)
                    } else {
                        k
                    };
                    o.insert(key, item);
                }
                changed
            }
            _ => false,
        }
    }
    if set.is_empty() {
        return false;
    }
    walk(set, v, 0)
}

/// Mask a JSON text frame: unchanged (no copy) unless it holds a pattern.
pub fn mask_text_frame(set: &MaskSet, text: &str) -> Option<String> {
    if set.is_empty() || !set.contains(text.as_bytes()) {
        return None;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(mut v) => {
            mask_json(set, &mut v);
            Some(v.to_string())
        }
        // Not JSON (never expected): mask the bytes.
        Err(_) => Some(set.mask_str(text)),
    }
}

/// Where a mask literal came from (several sources may hold the same value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// A secret-named variable in Settings → Terminal env.
    ShellEnv,
    /// Web's list of values delivered to this machine (last 30 days).
    Vault,
    /// A delivery in this process.
    Delivered,
    /// Private key material handed to the ssh agent.
    SshKey,
}

#[derive(Default)]
pub struct Secrets {
    literals: BTreeMap<(Source, String), Zeroizing<Vec<u8>>>,
    /// SHA-256 of every value in the published set (to tell growth).
    hashes: BTreeSet<[u8; 32]>,
    set: Arc<MaskSet>,
    shared: Shared,
    /// Per secret name, its own set: which secret a hit in a transcript is.
    named: Vec<(String, Arc<MaskSet>)>,
}

/// The secret name a literal stands for: shell variables and vault entries
/// carry it as their id, deliveries as `NAME@version`.
fn name_of(id: &str) -> &str {
    id.split('@').next().unwrap_or(id)
}

impl Secrets {
    pub fn set(&self) -> Arc<MaskSet> {
        self.set.clone()
    }

    /// A handle that always reads the current set.
    pub fn shared(&self) -> Shared {
        self.shared.clone()
    }

    pub fn literal_count(&self) -> usize {
        self.literals.len()
    }

    /// Replace every literal of one source (`id` → value). Returns true when
    /// the set gained a value it did not mask before (callers re-mask what
    /// they already hold).
    pub fn replace_source(
        &mut self,
        source: Source,
        values: impl IntoIterator<Item = (String, Zeroizing<Vec<u8>>)>,
    ) -> bool {
        self.literals.retain(|(s, _), _| *s != source);
        for (id, v) in values {
            self.literals.insert((source, id), v);
        }
        self.rebuild()
    }

    /// Add (or replace) one literal.
    pub fn insert(&mut self, source: Source, id: &str, value: Zeroizing<Vec<u8>>) -> bool {
        self.literals.insert((source, id.to_string()), value);
        self.rebuild()
    }

    pub fn remove(&mut self, source: Source, id: &str) -> bool {
        self.literals.remove(&(source, id.to_string()));
        self.rebuild()
    }

    fn rebuild(&mut self) -> bool {
        let mut hashes = BTreeSet::new();
        for v in self.literals.values() {
            if mask::forms(v).is_empty() {
                continue;
            }
            hashes.insert(<[u8; 32]>::from(Sha256::digest(v.as_slice())));
        }
        if hashes == self.hashes {
            return false;
        }
        let grew = hashes.iter().any(|h| !self.hashes.contains(h));
        let set = Arc::new(MaskSet::from_values(self.literals.values().map(|v| v.as_slice())));
        self.hashes = hashes;
        self.set = set.clone();
        self.shared.put(set);
        let mut by_name: BTreeMap<&str, Vec<&[u8]>> = BTreeMap::new();
        for ((_, id), v) in &self.literals {
            by_name.entry(name_of(id)).or_default().push(v.as_slice());
        }
        self.named = by_name
            .into_iter()
            .map(|(n, vals)| (n.to_string(), Arc::new(MaskSet::from_values(vals))))
            .filter(|(_, s)| !s.is_empty())
            .collect();
        grew
    }

    /// The names of the secrets whose values (in any encoding) `buf` holds.
    pub fn names_in(&self, buf: &[u8]) -> Vec<String> {
        if !self.set.contains(buf) {
            return vec![];
        }
        self.named
            .iter()
            .filter(|(_, s)| s.contains(buf))
            .map(|(n, _)| n.clone())
            .collect()
    }
}

/// Secret-named variables of a shell `env` map (the chat masker's rule).
pub fn secret_named<'a>(env: impl IntoIterator<Item = (&'a String, &'a String)>) -> Vec<(String, Zeroizing<Vec<u8>>)> {
    env.into_iter()
        .filter(|(k, _)| crate::chat::mask::is_secret_name(k))
        .map(|(k, v)| (k.clone(), Zeroizing::new(v.as_bytes().to_vec())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn z(s: &str) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(s.as_bytes().to_vec())
    }

    #[test]
    fn hits_name_their_secret() {
        let mut s = Secrets::default();
        s.insert(Source::Delivered, "GH_TOKEN@3", z("gh-value-0123456789"));
        s.insert(Source::ShellEnv, "API_SECRET", z("api-value-0123456789"));
        s.insert(Source::Vault, "OLD_TOKEN", z("old-value-0123456789"));
        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode("api-value-0123456789")
        };
        assert_eq!(
            s.names_in(format!("x {b64} gh-value-0123456789").as_bytes()),
            ["API_SECRET", "GH_TOKEN"]
        );
        assert_eq!(s.names_in(b"old-value-0123456789"), ["OLD_TOKEN"]);
        assert!(s.names_in(b"nothing here").is_empty());
    }

    #[test]
    fn growth_is_reported_once() {
        let mut s = Secrets::default();
        assert!(s.insert(Source::Delivered, "A", z("value-aaaaaaaa")));
        assert!(
            !s.insert(Source::Vault, "A@1", z("value-aaaaaaaa")),
            "same value, other source"
        );
        assert!(!s.remove(Source::Vault, "A@1"));
        assert!(!s.remove(Source::Delivered, "A"), "shrinking is not growth");
        assert!(s.set().is_empty());
        assert!(!s.insert(Source::Delivered, "short", z("abc")), "too short to mask");
    }

    #[test]
    fn json_frames_are_masked_by_value() {
        let mut s = Secrets::default();
        s.insert(Source::Delivered, "T", z("tok-0123456789abcdef"));
        let set = s.set();
        let text = json!({"t":"event","data":{"cmd":"echo tok-0123456789abcdef","tok-0123456789abcdef":1}}).to_string();
        let out = mask_text_frame(&set, &text).expect("changed");
        assert!(!out.contains("tok-0123456789abcdef"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["data"]["cmd"], "echo ********************");
        assert!(mask_text_frame(&set, r#"{"t":"ping"}"#).is_none());
    }
}
