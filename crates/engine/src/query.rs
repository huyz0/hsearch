//! Typed query lowering with a shared node, byte, depth and token budget.
use crate::schema::{KIND_FIELD, ROOT_KIND};
use crate::vector::KnnContext;
use crate::{Error, Kind, MAX_TERMS, Refusal, Result, Schema, Value};
use lucene_search::extended_query::ExtendedQuery;
use lucene_search::join::{LateJoinQuery, QueryBitSetProducer, ScoreMode, ToParentBlockJoinQuery};
use lucene_search::query::{
    BooleanQuery, Clause, MatchAllDocsQuery, MatchNoDocsQuery, PhraseQuery, PointsRangeQuery,
    TermQuery,
};

/// How the scores of the matching children (or objects) of one parent combine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinScore {
    /// The parent scores 0 (a filter): the children are not scored.
    None,
    /// The mean of the children's scores.
    Avg,
    /// The highest.
    Max,
    /// The sum.
    Sum,
    /// The lowest.
    Min,
}

impl JoinScore {
    fn native(self) -> ScoreMode {
        match self {
            Self::None => ScoreMode::None,
            Self::Avg => ScoreMode::Avg,
            Self::Max => ScoreMode::Max,
            Self::Sum => ScoreMode::Total,
            Self::Min => ScoreMode::Min,
        }
    }
}

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
    /// The `k` documents whose vector in `field` is nearest `vector`, optionally
    /// among only those matching `filter`. The filter applies while the graph
    /// is walked, so `k` hits come back whenever `k` documents match it. It
    /// composes in a bool like any clause, scored by similarity.
    Knn {
        /// Declared vector field.
        field: &'a str,
        /// Query vector: the field's dimension, every component finite.
        vector: &'a [f32],
        /// Hits to keep, at most [`crate::MAX_KNN_K`].
        k: usize,
        /// Candidates the graph walk keeps (`ef_search`), at least `k` and at
        /// most [`crate::MAX_KNN_CANDIDATES`]; a wider beam trades latency for
        /// recall.
        candidates: usize,
        /// Clusters probed per segment on an IVF field; 0 uses the field's
        /// own default (`VectorSpec::with_nprobe`) and, when that is 0 too,
        /// the share of its clusters the segment calibrated when it was written,
        /// for 0.95 recall@10 on queries like its own vectors (5% when it did
        /// not). Queries far from every cluster need a deeper `nprobe`.
        /// Ignored by HNSW fields.
        nprobe: usize,
        /// Documents the neighbours must also match.
        filter: Option<&'a Query<'a>>,
    },
    /// Root documents with at least one object of the nested field `path` that matches
    /// `query`. The inner query names its fields `path.field` and all of them must match
    /// inside the same object, never across two.
    Nested {
        /// A path declared by [`Schema::with_nested`].
        path: &'a str,
        /// Matched against each object, not the root.
        query: &'a Query<'a>,
        /// How the matching objects' scores combine into the root's.
        score_mode: JoinScore,
    },
    /// Parents with children of the relation `relation` that match `query`, within the
    /// shard: parent and children share one, by their routing key.
    HasChild {
        /// A child relation of the join field.
        relation: &'a str,
        /// Matched against the children, in terms of root fields.
        query: &'a Query<'a>,
        /// How the matching children's scores combine into the parent's.
        score_mode: JoinScore,
        /// Fewest matching children a parent needs.
        min_children: usize,
        /// Most matching children it may have; 0 is no limit.
        max_children: usize,
    },
    /// Children whose parent, of the parent type `relation`, matches `query`.
    HasParent {
        /// The parent type of the join field.
        relation: &'a str,
        /// Matched against the parent.
        query: &'a Query<'a>,
        /// Whether a child takes its parent's score (otherwise it scores 0).
        score: bool,
    },
    /// Explicit refusal for an unadvertised operator.
    Unsupported,
}

/// Most nested queries one query may run.
const MAX_NESTED_CLAUSES: usize = 4;
/// Most has_child / has_parent queries one query may run, each searching its from side
/// once before the query itself runs.
const MAX_JOIN_CLAUSES: usize = 2;

