//! SSH wire pieces for the runner's ssh-agent (secrets-and-env.md §6a):
//! the agent protocol's framing, key blobs and signatures, `known_hosts`
//! matching, and the `session-bind@openssh.com` check that lets a key be
//! limited to named hosts.
//!
//! Keys: Ed25519 and ECDSA P-256 (private halves as PKCS#8, signed with
//! ring). Host keys (verifying a session bind): Ed25519, ECDSA P-256 and RSA.

use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, KeyPair};

pub const FAILURE: u8 = 5;
pub const SUCCESS: u8 = 6;
pub const REQUEST_IDENTITIES: u8 = 11;
pub const IDENTITIES_ANSWER: u8 = 12;
pub const SIGN_REQUEST: u8 = 13;
pub const SIGN_RESPONSE: u8 = 14;
pub const EXTENSION: u8 = 27;

pub const ED25519: &str = "ssh-ed25519";
pub const ECDSA_P256: &str = "ecdsa-sha2-nistp256";

/// A reader over SSH wire data (u32 big-endian lengths).
pub struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b }
    }
    pub fn u8(&mut self) -> Option<u8> {
        let (&x, rest) = self.b.split_first()?;
        self.b = rest;
        Some(x)
    }
    pub fn u32(&mut self) -> Option<u32> {
        if self.b.len() < 4 {
            return None;
        }
        let (h, rest) = self.b.split_at(4);
        self.b = rest;
        Some(u32::from_be_bytes(h.try_into().ok()?))
    }
    pub fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        if self.b.len() < n {
            return None;
        }
        let (s, rest) = self.b.split_at(n);
        self.b = rest;
        Some(s)
    }
    pub fn bool(&mut self) -> Option<bool> {
        self.u8().map(|x| x != 0)
    }
    pub fn rest(&self) -> &'a [u8] {
        self.b
    }
}

pub fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// An unsigned big integer as an SSH mpint.
pub fn put_mpint(out: &mut Vec<u8>, n: &[u8]) {
    let n = &n[n.iter().position(|&b| b != 0).unwrap_or(n.len())..];
    let mut v = vec![];
    if n.first().is_some_and(|&b| b & 0x80 != 0) {
        v.push(0);
    }
    v.extend_from_slice(n);
    put_string(out, &v);
}

/// One framed agent message (length prefix included).
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

/// A private key the agent can sign with.
pub enum PrivateKey {
    Ed25519(Ed25519KeyPair),
    EcdsaP256(EcdsaKeyPair),
}

impl std::fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PrivateKey::Ed25519(_) => "PrivateKey(ed25519)",
            PrivateKey::EcdsaP256(_) => "PrivateKey(ecdsa-p256)",
        })
    }
}

impl PrivateKey {
    /// From PKCS#8 DER (Ed25519 v1 or v2, ECDSA P-256).
    pub fn from_pkcs8(der: &[u8]) -> Result<PrivateKey, String> {
        if let Ok(k) = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der) {
            return Ok(PrivateKey::Ed25519(k));
        }
        EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, der, &SystemRandom::new())
            .map(PrivateKey::EcdsaP256)
            .map_err(|_| "unsupported key (Ed25519 or ECDSA P-256 in PKCS#8)".into())
    }

    /// A new Ed25519 key: (PKCS#8 DER, key).
    pub fn generate_ed25519() -> Result<(zeroize::Zeroizing<Vec<u8>>, PrivateKey), String> {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| "random")?;
        let der = zeroize::Zeroizing::new(doc.as_ref().to_vec());
        let k = Self::from_pkcs8(&der)?;
        Ok((der, k))
    }

    pub fn algorithm(&self) -> &'static str {
        match self {
            PrivateKey::Ed25519(_) => ED25519,
            PrivateKey::EcdsaP256(_) => ECDSA_P256,
        }
    }

    /// The public key blob (as in `authorized_keys`, decoded).
    pub fn blob(&self) -> Vec<u8> {
        let mut b = vec![];
        match self {
            PrivateKey::Ed25519(k) => {
                put_string(&mut b, ED25519.as_bytes());
                put_string(&mut b, k.public_key().as_ref());
            }
            PrivateKey::EcdsaP256(k) => {
                put_string(&mut b, ECDSA_P256.as_bytes());
                put_string(&mut b, b"nistp256");
                put_string(&mut b, k.public_key().as_ref());
            }
        }
        b
    }

    /// An SSH signature blob over `data`.
    pub fn sign(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = vec![];
        match self {
            PrivateKey::Ed25519(k) => {
                put_string(&mut out, ED25519.as_bytes());
                put_string(&mut out, k.sign(data).as_ref());
            }
            PrivateKey::EcdsaP256(k) => {
                let sig = k.sign(&SystemRandom::new(), data).map_err(|_| "sign")?;
                let (r, s) = sig.as_ref().split_at(32);
                let mut inner = vec![];
                put_mpint(&mut inner, r);
                put_mpint(&mut inner, s);
                put_string(&mut out, ECDSA_P256.as_bytes());
                put_string(&mut out, &inner);
            }
        }
        Ok(out)
    }
}

