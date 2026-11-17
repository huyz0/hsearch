//! Bounded, explicit materialized-index schema. No wire parsing or coercion.
use std::sync::Arc;

use lucene_analysis::{Analyzer, StandardAnalyzer};
use lucene_index::document::{Document, KeywordField, LongField, SharedText, Store, TextField};

use crate::{Error, Result};

/// Maximum native fields, including two reserved identity/version fields.
pub const MAX_FIELDS: usize = 16;
/// Maximum total UTF-8 bytes in one document, including names and identity.
pub const MAX_DOCUMENT_BYTES: usize = 8192;
/// Longest keyword term in bytes: Lucene's `IndexWriter.MAX_TERM_LENGTH`, which no
/// document limit raises.
pub const MAX_KEYWORD_BYTES: usize = 32_766;
/// Maximum analyzer tokens per document or query.
pub const MAX_TERMS: usize = 128;
/// Maximum retained logical identities, including deleted identities.
pub const MAX_DOCUMENTS: usize = 128;
/// Maximum admitted mutation operations over one adapter lifetime/checkpoint.
pub const MAX_OPERATIONS: usize = 256;

/// Largest `auto_merge_mb` the engine accepts; a larger value is clamped.
pub const MAX_AUTO_MERGE_MB: usize = 4096;

/// Admission bounds for one adapter. [`Limits::BOUNDED`] is the research
/// profile and the default; [`Limits::scale`] is the opt-in first-slice profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Retained logical identities, including deleted ones, and the largest
    /// native scope a search, count or aggregation will read.
    pub documents: usize,
    /// Mutation operations admitted over one adapter lifetime or checkpoint.
    pub operations: usize,
    /// Bytes of retry history kept. Over it, [`Limits::window`] decides.
    pub history_bytes: usize,
    /// Native segments a writer or snapshot may hold.
    pub segments: usize,
    /// Documents buffered by the native writer before it flushes a segment.
    pub buffered_documents: usize,
    /// When true the oldest retry history is dropped to make room, so an exact
    /// retry older than the window is no longer recognised here: the caller's
    /// log owns request identity. When false the adapter refuses instead.
    pub window: bool,
    /// Largest document, in bytes of identity, field names and values. A
    /// document over it is refused with [`Error::Limit`] before any analysis.
    pub document_bytes: usize,
    /// Most analyzed tokens in one document across its text fields. At or above
    /// `document_bytes` it cannot bind (a token is at least one byte) and no
    /// analysis pass is spent counting.
    pub terms: usize,
    /// Largest segment, in MiB, that the writer merges by itself on refresh
    /// (tiered, small segments first); 0 turns automatic merging off. Without it
    /// every refresh adds a segment until `segments` is hit and one blocking merge
    /// of the whole index runs on a write.
    pub auto_merge_mb: usize,
}

impl Limits {
    /// The bounded research profile: today's constants, refusing at each.
    pub const BOUNDED: Self = Self {
        documents: MAX_DOCUMENTS,
        operations: MAX_OPERATIONS,
        history_bytes: 262_144,
        segments: 32,
        buffered_documents: 8,
        window: false,
        document_bytes: MAX_DOCUMENT_BYTES,
        terms: MAX_TERMS,
        auto_merge_mb: 0,
    };

    /// The first-slice scale profile: a million identities, a bounded retry
    /// window, and larger segments so a bulk load does not fragment.
    pub const fn scale() -> Self {
        Self {
            documents: 1_000_000,
            operations: 100_000_000,
            history_bytes: 4 * 1024 * 1024,
            segments: 256,
            buffered_documents: 10_000,
            window: true,
            document_bytes: MAX_DOCUMENT_BYTES,
            terms: MAX_TERMS,
            auto_merge_mb: 0,
        }
    }

    /// This profile admitting documents of up to `bytes`, with no separate term
    /// bound: a token is at least one byte, so `bytes` of text cannot exceed it.
    /// Set `terms` afterwards to bound tokens below the byte limit.
    #[must_use]
    pub const fn with_document_bytes(mut self, bytes: usize) -> Self {
        self.document_bytes = bytes;
        self.terms = bytes;
        self
    }

    fn valid(&self) -> bool {
        self.documents > 0
            && self.documents <= i32::MAX as usize
            && self.operations > 0
            && self.history_bytes > 0
            && self.segments > 0
            && self.buffered_documents > 0
            && self.buffered_documents <= i32::MAX as usize
            && self.document_bytes > 0
            && self.document_bytes <= i32::MAX as usize
            && self.terms > 0
    }
}

