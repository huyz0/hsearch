# Materialized engine adapter development

This is the M02.02 typed native component, not a service or a milestone acceptance record. The exact upstream revision is `46e280bc2a208544fb36ecd95932e89cd8bc5618`. Only `hsearch-engine` imports the five existing Lucene crates. No dependency revision, lockfile, inventory, shared selector or ledger is changed by this implementation author.

## Boundary and lifetime

`Engine<'d>` borrows the caller's `Directory`; its `IndexWriter<'d>` cannot outlive that directory. The supported mode is one serial `IndexWriter` with a nonblocking adapter admission gate, the `Lucene104` codec, version 10.5.0, an 8 MiB RAM buffer, eight buffered documents and the frozen StandardAnalyzer profile. Independent private preparations and frozen readers may overlap. Competing apply/refresh/commit/merge admissions return `Busy`. A second native writer must fail its directory lock. ConcurrentIndexWriter, custom codecs, analyzers, writer configuration and remote inputs are not exposed.

The pinned source symbols are `IndexWriter::open`, `set_analyzer`, `set_ram_buffer_size_mb`, `set_max_buffered_docs`, `update_fields_documents`, `delete_documents_by_term`, `commit`, `force_merge`, and `set_commit_on_close`. `index_writer/lifecycle.rs:73` takes `self` in `close`; therefore adapter close consumes the adapter. `set_commit_on_close(false)` makes close roll back pending work. Native `DirectoryReader::open_from_writer` supplies NRT visibility; `DirectoryReader::open` opens committed files. Neither establishes hsearch authority, object/quorum durability, searchable receipts or archive state.

Use a trusted, exclusively owned, already materialized directory. The upstream `Directory::open` returns whole-file bytes and its default `file_length` also opens the file. This component does not make an arbitrary directory implementation, malformed native index, remote source or external concurrent modification safe. Bounded read-at/codec input validation and larger-than-cache qualification remain M02.03. There is no new persisted hsearch file format, WAL or authority protocol.

## Preparation, identity and errors

`Schema::new` requires a positive schema revision, 1..14 unique ASCII user field names (16 native fields including identity/version) of at most 64 bytes, and explicit single-valued required field types. Indexed names starting with `_` are reserved. `Schema::prepare` accepts canonical schema order, stable nonempty caller IDs/requests of at most 256 UTF-8 bytes, positive caller versions, at most 16 distinct identities per batch, 8,192 bytes per document and 65,536 input bytes per batch. Integer values must fit int32, booleans must be typed booleans, and dates must be caller-parsed UTC milliseconds in years 0000..9999. Strings are never coerced. This typed representation does not preserve an original JSON spelling of a date or implement `strict_date_optional_time` parsing.

Preparation completes privately before admission. Invalid middle items cannot touch the writer; dropping a valid preparation represents caller refusal. `apply` is an explicit caller acceptance seam, not an authorization or consensus decision. It validates schema, exact retry payload, monotonic versions and all lifetime limits before the first native mutation. Repeated identities inside this component batch refuse. A future bulk facade must preserve independent semantic item receipts and repeated-ID order using separately resolved component calls; this batch is not the HTTP bulk contract.

Native documents store `_hs_id`, `_hs_version`, and every original typed field. Native internal document numbers never become logical identities. Request history retains exact canonical mutations and original ordered receipts, including missing deletes. A reused request with different payload refuses; a valid retry returns the old receipt without overwriting newer state. Tombstones retain versions. State is bounded to 128 identities, 128 physical documents between merges, 256 admitted lifetime operations and 262,144 history input bytes; no silent history eviction occurs.

`commit` returns an opaque caller-held in-process checkpoint only after native commit succeeds. Reopen requires that checkpoint and compares every live logical identity, version and stored payload, detecting duplicate IDs as well as altered data. Native files alone cannot recover deleted-version/retry truth. The alternative is caller-resolved history replay into a fresh directory. Checkpoints are not serialized or advertised as crash-durable idempotency state.

Native apply, refresh, merge or commit errors can follow partial native effects. Continuation then refuses as `Poisoned`; existing snapshots remain readable. There is no rollback claim for a failed apply/commit. Close consumes the writer and attempts native rollback with commit-on-close disabled. Failures are redacted enum values and never include native error strings or user payloads. A failed native close is returned; it is not retried or declared successful.

## Selected native capabilities

| Capability | Production boundary and pinned dependency | Public/negative evidence |
| --- | --- | --- |
| Keyword, boolean, int32 and date | `Schema`, `Value`; KeywordField/LongField with checked conversion | Public corpus; typed boundary, unknown field and coercion tests |
| Frozen text analyzer | `analyze_text`, StandardAnalyzer; UTF-16 offsets and positions | Four frozen public analyzer cases; byte/token refusal |
| match_all and exact term/terms | `Query::All/Term/Terms`; MatchAllDocsQuery, TermQuery, exact numeric points | Public match-all/keyword/terms/case-sensitive text cases |
| Numeric/date inclusive range | `Query::Range`; PointsRangeQuery | Public integer/date cases; int32/date endpoints and invalid ranges |
| bool must/filter/should/must_not | `Query::Bool`; BooleanQuery with integer threshold | Public integer-threshold case; pure negative and optional scoring roles |
| match and zero-slop phrase | `Query::Match`; analyzer positions and PhraseQuery::with_positions (query.rs:1736) | Public match/phrase/accent cases; retained independent phrase-to-AND negative |
| Local BM25 ranking | Native IndexSearcher, default similarity and text norms | Public ranking order; no score-bit/topology universality claim |
| Sort/projection | `Sort`, `Snapshot::search`; native numeric/string/score comparator, logical-ID tie break | Public field sort and UID projection; complete typed source projection/reopen |
| Exact count | Complete bounded native snapshot, physical scope below native total-hit threshold 1,000 | Public exact totals, zero-size result, finite end-to-end scope |
| Sum/min/max/value_count | `Snapshot::metrics`; native DocValues MetricSpec, finite f64 state, null min/max on empty | Public price metrics and empty result |
| Finite exact key-ordered groups | `Snapshot::groups`; collect at most physical cardinality, require no omitted documents, then check caller budget | Public three-key oracle; independent 128-key overflow refusal |
| Mutations and reader lifecycle | `Engine::apply/refresh/commit/merge/close`; native writer/reader methods | Reopen/replay/rollback/fault tests; retained unsafe speculative-serving-writer oracle |

These are finite development ceilings, not M02.05 workload calibration or performance defaults.

Queries share a 128-node, 128-term, 8,192-byte and depth-eight budget. Each adapter admits at most four retained native readers, released on snapshot drop. Search admits at most 128 hits, four explicit sort keys, 16 projected fields and 32 native segments. Unknown fields and unsupported operators/field combinations refuse before opening search state. No partial or truncated count/group is returned as exact. Aggregate outputs are bounded per requested field/bucket; f64 metrics do not promise arbitrary-precision integer arithmetic.

## Complete fixture/history ownership mapping

`adapter_public.rs` is generated from the exact source hashes in the accepted research envelope, without editing fixtures or expected values. It covers all 14 selected public search/aggregation cases and four analyzer cases using the production adapter. Regenerate and check with `python crates/engine/tests/render_adapter_fixtures.py --root .` and `--check`; the formatter is explicit/replaceable through `--formatter`, and freshness compares formatted bytes exactly. The earlier research projection and its generator remain unchanged. Pinned Java/OpenSearch evidence remains the accepted M02.01 reference, not a newly executed JVM campaign here.

The 11 public mutation histories contain 34 HTTP steps. They are mapped below; this component does not claim to execute HTTP histories or to supply unimplemented facade/authority semantics.

| Public history | Native component evidence or remaining owner |
| --- | --- |
| stable-id-crud | Put/update/delete, exact stored identity/version/payload, missing delete receipt; authoritative latest GET remains M04.03/M06.05/M07.03 |
| create-conflict | Caller resolves create/conditional authority before apply; wire create result/conflict remains M04.03/M06.05 |
| strict-unknown-field | Schema/typed preparation refusal before effects; JSON mapping validation remains M03.03/M06.05 |
| bulk-semantic-isolation | Private whole-batch preparation and ordered component receipts; independent wire item semantics remain M06.05/M07.03 |
| bulk-repeated-id-order | Component batch explicitly refuses repeated IDs; facade resolves ordered per-ID calls, M06.05/M07.03 |
| bulk-malformed-tail | No parser here; typed input and invalid-middle control cover the native effect boundary, NDJSON parser remains M06.05 |
| generated-id-adaptation | Empty identity refuses; no generated-ID capability |
| stale-occ-condition | Strict caller version monotonicity and exact retry replay tested; incarnation-bound opaque authority tokens remain M04.03/M06.05 |
| forced-refresh-adaptation | Explicit native refresh only; forced HTTP refresh and durability/wait receipts remain M06.05/M07.05/M09.08 |
| pit-mutation-repeat-pages | Frozen native snapshots survive mutation and close; quota/TTL/cursor/routing/PIT protocol remains M07.05/M10.01 |
| bounded-total-hit-relation | Complete finite native exact scope; lower-bound/distributed budget response remains M06.05/M07.04 |

All 16 profile-property inputs remain declared future conformance inputs, not native runtime proof. A01 maps to schema limits; A02/A04 to identity/retry state without collection-incarnation authority; A03 to typed effect admission without NDJSON/bulk semantics; A05 to explicit native visibility/commit separation; A06 to bounded IR; A07 to frozen readers without cursor/TTL; A08 to finite exact counts; A09 to overflow refusal; A10/A11/A12/A15 to explicit unsupported operators rather than empty success. A13 logical routing, A14 catalog/maintenance paging and A16 wire discovery/parameter validation remain their declared non-engine owners. No missing HTTP feature is described as a native port or accepted profile amendment.

## Qualification still required

Development uses claim `m02-02-c181b8a44171`, base `667a0c028182d3b0237048d0b9e0c838810b0ca8`, author `01a1ab9f-fde6-7952-9877-efb2fce35b9c`, and the normal stock TaskStore/Sandbox fixture. The complete clean base check passed. Compiler/check failures and normal resume receipts remain in the author's immutable per-attempt artifact directory. Later implementation results must bind their actual source fingerprint, not borrow the base pass.

This file is not final acceptance. The root still sequences actual-main reconciliation, repaired collector qualification, >=90% production coverage controls, native x64/ARM/full profile matrix, final independent correctness and architecture roles, CI and protected integration. Original review base992 and issued0/planned2/hard3 remain unchanged. All M02.09 and earlier failed histories remain frozen. No public push, review packet, workflow, merge, release or provider campaign is performed here.

### Observed development checkpoint and capacity blocker

The unchanged667 baseline passed the complete ordinary native check. Implementation check05 subsequently passed all41workspace tests in both dev and release, doctests, production release-library build, format, Clippy, API/dependency/planning checks, and the unchanged native research repetition controls. Its exact source fingerprint and retained normal command are recorded in `implementation-check-05.json`; it predates the final phrase-position/reader-budget/typed-edge increment.

The final code increment passed normal compile05 and then native format, all-target/all-feature workspace Clippy with warnings denied, API/inventory checks and22dependency negative controls in check06. That check failed while compiling the dev nextest targets, before new tests executed, with the stock worker's exact exception `RuntimeError: Declared cache/target disk budget exceeded`. The frozen scratch budget is8GiB. This is not a semantic test failure, a timeout, a pass, or proof of a particular measured peak; the exception did not retain an exact peak byte count. No quota, command table, collector, fixture assertions or frozen controls were changed to bypass it.

The source and retained failure remain under claim m02-02-c181b8a44171. Normal owner resume reconciles the stopped worker and preserves the fixture. Because only the designated integrator may commit this task, the next step is an integrator-owned clean source checkpoint and normal fixture/claim turnover with the same bounds and preserved history, followed by the complete ordinary check. No unchanged retry or manual cache deletion is authorized by this report. Final increment tests, complete final-tree checks, coverage, ARM and independent reviews remain unqualified.