/// `<alg> <base64 blob> <comment>`.
pub fn authorized_line(blob: &[u8], comment: &str) -> String {
    let alg = Reader::new(blob)
        .string()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .unwrap_or_default();
    format!(
        "{alg} {} {comment}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    )
    .trim_end()
    .to_string()
}

/// `SHA256:<base64, no padding>` of a key blob (what `ssh-keygen -l` shows).
pub fn fingerprint(blob: &[u8]) -> String {
    use sha2::Digest;
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha2::Sha256::digest(blob))
    )
}

/// The key blob of an `authorized_keys` line.
pub fn blob_of_line(line: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(line.split_whitespace().nth(1)?)
        .ok()
}

/// Verify an SSH signature blob by host key `key_blob` over `data`.
pub fn verify(key_blob: &[u8], data: &[u8], sig_blob: &[u8]) -> bool {
    let mut k = Reader::new(key_blob);
    let mut s = Reader::new(sig_blob);
    let (Some(kalg), Some(salg), Some(sig)) = (k.string(), s.string(), s.string()) else {
        return false;
    };
    match (kalg, salg) {
        (b"ssh-ed25519", b"ssh-ed25519") => {
            let Some(pk) = k.string() else { return false };
            signature::UnparsedPublicKey::new(&signature::ED25519, pk)
                .verify(data, sig)
                .is_ok()
        }
        (b"ecdsa-sha2-nistp256", b"ecdsa-sha2-nistp256") => {
            let (Some(_curve), Some(q)) = (k.string(), k.string()) else {
                return false;
            };
            let mut r = Reader::new(sig);
            let (Some(rr), Some(ss)) = (r.string(), r.string()) else {
                return false;
            };
            let fixed = |x: &[u8]| -> Option<[u8; 32]> {
                let x = &x[x.iter().position(|&b| b != 0).unwrap_or(x.len())..];
                if x.len() > 32 {
                    return None;
                }
                let mut o = [0u8; 32];
                o[32 - x.len()..].copy_from_slice(x);
                Some(o)
            };
            let (Some(r32), Some(s32)) = (fixed(rr), fixed(ss)) else {
                return false;
            };
            let mut rs = r32.to_vec();
            rs.extend_from_slice(&s32);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, q)
                .verify(data, &rs)
                .is_ok()
        }
        (b"ssh-rsa", alg) => {
            let (Some(e), Some(n)) = (k.string(), k.string()) else {
                return false;
            };
            let params = match alg {
                b"rsa-sha2-512" => &signature::RSA_PKCS1_2048_8192_SHA512,
                b"rsa-sha2-256" => &signature::RSA_PKCS1_2048_8192_SHA256,
                _ => return false,
            };
            // mpints carry a leading zero when the top bit is set; ring wants none.
            let strip = |x: &[u8]| x[x.iter().position(|&b| b != 0).unwrap_or(x.len())..].to_vec();
            signature::RsaPublicKeyComponents {
                n: strip(n),
                e: strip(e),
            }
            .verify(params, data, sig)
            .is_ok()
        }
        _ => false,
    }
}

/// What a `session-bind@openssh.com` extension proved: the host key the
/// client verified for this connection and the session it authenticates.
#[derive(Debug, Clone, PartialEq)]
pub struct Bind {
    pub host_key: Vec<u8>,
    pub session_id: Vec<u8>,
    pub forwarding: bool,
}

/// Parse and check a session bind (the host signed the session id).
pub fn session_bind(contents: &[u8]) -> Option<Bind> {
    let mut r = Reader::new(contents);
    let host_key = r.string()?.to_vec();
    let session_id = r.string()?.to_vec();
    let sig = r.string()?;
    let forwarding = r.bool()?;
    verify(&host_key, &session_id, sig).then_some(Bind {
        host_key,
        session_id,
        forwarding,
    })
}

/// The session id a userauth request to be signed names (`SSH_MSG_USERAUTH_REQUEST`).
pub fn userauth_session(data: &[u8]) -> Option<&[u8]> {
    let mut r = Reader::new(data);
    let sid = r.string()?;
    (r.u8()? == 50).then_some(sid)
}

/// An `ssh-keygen -Y sign` request (commit signing): data starts with `SSHSIG`.
pub fn is_sshsig(data: &[u8]) -> bool {
    data.starts_with(b"SSHSIG")
}

/// Host keys `known_hosts` lists for `host` (`host` or `[host]:port`; hashed
/// entries too); `@revoked` keys are returned separately.
pub fn known_keys(known_hosts: &str, host: &str) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut keys = vec![];
    let mut revoked = vec![];
    for line in known_hosts.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let mut first = parts.next().unwrap_or("");
        let marker = first.starts_with('@').then(|| first.to_string());
        if marker.is_some() {
            first = parts.next().unwrap_or("");
        }
        if marker.as_deref() == Some("@cert-authority") {
            continue;
        }
        let (Some(_alg), Some(b64)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !first.split(',').any(|h| host_matches(h, host)) {
            continue;
        }
        let Ok(blob) = base64::engine::general_purpose::STANDARD.decode(b64) else {
            continue;
        };
        if marker.as_deref() == Some("@revoked") {
            revoked.push(blob);
        } else {
            keys.push(blob);
        }
    }
    (keys, revoked)
}

