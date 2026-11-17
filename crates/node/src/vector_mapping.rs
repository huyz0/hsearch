//! The `knn_vector` mapping type: parsing its OpenSearch-shaped parameters and
//! the one-line stored spelling that round-trips them.
use hsearch_engine::{
    DEFAULT_IVF_CLUSTER_SIZE, IVF_CLUSTER_SIZE, Index, IvfQuantizer, Kind, MAX_NPROBE, Quantizer,
    Rerank, ScalarBits, Similarity, VectorSpec,
};
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

const IVF_QUANTIZERS: [(&str, IvfQuantizer); 13] = [
    ("bbq", IvfQuantizer::Bbq),
    ("osq2", IvfQuantizer::Osq(2)),
    ("osq4", IvfQuantizer::Osq(4)),
    ("osq7", IvfQuantizer::Osq(7)),
    ("erq2", IvfQuantizer::Erq(2)),
    ("erq3", IvfQuantizer::Erq(3)),
    ("erq4", IvfQuantizer::Erq(4)),
    ("erq5", IvfQuantizer::Erq(5)),
    ("erq6", IvfQuantizer::Erq(6)),
    ("erq7", IvfQuantizer::Erq(7)),
    ("erq8", IvfQuantizer::Erq(8)),
    ("rabitq", IvfQuantizer::RaBitQ),
    ("auto", IvfQuantizer::Auto),
];

const RERANKS: [(&str, Rerank); 4] = [
    ("f32", Rerank::F32),
    ("f16", Rerank::F16),
    ("bf16", Rerank::Bf16),
    ("int8", Rerank::Int8),
];

/// The reason an OpenSearch ivf parameter has no counterpart here.
fn ivf_refusal(parameter: &str) -> &'static str {
    match parameter {
        "nlist" => {
            "the cluster count follows each segment's size; set [target_cluster_size], the vectors a cluster is split down to"
        }
        "encoder" => {
            "codes are chosen by [quantizer] (bbq, osq2/4/7, erq2-8, rabitq, auto) and the stored vectors by [rerank_encoding]; there is no flat or pq encoder"
        }
        _ => "it has no counterpart",
    }
}

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
/// `l2`), `method` (`hnsw` with `m` and `ef_construction`, or `ivf` with
/// `target_cluster_size` and `rerank_encoding`) and `quantizer`.
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
    let (mut cluster, mut rerank) = (DEFAULT_IVF_CLUSTER_SIZE, Rerank::F32);
    let mut nprobe = 0u32;
    let mut ivf = false;
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
        ivf = match method.get("name").and_then(Value::as_str) {
            Some("hnsw") => false,
            Some("ivf") => true,
            _ => return Err(bad(name, "[method.name] must be [hnsw] or [ivf]")),
        };
        // OpenSearch offers ivf only on faiss, so a faiss ivf mapping means
        // this engine's ivf; hnsw stays lucene-only.
        let engines: &[&str] = if ivf {
            &["lucene", "faiss"]
        } else {
            &["lucene"]
        };
        if let Some(engine) = method.get("engine")
            && !engine.as_str().is_some_and(|e| engines.contains(&e))
        {
            return Err(bad(
                name,
                format!("[method.engine] must be one of {engines:?}"),
            ));
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
            if ivf {
                if let Some(extra) = parameters.keys().find(|k| {
                    !["target_cluster_size", "nprobes", "rerank_encoding"].contains(&k.as_str())
                }) {
                    return Err(bad(
                        name,
                        format!(
                            "unsupported method parameter [{extra}]: {}",
                            ivf_refusal(extra)
                        ),
                    ));
                }
                if let Some(v) = parameters.get("target_cluster_size") {
                    cluster = v
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|n| IVF_CLUSTER_SIZE.contains(n))
                        .ok_or_else(|| {
                            bad(
                                name,
                                format!(
                                    "[target_cluster_size] must be {} to {}",
                                    IVF_CLUSTER_SIZE.start(),
                                    IVF_CLUSTER_SIZE.end()
                                ),
                            )
                        })?;
                }
                if let Some(v) = parameters.get("nprobes") {
                    nprobe = v
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|n| (1..=MAX_NPROBE as u32).contains(n))
                        .ok_or_else(|| bad(name, format!("[nprobes] must be 1 to {MAX_NPROBE}")))?;
                }
                if let Some(v) = parameters.get("rerank_encoding") {
                    let text = v
                        .as_str()
                        .ok_or_else(|| bad(name, "[rerank_encoding] must be a string"))?;
                    rerank = RERANKS
                        .iter()
                        .find(|(n, _)| *n == text)
                        .map(|(_, v)| *v)
                        .ok_or_else(|| {
                            bad(name, format!("unsupported rerank_encoding [{text}]"))
                        })?;
                }
            } else {
                only(name, parameters, &["m", "ef_construction"], "method")?;
                if let Some(v) = parameters.get("m") {
                    m = number(name, "m", v)?;
                }
                if let Some(v) = parameters.get("ef_construction") {
                    efc = number(name, "ef_construction", v)?;
                }
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
    let quantizer = definition.get("quantizer").map(Value::as_str);
    let spec = if ivf {
        let quantizer = match quantizer {
            None => IvfQuantizer::Bbq,
            Some(Some(s)) => IVF_QUANTIZERS
                .iter()
                .find(|(n, _)| *n == s)
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    bad(
                        name,
                        format!(
                            "unsupported ivf quantizer [{s}]; an ivf field always keeps codes: bbq, osq2, osq4, osq7, erq2-erq8, rabitq or auto"
                        ),
                    )
                })?,
            Some(None) => return Err(bad(name, "[quantizer] must be a string")),
        };
        if nprobe == 0 {
            return Err(bad(
                name,
                "[method.parameters.nprobes] is required for ivf: the automatic depth probes 5% of the clusters, which measured 0.325 recall@10 at 50,000 vectors; about a sixth of a segment's clusters reached 0.97",
            ));
        }
        VectorSpec::ivf(dimension, similarity, cluster, quantizer, rerank).with_nprobe(nprobe)
    } else {
        let quantizer = match quantizer {
            None => Quantizer::None,
            Some(Some(s)) => QUANTIZERS
                .iter()
                .find(|(n, _)| *n == s)
                .map(|(_, v)| *v)
                .ok_or_else(|| bad(name, format!("unsupported quantizer [{s}]")))?,
            Some(None) => return Err(bad(name, "[quantizer] must be a string")),
        };
        VectorSpec::hnsw(dimension, similarity)
            .with_graph(m, efc)
            .with_quantizer(quantizer)
    };
    // The engine's own bounds decide: a schema is refused whole when they fail.
    hsearch_engine::Schema::new(1, &[("v", Kind::Vector(spec))]).map_err(|_| {
        bad(
            name,
            "dimension, m, ef_construction, target_cluster_size or quantizer is out of range",
        )
    })?;
    Ok(Kind::Vector(spec))
}

