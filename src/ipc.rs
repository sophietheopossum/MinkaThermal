//! The socket protocol (§12.3): NDJSON over a Unix socket, in MinkaSyu's `handle_line` shape.
//!
//! - request: `{"id":1,"method":"…","params":{…}}`; a request with no id gets no reply;
//! - reply: `{"id":1,"result":…}` or `{"id":1,"error":{"code":"…","message":"…"}}`;
//! - broadcast: `{"event":"…","payload":…}`.
//!
//! This module holds the wire format, the service's per-client buffers and the CLI's blocking
//! `call`. What each method does is the service's business (`service.rs`).

use serde::Serialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// A request line longer than this is not a request: the client is dropped. The longest real one
/// is a few hundred bytes.
pub const MAX_LINE: usize = 64 * 1024;
/// A client this far behind on its replies and events is dropped (§12.3), so a window that stopped
/// reading cannot make the service hold an ever-growing buffer.
pub const MAX_BEHIND: usize = 1 << 20;
/// How long the CLI waits for a reply. Every method answers at once; this only bounds a wedged
/// service.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// An error reply. The codes are a fixed vocabulary: `bad_request`, `unknown_method`, `refused`,
/// `busy` and `internal`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcError {
    pub code: String,
    pub message: String,
}

impl IpcError {
    fn new(code: &str, message: impl Into<String>) -> IpcError {
        IpcError {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> IpcError {
        IpcError::new("bad_request", message)
    }

    pub fn unknown_method(method: &str) -> IpcError {
        IpcError::new("unknown_method", format!("no such method: {method}"))
    }

    /// The request was understood but does not apply now; the message says why, in words she
    /// can read.
    pub fn refused(message: impl Into<String>) -> IpcError {
        IpcError::new("refused", message)
    }

    pub fn busy(message: impl Into<String>) -> IpcError {
        IpcError::new("busy", message)
    }

    pub fn internal(message: impl Into<String>) -> IpcError {
        IpcError::new("internal", message)
    }
}

/// A parsed request. `id` is None for fire-and-forget requests, which get no reply.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: Option<Value>,
    pub method: String,
    /// Always an object (`{}` when the request had none).
    pub params: Value,
}

/// Parses one request line. On an error the id is returned too when it could be read, so the
/// reply can still be matched; a line that is not JSON at all is answered with id null.
pub fn parse_request(line: &[u8]) -> Result<Request, (Option<Value>, IpcError)> {
    let value: Value = serde_json::from_slice(line)
        .map_err(|e| (Some(Value::Null), IpcError::bad_request(format!("not JSON: {e}"))))?;
    let Value::Object(mut map) = value else {
        return Err((
            Some(Value::Null),
            IpcError::bad_request("a request is a JSON object"),
        ));
    };
    let id = map.remove("id").filter(|v| !v.is_null());
    let method = match map.remove("method") {
        Some(Value::String(m)) => m,
        _ => return Err((id, IpcError::bad_request("the request has no method"))),
    };
    let params = match map.remove("params") {
        None | Some(Value::Null) => json!({}),
        Some(p @ Value::Object(_)) => p,
        Some(_) => return Err((id, IpcError::bad_request("params must be an object"))),
    };
    Ok(Request { id, method, params })
}

/// A reply line, newline included.
pub fn reply_line(id: &Value, result: Result<Value, IpcError>) -> Vec<u8> {
    let msg = match result {
        Ok(value) => json!({ "id": id, "result": value }),
        Err(e) => json!({ "id": id, "error": { "code": e.code, "message": e.message } }),
    };
    line_of(&msg)
}

/// A broadcast line, newline included.
pub fn event_line<T: Serialize + ?Sized>(event: &str, payload: &T) -> Vec<u8> {
    line_of(&json!({ "event": event, "payload": payload }))
}

fn line_of(msg: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(msg).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// One client of the service: a non-blocking stream with an input buffer for partial lines and
/// an output buffer for what the socket would not take yet.
#[derive(Debug)]
pub struct Conn {
    stream: UnixStream,
    input: Vec<u8>,
    out: Vec<u8>,
    /// Bytes at the front of `out` already written.
    sent: usize,
    /// `live.subscribe {on: true}`: one `live` event per row.
    pub live: bool,
    /// The peer has shut down its side. Its last requests are still answered; the connection
    /// closes once the replies are out (so `socat` and the like get their answer).
    pub eof: bool,
    /// Why it must go: the service drops it at the end of the step.
    pub dead: Option<&'static str>,
}

impl Conn {
    pub fn new(stream: UnixStream) -> io::Result<Conn> {
        stream.set_nonblocking(true)?;
        Ok(Conn {
            stream,
            input: Vec::new(),
            out: Vec::new(),
            sent: 0,
            live: false,
            eof: false,
            dead: None,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Bytes queued and not yet written.
    pub fn pending(&self) -> usize {
        self.out.len() - self.sent
    }

    /// Reads whatever the socket holds and returns the complete lines. At end of file a last line
    /// without a newline still counts. A line over `MAX_LINE` marks the client dead.
    pub fn read_lines(&mut self) -> Vec<Vec<u8>> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => self.input.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // A peer that closes with our output still unread resets the connection: the
                // usual way a live-subscribed window goes, not a fault worth a journal line.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    self.dead = Some("gone");
                    break;
                }
                Err(_) => {
                    self.dead = Some("read error");
                    break;
                }
            }
            if self.input.len() > MAX_LINE && !self.input.contains(&b'\n') {
                break;
            }
        }
        let mut lines = Vec::new();
        while let Some(pos) = self.input.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.input.drain(..=pos).collect();
            lines.push(line[..line.len() - 1].to_vec());
        }
        if self.input.len() > MAX_LINE {
            self.input.clear();
            self.dead = Some("a request line over 64 KiB");
        } else if self.eof && !self.input.is_empty() {
            lines.push(std::mem::take(&mut self.input));
        }
        lines.retain(|l| !l.trim_ascii().is_empty());
        lines
    }

