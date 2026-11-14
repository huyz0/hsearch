//! The 40-byte envelope every object starts with.
use crate::codec::Writer;
use crate::crc::crc32c;
use crate::limits::*;
use crate::{FormatError, Result};

pub(crate) const MAGIC: [u8; 4] = *b"HSFO";
pub(crate) const ENVELOPE_VERSION: u16 = 1;
pub(crate) const HEADER_LEN: usize = 40;

/// Mandatory capability bits this build implements. Bit 0 is reserved for an AEAD-wrapped body
/// (M05) and is **not** implemented, so a body carrying it is refused, not passed through.
pub const SUPPORTED_MANDATORY_CAPS: u64 = 0;

/// What an object is. Codes are stable and never reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// Fleet format-version record.
    Fleet = 1,
    /// A lane's structural descriptor.
    LaneDescriptor = 2,
    /// One ordered lane slot: data or fence.
    LaneSlot = 3,
    /// Small manifest root.
    ManifestRoot = 4,
    /// Bounded manifest page.
    ManifestPage = 5,
    /// Content-addressed mapping.
    Mapping = 6,
    /// Durable pin.
    Pin = 7,
    /// Bounded cursor token.
    Cursor = 8,
}

impl Kind {
    pub(crate) fn code(self) -> u16 {
        self as u16
    }

    fn from_code(code: u16) -> Result<Self> {
        Ok(match code {
            1 => Self::Fleet,
            2 => Self::LaneDescriptor,
            3 => Self::LaneSlot,
            4 => Self::ManifestRoot,
            5 => Self::ManifestPage,
            6 => Self::Mapping,
            7 => Self::Pin,
            8 => Self::Cursor,
            _ => return Err(FormatError::UnknownKind(code)),
        })
    }

    /// Largest body this kind may carry, checked from the header before the body is read.
    #[must_use]
    pub fn max_body(self) -> usize {
        match self {
            Self::Fleet => MAX_FLEET_BODY,
            Self::LaneDescriptor => MAX_LANE_DESCRIPTOR_BODY,
            Self::LaneSlot => MAX_SLOT_BODY_BYTES,
            Self::ManifestRoot => MAX_ROOT_BODY,
            Self::ManifestPage => MAX_PAGE_BODY,
            Self::Mapping => MAX_MAPPING_BODY,
            Self::Pin => MAX_PIN_BODY,
            Self::Cursor => MAX_CURSOR_BODY,
        }
    }

    /// Newest per-kind layout version this build reads and writes.
    #[must_use]
    pub fn current_version(self) -> u16 {
        1
    }
}

/// A validated header. Obtain it from the first [`Header::LEN`] bytes of an object to learn
/// the exact object length before reading or allocating the body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    /// What the object is.
    pub kind: Kind,
    /// The kind's layout version.
    pub kind_version: u16,
    /// Mandatory capabilities the writer used.
    pub mandatory_caps: u64,
    /// Body length in bytes, already bounded for the kind.
    pub body_len: u32,
    body_crc: u32,
}

impl Header {
    /// Header length in bytes.
    pub const LEN: usize = HEADER_LEN;

    /// Validate the first [`Header::LEN`] bytes. Extra bytes are ignored here.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let Some(h) = bytes.get(..HEADER_LEN) else {
            return Err(FormatError::Truncated);
        };
        let word = |a: usize, n: usize| -> u64 {
            let mut v = 0u64;
            for (i, b) in h.get(a..a + n).unwrap_or_default().iter().enumerate() {
                v |= u64::from(*b) << (8 * i);
            }
            v
        };
        if h.get(0..4) != Some(&MAGIC[..]) {
            return Err(FormatError::BadMagic);
        }
        // Version first: a future layout may not keep the checksum where this one does.
        let env = word(4, 2) as u16;
        if env != ENVELOPE_VERSION {
            return Err(FormatError::UnsupportedEnvelope(env));
        }
        if crc32c(h.get(..36).unwrap_or_default()) != word(36, 4) as u32 {
            return Err(FormatError::Checksum);
        }
        // Flags and optional capabilities are ignorable by definition: a reader that does not know
        // a bit loses nothing it needs, so a newer writer may set them without a fleet gate.
        let code = word(6, 2) as u16;
        let kind = Kind::from_code(code)?;
        let kind_version = word(8, 2) as u16;
        if kind_version == 0 || kind_version > kind.current_version() {
            return Err(FormatError::UnknownVersion {
                kind: code,
                version: kind_version,
            });
        }
        let mandatory_caps = word(12, 8);
        let unknown = mandatory_caps & !SUPPORTED_MANDATORY_CAPS;
        if unknown != 0 {
            return Err(FormatError::UnknownMandatoryCapability(unknown));
        }
        let body_len = word(28, 4) as u32;
        if body_len as usize > kind.max_body() {
            return Err(FormatError::TooLarge("body"));
        }
        Ok(Self {
            kind,
            kind_version,
            mandatory_caps,
            body_len,
            body_crc: word(32, 4) as u32,
        })
    }

    /// Header plus body, the exact length of the object.
    #[must_use]
    pub fn total_len(&self) -> usize {
        HEADER_LEN + self.body_len as usize
    }
}

/// Validate a whole object of the expected `kind` and return its body, which is covered by the
/// body checksum. The input must be exactly one object.
pub fn open(bytes: &[u8], expect: Kind) -> Result<(Header, &[u8])> {
    let header = Header::parse(bytes)?;
    if header.kind != expect {
        return Err(FormatError::Mismatch("object kind"));
    }
    let total = header.total_len();
    if bytes.len() < total {
        return Err(FormatError::Truncated);
    }
    if bytes.len() > total {
        return Err(FormatError::Trailing);
    }
    let body = bytes.get(HEADER_LEN..).unwrap_or_default();
    if crc32c(body) != header.body_crc {
        return Err(FormatError::Checksum);
    }
    Ok((header, body))
}

#[doc(hidden)]
/// Append an envelope around caller-supplied body bytes. For tests and fixtures that need to
/// forge a valid checksum around an invalid body; the typed `encode` methods are the normal path.
pub fn seal(
    out: &mut Vec<u8>,
    kind: Kind,
    kind_version: u16,
    mandatory_caps: u64,
    body: &[u8],
) -> Result<()> {
    let mut w = Writer::begin(out, kind, kind_version, mandatory_caps);
    w.bytes(body);
    w.finish()
}
