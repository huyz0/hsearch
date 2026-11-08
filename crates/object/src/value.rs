use crate::{Error, Precondition};

macro_rules! bounded {
    ($name:ident, $max:expr, $empty:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub struct $name(Vec<u8>);

        impl $name {
            /// Copy a value only after checking its byte bound.
            pub fn new(value: &[u8]) -> Result<Self, Error> {
                if value.len() > $max || (!$empty && value.is_empty()) {
                    return Err(Error::InvalidInput);
                }
                Ok(Self(value.to_vec()))
            }

            /// Return the uninterpreted bytes. These bytes may be sensitive.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }

            /// Return the bounded byte length.
            #[must_use]
            pub fn len(&self) -> usize {
                self.0.len()
            }

            /// Whether this value is empty.
            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name)).field("bytes", &self.0.len()).finish()
            }
        }
    };
}

bounded!(ObjectKey, 1024, false, "Opaque object key, bounded to 1024 bytes.");
bounded!(RequestId, 128, false, "Opaque mutation identity, bounded to 128 bytes.");
bounded!(VersionToken, 256, false, "Equality-only provider version, bounded to 256 bytes.");
bounded!(Payload, 16 * 1024 * 1024, true, "Payload with an absolute 16 MiB allocation bound.");
bounded!(Metadata, 8192, true, "Opaque metadata with an absolute 8 KiB allocation bound.");

/// A nonempty half-open byte range, checked before arithmetic or allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    offset: u64,
    length: u64,
}

impl ByteRange {
    /// Validate nonempty length and checked offset plus length.
    pub fn new(offset: u64, length: u64) -> Result<Self, Error> {
        if length == 0 || offset.checked_add(length).is_none() {
            return Err(Error::InvalidRange);
        }
        Ok(Self { offset, length })
    }

    /// First byte in the selected immutable version.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Exact requested byte count.
    #[must_use]
    pub const fn length(self) -> u64 {
        self.length
    }

    /// Reject ranges beyond the object's full length.
    pub fn validate(self, object_length: u64) -> Result<(), Error> {
        if self.offset + self.length > object_length {
            return Err(Error::InvalidRange);
        }
        Ok(())
    }
}

/// An immutable request envelope; retries reuse the entire envelope unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mutation {
    /// Stable caller-selected request identity.
    pub request_id: RequestId,
    /// Selected opaque object key.
    pub key: ObjectKey,
    /// Exact bytes to create or conditionally replace.
    pub payload: Payload,
    /// Metadata stored atomically with the bytes.
    pub metadata: Metadata,
    /// Required atomic condition; unconditional writes cannot be expressed.
    pub precondition: Precondition,
}
