//! Loader tests against real shared libraries built from `tests/c/hs_test_module.c`.
//!
//! Each case compiles the same source with `cc`, with one compile-time variant that
//! breaks one rule of the contract, so what is loaded is a genuine ELF object and
//! not a mock. Needs `cc` on the path, like `tools/tests/test_extension_abi.py`.
#![cfg(target_os = "linux")]
#![allow(missing_docs)]

use hsearch_module_abi::{CallError, FailureKind, Kind, status};
use hsearch_module_native::{
    LoadConfig, LoadError, LogSink, ModuleState, NativeModule, Policy, Request,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn desc(abi: &str, engine: &str, max_calls: u32) -> String {
    format!(
        r#"{{"name":"test-module","version":"0.1.0","abi":"{abi}","engine_api":"{engine}","max_concurrent_calls":{max_calls},"io_bytes":4096,"extension_points":[{{"kind":"analyzer","id":"echo","version":"1.0.0","deterministic":true}},{{"kind":"query","id":"q","version":"1.0.0","deterministic":true}}]}}"#
    )
}

struct Built {
    path: PathBuf,
    sha: [u8; 32],
    dir: PathBuf,
}

/// Compiles the test module with `-D` flags and returns the library and its hash.
fn build(label: &str, desc_json: &str, flags: &[&str]) -> Built {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("native-modules")
        .join(label);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("module.so");
    let status = Command::new("cc")
        .args([
            "-shared",
            "-fPIC",
            "-O1",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-std=gnu11",
        ])
        .arg("-I")
        .arg(root.join("../../fixtures/extensions/abi-v1"))
        .arg(format!(
            "-DDESC_JSON=\"{}\"",
            desc_json.replace('"', "\\\"")
        ))
        .args(flags)
        .arg(root.join("tests/c/hs_test_module.c"))
        .arg("-o")
        .arg(&path)
        .status()
        .expect("cc is required to run the loader tests");
    assert!(status.success(), "cc failed for {label}");
    let sha = Sha256::digest(std::fs::read(&path).expect("built")).into();
    Built { path, sha, dir }
}

fn load_default(label: &str, flags: &[&str]) -> (NativeModule, Built) {
    let built = build(label, &desc("1.0", "1.0", 4), flags);
    let module = NativeModule::load(&built.path, LoadConfig::new(built.sha)).expect("loads");
    (module, built)
}

fn req<'a>(input: &'a [u8], bound: u32, limit: u32) -> Request<'a> {
    Request {
        kind: Kind::Analyzer,
        point: "echo",
        input,
        timeout: Duration::from_secs(5),
        output_bound: bound,
        output_limit: limit,
    }
}

fn call(m: &NativeModule, input: &[u8]) -> Result<Vec<u8>, CallError> {
    let mut out = Vec::new();
    m.call(&req(input, 64, 1024), &mut out).map(|()| out)
}

fn n_cmd(c: u8, n: u32) -> Vec<u8> {
    let mut v = vec![c];
    v.extend_from_slice(&n.to_le_bytes());
    v
}

fn refused(built: &Built, cfg: LoadConfig) -> LoadError {
    NativeModule::load(&built.path, cfg).expect_err("must be refused")
}

#[test]
fn loads_a_valid_module_and_runs_calls() {
    let (m, _b) = load_default("valid", &[]);
    assert_eq!(m.state(), ModuleState::Ready);
    assert_eq!(m.descriptor().name, "test-module");
    assert_eq!(m.descriptor().extension_points.len(), 2);
    let mut input = b"e".to_vec();
    input.extend_from_slice(b"hello");
    assert_eq!(call(&m, &input).expect("echo"), b"hello");
    // The module sees the call's kind, point id and a nonzero call id.
    let seen = call(&m, b"i").expect("info");
    assert_ne!(u64::from_le_bytes(seen[..8].try_into().unwrap()), 0);
    assert_eq!(u32::from_le_bytes(seen[8..12].try_into().unwrap()), 1);
    assert_eq!(&seen[12..], b"echo");
    let mut out = Vec::new();
    let query = Request {
        kind: Kind::Query,
        point: "q",
        ..req(b"i", 64, 64)
    };
    m.call(&query, &mut out).expect("query point");
    assert_eq!(u32::from_le_bytes(out[8..12].try_into().unwrap()), 2);
    assert_eq!(&out[12..], b"q");
}