/// Supported single-valued field kinds. All fields are required and stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Exact UTF-8 term, with sorted doc values.
    Keyword,
    /// Standard analyzer, no stopwords, 255-UTF-16-unit scanner buffer.
    Text,
    /// Signed 32-bit integer, without coercion.
    Integer,
    /// Exact boolean, indexed as lowercase `true` or `false`.
    Boolean,
    /// Caller-parsed UTC epoch milliseconds in the four-digit year domain.
    Date,
}

/// Typed input. Date parsing belongs to the caller; strings are never coerced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    /// UTF-8 keyword or analyzed text.
    String(&'a str),
    /// The same, held in a shared allocation the document keeps: an analyzed text field
    /// indexes and stores these bytes without copying them, so a very large text is held once.
    Shared(&'a Arc<String>),
    /// Integer, checked against the declared numeric domain.
    Integer(i64),
    /// Boolean.
    Boolean(bool),
    /// UTC epoch milliseconds, checked against years 0000 through 9999.
    Date(i64),
}

impl Value<'_> {
    /// The text of a string value, borrowed or shared.
    pub fn text(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            Value::Shared(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// An explicit versioned schema with unique bounded field names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub(crate) revision: u64,
    pub(crate) fields: Vec<(String, Kind)>,
    pub(crate) limits: Limits,
}

impl Schema {
    /// Validate the entire schema before allocating owned field names.
    pub fn new(revision: u64, fields: &[(&str, Kind)]) -> Result<Self> {
        if revision == 0 || fields.is_empty() || fields.len() + 2 > MAX_FIELDS {
            return Err(Error::Schema);
        }
        for (i, (name, _)) in fields.iter().enumerate() {
            if name.is_empty()
                || name.len() > 64
                || name.starts_with('_')
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || fields[..i].iter().any(|(previous, _)| previous == name)
            {
                return Err(Error::Schema);
            }
        }
        Ok(Self {
            revision,
            fields: fields.iter().map(|(n, k)| (n.to_string(), *k)).collect(),
            limits: Limits::BOUNDED,
        })
    }

    /// Replace the admission bounds. Part of schema identity: a checkpoint only
    /// reopens under the limits it was taken with.
    pub fn with_limits(mut self, limits: Limits) -> Result<Self> {
        if !limits.valid() {
            return Err(Error::Schema);
        }
        self.limits = limits;
        Ok(self)
    }

    /// This schema with automatic merging of segments up to `mb` MiB (0 turns it
    /// off). A runtime tuning, not part of what the data means; it is part of the
    /// schema's equality only because the limits are, so a checkpoint reopens under
    /// the same setting it was taken with.
    #[must_use]
    pub fn with_auto_merge_mb(mut self, mb: usize) -> Self {
        self.limits.auto_merge_mb = mb.min(MAX_AUTO_MERGE_MB);
        self
    }

    /// Declared fields in canonical order.
    pub fn fields(&self) -> impl Iterator<Item = (&str, Kind)> {
        self.fields.iter().map(|(n, k)| (n.as_str(), *k))
    }

    /// The admission bounds in force.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Return the exact declared field kind, refusing unknown fields.
    pub fn kind(&self, name: &str) -> Result<Kind> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, k)| *k)
            .ok_or(Error::Schema)
    }

    pub(crate) fn document(
        &self,
        id: &str,
        version: i64,
        fields: &[(&str, Value<'_>)],
    ) -> Result<Document> {
        if id.is_empty() || id.len() > 256 || version <= 0 || fields.len() != self.fields.len() {
            return Err(Error::Document);
        }
        let mut bytes = id.len();
        // Check bytes and types before allocating native fields or analyzing text.
        for (i, (name, value)) in fields.iter().enumerate() {
            if fields[..i].iter().any(|(other, _)| other == name) {
                return Err(Error::Document);
            }
            bytes = bytes
                .checked_add(name.len())
                .and_then(|n| {
                    n.checked_add(match value {
                        Value::String(s) => s.len(),
                        Value::Shared(s) => s.len(),
                        _ => 8,
                    })
                })
                .ok_or(Error::Limit)?;
            if bytes > self.limits.document_bytes {
                return Err(Error::Limit);
            }
            let kind = self.kind(name)?;
            validate(kind, *value)?;
            if let (Kind::Keyword, Some(s)) = (kind, value.text())
                && s.len() > MAX_KEYWORD_BYTES
            {
                return Err(Error::Limit);
            }
        }
        // A token is at least one byte, so text of at most `terms` bytes cannot
        // exceed the term bound and needs no analysis pass. Only longer text pays for
        // the exact count, which streams: no token is retained.
        let text_bytes: usize = fields
            .iter()
            .filter_map(|(name, value)| match (self.kind(name), value) {
                (Ok(Kind::Text), v) => v.text().map(str::len),
                _ => None,
            })
            .sum();
        let count_terms = text_bytes > self.limits.terms;
        let mut document = Document::new();
        document.add(KeywordField::new("_hs_id", id, Store::Yes));
        document.add(LongField::new("_hs_version", version, Store::Yes));
        let mut analyzed_terms = 0;
        for (name, value) in fields {
            match (self.kind(name)?, value) {
                (Kind::Keyword, Value::String(s)) => {
                    document.add(KeywordField::new(*name, *s, Store::Yes))
                }
                (Kind::Keyword, Value::Shared(s)) => {
                    document.add(KeywordField::new(*name, s.as_str(), Store::Yes))
                }
                (Kind::Text, Value::String(_) | Value::Shared(_)) => {
                    let text = value.text().ok_or(Error::Document)?;
                    if count_terms {
                        analyzed_terms += count_tokens(text)?;
                        if analyzed_terms > self.limits.terms {
                            return Err(Error::Limit);
                        }
                    }
                    // One copy of the text, shared by the analyzer and the stored value: a
                    // caller's `Arc` is used as it is, a borrowed `str` is copied once.
                    let shared = match value {
                        Value::Shared(s) => SharedText::from(Arc::clone(s)),
                        _ => SharedText::from(text),
                    };
                    document.add(TextField::new_shared(*name, shared, Store::Yes));
                }
                (Kind::Integer, Value::Integer(n)) | (Kind::Date, Value::Date(n)) => {
                    document.add(LongField::new(*name, *n, Store::Yes))
                }
                (Kind::Boolean, Value::Boolean(b)) => {
                    document.add(KeywordField::new(*name, b.to_string(), Store::Yes))
                }
                _ => return Err(Error::Document),
            }
        }
        Ok(document)
    }
}

/// Analyzed token count of `text`, streamed: the analyzer hands each token to a
/// counter, so memory does not grow with the text.
fn count_tokens(text: &str) -> Result<usize> {
    let mut count = 0usize;
    analyzer()
        .try_for_each_token(text, |_, _, _, _| count += 1)
        .map_err(|_| Error::Native)?;
    Ok(count)
}

pub(crate) fn validate(kind: Kind, value: Value<'_>) -> Result<()> {
    match (kind, value) {
        (Kind::Keyword | Kind::Text, Value::String(_) | Value::Shared(_))
        | (Kind::Boolean, Value::Boolean(_)) => Ok(()),
        (Kind::Integer, Value::Integer(n)) if i32::try_from(n).is_ok() => Ok(()),
        (Kind::Date, Value::Date(n))
            if (-62_167_219_200_000..=253_402_300_799_999).contains(&n) =>
        {
            Ok(())
        }
        _ => Err(Error::Document),
    }
}

pub(crate) fn analyzer() -> Analyzer {
    Analyzer::new(StandardAnalyzer::new())
}

/// A standard-analyzer token with zero-based position and UTF-16 offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Original normalized term.
    pub term: String,
    /// Zero-based analyzer position.
    pub position: i32,
    /// Inclusive UTF-16 start offset.
    pub start_offset: i32,
    /// Exclusive UTF-16 end offset.
    pub end_offset: i32,
}
/// Analyze within the same text byte/token bounds used for indexing and querying.
pub fn analyze_text(text: &str) -> Result<Vec<Token>> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::Limit);
    }
    let stream = analyzer()
        .try_analyze_stream(text)
        .map_err(|_| Error::Native)?;
    if stream.tokens.len() > MAX_TERMS {
        return Err(Error::Limit);
    }
    let mut position = -1_i32;
    stream
        .tokens
        .into_iter()
        .map(|t| {
            position = position
                .checked_add(t.position_increment)
                .ok_or(Error::Limit)?;
            Ok(Token {
                term: t.term,
                position,
                start_offset: t.start_offset,
                end_offset: t.end_offset,
            })
        })
        .collect()
}
