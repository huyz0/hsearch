//! The lane's durable reclaim cover: a monotonic marker written **before** any slot below it is
//! removed. An opener checks it after replaying, so a replay start that is below the cover (a hole
//! of any length) is refused instead of read as the end of the log and fenced into the hole.
//!
//! Not a ratified format kind: a 32-byte record (magic, cover, token, CRC-32C) kept here so the
//! rule is enforced now. The cover belongs in the structural head (M03.05), which replaces this
//! object; until then it is the one extra object a lane keeps, written once per reclaim and read
//! (one HEAD, plus one GET when it exists) once per open.
use hsearch_format::{FormatError, Id, Layout, crc32c};
use hsearch_object::{MetadataStore, Precondition, VersionToken};

use crate::io::{Written, fresh_token, key, mutation, read_object, request_id, write};
use crate::members::LaneId;
use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"HSLC";
const LEN: usize = 32;
const MAX_ATTEMPTS: u32 = 8;

fn cover_key(layout: &Layout, id: &LaneId) -> String {
    let head = layout.lane_head(&id.domain, id.incarnation);
    format!("{}COVER", head.strip_suffix("HEAD").unwrap_or(&head))
}

fn encode(cover: u64, token: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&cover.to_le_bytes());
    out.extend_from_slice(token);
    let crc = crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

fn decode(bytes: &[u8]) -> Result<u64> {
    let bad = Error::Format(FormatError::Malformed("lane cover"));
    if bytes.len() != LEN || bytes.get(..4) != Some(MAGIC) {
        return Err(bad);
    }
    let (body, crc) = bytes.split_at(LEN - 4);
    if crc32c(body).to_le_bytes() != *crc {
        return Err(Error::Format(FormatError::Checksum));
    }
    let raw: [u8; 8] = bytes.get(4..12).ok_or(bad)?.try_into().map_err(|_| bad)?;
    Ok(u64::from_le_bytes(raw))
}

/// The durable cover (0 when none was ever written) and the version a raise must name.
pub(crate) async fn read_cover<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
) -> Result<(u64, Option<VersionToken>)> {
    let k = key(&cover_key(layout, id))?;
    match read_object(store, &k, 256).await? {
        None => Ok((0, None)),
        Some((bytes, version)) => Ok((decode(bytes.as_bytes())?, Some(version))),
    }
}

/// Raise the cover to at least `cover`. A concurrent raiser that got further is kept.
pub(crate) async fn raise_cover<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    cover: u64,
) -> Result<()> {
    let _: &Id = &id.domain;
    for _ in 0..MAX_ATTEMPTS {
        let (current, version) = read_cover(store, layout, id).await?;
        if current >= cover {
            return Ok(());
        }
        let token = fresh_token();
        let bytes = encode(cover, token.as_bytes());
        let request = mutation(
            &request_id("cv", &id.domain, cover, &token),
            &cover_key(layout, id),
            &bytes,
            version.map_or(Precondition::Absent, Precondition::Matches),
        )?;
        let replace = request.precondition != Precondition::Absent;
        match write(store, &request, replace).await {
            Written::Done(_) => return Ok(()),
            Written::Occupied => {}
            Written::Refused(error) => return Err(error.into()),
            Written::Unknown => return Err(Error::Contended),
        }
    }
    Err(Error::Contended)
}