#[test]
fn only_declared_points_inside_the_bounds_are_called() {
    let (m, _b) = load_default("bounds", &[]);
    let mut out = Vec::new();
    let undeclared = Request {
        point: "nope",
        ..req(b"e", 64, 64)
    };
    let e = m.call(&undeclared, &mut out).expect_err("undeclared");
    assert_eq!(e.failure, FailureKind::Unavailable);
    let wrong_kind = Request {
        kind: Kind::Scoring,
        point: "echo",
        ..req(b"e", 64, 64)
    };
    assert_eq!(
        m.call(&wrong_kind, &mut out).expect_err("kind").failure,
        FailureKind::Unavailable
    );
    let big = vec![b'e'; 1024 * 1024 + 1];
    assert_eq!(
        m.call(&req(&big, 64, 64), &mut out)
            .expect_err("input")
            .failure,
        FailureKind::Limit
    );
    for (bound, limit) in [(0, 64), (64, 32), (64, 16 * 1024 * 1024 + 1)] {
        assert_eq!(
            m.call(&req(b"e", bound, limit), &mut out)
                .expect_err("bounds")
                .failure,
            FailureKind::Limit,
            "{bound} {limit}"
        );
    }
    assert_eq!(m.state(), ModuleState::Ready);
}

#[test]
fn refuses_a_file_that_is_not_the_pinned_one() {
    let built = build("hash", &desc("1.0", "1.0", 4), &[]);
    let mut wrong = built.sha;
    wrong[0] ^= 1;
    assert_eq!(
        refused(&built, LoadConfig::new(wrong)),
        LoadError::HashMismatch
    );
}

#[test]
fn refuses_a_wrong_version_before_any_other_symbol_is_resolved() {
    // ONLY_VERSION leaves nothing but hs_abi_version: a refusal by version, not by a
    // missing symbol, proves the other four were never looked up.
    for (label, ret) in [
        ("major2", "0x00020000u"),
        ("newer-minor", "0x00010001u"),
        ("major0", "0x00000000u"),
    ] {
        let built = build(
            label,
            &desc("1.0", "1.0", 4),
            &["-DONLY_VERSION", &format!("-DABI_RET={ret}")],
        );
        let error = refused(&built, LoadConfig::new(built.sha));
        assert!(
            matches!(
                error,
                LoadError::AbiRefused {
                    host: 0x0001_0000,
                    ..
                }
            ),
            "{label}: {error:?}"
        );
        assert_eq!(error.failure_kind(), FailureKind::VersionRefused);
    }
    let built = build("version-only", &desc("1.0", "1.0", 4), &["-DONLY_VERSION"]);
    assert_eq!(
        refused(&built, LoadConfig::new(built.sha)),
        LoadError::MissingSymbol("hs_module_init")
    );
}

#[test]
fn accepts_an_older_minor_and_refuses_a_newer_one() {
    let built = build("minor", &desc("1.0", "1.0", 4), &[]);
    let mut host_newer = LoadConfig::new(built.sha);
    host_newer.host_abi = 0x0001_0001;
    assert!(
        NativeModule::load(&built.path, host_newer).is_ok(),
        "module 1.0 on host 1.1"
    );
    let newer = build(
        "minor-new",
        &desc("1.1", "1.0", 4),
        &["-DABI_RET=0x00010001u"],
    );
    assert!(
        matches!(
            refused(&newer, LoadConfig::new(newer.sha)),
            LoadError::AbiRefused { .. }
        ),
        "module 1.1 on host 1.0"
    );
    let mut host_11 = LoadConfig::new(newer.sha);
    host_11.host_abi = 0x0001_0001;
    assert!(
        NativeModule::load(&newer.path, host_11).is_ok(),
        "module 1.1 on host 1.1"
    );
}

