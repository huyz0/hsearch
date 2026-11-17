//! Compaction: replacing the base the head names with a rewritten one that covers the same history.
//!
//! A compaction is a publish whose source is validated twice and whose replacement may only take
//! the source's place: the head must name exactly the source root (a base published since carries
//! updates and deletes the merge never saw, so replacing it would resurrect them), the replacement
//! must cover exactly what the source covered under the same mapping and key, and its generation
//! must be above the collection boundary (a compaction that stalled across a collection must not
//! name generations the collector may have swept). The head transition is a compare-and-swap on
//! the version read, so a source that moves after the last check is still refused. What the merge
//! put in the files is the engine's contract, not checked here.
use std::sync::Arc;
use std::time::Duration;

use hsearch_engine::LocalFiles;
use hsearch_format::{Layout, ManifestRoot, StructuralHead};
use hsearch_object::{MetadataStore, VersionToken};

use crate::boundary::{self, Boundary};
use crate::io::Io;
use crate::publish::{
    BaseSpec, Dependencies, Error as PublishError, HeadRead, Published, Publisher, manifest_id,
};

/// Why a compaction was refused.
#[derive(Debug)]
pub enum Error {
    /// The head no longer names the source root (or the spec's previous base is not the source).
    SourceMoved,
    /// The replacement is under another mapping or key than the source.
    Incompatible,
    /// The replacement covers more or less history than the source.
    CoverageChanged,
    /// The replacement's generation is not above the collection boundary.
    Swept {
        /// The boundary that refused it.
        boundary: u64,
    },
    /// The boundary object is not valid.
    Boundary,
    /// Publishing or activating failed; objects written stay for collection.
    Publish(PublishError),
}

impl From<PublishError> for Error {
    fn from(e: PublishError) -> Self {
        Self::Publish(e)
    }
}

/// Compacts shards of one deployment.
pub struct Compactor<S> {
    publisher: Publisher<S>,
    io: Io<S>,
    layout: Layout,
}

impl<S: MetadataStore + hsearch_object::LargeObjectWriter + 'static> Compactor<S> {
    /// A compactor publishing through `store`; `part_size` bounds upload memory (at least 5 MiB).
    #[must_use]
    pub fn new(store: Arc<S>, layout: Layout, part_size: usize, deadline: Duration) -> Self {
        Self {
            publisher: Publisher::new(Arc::clone(&store), layout.clone(), part_size, deadline),
            io: Io::new(store, deadline),
            layout,
        }
    }

    /// Prove `source` is what the head names and that `spec` describes a compaction of it.
    pub fn validate_source(
        &self,
        at: &HeadRead<'_>,
        spec: &BaseSpec<'_>,
        source: &ManifestRoot,
        dependencies: &dyn Dependencies,
    ) -> Result<(), Error> {
        let named = manifest_id(&spec.index, &spec.shard, &source.token);
        if at.head.checkpoint != Some(named)
            || spec.previous.map(|p| &p.token) != Some(&source.token)
        {
            return Err(Error::SourceMoved);
        }
        if spec.mapping != source.mapping || spec.crypto_envelope != source.crypto_envelope {
            return Err(Error::Incompatible);
        }
        if spec.cut.covers() != source.covered {
            return Err(Error::CoverageChanged);
        }
        self.publisher.verify_root(
            &spec.index,
            &spec.shard,
            spec.incarnation,
            &named,
            dependencies,
        )?;
        Ok(())
    }

    fn above_boundary(&self, spec: &BaseSpec<'_>, generation: u64) -> Result<(), Error> {
        let (boundary, _) = Boundary::new(&self.layout, &spec.index, &spec.shard, spec.incarnation)
            .read(&self.io)
            .map_err(|e| match e {
                boundary::Error::Corrupt => Error::Boundary,
                boundary::Error::Store(e) => Error::Publish(PublishError::Store(e)),
            })?;
        if generation < boundary {
            return Err(Error::Swept { boundary });
        }
        Ok(())
    }

    /// Replace `source` (which `at` must name) with the base made of `names` in `files`. Nothing is
    /// named by the head unless every check passes; a refusal after the upload leaves orphans.
    pub fn compact(
        &self,
        files: &LocalFiles,
        names: &[String],
        spec: &BaseSpec<'_>,
        at: &HeadRead<'_>,
        dependencies: &dyn Dependencies,
    ) -> Result<(Published, StructuralHead, VersionToken), Error> {
        let source = spec.previous.ok_or(Error::SourceMoved)?;
        self.validate_source(at, spec, source, dependencies)?;
        let published = self.publisher.publish(files, names, spec, dependencies)?;
        let (head, version) = self.finish(at, &published, spec, dependencies)?;
        Ok((published, head, version))
    }

    /// Name a replacement published by [`Publisher::publish`] in the head. This is the step a
    /// stalled compaction resumes at: it refuses a generation the collector may have swept
    /// ([`Error::Swept`]) and a head that moved ([`PublishError::Lost`]), and writes nothing then.
    pub fn finish(
        &self,
        at: &HeadRead<'_>,
        published: &Published,
        spec: &BaseSpec<'_>,
        dependencies: &dyn Dependencies,
    ) -> Result<(StructuralHead, VersionToken), Error> {
        self.above_boundary(spec, published.root.generation)?;
        Ok(self.publisher.replace(at, published, spec, dependencies)?)
    }
}
