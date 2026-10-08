//! Shared harness for the dependency-matrix injection tests (TASK-023,
//! FR-1112): a minimal fault-injecting HTTP server and helpers for
//! producing "down" endpoints. Every matrix row's test injects its failure
//! through one of these.
//!
//! The module is compiled once per test binary (`mod common;` in each
//! file), so items used by only some binaries read as dead code in the
//! others. The `allow` below covers that harness sharing, not production
//! code.

#![allow(dead_code)]

pub mod ads_harness;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// What the fault server does with each request.
#[derive(Debug, Clone)]
pub enum FaultMode {
    /// `200` with an empty JSON object — the dependency is healthy.
    Echo,
    /// Every request gets this status (and an empty JSON body).
    Status(u16),
    /// Sleep first, then answer — a slow dependency.
    Delay { delay_ms: u64, status: u16 },
    /// `200` with this exact body — for protocols shaped by the dependency
    /// under test (DoH JSON, for example).
    Body(String),
}

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Shared {
    mode: Mutex<FaultMode>,
    requests: Mutex<Vec<RecordedRequest>>,
}

/// A fault-injecting HTTP/1.1 server on an ephemeral port. One accept-loop
/// thread; every connection is answered once and closed, which is all the
/// adapters under test need.
pub struct FaultServer {
    base_url: String,
    shared: Arc<Shared>,
    local_addr: std::net::SocketAddr,
}

impl FaultServer {
    pub fn start(mode: FaultMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let local_addr = listener.local_addr().expect("local addr");
        let shared = Arc::new(Shared {
            mode: Mutex::new(mode),
            requests: Mutex::new(Vec::new()),
        });
        let loop_shared = shared.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let shared = loop_shared.clone();
                std::thread::spawn(move || handle(stream, shared));
            }
        });
        Self {
            base_url: format!("http://{local_addr}"),
            shared,
            local_addr,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    pub fn set_mode(&self, mode: FaultMode) {
        *self.shared.mode.lock().unwrap() = mode;
    }

    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.shared.requests.lock().unwrap().clone()
    }
}

fn handle(mut stream: TcpStream, shared: Arc<Shared>) {
    let request = match read_request(&mut stream) {
        Some(request) => request,
        None => return,
    };
    let mode = shared.mode.lock().unwrap().clone();
    shared.requests.lock().unwrap().push(request);
    let (sleep_ms, status, body) = match mode {
        FaultMode::Echo => (0, 200, "{}".to_string()),
        FaultMode::Status(status) => (0, status, "{\"error\":true}".to_string()),
        FaultMode::Delay { delay_ms, status } => (delay_ms, status, "{\"error\":true}".to_string()),
        FaultMode::Body(body) => (0, 200, body),
    };
    if sleep_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(pos) = find_header_end(&buffer) {
            break pos;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    // The body (if any) is consumed but unused; drain what content-length
    // names so the client never blocks on a full socket buffer.
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some(RecordedRequest { path, headers })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

/// A port that refuses connections: bind, read the port, drop the
/// listener. This is the "dependency down" endpoint.
pub fn closed_endpoint() -> (String, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    (format!("http://{addr}"), addr.port())
}

/// What the RESP fault server does with each command batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespMode {
    /// Answer `+OK` — a healthy Redis.
    Ok,
    /// Answer `-ERR simulated redis failure` — an erroring Redis.
    Error,
    /// Read and discard, never answer — a hung Redis (the client's
    /// response timeout fires).
    Blackhole,
}

/// A minimal RESP-speaking fault server for the Redis rows. Real Redis
/// clients cannot even *establish* a connection manager against a server
/// that answers non-RESP bytes, so the honest injection is a server that
/// speaks RESP and then starts failing: tests connect while healthy and
/// flip the mode — exactly how a production Redis fails under a live
/// process.
pub struct RespServer {
    port: u16,
    mode: Arc<Mutex<RespMode>>,
}

impl RespServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let mode = Arc::new(Mutex::new(RespMode::Ok));
        let loop_mode = mode.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mode = loop_mode.clone();
                std::thread::spawn(move || serve_resp(stream, mode));
            }
        });
        Self { port, mode }
    }

    pub fn redis_url(&self) -> String {
        format!("redis://127.0.0.1:{}/", self.port)
    }

    pub fn set_mode(&self, mode: RespMode) {
        *self.mode.lock().unwrap() = mode;
    }
}

fn serve_resp(mut stream: TcpStream, mode: Arc<Mutex<RespMode>>) {
    // Accumulate bytes and drain every complete top-level RESP array with
    // one reply each. Per-batch replies are not enough: the client
    // pipelines its connect-time `CLIENT SETINFO` pair in one write and
    // awaits two replies, and a command may likewise arrive split across
    // reads — so framing is parsed, not assumed.
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                if pending.len() > 1024 * 1024 {
                    break;
                }
                loop {
                    match command_len(&pending) {
                        Some(len) => {
                            pending.drain(..len);
                            match *mode.lock().unwrap() {
                                RespMode::Ok => {
                                    if stream.write_all(b"+OK\r\n").is_err() {
                                        return;
                                    }
                                }
                                RespMode::Error => {
                                    if stream
                                        .write_all(b"-ERR simulated redis failure\r\n")
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                RespMode::Blackhole => {
                                    // Swallow the command; the client's
                                    // response timeout is the verdict.
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    }
}

/// Byte length of the first complete top-level RESP array (`*argc`
/// followed by `$len`-framed bulk strings), or `None` while it is
/// incomplete or not an array.
fn command_len(buffer: &[u8]) -> Option<usize> {
    if buffer.first() != Some(&b'*') {
        return None;
    }
    let mut pos = 0;
    let argc: usize = read_line(buffer, &mut pos)?.get(1..)?.parse().ok()?;
    for _ in 0..argc {
        let header = read_line(buffer, &mut pos)?;
        if !header.starts_with('$') {
            return None;
        }
        let len: usize = header.get(1..)?.parse().ok()?;
        if buffer.len() < pos + len + 2 {
            return None;
        }
        pos += len + 2;
    }
    Some(pos)
}

fn read_line<'a>(buffer: &'a [u8], pos: &mut usize) -> Option<&'a str> {
    let start = *pos;
    let end = buffer.get(start..)?.windows(2).position(|w| w == b"\r\n")? + start;
    *pos = end + 2;
    std::str::from_utf8(buffer.get(start..end)?).ok()
}
