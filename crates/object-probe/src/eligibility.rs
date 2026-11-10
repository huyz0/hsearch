//! Complete fixed-matrix eligibility. Required uncertainty never becomes ready.
use crate::codec::payload_digest;
use crate::model::{Capture, CaptureEvent, CapturePlan, HISTORY_MICROS};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Independently evaluated scenario state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// Complete legal evidence under the exact admitted contract.
    Supported,
    /// A decisive illegal observation.
    Failed,
    /// Missing, unsupported, truncated or exhausted required proof.
    Inconclusive,
}
/// Six original behavior areas in create/update/loss/range/restart/multipart order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    /// No required scenario may be absent or optional.
    pub scenarios: [Status; 6],
    /// Bounded deterministic model transitions, not native measurements.
    pub transitions: u32,
}
impl Report {
    /// Every required area must have a legal complete witness.
    pub fn ready(&self) -> bool {
        self.scenarios.iter().all(|s| *s == Status::Supported)
    }
}

fn observed(capture: &Capture, id: u16) -> Option<&CaptureEvent> {
    capture.events.iter().find(|e| e.id == id)
}
fn successful(event: &CaptureEvent) -> bool {
    matches!(event.status, 200 | 201 | 204) && event.boundary == "reply" && event.error.is_empty()
}

