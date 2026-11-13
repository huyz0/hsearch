//! `_search` bodies: parse into an owned tree, then lower to the engine's typed
//! query IR. Anything outside the supported subset is refused by name.
use hsearch_engine::{Kind, Query, Sort, Value as Typed};
use hsearch_index::Field;
use serde_json::Value;
use typed_arena::Arena;

use crate::ApiError;
use crate::mapping::{Mapping, typed};

/// Largest `from + size`, the result window.
pub const MAX_WINDOW: usize = 1000;
const MAX_DEPTH: usize = 8;
const DATE_MAX: i64 = 253_402_300_799_999;

#[derive(Debug)]
pub(crate) enum Q {
    All,
    Term {
        field: String,
        value: Field,
    },
    Terms {
        field: String,
        values: Vec<Field>,
    },
    Range {
        field: String,
        kind: Kind,
        low: i64,
        high: i64,
    },
    Match {
        field: String,
        text: String,
        phrase: bool,
    },
    Bool {
        must: Vec<Q>,
        filter: Vec<Q>,
        should: Vec<Q>,
        must_not: Vec<Q>,
        minimum_should_match: usize,
    },
}

/// Which stored fields a hit returns.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Source {
    All,
    None,
    Some(Vec<String>),
}

#[derive(Debug)]
pub(crate) struct Search {
    pub(crate) query: Q,
    pub(crate) from: usize,
    pub(crate) size: usize,
    pub(crate) sort: Vec<(Option<String>, bool)>,
    pub(crate) source: Source,
}

fn unsupported(what: &str) -> ApiError {
    ApiError::parse(format!("unsupported {what}"))
}

fn single<'v>(value: &'v Value, what: &str) -> Result<(&'v String, &'v Value), ApiError> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::parse(format!("[{what}] must be an object")))?;
    let mut entries = object.iter();
    match (entries.next(), entries.next()) {
        (Some(entry), None) => Ok(entry),
        _ => Err(ApiError::parse(format!(
            "[{what}] must name exactly one field"
        ))),
    }
}

fn kind_of(mapping: &Mapping, field: &str) -> Result<Kind, ApiError> {
    mapping.kind(field).ok_or_else(|| {
        ApiError::new(
            400,
            "query_shard_exception",
            format!("unknown field [{field}]"),
        )
    })
}

fn value_for(mapping: &Mapping, field: &str, value: &Value) -> Result<Field, ApiError> {
    let kind = kind_of(mapping, field)?;
    typed(kind, value).map_err(|why| {
        ApiError::new(
            400,
            "query_shard_exception",
            format!("field [{field}]: {why}"),
        )
    })
}

fn clauses(mapping: &Mapping, value: Option<&Value>, depth: usize) -> Result<Vec<Q>, ApiError> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(|v| query(mapping, v, depth + 1)).collect(),
        Some(single) => Ok(vec![query(mapping, single, depth + 1)?]),
    }
}

fn bound(
    kind: Kind,
    value: &Value,
    exclusive_low: bool,
    exclusive_high: bool,
    low: bool,
) -> Result<i64, ApiError> {
    let field =
        typed(kind, value).map_err(|why| ApiError::new(400, "query_shard_exception", why))?;
    let n = match field {
        Field::Int(n) | Field::Date(n) => n,
        _ => return Err(unsupported("range on this field type")),
    };
    let exclusive = if low { exclusive_low } else { exclusive_high };
    match (exclusive, low) {
        (false, _) => Ok(n),
        (true, true) => n
            .checked_add(1)
            .ok_or_else(|| ApiError::bad("range bound overflow")),
        (true, false) => n
            .checked_sub(1)
            .ok_or_else(|| ApiError::bad("range bound overflow")),
    }
}