/// Most kNN searches one query may run, each a graph walk per segment.
const MAX_KNN_CLAUSES: usize = 4;

struct Budget {
    nodes: usize,
    bytes: usize,
    terms: usize,
    /// kNN clauses seen so far.
    knn: usize,
    /// Validate only: a kNN clause is checked but not run.
    dry: bool,
    /// Nested queries seen so far.
    nested: usize,
    /// Join queries seen so far.
    joins: usize,
    /// The nested path whose objects the clause being lowered is matched against.
    scope: Option<usize>,
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
    lower_with(schema, query, None)
}
/// Check a query against the schema and its budgets without running any kNN
/// search, so a refusable query is refused before any segment is opened.
pub(crate) fn validate(schema: &Schema, query: &Query<'_>) -> Result<()> {
    clause(
        schema,
        query,
        0,
        &mut Budget {
            nodes: 0,
            bytes: 0,
            terms: 0,
            knn: 0,
            dry: true,
            nested: 0,
            joins: 0,
            scope: None,
        },
        None,
    )
    .map(|_| ())
}
/// [`lower`] for a query that may carry a kNN search, which needs the segments
/// it runs over: each is resolved to its fixed hits here.
pub(crate) fn lower_with(
    schema: &Schema,
    query: &Query<'_>,
    knn: Option<&KnnContext<'_, '_>>,
) -> Result<BooleanQuery> {
    let root = if schema.has_nested() && may_match_object(query) {
        // Nested objects are documents of their own; a top-level query sees only roots.
        vec![Clause::Term(TermQuery::new(
            KIND_FIELD,
            ROOT_KIND.as_bytes(),
        ))]
    } else {
        Vec::new()
    };
    Ok(BooleanQuery {
        filter: root,
        must: vec![clause(
            schema,
            query,
            0,
            &mut Budget {
                nodes: 0,
                bytes: 0,
                terms: 0,
                knn: 0,
                dry: false,
                nested: 0,
                joins: 0,
                scope: None,
            },
            knn,
        )?],
        ..Default::default()
    })
}
/// The kind of `field` where the clause being lowered stands: inside a nested query only
/// that path's `path.field` names resolve, outside it only root fields do, and a nested
/// path itself is never a field to match on.
fn field_kind(schema: &Schema, budget: &Budget, field: &str) -> Result<Kind> {
    match budget.scope {
        Some(at) => match schema.nested_field(field) {
            Some((found, kind)) if found == at => Ok(kind),
            _ => Err(Error::Refused(Refusal::WrongScope)),
        },
        None => {
            if schema.nested_field(field).is_some() {
                return Err(Error::Refused(Refusal::WrongScope));
            }
            match schema.kind(field)? {
                Kind::Nested => Err(Error::Refused(Refusal::WrongScope)),
                kind => Ok(kind),
            }
        }
    }
}

/// Whether `query` could match a nested object, not only a root. Objects hold only their own
/// `path.field` values, the root's id and their kind, so a clause on a root field never
/// matches one, and a conjunction with such a clause cannot either. Everything else
/// (`All`, an id, a bool with nothing positive to restrict it) is held to roots by a filter.
fn may_match_object(query: &Query<'_>) -> bool {
    match query {
        Query::Term { .. }
        | Query::Terms { .. }
        | Query::Range { .. }
        | Query::Match { .. }
        | Query::Knn { .. }
        | Query::Nested { .. }
        | Query::HasChild { .. }
        | Query::HasParent { .. } => false,
        Query::Bool { must, filter, .. } => must.iter().chain(filter.iter()).all(may_match_object),
        Query::All | Query::Id(_) | Query::Unsupported => true,
    }
}

