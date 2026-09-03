//! JSON-RPC 2.0 message types and MCP stdio framing.
//!
//! MCP uses JSON-RPC 2.0. On stdio the transport framing is newline-delimited
//! JSON; for robustness we also accept the LSP-style `Content-Length:` header
//! framing that a few servers still emit.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Num(u64),
    Str(String),
    Null,
}

#[allow(dead_code)]
impl Id {
    pub fn num(n: u64) -> Id {
        Id::Num(n)
    }
    pub fn as_value(&self) -> Value {
        match self {
            Id::Num(n) => Value::from(*n),
            Id::Str(s) => Value::from(s.clone()),
            Id::Null => Value::Null,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Id::Null)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Success {
    pub jsonrpc: String,
    pub id: Id,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcErrorBody {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcErrorMsg {
    pub jsonrpc: String,
    pub id: Id,
    pub error: RpcErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Success(Success),
    RpcError(RpcErrorMsg),
}

#[allow(dead_code)]
impl Message {
    pub fn id(&self) -> Option<&Id> {
        match self {
            Message::Request(r) => Some(&r.id),
            Message::Success(s) => Some(&s.id),
            Message::RpcError(e) => Some(&e.id),
            Message::Notification(_) => None,
        }
    }
    /// `(id, method, params, is_notification)` for requests.
    pub fn as_request(&self) -> Option<(&Id, &str, Option<&Value>)> {
        match self {
            Message::Request(r) => Some((&r.id, &r.method, r.params.as_ref())),
            _ => None,
        }
    }
    pub fn as_success(&self) -> Option<(&Id, &Value)> {
        match self {
            Message::Success(s) => Some((&s.id, &s.result)),
            _ => None,
        }
    }
    pub fn as_error(&self) -> Option<(&Id, &RpcErrorBody)> {
        match self {
            Message::RpcError(e) => Some((&e.id, &e.error)),
            _ => None,
        }
    }
}

// --- constructors ----------------------------------------------------------

pub fn req(id: u64, method: &str, params: Value) -> Message {
    Message::Request(Request {
        jsonrpc: JSONRPC.to_string(),
        id: Id::Num(id),
        method: method.to_string(),
        params: Some(params),
    })
}

pub fn notif(method: &str, params: Option<Value>) -> Message {
    Message::Notification(Notification {
        jsonrpc: JSONRPC.to_string(),
        method: method.to_string(),
        params,
    })
}

pub fn ok(id: Id, result: Value) -> Message {
    Message::Success(Success {
        jsonrpc: JSONRPC.to_string(),
        id,
        result,
    })
}

pub fn err(id: Id, code: i64, message: impl Into<String>) -> Message {
    Message::RpcError(RpcErrorMsg {
        jsonrpc: JSONRPC.to_string(),
        id,
        error: RpcErrorBody {
            code,
            message: message.into(),
            data: None,
        },
    })
}

// --- standard JSON-RPC error codes ----------------------------------------

pub const PARSE_ERROR: i64 = -32700;
#[allow(dead_code)]
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
#[allow(dead_code)]
pub const INTERNAL_ERROR: i64 = -32603;

// --- framing ---------------------------------------------------------------

/// A frame reader over a byte stream that speaks the MCP stdio framing.
///
/// Each frame is either:
///   * a bare JSON object terminated by `\n`, or
///   * LSP-style `Content-Length: N\r\n\r\n<exactly N bytes>`.
///
/// The reader buffers partial input and yields complete frames.
pub struct FrameReader {
    buf: Vec<u8>,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub fn new() -> Self {
        FrameReader { buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame, if any. Returns `Ok(None)` when more
    /// input is required.
    pub fn next_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            // LSP-style header framing?
            if self.buf.starts_with(b"Content-Length:") {
                if let Some(header_end) = find_bytes(&self.buf, b"\r\n\r\n") {
                    let header =
                        String::from_utf8_lossy(&self.buf[..header_end]).to_string();
                    let len = parse_content_length(&header).ok_or_else(|| {
                        anyhow::anyhow!("malformed Content-Length header in frame")
                    })?;
                    let body_start = header_end + 4;
                    if self.buf.len() >= body_start + len {
                        let body = self.buf[body_start..body_start + len].to_vec();
                        self.buf.drain(..body_start + len);
                        return Ok(Some(body));
                    }
                    return Ok(None); // wait for more bytes
                }
                return Ok(None);
            }
            // Newline-delimited JSON
            if let Some(pos) = find_bytes(&self.buf, b"\n") {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                    line.pop();
                }
                // skip stray blank lines / BOM
                let mut body: &[u8] = &line;
                if let Some(stripped) = body.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
                    body = stripped;
                }
                if body.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                return Ok(Some(body.to_vec()));
            }
            return Ok(None);
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn parse_content_length(header: &str) -> Option<usize> {
    for line in header.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            return rest.trim().parse().ok();
        }
        if let Some(rest) = line.strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Serialize a message to the newline-delimited wire form.
pub fn encode(msg: &Message) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(msg).expect("message serialization cannot fail");
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_framing_roundtrip() {
        let mut fr = FrameReader::new();
        fr.push(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n{\"jsonrpc\"");
        let frame = fr.next_frame().unwrap().unwrap();
        let msg: Message = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(msg, Message::Request(r) if r.method == "ping"));

        // partial JSON still buffered
        fr.push(b":\"2.0\",\"method\":\"x\"}\n");
        let frame2 = fr.next_frame().unwrap().unwrap();
        let msg2: Message = serde_json::from_slice(&frame2).unwrap();
        assert!(matches!(msg2, Message::Notification(n) if n.method == "x"));
    }

    #[test]
    fn content_length_framing() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"t"}}"#;
        let mut fr = FrameReader::new();
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(body);
        fr.push(&framed);
        let frame = fr.next_frame().unwrap().unwrap();
        assert_eq!(frame, body);
    }

    #[test]
    fn split_content_length_framing() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), String::from_utf8_lossy(body));
        let mut fr = FrameReader::new();
        let mut got: Option<Vec<u8>> = None;
        for chunk in framed.as_bytes().chunks(4) {
            fr.push(chunk);
            if let Ok(Some(frame)) = fr.next_frame() {
                got = Some(frame);
            }
        }
        assert_eq!(got.unwrap(), body);
    }

    #[test]
    fn against_parse_invalid() {
        let mut fr = FrameReader::new();
        fr.push(b"{bad json}\n");
        let frame = fr.next_frame().unwrap().unwrap();
        assert!(serde_json::from_slice::<Message>(&frame).is_err());
    }

    #[test]
    fn bom_and_blank_lines_skipped() {
        let mut fr = FrameReader::new();
        fr.push(b"\xef\xbb\xbf{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n\n");
        let frame = fr.next_frame().unwrap().unwrap();
        let msg: Message = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(msg, Message::Notification(n) if n.method == "ping"));
    }
}