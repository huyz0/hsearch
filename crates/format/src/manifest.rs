//! Manifest root and bounded pages (invariant 5): a small root names pages by id and digest,
//! each page lists files by object id, length and digest.
use crate::codec::{Reader, Writer};
use crate::envelope::{Kind, open};
use crate::limits::*;
use crate::{CommitToken, Digest, FormatError, Id, Result};
use sha2::{Digest as _, Sha256};

/// One file of a published segment set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    /// Engine file name: non-empty, at most [`MAX_FILE_NAME_BYTES`], no `/`, NUL, `.` or `..`.
    pub name: String,
    /// The object holding the bytes.
    pub object: Id,
    /// Exact length in bytes.
    pub length: u64,
    /// SHA-256 of the file bytes.
    pub digest: Digest,
}

/// An immutable page of at most [`MAX_PAGE_ENTRIES`] files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestPage {
    /// Files in name order, names unique.
    pub entries: Vec<FileEntry>,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_FILE_NAME_BYTES
        && n != "."
        && n != ".."
        && !n.bytes().any(|b| b == b'/' || b == 0)
}

impl ManifestPage {
    /// Append the encoded page to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut w = Writer::begin(out, Kind::ManifestPage, 1, 0);
        if let Err(e) = w.count(self.entries.len(), MAX_PAGE_ENTRIES, "page entries") {
            return w.abort(e);
        }
        for (i, e) in self.entries.iter().enumerate() {
            let sorted = i == 0 || self.entries.get(i - 1).is_some_and(|p| p.name < e.name);
            if !valid_name(&e.name) || !sorted {
                return w.abort(FormatError::Malformed("file name"));
            }
            w.u16(u16::try_from(e.name.len()).unwrap_or(u16::MAX));
            w.bytes(e.name.as_bytes());
            w.bytes(&e.object);
            w.u64(e.length);
            w.bytes(&e.digest);
        }
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::ManifestPage)?;
        let mut r = Reader::new(body);
        // Smallest entry: name length 2 + one name byte + id 16 + length 8 + digest 32.
        let n = r.count(MAX_PAGE_ENTRIES, 59, "page entries")?;
        let mut entries: Vec<FileEntry> = Vec::with_capacity(n);
        for _ in 0..n {
            let len = usize::from(r.u16()?);
            if len > MAX_FILE_NAME_BYTES {
                return Err(FormatError::TooLarge("file name"));
            }
            let name = std::str::from_utf8(r.take(len)?)
                .map_err(|_| FormatError::Malformed("file name"))?;
            if !valid_name(name) || entries.last().is_some_and(|p| p.name.as_str() >= name) {
                return Err(FormatError::Malformed("file name"));
            }
            entries.push(FileEntry {
                name: name.to_owned(),
                object: r.id()?,
                length: r.u64()?,
                digest: r.array()?,
            });
        }
        r.finish()?;
        Ok(Self { entries })
    }
}

/// A root's reference to one page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRef {
    /// Page object id.
    pub page: Id,
    /// Entries the page must hold.
    pub entry_count: u32,
    /// SHA-256 of the page's whole encoded object.
    pub digest: Digest,
}

/// The small root published by conditional write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestRoot {
    /// Starts at 1, one higher per publication.
    pub generation: u64,
    /// The index the manifest belongs to.
    pub index: Id,
    /// Engine file-format label the files open under.
    pub engine_format: String,
    /// Content hash of the mapping the files were written under (PR-43, PR-44).
    pub mapping: Digest,
    /// Slots below this ordinal are contained in the files (the checkpoint).
    pub covered: u64,
    /// The crypto envelope the objects depend on.
    pub crypto_envelope: Id,
    /// Pages, at most [`MAX_ROOT_PAGES`].
    pub pages: Vec<PageRef>,
    /// Unique per commit (M03.11).
    pub token: CommitToken,
}

impl ManifestRoot {
    /// Append the encoded root to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.generation == 0 || self.engine_format.is_empty() {
            return Err(FormatError::Malformed("root generation or engine format"));
        }
        let mut w = Writer::begin(out, Kind::ManifestRoot, 1, 0);
        w.u64(self.generation);
        w.bytes(&self.index);
        if let Err(e) = w.short_str(&self.engine_format, MAX_NAME_BYTES, "engine format") {
            return w.abort(e);
        }
        w.bytes(&self.mapping);
        w.u64(self.covered);
        w.bytes(&self.crypto_envelope);
        if let Err(e) = w.count(self.pages.len(), MAX_ROOT_PAGES, "root pages") {
            return w.abort(e);
        }
        for p in &self.pages {
            if p.entry_count == 0 || p.entry_count as usize > MAX_PAGE_ENTRIES {
                return w.abort(FormatError::Malformed("page entry count"));
            }
            w.bytes(&p.page);
            w.u32(p.entry_count);
            w.bytes(&p.digest);
        }
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::ManifestRoot)?;
        let mut r = Reader::new(body);
        let generation = r.u64()?;
        let index = r.id()?;
        let engine_format = r.short_str(MAX_NAME_BYTES, "engine format")?.to_owned();
        let mapping = r.array()?;
        let covered = r.u64()?;
        let crypto_envelope = r.id()?;
        let n = r.count(MAX_ROOT_PAGES, 52, "root pages")?;
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            let page = PageRef {
                page: r.id()?,
                entry_count: r.u32()?,
                digest: r.array()?,
            };
            if page.entry_count == 0 || page.entry_count as usize > MAX_PAGE_ENTRIES {
                return Err(FormatError::Malformed("page entry count"));
            }
            pages.push(page);
        }
        let token = CommitToken::new(r.array()?)?;
        r.finish()?;
        if generation == 0 || engine_format.is_empty() {
            return Err(FormatError::Malformed("root generation or engine format"));
        }
        Ok(Self {
            generation,
            index,
            engine_format,
            mapping,
            covered,
            crypto_envelope,
            pages,
            token,
        })
    }

    /// Decode and require the index the key named.
    pub fn decode_for(bytes: &[u8], index: &Id) -> Result<Self> {
        let r = Self::decode(bytes)?;
        if &r.index != index {
            return Err(FormatError::Mismatch("manifest index"));
        }
        Ok(r)
    }

    /// Check fetched page bytes against reference `i`: the digest covers the whole object, then
    /// the entry count must match. The provider's ETag plays no part.
    pub fn verify_page(&self, i: usize, page_bytes: &[u8]) -> Result<ManifestPage> {
        let want = self
            .pages
            .get(i)
            .ok_or(FormatError::Mismatch("page index"))?;
        if Sha256::digest(page_bytes).as_slice() != want.digest {
            return Err(FormatError::Checksum);
        }
        let page = ManifestPage::decode(page_bytes)?;
        if page.entries.len() != want.entry_count as usize {
            return Err(FormatError::Mismatch("page entry count"));
        }
        Ok(page)
    }
}
