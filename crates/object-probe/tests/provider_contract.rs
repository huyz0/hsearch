//! Preserve the unchanged accepted public-trait oracle and unconditional-CAS
//! negative. This development-only inclusion is not a RustFS provider result.
#[path = "../../object/tests/conformance.rs"]
mod unchanged_public_contract;

#[test]
fn unknown_retry_borrows_every_original_field_without_a_replacement_identity() {
    use hsearch_object::{
        Metadata, Mutation, ObjectKey, Payload, Precondition, RequestId, RetryAction,
    };
    let original = Mutation {
        request_id: RequestId::new(b"original").unwrap(),
        key: ObjectKey::new(b"original-key").unwrap(),
        payload: Payload::new(b"original-payload").unwrap(),
        metadata: Metadata::new(b"original-metadata").unwrap(),
        precondition: Precondition::Absent,
    };
    match hsearch_object::retry_action(&original, hsearch_object::Error::UnknownOutcome) {
        RetryAction::Resolve(value) => assert!(std::ptr::eq(value, &original)),
        _ => panic!("unknown mutation must resolve the unchanged envelope"),
    }
}

#[test]
fn actual_checker_cli_refuses_arguments_without_reflecting_them() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_hsearch-object-probe"))
        .arg("private-input-must-not-be-reflected")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"rustfs-checker-refused:fixed-profile-only\n"
    );
}
