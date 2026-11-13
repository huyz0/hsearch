//! Complete finite aggregations over the same native query/snapshot.
use crate::{Error, Kind, MAX_FIELDS, Query, Result, Snapshot};
use lucene_search::aggs::{self, MetricSpec, NEED_ALL, Source, ValueKind};
use lucene_search::terms_agg;

/// Exact metric state for one numeric/date field.
#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    /// Number of selected values.
    pub count: u64,
    /// Sum, with zero for an empty selection.
    pub sum: f64,
    /// Minimum, absent for an empty selection.
    pub min: Option<f64>,
    /// Maximum, absent for an empty selection.
    pub max: Option<f64>,
}
/// An exact key-ordered finite bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bucket {
    /// Original UTF-8 indexed keyword.
    pub key: String,
    /// Complete selected document count.
    pub count: u64,
}
impl Snapshot {
    /// Compute sum/min/max/value_count for bounded numeric/date fields.
    pub fn metrics(&self, query: &Query<'_>, fields: &[&str]) -> Result<Vec<Metrics>> {
        if fields.is_empty() || fields.len() > MAX_FIELDS {
            return Err(Error::Limit);
        }
        let query = crate::query::lower(&self.schema, query)?;
        let specs = fields
            .iter()
            .map(|name| {
                if !matches!(self.schema.kind(name)?, Kind::Integer | Kind::Date) {
                    return Err(Error::Unsupported);
                }
                Ok(MetricSpec {
                    field: name.to_string(),
                    kind: ValueKind::Long,
                    source: Source::DocValues,
                    needs: NEED_ALL,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut opened = self.reader.open_segments().map_err(|_| Error::Native)?;
        opened.open_points().map_err(|_| Error::Native)?;
        let segments = opened.as_open_segments();
        aggs::metric_states(&segments, self.reader.segment_readers(), &query, &specs)
            .map_err(|_| Error::Native)?
            .into_iter()
            .map(|v| {
                if v.count == 0 {
                    return Ok(Metrics {
                        count: 0,
                        sum: 0.0,
                        min: None,
                        max: None,
                    });
                }
                if v.count > self.schema.limits.documents as u64
                    || ![v.sum, v.min, v.max].iter().all(|n| n.is_finite())
                {
                    return Err(Error::Limit);
                }
                Ok(Metrics {
                    count: v.count,
                    sum: v.sum,
                    min: Some(v.min),
                    max: Some(v.max),
                })
            })
            .collect()
    }
    /// Enumerate all keyword buckets before applying the caller's bucket bound.
    /// A physical document has exactly one keyword value, so the physical scope
    /// bounds native cardinality. Overflow refuses; no truncated exact success.
    pub fn groups(
        &self,
        query: &Query<'_>,
        field: &str,
        max_buckets: usize,
    ) -> Result<Vec<Bucket>> {
        if max_buckets == 0 || max_buckets > self.schema.limits.documents {
            return Err(Error::Limit);
        }
        if self.schema.kind(field)? != Kind::Keyword {
            return Err(Error::Unsupported);
        }
        let query = crate::query::lower(&self.schema, query)?;
        let mut opened = self.reader.open_segments().map_err(|_| Error::Native)?;
        opened.open_points().map_err(|_| Error::Native)?;
        let segments = opened.as_open_segments();
        let result = terms_agg::terms(
            &segments,
            self.reader.segment_readers(),
            &query,
            field,
            self.schema.limits.documents,
        )
        .map_err(|_| Error::Native)?;
        if result.other_doc_count != 0 || result.buckets.len() > max_buckets {
            return Err(Error::Limit);
        }
        let mut buckets = result
            .buckets
            .into_iter()
            .map(|(key, count)| {
                Ok(Bucket {
                    key: String::from_utf8(key).map_err(|_| Error::Native)?,
                    count,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        buckets.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(buckets)
    }
}
