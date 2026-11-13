//! A plain HTTP/1.1 client over a raw socket: no SDK, no keep-alive.
#![allow(dead_code)]
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "hsearch-node-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn call(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let decoded;
    let rest = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decoded = unchunk(rest);
        decoded.as_str()
    } else {
        rest
    };
    let json = serde_json::from_str(rest).unwrap_or_else(|e| {
        if rest.is_empty() {
            Value::Null
        } else {
            panic!(
                "unparseable body ({e}) status {status}: head {:?} ... tail {:?}",
                &rest[..rest.len().min(200)],
                &rest[rest.len().saturating_sub(100)..]
            )
        }
    });
    (status, json)
}

pub const MAPPING: &str = r#"{"mappings":{"properties":{
  "title":{"type":"text"},"tag":{"type":"keyword"},"n":{"type":"integer"},
  "ok":{"type":"boolean"},"at":{"type":"date"}}}}"#;

pub fn doc(id: usize) -> String {
    format!(
        "{{\"index\":{{\"_id\":\"d{id}\"}}}}\n{{\"title\":\"quick brown fox {id}\",\"tag\":\"t{}\",\"n\":{id},\"ok\":{},\"at\":\"2026-01-0{}\"}}\n",
        id % 3,
        id.is_multiple_of(2),
        1 + id % 9
    )
}

/// Decode an HTTP/1.1 chunked body.
fn unchunk(mut rest: &str) -> String {
    let mut out = String::new();
    loop {
        let (size, after) = rest.split_once("\r\n").unwrap();
        let size = usize::from_str_radix(size.trim(), 16).unwrap();
        if size == 0 {
            return out;
        }
        out.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
}
