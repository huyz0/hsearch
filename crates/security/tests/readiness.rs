//! M05.04 readiness: every persistent data path is declared and protected, production refuses to start
//! otherwise, and the at-rest claim cannot say "full" while engine scratch is plaintext.

use std::collections::BTreeMap;

use hsearch_security::keys::Profile;
use hsearch_security::readiness::{
    ALL_PATHS, AtRestClaim, DataPath, Declaration, NotReady, Protection, VolumeProbe, VolumeState,
    enforce, evaluate,
};

struct Probe(BTreeMap<&'static str, VolumeState>);

impl VolumeProbe for Probe {
    fn probe(&self, location: &str) -> VolumeState {
        self.0
            .get(location)
            .cloned()
            .unwrap_or(VolumeState::Unknown)
    }
}

fn decl(path: DataPath, location: &str, protection: Protection) -> Declaration {
    Declaration {
        path,
        location: location.into(),
        protection,
    }
}

/// A fully protected node: objects, journal and cache sealed; scratch on verified encrypted volumes.
fn good() -> (Vec<Declaration>, Probe) {
    (
        vec![
            decl(DataPath::ObjectData, "bucket/v1", Protection::Sealed),
            decl(DataPath::Journal, "/var/hs/journal", Protection::Sealed),
            decl(
                DataPath::PersistentCache,
                "/var/hs/cache",
                Protection::Sealed,
            ),
            decl(DataPath::ReplaySpool, "/var/hs/spool", Protection::Sealed),
            decl(
                DataPath::EngineScratch,
                "/mnt/enc/scratch",
                Protection::EncryptedVolume,
            ),
            decl(
                DataPath::BuildScratch,
                "/mnt/enc/build",
                Protection::EncryptedVolume,
            ),
        ],
        Probe(BTreeMap::from([
            ("/mnt/enc/scratch", VolumeState::Encrypted),
            ("/mnt/enc/build", VolumeState::Encrypted),
        ])),
    )
}

fn replace(
    mut d: Vec<Declaration>,
    path: DataPath,
    location: &str,
    protection: Protection,
) -> Vec<Declaration> {
    d.retain(|x| x.path != path);
    d.push(decl(path, location, protection));
    d
}

#[test]
fn a_fully_protected_node_may_claim_full_at_rest_protection() {
    let (d, p) = good();
    let report = enforce(Profile::Production, &d, &p).unwrap();
    assert!(report.findings.is_empty());
    assert_eq!(report.claim, AtRestClaim::Full);
}

#[test]
fn production_refuses_to_start_when_engine_scratch_is_plaintext() {
    let (d, mut p) = good();
    p.0.insert("/mnt/enc/scratch", VolumeState::Plaintext);
    let NotReady(findings) = enforce(Profile::Production, &d, &p).unwrap_err();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].path, DataPath::EngineScratch);
    assert_eq!(findings[0].location, "/mnt/enc/scratch");
}

#[test]
fn a_claim_of_an_encrypted_volume_is_not_taken_on_trust() {
    // Declared encrypted, but the probe cannot tell: that is unprotected, never "probably fine".
    let (d, mut p) = good();
    p.0.remove("/mnt/enc/build");
    let err = enforce(Profile::Production, &d, &p).unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].path, DataPath::BuildScratch);
    // Declared plaintext is refused without needing a probe.
    let d = replace(
        d,
        DataPath::EngineScratch,
        "/tmp/scratch",
        Protection::Plaintext,
    );
    let err = enforce(Profile::Production, &d, &p).unwrap_err();
    assert_eq!(err.0.len(), 2);
}

#[test]
fn engine_scratch_cannot_be_declared_sealed() {
    // The engine writes ordinary filesystem bytes; no application format can cover them.
    let (d, p) = good();
    for path in [DataPath::EngineScratch, DataPath::BuildScratch] {
        let d = replace(d.clone(), path, "/mnt/enc/x", Protection::Sealed);
        let err = enforce(Profile::Production, &d, &p).unwrap_err();
        assert_eq!(err.0.len(), 1, "{path:?}");
        assert_eq!(err.0[0].path, path);
    }
}

#[test]
fn a_path_nobody_declared_is_a_finding_not_an_assumption() {
    let (d, p) = good();
    for gone in ALL_PATHS {
        let mut d = d.clone();
        d.retain(|x| x.path != gone);
        let err = enforce(Profile::Production, &d, &p).unwrap_err();
        assert_eq!(err.0.len(), 1, "{gone:?}");
        assert_eq!(err.0[0].path, gone);
        assert!(err.0[0].location.is_empty());
    }
}

#[test]
fn several_locations_for_one_path_are_each_checked() {
    let (mut d, mut p) = good();
    d.push(decl(
        DataPath::EngineScratch,
        "/mnt/other",
        Protection::EncryptedVolume,
    ));
    p.0.insert("/mnt/other", VolumeState::Plaintext);
    let err = enforce(Profile::Production, &d, &p).unwrap_err();
    assert_eq!(err.0.len(), 1);
    assert_eq!(err.0[0].location, "/mnt/other");
}

#[test]
fn development_reports_what_production_would_refuse() {
    let (d, mut p) = good();
    p.0.insert("/mnt/enc/scratch", VolumeState::Plaintext);
    let report = enforce(Profile::Development, &d, &p).unwrap();
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.claim, AtRestClaim::ObjectsOnly);
}

#[test]
fn plaintext_scratch_caps_the_claim_at_objects_only() {
    // The definition of done: object encryption is never "full at-rest protection" while scratch is plain.
    let (d, p) = good();
    let d = replace(d, DataPath::EngineScratch, "/tmp/s", Protection::Plaintext);
    let report = evaluate(Profile::Development, &d, &p);
    assert_eq!(report.claim, AtRestClaim::ObjectsOnly);
    // An unprotected journal also caps it.
    let (d, p) = good();
    let d = replace(d, DataPath::Journal, "/tmp/j", Protection::Plaintext);
    assert_eq!(
        evaluate(Profile::Development, &d, &p).claim,
        AtRestClaim::ObjectsOnly
    );
}

#[test]
fn every_path_class_is_listed_once() {
    let mut seen = std::collections::BTreeSet::new();
    for p in ALL_PATHS {
        assert!(seen.insert(p));
    }
    assert_eq!(seen.len(), 6);
}

#[test]
fn unprotected_objects_are_no_claim_at_all() {
    let (d, p) = good();
    let d = replace(d, DataPath::ObjectData, "bucket", Protection::Plaintext);
    assert_eq!(
        evaluate(Profile::Development, &d, &p).claim,
        AtRestClaim::None
    );
    // A node with nothing declared claims nothing and has six findings.
    let report = evaluate(Profile::Development, &[], &p);
    assert_eq!(
        (report.claim, report.findings.len()),
        (AtRestClaim::None, 6)
    );
}
