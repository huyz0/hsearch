//! The WASM host against hand-written modules: every limit, every refusal.

use hsearch_module_abi::{CallError, FailureKind, Kind};
use hsearch_module_wasm::{Limits, LoadError, ModuleState, Request, WasmConfig, WasmModule};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

const DESC: &str = r#"{"name":"wasm-t","version":"0.1.0","abi":"1.0","engine_api":"1.0","max_concurrent_calls":2,"io_bytes":65536,"extension_points":[{"kind":"analyzer","id":"t","version":"1.0.0","deterministic":true}]}"#;

/// A module whose first input byte picks the behaviour of `hs_module_call`.
fn module_wat(extra_imports: &str, memory: &str, abi: u32, init_status: i32) -> String {
    module_wat_with(
        DESC,
        (0x1000, 0x11000),
        0,
        extra_imports,
        memory,
        abi,
        init_status,
    )
}

fn module_wat_with(
    descriptor: &str,
    regions: (u32, u32),
    describe_status: i32,
    extra_imports: &str,
    memory: &str,
    abi: u32,
    init_status: i32,
) -> String {
    let n = descriptor.len();
    let (inr, outr) = regions;
    let desc = descriptor.replace('"', "\\\"");
    format!(
        r#"(module
  (import "hsearch_host_v1" "log" (func $log (param i32 i32 i32)))
  (import "hsearch_host_v1" "cancelled" (func $cancelled (result i32)))
  {extra_imports}
  {memory}
  (global $out_len (mut i32) (i32.const 0))
  (global $inited (mut i32) (i32.const 0))
  (data (i32.const 0x21000) "{desc}")
  (func (export "hs_abi_version") (result i32) (i32.const {abi}))
  (func (export "hs_module_init") (param i32) (result i32) (if (i32.eqz (i32.const {init_status})) (then (global.set $inited (i32.const 1)))) (i32.const {init_status}))
  (func (export "hs_module_describe") (result i32)
    (memory.copy (i32.const 0x11000) (i32.const 0x21000) (i32.const {n}))
    (global.set $out_len (i32.const {n}))
    (i32.const {describe_status}))
  (func (export "hs_module_call") (param $kind i32) (param $plen i32) (param $ilen i32) (param $t i32) (result i32)
    (local $mode i32) (local $i i32)
    (if (i32.eqz (global.get $inited)) (then (return (i32.const -7))))
    (local.set $mode (i32.load8_u (i32.add (i32.const 0x1000) (local.get $plen))))
    ;; 'n' report one `cancelled` poll, 'k' poll until it is set then say 1
    (if (i32.eq (local.get $mode) (i32.const 110)) (then
      (i32.store8 (i32.const 0x11000) (call $cancelled)) (global.set $out_len (i32.const 1)) (return (i32.const 0))))
    (if (i32.eq (local.get $mode) (i32.const 107)) (then
      (loop $k (br_if $k (i32.eqz (call $cancelled))))
      (i32.store8 (i32.const 0x11000) (i32.const 1)) (global.set $out_len (i32.const 1)) (return (i32.const 0))))
    ;; 'e' echo the input
    (if (i32.eq (local.get $mode) (i32.const 101)) (then
      (memory.copy (i32.const 0x11000) (i32.add (i32.const 0x1000) (local.get $plen)) (local.get $ilen))
      (global.set $out_len (local.get $ilen)) (return (i32.const 0))))
    ;; 'u' unreachable
    (if (i32.eq (local.get $mode) (i32.const 117)) (then unreachable))
    ;; 'o' out of bounds load
    (if (i32.eq (local.get $mode) (i32.const 111)) (then (drop (i32.load (i32.const 0x7fffff00)))))
    ;; 'l' loop forever
    (if (i32.eq (local.get $mode) (i32.const 108)) (then (loop $l (br $l))))
    ;; 'h' log until the budget is gone
    (if (i32.eq (local.get $mode) (i32.const 104)) (then
      (loop $h (call $log (i32.const 2) (i32.const 0x1000) (i32.const 1)) (br $h))))
    ;; 'c' poll cancelled until it says so
    (if (i32.eq (local.get $mode) (i32.const 99)) (then
      (loop $c (br_if $c (i32.eqz (call $cancelled))))))
    ;; 's' status -6 (module failure) with detail "bad"
    (if (i32.eq (local.get $mode) (i32.const 115)) (then
      (i32.store8 (i32.const 0x11000) (i32.const 98)) (global.set $out_len (i32.const 1)) (return (i32.const -6))))
    ;; 'b' buffer-too-small, 'z' an undefined status, 'r' unbounded recursion, 'd' divide by zero
    (if (i32.eq (local.get $mode) (i32.const 98)) (then (return (i32.const -1))))
    (if (i32.eq (local.get $mode) (i32.const 122)) (then (return (i32.const -99))))
    (if (i32.eq (local.get $mode) (i32.const 114)) (then (drop (call $recurse (i32.const 0)))))
    (if (i32.eq (local.get $mode) (i32.const 100)) (then (drop (i32.div_u (i32.const 1) (i32.sub (local.get $plen) (local.get $plen)))))) 
    ;; 'm' grow the memory by the input's second byte pages, answer with the grow result
    (if (i32.eq (local.get $mode) (i32.const 109)) (then
      (i32.store (i32.const 0x11000) (memory.grow (i32.load8_u (i32.add (i32.const 0x1001) (local.get $plen)))))
      (global.set $out_len (i32.const 4)) (return (i32.const 0))))
    ;; 'x' claim more output than the region holds
    (if (i32.eq (local.get $mode) (i32.const 120)) (then (global.set $out_len (i32.const 0x7fffffff)) (return (i32.const 0))))
    ;; 'w' write a mark into memory that a recycled instance must not carry over: reads then sets it
    (if (i32.eq (local.get $mode) (i32.const 119)) (then
      (i32.store (i32.const 0x11000) (i32.load (i32.const 0x30000)))
      (i32.store (i32.const 0x30000) (i32.const 7))
      (global.set $out_len (i32.const 4)) (return (i32.const 0))))
    (global.set $out_len (i32.const 0))
    (i32.const 0))
  (func $recurse (param i32) (result i32) (i32.add (call $recurse (i32.add (local.get 0) (i32.const 1))) (i32.const 1)))
  (func (export "hs_module_shutdown") (call $log (i32.const 3) (i32.const 0x21000) (i32.const 8)))
  (func (export "hs_input_region") (result i32) (i32.const {inr}))
  (func (export "hs_output_region") (result i32) (i32.const {outr}))
  (func (export "hs_output_len") (result i32) (global.get $out_len))
)"#
    )
}

