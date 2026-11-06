//! `recompact gateway`: recompact's compaction for sessions that no launcher can switch, such as
//! Code sessions in the Claude desktop app.
//!
//! The desktop app runs Claude Code under its own control, so the launcher's in-place switch
//! (`/resume <twin>` typed into the session) is not available there, and stopping the process
//! would end its background shells, monitors and agents. What does stay in the same process is
//! Claude Code's own compaction: it sends one request whose answer becomes the new context, and
//! the session keeps running (measured, CLI 2.1.317: a background shell started before the
//! compaction finished after it and its notice arrived).
//!
//! The gateway is a small HTTP proxy in front of the session's inference endpoint. It forwards
//! every request unchanged, except a compaction request from a session it serves: that one it
//! answers itself, with recompact's twin of the session rendered as the summary. Claude Code
//! installs that text as the conversation so far, in the running process.
//!
//! Each served session keeps its twin chain: the first compaction makes a twin of the session
//! file; the next one appends what the session did since (the records after the summary this
//! gateway gave it) to that twin and compacts the result, so later compactions build on
//! recompact's structure instead of re-summarizing a summary. Recall reads the twin chain.
//!
//! When recompact cannot compact (an error, or no meaningful reduction), the request goes on to
//! the endpoint unchanged and Claude Code's own summary applies.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::{
    content, continue_file, continue_opts, default_target, human_queued_text, is_human_queued,
    load_jsonl, locate_session, preserved_uuids, rec_type, rec_uuid, recompact_home,
    select_active, tool_result_text, truthy, user_text,
};

/// The first line of every summary the gateway serves; `sync_into` finds its own summary by it.
pub const GATEWAY_MARK: &str = "recompact-gateway";
/// How Claude Code's compaction prompt starts (CLI 2.1.317 to 2.1.319). The request is otherwise
/// an ordinary Messages request: the conversation, with this prompt as the last user text.
const COMPACT_PROMPT: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";
const DEFAULT_PORT: u16 = 62897;
const LABEL: &str = "com.segment-recompact.gateway";
const PING_EVERY: Duration = Duration::from_secs(5);
const HOP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

#[derive(Clone)]
struct Cfg {
    host: String,
    port: u16,
    base: String,
    /// Serve every session, not only desktop ones (tests, and anyone who wants it).
    all: bool,
    at: usize,
}

#[derive(Clone)]
struct Job {
    session: String,
    path: PathBuf,
    /// The conversation the request carries, without its compaction prompt: a retry of the same
    /// compaction (Claude Code rewords the prompt when it retries) has the same key.
    key: u64,
}

// ------------------------------------------------------------------------------------ command

/// `recompact gateway [--port P] [--upstream URL] [--all]`, or `gateway install|uninstall|status`.
pub fn cmd_gateway(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("install") => return gateway_install(&args[1..]),
        Some("uninstall") => return gateway_uninstall(),
        Some("status") => return gateway_status(),
        _ => {}
    }
    let (_, opts) = crate::parse_opts(args);
    let get = |k: &str| opts.get(k).and_then(|v| v.as_str()).map(String::from);
    let port: u16 = get("port").and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    let upstream = get("upstream")
        .or_else(|| std::env::var("RECOMPACT_GATEWAY_UPSTREAM").ok())
        .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok());
    let Some(upstream) = upstream else {
        eprintln!("recompact gateway: name the endpoint to forward to with --upstream http://host:port");
        return 2;
    };
    let Some((host, uport, base)) = parse_upstream(&upstream) else {
        eprintln!("recompact gateway: --upstream must be an http:// URL (got {upstream})");
        return 2;
    };
    let cfg = Arc::new(Cfg {
        host,
        port: uport,
        base,
        all: opts.contains_key("all"),
        at: std::env::var("RECOMPACT_AT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(500_000),
    });
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("recompact gateway: cannot listen on 127.0.0.1:{port}: {e}");
            return 1;
        }
    };
    // Summarizer runs are recompact's own: their hooks stay quiet.
    std::env::set_var("RECOMPACT_INTERNAL", "1");
    log(&format!("listening on 127.0.0.1:{port}, forwarding to {upstream}"));
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let cfg = Arc::clone(&cfg);
        std::thread::spawn(move || handle(stream, &cfg));
    }
    0
}

