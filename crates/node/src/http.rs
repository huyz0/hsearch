//! HTTP routing over `tiny_http`. Loopback by default, no authentication.
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;

use hsearch_index::Wait;
use hsearch_object::MetadataStore;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::bulk::MAX_BULK_BYTES;
use crate::{ApiError, Node};

/// Bodies of non-bulk requests are small: mappings and queries.
const MAX_BODY: u64 = 1024 * 1024;

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn params(query: &str) -> Result<Vec<(String, String)>, ApiError> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match (percent_decode(k), percent_decode(v)) {
                (Some(k), Some(v)) => Ok((k, v)),
                _ => Err(ApiError::bad("malformed query string")),
            }
        })
        .collect()
}

fn allow(found: &[(String, String)], known: &[&str]) -> Result<(), ApiError> {
    match found.iter().find(|(k, _)| !known.contains(&k.as_str())) {
        Some((k, _)) => Err(ApiError::bad(format!("unrecognized parameter [{k}]"))),
        None => Ok(()),
    }
}

fn json_body(request: &mut Request) -> Result<Value, ApiError> {
    if request.body_length().is_some_and(|n| n as u64 > MAX_BODY) {
        return Err(ApiError::new(
            413,
            "payload_too_large_exception",
            "request body too large",
        ));
    }
    let mut bytes = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::bad("the request body could not be read"))?;
    if bytes.len() as u64 > MAX_BODY {
        return Err(ApiError::new(
            413,
            "payload_too_large_exception",
            "request body too large",
        ));
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::parse("the body is not valid JSON"))
}

/// Route one request to its operation.
pub(crate) fn route<S: MetadataStore + 'static>(
    node: &Node<S>,
    request: &mut Request,
) -> Result<(u16, Value), ApiError> {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let found = params(query)?;
    let segments: Vec<String> = path
        .split('/')
        .skip(1)
        .map(|s| percent_decode(s).ok_or_else(|| ApiError::bad("malformed path")))
        .collect::<Result<_, _>>()?;
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    let method = request.method().clone();
    let wrong = || {
        ApiError::new(
            405,
            "method_not_allowed_exception",
            "method not allowed for this route",
        )
    };
    match segments.as_slice() {
        ["_bulk"] | [_, "_bulk"] => {
            if method != Method::Post {
                return Err(wrong());
            }
            allow(&found, &["refresh"])?;
            let wait = match found
                .iter()
                .find(|(k, _)| k == "refresh")
                .map(|(_, v)| v.as_str())
            {
                None | Some("false") => Wait::Durable,
                Some("true" | "wait_for" | "") => Wait::Searchable,
                Some(_) => return Err(ApiError::bad("[refresh] must be true, false or wait_for")),
            };
            if request
                .body_length()
                .is_some_and(|n| n as u64 > MAX_BULK_BYTES)
            {
                return Err(ApiError::new(
                    413,
                    "payload_too_large_exception",
                    "bulk byte limit exceeded",
                ));
            }
            let default = if segments.len() == 2 {
                Some(segments[0])
            } else {
                None
            };
            node.bulk(request.as_reader(), default, wait)
                .map(|v| (200, v))
        }
        [index] if !index.is_empty() && !index.starts_with('_') => {
            if method != Method::Put {
                return Err(wrong());
            }
            allow(&found, &[])?;
            let body = json_body(request)?;
            node.create(index, &body).map(|v| (200, v))
        }
        [index, "_search"] => {
            if method != Method::Post && method != Method::Get {
                return Err(wrong());
            }
            allow(&found, &[])?;
            let body = json_body(request)?;
            node.search(index, &body).map(|v| (200, v))
        }
        [index, "_refresh"] => {
            if method != Method::Post {
                return Err(wrong());
            }
            allow(&found, &[])?;
            node.refresh(index).map(|v| (200, v))
        }
        [index, "_doc", id] => {
            if method != Method::Get {
                return Err(wrong());
            }
            allow(&found, &[])?;
            node.get(index, id)
        }
        _ => Err(ApiError::new(
            404,
            "no_handler_found_exception",
            "no handler for this route",
        )),
    }
}

/// A running server and the threads that serve it.
pub struct Running {
    server: Arc<Server>,
    workers: Vec<JoinHandle<()>>,
    /// The bound address, useful when port 0 was requested.
    pub addr: SocketAddr,
}

impl Running {
    /// Stop accepting and wait for the workers.
    pub fn shutdown(self) {
        for _ in &self.workers {
            self.server.unblock();
        }
        for worker in self.workers {
            let _ = worker.join();
        }
    }
}

/// Bind `addr` and serve with `workers` threads. A non-loopback address is
/// refused unless `allow_non_loopback` says the lack of authentication is understood.
pub fn serve<S: MetadataStore + 'static>(
    node: Arc<Node<S>>,
    addr: &str,
    workers: usize,
    allow_non_loopback: bool,
) -> Result<Running, String> {
    let parsed: SocketAddr = addr
        .parse()
        .map_err(|_| format!("bad listen address {addr}"))?;
    if !parsed.ip().is_loopback() && !allow_non_loopback {
        return Err("refusing a non-loopback address: this node has no authentication".into());
    }
    let server = Arc::new(Server::http(parsed).map_err(|e| format!("cannot bind {addr}: {e}"))?);
    let addr = server.server_addr().to_ip().ok_or("no socket address")?;
    let json_header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .map_err(|()| "header".to_string())?;
    let handles = (0..workers.max(1))
        .map(|_| {
            let (server, node, header) =
                (Arc::clone(&server), Arc::clone(&node), json_header.clone());
            std::thread::spawn(move || {
                while let Ok(mut request) = server.recv() {
                    let (status, body) = match route(&node, &mut request) {
                        Ok(ok) => ok,
                        Err(error) => (error.status, error.body()),
                    };
                    let response = Response::from_string(body.to_string())
                        .with_status_code(status)
                        .with_header(header.clone());
                    let _ = request.respond(response);
                }
            })
        })
        .collect();
    Ok(Running {
        server,
        workers: handles,
        addr,
    })
}
