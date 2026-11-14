//! Bulk memory is bounded by the configured buffer, not by body or document size.
//!
//! Peak resident memory of the whole process, server workers and the test client
//! alike (Linux: /proc, `clear_refs` resets the high-water mark). Nothing in
//! these bodies is committed (the engine refuses fields over its 8 KiB cap, and
//! the node refuses documents over the buffer), so the in-memory store does not
//! grow and any growth is request memory. RSS is allocator-dependent, so bounds
//! are generous multiples of the buffer, and the main claim is relative: ten
//! times the body costs the same.
mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use hsearch_node::{Limits, Node, serve};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use serde_json::{Value, json};
use support::*;

/// `VmRSS` or `VmHWM` of this process, bytes, from /proc (Linux only).
fn status_kb(field: &str) -> usize {
    let text = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = text.lines().find(|l| l.starts_with(field)).unwrap();
    let kb: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kb * 1024
}

const BUFFER: usize = 1 << 20;

fn limits() -> Limits {
    Limits {
        max_document_bytes: 200 << 20,
        max_bulk_bytes: 512 << 20,
        max_bulk_items: 5000,
        buffer_bytes: BUFFER,
        memory_budget_bytes: 64 << 20,
    }
}

fn store() -> Arc<MemoryStore> {
    Arc::new(
        MemoryStore::new(MemoryLimits {
            objects: 4096,
            versions: 16384,
            version_bytes: 64 << 20,
            identities: 16384,
            identity_bytes: 64 << 20,
        })
        .unwrap(),
    )
}

/// One action line and a source whose title is `title_bytes` long.
fn head(id: usize) -> String {
    format!("{{\"index\":{{\"_id\":\"big{id}\"}}}}\n{{\"title\":\"")
}
const TAIL: &str = "\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}\n";

/// POST `documents` documents of `title_bytes` each as one streamed body, never
/// holding more than a 64 KiB chunk on the client side. Returns the response.
fn post_stream(addr: SocketAddr, documents: usize, title_bytes: usize) -> (u16, Value) {
    let length: usize = (0..documents)
        .map(|i| head(i).len() + title_bytes + TAIL.len())
        .sum();
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST /orders/_bulk HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n"
    )
    .unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    for i in 0..documents {
        stream.write_all(head(i).as_bytes()).unwrap();
        let mut left = title_bytes;
        while left > 0 {
            let n = left.min(chunk.len());
            stream.write_all(&chunk[..n]).unwrap();
            left -= n;
        }
        stream.write_all(TAIL.as_bytes()).unwrap();
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let decoded;
    let body = if head.to_ascii_lowercase().contains("chunked") {
        decoded = unchunk(rest);
        decoded.as_str()
    } else {
        rest
    };
    (status, serde_json::from_str(body).unwrap())
}

/// Peak resident bytes above the level at the start of `run`. Writing 5 to
/// `clear_refs` resets the kernel's high-water mark for this process.
fn peak_over<T>(run: impl FnOnce() -> T) -> (usize, T) {
    std::fs::write("/proc/self/clear_refs", "5").unwrap();
    let base = status_kb("VmRSS:");
    let out = run();
    (status_kb("VmHWM:").saturating_sub(base), out)
}

#[test]
#[cfg(target_os = "linux")]
fn peak_memory_follows_the_buffer_not_the_body_or_the_document() {
    let (store, dir) = (store(), Scratch::new("memory"));
    let node = Arc::new(Node::with_limits(store, dir.0.clone(), limits()).unwrap());
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    // Warm up so one-time allocations (threads, store tables) are not counted.
    assert_eq!(post_stream(addr, 4, 1000).0, 200);

    // A bulk of many large documents, each under the buffer. The engine refuses
    // each one (its field cap is 8 KiB), so nothing is retained.
    let (small_body, (status, out)) = peak_over(|| post_stream(addr, 30, 700 * 1024));
    assert_eq!((status, &out["errors"]), (200, &json!(true)));
    let (large_body, (status, out)) = peak_over(|| post_stream(addr, 300, 700 * 1024));
    assert_eq!((status, &out["errors"]), (200, &json!(true)));
    assert_eq!(out["items"].as_array().unwrap().len(), 300);
    println!("memory: 21 MiB body peak {small_body} B, 210 MiB body peak {large_body} B");
    assert!(
        large_body <= small_body + 6 * BUFFER,
        "ten times the body must not cost more memory: {small_body} -> {large_body}"
    );
    assert!(
        large_body <= 12 * BUFFER,
        "peak {large_body} B over 12 buffers of {BUFFER} B"
    );

    // One 200 MiB document (over the 200 MiB limit once its frame is counted):
    // refused by name and streamed past.
    let (huge, (status, out)) = peak_over(|| post_stream(addr, 1, 200 << 20));
    println!("memory: one 200 MiB document peak {huge} B");
    assert_eq!(status, 200, "{out}");
    let error = &out["items"][0]["index"]["error"];
    assert_eq!(
        error["type"],
        json!("document_too_large_exception"),
        "{out}"
    );
    assert!(
        huge <= 12 * BUFFER,
        "peak {huge} B over 12 buffers of {BUFFER} B"
    );

    // Over the configured limit the reason names the limit; between the buffer
    // and the limit it names the buffer.
    let reason = |out: &Value| {
        out["items"][0]["index"]["error"]["reason"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(
        reason(&out).contains("document limit of 209715200 bytes"),
        "{out}"
    );
    let (status, out) = post_stream(addr, 1, 100 << 20);
    assert_eq!(status, 200);
    assert!(reason(&out).contains("streaming buffer"), "{out}");
    running.shutdown();
}
