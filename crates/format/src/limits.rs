//! Every bound in one place. Each is checked before allocation.

/// Largest document, in bytes (PR-40). A document above the inline bound lives in its own
/// object and a slot carries only a reference.
pub const MAX_DOCUMENT_BYTES: u64 = 200 * 1024 * 1024;
/// Largest payload carried inside a lane slot.
pub const MAX_INLINE_RECORD_BYTES: usize = 1024 * 1024;
/// Largest encoded lane slot body; equal to the v0 slot ceiling and below the object contract bound.
pub const MAX_SLOT_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Most records in one slot.
pub const MAX_SLOT_RECORDS: usize = 4096;
/// Default number of indexes sharing a lane (M03.09 decision 4).
pub const DEFAULT_LANE_GROUP: u32 = 1000;
/// Absolute ceiling a lane's group size may be configured to.
pub const MAX_LANE_GROUP: u32 = 4096;
/// Longest BYOK key identifier (a cloud KMS resource name).
pub const MAX_KEY_ID_BYTES: usize = 512;
/// Longest residency or engine-format string.
pub const MAX_NAME_BYTES: usize = 64;
/// Most files one manifest page lists.
pub const MAX_PAGE_ENTRIES: usize = 2048;
/// Most pages one manifest root references.
pub const MAX_ROOT_PAGES: usize = 4096;
/// Longest file name in a manifest.
pub const MAX_FILE_NAME_BYTES: usize = 255;
/// Largest canonical mapping body (proposed; PR-42 owns the final limit).
pub const MAX_MAPPING_BYTES: usize = 1024 * 1024;
/// Most shard positions in an inline cursor.
pub const MAX_CURSOR_ENTRIES: usize = 32;
/// Largest object key the layout may produce.
pub const MAX_KEY_BYTES: usize = 1024;
/// Longest deployment name in a key.
pub const MAX_DEPLOYMENT_BYTES: usize = 63;

pub(crate) const MAX_FLEET_BODY: usize = 4 * 1024;
pub(crate) const MAX_LANE_DESCRIPTOR_BODY: usize = 128 * 1024;
pub(crate) const MAX_ROOT_BODY: usize = 512 * 1024;
pub(crate) const MAX_PAGE_BODY: usize = 1024 * 1024;
pub(crate) const MAX_MAPPING_BODY: usize = MAX_MAPPING_BYTES + 32;
pub(crate) const MAX_PIN_BODY: usize = 1024;
pub(crate) const MAX_CURSOR_BODY: usize = 2 * 1024;
