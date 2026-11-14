//! `_bulk` NDJSON, streamed: one action and its source at a time, under fixed
//! byte, item and buffer bounds, with a per-item verdict.
//!
//! Nothing here holds the body. A line is read into one reused buffer of at
//! most the configured buffer size; a longer line is counted and discarded as it
//! streams past, so a 200 MiB document costs the same memory as a 1 KiB one.
use std::io::{BufRead, BufReader, Read};

use serde_json::Value;

use crate::ApiError;
use crate::limits::Limits;

/// Largest bulk body by default, bytes (M02.05).
pub const MAX_BULK_BYTES: u64 = 64 * 1024 * 1024;
/// Most actions in one bulk by default.
pub const MAX_BULK_ITEMS: usize = 5000;
/// Largest action line: an action names an index and an id and nothing else.
const MAX_ACTION_LINE: usize = 16 * 1024;
/// The socket reader's own buffer, constant per request.
const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) enum Action {
    Index,
    Delete,
}

#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) action: String,
    pub(crate) index: String,
    pub(crate) id: String,
    /// The action to run, the source it carries, or the reason it was refused.
    pub(crate) work: Result<(Action, Option<Value>), ApiError>,
    /// Raw source bytes this item holds while it waits for its write.
    pub(crate) bytes: usize,
}

fn too_big(what: &str) -> ApiError {
    ApiError::new(
        413,
        "payload_too_large_exception",
        format!("bulk {what} limit exceeded"),
    )
}

/// What one line read produced.
enum Line {
    End,
    /// The text is in the stream's buffer, newline stripped.
    Text,
    /// The line was longer than allowed: its length, nothing kept.
    Oversized(u64),
}

/// A bulk body read as a stream of items.
pub(crate) struct Stream<R: Read> {
    reader: BufReader<R>,
    limits: Limits,
    consumed: u64,
    items: usize,
    line: Vec<u8>,
}

impl<R: Read> Stream<R> {
    pub(crate) fn new(body: R, limits: Limits) -> Self {
        Self {
            reader: BufReader::with_capacity(READ_CHUNK, body),
            limits,
            consumed: 0,
            items: 0,
            line: Vec::new(),
        }
    }

    /// Read one line of at most `cap` content bytes into `self.line`. A longer
    /// line is consumed to its newline without being kept.
    fn read_line(&mut self, cap: usize) -> Result<Line, ApiError> {
        self.line.clear();
        let mut total: u64 = 0;
        let mut kept = true;
        let mut ending: u64 = 0;
        loop {
            let available = self
                .reader
                .fill_buf()
                .map_err(|_| ApiError::bad("the request body could not be read"))?;
            if available.is_empty() {
                break;
            }
            let (take, found) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => {
                    ending = 1 + u64::from(at > 0 && available[at - 1] == b'\r');
                    (at + 1, true)
                }
                None => (available.len(), false),
            };
            self.consumed += take as u64;
            if self.consumed > self.limits.max_bulk_bytes {
                return Err(too_big("byte"));
            }
            total += take as u64;
            // Two spare bytes hold the line ending, which is not content.
            if kept && self.line.len() + take <= cap + 2 {
                self.line.extend_from_slice(&available[..take]);
            } else {
                kept = false;
                self.line.clear();
            }
            self.reader.consume(take);
            if found {
                break;
            }
        }
        if total == 0 {
            return Ok(Line::End);
        }
        if kept {
            while matches!(self.line.last(), Some(b'\n' | b'\r')) {
                self.line.pop();
            }
            if self.line.len() <= cap {
                return Ok(Line::Text);
            }
        }
        self.line.clear();
        Ok(Line::Oversized(total - ending))
    }

    /// The next item, or `None` at the end of the body. A malformed action line
    /// is an error: the stream cannot be resynchronised. A bad or oversized
    /// source line is that item's error.
    pub(crate) fn next(&mut self, default_index: Option<&str>) -> Result<Option<Item>, ApiError> {
        let cap = MAX_ACTION_LINE.min(self.limits.buffer_bytes);
        let text = loop {
            match self.read_line(cap)? {
                Line::End => return Ok(None),
                Line::Oversized(_) => {
                    return Err(ApiError::parse("an action line is too long"));
                }
                Line::Text if self.line.is_empty() => {}
                Line::Text => break std::mem::take(&mut self.line),
            }
        };
        if self.items == self.limits.max_bulk_items {
            return Err(too_big("item"));
        }
        self.items += 1;
        let action: Value = serde_json::from_slice(&text)
            .map_err(|_| ApiError::parse("malformed bulk action line"))?;
        let object = action
            .as_object()
            .filter(|o| o.len() == 1)
            .ok_or_else(|| ApiError::parse("an action line names exactly one action"))?;
        let (name, meta) = object.iter().next().expect("one entry");
        let meta = meta
            .as_object()
            .ok_or_else(|| ApiError::parse("action metadata must be an object"))?;
        if let Some(extra) = meta.keys().find(|k| *k != "_index" && *k != "_id") {
            return Err(ApiError::parse(format!(
                "unsupported action parameter [{extra}]"
            )));
        }
        let index = match (meta.get("_index"), default_index) {
            (Some(Value::String(i)), _) => i.clone(),
            (None, Some(i)) => i.to_string(),
            _ => return Err(ApiError::bad("an action needs an _index")),
        };
        let id = match meta.get("_id") {
            Some(Value::String(i)) if !i.is_empty() => i.clone(),
            _ => String::new(),
        };
        let needs_source = matches!(name.as_str(), "index" | "create" | "update");
        let mut bytes = 0;
        let source: Option<Result<Value, ApiError>> = if needs_source {
            Some(self.source(&mut bytes)?)
        } else {
            None
        };
        let work = if id.is_empty() {
            Err(ApiError::bad("an explicit string _id is required"))
        } else {
            match (name.as_str(), source) {
                ("index", Some(source)) => source.map(|v| (Action::Index, Some(v))),
                ("delete", _) => Ok((Action::Delete, None)),
                ("create" | "update", _) => Err(ApiError::new(
                    400,
                    "unsupported_exception",
                    format!("the [{name}] action is not supported by this node"),
                )),
                _ => return Err(ApiError::parse(format!("unknown bulk action [{name}]"))),
            }
        };
        Ok(Some(Item {
            action: name.clone(),
            index,
            id,
            work,
            bytes,
        }))
    }

    /// Read and parse a source line: the document, bounded by the smaller of the
    /// document limit and the buffer.
    fn source(&mut self, bytes: &mut usize) -> Result<Result<Value, ApiError>, ApiError> {
        let configured = self.limits.max_document_bytes;
        let cap = configured.min(self.limits.buffer_bytes as u64) as usize;
        match self.read_line(cap)? {
            Line::End => Err(ApiError::parse("an action is missing its source line")),
            Line::Oversized(n) => Ok(Err(too_large(n, configured, self.limits.buffer_bytes))),
            Line::Text => {
                *bytes = self.line.len();
                let parsed = serde_json::from_slice::<Value>(&self.line).map_err(|_| {
                    ApiError::new(400, "mapper_parsing_exception", "malformed source line")
                });
                self.line.clear();
                Ok(parsed)
            }
        }
    }
}