pub(crate) fn query(mapping: &Mapping, value: &Value, depth: usize) -> Result<Q, ApiError> {
    if depth > MAX_DEPTH {
        return Err(ApiError::new(
            400,
            "limit_exceeded_exception",
            "query nesting too deep",
        ));
    }
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::parse("a query must be an object"))?;
    let mut entries = object.iter();
    let (name, body) = match (entries.next(), entries.next()) {
        (Some(entry), None) => entry,
        _ => return Err(ApiError::parse("a query must name exactly one clause")),
    };
    match name.as_str() {
        "match_all" => {
            if body.as_object().is_none_or(|o| !o.is_empty()) {
                return Err(unsupported("match_all parameter"));
            }
            Ok(Q::All)
        }
        "term" => {
            let (field, spec) = single(body, "term")?;
            let spec = match spec {
                Value::Object(o) => {
                    if o.keys().any(|k| k != "value") {
                        return Err(unsupported("term parameter"));
                    }
                    o.get("value")
                        .ok_or_else(|| ApiError::parse("[term] needs a value"))?
                }
                other => other,
            };
            Ok(Q::Term {
                field: field.clone(),
                value: value_for(mapping, field, spec)?,
            })
        }
        "terms" => {
            let (field, spec) = single(body, "terms")?;
            let list = spec
                .as_array()
                .ok_or_else(|| ApiError::parse("[terms] needs an array"))?;
            let values = list
                .iter()
                .map(|v| value_for(mapping, field, v))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Q::Terms {
                field: field.clone(),
                values,
            })
        }
        "range" => {
            let (field, spec) = single(body, "range")?;
            let kind = kind_of(mapping, field)?;
            let spec = spec
                .as_object()
                .ok_or_else(|| ApiError::parse("[range] needs bounds"))?;
            if let Some(extra) = spec
                .keys()
                .find(|k| !["gte", "gt", "lte", "lt"].contains(&k.as_str()))
            {
                return Err(unsupported(&format!("range parameter [{extra}]")));
            }
            if spec.contains_key("gte") && spec.contains_key("gt")
                || spec.contains_key("lte") && spec.contains_key("lt")
            {
                return Err(ApiError::parse("a range bound is given once"));
            }
            let (min, max) = match kind {
                Kind::Integer => (i64::from(i32::MIN), i64::from(i32::MAX)),
                Kind::Date => (0, DATE_MAX),
                _ => return Err(unsupported("range on this field type")),
            };
            let low = match spec.get("gte").or_else(|| spec.get("gt")) {
                Some(v) => bound(kind, v, spec.contains_key("gt"), false, true)?,
                None => min,
            };
            let high = match spec.get("lte").or_else(|| spec.get("lt")) {
                Some(v) => bound(kind, v, false, spec.contains_key("lt"), false)?,
                None => max,
            };
            Ok(Q::Range {
                field: field.clone(),
                kind,
                low,
                high,
            })
        }
        "match" | "match_phrase" => {
            let (field, spec) = single(body, name)?;
            let text = match spec {
                Value::String(s) => s,
                Value::Object(o) => {
                    if o.keys().any(|k| k != "query") {
                        return Err(unsupported(&format!("{name} parameter")));
                    }
                    o.get("query")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ApiError::parse("[query] must be a string"))?
                }
                _ => return Err(ApiError::parse("[match] needs text")),
            };
            kind_of(mapping, field)?;
            Ok(Q::Match {
                field: field.clone(),
                text: text.to_string(),
                phrase: name == "match_phrase",
            })
        }
        "bool" => {
            let o = body
                .as_object()
                .ok_or_else(|| ApiError::parse("[bool] must be an object"))?;
            if let Some(extra) = o.keys().find(|k| {
                ![
                    "must",
                    "filter",
                    "should",
                    "must_not",
                    "minimum_should_match",
                ]
                .contains(&k.as_str())
            }) {
                return Err(unsupported(&format!("bool parameter [{extra}]")));
            }
            let must = clauses(mapping, o.get("must"), depth)?;
            let filter = clauses(mapping, o.get("filter"), depth)?;
            let should = clauses(mapping, o.get("should"), depth)?;
            let must_not = clauses(mapping, o.get("must_not"), depth)?;
            let minimum_should_match = match o.get("minimum_should_match") {
                Some(v) => v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| unsupported("minimum_should_match (integers only)"))?,
                None => usize::from(must.is_empty() && filter.is_empty() && !should.is_empty()),
            };
            Ok(Q::Bool {
                must,
                filter,
                should,
                must_not,
                minimum_should_match,
            })
        }
        other => Err(unsupported(&format!("query [{other}]"))),
    }
}

