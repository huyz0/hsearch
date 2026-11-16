//! The paged name catalog (M03.05): names to opaque index identities, spread over a fixed fan-out
//! of bounded pages by the hash of the name, each page changed by its own conditional write.
//!
//! No global write bottleneck: two names in different buckets never contend. No LIST: a bucket's
//! key is derived from the catalog identity and the bucket number, so a name is resolved with one
//! HEAD and one read, and enumeration visits buckets `0..2^bits` by key. A deleted name leaves a
//! tombstone holding the identity it lost; the catalog refuses to bind the name to that identity
//! again, which is what "names cannot resurrect old UUIDs" means here. Tombstones are kept for
//! the life of the catalog (bounded by the number of distinct names ever used).
use hsearch_format::{
    CatalogEntry, CatalogPage, CatalogRoot, Id, IdentityBinding, Layout, MAX_CATALOG_BUCKET_BITS,
    NameState, bucket_of,
};
use hsearch_object::{MetadataStore, Precondition, VersionToken};

use crate::io::{Written, fresh_token, key, mutation, read_object, request_id, write};
use crate::{Error, Result};

/// A bucket page is at most this long on the wire (the format's body bound plus the envelope).
const PAGE_READ_BOUND: u64 = 640 * 1024;
const ROOT_READ_BOUND: u64 = 1024;
/// A page change that keeps losing its race or its reply is given up after this many attempts.
const MAX_ATTEMPTS: u32 = 16;

/// What a name resolves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolution {
    /// No binding was ever made.
    Absent,
    /// The name resolves to `index`.
    Live {
        /// The identity.
        index: Id,
        /// How many times the name has been bound or deleted.
        name_generation: u64,
    },
    /// The name was deleted; `last` is the identity it may never return to.
    Deleted {
        /// The identity the name lost.
        last: Id,
        /// How many times the name has been bound or deleted.
        name_generation: u64,
    },
}

/// Where an enumeration resumes. Opaque: obtain it from [`Catalog::enumerate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogCursor {
    bucket: u32,
    after: Option<String>,
}

/// What [`Catalog::verify`] found.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VerifyReport {
    /// Pages that exist.
    pub pages: u64,
    /// Pages that do not decode, or are not the page their key named.
    pub pages_bad: u64,
    /// Live names.
    pub live: u64,
    /// Tombstones.
    pub deleted: u64,
}

/// An open catalog: its identity and fan-out. Cheap to clone, holds no store.
#[derive(Clone, Debug)]
pub struct Catalog {
    id: Id,
    bits: u8,
}

/// Create a catalog with `2^bucket_bits` pages by conditional create of its root.
/// [`Error::Exists`] if the identity is taken.
pub async fn create_catalog<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    catalog: Id,
    bucket_bits: u8,
) -> Result<Catalog> {
    if bucket_bits == 0 || bucket_bits > MAX_CATALOG_BUCKET_BITS {
        return Err(Error::Invalid("catalog bucket bits"));
    }
    let root = CatalogRoot {
        catalog,
        bucket_bits,
        token: fresh_token(),
    };
    let mut bytes = Vec::new();
    root.encode(&mut bytes)?;
    let request = mutation(
        &request_id("cr", &catalog, 0, &root.token),
        &layout.catalog_root(&catalog),
        &bytes,
        Precondition::Absent,
    )?;
    match write(store, &request, false).await {
        Written::Done(_) => Ok(Catalog {
            id: catalog,
            bits: bucket_bits,
        }),
        Written::Occupied => Err(Error::Exists),
        Written::Refused(error) => Err(error.into()),
        Written::Unknown => Err(Error::Contended),
    }
}

/// Open an existing catalog by identity, or [`Error::NoCatalog`].
pub async fn open_catalog<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    catalog: Id,
) -> Result<Catalog> {
    let k = key(&layout.catalog_root(&catalog))?;
    let (bytes, _) = read_object(store, &k, ROOT_READ_BOUND)
        .await?
        .ok_or(Error::NoCatalog)?;
    let root = CatalogRoot::decode(bytes.as_bytes())?;
    if root.catalog != catalog {
        return Err(hsearch_format::FormatError::Mismatch("catalog identity").into());
    }
    Ok(Catalog {
        id: catalog,
        bits: root.bucket_bits,
    })
}

/// What a page edit decided.
enum Edit {
    /// Nothing to write: the page already says what the caller wants.
    Settled,
    /// Write this entry.
    Put(CatalogEntry),
}

impl Catalog {
    /// The catalog's identity.
    #[must_use]
    pub fn id(&self) -> Id {
        self.id
    }

    /// The fan-out: `2^bucket_bits` pages.
    #[must_use]
    pub fn bucket_bits(&self) -> u8 {
        self.bits
    }

