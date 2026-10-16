//! Binary data frames.
//!
//! ```text
//! offset  size  field
//! 0       1     kind     1 OUTPUT  2 INPUT  3 SNAPSHOT  4 CHAT
//! 1       1     flags    bit0 GAP; bit1 EPHEMERAL (CHAT only); others reserved = 0
//! 2       4     req      u32 BE
//! 6       1     idLen    1..255
//! 7       n     paneId   UTF-8
//! 7+n     8     offset   u64 BE
//! 15+n    ...   payload
//! ```
//!
//! CHAT frames (kind 4) reuse the header with the chat session id in the id
//! field and the event's `seq` as the offset; [`crate::chat::ChatFrame`]
//! encodes and decodes them. [`DataFrame::decode`] refuses them: a pane
//! stream never carries one.

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const FLAG_GAP: u8 = 0x01;
/// CHAT only: a token delta that is not stored and has no seq of its own.
pub const FLAG_EPHEMERAL: u8 = 0x02;
/// The kind byte of a CHAT frame.
pub const KIND_CHAT: u8 = 4;
pub(crate) const HEADER_FIXED: usize = 1 + 1 + 4 + 1 + 8;

/// The fixed header fields of any binary frame: (kind, flags, req, id, offset, payload start).
pub(crate) fn parse_header(buf: &[u8]) -> Option<(u8, u8, u32, String, u64, usize)> {
    if buf.len() < HEADER_FIXED + 1 || buf.len() > MAX_FRAME_BYTES {
        return None;
    }
    let kind = buf[0];
    let flags = buf[1];
    let req = u32::from_be_bytes(buf[2..6].try_into().ok()?);
    let id_len = buf[6] as usize;
    if id_len < 1 || buf.len() < HEADER_FIXED + id_len {
        return None;
    }
    let id = std::str::from_utf8(&buf[7..7 + id_len]).ok()?.to_owned();
    let offset = u64::from_be_bytes(buf[7 + id_len..15 + id_len].try_into().ok()?);
    Some((kind, flags, req, id, offset, HEADER_FIXED + id_len))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DataKind {
    Output = 1,
    Input = 2,
    Snapshot = 3,
}

impl DataKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Output),
            2 => Some(Self::Input),
            3 => Some(Self::Snapshot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrame {
    pub kind: DataKind,
    /// Bytes before this frame were lost (the ring wrapped).
    pub gap: bool,
    /// 0 = live broadcast; otherwise the attach request this answers.
    pub req: u32,
    pub pane_id: String,
    /// Stream offset of the first payload byte (SNAPSHOT: where live output resumes; INPUT: 0).
    pub offset: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    PaneIdLength,
    TooLarge,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PaneIdLength => write!(f, "paneId must be 1..255 UTF-8 bytes"),
            Self::TooLarge => write!(f, "frame exceeds MAX_FRAME_BYTES"),
        }
    }
}
impl std::error::Error for EncodeError {}

/// Largest payload that fits in one frame for a given pane id.
pub fn max_payload(pane_id: &str) -> usize {
    MAX_FRAME_BYTES - HEADER_FIXED - pane_id.len()
}

impl DataFrame {
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        encode_parts(self.kind, self.gap, self.req, &self.pane_id, self.offset, &self.payload)
    }

    /// Parse a binary frame, or `None` if it is not a valid v1 data frame. Never panics.
    pub fn decode(buf: &[u8]) -> Option<DataFrame> {
        let (kind, flags, req, pane_id, offset, start) = parse_header(buf)?;
        let kind = DataKind::from_u8(kind)?;
        if flags & !FLAG_GAP != 0 {
            return None;
        }
        Some(DataFrame {
            kind,
            gap: flags & FLAG_GAP != 0,
            req,
            pane_id,
            offset,
            payload: buf[start..].to_vec(),
        })
    }
}

/// Encode without first building a [`DataFrame`] (avoids copying the payload).
pub fn encode_parts(
    kind: DataKind,
    gap: bool,
    req: u32,
    pane_id: &str,
    offset: u64,
    payload: &[u8],
) -> Result<Vec<u8>, EncodeError> {
    encode_raw(
        kind as u8,
        if gap { FLAG_GAP } else { 0 },
        req,
        pane_id,
        offset,
        payload,
    )
}

pub(crate) fn encode_raw(
    kind: u8,
    flags: u8,
    req: u32,
    pane_id: &str,
    offset: u64,
    payload: &[u8],
) -> Result<Vec<u8>, EncodeError> {
    let id = pane_id.as_bytes();
    if id.is_empty() || id.len() > 255 {
        return Err(EncodeError::PaneIdLength);
    }
    let len = HEADER_FIXED + id.len() + payload.len();
    if len > MAX_FRAME_BYTES {
        return Err(EncodeError::TooLarge);
    }
    let mut out = Vec::with_capacity(len);
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&req.to_be_bytes());
    out.push(id.len() as u8);
    out.extend_from_slice(id);
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversize_and_bad_ids() {
        assert_eq!(
            encode_parts(DataKind::Output, false, 0, "", 0, b"x"),
            Err(EncodeError::PaneIdLength)
        );
        let long = "x".repeat(256);
        assert_eq!(
            encode_parts(DataKind::Output, false, 0, &long, 0, b"x"),
            Err(EncodeError::PaneIdLength)
        );
        let big = vec![0u8; max_payload("p") + 1];
        assert_eq!(
            encode_parts(DataKind::Output, false, 0, "p", 0, &big),
            Err(EncodeError::TooLarge)
        );
        let fits = vec![0u8; max_payload("p")];
        let enc = encode_parts(DataKind::Output, false, 0, "p", 0, &fits).unwrap();
        assert_eq!(enc.len(), MAX_FRAME_BYTES);
        assert!(DataFrame::decode(&enc).is_some());
    }

    #[test]
    fn decode_never_panics_on_prefixes() {
        let f = DataFrame {
            kind: DataKind::Snapshot,
            gap: true,
            req: 9,
            pane_id: "w1:p1".into(),
            offset: 77,
            payload: b"abc".to_vec(),
        };
        let enc = f.encode().unwrap();
        for i in 0..enc.len() {
            let _ = DataFrame::decode(&enc[..i]);
        }
        assert_eq!(DataFrame::decode(&enc), Some(f));
    }
}
