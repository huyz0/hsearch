//! The `knn_vector` mapping type: parsing its OpenSearch-shaped parameters and
//! the one-line stored spelling that round-trips them.
use hsearch_engine::{Kind, Quantizer, ScalarBits, Similarity, VectorSpec};
use serde_json::{Map, Value};

use crate::ApiError;

fn bad(name: &str, why: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        400,
        "mapper_parsing_exception",
        format!("field [{name}]: {why}"),
    )
}

const SPACES: [(&str, Similarity); 4] = [
    ("l2", Similarity::L2),
    ("cosinesimil", Similarity::Cosine),
    ("dot_product", Similarity::DotProduct),
    ("innerproduct", Similarity::MaxInnerProduct),
];

const QUANTIZERS: [(&str, Quantizer); 15] = [
    ("none", Quantizer::None),
    ("scalar8", Quantizer::Scalar(ScalarBits::Eight)),
    ("scalar7", Quantizer::Scalar(ScalarBits::Seven)),
    ("scalar4", Quantizer::Scalar(ScalarBits::Four)),
    ("scalar2", Quantizer::Scalar(ScalarBits::Two)),
    ("scalar1", Quantizer::Scalar(ScalarBits::One)),
    ("bbq", Quantizer::Bbq),
    ("rabitq", Quantizer::RaBitQ),
    ("erq2", Quantizer::Erq(2)),
    ("erq3", Quantizer::Erq(3)),
    ("erq4", Quantizer::Erq(4)),
    ("erq5", Quantizer::Erq(5)),
    ("erq6", Quantizer::Erq(6)),
    ("erq7", Quantizer::Erq(7)),
    ("erq8", Quantizer::Erq(8)),
];

fn number(name: &str, what: &str, value: &Value) -> Result<u16, ApiError> {
    value
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| bad(name, format!("[{what}] must be a whole number")))
}

fn only(
    name: &str,
    object: &Map<String, Value>,
    allowed: &[&str],
    what: &str,
) -> Result<(), ApiError> {
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(extra) => Err(bad(name, format!("unsupported {what} parameter [{extra}]"))),
        None => Ok(()),
    }
}

/// A `knn_vector` definition: `dimension` (required), `space_type` (default
/// `l2`), `method` (`hnsw`, with `m` and `ef_construction`) and `quantizer`.
pub(crate) fn parse(name: &str, definition: &Map<String, Value>) -> Result<Kind, ApiError> {
    only(
        name,
        definition,
        &["type", "dimension", "space_type", "method", "quantizer"],
        "knn_vector",
    )?;
    let dimension = definition
        .get("dimension")
        .ok_or_else(|| bad(name, "[dimension] is required"))
        .and_then(|v| number(name, "dimension", v))?;
    let mut space = definition.get("space_type");
    let (mut m, mut efc) = (16, 100);
    if let Some(method) = definition.get("method") {
        let method = method
            .as_object()
            .ok_or_else(|| bad(name, "[method] must be an object"))?;
        only(
            name,
            method,
            &["name", "engine", "space_type", "parameters"],
            "method",
        )?;
        match method.get("name").and_then(Value::as_str) {
            Some("hnsw") => {}
            _ => {
                return Err(bad(
                    name,
                    "[method.name] must be [hnsw]; ivf is not supported yet",
                ));
            }
        }
        if let Some(engine) = method.get("engine")
            && engine.as_str() != Some("lucene")
        {
            return Err(bad(name, "[method.engine] must be [lucene]"));
        }
        if let Some(inner) = method.get("space_type") {
            if space.is_some() {
                return Err(bad(name, "[space_type] is given twice"));
            }
            space = Some(inner);
        }
        if let Some(parameters) = method.get("parameters") {
            let parameters = parameters
                .as_object()
                .ok_or_else(|| bad(name, "[method.parameters] must be an object"))?;
            only(name, parameters, &["m", "ef_construction"], "method")?;
            if let Some(v) = parameters.get("m") {
                m = number(name, "m", v)?;
            }
            if let Some(v) = parameters.get("ef_construction") {
                efc = number(name, "ef_construction", v)?;
            }
        }
    }
    let similarity = match space.map(Value::as_str) {
        None => Similarity::L2,
        Some(Some(s)) => SPACES
            .iter()
            .find(|(n, _)| *n == s)
            .map(|(_, v)| *v)
            .ok_or_else(|| bad(name, format!("unsupported space_type [{s}]")))?,
        Some(None) => return Err(bad(name, "[space_type] must be a string")),
    };
    let quantizer = match definition.get("quantizer").map(Value::as_str) {
        None => Quantizer::None,
        Some(Some(s)) => QUANTIZERS
            .iter()
            .find(|(n, _)| *n == s)
            .map(|(_, v)| *v)
            .ok_or_else(|| bad(name, format!("unsupported quantizer [{s}]")))?,
        Some(None) => return Err(bad(name, "[quantizer] must be a string")),
    };
    let spec = VectorSpec::hnsw(dimension, similarity)
        .with_graph(m, efc)
        .with_quantizer(quantizer);
    // The engine's own bounds decide: a schema is refused whole when they fail.
    hsearch_engine::Schema::new(1, &[("v", Kind::Vector(spec))]).map_err(|_| {
        bad(
            name,
            "dimension, m, ef_construction or quantizer is out of range",
        )
    })?;
    Ok(Kind::Vector(spec))
}

