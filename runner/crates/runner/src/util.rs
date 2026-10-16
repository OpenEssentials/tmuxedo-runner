//! Small helpers with no better home: time formatting, host facts, tokens.

use base64::Engine;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// RFC 3339 UTC with milliseconds, e.g. `2026-10-02T10:00:00.123Z`.
pub fn iso(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60,
        ms % 1000
    )
}

pub fn iso_now() -> String {
    iso(now_ms())
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "unknown".into()
}

/// `linux` / `darwin`, as used in download names and the manifest.
pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    }
}

pub fn arch_name() -> &'static str {
    std::env::consts::ARCH
}

/// Manifest key / download suffix, e.g. `linux-x86_64`.
pub fn platform_key() -> String {
    format!("{}-{}", os_name(), arch_name())
}

/// The user's login shell: `$SHELL`, else the passwd entry, else `/bin/sh`.
pub fn login_shell() -> String {
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() {
            return s;
        }
    }
    // SAFETY: getpwuid returns a pointer to static storage or null; read immediately.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            let s = std::ffi::CStr::from_ptr((*pw).pw_shell).to_string_lossy().into_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "/bin/sh".into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("OS random source");
    b
}

pub fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn random_id() -> String {
    hex::encode(random_bytes(12))
}

/// `claude --version` → `2.1.288`, if Claude Code is installed (the Claude
/// plugin's `agent.version_cmd`, run with its scrubbed env).
pub async fn claude_version() -> Option<String> {
    crate::plugin::agent_version(crate::plugin::claude()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_790_000_000_123), "2026-09-21T14:13:20.123Z");
        assert_eq!(iso(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }
}