/// Parse a whole search body.
pub(crate) fn parse(mapping: &Mapping, body: &Value) -> Result<Search, ApiError> {
    let object = body
        .as_object()
        .ok_or_else(|| ApiError::parse("the body must be a JSON object"))?;
    if let Some(extra) = object
        .keys()
        .find(|k| !["query", "from", "size", "sort", "_source"].contains(&k.as_str()))
    {
        return Err(unsupported(&format!("search parameter [{extra}]")));
    }
    let number = |key: &str, default: usize| -> Result<usize, ApiError> {
        match object.get(key) {
            None => Ok(default),
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| ApiError::bad(format!("[{key}] must be a non-negative integer"))),
        }
    };
    let (from, size) = (number("from", 0)?, number("size", 10)?);
    if from.checked_add(size).is_none_or(|w| w > MAX_WINDOW) {
        return Err(ApiError::bad(format!(
            "result window is too large: from + size must be at most {MAX_WINDOW}"
        )));
    }
    let query = match object.get("query") {
        Some(q) => query(mapping, q, 0)?,
        None => Q::All,
    };
    let mut sort = Vec::new();
    match object.get("sort") {
        None => {}
        Some(Value::Array(items)) => {
            if items.len() > 4 {
                return Err(ApiError::new(
                    400,
                    "limit_exceeded_exception",
                    "at most 4 sort keys",
                ));
            }
            for item in items {
                sort.push(sort_key(mapping, item)?);
            }
        }
        Some(single) => sort.push(sort_key(mapping, single)?),
    }
    let source = match object.get("_source") {
        None | Some(Value::Bool(true)) => Source::All,
        Some(Value::Bool(false)) => Source::None,
        Some(Value::Array(names)) => {
            let mut out = Vec::new();
            for name in names {
                let name = name
                    .as_str()
                    .ok_or_else(|| ApiError::bad("_source names must be strings"))?;
                kind_of(mapping, name)?;
                if out.iter().any(|n: &String| n == name) {
                    return Err(ApiError::bad("duplicate _source field"));
                }
                out.push(name.to_string());
            }
            Source::Some(out)
        }
        Some(_) => return Err(unsupported("_source form")),
    };
    Ok(Search {
        query,
        from,
        size,
        sort,
        source,
    })
}

fn sort_key(mapping: &Mapping, item: &Value) -> Result<(Option<String>, bool), ApiError> {
    let (name, order) = match item {
        Value::String(name) => (name.as_str(), None),
        Value::Object(_) => {
            let (name, spec) = single(item, "sort")?;
            let order = match spec {
                Value::String(o) => Some(o.as_str()),
                Value::Object(o) => {
                    if o.keys().any(|k| k != "order") {
                        return Err(unsupported("sort parameter"));
                    }
                    o.get("order").and_then(Value::as_str)
                }
                _ => return Err(unsupported("sort form")),
            };
            (name.as_str(), order)
        }
        _ => return Err(unsupported("sort form")),
    };
    let descending = match order {
        None => name == "_score",
        Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(unsupported("sort order")),
    };
    if name == "_score" {
        return Ok((None, descending));
    }
    kind_of(mapping, name)?;
    Ok((Some(name.to_string()), descending))
}

fn engine_value(field: &Field) -> Typed<'_> {
    match field {
        Field::Str(s) => Typed::String(s),
        Field::Int(n) => Typed::Integer(*n),
        Field::Bool(b) => Typed::Boolean(*b),
        Field::Date(n) => Typed::Date(*n),
    }
}

/// Lower to the engine IR, with slices owned by the two arenas.
pub(crate) fn lower<'a>(
    q: &'a Q,
    queries: &'a Arena<Query<'a>>,
    values: &'a Arena<Typed<'a>>,
) -> Query<'a> {
    match q {
        Q::All => Query::All,
        Q::Term { field, value } => Query::Term {
            field,
            value: engine_value(value),
        },
        Q::Terms {
            field,
            values: list,
        } => Query::Terms {
            field,
            values: &*values.alloc_extend(list.iter().map(engine_value)),
        },
        Q::Range {
            field,
            kind,
            low,
            high,
        } => {
            let make = |n: i64| {
                if *kind == Kind::Date {
                    Typed::Date(n)
                } else {
                    Typed::Integer(n)
                }
            };
            Query::Range {
                field,
                lower: make(*low),
                upper: make(*high),
            }
        }
        Q::Match {
            field,
            text,
            phrase,
        } => Query::Match {
            field,
            text,
            phrase: *phrase,
        },
        Q::Bool {
            must,
            filter,
            should,
            must_not,
            minimum_should_match,
        } => {
            let group = |list: &'a [Q]| -> &'a [Query<'a>] {
                &*queries.alloc_extend(list.iter().map(|c| lower(c, queries, values)))
            };
            Query::Bool {
                must: group(must),
                filter: group(filter),
                should: group(should),
                must_not: group(must_not),
                minimum_should_match: *minimum_should_match,
            }
        }
    }
}

/// The engine sort list for a parsed search. No sort means best score first.
pub(crate) fn engine_sort(sort: &[(Option<String>, bool)]) -> Vec<Sort<'_>> {
    if sort.is_empty() {
        return vec![Sort::Score];
    }
    sort.iter()
        .map(|(name, descending)| match name {
            Some(name) => Sort::Field {
                name,
                descending: *descending,
            },
            None => Sort::Score,
        })
        .collect()
}
