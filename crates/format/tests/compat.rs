//! M03.06: the format compatibility matrix. Every object kind is held to the same promises, and
//! `docs/plan/format-compatibility.md` lists exactly the kinds that exist.
mod common;
use common::patch_header;
use hsearch_format::{FormatError, Header, Kind, open, seal};

/// Every kind. The `match` below stops compiling when a kind is added without being listed here.
const ALL: [Kind; 12] = [
    Kind::Fleet,
    Kind::LaneDescriptor,
    Kind::LaneSlot,
    Kind::ManifestRoot,
    Kind::ManifestPage,
    Kind::Mapping,
    Kind::Pin,
    Kind::Cursor,
    Kind::StructuralHead,
    Kind::CatalogRoot,
    Kind::CatalogPage,
    Kind::IdentityBinding,
];

fn listed(kind: Kind) -> bool {
    match kind {
        Kind::Fleet
        | Kind::LaneDescriptor
        | Kind::LaneSlot
        | Kind::ManifestRoot
        | Kind::ManifestPage
        | Kind::Mapping
        | Kind::Pin
        | Kind::Cursor
        | Kind::StructuralHead
        | Kind::CatalogRoot
        | Kind::CatalogPage
        | Kind::IdentityBinding => ALL.contains(&kind),
    }
}

fn sealed(kind: Kind, version: u16, caps: u64) -> Vec<u8> {
    let mut out = Vec::new();
    seal(&mut out, kind, version, caps, b"body").unwrap();
    out
}

#[test]
fn every_kind_is_listed_and_codes_are_dense() {
    assert!(ALL.iter().all(|k| listed(*k)));
    for (i, kind) in ALL.iter().enumerate() {
        assert_eq!(*kind as usize, i + 1, "codes are stable and never reused");
    }
    // The code after the last is unknown, so a new kind cannot arrive without a row here.
    let next = patch_header(&sealed(Kind::Fleet, 1, 0), 6, &13u16.to_le_bytes());
    assert_eq!(
        Header::parse(&next).unwrap_err(),
        FormatError::UnknownKind(13)
    );
}

#[test]
fn each_kind_reads_its_own_version_and_refuses_the_rest() {
    for kind in ALL {
        let v = kind.current_version();
        let object = sealed(kind, v, 0);
        let (header, body) = open(&object, kind).unwrap();
        assert_eq!(
            (header.kind, header.kind_version, body),
            (kind, v, &b"body"[..])
        );
        // A newer layout is refused, never read as the current one.
        assert_eq!(
            open(&sealed(kind, v + 1, 0), kind).unwrap_err(),
            FormatError::UnknownVersion {
                kind: kind as u16,
                version: v + 1
            },
            "{kind:?}"
        );
        // Version zero was never written.
        assert!(open(&sealed(kind, 0, 0), kind).is_err(), "{kind:?}");
        // An unknown mandatory capability is refused; the next unknown bit is no different.
        assert_eq!(
            open(&sealed(kind, v, 1 << 40), kind).unwrap_err(),
            FormatError::UnknownMandatoryCapability(1 << 40),
            "{kind:?}"
        );
        // A body above the kind's bound is refused from the header alone.
        let big = vec![0u8; kind.max_body() + 1];
        let mut out = Vec::new();
        let sealed_big = seal(&mut out, kind, v, 0, &big);
        assert!(
            sealed_big.is_err() || open(&out, kind).is_err(),
            "{kind:?} accepted an oversized body"
        );
    }
}

#[test]
fn no_kind_reads_another_kinds_object() {
    for written in ALL {
        let bytes = sealed(written, written.current_version(), 0);
        for expected in ALL {
            let got = open(&bytes, expected);
            assert_eq!(
                got.is_ok(),
                written == expected,
                "{written:?} as {expected:?}"
            );
            if written != expected {
                assert_eq!(got.unwrap_err(), FormatError::Mismatch("object kind"));
            }
        }
    }
}

#[test]
fn the_compatibility_document_matches_the_kinds() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/plan/format-compatibility.md"
    ))
    .unwrap();
    let rows: Vec<Vec<&str>> = doc
        .lines()
        .filter(|l| l.starts_with("| ") && l.as_bytes().get(2).is_some_and(u8::is_ascii_digit))
        .map(|l| l.trim_matches('|').split('|').map(str::trim).collect())
        .collect();
    assert_eq!(rows.len(), ALL.len(), "one row per kind");
    for (row, kind) in rows.iter().zip(ALL) {
        assert_eq!(row[0], (kind as u16).to_string());
        assert_eq!(row[1], format!("{kind:?}"));
        assert_eq!(row[2], kind.current_version().to_string(), "{kind:?}");
        assert_eq!(row[3], kind.max_body().to_string(), "{kind:?} body bound");
    }
}