fn quoted(raw: &str) -> String {
    let mut value = String::from("\"");
    for c in raw.chars() {
        if matches!(c, '"' | '\\') {
            value.push('\\');
        }
        value.push(c);
    }
    value.push('"');
    value
}
fn hex(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn mutation_metadata(
    plan: &CapturePlan,
    capture: &Capture,
    spec: &crate::model::Specification,
) -> Option<String> {
    let event = observed(capture, spec.id)?;
    let run = hex(&plan.binding.run);
    let mut upload = String::new();
    let mut parts = String::new();
    let body = if spec.action == "complete" {
        upload = observed(capture, spec.parent)?.auxiliary.clone();
        parts = event.auxiliary.clone();
        let mut xml = String::from("<CompleteMultipartUpload>");
        for part in parts.split(';') {
            let (number, token) = part.split_once(':')?;
            let token = String::from_utf8(crate::codec::unhex(token, 256).ok()?).ok()?;
            let escaped = token
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            xml.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{escaped}</ETag></Part>"
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        crate::codec::sha256(xml.as_bytes())
    } else {
        payload_digest(&[(spec.seed, spec.length, 0)]).ok()?
    };
    let envelope = format!(
        "{{\"body_sha256\":{},\"bucket\":{},\"caller_metadata\":\"probe-caller-v2\",\"condition\":{},\"key\":{},\"operation\":{},\"parts\":{},\"upload\":{}}}",
        quoted(&hex(&body)),
        quoted(&plan.bucket),
        quoted(&event.condition),
        quoted(&format!("{run}/{}", spec.key)),
        quoted(&event.request),
        quoted(&parts),
        quoted(&upload)
    );
    Some(format!(
        "{}|{}|probe-caller-v2",
        event.request,
        hex(&crate::codec::sha256(envelope.as_bytes()))
    ))
}

fn opaque_list(raw: &str) -> Option<Vec<String>> {
    let mut bytes = raw.strip_prefix('[')?.strip_suffix(']')?.bytes().peekable();
    let mut values = Vec::new();
    while bytes.peek().is_some() {
        if values.len() == 16 || bytes.next()? != b'"' {
            return None;
        }
        let mut value = String::new();
        loop {
            let byte = bytes.next()?;
            if byte == b'"' {
                break;
            }
            let byte = if byte == b'\\' {
                let escaped = bytes.next()?;
                if !matches!(escaped, b'\\' | b'"') {
                    return None;
                }
                escaped
            } else {
                byte
            };
            if !(32..127).contains(&byte) || value.len() == 256 {
                return None;
            }
            value.push(char::from(byte));
        }
        if value.is_empty() || value == "null" || values.contains(&value) {
            return None;
        }
        values.push(value);
        if bytes.peek().is_some() && bytes.next()? != b',' {
            return None;
        }
    }
    Some(values)
}

fn resolution(
    plan: &CapturePlan,
    capture: &Capture,
    spec: &crate::model::Specification,
    event: &CaptureEvent,
) -> bool {
    let Some(original) = plan.specifications.iter().find(|s| s.id == spec.parent) else {
        return false;
    };
    let Some(old) = observed(capture, original.id) else {
        return false;
    };
    let candidates: Vec<_> = plan
        .specifications
        .iter()
        .filter(|s| s.action == "resolve-head" && s.key == spec.key)
        .filter_map(|s| observed(capture, s.id))
        .filter(|e| e.status == 200 && e.metadata.split('|').next() == Some(old.request.as_str()))
        .collect();
    if candidates.len() != 1
        || event.request != old.request
        || event.condition != old.condition
        || event.status != 200
        || event.selected != candidates[0].version
        || event.version != event.selected
        || Some(event.metadata.clone()) != mutation_metadata(plan, capture, original)
        || candidates[0].head_length != Some(event.received)
        || event.auxiliary != candidates[0].id.to_string()
    {
        return false;
    }
    if original.action == "complete" {
        crate::multipart::completion_digest(plan, original, event.received) == Some(event.digest)
    } else {
        event.received == original.length
            && payload_digest(&[(original.seed, original.length, 0)]).ok() == Some(event.digest)
    }
}

/// Evaluate independently admitted requests against all actual attempt records.
/// The legacy search and its UnmodeledEvidence behavior are unchanged.
pub fn evaluate(plan: &CapturePlan, capture: &Capture, restart: &str) -> Report {
    let started = Instant::now();
    let mut report = Report {
        scenarios: [Status::Supported; 6],
        transitions: 0,
    };
    let incomplete = || Report {
        scenarios: [Status::Inconclusive; 6],
        transitions: 0,
    };
    if !capture.complete
        || plan.specifications.is_empty()
        || capture.events.len() != plan.specifications.len()
        || plan.specifications.len() > 256
        || capture.events.len() > 256
        || capture.attempts > 512
        || usize::from(capture.attempts) != capture.events.len()
        || plan.specifications.iter().any(|s| {
            s.id == 0
                || s.id > 256
                || s.scenario > 5
                || s.client >= 16
                || s.parent >= s.id
                || s.key.len() > 1024
                || s.length > 16 * 1024 * 1024
                || s.range_length > 16 * 1024 * 1024
        })
        || capture.events.iter().any(|e| {
            e.start > e.end
                || e.end > HISTORY_MICROS
                || e.etag.len() > 256
                || e.version.len() > 256
                || e.auxiliary.len() > 8192
                || e.request.len() > 128
        })
    {
        return incomplete();
    }
    let mut attempts = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut tokens = BTreeSet::new();
    let mut heads = BTreeMap::new();
    let run: String = plan
        .binding
        .run
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut areas = [0u16; 6];
    for spec in &plan.specifications {
        report.transitions += 1;
        if report.transitions >= 100_000 || started.elapsed() >= Duration::from_secs(10) {
            return incomplete();
        }
        let area = usize::from(spec.scenario);
        areas[area] += 1;
        let Some(event) = observed(capture, spec.id) else {
            return incomplete();
        };
        let request_id = if matches!(spec.action.as_str(), "resolve" | "replay" | "reuse-id") {
            spec.parent
        } else {
            spec.id
        };
        if !ids.insert(event.id)
            || !attempts.insert(event.attempt)
            || event.attempt > capture.attempts
            || event.end > HISTORY_MICROS
            || event.request != format!("{run}-{request_id}")
        {
            report.scenarios[area] = Status::Failed;
            continue;
        }
        if matches!(
            event.error.as_str(),
            "provider-envelope-missing"
                | "secret-bearing-response"
                | "completion-envelope-not-provider-bound"
        ) {
            report.scenarios[area] = Status::Failed;
            continue;
        }
        if event.boundary == "unimplemented"
            || event.boundary == "not-transmitted"
            || (!event.error.is_empty() && event.error != "response-not-observed")
        {
            if report.scenarios[area] != Status::Failed {
                report.scenarios[area] = Status::Inconclusive;
            }
            continue;
        }
        if matches!(event.status, 429 | 500 | 501 | 502 | 503 | 504) {
            if report.scenarios[area] != Status::Failed {
                report.scenarios[area] = Status::Inconclusive;
            }
            continue;
        }
        let parent = if spec.parent == 0 {
            None
        } else {
            observed(capture, spec.parent)
        };
        if let Some(parent) = parent {
            if parent.end > event.start {
                report.scenarios[area] = Status::Failed;
            }
        }
        let prior = report.scenarios[area];
        match spec.action.as_str() {
            "bucket" | "versioning" => {
                if !successful(event) {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "create" | "update" => {
                let expected = if spec.action == "create" {
                    Some("*")
                } else {
                    parent.map(|p| p.etag.as_str())
                };
                if expected.is_none()
                    || expected != Some(event.condition.as_str())
                    || event.sent < spec.length
                {
                    report.scenarios[area] = Status::Failed;
                }
                if matches!(event.status, 200 | 201 | 204) {
                    if event.etag.is_empty() || event.version.is_empty() || event.version == "null"
                    {
                        report.scenarios[area] = Status::Inconclusive;
                    } else if !tokens.insert((spec.key.clone(), event.version.clone())) {
                        // Reused ETags are allowed. Reused immutable versions are not.
                        report.scenarios[area] = Status::Failed;
                    }
                } else if !matches!(event.status, 409 | 412) && spec.fault != "disconnect" {
                    report.scenarios[area] = Status::Inconclusive;
                }
                if spec.fault == "suppress" {
                    if event.boundary != "reply-suppressed" || !matches!(event.status, 200 | 201) {
                        report.scenarios[area] = Status::Failed;
                    }
                } else if spec.fault == "disconnect" {
                    if event.boundary != "after-transmit-before-reply" || event.status != 0 {
                        report.scenarios[area] = Status::Failed;
                    }
                } else if event.boundary != "reply" {
                    report.scenarios[area] = Status::Inconclusive;
                }
                if matches!(event.status, 200 | 201 | 204) {
                    heads.insert(spec.key.as_str(), spec);
                }
            }
            "reuse-id" => {
                if event.condition != "*" || event.boundary != "reply" || event.status != 409 {
                    report.scenarios[area] = if matches!(event.status, 200 | 201 | 204) {
                        Status::Failed
                    } else {
                        Status::Inconclusive
                    };
                }
            }
            "stale-update" => {
                let Some(parent) = parent else {
                    return incomplete();
                };
                if event.condition != parent.etag
                    || !matches!(event.status, 409 | 412)
                    || event.boundary != "reply"
                {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "replay" => {
                let Some(parent) = parent else {
                    return incomplete();
                };
                if event.condition != parent.condition
                    || event.sent == 0
                    || event.boundary != "reply"
                    || (successful(event) && event.version != parent.version)
                    || (!successful(event) && !matches!(event.status, 409 | 412))
                {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "range" if spec.scenario != 5 => {
                let Some(parent) = parent else {
                    return incomplete();
                };
                let Some(original) = plan.specifications.iter().find(|s| s.id == spec.parent)
                else {
                    return incomplete();
                };
                if parent.version.is_empty() || parent.version == "null" {
                    report.scenarios[area] = Status::Inconclusive;
                    continue;
                }
                if event.selected != parent.version {
                    report.scenarios[area] = Status::Failed;
                }
                if spec
                    .offset
                    .checked_add(spec.range_length)
                    .is_none_or(|end| end > original.length)
                {
                    if event.status != 416 {
                        report.scenarios[area] = Status::Failed;
                    }
                } else if event.status != 206
                    || event.version != parent.version
                    || event.received != spec.range_length
                    || payload_digest(&[(original.seed, spec.range_length, spec.offset)]).ok()
                        != Some(event.digest)
                {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "resolve" => {
                if !successful(event) {
                    report.scenarios[area] = Status::Inconclusive;
                } else if !resolution(plan, capture, spec, event) {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "versions" => {
                if !successful(event) || opaque_list(&event.auxiliary).is_none() {
                    report.scenarios[area] = Status::Inconclusive;
                }
            }
            "resolve-head" => {
                let Some(parent) = parent else {
                    return incomplete();
                };
                if !successful(parent) {
                    if report.scenarios[area] != Status::Failed {
                        report.scenarios[area] = Status::Inconclusive;
                    }
                    continue;
                }
                let Some(versions) = opaque_list(&parent.auxiliary) else {
                    if report.scenarios[area] != Status::Failed {
                        report.scenarios[area] = Status::Inconclusive;
                    }
                    continue;
                };
                if let Some(version) = versions.get(spec.offset as usize) {
                    if !successful(event) {
                        report.scenarios[area] = Status::Inconclusive;
                    } else if event.selected != *version || event.version != *version {
                        report.scenarios[area] = Status::Failed;
                    }
                } else if event.boundary != "not-applicable"
                    || event.sent != 0
                    || event.received != 0
                    || event.status != 0
                {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "head" => {
                let Some(original) = heads.get(spec.key.as_str()).copied() else {
                    return incomplete();
                };
                let Some(parent) = observed(capture, original.id) else {
                    return incomplete();
                };
                if !successful(event)
                    || event.etag != parent.etag
                    || event.version != parent.version
                    || event.head_length != Some(original.length)
                    || Some(event.metadata.clone()) != mutation_metadata(plan, capture, original)
                {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "race-head" | "race-range" => {
                let winners: Vec<_> = plan
                    .specifications
                    .iter()
                    .filter(|s| u64::from(s.race) == spec.offset && s.race != 0)
                    .filter_map(|s| {
                        observed(capture, s.id)
                            .filter(|e| successful(e))
                            .map(|e| (s, e))
                    })
                    .collect();
                if winners.len() != 1 {
                    report.scenarios[area] = Status::Failed;
                    continue;
                }
                let (winner, receipt) = winners[0];
                if event.version != receipt.version
                    || Some(event.metadata.clone()) != mutation_metadata(plan, capture, winner)
                {
                    report.scenarios[area] = Status::Failed;
                }
                if spec.action == "race-head" {
                    if !successful(event)
                        || event.etag != receipt.etag
                        || event.head_length != Some(winner.length)
                    {
                        report.scenarios[area] = Status::Failed;
                    }
                } else if event.status != 206
                    || event.selected != receipt.version
                    || event.received != winner.length
                    || payload_digest(&[(winner.seed, winner.length, 0)]).ok() != Some(event.digest)
                {
                    report.scenarios[area] = Status::Failed;
                }
            }
            "restart" | "init" | "part" | "complete" | "complete-invalid" | "abort" | "parts"
            | "uploads" | "range" => {}
            _ => report.scenarios[area] = Status::Failed,
        }
        if prior == Status::Failed {
            report.scenarios[area] = Status::Failed;
        }
    }
    if (1..=capture.attempts).any(|id| !attempts.contains(&id)) {
        for status in &mut report.scenarios {
            if *status != Status::Failed {
                *status = Status::Inconclusive;
            }
        }
        return report;
    }
    for (index, count) in areas.iter().enumerate() {
        if *count == 0 && report.scenarios[index] != Status::Failed {
            report.scenarios[index] = Status::Inconclusive;
        }
    }
    for (race, area, action) in [(1, 0, "create"), (2, 1, "update")] {
        let group: Vec<_> = plan
            .specifications
            .iter()
            .filter(|s| s.race == race)
            .collect();
        let mut clients = 0u16;
        let mut latest = 0;
        let mut earliest = u64::MAX;
        let mut winners = 0;
        let mut uncertain = false;
        if group.len() != 16 {
            report.scenarios[area] = Status::Inconclusive;
            continue;
        }
        for spec in group {
            report.transitions += 1;
            let Some(event) = observed(capture, spec.id) else {
                return incomplete();
            };
            if spec.action != action || clients & (1 << spec.client) != 0 {
                report.scenarios[area] = Status::Failed;
            }
            if event.sent == 0
                || event.boundary != "reply"
                || !matches!(event.status, 200 | 201 | 204 | 409 | 412)
            {
                uncertain = true;
                if report.scenarios[area] != Status::Failed {
                    report.scenarios[area] = Status::Inconclusive;
                }
            }
            clients |= 1 << spec.client;
            latest = latest.max(event.start);
            earliest = earliest.min(event.end);
            winners += u16::from(successful(event));
        }
        if (clients != u16::MAX || latest >= earliest) && report.scenarios[area] != Status::Failed {
            report.scenarios[area] = Status::Inconclusive;
        }
        if winners > 1 || winners == 0 && !uncertain {
            report.scenarios[area] = Status::Failed;
        }
    }
    // The complete predeclared workload must retain every decisive subcase.
    let count = |area, action| {
        plan.specifications
            .iter()
            .filter(|s| s.scenario == area && s.action == action)
            .count()
    };
    if count(2, "create") != 2 || count(2, "resolve") != 2 || count(2, "reuse-id") != 1 {
        if report.scenarios[2] != Status::Failed {
            report.scenarios[2] = Status::Inconclusive;
        }
    }
    if count(3, "range") != 5
        || count(3, "update") != 3
        || count(3, "replay") != 1
        || count(3, "stale-update") != 1
    {
        if report.scenarios[3] != Status::Failed {
            report.scenarios[3] = Status::Inconclusive;
        }
    }
    if count(4, "restart") != 1 || count(4, "head") != 1 || count(4, "range") != 1 {
        report.scenarios[4] = Status::Inconclusive;
    }
    if areas[4] != 0
        && !crate::restart::verify(plan, capture, restart)
        && report.scenarios[4] == Status::Supported
    {
        report.scenarios[4] = Status::Failed;
    }
    if areas[5] != 0
        && !crate::multipart::verify(plan, capture)
        && report.scenarios[5] == Status::Supported
    {
        report.scenarios[5] = Status::Failed;
    }
    if started.elapsed() >= Duration::from_secs(10) {
        return incomplete();
    }
    report
}
