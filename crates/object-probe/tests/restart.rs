//! Same-volume boot changes and exact source/deployment negative controls.
use hsearch_object_probe::{model::*, restart};

fn fixture() -> (CapturePlan, Capture, String) {
    let plan = CapturePlan { binding: Binding { digests: [[0; 32]; 7], run: [0; 16], platform: 1 },
        bucket: "bucket".into(), specifications: vec![
            Specification { id: 1, scenario: 4, action: "create".into(), ..Specification::default() },
            Specification { id: 2, parent: 1, scenario: 4, action: "restart".into(), ..Specification::default() },
            Specification { id: 3, parent: 1, scenario: 4, action: "head".into(), ..Specification::default() }] };
    let pairs = [("after_boot", '2'), ("after_cid", '3'), ("after_config", '4'), ("after_image", '5'),
        ("after_volume", '6'), ("before_boot", '1'), ("before_cid", '3'), ("before_config", '4'),
        ("before_image", '5'), ("before_volume", '6')];
    let receipt = "{".to_owned() + &pairs.iter().map(|(k,v)| format!("\"{k}\":\"{}\"", v.to_string().repeat(64))).collect::<Vec<_>>().join(",") + "}";
    let capture = Capture { complete: true, attempts: 3, events: vec![
        CaptureEvent { id: 1, attempt: 1, end: 1, status: 200, boundary: "reply".into(),
            version: "opaque-version".into(), etag: "opaque-etag".into(), request: "original".into(), ..CaptureEvent::default() },
        CaptureEvent { id: 2, attempt: 2, start: 2, end: 3, boundary: "host-restart".into(), auxiliary: receipt.clone(), ..CaptureEvent::default() },
        CaptureEvent { id: 3, attempt: 3, start: 4, end: 5, status: 200, boundary: "reply".into(),
            version: "opaque-version".into(), etag: "opaque-etag".into(), metadata: "original|bound-envelope|caller".into(), ..CaptureEvent::default() }], ..Capture::default() };
    (plan, capture, receipt)
}

#[test]
fn changed_boot_same_volume_and_original_object_have_a_witness() {
    let (plan, capture, receipt) = fixture();
    assert!(restart::verify(&plan, &capture, &receipt));
}

#[test]
fn fresh_volume_image_config_or_container_cannot_be_called_a_restart() {
    let (plan, original, receipt) = fixture();
    for key in ["after_volume", "after_image", "after_config", "after_cid", "after_boot"] {
        let mut capture = original.clone();
        let old = if key == "after_boot" { "2".repeat(64) } else {
            match key { "after_volume" => "6", "after_image" => "5", "after_config" => "4", _ => "3" }.repeat(64)
        };
        let value = if key == "after_boot" { "1".repeat(64) } else { "9".repeat(64) };
        let changed = receipt.replace(&format!("\"{key}\":\"{old}\""), &format!("\"{key}\":\"{value}\""));
        capture.events[1].auxiliary = changed.clone();
        assert!(!restart::verify(&plan, &capture, &changed), "{key}");
    }
}

#[test]
fn replacement_objects_missing_receipt_and_overlapping_restart_refuse() {
    let (plan, original, receipt) = fixture();
    for mode in 0..4 {
        let mut capture = original.clone();
        match mode { 0 => capture.events[2].version = "replacement".into(), 1 => capture.events[1].auxiliary.clear(),
            2 => capture.events[2].start = 2, _ => capture.events[2].metadata = "other-request".into() }
        assert!(!restart::verify(&plan, &capture, &receipt));
    }
}
