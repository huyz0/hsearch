//! Bounded, explicit materialized-index schema. No wire parsing or coercion.
use std::sync::Arc;

use lucene_analysis::{Analyzer, StandardAnalyzer};
use lucene_index::document::{
    Document, KeywordField, KnnFloatVectorField, LongField, SharedText, SortedDocValuesField,
    Store, StringField, TextField,
};

use crate::vector::{MAX_VECTOR_FIELDS, VectorSpec};
use crate::{Error, Refusal, Result};

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

/// Most nested paths a schema may declare.
pub const MAX_NESTED_PATHS: usize = 4;
/// Deepest nesting a path may have. Nested inside nested is refused, not flattened.
pub const MAX_NESTED_DEPTH: usize = 1;
/// Most nested objects one document may carry across all its nested paths. Each is a
/// native document of its own, so each also counts against [`Limits::documents`].
pub const MAX_NESTED_OBJECTS: usize = 64;
/// Most child relations one parent type may have in the join field.
pub const MAX_CHILD_RELATIONS: usize = 4;
/// The `_hs_kind` value of a root document, in a schema that has nested paths.
pub(crate) const ROOT_KIND: &str = "_root";
/// The indexed kind of a native document: [`ROOT_KIND`] or its nested path.
pub(crate) const KIND_FIELD: &str = "_hs_kind";

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
    /// A dense float vector, indexed as HNSW (plain or quantized) and searched
    /// by nearest neighbour. Indexed only: the vector is not a stored field, so
    /// a projection cannot return it.
    Vector(VectorSpec),
    /// An array of objects, each indexed as a native document of its own, ahead of
    /// its root, so a query matches inside one object and never across two. Declared
    /// by [`Schema::with_nested`], which gives the object's fields. Not stored on the
    /// root: a projection cannot return it.
    Nested,
    /// The parent-child relation of the index, declared by [`Schema::with_join`]: each
    /// document is a parent or one of its named child relations. Parent and children
    /// are separate documents that must live on one shard.
    Join,
}

/// Typed input. Date parsing belongs to the caller; strings are never coerced.
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// A vector of exactly the field's dimension, every component finite.
    Vector(&'a [f32]),
    /// The objects of a nested field, each listing the nested fields in declared order.
    Nested(&'a [&'a [(&'a str, Value<'a>)]]),
    /// A document's place in the join: the relation it plays, and for a child the id of
    /// its parent, which is also its routing key.
    Join {
        /// A parent type or child relation named by the join.
        relation: &'a str,
        /// Required for a child, refused for a parent.
        parent: Option<&'a str>,
    },
}

/// What a nested path holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NestedSpec {
    pub(crate) name: String,
    pub(crate) fields: Vec<(String, Kind)>,
}

/// The join field of an index: one parent type and its child relations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JoinSpec {
    pub(crate) name: String,
    pub(crate) parent: String,
    pub(crate) children: Vec<String>,
}

