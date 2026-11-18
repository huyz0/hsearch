//! Frozen, bounded native readers. Search never mutates a writer.
use crate::{Error, Kind, MAX_FIELDS, Query, Result, Schema};
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
// This owning wrapper keeps the teardown observer inside the same field as
// the native reader, so a reservation-first regression cannot precede it unseen.
pub(crate) struct NativeReader {
    #[cfg(test)]
    _before_drop: Option<tests::BeforeReaderDrop>,
    reader: DirectoryReader,
}
impl std::ops::Deref for NativeReader {
    type Target = DirectoryReader;
    fn deref(&self) -> &DirectoryReader {
        &self.reader
    }
}

/// A frozen native NRT or committed reader, independent of later writer changes.
pub struct Snapshot {
    pub(crate) reader: NativeReader,
    pub(crate) schema: Schema,
    // Fields drop in declaration order. Native buffers and NRT holds must be
    // destroyed before another refresh can observe this reservation refunded.
    _reservation: Reservation,
}
impl Snapshot {
    pub(crate) fn new(
        reader: DirectoryReader,
        schema: Schema,
        reservation: Reservation,
    ) -> Result<Self> {
        // Establish the destruction order before validation can return an
        // error; function-parameter drop order must not refund the slot early.
        let snapshot = Self {
            reader: NativeReader {
                #[cfg(test)]
                _before_drop: None,
                reader,
            },
            schema,
            _reservation: reservation,
        };
        let limits = snapshot.schema.limits;
        if !(0..=limits.documents as i32).contains(&snapshot.reader.max_doc())
            || snapshot.reader.segment_readers().len() > limits.segments
        {
            return Err(Error::Limit);
        }
        Ok(snapshot)
    }
    /// Open the last native commit in `directory` read-only, with no writer, no
    /// lock and no retry state. This is how a search role serves committed
    /// segments, including over a remote-backed directory. Only a native commit
    /// is visible; nothing is verified against hsearch retry history.
    pub fn open_committed<D: lucene_store::Directory>(
        directory: &crate::EngineDirectory<D>,
        schema: Schema,
    ) -> Result<Self> {
        let reader = DirectoryReader::open(&directory.observed).map_err(|_| Error::Native)?;
        let counter = Arc::new(AtomicUsize::new(0));
        Self::new(reader, schema, Reservation::new(&counter)?)
    }
    /// Read one logical identity from this frozen snapshot. This is not an
    /// authoritative latest GET or a distributed freshness guarantee.
    pub fn get(&self, id: &str, projection: &[&str]) -> Result<Option<Hit>> {
        let mut result = self.search(&Query::Id(id), &[], 1, projection)?;
        if result.total > 1 {
            return Err(Error::Checkpoint);
        }
        Ok(result.hits.pop())
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
        if size > self.schema.limits.documents || sort.len() > 4 || projection.len() > MAX_FIELDS {
            return Err(Error::Limit);
        }
        let factory = crate::EngineFactory::new();
        factory.require(crate::capability_of_query(query))?;
        for s in sort {
            factory.require(crate::capability_of_sort(s))?;
        }
        // Refuse what can be refused before any segment is opened.
        crate::query::validate(&self.schema, query)?;
        for (i, name) in projection.iter().enumerate() {
            if matches!(
                self.schema.kind(name)?,
                Kind::Vector(_) | Kind::Nested | Kind::Join
            ) {
                return Err(Error::Unsupported);
            }
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
                        Kind::Text | Kind::Vector(_) | Kind::Nested | Kind::Join => {
                            return Err(Error::Unsupported);
                        }
                    };
                    SortField::numeric(name, kind, *descending)
                }
            });
        }
        sorts.push(SortField::numeric("_hs_id", SortType::String, false));
        let mut opened = self.reader.open_segments().map_err(|_| Error::Native)?;
        opened.open_points().map_err(|_| Error::Native)?;
        let segments = opened.as_open_segments();
        // A kNN search runs over these segments, so the query is lowered only now.
        let query = crate::query::lower_with(
            &self.schema,
            query,
            Some(&crate::vector::KnnContext {
                segments: &segments,
            }),
        )?;
        let text_fields = self.schema.text_fields();
        let maps = self
            .reader
            .field_norms_by_field(&text_fields)
            .map_err(|_| Error::Native)?;
        let norms: Vec<_> = maps.iter().map(Some).collect();
        let searcher = IndexSearcher::new(&segments, &norms).map_err(|_| Error::Native)?;
        // A has_child or has_parent searches its from side here, once, over this snapshot.
        let query = if lucene_search::join::has_late_joins(&query) {
            lucene_search::join::resolve_late_joins(&query, &searcher, &|field| {
                lucene_search::join::ordinal_map(&searcher, field).map(Some)
            })
            .map_err(|_| Error::Native)?
            .unwrap_or(query)
        } else {
            query
        };
        let result = searcher
            .search_sorted(
                self.reader.segment_readers(),
                &query,
                size.max(1),
                &sorts,
                None,
            )
            .map_err(|_| Error::Native)?;
        // Past the collector's counting threshold the value is only a lower
        // bound, so the exact count comes from a counting pass.
        let total = if result.total.relation == lucene_search::collector::TotalHitsRelation::EqualTo
        {
            result.total.value
        } else {
            searcher.count(&query).map_err(|_| Error::Native)?
        };
        if searcher.timed_out() || total > self.schema.limits.documents as u64 {
            return Err(Error::Limit);
        }
        let hits = result
            .hits
            .iter()
            .take(size)
            .map(|h| self.hit(h.doc, projection))
            .collect::<Result<_>>()?;
        Ok(SearchResult { hits, total })
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
/// A field value read back from the index while a checkpoint is verified.
enum Recovered {
    Text(String),
    Long(i64),
    Floats(Vec<f32>),
}
/// Compare a reopened index against what the caller expects of it.
pub(crate) fn verify(
    reader: &DirectoryReader,
    schema: &Schema,
    expected: &BTreeMap<String, crate::adapter::Live>,
) -> Result<()> {
    let live = expected
        .values()
        .filter(|live| live.digest.is_some())
        .count();
    let seen = scan(reader, schema, |id, version, digest, _| {
        let op = expected.get(id).ok_or(Error::Checkpoint)?;
        if version != op.version || op.digest != Some(digest) {
            return Err(Error::Checkpoint);
        }
        Ok(())
    })?;
    if seen != live {
        return Err(Error::Checkpoint);
    }
    Ok(())
}

