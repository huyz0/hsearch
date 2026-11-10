# Direct-image RustFS capability profile

This is diagnostic source, not provider qualification, production readiness or
M03.02 acceptance. The server remains the digest-pinned development deployment
in `dev/images.json` and the existing private Compose topology. This profile
does not build RustFS, import an SDK, change a pin, or replace `dev/probe.py`.

The complete fixed program currently has 154 invocation/refusal records. Its
plan, deterministic seeds, participants, operation identities, keys, parent
references, ranges and fault boundaries are frozen before capture. Source,
transport, checker binary, actual image, config, topology, platform and program
bindings are independently supplied by the owner. Version tokens are full opaque
values; ETag equality is not a body checksum or immutable generation identity.

| Area | Captured operations and independent check |
| --- | --- |
| Create/update | Two16-client barriers, at least8 actual calls started before the first completion in each race, exact signed conditions, one legal winner and retained contents. Serialized sends, unconditional overwrite and two successors cannot pass. |
| Lost responses | A complete reply intentionally withheld from the caller and a distinct socket-close-after-send case. Every version listing, exact-version HEAD and payload read is an explicit planned attempt. A miss, truncated listing, failed read or delayed acceptance stays inconclusive. |
| Identity | Internal request identity and full original envelope are signed metadata on the mutation. The envelope covers bucket/key, whole body digest, caller metadata, original condition, upload identity and ordered completion descriptor where applicable. Internal identity metadata is segregated from caller metadata. A historical matching object alone does not establish replay safety or changed-ID rejection. |
| Version/range/ABA | Independent selected-byte hashes, old versions after updates and ABA, invalid ranges, an exact original replay after ABA and a fresh request with a stale original version condition. Repeated ETags never establish exact VersionId CAS. |
| Restart | Known client removal before the host restart hook; actual CID/image/config/data-volume equality, changed boot identity, original metadata and exact-version bytes after restart. No socket enters the guest. This is process restart, not power-loss durability. |
| Multipart | Initiation identities; ordered parts and replacement of a repeated part; explicit reversed and duplicate completion descriptors; correct completion; selected bytes across part boundaries; abort/NoSuchUpload; lost completion; retained orphan visibility and scoped abort/list cleanup. Required descriptor identity is not inferred from identical final bytes or initiation metadata. |
| Eligibility | Six separately reported supported/failed/inconclusive areas. Missing, truncated, unsupported, unimplemented, exhausted or illegal required evidence returns nonzero readiness. No scenario is optional. |


The prospective `rustfs-capabilities-v3` profile requires all16 distinct clients
and at least8 starts strictly before the earliest completion, using actual
half-open HTTP call intervals. Equal start/completion timestamps do not overlap.
Both create and update races retain exact single-winner, retained-content,
condition, identity and integrity checks across all16 clients. Missing clients,
invalid timestamps, sequential calls and cohorts smaller than8 cannot qualify.
This measures a many-client race without requiring all16 intervals to share one
instant, which is sensitive to scheduling after barrier release. It changes no
atomicity requirement, client count, sample count, cell count or resource cap.

The criterion is frozen in the plan JSON and the program digest alongside the
profile and all operations before capture. Source and checker hashes also bind
the evaluator. Binary framing remains version two. Old d846 x64 evidence stays
incomplete under its original all16 common-intersection criterion; it is not
reinterpreted by this profile. Fresh captures and full qualification are required.
No timestamps are padded or rewritten and no completion is delayed to pass.

## Provider evidence versus diagnostic limitations

The client attempts a bounded S3 version-history recovery strategy. It does not
assume that a dedicated Resolve RPC is necessary. Up to16 versions are admitted
for an exact task key, followed by separately recorded HEADs and an exact-version
read matched to the original atomically stored request/envelope metadata.
Missing candidate slots are explicit nontransmitted not-applicable records, not
omitted I/O. There is no implicit network retry or latest-read substitution.

This strategy has limits that the experiments must expose. A terminal listing
cannot disprove a still-delayed commit. Scan-then-write cannot establish at-most-
once concurrent retry. A reused ETag cannot provide exact-version CAS under ABA.
Initiation metadata does not automatically bind a later multipart descriptor.
Matching a key does not establish rejection of an identity reused on another
key. The workload includes separate decisive cases for these requirements.
An actual observed violation is a failed required capability; a missing client
route or absent proof is inconclusive, not an observed RustFS limitation.

Source feasibility was investigated against RustFS1.0.4 source
`46337942d55f3a8e6a97b147ca98d9ea8115555d`: version listing in
`bucket_usecase.rs:2868/3120`, version HEAD in `head.rs:339/526`, version GET in
`get.rs:2618`, and ETag conditions in `storage/options.rs:316`. Its historical
same-object-copy expected-version extension is not assumed to be a general PUT
VersionId CAS. These are source references, not runtime evidence or proof that
the pinned image satisfies the full public contract.

## Bounds and framing

The original ceilings remain16 clients,256 operations,512 attempt/refusal
records,1,024 atoms,704KiB data plus64KiB diagnostics,100,000 checker transitions,
ten seconds of cooperative model checking and64MiB declared checker working
storage. Old version-one histories and all original checker controls remain;
legacy restart/auxiliary evidence still returns UnmodeledEvidence. Version two
adds explicit request-producer references and complete auxiliary models.

