//! Plugin package signing (docs/design/plugins.md §9.2, D-4).
//!
//! A package is a directory. Its **digest** is SHA-256 over a canonical
//! list: for every file (sorted by path, `SIGNATURE` excluded)
//! `<path>\n<sha256 hex of its bytes>\n`. `SIGNATURE` holds
//! `tmuxedo-plugin-sig-v1 <key id> <base64 ed25519 signature over the
//! digest bytes>`. The runner carries the **current** and **next** public
//! keys; a signature verifies if either key accepts it, so a key can roll
//! over without a flag day. Verification runs at every load, not only at
//! install.
//!
//! The keys below are **not set yet**: the private key will be held offline
//! by the operator, with an offline publishing flow;
//! until then nothing but a built-in plugin can load, which is the intended
//! state (D-2: built-in + curated only).

use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Hex-encoded 32-byte ed25519 public keys. `None` = slot empty.
pub const CURRENT_KEY: Option<&str> = None;
pub const NEXT_KEY: Option<&str> = None;

pub const SIGNATURE_FILE: &str = "SIGNATURE";
const SIG_PREFIX: &str = "tmuxedo-plugin-sig-v1";
/// Per-file and per-package size limits (a manifest plugin is tiny).
pub const MAX_FILE: u64 = 4 * 1024 * 1024;
pub const MAX_PACKAGE: u64 = 32 * 1024 * 1024;

/// A package's files, by relative path.
pub type Files = BTreeMap<String, Vec<u8>>;

/// The canonical digest.
pub fn digest(files: &Files) -> [u8; 32] {
    let mut h = Sha256::new();
    for (path, bytes) in files {
        if path == SIGNATURE_FILE {
            continue;
        }
        h.update(path.as_bytes());
        h.update(b"\n");
        h.update(hex::encode(Sha256::digest(bytes)).as_bytes());
        h.update(b"\n");
    }
    h.finalize().into()
}

/// Key id = first 8 hex chars of sha256(public key).
pub fn key_id(public_key: &[u8]) -> String {
    hex::encode(Sha256::digest(public_key))[..8].to_string()
}

/// Render a `SIGNATURE` file (the publishing side; tests).
pub fn signature_text(public_key: &[u8], sig: &[u8]) -> String {
    format!(
        "{SIG_PREFIX} {} {}\n",
        key_id(public_key),
        base64::engine::general_purpose::STANDARD.encode(sig)
    )
}

fn parse_signature(text: &str) -> Result<(String, Vec<u8>), String> {
    let mut it = text.split_whitespace();
    if it.next() != Some(SIG_PREFIX) {
        return Err("SIGNATURE: not a tmuxedo-plugin-sig-v1 line".into());
    }
    let kid = it.next().ok_or("SIGNATURE: missing key id")?;
    let b64 = it.next().ok_or("SIGNATURE: missing signature")?;
    let sig = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| "SIGNATURE: signature is not base64".to_string())?;
    if sig.len() != 64 {
        return Err("SIGNATURE: an ed25519 signature is 64 bytes".into());
    }
    Ok((kid.to_string(), sig))
}

/// Verify `files` (which must contain `SIGNATURE`) against `keys`
/// (raw 32-byte ed25519 public keys). Ok(key id) on success.
pub fn verify_with(keys: &[Vec<u8>], files: &Files) -> Result<String, String> {
    let text = files
        .get(SIGNATURE_FILE)
        .ok_or_else(|| format!("{SIGNATURE_FILE} is missing"))?;
    let (kid, sig) = parse_signature(&String::from_utf8_lossy(text))?;
    let d = digest(files);
    for k in keys {
        if key_id(k) != kid {
            continue;
        }
        let pk = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, k);
        return match pk.verify(&d, &sig) {
            Ok(()) => Ok(kid),
            Err(_) => Err("SIGNATURE does not match the package digest".into()),
        };
    }
    Err(format!("SIGNATURE key {kid} is not a tmuxedo plugin signing key"))
}

/// The compiled-in keys (current, next) that are set.
pub fn builtin_keys() -> Vec<Vec<u8>> {
    [CURRENT_KEY, NEXT_KEY]
        .into_iter()
        .flatten()
        .filter_map(|h| hex::decode(h).ok())
        .filter(|k| k.len() == 32)
        .collect()
}

/// Verify against the compiled-in keys.
pub fn verify(files: &Files) -> Result<String, String> {
    let keys = builtin_keys();
    if keys.is_empty() {
        return Err("no plugin signing key is compiled into this runner".into());
    }
    verify_with(&keys, files)
}