/// What a reopened index says of its live documents: identity, version and the digest of the
/// stored values, with the native documents each root holds. Tombstones are not in the index.
pub(crate) fn rebuild(
    reader: &DirectoryReader,
    schema: &Schema,
) -> Result<BTreeMap<String, crate::adapter::Live>> {
    let mut found = BTreeMap::new();
    scan(reader, schema, |id, version, digest, docs| {
        found.insert(
            id.to_string(),
            crate::adapter::Live {
                version,
                digest: Some(digest),
                docs,
            },
        );
        Ok(())
    })?;
    Ok(found)
}

/// Visit every live root document once with `(id, version, digest, native documents)`, refusing
/// a duplicate identity. Returns how many were visited.
fn scan(
    reader: &DirectoryReader,
    schema: &Schema,
    mut visit: impl FnMut(&str, i64, u128, usize) -> Result<()>,
) -> Result<usize> {
    // Use the same bounded production reader and preserve duplicate detection.
    if reader.max_doc() > schema.limits.documents as i32
        || reader.segment_readers().len() > schema.limits.segments
    {
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
        .search(&query, schema.limits.documents)
        .map_err(|_| Error::Native)?;
    // Each vector field's segments are opened once, not once per document.
    let mut vectors = Vec::new();
    for (name, kind) in &schema.fields {
        if let Kind::Vector(spec) = kind {
            vectors.push((
                name.as_str(),
                crate::vector::StoredVectors::open(reader, name, usize::from(spec.dimension))?,
            ));
        }
    }
    // Nested objects precede their root in doc order, so one pass pairs each root with them.
    let mut objects: std::collections::HashMap<i32, Vec<(String, i32)>> = Default::default();
    if schema.has_nested() {
        let mut pending = Vec::new();
        for doc in 0..reader.max_doc() {
            let FieldValue::String(kind) = stored(reader, doc, crate::schema::KIND_FIELD)? else {
                return Err(Error::Checkpoint);
            };
            if kind == crate::schema::ROOT_KIND {
                objects.insert(doc, std::mem::take(&mut pending));
            } else {
                pending.push((kind, doc));
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for hit in result.score_docs {
        let id = match stored(reader, hit.doc, "_hs_id")? {
            FieldValue::String(s) => s,
            _ => return Err(Error::Checkpoint),
        };
        if !seen.insert(id.clone()) {
            return Err(Error::Checkpoint);
        }
        let FieldValue::Long(version) = stored(reader, hit.doc, "_hs_version")? else {
            return Err(Error::Checkpoint);
        };
        let mut native_docs = 1usize;
        let mut values = Vec::with_capacity(fields.len());
        let scalar = |doc: i32, name: &str, kind: Kind| -> Result<Recovered> {
            match (kind, stored(reader, doc, name)?) {
                (Kind::Keyword | Kind::Text | Kind::Boolean, FieldValue::String(s)) => {
                    Ok(Recovered::Text(s))
                }
                (Kind::Integer | Kind::Date, FieldValue::Long(n)) => Ok(Recovered::Long(n)),
                _ => Err(Error::Checkpoint),
            }
        };
        for (name, kind) in schema.fields.iter().map(|(n, k)| (n.as_str(), *k)) {
            match kind {
                Kind::Vector(_) => values.push(Recovered::Floats(
                    vectors
                        .iter()
                        .find(|(n, _)| *n == name)
                        .ok_or(Error::Checkpoint)?
                        .1
                        .get(hit.doc)?,
                )),
                Kind::Nested => {
                    let at = schema.nested_index(name).ok_or(Error::Checkpoint)?;
                    let mine: Vec<i32> = objects
                        .get(&hit.doc)
                        .map(|all| {
                            all.iter()
                                .filter(|(path, _)| path == name)
                                .map(|(_, doc)| *doc)
                                .collect()
                        })
                        .unwrap_or_default();
                    native_docs += mine.len();
                    values.push(Recovered::Long(mine.len() as i64));
                    for doc in mine {
                        for (sub, sub_kind) in &schema.nested[at].fields {
                            values.push(scalar(doc, &format!("{name}.{sub}"), *sub_kind)?);
                        }
                    }
                }
                Kind::Join => {
                    let join = schema.join.as_ref().ok_or(Error::Checkpoint)?;
                    values.push(scalar(hit.doc, name, Kind::Keyword)?);
                    values.push(scalar(hit.doc, &join.id_field(), Kind::Keyword)?);
                }
                _ => values.push(scalar(hit.doc, name, kind)?),
            }
        }
        let got = crate::adapter::digest(values.iter().map(|v| match v {
            Recovered::Text(s) => crate::adapter::Stored::Text(s),
            Recovered::Long(n) => crate::adapter::Stored::Long(*n),
            Recovered::Floats(v) => crate::adapter::Stored::Floats(v),
        }));
        visit(&id, version, got, native_docs)?;
    }
    Ok(seen.len())
}

#[cfg(test)]
mod tests {
    //! Pause an actual snapshot before native reader teardown, then compete for its slot.
    use super::*;
    use crate::{Engine, Mutation, Value};
    use lucene_store::ByteBuffersDirectory;
    use std::sync::{Mutex, atomic::AtomicBool, mpsc};
    use std::time::Duration;

    pub(super) struct BeforeReaderDrop {
        entered: mpsc::SyncSender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        observation_failed: Arc<AtomicBool>,
    }
    impl Drop for BeforeReaderDrop {
        fn drop(&mut self) {
            // This test-only field precedes the actual DirectoryReader. No native
            // buffer or NRT hold has been destroyed when the observer is notified.
            let observed = self.entered.send(()).is_ok()
                && self
                    .resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .is_ok();
            self.observation_failed.store(!observed, Ordering::SeqCst);
        }
    }

    #[test]
    fn reader_teardown_keeps_its_slot_until_native_resources_are_gone() {
        for reopen in [false, true] {
            let directory = ByteBuffersDirectory::new();
            let observed_directory = crate::EngineDirectory::new(&directory);
            let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
            let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
            engine
                .apply(
                    schema
                        .prepare(
                            "initial",
                            &[Mutation::Put {
                                id: "id",
                                version: 1,
                                fields: &[("key", Value::String("retained"))],
                            }],
                        )
                        .unwrap(),
                )
                .unwrap();
            let checkpoint = engine.commit().unwrap();
            let mut retiring = engine.refresh().unwrap();
            let retained = [
                engine.refresh().unwrap(),
                engine.refresh().unwrap(),
                engine.refresh().unwrap(),
            ];
            let engine = if reopen {
                engine.close().unwrap();
                Engine::open(&observed_directory, schema, Some(checkpoint)).unwrap()
            } else {
                engine
            };
            let (entered_tx, entered_rx) = mpsc::sync_channel(1);
            let (resume_tx, resume_rx) = mpsc::sync_channel(1);
            let observation_failed = Arc::new(AtomicBool::new(false));
            retiring.reader._before_drop = Some(BeforeReaderDrop {
                entered: entered_tx,
                resume: Mutex::new(resume_rx),
                observation_failed: Arc::clone(&observation_failed),
            });
            std::thread::scope(|scope| {
                let dropping = scope.spawn(move || drop(retiring));
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                // Four real native readers still exist. The old reservation-first
                // field order permits refresh here, before native teardown starts.
                let competing = engine.refresh();
                resume_tx.send(()).unwrap();
                dropping.join().unwrap();
                assert!(matches!(competing, Err(Error::Limit)));
            });
            assert!(!observation_failed.load(Ordering::SeqCst));
            let replacement = engine.refresh().unwrap();
            assert!(matches!(engine.refresh(), Err(Error::Limit)));
            for snapshot in retained.iter().chain(std::iter::once(&replacement)) {
                let hit = snapshot.get("id", &["key"]).unwrap().unwrap();
                assert_eq!(hit.version, 1);
                assert_eq!(
                    hit.fields,
                    vec![(
                        "key".to_string(),
                        StoredValue::String("retained".to_string())
                    )]
                );
            }
            engine.close().unwrap();
        }
    }
}
