//! Files and uploads (docs/protocol.md "Files"), runner side of the wire.
//!
//! FILE frames (binary kind 5) reuse the pane-frame header: the transfer id
//! (an upload id or the gateway's download id) in the id field, the chunk
//! number (upload) or read number (download) in `req`, the byte offset in
//! the file as the offset. The control frames are `file_put` /
//! `file_put_cancel` (gateway → runner) and `file_ack` / `disk` (runner →
//! gateway). The file-name rules, chunk arithmetic and disk levels are the
//! same as `packages/protocol/src/files.ts`; both are held to
//! `packages/protocol/fixtures/files.json`.

use crate::codec::{encode_raw, parse_header, EncodeError};
use serde::{Deserialize, Serialize};

/// The hello feature of a runner that serves files.
pub const FEATURE_FILES: &str = "files";
/// The kind byte of a FILE frame.
pub const KIND_FILE: u8 = 5;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

pub const CHUNK_DEFAULT: u64 = 8 * MIB;
pub const CHUNK_MIN: u64 = 256 * KIB;
pub const CHUNK_MAX: u64 = 16 * MIB;
pub const MAX_CHUNKS: u64 = 10_000;
/// Bytes of file data per FILE frame.
pub const FRAME_PAYLOAD: usize = 512 * 1024;
/// Most bytes one `files.read` sends.
pub const READ_WINDOW: u64 = 4 * MIB;
pub const MAX_FILE_DEFAULT: u64 = 2 * GIB;
pub const MAX_FILE_CAP: u64 = 10 * GIB;
/// Partial uploads are swept this long after their last chunk.
pub const PARTIAL_TTL_MS: u64 = 24 * 3600 * 1000;
/// Inbox and outbox files are swept this long after they arrived (unless pinned).
pub const RETENTION_MS: u64 = 14 * 24 * 3600 * 1000;
/// Most characters of a stored file name.
pub const NAME_MAX: usize = 120;

/// An upload id or download transfer id: `[A-Za-z0-9_-]{8,64}`.
pub fn valid_transfer_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One FILE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFrame {
    pub id: String,
    pub req: u32,
    pub offset: u64,
    pub payload: Vec<u8>,
}

impl FileFrame {
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        if !valid_transfer_id(&self.id) {
            return Err(EncodeError::PaneIdLength);
        }
        encode_raw(KIND_FILE, 0, self.req, &self.id, self.offset, &self.payload)
    }

    /// Parse a FILE frame, or `None` for anything else. Never panics.
    pub fn decode(buf: &[u8]) -> Option<FileFrame> {
        let (kind, flags, req, id, offset, start) = parse_header(buf)?;
        if kind != KIND_FILE || flags != 0 || !valid_transfer_id(&id) {
            return None;
        }
        Some(FileFrame {
            id,
            req,
            offset,
            payload: buf[start..].to_vec(),
        })
    }
}

/// Is this binary frame a FILE frame (cheap check)?
pub fn is_file_frame(buf: &[u8]) -> bool {
    buf.first() == Some(&KIND_FILE)
}

// ── chunks ─────────────────────────────────────────────────────────────

pub fn chunk_count(size: u64, chunk_size: u64) -> u64 {
    if size == 0 || chunk_size == 0 {
        0
    } else {
        size.div_ceil(chunk_size)
    }
}

/// Bytes in chunk `n`; None when out of range.
pub fn chunk_len(size: u64, chunk_size: u64, n: u64) -> Option<u64> {
    let count = chunk_count(size, chunk_size);
    if n >= count {
        return None;
    }
    Some(if n == count - 1 {
        size - n * chunk_size
    } else {
        chunk_size
    })
}

pub fn valid_upload_shape(size: u64, chunk_size: u64, max_file: u64) -> bool {
    size <= max_file && (CHUNK_MIN..=CHUNK_MAX).contains(&chunk_size) && chunk_count(size, chunk_size) <= MAX_CHUNKS
}

pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ── names ──────────────────────────────────────────────────────────────

fn removed(c: char) -> bool {
    let u = c as u32;
    (0x202a..=0x202e).contains(&u)
        || (0x2066..=0x2069).contains(&u)
        || matches!(u, 0x200b | 0x200e | 0x200f | 0xfeff | 0x061c)
}

/// JavaScript's `\s` (what files.ts uses): Unicode White_Space plus BOM (already removed).
fn js_space(c: char) -> bool {
    c.is_whitespace()
}