impl JoinSpec {
    /// The doc-values field holding the parent id of both a parent and its children.
    pub(crate) fn id_field(&self) -> String {
        format!("{}#{}", self.name, self.parent)
    }
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
    pub(crate) nested: Vec<NestedSpec>,
    pub(crate) join: Option<JoinSpec>,
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
        if fields
            .iter()
            .any(|(_, k)| matches!(k, Kind::Nested | Kind::Join))
        {
            // Declared only by `with_nested` and `with_join`, which give them a body.
            return Err(Error::Schema);
        }
        let vectors = fields.iter().filter_map(|(_, k)| match k {
            Kind::Vector(spec) => Some(spec),
            _ => None,
        });
        if vectors.clone().count() > MAX_VECTOR_FIELDS || vectors.into_iter().any(|s| !s.valid()) {
            return Err(Error::Schema);
        }
        Ok(Self {
            revision,
            fields: fields.iter().map(|(n, k)| (n.to_string(), *k)).collect(),
            limits: Limits::BOUNDED,
            nested: Vec::new(),
            join: None,
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

    /// Declare a nested path: an array of objects holding `fields`, each object indexed as
    /// a document of its own and queried with [`crate::Query::Nested`]. Fields are scalar
    /// (keyword, text, integer, boolean, date), at most [`MAX_FIELDS`] across all paths,
    /// and there are at most [`MAX_NESTED_PATHS`] paths. Nesting is one level deep
    /// ([`MAX_NESTED_DEPTH`]): an object cannot hold a nested path.
    pub fn with_nested(mut self, path: &str, fields: &[(&str, Kind)]) -> Result<Self> {
        let sub_total: usize = self.nested.iter().map(|n| n.fields.len()).sum();
        if !ident(path)
            || self.fields.iter().any(|(n, _)| n == path)
            || self.nested.len() >= MAX_NESTED_PATHS
            || fields.is_empty()
            || sub_total + fields.len() > MAX_FIELDS
            || self.fields.len() + 1 + 2 > MAX_FIELDS
        {
            return Err(Error::Schema);
        }
        for (i, (name, kind)) in fields.iter().enumerate() {
            if !ident(name)
                || fields[..i].iter().any(|(other, _)| other == name)
                || !matches!(
                    kind,
                    Kind::Keyword | Kind::Text | Kind::Integer | Kind::Boolean | Kind::Date
                )
            {
                return Err(Error::Schema);
            }
        }
        self.fields.push((path.to_string(), Kind::Nested));
        self.nested.push(NestedSpec {
            name: path.to_string(),
            fields: fields.iter().map(|(n, k)| (n.to_string(), *k)).collect(),
        });
        Ok(self)
    }

    /// Declare the join field: documents of the index are `parent` documents or one of the
    /// `children` relations, each child naming its parent's id. One join field per index,
    /// one parent type, at most [`MAX_CHILD_RELATIONS`] child relations. Queried with
    /// [`crate::Query::HasChild`] and [`crate::Query::HasParent`].
    pub fn with_join(mut self, name: &str, parent: &str, children: &[&str]) -> Result<Self> {
        if self.join.is_some()
            || !ident(name)
            || !ident(parent)
            || self.fields.iter().any(|(n, _)| n == name)
            || children.is_empty()
            || children.len() > MAX_CHILD_RELATIONS
            || self.fields.len() + 1 + 2 > MAX_FIELDS
            || children
                .iter()
                .enumerate()
                .any(|(i, c)| !ident(c) || *c == parent || children[..i].contains(c))
        {
            return Err(Error::Schema);
        }
        self.fields.push((name.to_string(), Kind::Join));
        self.join = Some(JoinSpec {
            name: name.to_string(),
            parent: parent.to_string(),
            children: children.iter().map(|c| c.to_string()).collect(),
        });
        Ok(self)
    }

    /// Whether any path is nested. Then every document carries its kind, and a top-level
    /// query sees only roots.
    pub fn has_nested(&self) -> bool {
        !self.nested.is_empty()
    }

    /// The nested paths, in declared order.
    pub fn nested_paths(&self) -> impl Iterator<Item = &str> {
        self.nested.iter().map(|n| n.name.as_str())
    }

    pub(crate) fn nested_index(&self, path: &str) -> Option<usize> {
        self.nested.iter().position(|n| n.name == path)
    }

    /// A nested field named `path.sub`: its path's position and its kind.
    pub(crate) fn nested_field(&self, full: &str) -> Option<(usize, Kind)> {
        let (path, sub) = full.split_once('.')?;
        let at = self.nested_index(path)?;
        let kind = self.nested[at]
            .fields
            .iter()
            .find(|(n, _)| n == sub)
            .map(|(_, k)| *k)?;
        Some((at, kind))
    }

    /// Every analyzed text field, nested ones as `path.sub`, for norms.
    pub(crate) fn text_fields(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .fields
            .iter()
            .filter(|(_, k)| *k == Kind::Text)
            .map(|(n, _)| n.clone())
            .collect();
        for spec in &self.nested {
            for (sub, kind) in &spec.fields {
                if *kind == Kind::Text {
                    names.push(format!("{}.{sub}", spec.name));
                }
            }
        }
        names
    }

    /// The key a router must hash to place this document: for a child, its parent's id, so
    /// parent and children land on one shard; for any other document, its own id. This is
    /// what makes a join local to a shard, so no query ever needs a join across shards.
    /// A child that names no parent has no routing key and is refused.
    pub fn route_key<'a>(&self, id: &'a str, fields: &[(&str, Value<'a>)]) -> Result<&'a str> {
        let Some(join) = &self.join else {
            return Ok(id);
        };
        let value = fields
            .iter()
            .find(|(n, _)| *n == join.name)
            .map(|(_, v)| *v)
            .ok_or(Error::Document)?;
        let Value::Join { relation, parent } = value else {
            return Err(Error::Document);
        };
        join.parent_id(id, relation, parent)
    }

    /// [`Schema::route_key`], and the key the caller actually routed by: a document placed by
    /// anything but its routing key would sit apart from its parent or children, which is a
    /// join across shards, refused rather than approximated.
    pub fn check_route<'a>(
        &self,
        id: &'a str,
        fields: &[(&str, Value<'a>)],
        routed_by: &str,
    ) -> Result<()> {
        if self.route_key(id, fields)? == routed_by {
            Ok(())
        } else {
            Err(Error::Refused(Refusal::CrossShardJoin))
        }
    }

    /// Native documents one logical document becomes: its nested objects, then itself.
    pub(crate) fn block(
        &self,
        id: &str,
        version: i64,
        fields: &[(&str, Value<'_>)],
    ) -> Result<Vec<Document>> {
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
                .and_then(|n| n.checked_add(value_bytes(value)))
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
            .map(|(name, value)| match (self.kind(name), value) {
                (Ok(Kind::Text), v) => v.text().map_or(0, str::len),
                (Ok(Kind::Nested), v) => text_len(v),
                _ => 0,
            })
            .sum();
        let mut counter = Counter {
            count: text_bytes > self.limits.terms,
            terms: 0,
        };
        let mut document = Document::new();
        document.add(KeywordField::new("_hs_id", id, Store::Yes));
        document.add(LongField::new("_hs_version", version, Store::Yes));
        if self.has_nested() {
            document.add(KeywordField::new(KIND_FIELD, ROOT_KIND, Store::Yes));
        }
        let mut block = Vec::new();
        let mut objects = 0usize;
        for (name, value) in fields {
            match (self.kind(name)?, value) {
                (Kind::Nested, Value::Nested(array)) => {
                    let at = self.nested_index(name).ok_or(Error::Schema)?;
                    objects = objects.checked_add(array.len()).ok_or(Error::Limit)?;
                    if objects > MAX_NESTED_OBJECTS || objects >= self.limits.documents {
                        return Err(Error::Limit);
                    }
                    for object in *array {
                        block.push(self.child(at, id, object, &mut counter)?);
                    }
                }
                (Kind::Join, Value::Join { relation, parent }) => {
                    let join = self.join.as_ref().ok_or(Error::Schema)?;
                    let parent_id = join.parent_id(id, relation, *parent)?;
                    document.add(KeywordField::new(*name, *relation, Store::Yes));
                    // As Lucene's join does: one term and one single-valued sorted doc value,
                    // the field the global-ordinals join reads.
                    let field = join.id_field();
                    document.add(StringField::new(field.as_str(), parent_id, Store::Yes));
                    document.add(SortedDocValuesField::new(field, parent_id));
                }
                (kind, value) => self.scalar(&mut document, name, kind, value, &mut counter)?,
            }
        }
        block.push(document);
        Ok(block)
    }

    /// One nested object as a native document, indexed ahead of its root.
    fn child(
        &self,
        at: usize,
        id: &str,
        object: &[(&str, Value<'_>)],
        counter: &mut Counter,
    ) -> Result<Document> {
        let spec = &self.nested[at];
        if object.len() != spec.fields.len() {
            return Err(Error::Document);
        }
        let mut document = Document::new();
        // The root's id, so replacing or deleting the root takes its objects with it.
        document.add(KeywordField::new("_hs_id", id, Store::No));
        document.add(KeywordField::new(KIND_FIELD, &spec.name, Store::Yes));
        for ((name, value), (declared, kind)) in object.iter().zip(&spec.fields) {
            if name != declared {
                return Err(Error::Document);
            }
            validate(*kind, *value)?;
            if let (Kind::Keyword, Some(s)) = (kind, value.text())
                && s.len() > MAX_KEYWORD_BYTES
            {
                return Err(Error::Limit);
            }
            let full = format!("{}.{name}", spec.name);
            self.scalar(&mut document, &full, *kind, value, counter)?;
        }
        Ok(document)
    }

    fn scalar(
        &self,
        document: &mut Document,
        name: &str,
        kind: Kind,
        value: &Value<'_>,
        counter: &mut Counter,
    ) -> Result<()> {
        match (kind, value) {
            (Kind::Keyword, Value::String(s)) => {
                document.add(KeywordField::new(name, *s, Store::Yes))
            }
            (Kind::Keyword, Value::Shared(s)) => {
                document.add(KeywordField::new(name, s.as_str(), Store::Yes))
            }
            (Kind::Text, Value::String(_) | Value::Shared(_)) => {
                let text = value.text().ok_or(Error::Document)?;
                if counter.count {
                    counter.terms += count_tokens(text)?;
                    if counter.terms > self.limits.terms {
                        return Err(Error::Limit);
                    }
                }
                // One copy of the text, shared by the analyzer and the stored value: a
                // caller's `Arc` is used as it is, a borrowed `str` is copied once.
                let shared = match value {
                    Value::Shared(s) => SharedText::from(Arc::clone(s)),
                    _ => SharedText::from(text),
                };
                document.add(TextField::new_shared(name, shared, Store::Yes));
            }
            (Kind::Integer, Value::Integer(n)) | (Kind::Date, Value::Date(n)) => {
                document.add(LongField::new(name, *n, Store::Yes))
            }
            (Kind::Vector(spec), Value::Vector(v)) => document.add(
                KnnFloatVectorField::new(name, v.to_vec(), spec.native_similarity())
                    .map_err(|_| Error::Document)?,
            ),
            (Kind::Boolean, Value::Boolean(b)) => {
                document.add(KeywordField::new(name, b.to_string(), Store::Yes))
            }
            _ => return Err(Error::Document),
        }
        Ok(())
    }
}

/// The analyzed-token budget of one document, spent across its root and nested objects.
struct Counter {
    /// Whether an exact count is needed at all: only text over the term bound pays for it.
    count: bool,
    terms: usize,
}

impl JoinSpec {
    /// The parent id a document with this join value is stored under, or why it has none.
    fn parent_id<'a>(
        &self,
        id: &'a str,
        relation: &str,
        parent: Option<&'a str>,
    ) -> Result<&'a str> {
        if relation == self.parent {
            return match parent {
                None => Ok(id),
                Some(_) => Err(Error::Refused(Refusal::ParentNamesParent)),
            };
        }
        if !self.children.iter().any(|c| c == relation) {
            return Err(Error::Refused(Refusal::UnknownRelation));
        }
        match parent {
            Some(p) if !p.is_empty() && p.len() <= 256 => Ok(p),
            _ => Err(Error::Refused(Refusal::ChildWithoutParent)),
        }
    }
}

fn ident(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Bytes a value adds to its document's size: text and keys as they are, a vector at four a
/// component, everything else eight, and a nested value the names and values of its objects.
pub(crate) fn value_bytes(value: &Value<'_>) -> usize {
    match value {
        Value::String(s) => s.len(),
        Value::Shared(s) => s.len(),
        Value::Vector(v) => v.len().saturating_mul(4),
        Value::Nested(objects) => objects
            .iter()
            .flat_map(|o| o.iter())
            .fold(0usize, |n, (name, v)| {
                n.saturating_add(name.len()).saturating_add(value_bytes(v))
            }),
        Value::Join { relation, parent } => relation.len() + parent.map_or(0, str::len),
        _ => 8,
    }
}

fn text_len(value: &Value<'_>) -> usize {
    match value {
        Value::Nested(objects) => objects
            .iter()
            .flat_map(|o| o.iter())
            .map(|(_, v)| v.text().map_or(0, str::len))
            .sum(),
        v => v.text().map_or(0, str::len),
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
        // The objects and the relation are checked against their declared bodies when the
        // document is built; here only the shape is.
        (Kind::Nested, Value::Nested(_)) | (Kind::Join, Value::Join { .. }) => Ok(()),
        (Kind::Keyword | Kind::Text, Value::String(_) | Value::Shared(_))
        | (Kind::Boolean, Value::Boolean(_)) => Ok(()),
        (Kind::Vector(spec), Value::Vector(v)) => spec.check(v),
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