/// The stored spelling of a vector kind, after `knn_vector:`. An hnsw field
/// keeps the spelling it always had, so a mapping without an ivf field keeps
/// its bytes and its hash.
pub(crate) fn spelling(spec: &VectorSpec) -> String {
    let space = SPACES
        .iter()
        .find(|(_, v)| *v == spec.similarity)
        .map_or("l2", |(n, _)| n);
    if let Index::Ivf {
        target_cluster_size,
        nprobe,
        rerank,
        quantizer,
    } = spec.index
    {
        let quantizer = IVF_QUANTIZERS
            .iter()
            .find(|(_, v)| *v == quantizer)
            .map_or("bbq", |(n, _)| n);
        let rerank = RERANKS
            .iter()
            .find(|(_, v)| *v == rerank)
            .map_or("f32", |(n, _)| n);
        return format!(
            "knn_vector:dim={},space={space},ivf,cs={target_cluster_size},np={nprobe},rr={rerank},q={quantizer}",
            spec.dimension
        );
    }
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
    let similarity = SPACES.iter().find(|(n, _)| *n == space)?.1;
    let third = parts.next()?;
    let spec = if third == "ivf" {
        let cs = parts.next()?.strip_prefix("cs=")?.parse().ok()?;
        let np = parts.next()?.strip_prefix("np=")?.parse().ok()?;
        let rr = parts.next()?.strip_prefix("rr=")?;
        let q = parts.next()?.strip_prefix("q=")?;
        let rerank = RERANKS.iter().find(|(n, _)| *n == rr)?.1;
        let quantizer = IVF_QUANTIZERS.iter().find(|(n, _)| *n == q)?.1;
        if !IVF_CLUSTER_SIZE.contains(&cs) || np as usize > MAX_NPROBE {
            return None;
        }
        VectorSpec::ivf(dim, similarity, cs, quantizer, rerank).with_nprobe(np)
    } else {
        let m = third.strip_prefix("m=")?.parse().ok()?;
        let efc = parts.next()?.strip_prefix("efc=")?.parse().ok()?;
        let q = parts.next()?.strip_prefix("q=")?;
        let quantizer = QUANTIZERS.iter().find(|(n, _)| *n == q)?.1;
        VectorSpec::hnsw(dim, similarity)
            .with_graph(m, efc)
            .with_quantizer(quantizer)
    };
    if parts.next().is_some() {
        return None;
    }
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
    fn every_ivf_variant_round_trips_through_its_spelling() {
        for (_, similarity) in SPACES {
            for (_, quantizer) in IVF_QUANTIZERS {
                for (_, rerank) in RERANKS {
                    let spec =
                        VectorSpec::ivf(24, similarity, 256, quantizer, rerank).with_nprobe(7);
                    let text = spelling(&spec);
                    assert_eq!(from_spelling(&text), Some(Kind::Vector(spec)), "{text}");
                }
            }
        }
    }

    #[test]
    fn a_stored_ivf_spelling_that_is_not_exact_is_refused() {
        for text in [
            "knn_vector:dim=4,space=l2,ivf,cs=256,np=0,rr=f32",
            "knn_vector:dim=4,space=l2,ivf,cs=256,np=0,rr=f32,q=bbq,x=1",
            "knn_vector:dim=4,space=l2,ivf,cs=256,rr=f32,q=bbq",
            "knn_vector:dim=4,space=l2,ivf,cs=256,np=0,rr=f64,q=bbq",
            "knn_vector:dim=4,space=l2,ivf,cs=256,np=0,rr=f32,q=none",
            "knn_vector:dim=4,space=l2,ivf,cs=0256,np=0,rr=f32,q=bbq",
        ] {
            assert_eq!(from_spelling(text), None, "{text}");
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
