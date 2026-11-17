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
use lucene_codecs::vectors::FloatVectorValues;
use lucene_search::directory_reader::DirectoryReader;
use lucene_search::extended_query::{DocAndScoreQuery, ExtendedQuery};
use lucene_search::knn_search::{KnnTarget, knn_top_hits_with};
use lucene_search::multi_segment::OpenSegment;
use lucene_search::query::Clause;
use lucene_util::quantization::ScalarEncoding;

use crate::{Error, Result};

/// Largest vector dimension a field may declare. A document also has to fit
/// `Limits::document_bytes` at 4 bytes a component, so the default 8 KiB bound
/// reaches about 2,000 dimensions; raise the limit for more.
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

/// Fewest and most vectors an IVF cluster is split down to
/// (`KnnVectorsFormat::ivf_quantized`'s bounds).
pub const IVF_CLUSTER_SIZE: std::ops::RangeInclusive<u32> = 16..=65_536;
/// Cluster size an IVF field gets when the mapping gives none.
pub const DEFAULT_IVF_CLUSTER_SIZE: u32 = 512;

/// How an IVF field stores the full-precision vectors it reranks on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rerank {
    /// 32-bit floats, exactly.
    F32,
    /// 16-bit floats: half the bytes per reranked candidate.
    F16,
    /// bfloat16: half the bytes, the float32 range.
    Bf16,
    /// 8-bit residuals from the cluster centroid: about a quarter of the bytes.
    Int8,
}

/// The codes an IVF field keeps in each cluster's posting list. Every IVF
/// field has codes; the full-precision vectors are what it reranks on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IvfQuantizer {
    /// 1 bit per dimension plus 16 bytes, 4-bit queries.
    Bbq,
    /// The same quantizer at 2, 4 or 7 bits per dimension.
    Osq(u8),
    /// Extended RaBitQ at 2 to 8 bits per dimension.
    Erq(u8),
    /// BBQ after a random rotation.
    RaBitQ,
    /// Picked per segment from the field's own vectors.
    Auto,
}

impl IvfQuantizer {
    fn valid(self) -> bool {
        match self {
            Self::Osq(bits) => matches!(bits, 2 | 4 | 7),
            Self::Erq(bits) => (2..=8).contains(&bits),
            _ => true,
        }
    }

    /// Its place in `IVF_QUANTIZERS`, the wire number the format records.
    fn wire(self) -> u8 {
        match self {
            Self::Bbq => 0,
            Self::Osq(2) => 1,
            Self::Osq(4) => 2,
            Self::Osq(_) => 3,
            Self::Erq(bits) => 4 + (bits - 2),
            Self::RaBitQ => 11,
            Self::Auto => 12,
        }
    }
}

/// How a vector field is indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Index {
    /// An HNSW graph, with `m` and `ef_construction` from the spec.
    Hnsw,
    /// Clusters of about `target_cluster_size` vectors with coded posting
    /// lists, written once per segment (`lucene-ivf`).
    Ivf {
        /// Vectors a cluster is split down to.
        target_cluster_size: u32,
        /// Clusters a query probes when it names none; 0 leaves it to the
        /// field's automatic depth (a share of its clusters).
        nprobe: u32,
        /// Encoding of the vectors reranked on.
        rerank: Rerank,
        /// Codes of the posting lists.
        quantizer: IvfQuantizer,
    },
}