#[test]
fn refuses_a_module_missing_a_symbol() {
    let built = build("no-call", &desc("1.0", "1.0", 4), &["-DOMIT_CALL"]);
    let error = refused(&built, LoadConfig::new(built.sha));
    assert_eq!(error, LoadError::MissingSymbol("hs_module_call"));
    assert_eq!(error.failure_kind(), FailureKind::Unavailable);
}

fn marker(built: &Built) -> PathBuf {
    built.dir.join("shutdown.marker")
}

fn with_marker(built: &Built) -> LoadConfig {
    let _ = std::fs::remove_file(marker(built));
    let mut cfg = LoadConfig::new(built.sha);
    cfg.module_config = marker(built).to_string_lossy().into_owned().into_bytes();
    cfg
}

#[test]
fn refuses_an_engine_api_the_host_does_not_implement() {
    for (label, api) in [("api-newer", "1.1"), ("api-major", "2.0")] {
        let built = build(label, &desc("1.0", api, 4), &[]);
        let error = refused(&built, with_marker(&built));
        assert!(
            matches!(error, LoadError::EngineApiRefused(_)),
            "{label}: {error:?}"
        );
        assert_eq!(error.failure_kind(), FailureKind::VersionRefused);
        // init had succeeded, so the module was shut down properly.
        assert!(
            marker(&built).exists(),
            "{label}: shutdown must run after a refused descriptor"
        );
    }
}

#[test]
fn refuses_bad_descriptors_and_shuts_down_after_init() {
    let cases: Vec<(&str, String, Vec<&str>, &str)> = vec![
        (
            "desc-json",
            "not json".to_owned(),
            vec![],
            "descriptor refused",
        ),
        (
            "desc-unknown-key",
            desc("1.0", "1.0", 4).replace("\"io_bytes\"", "\"x\":1,\"io_bytes\""),
            vec![],
            "unknown or missing key",
        ),
        (
            "desc-lie",
            desc("1.0", "1.0", 4),
            vec!["-DDESC_LIE"],
            "exceeds its buffer",
        ),
        (
            "desc-fail",
            desc("1.0", "1.0", 4),
            vec!["-DDESC_FAIL"],
            "hs_module_describe failed",
        ),
        (
            "desc-overrun",
            desc("1.0", "1.0", 4),
            vec!["-DDESC_OVERRUN"],
            "past the descriptor buffer",
        ),
    ];
    for (label, json, flags, needle) in cases {
        let built = build(label, &json, &flags);
        let error = refused(&built, with_marker(&built));
        assert!(
            matches!(error, LoadError::BadDescriptor(_)),
            "{label}: {error:?}"
        );
        assert!(
            error.to_string().contains(needle)
                || matches!(&error, LoadError::BadDescriptor(w) if w.contains(needle) || needle == "descriptor refused"),
            "{label}: {error}"
        );
        assert!(marker(&built).exists(), "{label}: shutdown must run");
    }
}

#[test]
fn refuses_a_descriptor_that_disagrees_with_the_module_or_the_node() {
    // The descriptor claims 1.0 while hs_abi_version says 1.1.
    let built = build(
        "abi-disagree",
        &desc("1.0", "1.0", 4),
        &["-DABI_RET=0x00010001u"],
    );
    let mut cfg = LoadConfig::new(built.sha);
    cfg.host_abi = 0x0001_0001;
    assert_eq!(
        refused(&built, cfg),
        LoadError::DescriptorMismatch("descriptor abi differs from hs_abi_version")
    );
    let built = build("name-disagree", &desc("1.0", "1.0", 4), &[]);
    let mut cfg = LoadConfig::new(built.sha);
    cfg.expected_name = Some("other".to_owned());
    assert!(matches!(
        refused(&built, cfg),
        LoadError::DescriptorMismatch(_)
    ));
    let mut cfg = LoadConfig::new(built.sha);
    cfg.expected_name = Some("test-module".to_owned());
    assert!(NativeModule::load(&built.path, cfg).is_ok());
}

#[test]
fn init_failure_is_refused_and_shutdown_is_not_called() {
    let built = build("init-fail", &desc("1.0", "1.0", 4), &["-DINIT_FAIL"]);
    assert_eq!(
        refused(&built, with_marker(&built)),
        LoadError::InitFailed(status::MODULE)
    );
    assert!(!marker(&built).exists());
}