    /// Queues a line and writes what the socket takes now. False when the client is more than
    /// `MAX_BEHIND` behind: the caller drops it.
    pub fn queue(&mut self, bytes: &[u8]) -> bool {
        if self.dead.is_some() {
            return true;
        }
        if self.pending() + bytes.len() > MAX_BEHIND {
            self.dead = Some("more than 1 MB behind");
            return false;
        }
        if self.sent > 0 && self.sent == self.out.len() {
            self.out.clear();
            self.sent = 0;
        }
        self.out.extend_from_slice(bytes);
        self.flush();
        true
    }

    /// Writes until the socket would block or the buffer is empty.
    pub fn flush(&mut self) {
        while self.sent < self.out.len() && self.dead.is_none() {
            match self.stream.write(&self.out[self.sent..]) {
                Ok(0) => self.dead = Some("write returned 0"),
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // The peer went away (EPIPE, ECONNRESET): nothing left to tell it.
                Err(_) => self.dead = Some("gone"),
            }
        }
        if self.sent == self.out.len() {
            self.out.clear();
            self.sent = 0;
        } else if self.sent > 64 * 1024 {
            self.out.drain(..self.sent);
            self.sent = 0;
        }
    }
}

/// Why a CLI call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// Nothing listens on the socket (no file, or a stale one).
    NotRunning,
    /// The service answered with an error.
    Remote(IpcError),
    Io(String),
}

