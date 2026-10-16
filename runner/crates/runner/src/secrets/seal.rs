//! Values sealed to this runner (docs/design/secrets-and-env.md §3.3,
//! docs/protocol.md "Secrets").
//!
//! The runner has a long-lived X25519 key pair; web seals each delivery to
//! its public key, so the gateway and Cloudflare relay only ciphertext:
//!
//! ```text
//! sealed = "x1." b64url(epk) "." b64url(nonce) "." b64url(ciphertext ‖ tag)
//! key    = HKDF-SHA256(salt = epk ‖ runner_pk, ikm = X25519(esk, runner_pk),
//!                      info = "tmuxedo-secret-v1")
//! AEAD   = ChaCha20-Poly1305(key, nonce, aad = context)
//! ```
//!
//! `context` binds a blob to its use (`<machineId>|req:<id>` for an answer,
//! `<machineId>|mask` for the mask list), so a ciphertext replayed into
//! another answer, or to another machine, does not open.
//!
//! The private key is a 0600 file in the state directory, read at start.
//! It never leaves the machine and is never logged.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::hkdf;
use std::path::Path;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

pub const ALG: &str = "x25519-hkdf-sha256-chacha20poly1305";
const INFO: &[u8] = b"tmuxedo-secret-v1";
const PREFIX: &str = "x1";

pub struct RunnerKey {
    secret: StaticSecret,
    public: PublicKey,
}

impl std::fmt::Debug for RunnerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunnerKey({})", self.public_b64())
    }
}

impl RunnerKey {
    pub fn generate() -> RunnerKey {
        let mut b = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(b.as_mut()).expect("system random");
        Self::from_bytes(*b)
    }

    fn from_bytes(b: [u8; 32]) -> RunnerKey {
        let secret = StaticSecret::from(b);
        let public = PublicKey::from(&secret);
        RunnerKey { secret, public }
    }

    /// Load the key at `path`, or create it (0600) if there is none. A
    /// corrupt file is replaced (web re-registers the new public key; values
    /// sealed to the old one simply fail to open and are asked for again).
    pub fn load_or_create(path: &Path) -> Result<RunnerKey, String> {
        if let Ok(raw) = std::fs::read(path) {
            let raw = Zeroizing::new(raw);
            if let Ok(text) = std::str::from_utf8(&raw) {
                if let Ok(bytes) = B64.decode(text.trim()) {
                    if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                        let k = Self::from_bytes(arr);
                        let mut bytes = bytes;
                        zeroize::Zeroize::zeroize(&mut bytes);
                        return Ok(k);
                    }
                }
            }
            crate::log!("secrets: {} is unreadable; making a new key", path.display());
        }
        let k = Self::generate();
        let text = Zeroizing::new(B64.encode(k.secret.to_bytes()));
        crate::config::write_private(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(k)
    }

    /// From a base64url private key (fixtures and tests).
    pub fn from_b64(s: &str) -> Option<RunnerKey> {
        let b = B64.decode(s).ok()?;
        Some(Self::from_bytes(<[u8; 32]>::try_from(b.as_slice()).ok()?))
    }

    pub fn public_b64(&self) -> String {
        B64.encode(self.public.as_bytes())
    }

    /// Open a blob sealed to this key under `context`.
    pub fn open(&self, sealed: &str, context: &str) -> Result<Zeroizing<Vec<u8>>, String> {
        let mut parts = sealed.split('.');
        let (Some(PREFIX), Some(epk), Some(nonce), Some(ct), None) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("not a sealed value".into());
        };
        let epk = <[u8; 32]>::try_from(B64.decode(epk).map_err(|_| "bad key")?.as_slice()).map_err(|_| "bad key")?;
        let nonce =
            <[u8; 12]>::try_from(B64.decode(nonce).map_err(|_| "bad nonce")?.as_slice()).map_err(|_| "bad nonce")?;
        let mut buf = Zeroizing::new(B64.decode(ct).map_err(|_| "bad ciphertext")?);
        let shared = self.secret.diffie_hellman(&PublicKey::from(epk));
        if !shared.was_contributory() {
            return Err("degenerate key".into());
        }
        let key = derive(shared.as_bytes(), &epk, self.public.as_bytes())?;
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(context.as_bytes()),
                &mut buf,
            )
            .map_err(|_| "does not open (wrong key or context)")?;
        Ok(Zeroizing::new(plain.to_vec()))
    }
}

fn derive(shared: &[u8], epk: &[u8; 32], rpk: &[u8; 32]) -> Result<LessSafeKey, String> {
    let mut salt = Vec::with_capacity(64);
    salt.extend_from_slice(epk);
    salt.extend_from_slice(rpk);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(shared);
    let okm = prk.expand(&[INFO], &CHACHA20_POLY1305).map_err(|_| "hkdf")?;
    Ok(LessSafeKey::new(UnboundKey::from(okm)))
}

/// Seal `plain` to `public` (web's side; here for tests and the fixtures).
pub fn seal(public_b64: &str, plain: &[u8], context: &str) -> Result<String, String> {
    let rpk = <[u8; 32]>::try_from(B64.decode(public_b64).map_err(|_| "bad key")?.as_slice()).map_err(|_| "bad key")?;
    let eph = RunnerKey::generate();
    let shared = eph.secret.diffie_hellman(&PublicKey::from(rpk));
    let key = derive(shared.as_bytes(), eph.public.as_bytes(), &rpk)?;
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?;
    let mut buf = plain.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(context.as_bytes()),
        &mut buf,
    )
    .map_err(|_| "seal")?;
    Ok(format!(
        "{PREFIX}.{}.{}.{}",
        B64.encode(eph.public.as_bytes()),
        B64.encode(nonce),
        B64.encode(&buf)
    ))
}

/// The context string of an answer to request `id` on `machine`.
pub fn answer_context(machine: &str, id: &str) -> String {
    format!("{machine}|req:{id}")
}

pub fn mask_context(machine: &str) -> String {
    format!("{machine}|mask")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_binds_the_context() {
        let k = RunnerKey::generate();
        let s = seal(&k.public_b64(), b"value-123456", "m1|req:7").unwrap();
        assert_eq!(k.open(&s, "m1|req:7").unwrap().as_slice(), b"value-123456");
        assert!(k.open(&s, "m1|req:8").is_err(), "another request");
        assert!(k.open(&s, "m2|req:7").is_err(), "another machine");
        assert!(RunnerKey::generate().open(&s, "m1|req:7").is_err(), "another runner");
        let mut bad = s.clone();
        bad.pop();
        bad.push(if s.ends_with('A') { 'B' } else { 'A' });
        assert!(k.open(&bad, "m1|req:7").is_err(), "tampered");
        assert!(k.open("x1.a.b", "m1|req:7").is_err());
        assert!(!s.contains("value-123456"));
    }

    #[test]
    fn key_file_is_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("secret-key");
        let a = RunnerKey::load_or_create(&p).unwrap();
        let b = RunnerKey::load_or_create(&p).unwrap();
        assert_eq!(a.public_b64(), b.public_b64());
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&p, "junk").unwrap();
        let c = RunnerKey::load_or_create(&p).unwrap();
        assert_ne!(a.public_b64(), c.public_b64());
    }
}