fn host_matches(pattern: &str, host: &str) -> bool {
    if let Some(h) = pattern.strip_prefix("|1|") {
        let Some((salt, hash)) = h.split_once('|') else {
            return false;
        };
        let e = &base64::engine::general_purpose::STANDARD;
        let (Ok(salt), Ok(hash)) = (e.decode(salt), e.decode(hash)) else {
            return false;
        };
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        return ring::hmac::verify(&key, host.as_bytes(), &hash).is_ok();
    }
    !pattern.starts_with('!') && pattern.eq_ignore_ascii_case(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sign_and_verify() {
        let (der, k) = PrivateKey::generate_ed25519().unwrap();
        let blob = k.blob();
        let sig = k.sign(b"hello").unwrap();
        assert!(verify(&blob, b"hello", &sig));
        assert!(!verify(&blob, b"hellO", &sig));
        let again = PrivateKey::from_pkcs8(&der).unwrap();
        assert_eq!(again.blob(), blob);
        let line = authorized_line(&blob, "tmuxedo laptop");
        assert!(line.starts_with("ssh-ed25519 AAAA") && line.ends_with(" tmuxedo laptop"));
        assert_eq!(blob_of_line(&line).unwrap(), blob);
        assert!(fingerprint(&blob).starts_with("SHA256:"));
        let ec =
            EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).unwrap();
        let ek = PrivateKey::from_pkcs8(ec.as_ref()).unwrap();
        assert_eq!(ek.algorithm(), ECDSA_P256);
        let s = ek.sign(b"data").unwrap();
        assert!(verify(&ek.blob(), b"data", &s));
        assert!(!verify(&ek.blob(), b"other", &s));
        assert!(PrivateKey::from_pkcs8(b"junk").is_err());
    }

    #[test]
    fn session_binds_are_checked() {
        let (_, host) = PrivateKey::generate_ed25519().unwrap();
        let sid = b"session-identifier-0123456789".to_vec();
        let mut ext = vec![];
        put_string(&mut ext, &host.blob());
        put_string(&mut ext, &sid);
        put_string(&mut ext, &host.sign(&sid).unwrap());
        ext.push(0);
        let b = session_bind(&ext).unwrap();
        assert_eq!(
            (b.host_key, b.session_id, b.forwarding),
            (host.blob(), sid.clone(), false)
        );
        // A signature by another key over the same id is refused.
        let (_, other) = PrivateKey::generate_ed25519().unwrap();
        let mut bad = vec![];
        put_string(&mut bad, &host.blob());
        put_string(&mut bad, &sid);
        put_string(&mut bad, &other.sign(&sid).unwrap());
        bad.push(0);
        assert!(session_bind(&bad).is_none());
        let mut ua = vec![];
        put_string(&mut ua, &sid);
        ua.push(50);
        assert_eq!(userauth_session(&ua), Some(sid.as_slice()));
        assert!(is_sshsig(b"SSHSIG\0\0\0\x01"));
    }

    #[test]
    fn known_hosts_plain_bracketed_hashed_and_revoked() {
        let (_, a) = PrivateKey::generate_ed25519().unwrap();
        let (_, b) = PrivateKey::generate_ed25519().unwrap();
        let line_a = authorized_line(&a.blob(), "");
        let line_b = authorized_line(&b.blob(), "");
        // A hashed entry for "hashed.example".
        let salt = b"0123456789abcdefghij";
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, salt);
        let tag = ring::hmac::sign(&key, b"hashed.example");
        let e = &base64::engine::general_purpose::STANDARD;
        let hashed = format!("|1|{}|{}", e.encode(salt), e.encode(tag.as_ref()));
        let kh = format!(
            "# comment\ngithub.com,140.82.1.1 {line_a}\n[127.0.0.1]:2222 {line_b}\n{hashed} {line_a}\n@revoked evil.example {line_b}\n@cert-authority *.corp {line_a}\n"
        );
        assert_eq!(known_keys(&kh, "github.com").0, vec![a.blob()]);
        assert_eq!(known_keys(&kh, "GitHub.com").0, vec![a.blob()]);
        assert_eq!(known_keys(&kh, "[127.0.0.1]:2222").0, vec![b.blob()]);
        assert_eq!(known_keys(&kh, "hashed.example").0, vec![a.blob()]);
        assert!(known_keys(&kh, "gitlab.com").0.is_empty());
        assert_eq!(known_keys(&kh, "evil.example").1, vec![b.blob()]);
        let mut m = vec![];
        put_mpint(&mut m, &[0x80, 1]);
        assert_eq!(m, [0, 0, 0, 3, 0, 0x80, 1]);
    }
}