fn exact(schema: &Schema, field: &str, value: Value<'_>, budget: &mut Budget) -> Result<Clause> {
    let kind = field_kind(schema, budget, field)?;
    if kind == Kind::Join {
        // A term on the join field names a relation.
        let Value::String(relation) = value else {
            return Err(Error::Document);
        };
        budget.charge(0, field.len() + relation.len(), 1)?;
        return Ok(Clause::Term(TermQuery::new(field, relation.as_bytes())));
    }
    crate::schema::validate(kind, value)?;
    budget.charge(
        0,
        field.len()
            + match value {
                Value::String(s) => s.len(),
                Value::Shared(s) => s.len(),
                _ => 8,
            },
        1,
    )?;
    match value {
        Value::String(s) => Ok(Clause::Term(TermQuery::new(field, s.as_bytes()))),
        Value::Shared(s) => Ok(Clause::Term(TermQuery::new(field, s.as_bytes()))),
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
        Value::Vector(_) | Value::Nested(_) | Value::Join { .. } => Err(Error::Unsupported),
    }
}

/// A term on the kind field: the documents of one nested path, or the roots.
fn kind_term(kind: &str) -> Clause {
    Clause::Term(TermQuery::new(KIND_FIELD, kind.as_bytes()))
}

/// `inner` restricted to the children of one nested path.
fn within(inner: Clause, kind: &str) -> Clause {
    Clause::Boolean(Box::new(BooleanQuery {
        must: vec![inner],
        filter: vec![kind_term(kind)],
        ..Default::default()
    }))
}

