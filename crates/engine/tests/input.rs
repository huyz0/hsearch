//! Compile and bounded-input smoke checks against the exact upstream revision.

use hsearch_engine::Input;
use std::sync::Arc;

#[test]
fn pinned_input_supports_shared_bounded_slices() {
    hsearch_testkit::assert_send_sync::<Input>();
    let input = Arc::new(Input::Owned(vec![11, 22, 33, 44]));
    let slice = Input::slice_of(&input, 1, 2).unwrap();
    drop(input);
    assert_eq!(&*slice, &[22, 33]);
}

#[test]
fn pinned_input_rejects_out_of_bounds_and_overflow() {
    let input = Arc::new(Input::Owned(vec![1, 2]));
    assert!(Input::slice_of(&input, 2, 1).is_err());
    assert!(Input::slice_of(&input, usize::MAX, 1).is_err());
    assert!(Input::slice_of(&input, 2, 0).unwrap().is_empty());
}