#[test]
fn refuses_images_it_cannot_vouch_for() {
    let built = build(
        "deps",
        &desc("1.0", "1.0", 4),
        &["-Wl,--no-as-needed", "-lstdc++"],
    );
    assert!(
        matches!(refused(&built, LoadConfig::new(built.sha)), LoadError::DisallowedDependency(n) if n.contains("stdc++"))
    );
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("native-modules")
        .join("images");
    std::fs::create_dir_all(&dir).expect("dir");
    let sha_of = |bytes: &[u8]| -> [u8; 32] { Sha256::digest(bytes).into() };
    let text = dir.join("text.so");
    std::fs::write(&text, b"this is not an ELF file").expect("write");
    let error = NativeModule::load(&text, LoadConfig::new(sha_of(b"this is not an ELF file")))
        .expect_err("text");
    assert!(matches!(error, LoadError::BadImage(_)), "{error:?}");
    let empty = dir.join("empty.so");
    std::fs::write(&empty, b"").expect("write");
    assert!(matches!(
        NativeModule::load(&empty, LoadConfig::new(sha_of(b""))).expect_err("empty"),
        LoadError::BadImage(_)
    ));
    assert!(matches!(
        NativeModule::load(&dir.join("absent.so"), LoadConfig::new([0; 32])).expect_err("absent"),
        LoadError::Io(_)
    ));
    assert!(matches!(
        NativeModule::load(&dir, LoadConfig::new([0; 32])).expect_err("directory"),
        LoadError::Io(_)
    ));
    let ok = build("size", &desc("1.0", "1.0", 4), &[]);
    let mut small = LoadConfig::new(ok.sha);
    small.max_library_bytes = 1024;
    assert_eq!(refused(&ok, small), LoadError::TooLarge);
    let mut big_config = LoadConfig::new(ok.sha);
    big_config.module_config = vec![0; 64 * 1024 + 1];
    assert_eq!(refused(&ok, big_config), LoadError::ConfigTooLarge);
}

#[test]
fn a_too_small_buffer_is_retried_once_within_the_limit() {
    let (m, _b) = load_default("retry", &[]);
    let mut out = Vec::new();
    m.call(&req(&n_cmd(b'n', 300), 64, 1024), &mut out)
        .expect("retry succeeds");
    assert_eq!(out, vec![b'x'; 300]);
    // Over the point's limit is refused without a second run.
    let e = m
        .call(&req(&n_cmd(b'n', 2000), 64, 1024), &mut out)
        .expect_err("limit");
    assert_eq!(e.failure, FailureKind::Limit);
    // A buffer that fits first time is not retried: the result is exact.
    m.call(&req(&n_cmd(b'n', 64), 64, 1024), &mut out)
        .expect("fits");
    assert_eq!(out.len(), 64);
    assert_eq!(m.state(), ModuleState::Ready);
}

#[test]
fn malformed_outputs_are_refused_and_three_strikes_fail_the_module() {
    let (m, _b) = load_default("malformed", &[]);
    for (cmd, why) in [
        (&b"l"[..], "more bytes than its buffer"),
        (b"k", "larger buffer than the one it had"),
        (b"u", "undefined status"),
    ] {
        let e = call(&m, cmd).expect_err("malformed");
        assert_eq!(e.failure, FailureKind::OutputMalformed, "{why}");
        assert!(e.reason.contains(why), "{}", e.reason);
        assert!(!e.retriable());
        if m.state() == ModuleState::Failed {
            break;
        }
    }
    // Three strikes inside the window mark it failed: later calls are refused.
    assert_eq!(m.state(), ModuleState::Failed);
    assert_eq!(
        call(&m, b"eabc").expect_err("failed").failure,
        FailureKind::Unavailable
    );
}

#[test]
fn a_module_that_needs_a_bigger_buffer_twice_is_malformed() {
    let (m, _b) = load_default("twice", &[]);
    let e = m
        .call(&req(b"w", 64, 1024), &mut Vec::new())
        .expect_err("twice");
    assert_eq!(e.failure, FailureKind::OutputMalformed);
    assert!(e.reason.contains("twice"));
}

