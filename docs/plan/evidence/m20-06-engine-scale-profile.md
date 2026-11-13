# M20.06 evidence: engine scale profile

Provisional: measured in a debug build on a shared cloud container, in-memory directory.

- `Limits` rides on the schema. `Limits::BOUNDED` (128 documents, 256 operations, 262,144 history bytes, 32 segments, 8 buffered documents, history refused rather than windowed) is the default; `crates/engine/tests/adapter_scale.rs::default_profile_refuses_at_the_old_numbers` pins every number and the refusal at document 129. All pre-existing engine tests pass unchanged.
- `Limits::scale()` (1,000,000 documents, 4 MiB retry window, 256 segments, 10,000 buffered documents) applies, refreshes, searches, commits, closes and reopens 100,000 documents: count, term, text, sorted page and get all equal the expected values (`scale_profile_holds_searches_and_reopens_a_hundred_thousand_documents`). Whole test about 61 s in debug, of which the load is about 8 s.
- A real defect surfaced and was fixed: the native collector counts exactly only up to 1,000 hits, then reports a lower bound, which the adapter had treated as exact (100,000 documents reported 91,001). A search now takes an exact count pass whenever the collector says the value is a lower bound. Unreachable under the old 128 cap.
- Under the scale profile retry history is a window; request identity beyond it is the log layer's job (M20.03).
