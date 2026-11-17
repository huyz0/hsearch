//! One `_bulk` source line, parsed as it streams in.
//!
//! The line is never buffered. A JSON object of scalars is read straight off
//! the socket reader, and a string value is decoded into the `String` that
//! becomes the field, so a 200 MiB text field is held once while it is read and
//! not again as a line buffer. The caller's bounds still hold: the bytes of the
//! line count against the document cap as they pass, and a line that is
//! refused (oversized, malformed, nested or with too many members) is counted
//! to its newline and dropped so the stream resynchronises.
use std::io::{BufRead, BufReader, Read};

use serde_json::{Map, Value};

use crate::ApiError;

/// Most top-level members a source may carry: the mapping allows 14 fields, so
/// a document with more than this can only be refused.
const MAX_MEMBERS: usize = 256;
/// Longest bare token (a number or `true`/`false`/`null`) read for a value.
const MAX_TOKEN: usize = 4096;
/// Most numbers in one array value: a vector field's own bound.
const MAX_ARRAY: usize = 4096;
const NESTED: &str = "nested objects and arrays are not supported by this mapping";
const NOT_OBJECT: &str = "the source must be a JSON object";
const TOO_MANY: &str = "the source has more members than any mapping allows";

/// What reading one source line produced.
pub(crate) enum Source {
    /// The body ended before the line began.
    End,
    /// The line, parsed, and its length in bytes without the line ending.
    Document(Value, usize),
    /// The line was longer than the cap: its length. It was dropped.
    Oversized(u64),
    /// The line was refused for this reason (a `mapper_parsing_exception`).
    Refused(&'static str),
}

/// Why a parse stopped early.
enum Stop {
    /// The request itself failed: an unreadable body or the byte budget.
    Request(ApiError),
    Malformed,
    Refused(&'static str),
    Oversized,
}

struct Line<'a, R: Read> {
    reader: &'a mut BufReader<R>,
    /// The request's bytes read so far, shared with the caller.
    consumed: &'a mut u64,
    max_bulk: u64,
    cap: u64,
    /// Bytes of this line read so far, line ending included.
    line: u64,
    previous: u8,
    /// The byte before the last newline read, to tell `\r\n` from `\n`.
    before_newline: u8,
    /// Whether a flat array of numbers is a value (a vector); off, any array is nested.
    arrays: bool,
}

impl<'a, R: Read> Line<'a, R> {
    fn peek(&mut self) -> Result<Option<u8>, Stop> {
        let buf = self.reader.fill_buf().map_err(unreadable)?;
        Ok(buf.first().copied())
    }

    /// Consume `n` bytes of the buffer `fill_buf` last returned.
    fn bump(&mut self, n: usize, last: u8) -> Result<(), Stop> {
        self.reader.consume(n);
        if last == b'\n' {
            self.before_newline = self.previous;
        }
        self.previous = last;
        *self.consumed += n as u64;
        if *self.consumed > self.max_bulk {
            return Err(Stop::Request(ApiError::new(
                413,
                "payload_too_large_exception",
                "bulk byte limit exceeded",
            )));
        }
        self.line += n as u64;
        // Two spare bytes hold the line ending, which is not content.
        if self.line > self.cap + 2 {
            return Err(Stop::Oversized);
        }
        Ok(())
    }

    fn byte(&mut self) -> Result<Option<u8>, Stop> {
        match self.peek()? {
            Some(b) => {
                self.bump(1, b)?;
                Ok(Some(b))
            }
            None => Ok(None),
        }
    }

    fn skip_space(&mut self) -> Result<(), Stop> {
        while let Some(b @ (b' ' | b'\t' | b'\r')) = self.peek()? {
            self.bump(1, b)?;
        }
        Ok(())
    }

    fn expect(&mut self, want: u8) -> Result<(), Stop> {
        match self.byte()? {
            Some(b) if b == want => Ok(()),
            _ => Err(Stop::Malformed),
        }
    }