Each attempt intent is emitted before its send, then its completion is flushed.
A producer crash leaves the intent and charged partial logs, never rollback.
The independent decoder checks intent/completion association, every extent,
tag/count, mandatory terminal marker and separately authenticated plan hash.
Completed HTTP request bytes and received HTTP headers/chunk framing/body are
accounted separately from body-only digests. A failed partial send has only an
acknowledged-written lower bound, with an explicit unknown boundary. TLS/kernel
overhead is not claimed measured by these counters.

The current 154-record plan reserves679,936 bytes conservatively (16KiB for each
bounded version list,4KiB for other records). This is not emitted data or an RSS
measurement. A HOST counterfactual fixture measured 10,320 plan bytes, 32,366 partial-capture bytes and 46,718 complete-capture bytes; these are fake transport records, not provider measurements. The stricter serialized intent/completion pool is384KiB. Fixed
history prefix/terminal framing is at most128 bytes; the monitor footer is at
most4KiB. Including the separate64KiB diagnostic allowance remains below the
existing1MiB command half of a2MiB stock source packet. No V4 binary artifact
slot or increased stdout budget is used. Exhaustion refuses and preserves an
incomplete capture; it never drops a record to manufacture eligibility.

Plans/configs are immutable bounded inputs (64KiB each), individual history
inputs are at most704KiB, and a phase's complete input set is at most1MiB.
Callback admission includes retained plans, histories, partials, input slots and
orphan JSON within the existing64MiB per-claim ceiling before publishing a new
slot. It never treats a small JSON hash wrapper as payment for uncharged bytes.

## Serial resource and source ownership

Build the std-only private checker under BUILD:2 CPU,4GiB RAM,256 PIDs,8GiB disk.
Bind its actual binary hash to source/lock/toolchain and native architecture.
Known build removal precedes either client. The two client phases retain PROBE:
0.5 CPU,256MiB RAM,64 PIDs and **1MiB** scratch. Their only writable path is the
bounded `/tmp` tmpfs. The source, harness, inputs, checker cache, CA and credentials
are read-only; cargo cache is absent. No binary is copied into client scratch.
All writable build paths, including build tmpfs, are monitored. Known client
removal precedes the independent checker stage under BUILD. STORAGE remains
1 CPU,2GiB RAM,256 PIDs and10GiB disk throughout. No additional pool or concurrent
uncharged validator is introduced. Actual physical fit/RSS still needs proof.

The combined source connects `tools/task.py`, `tools/tests/test_task.py` and
`tools/coverage.py`. The fixed `rustfs-capabilities` task command uses normal
TaskStore claim, source fingerprint, lifecycle, evidence and resource admission.
Internal build/client/checker modes cannot be selected as arbitrary public task
commands. Input publication reserves actual bytes before writing a phase slot;
all retained slots, partial/orphan files and evidence share the existing64MiB
claim ceiling. Repeated profile dispatch refuses rather than replaying an
unknown or already attempted campaign. Primary profile command evidence stores
both Docker log pipes as tagged raw bytes (base64 plus SHA-256) before dependent
decoding. Their aggregate shares the existing1MiB command half; the monitor still
explicitly merges its child's stdout/stderr. Non-UTF8 bytes are preserved in
failed evidence, while live parsing requires strict UTF8 stdout and empty Docker
stderr. The `rustfs-raw-streams-v1` execution tag is required. Old failed packets
retain their `base64-of-utf8-output` tag and original source binding; their reader
claims decoded UTF8 only, never retroactive raw-byte fidelity. This keeps
worst-case control characters within the original packet instead of letting JSON
escaping exceed it. The validator still receives the original output unchanged;
malformed or oversized framing continues to refuse. Base64 is not sanitization:
secret checks inspect decoded raw bytes before retention and again across all
records before any public export file is created. Secret-bearing output retains
only failure codes, byte counts and digests. Existing transport checks still
reject actual access/secret values and auth-like provider fields before recording.

The hosted entrypoint requires selected sandbox jobs and an exact clean head,
creates its normal claim in job-local state and uses the job's shared registry.
Public evidence is exported under `hsearch-rustfs-capabilities`; the private
registry, snapshots, credentials and TLS material are never uploaded. A complete
real negative report can finish the investigation while keeping readiness false.
Any inconclusive scenario, partial record, missing proof or failed cleanup still
fails qualification. No observed real outcome is claimed by this source text.

Checker coverage uses an explicit diagnostic-only package scope and separate
output. It cannot mix with the production aggregate, exclude production code,
replace a required native suite or change any90% floor or zero-coverable N/A
rule. The ordinary workspace tests, lints and builds still include the checker.

Complete ordinary dev/release tests, doctests, lint/build, checker coverage,
production coverage, native x64/ARM, real pinned-image matrix, required hosted
jobs/ci-ok, final-source independent review and protected integration remain
required. HOST fixtures, source feasibility and source-checkpoint commits prove
none of those unrun results. OriginalM03.02 scope/base/issued0/planned2/hard3,
all historical failures, other task gates and39GiB retained leases are unchanged.