/// Read a package directory: regular files only (symlinks and anything that
/// is not a file or directory are refused), paths relative to the root,
/// bounded sizes.
pub fn read_dir(root: &Path) -> Result<Files, String> {
    let mut files = Files::new();
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in rd {
            let entry = entry.map_err(|e| e.to_string())?;
            let p = entry.path();
            let meta = std::fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let rel = p
                .strip_prefix(root)
                .map_err(|_| format!("{} escapes the package root", p.display()))?;
            let rel_s = rel.to_string_lossy().into_owned();
            if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) || rel_s.contains('\\') {
                return Err(format!("{rel_s}: path is not a plain relative path"));
            }
            if meta.file_type().is_symlink() {
                return Err(format!("{rel_s}: symlinks are not allowed in a plugin package"));
            }
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                if meta.len() > MAX_FILE {
                    return Err(format!("{rel_s}: larger than {MAX_FILE} bytes"));
                }
                total += meta.len();
                if total > MAX_PACKAGE {
                    return Err(format!("package larger than {MAX_PACKAGE} bytes"));
                }
                let bytes = std::fs::read(&p).map_err(|e| format!("{rel_s}: {e}"))?;
                files.insert(rel_s, bytes);
            } else {
                return Err(format!("{rel_s}: not a regular file"));
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    fn keypair() -> (ring::signature::Ed25519KeyPair, Vec<u8>) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pk = kp.public_key().as_ref().to_vec();
        (kp, pk)
    }

    fn package() -> Files {
        let mut f = Files::new();
        f.insert("plugin.toml".into(), b"id = 'x'\n".to_vec());
        f.insert("tests/hooks/a.jsonl".into(), b"{}\n".to_vec());
        f
    }

    #[test]
    fn digest_is_canonical_and_ignores_the_signature_file() {
        let a = package();
        let mut b = package();
        b.insert(SIGNATURE_FILE.into(), b"whatever".to_vec());
        assert_eq!(digest(&a), digest(&b));
        let mut c = package();
        c.insert("plugin.toml".into(), b"id = 'y'\n".to_vec());
        assert_ne!(digest(&a), digest(&c));
        let mut d = package();
        d.insert("tests/hooks/b.jsonl".into(), b"{}\n".to_vec());
        assert_ne!(digest(&a), digest(&d), "a new file changes the digest");
    }

    #[test]
    fn signatures_verify_with_current_or_next_key_and_catch_tampering() {
        let (kp1, pk1) = keypair();
        let (kp2, pk2) = keypair();
        let mut files = package();
        let sig = kp2.sign(&digest(&files));
        files.insert(SIGNATURE_FILE.into(), signature_text(&pk2, sig.as_ref()).into_bytes());
        // Signed by the "next" key: accepted while both slots are carried.
        assert_eq!(verify_with(&[pk1.clone(), pk2.clone()], &files).unwrap(), key_id(&pk2));
        assert!(verify_with(std::slice::from_ref(&pk1), &files)
            .unwrap_err()
            .contains("is not a tmuxedo plugin signing key"));
        // Tampered content.
        let mut bad = files.clone();
        bad.insert("plugin.toml".into(), b"id = 'evil'\n".to_vec());
        assert!(verify_with(std::slice::from_ref(&pk2), &bad)
            .unwrap_err()
            .contains("does not match"));
        // Signature by the right key over a different digest.
        let other = kp1.sign(b"something else");
        let mut wrong = package();
        wrong.insert(SIGNATURE_FILE.into(), signature_text(&pk1, other.as_ref()).into_bytes());
        assert!(verify_with(&[pk1], &wrong).is_err());
        // No SIGNATURE at all; a junk SIGNATURE.
        assert!(verify_with(std::slice::from_ref(&pk2), &package())
            .unwrap_err()
            .contains("missing"));
        let mut junk = package();
        junk.insert(SIGNATURE_FILE.into(), b"nope".to_vec());
        assert!(verify_with(&[pk2], &junk).is_err());
        // No key compiled in yet: every package is refused.
        assert!(builtin_keys().is_empty());
        assert!(verify(&files).unwrap_err().contains("no plugin signing key"));
    }

    #[test]
    fn read_dir_refuses_symlinks_and_reads_the_rest() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("tests/hooks")).unwrap();
        std::fs::write(d.path().join("plugin.toml"), "id = 'x'\n").unwrap();
        std::fs::write(d.path().join("tests/hooks/a.jsonl"), "{}\n").unwrap();
        let f = read_dir(d.path()).unwrap();
        assert_eq!(f.keys().collect::<Vec<_>>(), ["plugin.toml", "tests/hooks/a.jsonl"]);
        std::os::unix::fs::symlink("/etc/passwd", d.path().join("tests/link")).unwrap();
        assert!(read_dir(d.path()).unwrap_err().contains("symlinks are not allowed"));
    }
}