/// The stored spelling of a vector kind, after `knn_vector:`.
pub(crate) fn spelling(spec: &VectorSpec) -> String {
    let space = SPACES
        .iter()
        .find(|(_, v)| *v == spec.similarity)
        .map_or("l2", |(n, _)| n);
    let quantizer = QUANTIZERS
        .iter()
        .find(|(_, v)| *v == spec.quantizer)
        .map_or("none", |(n, _)| n);
    format!(
        "knn_vector:dim={},space={space},m={},efc={},q={quantizer}",
        spec.dimension, spec.m, spec.ef_construction
    )
}

/// Read a stored spelling back; `None` when it is not exactly one `spelling` writes.
pub(crate) fn from_spelling(text: &str) -> Option<Kind> {
    let rest = text.strip_prefix("knn_vector:")?;
    let mut parts = rest.split(',');
    let dim = parts.next()?.strip_prefix("dim=")?.parse().ok()?;
    let space = parts.next()?.strip_prefix("space=")?;
    let m = parts.next()?.strip_prefix("m=")?.parse().ok()?;
    let efc = parts.next()?.strip_prefix("efc=")?.parse().ok()?;
    let q = parts.next()?.strip_prefix("q=")?;
    if parts.next().is_some() {
        return None;
    }
    let similarity = SPACES.iter().find(|(n, _)| *n == space)?.1;
    let quantizer = QUANTIZERS.iter().find(|(n, _)| *n == q)?.1;
    let spec = VectorSpec::hnsw(dim, similarity)
        .with_graph(m, efc)
        .with_quantizer(quantizer);
    let kind = Kind::Vector(spec);
    (spelling(&spec) == text).then_some(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_space_and_quantizer_round_trips_through_its_spelling() {
        for (_, similarity) in SPACES {
            for (_, quantizer) in QUANTIZERS {
                let spec = VectorSpec::hnsw(24, similarity)
                    .with_graph(8, 77)
                    .with_quantizer(quantizer);
                let text = spelling(&spec);
                assert_eq!(from_spelling(&text), Some(Kind::Vector(spec)), "{text}");
            }
        }
    }

    #[test]
    fn a_stored_spelling_that_is_not_exact_is_refused() {
        for text in [
            "knn_vector:dim=4,space=l2,m=16,efc=100",
            "knn_vector:dim=4,space=l2,m=16,efc=100,q=none,x=1",
            "knn_vector:dim=4,space=hamming,m=16,efc=100,q=none",
            "knn_vector:dim=4,space=l2,m=16,efc=100,q=pq",
            "knn_vector:dim=x,space=l2,m=16,efc=100,q=none",
            "knn_vector:dim=04,space=l2,m=16,efc=100,q=none",
            "knn_vector",
        ] {
            assert_eq!(from_spelling(text), None, "{text}");
        }
    }
}
