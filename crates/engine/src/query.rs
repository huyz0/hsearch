//! Typed query lowering with a shared node, byte, depth and token budget.
use crate::{Error, Kind, MAX_DOCUMENTS, MAX_TERMS, Result, Schema, Value};
use lucene_search::query::{
    BooleanQuery, Clause, MatchAllDocsQuery, MatchNoDocsQuery, PhraseQuery, PointsRangeQuery,
    TermQuery,
};

/// Selected core-v1 query IR. No JSON DSL or unknown-option fallback.
pub enum Query<'a> {
    /// Exact caller identity, independent of user field names.
    Id(&'a str),
    /// Every live document.
    All,
    /// Exact indexed term; analyzed text terms remain case-sensitive.
    Term {
        /// Declared field.
        field: &'a str,
        /// Correctly typed exact value.
        value: Value<'a>,
    },
    /// OR of bounded exact terms.
    Terms {
        /// Declared field.
        field: &'a str,
        /// Exact values.
        values: &'a [Value<'a>],
    },
    /// Inclusive integer/date bounds. The caller converts exclusive bounds with
    /// checked arithmetic; wire date parsing is outside this adapter.
    Range {
        /// Declared integer/date field.
        field: &'a str,
        /// Inclusive lower endpoint.
        lower: Value<'a>,
        /// Inclusive upper endpoint.
        upper: Value<'a>,
    },
    /// Standard analyzed text with OR semantics, or zero-slop phrase semantics.
    Match {
        /// Declared text field.
        field: &'a str,
        /// Input text.
        text: &'a str,
        /// Require adjacent terms in original order.
        phrase: bool,
    },
    /// Integer-threshold boolean composition, with explicit scoring/filter roles.
    Bool {
        /// Scoring conjunction.
        must: &'a [Query<'a>],
        /// Non-scoring conjunction.
        filter: &'a [Query<'a>],
        /// Scoring disjunction.
        should: &'a [Query<'a>],
        /// Excluded documents.
        must_not: &'a [Query<'a>],
        /// Explicit integer threshold.
        minimum_should_match: usize,
    },
    /// Explicit refusal for an unadvertised operator.
    Unsupported,
}

