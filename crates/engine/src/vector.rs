//! Dense float vectors: the field's declared index (HNSW, plain or quantized),
//! its validation and the k-nearest-neighbour query's resolution.
//!
//! The index is the engine's own HNSW graph over the raw vectors. A quantized
//! variant adds codes beside the same graph; a search walks the graph scoring
//! codes and reranks the candidates on the raw vectors, so the choice trades
//! index bytes and distance cost against build time, never against the
//! exactness of the final order.
use lucene_codecs::field_infos::VectorSimilarityFunction;
use lucene_codecs::per_field_knn_vectors::KnnVectorsFormat;
use lucene_search::directory_reader::DirectoryReader;
use lucene_search::extended_query::{DocAndScoreQuery, ExtendedQuery};
use lucene_search::knn_search::{KnnTarget, knn_top_hits_with};
use lucene_search::multi_segment::OpenSegment;
use lucene_search::query::Clause;
use lucene_util::quantization::ScalarEncoding;

use crate::{Error, Result};

/// Largest vector dimension a field may declare.
pub const MAX_DIMENSION: usize = 4096;
/// Most vector fields in one schema.
pub const MAX_VECTOR_FIELDS: usize = 4;
/// Largest `k` a kNN query may ask for.
pub const MAX_KNN_K: usize = 10_000;
/// Largest candidate beam (`ef_search`) a kNN query may ask for.
pub const MAX_KNN_CANDIDATES: usize = 10_000;
/// Largest `nprobe` a kNN query may ask for.
pub const MAX_NPROBE: usize = 65_536;
/// Largest HNSW `m` (`Lucene99HnswVectorsFormat.MAXIMUM_MAX_CONN`).
pub const MAX_M: u16 = 512;
/// Largest HNSW `ef_construction` (`MAXIMUM_BEAM_WIDTH`).
pub const MAX_EF_CONSTRUCTION: u16 = 3200;

/// How two vectors are compared; the higher a score, the nearer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Similarity {
    /// Euclidean (`l2`) distance, scored as `1 / (1 + d²)`.
    L2,
    /// Cosine similarity, scored as `(1 + cos) / 2`.
    Cosine,
    /// Dot product of unit-length vectors, scored as `(1 + dot) / 2`.
    DotProduct,
    /// Maximum inner product (`innerproduct`), scored on a positive scale.
    MaxInnerProduct,
}

impl Similarity {
    fn native(self) -> VectorSimilarityFunction {
        match self {
            Self::L2 => VectorSimilarityFunction::Euclidean,
            Self::Cosine => VectorSimilarityFunction::Cosine,
            Self::DotProduct => VectorSimilarityFunction::DotProduct,
            Self::MaxInnerProduct => VectorSimilarityFunction::MaximumInnerProduct,
        }
    }
}

/// Bits a scalar quantizer keeps per dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarBits {
    /// 8 bits, one byte per dimension.
    Eight,
    /// 7 bits, one byte per dimension, scored as signed bytes.
    Seven,
    /// 4 bits, two dimensions per byte.
    Four,
    /// 2 bits per dimension.
    Two,
    /// 1 bit per dimension (32x smaller than float).
    One,
}

/// The codes an index keeps beside the raw vectors, or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quantizer {
    /// Raw vectors only.
    None,
    /// Lucene's optimized scalar quantization at the given width.
    Scalar(ScalarBits),
    /// Better binary quantization: 1 bit per dimension plus 16 bytes.
    Bbq,
    /// RaBitQ: 1 bit per dimension with a randomized rotation.
    RaBitQ,
    /// Extended RaBitQ at 2 to 8 bits per dimension.
    Erq(u8),
}

impl Quantizer {
    fn valid(self) -> bool {
        !matches!(self, Self::Erq(bits) if !(2..=8).contains(&bits))
    }
}