    async fn read_page<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
        bucket: u32,
    ) -> Result<Option<(CatalogPage, VersionToken)>> {
        let k = key(&layout.catalog_page(&self.id, bucket))?;
        match read_object(store, &k, PAGE_READ_BOUND).await? {
            None => Ok(None),
            Some((bytes, version)) => Ok(Some((
                CatalogPage::decode_at(bytes.as_bytes(), self.bits, &self.id, bucket)?,
                version,
            ))),
        }
    }

    /// What `name` resolves to: one HEAD and one read.
    pub async fn resolve<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
        name: &str,
    ) -> Result<Resolution> {
        let bucket = bucket_of(&self.id, self.bits, name);
        let Some((page, _)) = self.read_page(store, layout, bucket).await? else {
            return Ok(Resolution::Absent);
        };
        Ok(page
            .entries
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .and_then(|at| page.entries.get(at))
            .map_or(Resolution::Absent, |e| match e.state {
                NameState::Live => Resolution::Live {
                    index: e.index,
                    name_generation: e.name_generation,
                },
                NameState::Deleted => Resolution::Deleted {
                    last: e.index,
                    name_generation: e.name_generation,
                },
            }))
    }

    /// Bind `name` to `index`. [`Error::Exists`] if the name is live under another identity;
    /// [`Error::Invalid`] if `index` is the identity the name lost when it was deleted. Repeating
    /// a create that already succeeded is a no-op, which is how a lost reply is settled.
    pub async fn create<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
        name: &str,
        index: Id,
    ) -> Result<()> {
        if index == [0; 16] {
            return Err(Error::Invalid("the zero id is not an index identity"));
        }
        self.edit(store, layout, name, Some(&index), |found| match found {
            None => Ok(Edit::Put(CatalogEntry {
                name: name.to_owned(),
                state: NameState::Live,
                index,
                name_generation: 1,
            })),
            Some(e) if e.state == NameState::Live && e.index == index => Ok(Edit::Settled),
            Some(e) if e.state == NameState::Live => Err(Error::Exists),
            Some(e) if e.index == index => Err(Error::Invalid(
                "a deleted name cannot resurrect its old identity",
            )),
            Some(e) => Ok(Edit::Put(CatalogEntry {
                name: name.to_owned(),
                state: NameState::Live,
                index,
                name_generation: e.name_generation.saturating_add(1),
            })),
        })
        .await
    }

    /// Delete the binding of `name` to `index`, leaving a tombstone. [`Error::NoName`] if the name
    /// is absent or live under another identity (a stale delete). Repeating a delete that already
    /// happened is a no-op.
    pub async fn delete<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
        name: &str,
        index: &Id,
    ) -> Result<()> {
        self.edit(store, layout, name, None, |found| match found {
            Some(e) if e.state == NameState::Live && &e.index == index => {
                Ok(Edit::Put(CatalogEntry {
                    name: name.to_owned(),
                    state: NameState::Deleted,
                    index: e.index,
                    name_generation: e.name_generation.saturating_add(1),
                }))
            }
            Some(e) if e.state == NameState::Deleted && &e.index == index => Ok(Edit::Settled),
            _ => Err(Error::NoName),
        })
        .await
    }

    /// Bind `index` to `name` at `generation`, once and for ever: a create-only object per identity,
    /// written before the page that makes the name live. If the object is already there, the create
    /// is a retry of the same one (same name, same generation: a lost reply or a crash between the
    /// binding and the page) or it is a reuse, which is refused. This is what makes "an identity
    /// never comes back" hold for every identity a name has held, not only the last, at the price of
    /// one more PUT per create. An identity whose create lost the race for a generation is refused
    /// if it comes back later (it was never bound, but cannot be told from one that was); the
    /// caller picks a new identity.
    async fn bind<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
        index: &Id,
        name: &str,
        generation: u64,
    ) -> Result<()> {
        let binding = IdentityBinding {
            catalog: self.id,
            index: *index,
            name_generation: generation,
            name: name.to_owned(),
        };
        let mut bytes = Vec::new();
        binding.encode(&mut bytes)?;
        let k = layout.catalog_identity(&self.id, index);
        let request = mutation(
            &request_id("ci", index, generation, &fresh_token()),
            &k,
            &bytes,
            Precondition::Absent,
        )?;
        match write(store, &request, false).await {
            Written::Done(_) => return Ok(()),
            Written::Refused(error) => return Err(error.into()),
            Written::Occupied | Written::Unknown => {}
        }
        let object = key(&k)?;
        let Some((stored, _)) = read_object(store, &object, 1024).await? else {
            return Err(Error::Contended);
        };
        if IdentityBinding::decode_at(stored.as_bytes(), &self.id, index)? == binding {
            Ok(())
        } else {
            Err(Error::Invalid(
                "an identity is bound once and cannot be bound again",
            ))
        }
    }

    /// Read the bucket, decide against what is there, write one conditional page, and on a lost
    /// race or an unresolvable reply read again and decide again. The decision is re-made from the
    /// page every time, so an attempt whose reply was lost but which landed is recognised as
    /// settled instead of being applied twice.
    async fn edit<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
        name: &str,
        bind: Option<&Id>,
        decide: impl Fn(Option<&CatalogEntry>) -> Result<Edit>,
    ) -> Result<()> {
        let bucket = bucket_of(&self.id, self.bits, name);
        let k = layout.catalog_page(&self.id, bucket);
        let mut bound = false;
        for _ in 0..MAX_ATTEMPTS {
            let current = self.read_page(store, layout, bucket).await?;
            let found = current.as_ref().and_then(|(page, _)| {
                page.entries
                    .binary_search_by(|e| e.name.as_str().cmp(name))
                    .ok()
                    .and_then(|at| page.entries.get(at))
            });
            let Edit::Put(entry) = decide(found)? else {
                return Ok(());
            };
            if let Some(index) = bind.filter(|_| !bound) {
                self.bind(store, layout, index, name, entry.name_generation)
                    .await?;
                // Bound by this call: a retry in the same call (the page moved under it) keeps it.
                bound = true;
            }
            let (mut page, precondition) = match current {
                Some((page, version)) => (page, Precondition::Matches(version)),
                None => (
                    CatalogPage {
                        catalog: self.id,
                        bucket,
                        generation: 0,
                        entries: Vec::new(),
                        token: fresh_token(),
                    },
                    Precondition::Absent,
                ),
            };
            let previous = page.clone();
            match page.entries.binary_search_by(|e| e.name.as_str().cmp(name)) {
                Ok(at) => {
                    if let Some(slot) = page.entries.get_mut(at) {
                        *slot = entry;
                    }
                }
                Err(at) => page.entries.insert(at, entry),
            }
            page.generation = previous.generation.saturating_add(1);
            page.token = fresh_token();
            if previous.generation != 0 {
                previous.validate_successor(&page)?;
            }
            let mut bytes = Vec::new();
            page.encode(&mut bytes, self.bits)?;
            let request = mutation(
                &request_id(
                    "cp",
                    &self.id,
                    u64::from(bucket) << 32 | page.generation,
                    &page.token,
                ),
                &k,
                &bytes,
                precondition.clone(),
            )?;
            let replace = precondition != Precondition::Absent;
            match write(store, &request, replace).await {
                Written::Done(_) => return Ok(()),
                Written::Occupied | Written::Unknown => {}
                Written::Refused(error) => return Err(error.into()),
            }
        }
        Err(Error::Contended)
    }

    /// Up to `limit` live entries, bucket by bucket from `cursor` (or the start), and the cursor
    /// to continue from, or `None` once every bucket has been read. Order is by bucket, then name:
    /// stable, not alphabetical across buckets. One HEAD and one read per bucket visited; the
    /// final call may return no entries.
    pub async fn enumerate<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
        cursor: Option<CatalogCursor>,
        limit: usize,
    ) -> Result<(Vec<CatalogEntry>, Option<CatalogCursor>)> {
        if limit == 0 {
            return Err(Error::Invalid("enumeration limit"));
        }
        let buckets = 1u64 << self.bits;
        let (mut bucket, mut after) = cursor.map_or((0, None), |c| (c.bucket, c.after));
        let mut out = Vec::new();
        while u64::from(bucket) < buckets {
            if let Some((page, _)) = self.read_page(store, layout, bucket).await? {
                for e in &page.entries {
                    if e.state != NameState::Live
                        || after.as_deref().is_some_and(|a| e.name.as_str() <= a)
                    {
                        continue;
                    }
                    out.push(e.clone());
                    if out.len() == limit {
                        let next = CatalogCursor {
                            bucket,
                            after: Some(e.name.clone()),
                        };
                        return Ok((out, Some(next)));
                    }
                }
            }
            bucket += 1;
            after = None;
        }
        Ok((out, None))
    }

    /// Read every bucket and check that each page decodes as the page its key named, with every
    /// name in its own bucket. Cost: one HEAD per bucket, plus a read for each that exists; it is
    /// a repair and audit tool, not a hot path.
    pub async fn verify<S: MetadataStore>(
        &self,
        store: &S,
        layout: &Layout,
    ) -> Result<VerifyReport> {
        let mut report = VerifyReport::default();
        for bucket in 0..(1u32 << self.bits) {
            match self.read_page(store, layout, bucket).await {
                Ok(None) => {}
                Ok(Some((page, _))) => {
                    report.pages += 1;
                    for e in &page.entries {
                        match e.state {
                            NameState::Live => report.live += 1,
                            NameState::Deleted => report.deleted += 1,
                        }
                    }
                }
                Err(Error::Format(_)) => {
                    report.pages += 1;
                    report.pages_bad += 1;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(report)
    }
}