#[test]
fn strikes_age_out_of_the_window() {
    let built = build("strike-window", &desc("1.0", "1.0", 4), &[]);
    let mut cfg = LoadConfig::new(built.sha);
    cfg.policy = Policy {
        strike_limit: 2,
        strike_window: Duration::from_millis(150),
        ..Policy::default()
    };
    let m = NativeModule::load(&built.path, cfg).expect("load");
    assert!(call(&m, b"u").is_err());
    std::thread::sleep(Duration::from_millis(300));
    assert!(call(&m, b"u").is_err());
    assert_eq!(m.state(), ModuleState::Ready, "the first strike aged out");
    assert!(call(&m, b"u").is_err());
    assert_eq!(m.state(), ModuleState::Failed, "two within the window");
}

#[test]
fn an_overrun_fails_the_module_at_once() {
    let (m, _b) = load_default("overrun", &[]);
    let e = call(&m, b"o").expect_err("overrun");
    assert_eq!(e.failure, FailureKind::OutputMalformed);
    assert!(e.reason.contains("past its output buffer"));
    assert_eq!(m.state(), ModuleState::Failed);
    assert_eq!(
        call(&m, b"eabc").expect_err("failed").failure,
        FailureKind::Unavailable
    );
}

#[test]
fn module_status_becomes_a_typed_error_and_its_text_never_reaches_display() {
    let (m, _b) = load_default("status", &[]);
    for code in [
        status::INVALID_ARGUMENT,
        status::UNSUPPORTED,
        status::LIMIT,
        status::NOT_INITIALIZED,
    ] {
        let e = call(&m, &[b'g', code as u8]).expect_err("status");
        assert_eq!((e.failure, e.status), (FailureKind::Error, Some(code)));
        assert!(!e.retriable());
    }
    let e = call(&m, &[b'g', status::MODULE as u8]).expect_err("module");
    assert_eq!(e.detail.as_deref(), Some("boom /etc/shadow"));
    assert!(!e.to_string().contains("shadow"));
    assert!(
        e.to_string()
            .contains("module=test-module point=analyzer:echo status=-6")
    );
    assert_eq!(
        m.state(),
        ModuleState::Ready,
        "a module error is not a strike"
    );
    assert_eq!(
        call(&m, b"").expect_err("empty").status,
        Some(status::INVALID_ARGUMENT)
    );
}

#[test]
fn a_deadline_cancels_cooperatively_and_only_that_call() {
    let (m, _b) = load_default("cancel", &[]);
    let slow = n_cmd(b's', 3000);
    let neighbour = n_cmd(b's', 250);
    let started = Instant::now();
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| {
            let mut out = Vec::new();
            m.call(
                &Request {
                    timeout: Duration::from_millis(100),
                    ..req(&slow, 64, 64)
                },
                &mut out,
            )
        });
        let b = s.spawn(|| {
            let mut out = Vec::new();
            m.call(&req(&neighbour, 64, 64), &mut out)
        });
        (a.join().expect("a"), b.join().expect("b"))
    });
    let e = a.expect_err("cancelled at its deadline");
    assert_eq!(e.failure, FailureKind::Limit);
    assert!(e.reason.contains("cancelled"), "{}", e.reason);
    assert!(b.is_ok(), "the neighbour's call was not cancelled");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        m.state(),
        ModuleState::Ready,
        "a cooperative stop is not a failure"
    );
}

