//! Holder socket protocol, version [`HOLDER_PROTO`].
//!
//! Every message is `u32 BE length | u8 type | body`, where length counts the
//! type byte and the body. Type 1 is a JSON object (`op` names it); type 2 is
//! pane data: `u8 idLen | id | u64 BE offset | bytes` (holder → client:
//! output at that ring offset; client → holder: input, offset 0).
//!
//! The first message on a connection must be `hello` with the client's
//! protocol version; the holder answers with its own and closes on a mismatch.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

pub const HOLDER_PROTO: u32 = 1;
pub const MAX_MSG: usize = 4 * 1024 * 1024;
const T_JSON: u8 = 1;
const T_DATA: u8 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub id: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    #[serde(default)]
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: String,
    pub pid: u32,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// Oldest offset still in the ring.
    pub start: u64,
    /// Offset after the newest byte.
    pub end: u64,
    pub exited: Option<ExitInfo>,
    /// A pipe child (`spawn_pipe`): stdin/stdout pipes, no terminal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pipe: bool,
}

/// Client → holder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello {
        proto: u32,
        #[serde(default)]
        client: String,
    },
    Spawn {
        rid: u64,
        spec: SpawnSpec,
    },
    /// A **pipe child** (agent chat, docs/design/agent-chat.md §3.1): the
    /// program gets pipes instead of a terminal. Its stdout goes into the
    /// ring like pane output (attach/offsets/replay are the same), input
    /// frames go to its stdin, stderr is appended to the file `stderr` (or
    /// dropped), `cols`/`rows` are ignored. It survives the runner like a
    /// pane does. Older holders ignore this op (the runner times out and
    /// says the holder needs a restart).
    SpawnPipe {
        rid: u64,
        spec: SpawnSpec,
        #[serde(default)]
        stderr: Option<String>,
    },
    List {
        rid: u64,
    },
    /// Stream pane `id` from offset `from` (clamped to the ring), then live.
    Attach {
        rid: u64,
        id: String,
        from: u64,
    },
    Detach {
        id: String,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Signal the pane's process group.
    Kill {
        rid: u64,
        id: String,
        signal: i32,
    },
    /// Forget an exited pane.
    Remove {
        rid: u64,
        id: String,
    },
}

/// Holder → client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Event {
    Hello {
        proto: u32,
        version: String,
        pid: u32,
        /// [`crate::BUILD`]; absent from holders built before it existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
    },
    Ok {
        rid: u64,
        #[serde(default)]
        data: Value,
    },
    Error {
        rid: u64,
        message: String,
    },
    /// Sent before any data for the attachment. `gap` = `from` was older than the ring.
    Attached {
        rid: u64,
        id: String,
        from: u64,
        gap: bool,
        end: u64,
    },
    Exit {
        id: String,
        #[serde(flatten)]
        info: ExitInfo,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Json(Vec<u8>),
    Data { id: String, offset: u64, bytes: Vec<u8> },
}

pub fn encode_json<T: Serialize>(v: &T) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::with_capacity(5 + body.len());
    out.extend_from_slice(&((body.len() + 1) as u32).to_be_bytes());
    out.push(T_JSON);
    out.extend_from_slice(&body);
    out
}

pub fn encode_data(id: &str, offset: u64, bytes: &[u8]) -> Vec<u8> {
    let idb = &id.as_bytes()[..id.len().min(255)];
    let len = 1 + 1 + idb.len() + 8 + bytes.len();
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.push(T_DATA);
    out.push(idb.len() as u8);
    out.extend_from_slice(idb);
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Parse a message body (after the length prefix).
pub fn decode_body(body: &[u8]) -> io::Result<Msg> {
    match body.first() {
        Some(&T_JSON) => Ok(Msg::Json(body[1..].to_vec())),
        Some(&T_DATA) => {
            let id_len = *body.get(1).ok_or_else(|| bad("short data"))? as usize;
            if body.len() < 2 + id_len + 8 {
                return Err(bad("short data"));
            }
            let id = std::str::from_utf8(&body[2..2 + id_len])
                .map_err(|_| bad("bad id"))?
                .to_owned();
            let offset = u64::from_be_bytes(body[2 + id_len..10 + id_len].try_into().unwrap());
            Ok(Msg::Data {
                id,
                offset,
                bytes: body[10 + id_len..].to_vec(),
            })
        }
        _ => Err(bad("unknown message type")),
    }
}

pub fn read_msg(r: &mut impl Read) -> io::Result<Msg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_MSG {
        return Err(bad("bad length"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    decode_body(&body)
}

pub fn write_all(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_and_data_round_trip() {
        let req = Request::Attach {
            rid: 3,
            id: "w1:p1#1".into(),
            from: 42,
        };
        let enc = encode_json(&req);
        let Msg::Json(j) = read_msg(&mut &enc[..]).unwrap() else {
            panic!()
        };
        assert_eq!(serde_json::from_slice::<Request>(&j).unwrap(), req);
        let enc = encode_data("p", 1 << 40, b"hello");
        assert_eq!(
            read_msg(&mut &enc[..]).unwrap(),
            Msg::Data {
                id: "p".into(),
                offset: 1 << 40,
                bytes: b"hello".to_vec()
            }
        );
        let ev = Event::Exit {
            id: "p".into(),
            info: ExitInfo {
                code: Some(3),
                signal: None,
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), ev);
    }

    #[test]
    fn junk_is_an_error_not_a_panic() {
        assert!(read_msg(&mut &[0u8, 0, 0, 0][..]).is_err());
        assert!(read_msg(&mut &[0u8, 0, 0, 2, 9, 9][..]).is_err());
        assert!(read_msg(&mut &[0u8, 0, 0, 3, 2, 5, 1][..]).is_err());
    }
}