/// A declared dense-vector field: dimension, similarity and HNSW parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorSpec {
    /// Components of every vector in the field.
    pub dimension: u16,
    /// Comparison.
    pub similarity: Similarity,
    /// Graph degree: neighbours kept per node (Lucene's `maxConn`).
    pub m: u16,
    /// Beam width while building the graph.
    pub ef_construction: u16,
    /// Codes kept beside the raw vectors.
    pub quantizer: Quantizer,
}

impl VectorSpec {
    /// Plain HNSW with OpenSearch's defaults (`m` 16, `ef_construction` 100).
    pub const fn hnsw(dimension: u16, similarity: Similarity) -> Self {
        Self {
            dimension,
            similarity,
            m: 16,
            ef_construction: 100,
            quantizer: Quantizer::None,
        }
    }

    /// This spec keeping `quantizer`'s codes.
    #[must_use]
    pub const fn with_quantizer(mut self, quantizer: Quantizer) -> Self {
        self.quantizer = quantizer;
        self
    }

    /// This spec with graph parameters `m` and `ef_construction`.
    #[must_use]
    pub const fn with_graph(mut self, m: u16, ef_construction: u16) -> Self {
        self.m = m;
        self.ef_construction = ef_construction;
        self
    }

    pub(crate) fn valid(&self) -> bool {
        (1..=MAX_DIMENSION as u16).contains(&self.dimension)
            && (1..=MAX_M).contains(&self.m)
            && (1..=MAX_EF_CONSTRUCTION).contains(&self.ef_construction)
            && self.quantizer.valid()
    }

    pub(crate) fn native_similarity(&self) -> VectorSimilarityFunction {
        self.similarity.native()
    }

    /// The writer format this spec routes its field to.
    pub(crate) fn format(&self) -> KnnVectorsFormat {
        let (max_conn, beam_width) = (i32::from(self.m), i32::from(self.ef_construction));
        let scalar = |encoding| KnnVectorsFormat::HnswScalarQuantized {
            encoding,
            max_conn,
            beam_width,
        };
        // The wire number of a quantizer is its place in `IVF_QUANTIZERS`.
        let coded = |quantizer: u8| KnnVectorsFormat::HnswQuantized {
            quantizer,
            max_conn,
            beam_width,
        };
        match self.quantizer {
            Quantizer::None => KnnVectorsFormat::Hnsw {
                max_conn,
                beam_width,
            },
            Quantizer::Scalar(ScalarBits::Eight) => scalar(ScalarEncoding::UnsignedByte),
            Quantizer::Scalar(ScalarBits::Seven) => scalar(ScalarEncoding::SevenBit),
            Quantizer::Scalar(ScalarBits::Four) => scalar(ScalarEncoding::PackedNibble),
            Quantizer::Scalar(ScalarBits::Two) => scalar(ScalarEncoding::DibitQueryNibble),
            Quantizer::Scalar(ScalarBits::One) => scalar(ScalarEncoding::SingleBitQueryNibble),
            Quantizer::Bbq => coded(0),
            Quantizer::Erq(bits) => coded(4 + (bits - 2)),
            Quantizer::RaBitQ => coded(11),
        }
    }

    /// A vector of this field: right length, every component finite.
    pub(crate) fn check(&self, vector: &[f32]) -> Result<()> {
        if vector.len() != usize::from(self.dimension) || vector.iter().any(|c| !c.is_finite()) {
            return Err(Error::Document);
        }
        Ok(())
    }
}

/// Resolution context for a kNN query: the segments it runs over.
pub(crate) struct KnnContext<'s, 'r> {
    /// The reader's segments, each with its reader and deletions.
    pub segments: &'s [OpenSegment<'r>],
}

