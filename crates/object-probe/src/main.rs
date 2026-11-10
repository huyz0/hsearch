//! Fixed diagnostic checker. No SDK, network, compiler or provider access.
use hsearch_object_probe::{codec, eligibility};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

fn bounded(path: &str, cap: usize) -> Result<Vec<u8>, &'static str> {
    let path = Path::new(path);
    let before = fs::symlink_metadata(path).map_err(|_| "input-unavailable")?;
    if !before.is_file() || before.len() > cap as u64 { return Err("input-bound"); }
    let mut raw = Vec::new();
    File::open(path).map_err(|_| "input-unavailable")?.take(cap as u64 + 1).read_to_end(&mut raw).map_err(|_| "input-unavailable")?;
    let after = fs::symlink_metadata(path).map_err(|_| "input-changed")?;
    if raw.len() > cap || before.len() != after.len() || before.modified().ok() != after.modified().ok() { return Err("input-changed"); }
    Ok(raw)
}

fn run() -> Result<bool, &'static str> {
    if std::env::args_os().len() != 1 { return Err("fixed-profile-only"); }
    let plan_raw = bounded("/probe-input/plan.wire", 65536)?;
    let expected = bounded("/probe-input/admission.sha256", 64)?;
    let expected = std::str::from_utf8(&expected).map_err(|_| "admission-format")?;
    if codec::unhex(expected, 32).map_err(|_| "admission-format")? != codec::sha256(&plan_raw) { return Err("admission-binding"); }
    let plan = codec::decode_plan_v2(&plan_raw).map_err(|_| "plan-refused")?;
    let history = bounded("/probe-input/capture.history", 704 * 1024)?;
    let capture = codec::decode_capture_v2(&history, codec::sha256(&plan_raw)).map_err(|_| "capture-refused")?;
    let restart = bounded("/probe-input/restart.json", 1024)?;
    let restart = std::str::from_utf8(&restart).map_err(|_| "restart-format")?;
    let report = eligibility::evaluate(&plan, &capture, restart);
    let names: Vec<_> = report.scenarios.iter().map(|s| match s {
        eligibility::Status::Supported => "supported",
        eligibility::Status::Failed => "failed",
        eligibility::Status::Inconclusive => "inconclusive",
    }).collect();
    println!("{{\"profile\":\"rustfs-capabilities-v2\",\"ready\":{},\"scenarios\":[\"{}\"],\"transitions\":{}}}", report.ready(), names.join("\",\""), report.transitions);
    Ok(report.ready())
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(code) => { eprintln!("rustfs-checker-refused:{code}"); std::process::exit(2); }
    }
}
