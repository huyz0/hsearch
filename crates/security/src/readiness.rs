//! Persistent-data readiness (M05.04): every place the node persists bytes is declared, each declaration
//! is checked, and the claim a deployment may make about encryption at rest follows from the result.
//!
//! Object, journal, cache and spool bytes can be protected by the sealed block format. Engine and index-build
//! scratch cannot: the engine writes and maps ordinary filesystem files, so only an encrypted volume or
//! directory covers them, and a volume claim is only believed when a [`VolumeProbe`] confirms it. A path
//! nobody declared is a finding, not an assumption. Production refuses to start on any finding; development
//! reports them. WSL and Docker cannot certify an encrypted volume, so a probe there answers `Unknown`.

use crate::keys::Profile;

/// Every place the node may persist bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DataPath {
    /// Immutable objects in the object store.
    ObjectData,
    /// The local journal.
    Journal,
    /// The persistent cache.
    PersistentCache,
    /// The replay spool.
    ReplaySpool,
    /// Engine segment scratch.
    EngineScratch,
    /// Index build and merge scratch.
    BuildScratch,
}

/// Every data path, so none can be forgotten.
pub const ALL_PATHS: [DataPath; 6] = [
    DataPath::ObjectData,
    DataPath::Journal,
    DataPath::PersistentCache,
    DataPath::ReplaySpool,
    DataPath::EngineScratch,
    DataPath::BuildScratch,
];

/// How a declared location is protected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Protection {
    /// Bytes are written through the sealed block format.
    Sealed,
    /// The location is on an encrypted volume or directory.
    EncryptedVolume,
    /// Nothing protects it.
    Plaintext,
}

/// One declared location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// What persists there.
    pub path: DataPath,
    /// Where (a directory or a bucket prefix).
    pub location: String,
    /// What the operator or node claims.
    pub protection: Protection,
}

/// What a probe found for a location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeState {
    /// Verified encrypted.
    Encrypted,
    /// Verified not encrypted.
    Plaintext,
    /// Could not be determined.
    Unknown,
}

/// Looks at a location and says whether it is encrypted.
pub trait VolumeProbe {
    /// The state of `location`.
    fn probe(&self, location: &str) -> VolumeState;
}

/// One reason a configuration is not protected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// The path class.
    pub path: DataPath,
    /// The location, or empty if none was declared.
    pub location: String,
    /// Why.
    pub reason: &'static str,
}

/// What the at-rest claim may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtRestClaim {
    /// Every persistent path is protected.
    Full,
    /// Object data is protected but another path is not; never call this full at-rest protection.
    ObjectsOnly,
    /// Not even object data is protected.
    None,
}

/// The outcome of a check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Everything unprotected or unaccounted.
    pub findings: Vec<Finding>,
    /// What may be claimed.
    pub claim: AtRestClaim,
}

/// Production startup refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotReady(pub Vec<Finding>);

/// Checks declarations against a probe. Findings are ordered by path class, then declaration order.
pub fn evaluate(
    _profile: Profile,
    declarations: &[Declaration],
    probe: &dyn VolumeProbe,
) -> Report {
    let mut findings = Vec::new();
    for path in ALL_PATHS {
        let mut declared = false;
        for d in declarations.iter().filter(|d| d.path == path) {
            declared = true;
            let reason = match (&d.protection, scratch(path)) {
                (Protection::Plaintext, _) => Some("plaintext"),
                (Protection::Sealed, true) => {
                    Some("the engine writes ordinary files; only an encrypted volume covers them")
                }
                (Protection::Sealed, false) => None,
                (Protection::EncryptedVolume, _) => match probe.probe(&d.location) {
                    VolumeState::Encrypted => None,
                    VolumeState::Plaintext => Some("the volume is not encrypted"),
                    VolumeState::Unknown => Some("the volume's encryption could not be verified"),
                },
            };
            if let Some(reason) = reason {
                findings.push(Finding {
                    path,
                    location: d.location.clone(),
                    reason,
                });
            }
        }
        if !declared {
            findings.push(Finding {
                path,
                location: String::new(),
                reason: "not declared: an unaccounted persistent path",
            });
        }
    }
    let claim = if findings.is_empty() {
        AtRestClaim::Full
    } else if findings.iter().any(|f| f.path == DataPath::ObjectData) {
        AtRestClaim::None
    } else {
        AtRestClaim::ObjectsOnly
    };
    Report { findings, claim }
}

/// Paths the engine writes as ordinary files, which no application format can cover.
fn scratch(path: DataPath) -> bool {
    matches!(path, DataPath::EngineScratch | DataPath::BuildScratch)
}

/// Production refuses to start on any finding; development returns the report to log.
pub fn enforce(
    profile: Profile,
    declarations: &[Declaration],
    probe: &dyn VolumeProbe,
) -> Result<Report, NotReady> {
    let report = evaluate(profile, declarations, probe);
    if profile == Profile::Production && !report.findings.is_empty() {
        return Err(NotReady(report.findings));
    }
    Ok(report)
}