/// Run one kNN search and return it as a clause that composes in a bool, the
/// way Lucene rewrites a kNN query: fixed documents with fixed scores.
pub(crate) fn resolve(
    context: &KnnContext<'_, '_>,
    field: &str,
    vector: &[f32],
    (k, candidates, nprobe): (usize, usize, usize),
    filter: Option<&Clause>,
) -> Result<Clause> {
    if k == 0 || k > MAX_KNN_K || candidates > MAX_KNN_CANDIDATES || nprobe > MAX_NPROBE {
        return Err(Error::Limit);
    }
    let hits = knn_top_hits_with(
        context.segments,
        field,
        &KnnTarget::Float(vector.to_vec()),
        k,
        candidates.max(k),
        filter,
        nprobe,
    )
    .map_err(|_| Error::Native)?;
    let bases: Vec<i32> = context.segments.iter().map(|s| s.doc_base).collect();
    let hits = hits.into_iter().map(|h| (h.doc_id, h.score)).collect();
    Ok(Clause::Extended(Box::new(ExtendedQuery::DocAndScore(
        DocAndScoreQuery::new(hits, &bases),
    ))))
}

/// The raw vector stored for global document `doc`, read back from the segment
/// that holds it. Every document of a schema with a vector field has one, so
/// ordinals equal local document ids; anything else is a damaged index.
pub(crate) fn stored_vector(
    reader: &DirectoryReader,
    mut doc: i32,
    name: &str,
    dimension: usize,
) -> Result<Vec<f32>> {
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
            let flat = segment
                .flat_vectors_reader_for(number)
                .map_err(|_| Error::Native)?
                .ok_or(Error::Checkpoint)?;
            let values = flat
                .float_vector_values(number)
                .map_err(|_| Error::Native)?;
            if values.dimension() != dimension || values.size() != segment.max_doc {
                return Err(Error::Checkpoint);
            }
            return values.vector(doc).map_err(|_| Error::Native);
        }
        doc -= segment.max_doc;
    }
    Err(Error::Native)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_bounds_are_enforced() {
        let ok = VectorSpec::hnsw(8, Similarity::L2);
        assert!(ok.valid());
        assert!(!VectorSpec::hnsw(0, Similarity::L2).valid());
        assert!(!VectorSpec::hnsw(MAX_DIMENSION as u16 + 1, Similarity::L2).valid());
        assert!(!ok.with_graph(0, 100).valid());
        assert!(!ok.with_graph(MAX_M + 1, 100).valid());
        assert!(!ok.with_graph(16, 0).valid());
        assert!(!ok.with_graph(16, MAX_EF_CONSTRUCTION + 1).valid());
        assert!(!ok.with_quantizer(Quantizer::Erq(1)).valid());
        assert!(!ok.with_quantizer(Quantizer::Erq(9)).valid());
        assert!(ok.with_quantizer(Quantizer::Erq(8)).valid());
    }

    #[test]
    fn quantizers_route_to_their_formats() {
        let base = VectorSpec::hnsw(8, Similarity::Cosine);
        assert!(matches!(
            base.format(),
            KnnVectorsFormat::Hnsw {
                max_conn: 16,
                beam_width: 100
            }
        ));
        assert!(matches!(
            base.with_quantizer(Quantizer::Scalar(ScalarBits::Four))
                .format(),
            KnnVectorsFormat::HnswScalarQuantized {
                encoding: ScalarEncoding::PackedNibble,
                ..
            }
        ));
        let numbered = |q| match base.with_quantizer(q).format() {
            KnnVectorsFormat::HnswQuantized { quantizer, .. } => quantizer,
            other => panic!("not a coded format: {other:?}"),
        };
        assert_eq!(numbered(Quantizer::Bbq), 0);
        assert_eq!(numbered(Quantizer::Erq(2)), 4);
        assert_eq!(numbered(Quantizer::Erq(8)), 10);
        assert_eq!(numbered(Quantizer::RaBitQ), 11);
    }

    #[test]
    fn vectors_are_checked_for_length_and_finiteness() {
        let spec = VectorSpec::hnsw(3, Similarity::L2);
        assert!(spec.check(&[1.0, 2.0, 3.0]).is_ok());
        assert!(spec.check(&[1.0, 2.0]).is_err());
        assert!(spec.check(&[1.0, f32::NAN, 3.0]).is_err());
        assert!(spec.check(&[1.0, f32::INFINITY, 3.0]).is_err());
    }
}
