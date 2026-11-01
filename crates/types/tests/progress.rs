//! Independent checkpoint-ordering and overflow controls.

use hsearch_types::{InvalidProgress, Progress, Sequence};

#[test]
fn exhaustive_small_prefixes_match_independent_ordering_oracle() {
    for committed in 0..4 {
        for applied in 0..4 {
            for searchable in 0..4 {
                for archived in 0..4 {
                    let actual = Progress::try_new(
                        Sequence::new(committed),
                        Sequence::new(applied),
                        Sequence::new(searchable),
                        Sequence::new(archived),
                    );
                    let valid = [applied, searchable, archived]
                        .into_iter()
                        .all(|x| x <= committed)
                        && searchable <= applied;
                    assert_eq!(actual.is_ok(), valid);
                    if let Ok(progress) = actual {
                        assert_eq!(progress.committed().get(), committed);
                        assert_eq!(progress.applied().get(), applied);
                        assert_eq!(progress.searchable().get(), searchable);
                        assert_eq!(progress.archived().get(), archived);
                    }
                }
            }
        }
    }
}

#[test]
fn independent_consumers_and_u64_boundary() {
    let max = Sequence::new(u64::MAX);
    let zero = Sequence::default();
    let progress = Progress::try_new(max, zero, zero, max).unwrap();
    assert_eq!(progress.archived(), max);
    assert_eq!(
        Progress::default(),
        Progress::try_new(zero, zero, zero, zero).unwrap()
    );
    assert_eq!(zero.checked_next(), Some(Sequence::new(1)));
    assert_eq!(max.checked_next(), None);
}

#[test]
fn invalid_relationships_have_stable_errors() {
    let zero = Sequence::new(0);
    let one = Sequence::new(1);
    assert_eq!(
        Progress::try_new(zero, one, zero, zero),
        Err(InvalidProgress::AppliedAheadOfCommit)
    );
    assert_eq!(
        Progress::try_new(one, zero, one, zero),
        Err(InvalidProgress::SearchableAheadOfApplied)
    );
    assert_eq!(
        Progress::try_new(zero, zero, zero, one),
        Err(InvalidProgress::ArchivedAheadOfCommit)
    );
}