fn join_spec(schema: &Schema) -> Result<&crate::schema::JoinSpec> {
    schema.join.as_ref().ok_or(Error::Refused(Refusal::NotJoin))
}
fn clause(
    schema: &Schema,
    query: &Query<'_>,
    depth: usize,
    budget: &mut Budget,
    knn: Option<&KnnContext<'_, '_>>,
) -> Result<Clause> {
    if depth > 8 {
        return Err(Error::Limit);
    }
    budget.charge(1, 0, 0)?;
    match query {
        Query::All => Ok(Clause::MatchAllDocs(MatchAllDocsQuery::new(
            schema.limits.documents as i32,
        ))),
        Query::Unsupported => Err(Error::Unsupported),
        Query::Knn {
            field,
            vector,
            k,
            candidates,
            nprobe,
            filter,
        } => {
            if budget.scope.is_some() {
                return Err(Error::Refused(Refusal::WrongScope));
            }
            let Kind::Vector(spec) = field_kind(schema, budget, field)? else {
                return Err(Error::Unsupported);
            };
            spec.check(vector)?;
            budget.charge(0, field.len(), 0)?;
            budget.knn += 1;
            if budget.knn > MAX_KNN_CLAUSES || *k == 0 || *k > crate::MAX_KNN_K {
                return Err(Error::Limit);
            }
            if *candidates > crate::MAX_KNN_CANDIDATES || *nprobe > crate::MAX_NPROBE {
                return Err(Error::Limit);
            }
            let filter = filter
                .map(|f| clause(schema, f, depth + 1, budget, knn))
                .transpose()?;
            if budget.dry {
                return Ok(Clause::MatchNoDocs(MatchNoDocsQuery::new()));
            }
            let context = knn.ok_or(Error::Unsupported)?;
            crate::vector::resolve(
                context,
                field,
                vector,
                (
                    *k,
                    *candidates,
                    if *nprobe == 0 {
                        spec.default_nprobe()
                    } else {
                        *nprobe
                    },
                ),
                filter.as_ref(),
            )
        }
        Query::Id(id) => {
            if budget.scope.is_some() {
                return Err(Error::Refused(Refusal::WrongScope));
            }
            if id.is_empty() || id.len() > 256 {
                return Err(Error::Document);
            }
            budget.charge(0, id.len() + "_hs_id".len(), 1)?;
            Ok(Clause::Term(TermQuery::new("_hs_id", id.as_bytes())))
        }
        Query::Term { field, value } => exact(schema, field, *value, budget),
        Query::Terms { field, values } => {
            field_kind(schema, budget, field)?;
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
            let kind = field_kind(schema, budget, field)?;
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
            if field_kind(schema, budget, field)? != Kind::Text {
                return Err(Error::Unsupported);
            }
            budget.charge(0, field.len() + text.len(), 0)?;
            let terms = crate::schema::analyze_text(text)?;
            budget.charge(0, 0, terms.len())?;
            if *phrase && !terms.is_empty() {
                phrase_clause(field, terms)
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
        Query::Nested {
            path,
            query,
            score_mode,
        } => {
            if budget.scope.is_some() {
                return Err(Error::Refused(Refusal::NestedTooDeep));
            }
            let at = schema
                .nested_index(path)
                .ok_or(Error::Refused(Refusal::NotNested))?;
            budget.nested += 1;
            if budget.nested > MAX_NESTED_CLAUSES {
                return Err(Error::Limit);
            }
            budget.charge(0, path.len(), 0)?;
            budget.scope = Some(at);
            let inner = clause(schema, query, depth + 1, budget, knn);
            budget.scope = None;
            let parents = QueryBitSetProducer::new(BooleanQuery {
                must: vec![kind_term(ROOT_KIND)],
                ..Default::default()
            });
            Ok(Clause::Extended(Box::new(
                ExtendedQuery::ToParentBlockJoin(ToParentBlockJoinQuery::new(
                    within(inner?, path),
                    std::sync::Arc::new(parents),
                    score_mode.native(),
                )),
            )))
        }
        Query::HasChild {
            relation,
            query,
            score_mode,
            min_children,
            max_children,
        } => {
            let join = join_spec(schema)?;
            if budget.scope.is_some() {
                return Err(Error::Refused(Refusal::WrongScope));
            }
            if !join.children.iter().any(|c| c == relation) {
                return Err(Error::Refused(Refusal::NotJoin));
            }
            budget.joins += 1;
            if budget.joins > MAX_JOIN_CLAUSES {
                return Err(Error::Limit);
            }
            budget.charge(0, relation.len(), 1)?;
            let from = clause(schema, query, depth + 1, budget, knn)?;
            let (min, max) = match (i32::try_from(*min_children), i32::try_from(*max_children)) {
                (Ok(min), Ok(max)) if max == 0 || min <= max => {
                    (min, if max == 0 { i32::MAX } else { max })
                }
                _ => return Err(Error::Limit),
            };
            Ok(Clause::Extended(Box::new(ExtendedQuery::LateJoin(
                LateJoinQuery {
                    join_field: join.id_field(),
                    from_query: Box::new(Clause::Boolean(Box::new(BooleanQuery {
                        must: vec![from],
                        filter: vec![Clause::Term(TermQuery::new(
                            &join.name,
                            relation.as_bytes(),
                        ))],
                        ..Default::default()
                    }))),
                    to_query: Box::new(Clause::Term(TermQuery::new(
                        &join.name,
                        join.parent.as_bytes(),
                    ))),
                    score_mode: score_mode.native(),
                    min,
                    max,
                },
            ))))
        }
        Query::HasParent {
            relation,
            query,
            score,
        } => {
            let join = join_spec(schema)?;
            if budget.scope.is_some() {
                return Err(Error::Refused(Refusal::WrongScope));
            }
            if *relation != join.parent {
                return Err(Error::Refused(Refusal::NotJoin));
            }
            budget.joins += 1;
            if budget.joins > MAX_JOIN_CLAUSES {
                return Err(Error::Limit);
            }
            budget.charge(0, relation.len(), 1)?;
            let from = clause(schema, query, depth + 1, budget, knn)?;
            Ok(Clause::Extended(Box::new(ExtendedQuery::LateJoin(
                LateJoinQuery {
                    join_field: join.id_field(),
                    from_query: Box::new(Clause::Boolean(Box::new(BooleanQuery {
                        must: vec![from],
                        filter: vec![Clause::Term(TermQuery::new(
                            &join.name,
                            relation.as_bytes(),
                        ))],
                        ..Default::default()
                    }))),
                    to_query: Box::new(Clause::Boolean(Box::new(BooleanQuery {
                        should: join
                            .children
                            .iter()
                            .map(|c| Clause::Term(TermQuery::new(&join.name, c.as_bytes())))
                            .collect(),
                        minimum_should_match: 1,
                        ..Default::default()
                    }))),
                    score_mode: if *score {
                        ScoreMode::Max
                    } else {
                        ScoreMode::None
                    },
                    min: 0,
                    max: i32::MAX,
                },
            ))))
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
                    .map(|q| clause(schema, q, depth + 1, budget, knn))
                    .collect::<Result<Vec<_>>>()
            };
            let mut must = convert(must)?;
            let filter = convert(filter)?;
            let should = convert(should)?;
            let must_not = convert(must_not)?;
            // A pure negative/empty bool starts from the admitted live universe.
            if must.is_empty() && filter.is_empty() && should.is_empty() {
                must.push(Clause::MatchAllDocs(MatchAllDocsQuery::new(
                    schema.limits.documents as i32,
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

// Keep absolute analyzer positions, including holes, when constructing a phrase.
fn phrase_clause(field: &str, terms: Vec<crate::Token>) -> Result<Clause> {
    Ok(Clause::Phrase(
        PhraseQuery::with_positions(
            field,
            terms.into_iter().map(|t| (t.term.into_bytes(), t.position)),
        )
        .map_err(|_| Error::Native)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Mutation, Snapshot, Token};
    use lucene_codecs::stored_fields::FieldValue;
    use lucene_search::index_searcher::IndexSearcher;
    use lucene_store::ByteBuffersDirectory;

    fn logical_id(snapshot: &Snapshot, mut doc: i32) -> String {
        for segment in snapshot.reader.segment_readers() {
            if doc < segment.max_doc {
                let number = segment
                    .field_infos()
                    .fields
                    .iter()
                    .find(|f| f.name == "_hs_id")
                    .unwrap()
                    .number;
                let document = segment.stored_document(doc).unwrap().unwrap();
                return match document
                    .fields
                    .into_iter()
                    .find(|f| f.field_number == number)
                    .unwrap()
                    .value
                {
                    FieldValue::String(id) => id,
                    _ => panic!("logical identity must be a stored string"),
                };
            }
            doc -= segment.max_doc;
        }
        panic!("search returned a document outside the snapshot");
    }

    #[test]
    fn phrase_lowering_preserves_explicit_position_holes() {
        let directory = ByteBuffersDirectory::new();
        let observed_directory = crate::EngineDirectory::new(&directory);
        let schema = Schema::new(1, &[("title", Kind::Text)]).unwrap();
        let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
        for (id, title) in [
            ("adjacent", "brown fox"),
            ("gap", "brown red fox"),
            ("reversed", "fox brown"),
        ] {
            engine
                .apply(
                    schema
                        .prepare(
                            id,
                            &[Mutation::Put {
                                id,
                                version: 1,
                                fields: &[("title", Value::String(title))],
                            }],
                        )
                        .unwrap(),
                )
                .unwrap();
        }
        let snapshot = engine.refresh().unwrap();
        {
            let opened = snapshot.reader.open_segments().unwrap();
            let segments = opened.as_open_segments();
            let norms = vec![None; segments.len()];
            let searcher = IndexSearcher::new(&segments, &norms).unwrap();
            // Independent positional inputs: no lexical boundary is assumed
            // to make a hole in the declared no-stopword analyzer.
            for (positions, expected) in [([0, 2], "gap"), ([0, 1], "adjacent")] {
                let terms = [("brown", positions[0], 0, 5), ("fox", positions[1], 6, 9)]
                    .into_iter()
                    .map(|(term, position, start_offset, end_offset)| Token {
                        term: term.to_string(),
                        position,
                        start_offset,
                        end_offset,
                    })
                    .collect();
                let query = BooleanQuery {
                    must: vec![phrase_clause("title", terms).unwrap()],
                    ..Default::default()
                };
                let result = searcher.search(&query, 3).unwrap();
                assert!(!searcher.timed_out());
                let ids: Vec<_> = result
                    .score_docs
                    .iter()
                    .map(|hit| logical_id(&snapshot, hit.doc))
                    .collect();
                assert_eq!(ids, vec![expected]);
            }
        }
        engine.close().unwrap();
    }
}