/// The name an upload is stored under (before the time stamp), or None when
/// nothing usable is left; the rules of files.ts `sanitizeFileName`.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s = String::with_capacity(last.len());
    for ch in last.chars() {
        let u = ch as u32;
        if u < 0x20 || (0x7f..=0x9f).contains(&u) {
            if matches!(u, 0x09 | 0x0a | 0x0d) {
                s.push('_');
            }
            continue;
        }
        if removed(ch) {
            continue;
        }
        if js_space(ch) || "<>:\"|?*'`$".contains(ch) {
            s.push('_');
        } else {
            s.push(ch);
        }
    }
    // Runs of '_' collapse; leading [._-] and trailing [._] go.
    let mut t = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '_' && t.ends_with('_') {
            continue;
        }
        t.push(ch);
    }
    let t = t.trim_start_matches(['.', '_', '-']).trim_end_matches(['.', '_']);
    if t.is_empty() {
        return None;
    }
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= NAME_MAX {
        return Some(t.to_string());
    }
    let ext: Vec<char> = match t.rfind('.') {
        Some(i) if i > 0 => t[i..].chars().collect(),
        _ => vec![],
    };
    let keep_ext = if ext.len() > 1 && ext.len() <= 16 { ext } else { vec![] };
    let base: String = chars[..NAME_MAX - keep_ext.len()].iter().collect();
    let out = format!(
        "{}{}",
        base.trim_end_matches(['.', '_']),
        keep_ext.iter().collect::<String>()
    );
    (!out.is_empty()).then_some(out)
}

/// `2026-10-03T14-02-11Z` for a time in Unix ms.
pub fn upload_stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86400, secs % 86400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{m:02}-{s:02}Z")
}

// ── disk ───────────────────────────────────────────────────────────────

/// `ok`, `low` (< 10 % free), `critical` (< 5 %), `alert` (< 2 %).
pub fn disk_level(free: u64, total: u64) -> &'static str {
    if total == 0 {
        return "ok";
    }
    let r = free as f64 / total as f64;
    if r < 0.02 {
        "alert"
    } else if r < 0.05 {
        "critical"
    } else if r < 0.1 {
        "low"
    } else {
        "ok"
    }
}

/// Uploads are refused when less than this would be left after them.
pub fn free_space_floor(total: u64) -> u64 {
    (2 * GIB).max((total as f64 * 0.1).floor() as u64)
}

// ── control frames ─────────────────────────────────────────────────────

/// Gateway → runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum FileCtl {
    /// Chunk `n` follows as FILE frames totalling `size` bytes.
    #[serde(rename = "file_put")]
    Put {
        #[serde(rename = "uploadId")]
        upload_id: String,
        n: u32,
        size: u64,
        sha256: String,
    },
    /// The chunk's sender went away: forget what arrived of it.
    #[serde(rename = "file_put_cancel")]
    Cancel {
        #[serde(rename = "uploadId")]
        upload_id: String,
        n: u32,
    },
}

impl FileCtl {
    /// A valid gateway file frame, or None (never panics).
    pub fn parse(text: &str) -> Option<FileCtl> {
        let f: FileCtl = serde_json::from_str(text).ok()?;
        let ok = match &f {
            FileCtl::Put { upload_id, sha256, .. } => valid_transfer_id(upload_id) && valid_sha256(sha256),
            FileCtl::Cancel { upload_id, .. } => valid_transfer_id(upload_id),
        };
        ok.then_some(f)
    }
}

/// Runner → gateway: chunk `n` written and checked, or why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAck {
    pub t: String,
    #[serde(rename = "uploadId")]
    pub upload_id: String,
    pub n: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl FileAck {
    pub fn ok(upload_id: &str, n: u32, sha256: &str) -> Self {
        FileAck {
            t: "file_ack".into(),
            upload_id: upload_id.into(),
            n,
            ok: true,
            sha256: Some(sha256.into()),
            code: None,
            message: None,
        }
    }
    pub fn err(upload_id: &str, n: u32, code: &str, message: impl Into<String>) -> Self {
        FileAck {
            t: "file_ack".into(),
            upload_id: upload_id.into(),
            n,
            ok: false,
            sha256: None,
            code: Some(code.into()),
            message: Some(message.into()),
        }
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serialisable")
    }
}

/// Runner → gateway: the home disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskFrame {
    pub t: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub files_bytes: u64,
    pub level: String,
}

impl DiskFrame {
    pub fn new(free: u64, total: u64, files: u64) -> Self {
        DiskFrame {
            t: "disk".into(),
            free_bytes: free,
            total_bytes: total,
            files_bytes: files,
            level: disk_level(free, total).into(),
        }
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serialisable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps() {
        assert_eq!(upload_stamp(1_759_500_131_123), "2025-10-03T14-02-11Z");
        assert_eq!(upload_stamp(0), "1970-01-01T00-00-00Z");
        assert_eq!(upload_stamp(951_782_400_000), "2000-02-29T00-00-00Z");
    }

    #[test]
    fn ctl_frames_validate() {
        let sha = "a".repeat(64);
        let put = format!(r#"{{"t":"file_put","uploadId":"up_01234567","n":2,"size":10,"sha256":"{sha}"}}"#);
        assert!(matches!(
            FileCtl::parse(&put),
            Some(FileCtl::Put { n: 2, size: 10, .. })
        ));
        assert!(FileCtl::parse(r#"{"t":"file_put","uploadId":"up_01234567","n":2,"size":10,"sha256":"zz"}"#).is_none());
        assert!(FileCtl::parse(r#"{"t":"file_put_cancel","uploadId":"x","n":2}"#).is_none());
        assert!(FileCtl::parse("junk").is_none());
    }
}