#[test]
fn a_hung_call_fails_the_module_and_quarantines_its_buffers_until_it_returns() {
    let built = build("hang", &desc("1.0", "1.0", 2), &[]);
    let mut cfg = LoadConfig::new(built.sha);
    cfg.policy.grace = Duration::from_millis(100);
    let m = NativeModule::load(&built.path, cfg).expect("load");
    let started = Instant::now();
    let mut out = Vec::new();
    let e = m
        .call(
            &Request {
                timeout: Duration::from_millis(50),
                ..req(&n_cmd(b'h', 800), 4096, 4096)
            },
            &mut out,
        )
        .expect_err("hung");
    assert_eq!(e.failure, FailureKind::Limit);
    assert!(e.reason.contains("marked failed"));
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "the caller was released by the deadline, not by the module"
    );
    assert_eq!(m.state(), ModuleState::Failed);
    assert!(
        m.quarantined_bytes() >= 4096,
        "the lent output buffer is counted: {}",
        m.quarantined_bytes()
    );
    assert_eq!(
        call(&m, b"eabc").expect_err("failed").failure,
        FailureKind::Unavailable
    );
    // When the module finally returns, the buffers are released.
    let end = Instant::now() + Duration::from_secs(5);
    while m.quarantined_bytes() != 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(m.quarantined_bytes(), 0);
}

#[test]
fn a_full_queue_is_busy_and_retriable() {
    let built = build("busy", &desc("1.0", "1.0", 1), &[]);
    let m = NativeModule::load(&built.path, LoadConfig::new(built.sha)).expect("load");
    // One worker, three slots: one running, two queued, the fourth is refused at once.
    let hold = n_cmd(b's', 400);
    let results: Vec<Result<(), CallError>> = std::thread::scope(|s| {
        let running = s.spawn(|| m.call(&req(&hold, 64, 64), &mut Vec::new()));
        std::thread::sleep(Duration::from_millis(60));
        let queued: Vec<_> = (0..2)
            .map(|_| s.spawn(|| m.call(&req(&hold, 64, 64), &mut Vec::new())))
            .collect();
        std::thread::sleep(Duration::from_millis(60));
        let refused = m.call(&req(b"eabc", 64, 64), &mut Vec::new());
        let mut all = vec![refused];
        all.push(running.join().expect("running"));
        all.extend(queued.into_iter().map(|h| h.join().expect("queued")));
        all
    });
    let busy = results[0].as_ref().expect_err("fourth call");
    assert_eq!(busy.failure, FailureKind::Busy);
    assert!(busy.retriable());
    assert!(
        results[1..].iter().all(Result::is_ok),
        "the running and queued calls complete: {results:?}"
    );
    assert_eq!(m.state(), ModuleState::Ready);
}

#[test]
fn a_call_that_cannot_start_before_its_deadline_is_busy_and_does_not_run() {
    let built = build("busy-wait", &desc("1.0", "1.0", 1), &[]);
    let m = NativeModule::load(&built.path, LoadConfig::new(built.sha)).expect("load");
    let hold = n_cmd(b's', 500);
    let e = std::thread::scope(|s| {
        let running = s.spawn(|| m.call(&req(&hold, 64, 64), &mut Vec::new()));
        std::thread::sleep(Duration::from_millis(60));
        let e = m
            .call(
                &Request {
                    timeout: Duration::from_millis(80),
                    ..req(b"eabc", 64, 64)
                },
                &mut Vec::new(),
            )
            .expect_err("busy");
        running.join().expect("running").expect("completes");
        e
    });
    assert_eq!(e.failure, FailureKind::Busy);
    // The abandoned entry is recycled: the module serves normally afterwards.
    assert_eq!(call(&m, b"eabc").expect("served"), b"abc");
    assert_eq!(m.state(), ModuleState::Ready);
}

#[test]
fn many_threads_get_their_own_results_and_distinct_call_ids() {
    let (m, _b) = load_default("concurrent", &[]);
    let ids = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for t in 0..8u8 {
            let (m, ids) = (&m, &ids);
            s.spawn(move || {
                for i in 0..200u32 {
                    let mut input = vec![b'e', t];
                    input.extend_from_slice(&i.to_le_bytes());
                    let out = call(m, &input).expect("echo");
                    assert_eq!(
                        &out[..],
                        &input[1..],
                        "thread {t} call {i} got another call's bytes"
                    );
                    let seen = call(m, b"i").expect("id");
                    ids.lock()
                        .expect("ids")
                        .push(u64::from_le_bytes(seen[..8].try_into().unwrap()));
                }
            });
        }
    });
    let mut ids = ids.into_inner().expect("ids");
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        total,
        "call ids are unique among completed calls"
    );
}