    /// The rest of a JSON string after its opening quote, decoded into `out`.
    fn string(&mut self, out: &mut String) -> Result<(), Stop> {
        // The bytes of a character split across two buffer fills.
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let buf = self.reader.fill_buf().map_err(unreadable)?;
            if buf.is_empty() {
                return Err(Stop::Malformed);
            }
            let run = buf
                .iter()
                .position(|&b| b == b'"' || b == b'\\' || b < 0x20)
                .unwrap_or(buf.len());
            if run > 0 {
                let last = buf[run - 1];
                append(out, &mut carry, &buf[..run])?;
                self.bump(run, last)?;
                continue;
            }
            let b = buf[0];
            self.bump(1, b)?;
            match b {
                b'"' if carry.is_empty() => return Ok(()),
                b'\\' if carry.is_empty() => self.escape(out)?,
                _ => return Err(Stop::Malformed),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), Stop> {
        let c = match self.byte()?.ok_or(Stop::Malformed)? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let first = self.hex4()?;
                match first {
                    0xD800..=0xDBFF => {
                        self.expect(b'\\')?;
                        self.expect(b'u')?;
                        let second = self.hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&second) {
                            return Err(Stop::Malformed);
                        }
                        let code = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                        char::from_u32(code).ok_or(Stop::Malformed)?
                    }
                    0xDC00..=0xDFFF => return Err(Stop::Malformed),
                    _ => char::from_u32(first).ok_or(Stop::Malformed)?,
                }
            }
            _ => return Err(Stop::Malformed),
        };
        out.push(c);
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, Stop> {
        let mut n = 0u32;
        for _ in 0..4 {
            let digit = (self.byte()?.ok_or(Stop::Malformed)? as char)
                .to_digit(16)
                .ok_or(Stop::Malformed)?;
            n = n * 16 + digit;
        }
        Ok(n)
    }

    /// A number, `true`, `false` or `null`: a short run of token bytes, parsed
    /// by `serde_json` so the accepted forms are exactly its own.
    fn token(&mut self) -> Result<Value, Stop> {
        let mut token = Vec::new();
        while let Some(b) = self.peek()? {
            if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
                break;
            }
            token.push(b);
            if token.len() > MAX_TOKEN {
                return Err(Stop::Malformed);
            }
            self.bump(1, b)?;
        }
        match serde_json::from_slice::<Value>(&token) {
            Ok(v @ (Value::Number(_) | Value::Bool(_) | Value::Null)) => Ok(v),
            _ => Err(Stop::Malformed),
        }
    }

    /// A flat array of numbers, read one token at a time and bounded by
    /// [`MAX_ARRAY`]; anything else inside it is nested.
    fn numbers(&mut self) -> Result<Value, Stop> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_space()?;
        if self.peek()? == Some(b']') {
            self.bump(1, b']')?;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_space()?;
            match self.peek()?.ok_or(Stop::Malformed)? {
                b'[' | b'{' | b'"' => return Err(Stop::Refused(NESTED)),
                _ => match self.token()? {
                    v @ Value::Number(_) => items.push(v),
                    _ => return Err(Stop::Refused(NESTED)),
                },
            }
            if items.len() > MAX_ARRAY {
                return Err(Stop::Refused(TOO_MANY));
            }
            self.skip_space()?;
            match self.byte()?.ok_or(Stop::Malformed)? {
                b',' => {}
                b']' => return Ok(Value::Array(items)),
                _ => return Err(Stop::Malformed),
            }
        }
    }

    fn object(&mut self) -> Result<Value, Stop> {
        self.skip_space()?;
        match self.peek()? {
            Some(b'{') => self.bump(1, b'{')?,
            // Looks like a JSON value that is not an object: not this mapping's.
            Some(b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n') => {
                return Err(Stop::Refused(NOT_OBJECT));
            }
            _ => return Err(Stop::Malformed),
        }
        let mut map = Map::new();
        let mut commas = 0usize;
        self.skip_space()?;
        if self.peek()? == Some(b'}') {
            self.bump(1, b'}')?;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_space()?;
            self.expect(b'"')?;
            let mut key = String::new();
            self.string(&mut key)?;
            self.skip_space()?;
            self.expect(b':')?;
            self.skip_space()?;
            let value = match self.peek()?.ok_or(Stop::Malformed)? {
                b'"' => {
                    self.bump(1, b'"')?;
                    let mut text = String::new();
                    self.string(&mut text)?;
                    Value::String(text)
                }
                b'[' if self.arrays => self.numbers()?,
                b'{' | b'[' => return Err(Stop::Refused(NESTED)),
                _ => self.token()?,
            };
            map.insert(key, value);
            self.skip_space()?;
            match self.byte()?.ok_or(Stop::Malformed)? {
                b',' => {
                    commas += 1;
                    if commas >= MAX_MEMBERS {
                        return Err(Stop::Refused(TOO_MANY));
                    }
                }
                b'}' => return Ok(Value::Object(map)),
                _ => return Err(Stop::Malformed),
            }
        }
    }

    /// After the value: spaces, then the end of the line or of the body.
    fn finish(&mut self) -> Result<(), Stop> {
        self.skip_space()?;
        match self.peek()? {
            None => Ok(()),
            Some(b'\n') => self.bump(1, b'\n'),
            Some(_) => Err(Stop::Malformed),
        }
    }

    /// Count the rest of the line to its newline without keeping it.
    fn drain(&mut self) -> Result<(), ApiError> {
        loop {
            let buf = self.reader.fill_buf().map_err(unreadable_api)?;
            if buf.is_empty() {
                return Ok(());
            }
            let (take, found) = match buf.iter().position(|b| *b == b'\n') {
                Some(at) => (at + 1, true),
                None => (buf.len(), false),
            };
            if found {
                self.before_newline = if take > 1 {
                    buf[take - 2]
                } else {
                    self.previous
                };
            }
            self.previous = buf[take - 1];
            self.reader.consume(take);
            *self.consumed += take as u64;
            if *self.consumed > self.max_bulk {
                return Err(ApiError::new(
                    413,
                    "payload_too_large_exception",
                    "bulk byte limit exceeded",
                ));
            }
            self.line += take as u64;
            if found {
                return Ok(());
            }
        }
    }

    /// The length of the line without its ending, once it has been read whole.
    fn content(&self) -> u64 {
        if self.previous == b'\n' {
            self.line - 1 - u64::from(self.before_newline == b'\r')
        } else {
            self.line
        }
    }
}