/// Whether a service answers on `socket`: a connect succeeds. A stale socket file refuses.
pub fn listening(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// One request from the CLI, waiting for its reply. Broadcasts that arrive first (every write of
/// state.json is broadcast to every client, the caller included) are skipped.
pub fn call(socket: &Path, method: &str, params: Value) -> Result<Value, CallError> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(CallError::NotRunning);
        }
        Err(e) => return Err(CallError::Io(format!("{}: {e}", socket.display()))),
    };
    let io_err = |e: io::Error| CallError::Io(format!("{}: {e}", socket.display()));
    stream.set_read_timeout(Some(CALL_TIMEOUT)).map_err(io_err)?;
    let id = json!(1);
    stream
        .write_all(&line_of(
            &json!({ "id": id, "method": method, "params": params }),
        ))
        .map_err(io_err)?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).map_err(io_err)? == 0 {
            return Err(CallError::Io(
                "the service closed the connection without a reply".to_owned(),
            ));
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if msg.get("id") != Some(&id) {
            continue;
        }
        if let Some(e) = msg.get("error") {
            let text = |k: &str| {
                e.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            return Err(CallError::Remote(IpcError {
                code: text("code"),
                message: text("message"),
            }));
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies() {
        let r = parse_request(br#"{"id":7,"method":"state.get"}"#).unwrap();
        assert_eq!(
            r,
            Request {
                id: Some(json!(7)),
                method: "state.get".into(),
                params: json!({})
            }
        );
        let r = parse_request(br#"{"method":"live.subscribe","params":{"on":true}}"#).unwrap();
        assert_eq!((r.id, r.params), (None, json!({"on": true})));

        // Not JSON: answered with id null, since the id cannot be read.
        let (id, e) = parse_request(b"{nope").unwrap_err();
        assert_eq!((id, e.code.as_str()), (Some(Value::Null), "bad_request"));
        let (id, e) = parse_request(br#"{"id":"a","params":[]}"#).unwrap_err();
        assert_eq!((id, e.code.as_str()), (Some(json!("a")), "bad_request"));
        let (_, e) = parse_request(br#"{"id":2,"method":"x","params":[1]}"#).unwrap_err();
        assert_eq!(e.message, "params must be an object");
        assert!(parse_request(b"[1,2]").is_err());

        assert_eq!(
            reply_line(&json!(3), Ok(json!({}))),
            b"{\"id\":3,\"result\":{}}\n".to_vec()
        );
        let line = reply_line(&json!(4), Err(IpcError::refused("no case")));
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["error"], json!({"code": "refused", "message": "no case"}));
        let v: Value = serde_json::from_slice(&event_line("live", &json!({"t": 1}))).unwrap();
        assert_eq!(v, json!({"event": "live", "payload": {"t": 1}}));
    }

    #[test]
    fn lines_are_split_and_bounded() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut conn = Conn::new(a).unwrap();
        b.write_all(b"{\"id\":1}\n\n{\"id\":").unwrap();
        assert_eq!(conn.read_lines(), vec![b"{\"id\":1}".to_vec()]);
        b.write_all(b"2}\n").unwrap();
        assert_eq!(conn.read_lines(), vec![b"{\"id\":2}".to_vec()]);
        // A last line without a newline still counts once the peer has shut its side.
        b.write_all(b"{\"id\":3}").unwrap();
        b.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(conn.read_lines(), vec![b"{\"id\":3}".to_vec()]);
        assert!(conn.eof && conn.dead.is_none());

        let (a, mut b) = UnixStream::pair().unwrap();
        let mut conn = Conn::new(a).unwrap();
        b.set_nonblocking(true).unwrap();
        let chunk = vec![b'x'; 8 * 1024];
        for _ in 0..9 {
            let _ = b.write_all(&chunk);
            let _ = conn.read_lines();
        }
        assert_eq!(conn.dead, Some("a request line over 64 KiB"));
    }

    #[test]
    fn a_peer_that_closes_with_unread_output_is_gone_quietly() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut conn = Conn::new(a).unwrap();
        assert!(conn.queue(b"{\"event\":\"live\",\"payload\":{}}\n"));
        assert_eq!(conn.pending(), 0, "the socket took it");
        // Closed without reading it: the next read is ECONNRESET, which is a normal goodbye.
        drop(b);
        assert!(conn.read_lines().is_empty());
        assert_eq!(conn.dead, Some("gone"));
    }

    #[test]
    fn a_client_that_stops_reading_is_dropped() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut conn = Conn::new(a).unwrap();
        let line = vec![b'y'; 60 * 1024];
        let mut queued = 0;
        while conn.queue(&line) {
            queued += 1;
            assert!(queued < 100, "never dropped");
        }
        // The socket buffer took some of it; the rest waited until the 1 MB line was crossed.
        assert_eq!(conn.dead, Some("more than 1 MB behind"));
        assert!(conn.pending() <= MAX_BEHIND);
    }

    #[test]
    fn nothing_listening_is_not_running() {
        let dir = crate::testutil::test_dir("ipc-none");
        let sock = dir.join("s.sock");
        assert_eq!(call(&sock, "state.get", json!({})), Err(CallError::NotRunning));
        // A stale socket file: bound once, nobody listening now.
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(sock.exists());
        assert!(!listening(&sock));
        assert_eq!(call(&sock, "state.get", json!({})), Err(CallError::NotRunning));
    }
}
