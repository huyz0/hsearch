//! `_bulk` NDJSON, streamed: one action and its source at a time, under fixed
//! byte, item and buffer bounds, with a per-item verdict.
//!
//! Nothing here holds the body. An action line is read into one reused buffer;
//! a source line is parsed as it streams in, so a large text field is held once,
//! as the `String` that becomes the field. A longer line than allowed is counted
//! and discarded as it streams past.
use std::io::{BufRead, BufReader, Read};

use serde_json::Value;

use crate::ApiError;
use crate::limits::Limits;
use crate::source_line::{self, Source};

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
    /// The line was longer than allowed: nothing kept.
    Oversized,
}

/// A bulk body read as a stream of items.
pub(crate) struct Stream<R: Read> {
    reader: BufReader<R>,
    limits: Limits,
    consumed: u64,
    items: usize,
    /// Action lines only: a source line is never buffered.
    line: Vec<u8>,
    /// Whether a source may hold a flat array of numbers (a vector).
    arrays: bool,
}

impl<R: Read> Stream<R> {
    pub(crate) fn new(body: R, limits: Limits) -> Self {
        Self {
            reader: BufReader::with_capacity(READ_CHUNK, body),
            limits,
            consumed: 0,
            items: 0,
            line: Vec::new(),
            arrays: false,
        }
    }

    /// This stream reading a flat array of numbers in a source as a value.
    #[must_use]
    pub(crate) fn with_arrays(mut self, arrays: bool) -> Self {
        self.arrays = arrays;
        self
    }

