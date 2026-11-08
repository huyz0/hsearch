//! Frozen, bounded native readers. Search never mutates a writer.
use crate::{Error, Kind, MAX_DOCUMENTS, MAX_FIELDS, Query, Result, Schema};
use lucene_codecs::stored_fields::FieldValue;
use lucene_search::directory_reader::DirectoryReader;
use lucene_search::index_searcher::IndexSearcher;
use lucene_search::top_field::{SortField, SortType};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(crate) struct Reservation(Arc<AtomicUsize>);
impl Reservation {
    pub(crate) fn new(counter: &Arc<AtomicUsize>) -> Result<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| Error::Limit)?;
        Ok(Self(Arc::clone(counter)))
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Materialized stored value, preserving its schema type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredValue {
    /// UTF-8 keyword/text.
    String(String),
    /// Checked int32 represented without loss.
    Integer(i64),
    /// Boolean.
    Boolean(bool),
    /// UTC epoch milliseconds.
    Date(i64),
}
/// Sort key. Logical identity is always appended as a deterministic tie break.
pub enum Sort<'a> {
    /// Local default BM25, highest first. Scores are topology dependent.
    Score,
    /// A declared single-valued keyword, numeric, boolean or date field.
    Field {
        /// Declared name.
        name: &'a str,
        /// Reverse field order.
        descending: bool,
    },
}
/// A logical hit with explicitly selected source fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Stable caller identity.
    pub id: String,
    /// Caller-resolved logical version.
    pub version: i64,
    /// Projection in requested order, without regenerated values.
    pub fields: Vec<(String, StoredValue)>,
}
/// Complete exact results for the admitted materialized scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    /// Returned bounded hits.
    pub hits: Vec<Hit>,
    /// Exact count. The physical scope cap is below the native total-hit threshold.
    pub total: u64,
}
/// A frozen native NRT or committed reader, independent of later writer changes.
pub struct Snapshot {
    _reservation: Reservation,
    pub(crate) reader: DirectoryReader,
    pub(crate) schema: Schema,
}
impl Snapshot {
    pub(crate) fn new(
        reader: DirectoryReader,
        schema: Schema,
        reservation: Reservation,
    ) -> Result<Self> {
        if !(0..=MAX_DOCUMENTS as i32).contains(&reader.max_doc())
            || reader.segment_readers().len() > 32
        {
            return Err(Error::Limit);
        }
        Ok(Self {
            reader,
            schema,
            _reservation: reservation,
        })
    }
    /// Search the complete bounded native scope, with explicit source projection.
    /// Size zero returns no hits and still computes the exact admitted count.
    pub fn search(
        &self,
        query: &Query<'_>,
        sort: &[Sort<'_>],
        size: usize,
        projection: &[&str],
    ) -> Result<SearchResult> {
        if size > MAX_DOCUMENTS || sort.len() > 4 || projection.len() > MAX_FIELDS {
            return Err(Error::Limit);
        }
        let query = crate::query::lower(&self.schema, query)?;
        for (i, name) in projection.iter().enumerate() {
            self.schema.kind(name)?;
            if projection[..i].contains(name) {
                return Err(Error::Document);
            }
        }
        let mut sorts = Vec::with_capacity(sort.len() + 1);
        for s in sort {
            sorts.push(match s {
                Sort::Score => SortField::score(),
                Sort::Field { name, descending } => {
                    let kind = match self.schema.kind(name)? {
                        Kind::Keyword | Kind::Boolean => SortType::String,
                        Kind::Integer | Kind::Date => SortType::Long,
                        Kind::Text => return Err(Error::Unsupported),
                    };
                    SortField::numeric(name, kind, *descending)
                }
            });
        }
        sorts.push(SortField::numeric("_hs_id", SortType::String, false));
        let mut opened = self.reader.open_segments().map_err(|_| Error::Native)?;
        opened.open_points().map_err(|_| Error::Native)?;
        let segments = opened.as_open_segments();
        let text_fields: Vec<_> = self
            .schema
            .fields
            .iter()
            .filter(|(_, k)| *k == Kind::Text)
            .map(|(n, _)| n.clone())
            .collect();
        let maps = self
            .reader
            .field_norms_by_field(&text_fields)
            .map_err(|_| Error::Native)?;
        let norms: Vec<_> = maps.iter().map(Some).collect();
        let searcher = IndexSearcher::new(&segments, &norms).map_err(|_| Error::Native)?;
        let result = searcher
            .search_sorted(
                self.reader.segment_readers(),
                &query,
                size.max(1),
                &sorts,
                None,
            )
            .map_err(|_| Error::Native)?;
        if searcher.timed_out() || result.total.value > MAX_DOCUMENTS as u64 {
            return Err(Error::Limit);
        }
        let hits = result
            .hits
            .iter()
            .take(size)
            .map(|h| self.hit(h.doc, projection))
            .collect::<Result<_>>()?;
        Ok(SearchResult {
            hits,
            total: result.total.value,
        })
    }
    fn hit(&self, doc: i32, projection: &[&str]) -> Result<Hit> {
        let id = match stored(&self.reader, doc, "_hs_id")? {
            FieldValue::String(s) => s,
            _ => return Err(Error::Checkpoint),
        };
        let version = match stored(&self.reader, doc, "_hs_version")? {
            FieldValue::Long(n) => n,
            _ => return Err(Error::Checkpoint),
        };
        let fields = projection
            .iter()
            .map(|name| {
                let kind = self.schema.kind(name)?;
                let value = match (kind, stored(&self.reader, doc, name)?) {
                    (Kind::Keyword | Kind::Text, FieldValue::String(s)) => StoredValue::String(s),
                    (Kind::Integer, FieldValue::Long(n)) if i32::try_from(n).is_ok() => {
                        StoredValue::Integer(n)
                    }
                    (Kind::Date, FieldValue::Long(n)) => StoredValue::Date(n),
                    (Kind::Boolean, FieldValue::String(s)) if s == "true" || s == "false" => {
                        StoredValue::Boolean(s == "true")
                    }
                    _ => return Err(Error::Checkpoint),
                };
                Ok((name.to_string(), value))
            })
            .collect::<Result<_>>()?;
        Ok(Hit {
            id,
            version,
            fields,
        })
    }
}
fn stored(reader: &DirectoryReader, mut doc: i32, name: &str) -> Result<FieldValue> {
    if doc < 0 {
        return Err(Error::Native);
    }
    for segment in reader.segment_readers() {
        if doc < segment.max_doc {
            let number = segment
                .field_infos()
                .fields
                .iter()
                .find(|f| f.name == name)
                .ok_or(Error::Checkpoint)?
                .number;
            let document = segment
                .stored_document(doc)
                .map_err(|_| Error::Native)?
                .ok_or(Error::Checkpoint)?;
            return document
                .fields
                .into_iter()
                .find(|f| f.field_number == number)
                .map(|f| f.value)
                .ok_or(Error::Checkpoint);
        }
        doc -= segment.max_doc;
    }
    Err(Error::Native)
}
pub(crate) fn verify(
    reader: &DirectoryReader,
    schema: &Schema,
    expected: &BTreeMap<String, crate::adapter::Operation>,
) -> Result<()> {
    // Use the same bounded production reader and preserve duplicate detection.
    if reader.max_doc() > MAX_DOCUMENTS as i32 || reader.segment_readers().len() > 32 {
        return Err(Error::Limit);
    }
    let fields: Vec<_> = schema.fields.iter().map(|(n, _)| n.as_str()).collect();
    // DirectoryReader owns buffers but is not assumed Clone. Verify by borrowed
    // search primitives, then compare each original typed payload and version.
    let mut opened = reader.open_segments().map_err(|_| Error::Native)?;
    opened.open_points().map_err(|_| Error::Native)?;
    let segments = opened.as_open_segments();
    let norms = vec![None; segments.len()];
    let query = crate::query::lower(schema, &Query::All)?;
    let result = IndexSearcher::new(&segments, &norms)
        .map_err(|_| Error::Native)?
        .search(&query, MAX_DOCUMENTS)
        .map_err(|_| Error::Native)?;
    let mut seen = std::collections::BTreeSet::new();
    if result.score_docs.len() != expected.values().filter(|op| op.fields.is_some()).count() {
        return Err(Error::Checkpoint);
    }
    for hit in result.score_docs {
        let id = match stored(reader, hit.doc, "_hs_id")? {
            FieldValue::String(s) => s,
            _ => return Err(Error::Checkpoint),
        };
        if !seen.insert(id.clone()) {
            return Err(Error::Checkpoint);
        }
        let op = expected.get(&id).ok_or(Error::Checkpoint)?;
        if !matches!(stored(reader,hit.doc,"_hs_version")?,FieldValue::Long(n) if n==op.version) {
            return Err(Error::Checkpoint);
        }
        let original = op.fields.as_ref().ok_or(Error::Checkpoint)?;
        for (name, (_, value)) in fields.iter().zip(original) {
            let stored = stored(reader, hit.doc, name)?;
            let same = match (value, stored) {
                (crate::adapter::OwnedValue::String(a), FieldValue::String(b)) => *a == b,
                (crate::adapter::OwnedValue::Boolean(a), FieldValue::String(b)) => {
                    a.to_string() == b
                }
                (
                    crate::adapter::OwnedValue::Integer(a) | crate::adapter::OwnedValue::Date(a),
                    FieldValue::Long(b),
                ) => *a == b,
                _ => false,
            };
            if !same {
                return Err(Error::Checkpoint);
            }
        }
    }
    Ok(())
}
