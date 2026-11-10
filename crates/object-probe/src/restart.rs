//! Same-container, same-volume restart model. This is not power-loss proof.
use crate::model::{Capture, CapturePlan};
use std::collections::BTreeMap;

/// Refuse missing or changed independently authenticated host restart evidence.
pub fn verify(plan: &CapturePlan, capture: &Capture, expected: &str) -> bool {
    if expected.len() > 1024 || !expected.starts_with('{') || !expected.ends_with('}') {
        return false;
    }
    let mut values = BTreeMap::new();
    for pair in expected[1..expected.len() - 1].split(',') {
        let Some((key, value)) = pair.split_once(':') else {
            return false;
        };
        let (Some(key), Some(value)) = (
            key.strip_prefix('"').and_then(|k| k.strip_suffix('"')),
            value.strip_prefix('"').and_then(|v| v.strip_suffix('"')),
        ) else {
            return false;
        };
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || values.insert(key, value).is_some()
        {
            return false;
        }
    }
    const KEYS: [&str; 10] = [
        "before_boot",
        "after_boot",
        "before_cid",
        "after_cid",
        "before_image",
        "after_image",
        "before_volume",
        "after_volume",
        "before_config",
        "after_config",
    ];
    if values.len() != KEYS.len() || KEYS.iter().any(|key| !values.contains_key(key)) {
        return false;
    }
    if values["before_boot"] == values["after_boot"] {
        return false;
    }
    for (before, after) in [
        ("before_cid", "after_cid"),
        ("before_image", "after_image"),
        ("before_volume", "after_volume"),
        ("before_config", "after_config"),
    ] {
        if values[before] != values[after] {
            return false;
        }
    }
    let markers: Vec<_> = plan
        .specifications
        .iter()
        .filter(|s| s.action == "restart")
        .collect();
    if markers.len() != 1 {
        return false;
    }
    let marker = markers[0];
    let Some(receipt) = capture.events.iter().find(|e| e.id == marker.id) else {
        return false;
    };
    if receipt.boundary != "host-restart"
        || receipt.auxiliary != expected
        || receipt.sent != 0
        || receipt.received != 0
        || receipt.status != 0
    {
        return false;
    }
    let Some(original) = capture.events.iter().find(|e| e.id == marker.parent) else {
        return false;
    };
    let Some(after) = plan
        .specifications
        .iter()
        .find(|s| s.scenario == 4 && s.action == "head")
    else {
        return false;
    };
    let Some(observed) = capture.events.iter().find(|e| e.id == after.id) else {
        return false;
    };
    original.end <= receipt.start
        && receipt.end <= observed.start
        && observed.status == 200
        && !original.version.is_empty()
        && original.version != "null"
        && observed.version == original.version
        && observed.etag == original.etag
        && observed.metadata.split('|').next() == Some(original.request.as_str())
}
