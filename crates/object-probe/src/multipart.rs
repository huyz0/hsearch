//! Independent upload/part/completion/abort/orphan model over exact observations.
use crate::codec::{payload_digest, unhex};
use crate::model::{Capture, CaptureEvent, CapturePlan, Digest, Specification};
use std::collections::BTreeMap;

fn event(capture: &Capture, id: u16) -> Option<&CaptureEvent> { capture.events.iter().find(|e| e.id == id) }

/// Whole completion oracle assembled from the final recorded replacement of
/// each planned part, preserving the original upload and descriptor identity.
pub fn completion_digest(plan: &CapturePlan, completion: &Specification, length: u64) -> Option<Digest> {
    let mut parts = BTreeMap::new();
    for spec in plan.specifications.iter().filter(|s| s.action == "part" && s.parent == completion.parent && s.id < completion.id) {
        parts.insert(spec.offset, spec);
    }
    if parts.values().map(|s| s.length).sum::<u64>() != length { return None; }
    range_digest(&parts.values().copied().collect::<Vec<_>>(), 0, length)
}

/// Independent selected-byte oracle across completed ordered part boundaries.
pub fn range_digest(parts: &[&Specification], offset: u64, length: u64) -> Option<Digest> {
    if parts.len() > 16 || length == 0 { return None; }
    let end = offset.checked_add(length)?;
    let mut cursor = 0u64;
    let mut segments = Vec::new();
    for part in parts {
        let next = cursor.checked_add(part.length)?;
        let start = offset.max(cursor);
        let stop = end.min(next);
        if start < stop { segments.push((part.seed, stop - start, start - cursor)); }
        cursor = next;
    }
    if end > cursor { return None; }
    payload_digest(&segments).ok()
}

/// Require exact upload identity, last replacement of each numbered part,
/// canonical completion order, abort behavior and scoped orphan visibility.
pub fn verify(plan: &CapturePlan, capture: &Capture) -> bool {
    let mut uploads: BTreeMap<u16, (&str, BTreeMap<u64, &Specification>, bool)> = BTreeMap::new();
    let mut completed = 0;
    let mut aborted = 0;
    let mut replacements = 0;
    let mut before_orphan = false;
    let mut after_orphan = false;
    for spec in plan.specifications.iter().filter(|s| s.scenario == 5) {
        let Some(observed) = event(capture, spec.id) else { return false; };
        match spec.action.as_str() {
            "init" => {
                if observed.status != 200 || observed.auxiliary.is_empty() || observed.auxiliary.len() > 256
                    || uploads.values().any(|(id, _, _)| *id == observed.auxiliary) { return false; }
                uploads.insert(spec.id, (&observed.auxiliary, BTreeMap::new(), false));
            }
            "part" => {
                let Some((_, parts, terminal)) = uploads.get_mut(&spec.parent) else { return false; };
                if *terminal || observed.status != 200 || observed.etag.is_empty() || !(1..=16).contains(&spec.offset) { return false; }
                if parts.insert(spec.offset, spec).is_some() { replacements += 1; }
            }
            "complete" => {
                let Some((_, parts, terminal)) = uploads.get_mut(&spec.parent) else { return false; };
                if *terminal || parts.is_empty() || observed.status != 200 || observed.etag.is_empty() || observed.condition != "*" { return false; }
                let mut descriptor = String::new();
                for (index, (number, part)) in parts.iter().enumerate() {
                    if index != 0 { descriptor.push(';'); }
                    let Some(actual) = event(capture, part.id) else { return false; };
                    descriptor.push_str(&format!("{number}:"));
                    for byte in actual.etag.bytes() { descriptor.push_str(&format!("{byte:02x}")); }
                }
                if observed.auxiliary != descriptor { return false; }
                *terminal = true;
                completed += 1;
            }
            "complete-invalid" => {
                let Some((_, _, terminal)) = uploads.get(&spec.parent) else { return false; };
                if *terminal || observed.status != 400 || observed.boundary != "reply" || observed.condition != "*" { return false; }
            }
            "abort" => {
                let Some((_, _, terminal)) = uploads.get_mut(&spec.parent) else { return false; };
                if *terminal || observed.status != 204 { return false; }
                *terminal = true;
                aborted += 1;
            }
            "parts" => {
                let Some((_, parts, terminal)) = uploads.get(&spec.parent) else { return false; };
                if *terminal {
                    if observed.status != 404 { return false; }
                } else {
                    if observed.status != 200 { return false; }
                    let fields: Vec<_> = observed.auxiliary.split(';').collect();
                    if fields.len() != parts.len() * 3 { return false; }
                    for (record, (number, part)) in fields.chunks_exact(3).zip(parts) {
                        let Some(actual) = event(capture, part.id) else { return false; };
                        let expected = [("PartNumber", number.to_string()), ("ETag", actual.etag.clone()), ("Size", part.length.to_string())];
                        for (field, (name, value)) in record.iter().zip(expected) {
                            let Some((tag, raw)) = field.split_once(':') else { return false; };
                            if tag != name || unhex(raw, 256).ok().as_deref() != Some(value.as_bytes()) { return false; }
                        }
                    }
                }
            }
            "uploads" => {
                let Some((upload, _, terminal)) = uploads.get(&spec.parent) else { return false; };
                if observed.status != 200 { return false; }
                let fields: Vec<_> = if observed.auxiliary.is_empty() { Vec::new() } else { observed.auxiliary.split(';').collect() };
                if fields.len() % 2 != 0 { return false; }
                let mut present = false;
                for pair in fields.chunks_exact(2) {
                    let Some(raw_key) = pair[0].strip_prefix("Key:") else { return false; };
                    let Some(raw_id) = pair[1].strip_prefix("UploadId:") else { return false; };
                    let Ok(key) = unhex(raw_key, 1024) else { return false; };
                    let Ok(id) = unhex(raw_id, 256) else { return false; };
                    // No foreign key/upload can be treated as this task's orphan.
                    let run: String = plan.binding.run.iter().map(|b| format!("{b:02x}")).collect();
                    if key != format!("{run}/{}", spec.key).as_bytes() { return false; }
                    if id == upload.as_bytes() { present = true; }
                }
                if *terminal { after_orphan = !present; } else { before_orphan = present; }
            }
            "range" => {
                let Some(completion) = plan.specifications.iter().find(|s| s.id == spec.parent) else { return false; };
                let Some((_, parts, terminal)) = uploads.get(&completion.parent) else { return false; };
                let part_list: Vec<_> = parts.values().copied().collect();
                if !*terminal || observed.status != 206 || range_digest(&part_list, spec.offset, spec.range_length) != Some(observed.digest)
                    || observed.received != spec.range_length { return false; }
            }
            "resolve" => {
                if observed.boundary != "reply" { return false; }
            }
            "versions" | "resolve-head" => {}
            _ => return false,
        }
    }
    uploads.len() == 4 && completed == 2 && aborted == 2 && replacements == 1 && before_orphan && after_orphan
        && uploads.values().all(|(_, _, terminal)| *terminal)
}
