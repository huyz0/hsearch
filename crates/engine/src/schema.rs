//! Bounded, explicit materialized-index schema. No wire parsing or coercion.
use lucene_analysis::{Analyzer, StandardAnalyzer};
use lucene_index::document::{Document, KeywordField, LongField, Store, TextField};

use crate::{Error, Result};

/// Maximum native fields, including two reserved identity/version fields.
pub const MAX_FIELDS: usize = 16;
/// Maximum total UTF-8 bytes in one document, including names and identity.
pub const MAX_DOCUMENT_BYTES: usize = 8192;
/// Maximum analyzer tokens per document or query.
pub const MAX_TERMS: usize = 128;
/// Maximum retained logical identities, including deleted identities.
pub const MAX_DOCUMENTS: usize = 128;
/// Maximum admitted mutation operations over one adapter lifetime/checkpoint.
pub const MAX_OPERATIONS: usize = 256;

/// Supported single-valued field kinds. All fields are required and stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Exact UTF-8 term, with sorted doc values.
    Keyword,
    /// Standard analyzer, no stopwords, 255-character token limit.
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
    /// Integer, checked against the declared numeric domain.
    Integer(i64),
    /// Boolean.
    Boolean(bool),
    /// UTC epoch milliseconds, checked against years 0000 through 9999.
    Date(i64),
}

/// An explicit versioned schema with unique bounded field names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub(crate) revision: u64,
    pub(crate) fields: Vec<(String, Kind)>,
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
        })
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
                        _ => 8,
                    })
                })
                .ok_or(Error::Limit)?;
            if bytes > MAX_DOCUMENT_BYTES {
                return Err(Error::Limit);
            }
            validate(self.kind(name)?, *value)?;
        }
        let mut document = Document::new();
        document.add(KeywordField::new("_hs_id", id, Store::Yes));
        document.add(LongField::new("_hs_version", version, Store::Yes));
        let mut analyzed_terms = 0;
        for (name, value) in fields {
            match (self.kind(name)?, value) {
                (Kind::Keyword, Value::String(s)) => {
                    document.add(KeywordField::new(*name, *s, Store::Yes))
                }
                (Kind::Text, Value::String(s)) => {
                    analyzed_terms += analyze_text(s)?.len();
                    if analyzed_terms > MAX_TERMS {
                        return Err(Error::Limit);
                    }
                    document.add(TextField::new(*name, *s, Store::Yes));
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

pub(crate) fn validate(kind: Kind, value: Value<'_>) -> Result<()> {
    match (kind, value) {
        (Kind::Keyword | Kind::Text, Value::String(_)) | (Kind::Boolean, Value::Boolean(_)) => {
            Ok(())
        }
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