const MEM: &str = r#"(memory (export "memory") 4 8)"#;

fn good() -> String {
    module_wat("", MEM, 0x10000, 0)
}

fn load_wat(wat: &str, edit: impl FnOnce(&mut WasmConfig)) -> Result<WasmModule, LoadError> {
    let bytes = wat::parse_str(wat).expect("wat");
    let mut config = WasmConfig::new(Sha256::digest(&bytes).into());
    config.limits = Limits {
        max_memory_bytes: 8 * 65536,
        fuel_per_call: 1_000_000,
        ..Limits::default()
    };
    edit(&mut config);
    WasmModule::load_bytes(&bytes, config)
}

fn ok(edit: impl FnOnce(&mut WasmConfig)) -> WasmModule {
    load_wat(&good(), edit).expect("loads")
}

fn req(input: &[u8]) -> Request<'_> {
    Request {
        kind: Kind::Analyzer,
        point: "t",
        input,
        timeout: Duration::from_secs(5),
        output_bound: 256,
        output_limit: 256,
    }
}

fn call(m: &WasmModule, input: &[u8]) -> Result<Vec<u8>, CallError> {
    let mut out = Vec::new();
    m.call(&req(input), &mut out).map(|()| out)
}

#[test]
fn loads_and_echoes() {
    let m = ok(|_| {});
    assert_eq!(call(&m, b"ehello").unwrap(), b"ehello");
    assert_eq!(m.state(), ModuleState::Ready);
}

#[test]
fn a_wrong_hash_is_refused() {
    let bytes = wat::parse_str(good()).unwrap();
    let e = WasmModule::load_bytes(&bytes, WasmConfig::new([0; 32])).expect_err("refused");
    assert!(matches!(e, LoadError::HashMismatch));
}