#[test]
fn module_logs_are_bounded_and_counted() {
    let built = build("log", &desc("1.0", "1.0", 4), &[]);
    let lines: Arc<Mutex<Vec<(u32, usize, String)>>> = Arc::default();
    let sink_lines = Arc::clone(&lines);
    let sink: LogSink = Arc::new(move |r| {
        sink_lines
            .lock()
            .expect("lines")
            .push((r.level, r.message.len(), r.module.to_owned()))
    });
    let mut cfg = LoadConfig::new(built.sha);
    cfg.log = Some(sink);
    cfg.policy.log_per_second = 3;
    let m = NativeModule::load(&built.path, cfg).expect("load");
    call(&m, b"L").expect("log call");
    {
        let lines = lines.lock().expect("lines");
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0].0, lines[0].1, lines[0].2.as_str()),
            (3, 5, "test-module")
        );
        assert_eq!(
            (lines[1].0, lines[1].1),
            (2, 1024),
            "a 3000 byte line is cut to 1 KiB"
        );
    }
    call(&m, b"L").expect("log call");
    assert_eq!(
        lines.lock().expect("lines").len(),
        3,
        "the per-second limit passes three"
    );
    assert_eq!(m.dropped_logs(), 1);
}

#[test]
fn close_runs_shutdown_once_after_the_last_call_and_a_hung_call_holds_it_back() {
    let built = build("close", &desc("1.0", "1.0", 2), &[]);
    let m = NativeModule::load(&built.path, with_marker(&built)).expect("load");
    call(&m, b"eabc").expect("served");
    assert!(
        !marker(&built).exists(),
        "no shutdown while the module is loaded"
    );
    assert!(m.close(Duration::from_secs(5)));
    assert!(
        marker(&built).exists(),
        "shutdown ran after the last call returned"
    );

    let built = build("close-hung", &desc("1.0", "1.0", 2), &[]);
    let mut cfg = with_marker(&built);
    cfg.policy.grace = Duration::from_millis(50);
    let m = NativeModule::load(&built.path, cfg).expect("load");
    let mut out = Vec::new();
    let _ = m.call(
        &Request {
            timeout: Duration::from_millis(30),
            ..req(&n_cmd(b'h', 700), 64, 64)
        },
        &mut out,
    );
    assert!(
        !m.close(Duration::from_millis(100)),
        "a worker is still inside the module"
    );
    assert!(
        !marker(&built).exists(),
        "shutdown must not run while a call is outstanding"
    );
    let end = Instant::now() + Duration::from_secs(5);
    while !marker(&built).exists() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        marker(&built).exists(),
        "shutdown ran once the hung call returned"
    );
}

#[test]
fn dropping_a_module_stops_its_workers_and_shuts_it_down() {
    let built = build("drop", &desc("1.0", "1.0", 2), &[]);
    let m = NativeModule::load(&built.path, with_marker(&built)).expect("load");
    call(&m, b"eabc").expect("served");
    drop(m);
    let end = Instant::now() + Duration::from_secs(5);
    while !marker(&built).exists() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(marker(&built).exists());
}

#[test]
fn the_pool_is_sized_by_the_descriptor_and_capped_by_policy() {
    // Three slots per worker means a cap of one worker leaves two queue places only.
    let built = build("cap", &desc("1.0", "1.0", 500), &[]);
    let mut cfg = LoadConfig::new(built.sha);
    cfg.policy.max_workers = 1;
    let m = NativeModule::load(&built.path, cfg).expect("load");
    assert_eq!(m.descriptor().max_concurrent_calls, 500);
    let running = AtomicU32::new(0);
    let hold = n_cmd(b's', 300);
    let busy = std::thread::scope(|s| {
        let handles: Vec<_> = (0..6)
            .map(|_| {
                s.spawn(|| {
                    running.fetch_add(1, Ordering::SeqCst);
                    m.call(&req(&hold, 64, 64), &mut Vec::new())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .filter(|r| matches!(r, Err(e) if e.failure == FailureKind::Busy))
            .count()
    });
    assert!(
        busy >= 1,
        "six callers against one worker and two queue places must see Busy"
    );
}
