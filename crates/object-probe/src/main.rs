//! Fixed diagnostic checker. No SDK, network, compiler or provider access.
use hsearch_object_probe::{codec, eligibility};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

fn bounded(path: &Path, cap: usize) -> Result<Vec<u8>, &'static str> {
    let before = fs::symlink_metadata(path).map_err(|_| "input-unavailable")?;
    if !before.is_file() || before.len() > cap as u64 {
        return Err("input-bound");
    }
    let mut raw = Vec::new();
    File::open(path)
        .map_err(|_| "input-unavailable")?
        .take(cap as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| "input-unavailable")?;
    let after = fs::symlink_metadata(path).map_err(|_| "input-changed")?;
    if raw.len() > cap
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err("input-changed");
    }
    Ok(raw)
}

fn run(input_root: &Path, argument_count: usize) -> Result<bool, &'static str> {
    if argument_count != 1 {
        return Err("fixed-profile-only");
    }
    let plan_raw = bounded(&input_root.join("plan.wire"), 65536)?;
    let expected = bounded(&input_root.join("admission.sha256"), 64)?;
    let expected = std::str::from_utf8(&expected).map_err(|_| "admission-format")?;
    if codec::unhex(expected, 32).map_err(|_| "admission-format")? != codec::sha256(&plan_raw) {
        return Err("admission-binding");
    }
    let plan = codec::decode_plan_v2(&plan_raw).map_err(|_| "plan-refused")?;
    let history = bounded(&input_root.join("capture.history"), 704 * 1024)?;
    let capture = codec::decode_capture_v2(&history, codec::sha256(&plan_raw))
        .map_err(|_| "capture-refused")?;
    let restart = bounded(&input_root.join("restart.json"), 1024)?;
    let restart = std::str::from_utf8(&restart).map_err(|_| "restart-format")?;
    let report = eligibility::evaluate(&plan, &capture, restart);
    let names: Vec<_> = report
        .scenarios
        .iter()
        .map(|s| match s {
            eligibility::Status::Supported => "supported",
            eligibility::Status::Failed => "failed",
            eligibility::Status::Inconclusive => "inconclusive",
        })
        .collect();
    println!(
        "{{\"profile\":\"rustfs-capabilities-v4\",\"ready\":{},\"scenarios\":[\"{}\"],\"transitions\":{}}}",
        report.ready(),
        names.join("\",\""),
        report.transitions
    );
    Ok(report.ready())
}

fn exit_code(result: Result<bool, &'static str>) -> i32 {
    match result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(code) => {
            eprintln!("rustfs-checker-refused:{code}");
            2
        }
    }
}

fn main() {
    let code = exit_code(run(Path::new("/probe-input"), std::env::args_os().len()));
    if code != 0 {
        std::process::exit(code);
    }
}

#[cfg(test)]
#[path = "../tests/codec_golden.rs"]
mod golden;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Inputs(PathBuf);
    impl Inputs {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("hsearch-checker-{}-{nonce}", std::process::id()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn write(&self, name: &str, raw: &[u8]) {
            fs::write(self.0.join(name), raw).unwrap();
        }
        fn plan(&self, raw: &[u8]) {
            self.write("plan.wire", raw);
            let hash: String = codec::sha256(raw)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            self.write("admission.sha256", hash.as_bytes());
        }
        fn complete(&self) {
            self.plan(golden::FULL_PLAN);
            self.write("capture.history", golden::FULL_CAPTURE);
            self.write("restart.json", golden::RESTART.as_bytes());
        }
    }
    impl Drop for Inputs {
        fn drop(&mut self) {
            assert!(self.0.starts_with(std::env::temp_dir()));
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn fixed_profile_complete_and_partial_have_distinct_exit_codes() {
        let input = Inputs::new();
        assert_eq!(run(&input.0, 2), Err("fixed-profile-only"));
        assert_eq!(run(&input.0, 1), Err("input-unavailable"));
        input.complete();
        assert_eq!(run(&input.0, 1), Ok(true));
        input.plan(golden::PLAN);
        input.write("capture.history", golden::CAPTURE);
        assert_eq!(run(&input.0, 1), Ok(false));
        assert_eq!(exit_code(Ok(true)), 0);
        assert_eq!(exit_code(Ok(false)), 1);
        assert_eq!(exit_code(Err("input-bound")), 2);
    }

    #[test]
    fn admitted_input_refusals_do_not_reclassify_or_rebind_history() {
        let input = Inputs::new();
        input.complete();
        for raw in [vec![255], b"z".repeat(64)] {
            input.write("admission.sha256", &raw);
            assert_eq!(run(&input.0, 1), Err("admission-format"));
        }
        input.write("admission.sha256", &b"0".repeat(64));
        assert_eq!(run(&input.0, 1), Err("admission-binding"));
        input.plan(b"invalid plan\n");
        assert_eq!(run(&input.0, 1), Err("plan-refused"));
        input.complete();
        input.write("capture.history", b"invalid capture\n");
        assert_eq!(run(&input.0, 1), Err("capture-refused"));
        input.complete();
        input.write("restart.json", &[255]);
        assert_eq!(run(&input.0, 1), Err("restart-format"));
    }

    #[test]
    fn bounded_reader_refuses_directory_oversize_missing_and_links() {
        let input = Inputs::new();
        assert_eq!(bounded(&input.0, 3), Err("input-bound"));
        let path = input.0.join("small");
        input.write("small", b"abc");
        assert_eq!(bounded(&path, 2), Err("input-bound"));
        assert_eq!(bounded(&path, 3).unwrap(), b"abc");
        assert_eq!(
            bounded(&input.0.join("missing"), 3),
            Err("input-unavailable")
        );
        #[cfg(unix)]
        {
            let link = input.0.join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert_eq!(bounded(&link, 3), Err("input-bound"));
        }
    }
}
