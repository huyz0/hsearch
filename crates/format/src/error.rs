use std::fmt;

/// Why bytes were refused. Carries no payload, key or token text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    /// Fewer bytes than the header or a declared length requires.
    Truncated,
    /// Bytes after the declared end of the object.
    Trailing,
    /// The magic is not an hsearch object.
    BadMagic,
    /// A checksum or digest does not match.
    Checksum,
    /// The envelope layout version is not understood.
    UnsupportedEnvelope(u16),
    /// The object kind is not understood.
    UnknownKind(u16),
    /// The kind's own version is newer than this reader (or zero).
    UnknownVersion {
        /// Object kind code.
        kind: u16,
        /// The version found.
        version: u16,
    },
    /// A mandatory capability bit this reader does not implement; the mask is the unknown bits.
    UnknownMandatoryCapability(u64),
    /// A length, count or total exceeds the named bound.
    TooLarge(&'static str),
    /// A field is outside its allowed set or violates an invariant.
    Malformed(&'static str),
    /// The object is valid but is not the kind or identity expected.
    Mismatch(&'static str),
}

/// Result of a format operation.
pub type Result<T> = std::result::Result<T, FormatError>;

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated object"),
            Self::Trailing => f.write_str("trailing bytes after object"),
            Self::BadMagic => f.write_str("not an hsearch object"),
            Self::Checksum => f.write_str("checksum mismatch"),
            Self::UnsupportedEnvelope(v) => write!(f, "unsupported envelope version {v}"),
            Self::UnknownKind(k) => write!(f, "unknown object kind {k}"),
            Self::UnknownVersion { kind, version } => {
                write!(f, "unknown version {version} of kind {kind}")
            }
            Self::UnknownMandatoryCapability(m) => {
                write!(f, "unknown mandatory capabilities {m:#x}")
            }
            Self::TooLarge(what) => write!(f, "{what} exceeds its bound"),
            Self::Malformed(what) => write!(f, "malformed: {what}"),
            Self::Mismatch(what) => write!(f, "mismatch: {what}"),
        }
    }
}

impl std::error::Error for FormatError {}
