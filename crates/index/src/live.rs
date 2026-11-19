//! The owner's real-time view (M21.05): what a read sees beyond the searchable
//! snapshot. A write is durable (and acknowledged) before it is applied and
//! before a refresh makes it searchable, so a read that only used the snapshot
//! could return older data than an acknowledged write. The writer therefore
//! records every operation of a durable slot here, before it acknowledges, and
//! clears the record at the same instant it swaps in a snapshot that covers it.
//!
//! Memory is bounded: at most [`MAX_ENTRIES`] ids and about [`MAX_BYTES`] of
//! field data. Past either bound the writer refreshes (which empties the record);
//! if the refresh cannot be taken, the payloads are dropped and the ids kept as
//! [`Entry::Stale`], so the bound holds and a read of one of them asks for the
//! refresh first. Fields are shared (`Arc`), so recording an operation copies no
//! text and a read clones none under the lock.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use hsearch_engine::{Hit, Kind, Schema, StoredValue};

use crate::format::Field;
use crate::{Error, Result};

/// Most ids recorded between refreshes.
pub(crate) const MAX_ENTRIES: usize = 16_384;
/// Ids past which the writer refuses new work until a refresh can be taken.
pub(crate) const HARD_ENTRIES: usize = 4 * MAX_ENTRIES;
/// Most bytes of field data recorded between refreshes.
pub(crate) const MAX_BYTES: usize = 8 << 20;

/// What a read finds for an id the snapshot may not yet reflect.
#[derive(Clone)]
pub(crate) enum Entry {
    /// The latest acknowledged put.
    Doc { version: i64, fields: Vec<Field> },
    /// The latest acknowledged operation was a delete.
    Gone,
    /// Changed since the last refresh, payload dropped to hold the memory bound.
    Stale,
}

/// The record shared by the writer and the readers of one index.
pub(crate) struct Recent {
    map: HashMap<String, Entry>,
    bytes: usize,
    /// The schema the recorded fields are positional in: it only ever grows by
    /// appending, so it names every recorded position.
    schema: Schema,
    poisoned: bool,
}

pub(crate) type Overlay = Arc<Mutex<Recent>>;

pub(crate) fn overlay(schema: Schema) -> Overlay {
    Arc::new(Mutex::new(Recent {
        map: HashMap::new(),
        bytes: 0,
        schema,
        poisoned: false,
    }))
}

pub(crate) fn lock(overlay: &Overlay) -> Result<MutexGuard<'_, Recent>> {
    overlay.lock().map_err(|_| Error::Poisoned)
}

/// Bytes a recorded document holds.
fn weight(id: &str, fields: &[Field]) -> usize {
    64 + id.len()
        + fields
            .iter()
            .map(|f| match f {
                Field::Str(s) => s.len(),
                Field::Vector(v) => v.len() * 4,
                _ => 8,
            })
            .sum::<usize>()
}

impl Recent {
    /// Record `entry` as the latest state of `id`.
    pub(crate) fn record(&mut self, id: &str, entry: Entry) {
        let add = match &entry {
            Entry::Doc { fields, .. } => weight(id, fields),
            _ => 64 + id.len(),
        };
        if let Some(old) = self.map.insert(id.to_string(), entry) {
            self.bytes -= Self::held(id, &old);
        }
        self.bytes += add;
    }

    fn held(id: &str, entry: &Entry) -> usize {
        match entry {
            Entry::Doc { fields, .. } => weight(id, fields),
            _ => 64 + id.len(),
        }
    }

    /// Whether the record has outgrown its bounds.
    pub(crate) fn over(&self) -> bool {
        self.map.len() > MAX_ENTRIES || self.bytes > MAX_BYTES
    }

    /// Drop every payload, keeping the ids as stale, when a refresh cannot be
    /// taken: the bytes bound holds, and a read of such an id refreshes first. The
    /// writer refuses new work while the ids alone are past [`HARD_ENTRIES`].
    pub(crate) fn shed(&mut self) {
        for entry in self.map.values_mut() {
            if matches!(entry, Entry::Doc { .. }) {
                *entry = Entry::Stale;
            }
        }
        self.bytes = self.map.iter().map(|(id, e)| Self::held(id, e)).sum();
    }

    /// Empty the record: the snapshot now covers everything in it.
    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn set_schema(&mut self, schema: Schema) {
        self.schema = schema;
    }

    pub(crate) fn poison(&mut self) {
        self.poisoned = true;
    }

    pub(crate) fn poisoned(&self) -> bool {
        self.poisoned
    }

    pub(crate) fn get(&self, id: &str) -> Option<Entry> {
        self.map.get(id).cloned()
    }

    /// Where each projected name sits in a recorded document, `None` for a name
    /// that is not a stored field of the schema.
    pub(crate) fn positions(&self, projection: &[&str]) -> Vec<Option<usize>> {
        let fields: Vec<(&str, Kind)> = self.schema.fields().collect();
        projection
            .iter()
            .map(|name| {
                fields
                    .iter()
                    .position(|(n, k)| n == name && !matches!(k, Kind::Vector(_)))
            })
            .collect()
    }
}

/// A recorded document as a hit: the projected fields it carries, in projection
/// order, an absent one omitted exactly as the snapshot omits it.
pub(crate) fn hit(
    id: &str,
    version: i64,
    fields: &[Field],
    projection: &[&str],
    positions: &[Option<usize>],
) -> Hit {
    let mut out = Vec::with_capacity(projection.len());
    for (name, at) in projection.iter().zip(positions) {
        let Some(field) = at.and_then(|i| fields.get(i)) else {
            continue;
        };
        let value = match field {
            Field::Str(s) => StoredValue::String(String::clone(s)),
            Field::Int(n) => StoredValue::Integer(*n),
            Field::Bool(b) => StoredValue::Boolean(*b),
            Field::Date(n) => StoredValue::Date(*n),
            Field::Vector(_) | Field::Absent => continue,
        };
        out.push(((*name).to_string(), value));
    }
    Hit {
        id: id.to_string(),
        version,
        fields: out,
    }
}