#[test]
fn only_the_two_host_imports_are_allowed() {
    let wat = module_wat(
        r#"(import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))"#,
        MEM,
        0x10000,
        0,
    );
    let e = load_wat(&wat, |_| {}).expect_err("refused");
    assert!(
        matches!(e, LoadError::ForbiddenImport(ref s) if s.contains("fd_write")),
        "{e:?}"
    );
}

#[test]
fn a_memory_without_a_maximum_or_over_the_limit_is_refused() {
    let e = load_wat(
        &module_wat("", r#"(memory (export "memory") 4)"#, 0x10000, 0),
        |_| {},
    )
    .expect_err("no max");
    assert!(matches!(e, LoadError::MemoryLimit(_)), "{e:?}");
    let e = load_wat(
        &module_wat("", r#"(memory (export "memory") 4 9)"#, 0x10000, 0),
        |_| {},
    )
    .expect_err("over");
    assert!(matches!(e, LoadError::MemoryLimit(_)), "{e:?}");
}

#[test]
fn versions_follow_the_rule_and_are_judged_before_describe() {
    assert!(
        load_wat(&module_wat("", MEM, 0x10000, 0), |c| c.host_abi = 0x10001).is_ok(),
        "an older module minor is fine"
    );
    let e = load_wat(&module_wat("", MEM, 0x20000, 0), |_| {}).expect_err("major");
    assert!(matches!(e, LoadError::AbiRefused { .. }), "{e:?}");
    let e = load_wat(&module_wat("", MEM, 0x10001, 0), |_| {}).expect_err("newer minor");
    assert!(matches!(e, LoadError::AbiRefused { .. }), "{e:?}");
}

#[test]
fn an_init_failure_is_refused() {
    let e = load_wat(&module_wat("", MEM, 0x10000, -4), |_| {}).expect_err("init");
    assert!(matches!(e, LoadError::InitFailed(-4)), "{e:?}");
}

#[test]
fn a_name_that_disagrees_with_the_config_is_refused() {
    let e = load_wat(&good(), |c| c.expected_name = Some("other".into())).expect_err("name");
    assert!(matches!(e, LoadError::DescriptorMismatch(_)), "{e:?}");
}

#[test]
fn a_trap_is_typed_and_the_next_call_still_works() {
    let m = ok(|_| {});
    let e = call(&m, b"u").unwrap_err();
    assert_eq!(e.failure, FailureKind::Trapped);
    let e = call(&m, b"o").unwrap_err();
    assert_eq!(e.failure, FailureKind::Trapped);
    assert_eq!(call(&m, b"ex").unwrap(), b"ex");
}

#[test]
fn fuel_stops_a_loop() {
    let m = ok(|c| c.limits.fuel_per_call = 100_000);
    let e = call(&m, b"l").unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
    assert!(e.reason.contains("fuel"), "{}", e.reason);
}

#[test]
fn the_deadline_stops_a_loop_that_has_fuel_left() {
    let m = ok(|c| c.limits.fuel_per_call = u64::MAX / 2);
    let start = Instant::now();
    let mut out = Vec::new();
    let e = m
        .call(
            &Request {
                timeout: Duration::from_millis(60),
                ..req(b"l")
            },
            &mut out,
        )
        .unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
    assert!(e.reason.contains("deadline"), "{}", e.reason);
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn the_host_call_budget_stops_a_logging_loop() {
    let m = ok(|c| c.limits.fuel_per_call = u64::MAX / 2);
    let e = call(&m, b"h").unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
    assert!(e.reason.contains("host-call"), "{}", e.reason);
}

#[test]
fn a_status_becomes_a_typed_error_and_its_detail_stays_out_of_display() {
    let m = ok(|_| {});
    let e = call(&m, b"s").unwrap_err();
    assert_eq!(e.failure, FailureKind::Error);
    assert!(!e.to_string().contains("bad"));
    assert_eq!(
        m.state(),
        ModuleState::Ready,
        "a status the module chose is not a strike"
    );
}

#[test]
fn memory_grows_within_the_maximum_and_a_grow_past_it_is_a_limit() {
    let m = ok(|_| {});
    let out = call(&m, &[b'm', 2]).unwrap();
    assert_eq!(
        i32::from_le_bytes(out[..4].try_into().unwrap()),
        4,
        "within the maximum"
    );
    let e = call(&m, &[b'm', 50]).unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
    assert!(e.reason.contains("memory"), "{}", e.reason);
    assert_eq!(
        call(&m, b"ek").unwrap(),
        b"ek",
        "a fresh instance serves the next call"
    );
}

#[test]
fn an_oversized_output_length_is_malformed() {
    let m = ok(|_| {});
    assert_eq!(
        call(&m, b"x").unwrap_err().failure,
        FailureKind::OutputMalformed
    );
}

#[test]
fn an_instance_is_not_reused_after_a_trap_so_its_memory_is_fresh() {
    let m = ok(|_| {});
    let first = call(&m, b"w").unwrap();
    assert_eq!(first, 0u32.to_le_bytes());
    // A recycled instance keeps its linear memory between calls: documented, so state may persist
    // across clean calls, but a trapped instance is discarded.
    let _ = call(&m, b"u").unwrap_err();
    let again = call(&m, b"w").unwrap();
    assert_eq!(
        again,
        0u32.to_le_bytes(),
        "a new instance after the trap starts from zeroed memory"
    );
}

#[test]
fn undeclared_points_and_oversized_inputs_are_refused_without_running() {
    let m = ok(|_| {});
    let mut out = Vec::new();
    let e = m
        .call(
            &Request {
                point: "nope",
                ..req(b"e")
            },
            &mut out,
        )
        .unwrap_err();
    assert_eq!(e.failure, FailureKind::Unavailable);
    let big = vec![b'e'; 70_000];
    assert_eq!(call(&m, &big).unwrap_err().failure, FailureKind::Limit);
    let e = m
        .call(
            &Request {
                output_bound: 0,
                ..req(b"e")
            },
            &mut out,
        )
        .unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
}

#[test]
fn a_result_over_the_points_bound_is_a_limit_and_keeps_the_instance() {
    let m = ok(|_| {});
    let mut out = Vec::new();
    let e = m
        .call(
            &Request {
                output_bound: 2,
                output_limit: 2,
                ..req(b"eabc")
            },
            &mut out,
        )
        .unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
    assert!(out.is_empty());
}

#[test]
fn three_strikes_fail_the_module_and_it_stays_failed() {
    let m = ok(|_| {});
    for _ in 0..3 {
        assert_eq!(call(&m, b"u").unwrap_err().failure, FailureKind::Trapped);
    }
    assert_eq!(m.state(), ModuleState::Failed);
    assert_eq!(
        call(&m, b"e").unwrap_err().failure,
        FailureKind::Unavailable
    );
}

#[test]
fn strikes_age_out_of_the_window() {
    let m = ok(|c| c.limits.strike_window = Duration::from_millis(150));
    for _ in 0..2 {
        let _ = call(&m, b"u");
    }
    std::thread::sleep(Duration::from_millis(250));
    let _ = call(&m, b"u");
    assert_eq!(m.state(), ModuleState::Ready);
}

#[test]
fn many_threads_get_their_own_results() {
    let m = std::sync::Arc::new(ok(|_| {}));
    let mut handles = Vec::new();
    for t in 0..8u8 {
        let m = m.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..200u32 {
                let input = [b'e', t, (i % 251) as u8];
                let mut out = Vec::new();
                loop {
                    match m.call(&req(&input), &mut out) {
                        Ok(()) => break,
                        Err(e) if e.failure == FailureKind::Busy => std::thread::yield_now(),
                        Err(e) => panic!("{e}"),
                    }
                }
                assert_eq!(out, input);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn a_call_that_cannot_get_an_instance_before_its_deadline_is_busy() {
    let m = std::sync::Arc::new(ok(|c| c.limits.fuel_per_call = u64::MAX / 2));
    // The descriptor allows two concurrent calls: two long loops hold both instances.
    let holders: Vec<_> = (0..2)
        .map(|_| {
            let m = m.clone();
            std::thread::spawn(move || {
                let mut out = Vec::new();
                let _ = m.call(
                    &Request {
                        timeout: Duration::from_millis(600),
                        ..req(b"l")
                    },
                    &mut out,
                );
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(150));
    let mut out = Vec::new();
    let e = m
        .call(
            &Request {
                timeout: Duration::from_millis(50),
                ..req(b"e")
            },
            &mut out,
        )
        .unwrap_err();
    assert_eq!(e.failure, FailureKind::Busy);
    for h in holders {
        h.join().unwrap();
    }
}

#[test]
fn cancelled_is_false_before_the_deadline_and_true_after_it() {
    let m = ok(|c| c.limits.fuel_per_call = u64::MAX / 2);
    assert_eq!(
        call(&m, b"n").unwrap(),
        [0],
        "a generous deadline: not cancelled"
    );
    let start = Instant::now();
    let mut out = Vec::new();
    let r = m.call(
        &Request {
            timeout: Duration::from_millis(80),
            ..req(b"k")
        },
        &mut out,
    );
    assert!(
        start.elapsed() >= Duration::from_millis(60),
        "{:?}",
        start.elapsed()
    );
    assert!(
        start.elapsed() < Duration::from_millis(600),
        "{:?}",
        start.elapsed()
    );
    match r {
        Ok(()) => assert_eq!(out, [1], "the guest saw `cancelled` and returned"),
        Err(e) => assert!(
            e.reason.contains("deadline"),
            "or the epoch got there first: {}",
            e.reason
        ),
    }
}

fn refused(wat: &str, edit: impl FnOnce(&mut WasmConfig)) -> LoadError {
    load_wat(wat, edit).expect_err("refused")
}

#[test]
fn descriptor_and_region_disagreements_are_refused() {
    let with = |d: &str, r, st| module_wat_with(d, r, st, "", MEM, 0x10000, 0);
    let regions = (0x1000, 0x11000);
    assert!(matches!(
        refused(&with(DESC, regions, -1), |_| {}),
        LoadError::BadDescriptor(_)
    ));
    assert!(matches!(
        refused(
            &with(
                &DESC.replace(r#""engine_api":"1.0""#, r#""engine_api":"2.0""#),
                regions,
                0
            ),
            |_| {}
        ),
        LoadError::EngineApiRefused(_)
    ));
    assert!(matches!(
        refused(
            &with(
                &DESC.replace(r#""abi":"1.0""#, r#""abi":"1.1""#),
                regions,
                0
            ),
            |_| {}
        ),
        LoadError::DescriptorMismatch(_)
    ));
    assert!(
        matches!(
            refused(&with(&DESC.replace("65536", "262144"), regions, 0), |_| {}),
            LoadError::DescriptorMismatch(_)
        ),
        "regions past the memory"
    );
    assert!(
        matches!(
            refused(&with(DESC, regions, 0), |c| c.limits.max_io_bytes = 1024),
            LoadError::DescriptorMismatch(_)
        ),
        "over the host's io limit"
    );
    assert!(
        matches!(
            refused(&with(DESC, (0x11000, 0x11000), 0), |_| {}),
            LoadError::DescriptorMismatch(_)
        ),
        "overlap"
    );
    assert!(matches!(
        refused(&with("{not json", regions, 0), |_| {}),
        LoadError::BadDescriptor(_)
    ));
}

#[test]
fn an_oversized_config_and_oversized_files_are_refused() {
    assert!(matches!(
        refused(&good(), |c| c.module_config = vec![0; 70_000]),
        LoadError::ConfigTooLarge
    ));
    let bytes = wat::parse_str(good()).unwrap();
    let mut c = WasmConfig::new(Sha256::digest(&bytes).into());
    c.limits.max_wasm_bytes = 100;
    assert!(matches!(
        WasmModule::load_bytes(&bytes, c),
        Err(LoadError::TooLarge)
    ));
}

#[test]
fn a_module_that_is_not_valid_wasm_or_lacks_an_export_is_refused() {
    let junk = b"definitely not wasm";
    let config = WasmConfig::new(Sha256::digest(junk).into());
    assert!(matches!(
        WasmModule::load_bytes(junk, config),
        Err(LoadError::Invalid(_))
    ));
    let no_call = good().replace("hs_module_call", "hs_module_other");
    assert!(matches!(refused(&no_call, |_| {}), LoadError::BadExport(_)));
    let wrong_type = good().replace(
        r#"(func (export "hs_output_len") (result i32)"#,
        r#"(func (export "hs_output_len") (result i64) (drop) (i64.const 0)) (func (result i32)"#,
    );
    assert!(matches!(
        refused(&wrong_type, |_| {}),
        LoadError::Invalid(_) | LoadError::BadExport(_)
    ));
    let no_memory = good().replace(r#"(memory (export "memory") 4 8)"#, "(memory 4 8)");
    assert!(matches!(
        refused(&no_memory, |_| {}),
        LoadError::BadExport("memory")
    ));
}

#[test]
fn loading_from_a_path_refuses_what_is_not_a_regular_file() {
    let dir = std::env::temp_dir().join(format!("hs-wasm-load-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("m.wasm");
    let bytes = wat::parse_str(good()).unwrap();
    std::fs::write(&file, &bytes).unwrap();
    let cfg = || {
        let mut c = WasmConfig::new(Sha256::digest(&bytes).into());
        c.limits = Limits {
            max_memory_bytes: 8 * 65536,
            ..Limits::default()
        };
        c
    };
    let m = WasmModule::load(&file, cfg()).expect("a regular file loads");
    assert_eq!(m.descriptor().name, "wasm-t");
    assert!(format!("{m:?}").contains("wasm-t"));
    assert!(matches!(
        WasmModule::load(&dir, cfg()),
        Err(LoadError::Io(_))
    ));
    assert!(matches!(
        WasmModule::load(&dir.join("absent.wasm"), cfg()),
        Err(LoadError::Io(_))
    ));
    let mut small = cfg();
    small.limits.max_wasm_bytes = 10;
    assert!(matches!(
        WasmModule::load(&file, small),
        Err(LoadError::TooLarge)
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn every_load_error_names_itself_and_version_refusals_are_typed() {
    let all = [
        LoadError::Io("x"),
        LoadError::TooLarge,
        LoadError::HashMismatch,
        LoadError::Invalid("x"),
        LoadError::ForbiddenImport("a.b".into()),
        LoadError::BadExport("x"),
        LoadError::MemoryLimit("x"),
        LoadError::AbiRefused {
            module: 0x20000,
            host: 0x10000,
        },
        LoadError::ConfigTooLarge,
        LoadError::Trapped("x"),
        LoadError::InitFailed(-6),
        LoadError::BadDescriptor("x"),
        LoadError::DescriptorMismatch("x"),
        LoadError::EngineApiRefused(hsearch_module_abi::Version { major: 2, minor: 0 }),
    ];
    for e in &all {
        assert!(e.to_string().starts_with("wasm module refused"), "{e}");
    }
    assert_eq!(all[7].failure_kind(), FailureKind::VersionRefused);
    assert_eq!(all[13].failure_kind(), FailureKind::VersionRefused);
    assert_eq!(all[2].failure_kind(), FailureKind::Unavailable);
}

#[test]
fn statuses_stack_exhaustion_and_arithmetic_traps_are_typed() {
    let m = ok(|_| {});
    assert_eq!(
        call(&m, b"b").unwrap_err().failure,
        FailureKind::Limit,
        "buffer-too-small from the module"
    );
    assert_eq!(
        call(&m, b"z").unwrap_err().failure,
        FailureKind::OutputMalformed,
        "undefined status"
    );
    let e = call(&m, b"r").unwrap_err();
    assert_eq!(
        (e.failure, e.reason),
        (FailureKind::Trapped, "the module overflowed its stack")
    );
    let e = call(&m, b"d").unwrap_err();
    assert_eq!(
        (e.failure, e.reason),
        (FailureKind::Trapped, "the module trapped")
    );
}

#[test]
fn a_huge_timeout_is_refused_and_instances_are_counted() {
    let m = ok(|_| {});
    let mut out = Vec::new();
    let e = m
        .call(
            &Request {
                timeout: Duration::MAX,
                ..req(b"e")
            },
            &mut out,
        )
        .unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
    assert_eq!(m.live_instances(), 1, "the load-time instance only");
}

#[test]
fn module_logs_are_rate_limited_and_counted_against_the_budget() {
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink: hsearch_module_abi::LogSink = {
        let seen = seen.clone();
        std::sync::Arc::new(move |r: &hsearch_module_abi::LogRecord<'_>| {
            assert_eq!(r.module, "wasm-t");
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })
    };
    let m = ok(|c| {
        c.log = Some(sink);
        c.limits.log_per_second = 5;
        c.limits.fuel_per_call = u64::MAX / 2;
    });
    let e = call(&m, b"h").unwrap_err();
    assert_eq!(e.failure, FailureKind::Limit);
    assert_eq!(
        seen.load(std::sync::atomic::Ordering::Relaxed),
        5,
        "256 log calls, five delivered"
    );
}

#[test]
fn a_full_queue_is_busy_at_once() {
    let m = std::sync::Arc::new(ok(|c| c.limits.fuel_per_call = u64::MAX / 2));
    let holders: Vec<_> = (0..6)
        .map(|_| {
            let m = m.clone();
            std::thread::spawn(move || {
                let mut out = Vec::new();
                let _ = m.call(
                    &Request {
                        timeout: Duration::from_millis(800),
                        ..req(b"l")
                    },
                    &mut out,
                );
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(250));
    let start = Instant::now();
    let mut out = Vec::new();
    let e = m
        .call(
            &Request {
                timeout: Duration::from_secs(5),
                ..req(b"e")
            },
            &mut out,
        )
        .unwrap_err();
    assert_eq!(e.failure, FailureKind::Busy);
    assert!(
        start.elapsed() < Duration::from_millis(300),
        "refused without waiting: {:?}",
        start.elapsed()
    );
    for h in holders {
        h.join().unwrap();
    }
}

#[test]
fn every_instance_is_initialised_including_replacements_and_second_instances() {
    let m = std::sync::Arc::new(ok(|c| c.limits.fuel_per_call = u64::MAX / 2));
    // A replacement after a trap must be initialised: the module answers -7 until init ran.
    let _ = call(&m, b"u").unwrap_err();
    assert_eq!(call(&m, b"ehi").unwrap(), b"ehi");
    // A second instance, made while the first is busy, must be initialised too.
    let holder = {
        let m = m.clone();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = m.call(
                &Request {
                    timeout: Duration::from_millis(400),
                    ..req(b"l")
                },
                &mut out,
            );
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(call(&m, b"ehi").unwrap(), b"ehi");
    assert_eq!(m.live_instances(), 2);
    holder.join().unwrap();
}

#[test]
fn a_callers_short_deadline_never_marks_the_module_failed() {
    let m = ok(|c| c.limits.fuel_per_call = u64::MAX / 2);
    for _ in 0..5 {
        let mut out = Vec::new();
        let e = m
            .call(
                &Request {
                    timeout: Duration::from_millis(5),
                    ..req(b"l")
                },
                &mut out,
            )
            .unwrap_err();
        assert!(e.reason.contains("deadline"), "{}", e.reason);
    }
    assert_eq!(m.state(), ModuleState::Ready);
    assert_eq!(call(&m, b"ehi").unwrap(), b"ehi");
}

#[test]
fn a_start_function_that_never_ends_is_stopped_by_the_load_timeout() {
    let wat = good().replace(
        "(func (export \"hs_abi_version\")",
        "(func $s (loop $l (br $l)))\n  (start $s)\n  (func (export \"hs_abi_version\")",
    );
    let start = Instant::now();
    let e = load_wat(&wat, |c| {
        c.limits.fuel_per_call = u64::MAX / 2;
        c.limits.load_timeout = Duration::from_millis(100);
    })
    .expect_err("refused");
    assert!(matches!(e, LoadError::Trapped(_)), "{e:?}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn dropping_the_module_runs_shutdown_once_and_log_text_is_cleaned() {
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(u32, String)>::new()));
    let sink: hsearch_module_abi::LogSink = {
        let lines = lines.clone();
        std::sync::Arc::new(move |r: &hsearch_module_abi::LogRecord<'_>| {
            lines.lock().unwrap().push((r.level, r.message.to_owned()));
        })
    };
    let m = ok(|c| c.log = Some(sink));
    assert!(lines.lock().unwrap().is_empty());
    drop(m);
    let seen = lines.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "shutdown ran once: {seen:?}");
    assert_eq!(seen[0].0, 3);
    assert!(seen[0].1.starts_with("{\"name\""), "{:?}", seen[0].1);
    assert!(!seen[0].1.chars().any(char::is_control));
}

#[test]
fn a_panicking_log_sink_does_not_leak_the_instance_slot() {
    let sink: hsearch_module_abi::LogSink =
        std::sync::Arc::new(|_: &hsearch_module_abi::LogRecord<'_>| panic!("sink"));
    let m = ok(|c| c.log = Some(sink));
    for _ in 0..3 {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(&m, b"h")));
        assert!(r.is_err(), "the panic reaches the caller");
    }
    assert!(
        m.live_instances() <= 1,
        "slots were released: {}",
        m.live_instances()
    );
}