    /// Read one line of at most `cap` content bytes into `self.line`. A longer
    /// line is consumed to its newline without being kept.
    fn read_line(&mut self, cap: usize) -> Result<Line, ApiError> {
        self.line.clear();
        let mut total: u64 = 0;
        let mut kept = true;
        loop {
            let available = self
                .reader
                .fill_buf()
                .map_err(|_| ApiError::bad("the request body could not be read"))?;
            if available.is_empty() {
                break;
            }
            let (take, found) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => (at + 1, true),
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
        Ok(Line::Oversized)
    }

    /// The next item, or `None` at the end of the body. A malformed action line
    /// is an error: the stream cannot be resynchronised. A bad or oversized
    /// source line is that item's error.
    pub(crate) fn next(&mut self, default_index: Option<&str>) -> Result<Option<Item>, ApiError> {
        let cap = MAX_ACTION_LINE.min(self.limits.buffer_bytes);
        let action: Value = loop {
            match self.read_line(cap)? {
                Line::End => return Ok(None),
                Line::Oversized => {
                    return Err(ApiError::parse("an action line is too long"));
                }
                Line::Text if self.line.is_empty() => {}
                Line::Text => {
                    if self.items == self.limits.max_bulk_items {
                        return Err(too_big("item"));
                    }
                    self.items += 1;
                    // Parsed in place: the buffer stays this stream's, to be reused.
                    let parsed = serde_json::from_slice(&self.line);
                    self.line.clear();
                    break parsed.map_err(|_| ApiError::parse("malformed bulk action line"))?;
                }
            }
        };
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
    /// document limit and the buffer. The line is parsed as it streams in and
    /// never held whole, so a large text field is in memory once.
    fn source(&mut self, bytes: &mut usize) -> Result<Result<Value, ApiError>, ApiError> {
        let configured = self.limits.max_document_bytes;
        let indexable = INDEXABLE_SOURCE_BYTES as u64;
        let cap = configured
            .min(self.limits.buffer_bytes as u64)
            .min(indexable);
        let read = source_line::read(
            &mut self.reader,
            &mut self.consumed,
            self.limits.max_bulk_bytes,
            cap,
            self.arrays,
        )?;
        match read {
            Source::End => Err(ApiError::parse("an action is missing its source line")),
            Source::Oversized(n) => Ok(Err(too_large(
                n,
                configured,
                self.limits.buffer_bytes,
                indexable,
            ))),
            Source::Refused(reason) => {
                Ok(Err(ApiError::new(400, "mapper_parsing_exception", reason)))
            }
            Source::Document(value, len) => {
                *bytes = len;
                Ok(Ok(value))
            }
        }
    }
}

/// Largest source line the write path can index: the index's document ceiling
/// less what the engine charges to a document but the source line does not carry:
/// the identity (at most 256 bytes) and 8 bytes for each non-string value where
/// the JSON may spend one (at most 7 more for each of 14 fields).
const INDEXABLE_SOURCE_BYTES: usize = hsearch_index::MAX_INDEXED_DOCUMENT_BYTES - 512;

/// The structured refusal of a document over a limit, naming that limit.
fn too_large(size: u64, configured: u64, buffer: usize, indexable: u64) -> ApiError {
    let reason = if size > configured {
        format!("document of {size} bytes exceeds the document limit of {configured} bytes")
    } else if size > buffer as u64 {
        format!(
            "document of {size} bytes exceeds the {buffer}-byte streaming buffer \
             (document limit {configured} bytes); the engine cannot hold a larger document"
        )
    } else {
        format!(
            "document of {size} bytes is within the document limit of {configured} bytes \
             but this build can index documents of at most {indexable} bytes"
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

    #[test]
    fn a_large_source_line_is_never_buffered() {
        let big = format!(
            "{{\"index\":{{\"_id\":\"1\"}}}}\n{}\n{{\"index\":{{\"_id\":\"2\"}}}}\n{}\n",
            doc_of(3 << 20),
            doc_of(100)
        );
        let mut stream = Stream::new(big.as_bytes(), limits(8 << 20, 8 << 20));
        let first = stream.next(Some("d")).unwrap().unwrap();
        assert_eq!(first.bytes, 3 << 20);
        assert!(
            stream.line.capacity() <= 64,
            "the line buffer held the document: {}",
            stream.line.capacity()
        );
        assert!(stream.next(Some("d")).unwrap().is_some());
    }

    /// The source line `line`, after an action, as the item's verdict.
    fn source_of(line: &str) -> Result<serde_json::Value, ApiError> {
        let body =
            format!("{{\"index\":{{\"_id\":\"1\"}}}}\n{line}\n{{\"delete\":{{\"_id\":\"2\"}}}}\n");
        let items = all(&body, Limits::default());
        assert_eq!(items.len(), 2, "the stream resynchronises after {line:?}");
        assert!(matches!(items[1].work, Ok((Action::Delete, None))));
        match items.into_iter().next().unwrap().work {
            Ok((_, Some(v))) => Ok(v),
            Ok(_) => unreachable!(),
            Err(e) => Err(e),
        }
    }

    #[test]
    fn a_source_parses_to_what_serde_json_parses() {
        for line in [
            r#"{}"#,
            r#"  { "a" : "x" , "b":1,"c":-2.5e3,"d":true,"e":false,"f":null }  "#,
            r#"{"a":"\" \\ \/ \b \f \n \r \t \u00e9 \ud83d\ude00 é 世界 😀"}"#,
            r#"{"a":"x","a":"y"}"#,
            r#"{"":""}"#,
        ] {
            let want: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(source_of(line).unwrap(), want, "{line}");
        }
    }

    #[test]
    fn a_source_serde_json_refuses_is_refused_and_the_next_line_still_parses() {
        for line in [
            "",
            "   ",
            "not json",
            r#"{"a":1,}"#,
            r#"{"a" 1}"#,
            r#"{a:1}"#,
            r#"{"a":01}"#,
            r#"{"a":tru}"#,
            r#"{"a":1 "b":2}"#,
            r#"{"a":"x"} trailing"#,
            r#"{"a":"x""#,
            r#"{"a":"unterminated}"#,
            r#"{"a":"\x"}"#,
            r#"{"a":"\u12"}"#,
            r#"{"a":"\ud83d"}"#,
            r#"{"a":"\ud83d\u0041"}"#,
            r#"{"a":"\ude00"}"#,
            "{\"a\":\"tab\there\"}",
            r#"{"a":1e999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999}"#,
        ] {
            assert!(
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(line).is_err()
                    || line.contains("e999"),
                "{line:?} is a valid object to serde_json"
            );
            let refusal = source_of(line).unwrap_err();
            assert_eq!(refusal.kind, "mapper_parsing_exception", "{line:?}");
        }
    }

    #[test]
    fn the_member_and_nesting_bounds_are_exact_and_name_their_reason() {
        let members = |n: usize| {
            format!(
                "{{{}}}",
                (0..n)
                    .map(|i| format!("\"k{i}\":1"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        // 256 members is 255 commas: kept; 257 members is 256 commas: refused.
        assert!(source_of(&members(256)).is_ok());
        let refusal = source_of(&members(257)).unwrap_err();
        assert!(
            refusal.reason.contains("more members"),
            "{}",
            refusal.reason
        );
        for nested in [r#"{"a":[1]}"#, r#"{"a":{}}"#, r#"{"a":[]}"#] {
            let refusal = source_of(nested).unwrap_err();
            assert!(
                refusal.reason.contains("nested"),
                "{nested}: {}",
                refusal.reason
            );
        }
    }

    #[test]
    fn a_line_over_the_cap_is_refused_by_its_size_whatever_stopped_the_parse_first() {
        let limit = 4096;
        for head in ["{\"a\":[", "[1,", "not json", "{\"a\":1x", "{\"a\":\"\\x"] {
            for ending in ["\n", "\r\n"] {
                let line = format!("{head}{}", "z".repeat(limit + 10 - head.len()));
                let body = format!(
                    "{{\"index\":{{\"_id\":\"1\"}}}}\n{line}{ending}{{\"delete\":{{\"_id\":\"2\"}}}}\n"
                );
                let items = all(&body, limits(limit as u64, 1 << 20));
                assert_eq!(items.len(), 2, "{head:?} resynchronises");
                let refusal = items[0].work.as_ref().unwrap_err();
                assert_eq!(refusal.kind, "document_too_large_exception", "{head:?}");
                assert!(
                    refusal.reason.contains("4106 bytes"),
                    "{head:?}: {}",
                    refusal.reason
                );
            }
        }
    }

    #[test]
    fn a_long_number_is_whatever_serde_json_says_of_it() {
        let long = format!("{{\"a\":0.{}1}}", "0".repeat(300));
        let want: serde_json::Value = serde_json::from_str(&long).unwrap();
        assert_eq!(source_of(&long).unwrap(), want);
        let huge = format!("{{\"a\":1e{}}}", "9".repeat(300));
        assert!(serde_json::from_str::<serde_json::Value>(&huge).is_err());
        assert_eq!(
            source_of(&huge).unwrap_err().kind,
            "mapper_parsing_exception"
        );
    }

    #[test]
    fn a_source_that_is_not_an_object_says_so() {
        for line in ["[1]", "\"s\"", "5", "true", "null", "-1"] {
            let refusal = source_of(line).unwrap_err();
            assert_eq!(refusal.kind, "mapper_parsing_exception", "{line}");
            assert!(refusal.reason.contains("must be a JSON object"), "{line}");
        }
    }

    #[test]
    fn invalid_utf8_in_a_source_is_refused_whole_or_split_across_reads() {
        struct Drip<'a>(&'a [u8]);
        impl Read for Drip<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(1).min(out.len());
                out[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        for (bytes, ok) in [
            (&b"{\"a\":\"\xff\"}\n"[..], false),
            (&b"{\"a\":\"\xc3\"}\n"[..], false),
            (&b"{\"a\":\"\xe4\xb8\"}\n"[..], false),
            (&b"{\"a\":\"\xe4\xb8\x96\"}\n"[..], true),
            (&b"{\"a\":\"\xf0\x9f\x98\x80\"}\n"[..], true),
        ] {
            let mut body = b"{\"index\":{\"_id\":\"1\"}}\n".to_vec();
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"{\"delete\":{\"_id\":\"2\"}}\n");
            for drip in [false, true] {
                let mut stream = if drip {
                    let mut s = Stream::new(Drip(&body), Limits::default());
                    s.line.reserve(0);
                    let mut out = Vec::new();
                    while let Some(i) = s.next(Some("d")).unwrap() {
                        out.push(i);
                    }
                    out
                } else {
                    let mut s = Stream::new(body.as_slice(), Limits::default());
                    let mut out = Vec::new();
                    while let Some(i) = s.next(Some("d")).unwrap() {
                        out.push(i);
                    }
                    out
                };
                assert_eq!(stream.len(), 2, "{bytes:?} drip {drip}");
                assert_eq!(stream.remove(0).work.is_ok(), ok, "{bytes:?} drip {drip}");
            }
        }
    }

    #[test]
    fn a_multibyte_character_split_by_the_reader_buffer_is_kept_whole() {
        // 3-byte characters straddle the 64 KiB buffer's end at every offset.
        for pad in 0..4 {
            let text = format!("{}{}", "x".repeat(READ_CHUNK - 40 + pad), "世".repeat(30));
            let line = format!("{{\"a\":\"{text}\"}}");
            let want: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(source_of(&line).unwrap(), want, "pad {pad}");
        }
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

    #[test]
    fn amplifying_shapes_are_refused_before_a_value_is_built() {
        let nested = format!("[{}]", "1,".repeat(1000));
        let many = format!(
            "{{{}\"z\":1}}",
            (0..300).map(|i| format!("\"k{i}\":1,")).collect::<String>()
        );
        let body = format!(
            "{{\"index\":{{\"_id\":\"1\"}}}}\n{{\"a\":{nested}}}\n{{\"index\":{{\"_id\":\"2\"}}}}\n{many}\n{{\"index\":{{\"_id\":\"3\"}}}}\n{{\"a\":\"[{{,}}]\\\"[\",\"b\":2}}\n"
        );
        let items = all(&body, Limits::default());
        for refused in &items[..2] {
            assert_eq!(
                refused.work.as_ref().unwrap_err().kind,
                "mapper_parsing_exception"
            );
        }
        // Brackets and commas inside a string, escaped quotes included, are text.
        assert!(items[2].work.is_ok());
    }

    #[test]
    fn a_crlf_split_across_reads_reports_the_exact_size() {
        struct Split<'a>(&'a [u8], bool);
        impl Read for Split<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                // Break just before the "\n" of the first "\r\n".
                let at = self.0.windows(2).position(|w| w == b"\r\n");
                let n = match at {
                    Some(at) if !self.1 => (at + 1).min(out.len()),
                    _ => self.0.len().min(out.len()),
                };
                self.1 = true;
                out[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let body = format!("{{\"index\":{{\"_id\":\"1\"}}}}\n{}\r\n", doc_of(5000));
        let mut stream = Stream::new(Split(body.as_bytes(), false), limits(4096, 1 << 20));
        let item = stream.next(Some("d")).unwrap().unwrap();
        let refusal = item.work.unwrap_err();
        assert!(refusal.reason.contains("5000 bytes"), "{}", refusal.reason);
    }

    fn arrays(body: &str) -> Vec<Item> {
        let mut stream = Stream::new(body.as_bytes(), Limits::default()).with_arrays(true);
        let mut out = Vec::new();
        while let Some(item) = stream.next(Some("d")).unwrap() {
            out.push(item);
        }
        out
    }

    #[test]
    fn an_array_source_is_charged_for_what_its_elements_hold() {
        let zeros = vec!["0"; 4096].join(",");
        let body = format!("{{\"index\":{{\"_id\":\"1\"}}}}\n{{\"v\":[{zeros}]}}\n");
        let items = arrays(&body);
        assert!(items[0].work.is_ok());
        // Two text bytes an element, 32 resident: the charge is the larger.
        assert!(items[0].bytes >= 4096 * 32, "{}", items[0].bytes);
    }

    #[test]
    fn an_array_over_the_bound_or_with_a_bad_element_is_refused_and_the_stream_resyncs() {
        let over = vec!["0"; 4097].join(",");
        for source in [
            format!("{{\"v\":[{over}]}}"),
            r#"{"v":[1,"x"]}"#.to_string(),
            r#"{"v":[1,null]}"#.to_string(),
            r#"{"v":[1,[2]]}"#.to_string(),
        ] {
            let body = format!(
                "{{\"index\":{{\"_id\":\"1\"}}}}\n{source}\n{{\"index\":{{\"_id\":\"2\"}}}}\n{{\"v\":[1,2]}}\n"
            );
            let items = arrays(&body);
            assert_eq!(items.len(), 2, "{source}");
            assert!(items[0].work.is_err(), "{source}");
            assert!(
                items[1].work.is_ok(),
                "the next item must parse after {source}"
            );
        }
        let reason = |s: &str| {
            let body = format!("{{\"index\":{{\"_id\":\"1\"}}}}\n{s}\n");
            arrays(&body)[0].work.as_ref().unwrap_err().reason.clone()
        };
        assert!(reason(r#"{"v":[1,"x"]}"#).contains("only numbers"));
        assert!(reason(r#"{"v":[1,[2]]}"#).contains("nested"));
    }
}