fn parse_upstream(url: &str) -> Option<(String, u16, String)> {
    let rest = url.trim().strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80),
    };
    (!host.is_empty()).then(|| (host, port, path.to_string()))
}

fn log(line: &str) {
    eprintln!("{} {line}", crate::now_unix());
}

// ------------------------------------------------------------------------------------ http

struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    fn new(s: TcpStream) -> Self {
        Conn { s, buf: Vec::new() }
    }

    fn fill(&mut self) -> io::Result<usize> {
        let mut tmp = [0u8; 16384];
        let n = self.s.read(&mut tmp)?;
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(n)
    }

    /// The header block, through its blank line; `None` when the peer closed first.
    fn read_head(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(p) = find(&self.buf, b"\r\n\r\n") {
                return Ok(Some(self.buf.drain(..p + 4).collect()));
            }
            if self.buf.len() > 1 << 20 {
                return Err(io::Error::other("header block too large"));
            }
            if self.fill()? == 0 {
                return Ok(None);
            }
        }
    }

    fn read_n(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.buf.len() < n {
            if self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(self.buf.drain(..n).collect())
    }

    fn read_line(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if let Some(p) = find(&self.buf, b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..p + 2).collect();
                return Ok(line[..p].to_vec());
            }
            if self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    /// One chunk of a chunked body; empty at the end (trailers consumed).
    fn read_chunk(&mut self) -> io::Result<Vec<u8>> {
        let line = self.read_line()?;
        let text = String::from_utf8_lossy(&line);
        let size = usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::other("bad chunk size"))?;
        if size == 0 {
            while !self.read_line()?.is_empty() {}
            return Ok(Vec::new());
        }
        let data = self.read_n(size)?;
        self.read_n(2)?;
        Ok(data)
    }

    /// A message body as the headers frame it, passed to `sink` piece by piece.
    fn read_body(&mut self, head: &Head, mut sink: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        if head
            .get("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            loop {
                let c = self.read_chunk()?;
                if c.is_empty() {
                    return Ok(());
                }
                sink(&c)?;
            }
        }
        if let Some(n) = head.get("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
            let mut left = n;
            while left > 0 {
                if self.buf.is_empty() && self.fill()? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                let take = left.min(self.buf.len());
                let piece: Vec<u8> = self.buf.drain(..take).collect();
                sink(&piece)?;
                left -= take;
            }
            return Ok(());
        }
        // A response with neither runs to the close.
        loop {
            if !self.buf.is_empty() {
                let piece: Vec<u8> = std::mem::take(&mut self.buf);
                sink(&piece)?;
            }
            if self.fill()? == 0 {
                return Ok(());
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[derive(Clone)]
struct Head {
    line: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn parse(raw: &[u8]) -> Option<Head> {
        let text = String::from_utf8_lossy(raw);
        let mut lines = text.split("\r\n");
        let line = lines.next()?.to_string();
        let headers = lines
            .filter(|l| !l.is_empty())
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        Some(Head { line, headers })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn write_chunk(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write!(w, "{:x}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")?;
    w.flush()
}

fn sse(event: &str, data: &Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

fn handle(client: TcpStream, cfg: &Cfg) {
    let _ = client.set_read_timeout(Some(Duration::from_secs(300)));
    let mut c = Conn::new(client);
    let Ok(Some(raw)) = c.read_head() else { return };
    let Some(head) = Head::parse(&raw) else { return };
    let mut parts = head.line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return;
    };
    let (method, target) = (method.to_string(), target.to_string());
    // A request without Content-Length or chunked framing has no body.
    let mut body = Vec::new();
    let framed = head.get("content-length").is_some()
        || head
            .get("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    if framed
        && c
            .read_body(&head, |p| {
                body.extend_from_slice(p);
                Ok(())
            })
            .is_err()
    {
        return;
    }
    let _ = c.s.set_read_timeout(None);
    if method == "POST" && target.starts_with("/v1/messages") && !target.contains("count_tokens") {
        if let Some(job) = compaction_job(cfg, &head, &body) {
            serve_compaction(c, cfg, &head, &method, &target, &body, job);
            return;
        }
    }
    passthrough(c, cfg, &head, &method, &target, &body);
}

fn upstream_request(cfg: &Cfg, head: &Head, method: &str, target: &str, body: &[u8]) -> io::Result<Conn> {
    let mut s = TcpStream::connect((cfg.host.as_str(), cfg.port))?;
    let mut req = format!(
        "{method} {}{target} HTTP/1.1\r\nHost: {}:{}\r\n",
        cfg.base, cfg.host, cfg.port
    );
    for (k, v) in &head.headers {
        if !HOP_HEADERS.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    if !body.is_empty() || method == "POST" {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body)?;
    s.flush()?;
    Ok(Conn::new(s))
}

/// Forward a request and stream the answer back as it arrives (server-sent events included).
fn passthrough(mut c: Conn, cfg: &Cfg, head: &Head, method: &str, target: &str, body: &[u8]) {
    let up = upstream_request(cfg, head, method, target, body).and_then(|mut up| {
        let raw = up.read_head()?.ok_or(io::ErrorKind::UnexpectedEof)?;
        Ok((up, raw))
    });
    let (mut up, raw) = match up {
        Ok(v) => v,
        Err(e) => {
            let msg = json!({"type": "error", "error": {"type": "api_error",
                "message": format!("recompact gateway: the upstream at {}:{} did not answer: {e}", cfg.host, cfg.port)}})
            .to_string();
            let _ = write!(
                c.s,
                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{msg}",
                msg.len()
            );
            return;
        }
    };
    let Some(rh) = Head::parse(&raw) else { return };
    let mut out = format!("{}\r\n", rh.line);
    for (k, v) in &rh.headers {
        if !k.eq_ignore_ascii_case("connection") && !k.eq_ignore_ascii_case("keep-alive") {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    if c.s.write_all(out.as_bytes()).is_err() {
        return;
    }
    // The body ends where its own framing says, not when the upstream closes, which a server
    // that keeps the connection open would never do. Chunks go on as chunks, as they arrive.
    let chunked = rh
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let no_body = rh.line.split(' ').nth(1).is_some_and(|s| s == "204" || s == "304") || method == "HEAD";
    if !no_body {
        let client = &mut c.s;
        let sent = up.read_body(&rh, |p| {
            if chunked {
                write_chunk(client, p)
            } else {
                client.write_all(p)?;
                client.flush()
            }
        });
        if sent.is_ok() && chunked {
            let _ = write_chunk(&mut c.s, b"");
        }
    }
    let _ = up.s.shutdown(Shutdown::Both);
    let _ = c.s.shutdown(Shutdown::Both);
}

// ------------------------------------------------------------------------------------ compaction

/// Whether a Messages request body is Claude Code's compaction request.
pub fn is_compaction_request(body: &Value) -> bool {
    let Some(last) = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|m| m.last())
    else {
        return false;
    };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let texts: Vec<&str> = match last.get("content") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect(),
        _ => vec![],
    };
    texts
        .iter()
        .any(|t| t.starts_with(COMPACT_PROMPT) && t.contains("<summary>"))
}

/// The session a compaction request is for, when the gateway serves it.
fn compaction_job(cfg: &Cfg, head: &Head, body: &[u8]) -> Option<Job> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if !is_compaction_request(&v) {
        return None;
    }
    let session = head.get("x-claude-code-session-id")?.to_string();
    let path = locate_session(None, &session)?;
    if !cfg.all && !is_desktop_session(&path) {
        log(&format!("{session}: compaction passed through (not a desktop session)"));
        return None;
    }
    let msgs = v.get("messages").and_then(|m| m.as_array())?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&Value::Array(msgs[..msgs.len() - 1].to_vec()).to_string(), &mut h);
    Some(Job { session, path, key: std::hash::Hasher::finish(&h) })
}

/// A Code session the Claude desktop app runs (`entrypoint` "claude-desktop", "claude-desktop-3p").
pub fn is_desktop_session(path: &Path) -> bool {
    let Ok(f) = fs::File::open(path) else { return false };
    for line in BufReader::new(f).lines().map_while(Result::ok).take(400) {
        if let Ok(r) = serde_json::from_str::<Value>(&line) {
            if let Some(e) = r.get("entrypoint").and_then(|e| e.as_str()) {
                return e.starts_with("claude-desktop");
            }
        }
    }
    false
}

fn serve_compaction(mut c: Conn, cfg: &Cfg, head: &Head, method: &str, target: &str, body: &[u8], job: Job) {
    let started = Instant::now();
    if c
        .s
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        )
        .is_err()
    {
        return;
    }
    let (tx, rx) = mpsc::channel();
    {
        let (cfg, job) = (cfg.clone(), job.clone());
        std::thread::spawn(move || {
            let _ = tx.send(compact_for_gateway(&cfg, &job));
        });
    }
    let ping = sse("ping", &json!({"type": "ping"}));
    let result = loop {
        match rx.recv_timeout(PING_EVERY) {
            Ok(r) => break r,
            Err(RecvTimeoutError::Timeout) => {
                if write_chunk(&mut c.s, &ping).is_err() {
                    log(&format!("{}: the session stopped waiting for its compaction", job.session));
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break Err("the compaction worker stopped".into()),
        }
    };
    let secs = started.elapsed().as_secs();
    match result {
        Ok((twin, text)) => {
            let model = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| v.get("model").cloned())
                .unwrap_or(Value::Null);
            if send_message(&mut c.s, &model, &text).is_ok() {
                write_state(&job.session, &twin);
                log(&format!(
                    "{}: served twin {twin} ({} chars) in {secs}s",
                    job.session,
                    text.len()
                ));
            }
        }
        Err(why) => {
            log(&format!("{}: recompact did not compact ({why}); Claude Code's own summary applies", job.session));
            fallback(&mut c.s, cfg, head, method, target, body);
        }
    }
    let _ = c.s.shutdown(Shutdown::Both);
}

fn send_message(w: &mut TcpStream, model: &Value, text: &str) -> io::Result<()> {
    let id = format!("msg_recompact_{}", &crate::uuid_v4()[..8]);
    write_chunk(w, &sse("message_start", &json!({"type": "message_start", "message": {
        "id": id, "type": "message", "role": "assistant", "model": model, "content": [],
        "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 0}}})))?;
    write_chunk(w, &sse("content_block_start", &json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""}})))?;
    let chars: Vec<char> = text.chars().collect();
    for piece in chars.chunks(16_000) {
        let s: String = piece.iter().collect();
        write_chunk(w, &sse("content_block_delta", &json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": s}})))?;
    }
    write_chunk(w, &sse("content_block_stop", &json!({"type": "content_block_stop", "index": 0})))?;
    write_chunk(w, &sse("message_delta", &json!({"type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
        "usage": {"output_tokens": text.len() / 4}})))?;
    write_chunk(w, &sse("message_stop", &json!({"type": "message_stop"})))?;
    write_chunk(w, b"")
}

/// recompact could not compact: send the request on as it came and pass its answer through.
fn fallback(w: &mut TcpStream, cfg: &Cfg, head: &Head, method: &str, target: &str, body: &[u8]) {
    let mut plain = head.clone();
    plain.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("accept-encoding"));
    let result = (|| -> io::Result<()> {
        let mut up = upstream_request(cfg, &plain, method, target, body)?;
        let raw = up.read_head()?.ok_or(io::ErrorKind::UnexpectedEof)?;
        let rh = Head::parse(&raw).ok_or_else(|| io::Error::other("bad response head"))?;
        if rh.line.split(' ').nth(1) == Some("200") {
            up.read_body(&rh, |p| write_chunk(w, p))?;
        } else {
            let mut err = Vec::new();
            up.read_body(&rh, |p| {
                err.extend_from_slice(p);
                Ok(())
            })?;
            let v = serde_json::from_slice::<Value>(&err).unwrap_or_else(|_| {
                json!({"type": "error", "error": {"type": "api_error",
                    "message": String::from_utf8_lossy(&err).into_owned()}})
            });
            write_chunk(w, &sse("error", &v))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = write_chunk(
            w,
            &sse("error", &json!({"type": "error", "error": {"type": "api_error",
                "message": format!("recompact gateway: compaction failed and the upstream did not answer: {e}")}})),
        );
    }
    let _ = write_chunk(w, b"");
}

/// The last compaction made for each session, so a retry of it gets the same twin.
fn recent() -> &'static Mutex<HashMap<String, (u64, Instant, String, String)>> {
    static RECENT: OnceLock<Mutex<HashMap<String, (u64, Instant, String, String)>>> = OnceLock::new();
    RECENT.get_or_init(Default::default)
}

fn session_lock(session: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut m = LOCKS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(m.entry(session.to_string()).or_default())
}

/// Make (or extend and remake) the session's twin and render it as the summary text.
fn compact_for_gateway(cfg: &Cfg, job: &Job) -> Result<(String, String), String> {
    let lock = session_lock(&job.session);
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((key, when, twin, text)) = recent().lock().unwrap_or_else(|e| e.into_inner()).get(&job.session) {
        if *key == job.key && when.elapsed() < Duration::from_secs(1800) {
            log(&format!("{}: the same compaction again; serving twin {twin} again", job.session));
            return Ok((twin.clone(), text.clone()));
        }
    }
    let dir = job.path.parent().ok_or("the session file has no folder")?;
    let base = match read_state(&job.session) {
        Some(prev) if dir.join(format!("{prev}.jsonl")).exists() => {
            match sync_into(dir, &job.path, &prev) {
                Ok(n) => {
                    log(&format!("{}: {n} new record(s) added to twin {prev}", job.session));
                    prev
                }
                Err(why) => {
                    log(&format!("{}: starting a new twin chain ({why})", job.session));
                    job.session.clone()
                }
            }
        }
        _ => job.session.clone(),
    };
    let mut o = continue_opts(&Map::new(), Some("haiku"));
    o.threshold = cfg.at;
    o.target = Some(o.target.unwrap_or_else(|| default_target(cfg.at)));
    o.force = true;
    let (twin, rc) = continue_file(dir, &base, &o);
    if rc != 0 {
        return Err(format!("continue exited {rc}"));
    }
    if twin == base {
        return Err("no meaningful reduction".into());
    }
    let (records, _) = select_active(load_jsonl(&dir.join(format!("{twin}.jsonl"))));
    let text = format!(
        "<analysis>\nrecompact compacted this conversation; the summary below is its compacted copy.\n</analysis>\n\n<summary>\n{}\n</summary>",
        render_summary(&twin, &records)
    );
    recent()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(job.session.clone(), (job.key, Instant::now(), twin.clone(), text.clone()));
    Ok((twin, text))
}

/// Append to the twin what the session did after the summary this gateway gave it: every record
/// past that summary except Claude Code's own compaction records and the messages it kept from
/// before (the twin has those already). Returns how many records were added.
pub fn sync_into(dir: &Path, session_file: &Path, twin: &str) -> Result<usize, String> {
    let twin_path = dir.join(format!("{twin}.jsonl"));
    let records = load_jsonl(session_file);
    let marker = format!("{GATEWAY_MARK} twin {twin}");
    let at = records
        .iter()
        .rposition(|r| truthy(r, "isCompactSummary") && user_text(r).contains(&marker))
        .ok_or("the session does not hold this gateway's last summary")?;
    let kept: HashSet<String> = records[..at]
        .iter()
        .rev()
        .find(|r| is_boundary(r))
        .map(preserved_uuids)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let existing = load_jsonl(&twin_path);
    let have: HashSet<&str> = existing.iter().filter_map(rec_uuid).collect();
    let mut leaf = existing.iter().rev().find_map(rec_uuid).map(String::from);
    let mut out = String::new();
    let mut added = 0;
    for r in &records[at + 1..] {
        let Some(u) = rec_uuid(r) else { continue };
        if have.contains(u)
            || kept.contains(u)
            || truthy(r, "isCompactSummary")
            || truthy(r, "isSidechain")
            || is_boundary(r)
        {
            continue;
        }
        let mut rec = r.clone();
        rec["parentUuid"] = leaf.clone().map_or(Value::Null, Value::String);
        rec["sessionId"] = json!(twin);
        leaf = Some(u.to_string());
        out.push_str(&rec.to_string());
        out.push('\n');
        added += 1;
    }
    if added > 0 {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&twin_path)
            .map_err(|e| format!("cannot extend {}: {e}", twin_path.display()))?;
        f.write_all(out.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(added)
}

fn is_boundary(r: &Value) -> bool {
    rec_type(r) == "system" && r.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary")
}

/// The twin as the text Claude Code installs in place of the conversation.
pub fn render_summary(twin: &str, records: &[Value]) -> String {
    let mut out = format!(
        "{GATEWAY_MARK} twin {twin}\n\nThis is recompact's compacted copy of this conversation. Older work appears \
as summaries and shortened tool results; the most recent turns are verbatim. To read back anything a summary or a \
\"[recompact: …]\" marker left out, call the recompact `recall` tool with session \"{twin}\" and the id it names.\n"
    );
    let mut names: HashMap<String, String> = HashMap::new();
    for r in records {
        let blocks = content(r);
        match rec_type(r) {
            "user" if truthy(r, "isCompactSummary") => {
                section(&mut out, "Earlier summary", &user_text(r));
            }
            "user" => match blocks {
                Some(Value::String(s)) => section(&mut out, "User", s),
                Some(Value::Array(a)) => {
                    for b in a {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                section(&mut out, "User", b.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                            }
                            Some("tool_result") => {
                                let id = b.get("tool_use_id").and_then(|t| t.as_str()).unwrap_or("");
                                let name = names.get(id).map_or("tool", String::as_str);
                                let err = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                                let label = format!("{name} result{}", if err { " (error)" } else { "" });
                                section(&mut out, &label, &tool_result_text(b));
                            }
                            Some("image") => out.push_str("\n[image]\n"),
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            "assistant" => {
                for b in blocks.and_then(|c| c.as_array()).into_iter().flatten() {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => section(
                            &mut out,
                            "Assistant",
                            b.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                        ),
                        Some("tool_use") => {
                            let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                            if let Some(id) = b.get("id").and_then(|i| i.as_str()) {
                                names.insert(id.to_string(), name.to_string());
                            }
                            let input = b.get("input").map(Value::to_string).unwrap_or_default();
                            section(&mut out, &format!("Assistant called {name}"), &input);
                        }
                        _ => {}
                    }
                }
            }
            "attachment" if is_human_queued(r) => {
                section(&mut out, "User (sent while Claude worked)", &human_queued_text(r));
            }
            _ => {}
        }
    }
    escape_tags(&out)
}

fn section(out: &mut String, title: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    out.push_str(&format!("\n## {title}\n{}\n", text.trim_end()));
}

/// Claude Code reads the summary between `<summary>` tags; a tag inside the text must not end it.
fn escape_tags(s: &str) -> String {
    let mut out = s.to_string();
    for tag in ["summary", "analysis"] {
        out = out
            .replace(&format!("</{tag}"), &format!("</\u{200B}{tag}"))
            .replace(&format!("<{tag}"), &format!("<\u{200B}{tag}"));
    }
    out
}

// ------------------------------------------------------------------------------------ state

fn state_path(session: &str) -> PathBuf {
    recompact_home().join("gateway").join(format!("{session}.json"))
}

fn read_state(session: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&fs::read_to_string(state_path(session)).ok()?).ok()?;
    v.get("twin").and_then(|t| t.as_str()).map(String::from)
}

fn write_state(session: &str, twin: &str) {
    let p = state_path(session);
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(&p, json!({"twin": twin, "at": crate::now_unix()}).to_string());
}

/// The twin the gateway last served this session, if any: what recall reads for it.
pub fn gateway_twin(session: &str) -> Option<String> {
    read_state(session)
}

// ------------------------------------------------------------------------------------ launchd

fn plist_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

/// Whether the gateway is set up to run at login on this computer.
pub fn gateway_installed() -> bool {
    plist_path().is_some_and(|p| p.exists())
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn launchctl(args: &[&str]) -> bool {
    std::process::Command::new("launchctl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn gui_domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Restart the running gateway so it runs the binary the plugin now names (after an update).
pub fn gateway_restart() {
    if gateway_installed() {
        launchctl(&["kickstart", "-k", &format!("{}/{LABEL}", gui_domain())]);
    }
}

fn gateway_install(args: &[String]) -> i32 {
    let (_, opts) = crate::parse_opts(args);
    let get = |k: &str| opts.get(k).and_then(|v| v.as_str()).map(String::from);
    let Some(upstream) = get("upstream").or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok()) else {
        eprintln!("usage: recompact gateway install --upstream http://host:port [--port P]");
        return 2;
    };
    if parse_upstream(&upstream).is_none() {
        eprintln!("recompact gateway: --upstream must be an http:// URL (got {upstream})");
        return 2;
    }
    let port = get("port").unwrap_or_else(|| DEFAULT_PORT.to_string());
    let Some(plist) = plist_path() else { return 1 };
    let bin = recompact_home().join("bin/recompact");
    let logf = recompact_home().join("gateway.log");
    let mut env = vec![
        ("PATH".to_string(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".to_string(), std::env::var("HOME").unwrap_or_default()),
    ];
    for k in ["RECOMPACT_AT", "RECOMPACT_TARGET", "RECOMPACT_SUMMARIZE_WITH", "RECOMPACT_CLAUDE_BIN"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.to_string(), v));
        }
    }
    let env_xml: String = env
        .iter()
        .map(|(k, v)| format!("    <key>{}</key><string>{}</string>\n", xml(k), xml(v)))
        .collect();
    let args_xml: String = [bin.to_string_lossy().as_ref(), "gateway", "--port", &port, "--upstream", &upstream]
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml(a)))
        .collect();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LABEL}</string>\n  \
<key>ProgramArguments</key>\n  <array>\n{args_xml}  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n{env_xml}  </dict>\n  \
<key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>StandardOutPath</key><string>{log}</string>\n  \
<key>StandardErrorPath</key><string>{log}</string>\n</dict>\n</plist>\n",
        log = xml(&logf.to_string_lossy())
    );
    if let Some(d) = plist.parent() {
        let _ = fs::create_dir_all(d);
    }
    if let Err(e) = fs::write(&plist, body) {
        eprintln!("recompact gateway: cannot write {}: {e}", plist.display());
        return 1;
    }
    let domain = gui_domain();
    launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    if !launchctl(&["bootstrap", &domain, &plist.to_string_lossy()]) {
        eprintln!("recompact gateway: launchctl could not start {}", plist.display());
        return 1;
    }
    println!(
        "recompact gateway: running on http://127.0.0.1:{port}, forwarding to {upstream}; log {}",
        logf.display()
    );
    println!("Point the desktop app's gateway base URL at http://127.0.0.1:{port}.");
    0
}

fn gateway_uninstall() -> i32 {
    let Some(plist) = plist_path() else { return 1 };
    launchctl(&["bootout", &format!("{}/{LABEL}", gui_domain())]);
    let _ = fs::remove_file(&plist);
    println!("recompact gateway: stopped and removed {}", plist.display());
    0
}

fn gateway_status() -> i32 {
    if !gateway_installed() {
        println!("recompact gateway: not installed (recompact gateway install --upstream http://host:port)");
        return 1;
    }
    let running = launchctl(&["print", &format!("{}/{LABEL}", gui_domain())]);
    println!(
        "recompact gateway: {} · log {}",
        if running { "running" } else { "installed, not running" },
        recompact_home().join("gateway.log").display()
    );
    i32::from(!running)
}