/// A declared dense-vector field: dimension, similarity and index parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorSpec {
    /// HNSW graph or IVF clusters.
    pub index: Index,
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
            index: Index::Hnsw,
        }
    }

    /// An IVF field: clusters of `target_cluster_size`, `quantizer` codes and
    /// `rerank` full-precision vectors. `m`, `ef_construction` and the
    /// HNSW quantizer do not apply to it.
    pub const fn ivf(
        dimension: u16,
        similarity: Similarity,
        target_cluster_size: u32,
        quantizer: IvfQuantizer,
        rerank: Rerank,
    ) -> Self {
        let mut spec = Self::hnsw(dimension, similarity);
        spec.index = Index::Ivf {
            target_cluster_size,
            nprobe: 0,
            rerank,
            quantizer,
        };
        spec
    }

    /// This spec keeping `quantizer`'s codes.
    #[must_use]
    pub const fn with_quantizer(mut self, quantizer: Quantizer) -> Self {
        self.quantizer = quantizer;
        self
    }

    /// An IVF spec probing `nprobe` clusters by default (0: the automatic
    /// depth); an HNSW spec is returned unchanged.
    #[must_use]
    pub const fn with_nprobe(mut self, nprobe: u32) -> Self {
        if let Index::Ivf {
            target_cluster_size,
            rerank,
            quantizer,
            ..
        } = self.index
        {
            self.index = Index::Ivf {
                target_cluster_size,
                nprobe,
                rerank,
                quantizer,
            };
        }
        self
    }

    /// Clusters a query that names none probes: the field's default, 0 for
    /// the automatic depth or for an HNSW field.
    pub const fn default_nprobe(&self) -> usize {
        match self.index {
            Index::Ivf { nprobe, .. } => nprobe as usize,
            Index::Hnsw => 0,
        }
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
            && match self.index {
                Index::Hnsw => true,
                Index::Ivf {
                    target_cluster_size,
                    nprobe,
                    quantizer,
                    ..
                } => {
                    IVF_CLUSTER_SIZE.contains(&target_cluster_size)
                        && nprobe as usize <= MAX_NPROBE
                        && quantizer.valid()
                        && self.quantizer == Quantizer::None
                }
            }
    }

    pub(crate) fn native_similarity(&self) -> VectorSimilarityFunction {
        self.similarity.native()
    }

    /// The writer format this spec routes its field to.
    pub(crate) fn format(&self) -> KnnVectorsFormat {
        if let Index::Ivf {
            target_cluster_size,
            rerank,
            quantizer,
            ..
        } = self.index
        {
            return KnnVectorsFormat::Ivf {
                target_cluster_size,
                rerank_encoding: match rerank {
                    Rerank::F32 => 0,
                    Rerank::F16 => 1,
                    Rerank::Bf16 => 2,
                    Rerank::Int8 => 3,
                },
                quantizer: quantizer.wire(),
            };
        }
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

/// The raw vectors of one field, opened once per segment so reading one back
/// costs no more than the read itself. Every document of a schema with a vector
/// field has one, so a vector's ordinal is its local document id; anything else
/// is a damaged index.
pub(crate) struct StoredVectors<'r> {
    /// Per segment: its first global document, its document count and its vectors.
    segments: Vec<(i32, i32, FloatVectorValues<'r>)>,
}

impl<'r> StoredVectors<'r> {
    pub(crate) fn open(reader: &'r DirectoryReader, name: &str, dimension: usize) -> Result<Self> {
        let mut segments = Vec::with_capacity(reader.segment_readers().len());
        let mut base = 0i32;
        for segment in reader.segment_readers() {
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
            segments.push((base, segment.max_doc, values));
            base = base.checked_add(segment.max_doc).ok_or(Error::Limit)?;
        }
        Ok(Self { segments })
    }

    /// The vector of global document `doc`.
    pub(crate) fn get(&self, doc: i32) -> Result<Vec<f32>> {
        let at = self.segments.partition_point(|(base, _, _)| *base <= doc);
        let (base, max_doc, values) = at
            .checked_sub(1)
            .and_then(|i| self.segments.get(i))
            .ok_or(Error::Native)?;
        if doc - base >= *max_doc {
            return Err(Error::Native);
        }
        values.vector(doc - base).map_err(|_| Error::Native)
    }
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
    fn ivf_specs_route_to_the_ivf_format_and_keep_their_bounds() {
        let ivf = |size, q, r| VectorSpec::ivf(8, Similarity::L2, size, q, r);
        assert!(matches!(
            ivf(512, IvfQuantizer::Erq(4), Rerank::F16).format(),
            KnnVectorsFormat::Ivf {
                target_cluster_size: 512,
                rerank_encoding: 1,
                quantizer: 6
            }
        ));
        let wire = |q| match ivf(512, q, Rerank::F32).format() {
            KnnVectorsFormat::Ivf { quantizer, .. } => quantizer,
            other => panic!("not ivf: {other:?}"),
        };
        assert_eq!(wire(IvfQuantizer::Bbq), 0);
        assert_eq!(wire(IvfQuantizer::Osq(2)), 1);
        assert_eq!(wire(IvfQuantizer::Osq(4)), 2);
        assert_eq!(wire(IvfQuantizer::Osq(7)), 3);
        assert_eq!(wire(IvfQuantizer::RaBitQ), 11);
        assert_eq!(wire(IvfQuantizer::Auto), 12);
        assert!(ivf(16, IvfQuantizer::Bbq, Rerank::Int8).valid());
        assert!(!ivf(15, IvfQuantizer::Bbq, Rerank::F32).valid());
        assert!(!ivf(65_537, IvfQuantizer::Bbq, Rerank::F32).valid());
        assert!(!ivf(512, IvfQuantizer::Osq(3), Rerank::F32).valid());
        let probing = ivf(512, IvfQuantizer::Bbq, Rerank::F32).with_nprobe(16);
        assert!(probing.valid());
        assert_eq!(probing.default_nprobe(), 16);
        assert!(!probing.with_nprobe(MAX_NPROBE as u32 + 1).valid());
        // The default does not change which files an ivf field writes.
        assert_eq!(
            probing.format(),
            ivf(512, IvfQuantizer::Bbq, Rerank::F32).format()
        );
        assert_eq!(
            VectorSpec::hnsw(8, Similarity::L2)
                .with_nprobe(9)
                .default_nprobe(),
            0
        );
        assert!(!ivf(512, IvfQuantizer::Erq(9), Rerank::F32).valid());
        let with_hnsw_codes =
            ivf(512, IvfQuantizer::Bbq, Rerank::F32).with_quantizer(Quantizer::Bbq);
        assert!(!with_hnsw_codes.valid());
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