/// The structured refusal of a document over a limit, naming that limit.
fn too_large(size: u64, configured: u64, buffer: usize) -> ApiError {
    let reason = if size > configured {
        format!("document of {size} bytes exceeds the document limit of {configured} bytes")
    } else {
        format!(
            "document of {size} bytes exceeds the {buffer}-byte streaming buffer \
             (document limit {configured} bytes); the engine cannot hold a larger document"
        )
    };
    ApiError::new(413, "document_too_large_exception", reason)
}

/// Parse a whole bulk body into memory. Test and small-body helper: the node
/// streams through [`Stream`] instead.
#[cfg(test)]
pub(crate) fn parse<R: Read>(body: R, default_index: Option<&str>) -> Result<Vec<Item>, ApiError> {
    let mut stream = Stream::new(body, Limits::default());
    let mut items = Vec::new();
    while let Some(item) = stream.next(default_index)? {
        items.push(item);
    }
    if items.is_empty() {
        return Err(ApiError::bad("the bulk body has no actions"));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(body: &str) -> Result<Vec<Item>, ApiError> {
        parse(body.as_bytes(), Some("d"))
    }

    #[test]
    fn actions_parse_in_order_with_per_item_verdicts() {
        let items = run("{\"index\":{\"_id\":\"1\"}}\n{\"a\":1}\n{\"delete\":{\"_index\":\"o\",\"_id\":\"2\"}}\r\n\n{\"create\":{\"_id\":\"3\"}}\n{}\n{\"index\":{\"_id\":\"4\"}}\nnot json\n{\"index\":{}}\n{}\n").unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!((items[0].index.as_str(), items[0].id.as_str()), ("d", "1"));
        assert!(matches!(items[0].work, Ok((Action::Index, Some(_)))));
        assert_eq!(items[1].index, "o");
        assert!(matches!(items[1].work, Ok((Action::Delete, None))));
        assert_eq!(
            items[2].work.as_ref().unwrap_err().kind,
            "unsupported_exception"
        );
        assert_eq!(
            items[3].work.as_ref().unwrap_err().kind,
            "mapper_parsing_exception"
        );
        assert_eq!(
            items[4].work.as_ref().unwrap_err().kind,
            "illegal_argument_exception"
        );
    }

    #[test]
    fn a_broken_stream_refuses_the_whole_request() {
        for body in [
            "",
            "\n\n",
            "nope\n",
            "[]\n",
            "{\"index\":{},\"delete\":{}}\n",
            "{\"index\":5}\n",
            "{\"index\":{\"routing\":\"r\",\"_id\":\"1\"}}\n{}\n",
            "{\"index\":{\"_id\":\"1\"}}\n",
            "{\"bogus\":{\"_id\":\"1\"}}\n",
        ] {
            assert!(run(body).is_err(), "{body:?}");
        }
        assert!(parse("{\"delete\":{\"_id\":\"1\"}}\n".as_bytes(), None).is_err());
    }

    #[test]
    fn byte_and_item_limits_are_one_over_refused() {
        let item = "{\"delete\":{\"_id\":\"1\"}}\n";
        let ok = item.repeat(MAX_BULK_ITEMS);
        assert_eq!(run(&ok).unwrap().len(), MAX_BULK_ITEMS);
        let over = item.repeat(MAX_BULK_ITEMS + 1);
        assert_eq!(run(&over).unwrap_err().status, 413);
        // The byte budget counts every byte read, kept or discarded.
        let small = Limits {
            max_document_bytes: 64,
            max_bulk_bytes: 200,
            ..Limits::default()
        };
        let mut body = String::from("{\"index\":{\"_id\":\"1\"}}\n{\"a\":\"");
        body.push_str(&"x".repeat(300));
        body.push_str("\"}\n");
        let mut stream = Stream::new(body.as_bytes(), small);
        assert_eq!(stream.next(Some("d")).unwrap_err().status, 413);
    }

    fn limits(document: u64, buffer: usize) -> Limits {
        Limits {
            max_document_bytes: document,
            buffer_bytes: buffer,
            ..Limits::default()
        }
    }

    /// A one-line document of exactly `len` bytes.
    fn doc_of(len: usize) -> String {
        let head = "{\"a\":\"";
        let tail = "\"}";
        format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
    }

    fn all(body: &str, limits: Limits) -> Vec<Item> {
        let mut stream = Stream::new(body.as_bytes(), limits);
        let mut out = Vec::new();
        while let Some(item) = stream.next(Some("d")).unwrap() {
            out.push(item);
        }
        out
    }

    #[test]
    fn a_document_at_the_limit_is_kept_and_one_byte_over_is_refused_naming_it() {
        let limit = 4096;
        for ending in ["\n", "\r\n"] {
            let body = format!(
                "{{\"index\":{{\"_id\":\"1\"}}}}\n{}{ending}{{\"index\":{{\"_id\":\"2\"}}}}\n{}{ending}{{\"index\":{{\"_id\":\"3\"}}}}\n{}{ending}",
                doc_of(limit),
                doc_of(limit + 1),
                doc_of(10),
            );
            let items = all(&body, limits(limit as u64, 1 << 20));
            assert_eq!(items.len(), 3, "the stream resynchronises after a refusal");
            assert!(items[0].work.is_ok());
            assert_eq!(items[0].bytes, limit);
            let refusal = items[1].work.as_ref().unwrap_err();
            assert_eq!(
                (refusal.status, refusal.kind),
                (413, "document_too_large_exception")
            );
            assert!(refusal.reason.contains("4097 bytes"), "{}", refusal.reason);
            assert!(
                refusal.reason.contains("limit of 4096 bytes"),
                "{}",
                refusal.reason
            );
            assert_eq!(items[1].bytes, 0, "a refused document holds no memory");
            assert!(items[2].work.is_ok());
        }
    }

    #[test]
    fn a_document_between_the_buffer_and_the_limit_is_refused_naming_the_buffer() {
        let buffer = 64 * 1024;
        let body = format!(
            "{{\"index\":{{\"_id\":\"1\"}}}}\n{}\n{{\"delete\":{{\"_id\":\"2\"}}}}\n",
            doc_of(buffer + 1)
        );
        let items = all(&body, limits(1 << 20, buffer));
        let refusal = items[0].work.as_ref().unwrap_err();
        assert_eq!(refusal.kind, "document_too_large_exception");
        assert!(
            refusal.reason.contains("streaming buffer"),
            "{}",
            refusal.reason
        );
        assert!(matches!(items[1].work, Ok((Action::Delete, None))));
    }

    #[test]
    fn lines_spanning_many_reads_and_one_byte_readers_parse_the_same() {
        struct Drip<'a>(&'a [u8]);
        impl Read for Drip<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(1).min(out.len());
                out[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let body = format!(
            "{{\"index\":{{\"_id\":\"1\"}}}}\n{}\n{{\"delete\":{{\"_id\":\"2\"}}}}\n",
            doc_of(300_000)
        );
        let whole = all(&body, limits(1 << 20, 1 << 20));
        let mut dripped = Stream::new(Drip(body.as_bytes()), limits(1 << 20, 1 << 20));
        let mut count = 0;
        while let Some(item) = dripped.next(Some("d")).unwrap() {
            assert_eq!(item.bytes, whole[count].bytes);
            count += 1;
        }
        assert_eq!((count, whole[0].bytes), (2, 300_000));
    }

    #[test]
    fn an_overlong_action_line_refuses_the_request() {
        let body = format!(
            "{{\"delete\":{{\"_id\":\"{}\"}}}}\n",
            "i".repeat(MAX_ACTION_LINE)
        );
        let mut stream = Stream::new(body.as_bytes(), Limits::default());
        assert_eq!(stream.next(Some("d")).unwrap_err().status, 400);
    }
}