fn unreadable(_: std::io::Error) -> Stop {
    Stop::Request(unreadable_api(()))
}

fn unreadable_api<T>(_: T) -> ApiError {
    ApiError::bad("the request body could not be read")
}

/// Append `chunk` to `out`, carrying a character cut by the end of a buffer to
/// the next call. Invalid UTF-8 is malformed.
fn append(out: &mut String, carry: &mut Vec<u8>, mut chunk: &[u8]) -> Result<(), Stop> {
    while !carry.is_empty() && !chunk.is_empty() {
        carry.push(chunk[0]);
        chunk = &chunk[1..];
        match std::str::from_utf8(carry) {
            Ok(text) => {
                out.push_str(text);
                carry.clear();
            }
            Err(e) if e.error_len().is_none() => {}
            Err(_) => return Err(Stop::Malformed),
        }
    }
    match std::str::from_utf8(chunk) {
        Ok(text) => out.push_str(text),
        Err(e) => {
            let (valid, rest) = chunk.split_at(e.valid_up_to());
            // `valid` is checked UTF-8 by the error's own offset.
            out.push_str(std::str::from_utf8(valid).map_err(|_| Stop::Malformed)?);
            if e.error_len().is_some() {
                return Err(Stop::Malformed);
            }
            carry.extend_from_slice(rest);
        }
    }
    Ok(())
}

/// Read one source line of at most `cap` content bytes from `reader`.
///
/// `consumed` is the request's byte count, `max_bulk` its limit: an `Err` is the
/// request failing (the body could not be read, or it is over its byte budget),
/// not this line.
pub(crate) fn read<R: Read>(
    reader: &mut BufReader<R>,
    consumed: &mut u64,
    max_bulk: u64,
    cap: u64,
    arrays: bool,
) -> Result<Source, ApiError> {
    let mut line = Line {
        reader,
        consumed,
        max_bulk,
        cap,
        line: 0,
        previous: 0,
        before_newline: 0,
        arrays,
    };
    if line.peek().map_err(request)?.is_none() {
        return Ok(Source::End);
    }
    let parsed = line.object().and_then(|v| line.finish().map(|()| v));
    let stopped = match parsed {
        Ok(value) => {
            let content = line.content();
            return Ok(if content <= cap {
                Source::Document(value, content as usize)
            } else {
                // Inside the two spare bytes, but over: refused by its size.
                Source::Oversized(content)
            });
        }
        Err(stopped) => stopped,
    };
    if let Stop::Request(e) = stopped {
        return Err(e);
    }
    // Resynchronise: the rest of the line is counted and dropped. A line that
    // stopped at its own newline has nothing left to drain.
    if line.previous != b'\n' {
        line.drain()?;
    }
    // A line over the cap is refused by its size, whatever stopped the parse first.
    let content = line.content();
    if content > cap {
        return Ok(Source::Oversized(content));
    }
    Ok(match stopped {
        Stop::Refused(reason) => Source::Refused(reason),
        _ => Source::Refused("malformed source line"),
    })
}

fn request(stop: Stop) -> ApiError {
    match stop {
        Stop::Request(e) => e,
        _ => ApiError::bad("the request body could not be read"),
    }
}