struct Budget {
    nodes: usize,
    bytes: usize,
    terms: usize,
}
impl Budget {
    fn charge(&mut self, nodes: usize, bytes: usize, terms: usize) -> Result<()> {
        self.nodes = self.nodes.checked_add(nodes).ok_or(Error::Limit)?;
        self.bytes = self.bytes.checked_add(bytes).ok_or(Error::Limit)?;
        self.terms = self.terms.checked_add(terms).ok_or(Error::Limit)?;
        if self.nodes > 128 || self.bytes > 8192 || self.terms > MAX_TERMS {
            Err(Error::Limit)
        } else {
            Ok(())
        }
    }
}
pub(crate) fn lower(schema: &Schema, query: &Query<'_>) -> Result<BooleanQuery> {
    Ok(BooleanQuery {
        must: vec![clause(
            schema,
            query,
            0,
            &mut Budget {
                nodes: 0,
                bytes: 0,
                terms: 0,
            },
        )?],
        ..Default::default()
    })
}
fn exact(schema: &Schema, field: &str, value: Value<'_>, budget: &mut Budget) -> Result<Clause> {
    let kind = schema.kind(field)?;
    crate::schema::validate(kind, value)?;
    budget.charge(
        0,
        field.len()
            + match value {
                Value::String(s) => s.len(),
                _ => 8,
            },
        1,
    )?;
    match value {
        Value::String(s) => Ok(Clause::Term(TermQuery::new(field, s.as_bytes()))),
        Value::Boolean(b) => Ok(Clause::Term(TermQuery::new(
            field,
            if b {
                b"true".as_slice()
            } else {
                b"false".as_slice()
            },
        ))),
        Value::Integer(n) | Value::Date(n) => {
            Ok(Clause::PointsRange(PointsRangeQuery::new(field, n, n)))
        }
    }
}
fn clause(schema: &Schema, query: &Query<'_>, depth: usize, budget: &mut Budget) -> Result<Clause> {
    if depth > 8 {
        return Err(Error::Limit);
    }
    budget.charge(1, 0, 0)?;
    match query {
        Query::All => Ok(Clause::MatchAllDocs(MatchAllDocsQuery::new(
            MAX_DOCUMENTS as i32,
        ))),
        Query::Unsupported => Err(Error::Unsupported),
        Query::Id(id) => {
            if id.is_empty() || id.len() > 256 {
                return Err(Error::Document);
            }
            budget.charge(0, id.len() + "_hs_id".len(), 1)?;
            Ok(Clause::Term(TermQuery::new("_hs_id", id.as_bytes())))
        }
        Query::Term { field, value } => exact(schema, field, *value, budget),
        Query::Terms { field, values } => {
            schema.kind(field)?;
            if values.is_empty() {
                budget.charge(0, field.len(), 0)?;
            }
            if values.len() > MAX_TERMS {
                return Err(Error::Limit);
            }
            let should = values
                .iter()
                .map(|v| exact(schema, field, *v, budget))
                .collect::<Result<_>>()?;
            Ok(Clause::Boolean(Box::new(BooleanQuery {
                should,
                minimum_should_match: 1,
                ..Default::default()
            })))
        }
        Query::Range {
            field,
            lower,
            upper,
        } => {
            let kind = schema.kind(field)?;
            crate::schema::validate(kind, *lower)?;
            crate::schema::validate(kind, *upper)?;
            budget.charge(0, field.len() + 16, 0)?;
            let (lo, hi) = match (lower, upper) {
                (Value::Integer(lo), Value::Integer(hi)) | (Value::Date(lo), Value::Date(hi)) => {
                    (*lo, *hi)
                }
                _ => return Err(Error::Unsupported),
            };
            if lo > hi {
                return Ok(Clause::MatchNoDocs(MatchNoDocsQuery::new()));
            }
            Ok(Clause::PointsRange(PointsRangeQuery::new(*field, lo, hi)))
        }
        Query::Match {
            field,
            text,
            phrase,
        } => {
            if schema.kind(field)? != Kind::Text {
                return Err(Error::Unsupported);
            }
            budget.charge(0, field.len() + text.len(), 0)?;
            let terms = crate::schema::analyze_text(text)?;
            budget.charge(0, 0, terms.len())?;
            if *phrase && !terms.is_empty() {
                Ok(Clause::Phrase(
                    PhraseQuery::with_positions(
                        *field,
                        terms.into_iter().map(|t| (t.term.into_bytes(), t.position)),
                    )
                    .map_err(|_| Error::Native)?,
                ))
            } else {
                Ok(Clause::Boolean(Box::new(BooleanQuery {
                    should: terms
                        .into_iter()
                        .map(|t| Clause::Term(TermQuery::new(*field, t.term.into_bytes())))
                        .collect(),
                    minimum_should_match: 1,
                    ..Default::default()
                })))
            }
        }
        Query::Bool {
            must,
            filter,
            should,
            must_not,
            minimum_should_match,
        } => {
            let n = must
                .len()
                .checked_add(filter.len())
                .and_then(|n| n.checked_add(should.len()))
                .and_then(|n| n.checked_add(must_not.len()))
                .ok_or(Error::Limit)?;
            if n > 128 || *minimum_should_match > 128 {
                return Err(Error::Limit);
            }
            let mut convert = |qs: &[Query<'_>]| {
                qs.iter()
                    .map(|q| clause(schema, q, depth + 1, budget))
                    .collect::<Result<Vec<_>>>()
            };
            let mut must = convert(must)?;
            let filter = convert(filter)?;
            let should = convert(should)?;
            let must_not = convert(must_not)?;
            // A pure negative/empty bool starts from the admitted live universe.
            if must.is_empty() && filter.is_empty() && should.is_empty() {
                must.push(Clause::MatchAllDocs(MatchAllDocsQuery::new(
                    MAX_DOCUMENTS as i32,
                )));
            }
            Ok(Clause::Boolean(Box::new(BooleanQuery {
                must,
                filter,
                should,
                must_not,
                minimum_should_match: *minimum_should_match,
            })))
        }
    }
}
