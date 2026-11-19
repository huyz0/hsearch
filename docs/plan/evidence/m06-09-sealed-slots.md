# M06.09: seal commit payloads under a wrapped DEK

`provisional`: in-memory store, in-memory test KMS, one shared 4-vCPU x86_64 VM. **No RustFS run** (no Docker daemon in this environment) and no network KMS, so nothing here says anything about latency, a real KMS bill or an object store. Request and wrap counts are exact for the seeds used; throughput is relative.

## What exists

`hsearch-lane::sealing` is a seam, `hsearch-seal::SlotSealer` implements it. The lane never sees a key.

- **`SlotCipher`** (lane): `seal` replaces the inline payload of each `Put` record of a slot before the one conditional create; `open` reverses it on replay. The envelope, position, token and each record's index, sequence and operation stay in the clear, because the lane admits, orders and fences on them. A fence, a seal record and a delete are untouched.
- **`LaneWriter::open_sealed` / `open_with_head_sealed`** replay with plaintext records handed to `visit` and make every later `append` seal. `read_log_sealed` is the read-only form. A slot that does not authenticate aborts the open with `Error::Seal`: the lane is not opened over history it cannot read. A refusal while sealing writes nothing and does not poison the writer. The committer needs no change to take sealed payloads: it appends through the writer it is given.
- **Sealed payload** (inline bytes): `flags | [wrapper] | ciphertext | tag(16)`; AES-256-GCM; nonce = eight zero bytes then the record position (u32 big endian); AAD = the format's lane record AAD (domain, activation, lane, ordinal, position, index) then `seq`, the flags and a SHA-256 of every record's clear fields in the slot (count, index, seq, op, payload kind, external location), so a tail dropped or an operation rewritten stops every sealed payload of the slot opening (review finding, tested). The wrapper (key ref, version, wrapped key) rides in the first record of its key in the slot; the provider's authenticated context names the carrying index, the crypto domain and an object id derived from the slot position and that index.
- **Fresh key per slot, never reused.** One data key per slot per key group; a refused commit that is repeated seals again under a new key; a lost reply that landed resends the bytes the lane already holds (identical) and is resolved by identity.
- **Setting `DekScope`** (cluster default `PerKey`, overridable per index through `SlotScopes::dek_scope`): `PerKey` shares one data key per slot among all indexes with the same key reference and crypto domain; `PerIndex` wraps one per index per slot. The choice is recorded in the payload flags and replay honours what each payload recorded, so a lane may hold slots of both.
- `SlotSealer::overhead` states the most a sealed payload can exceed its plaintext by; `SlotStats` counts slots, records, wraps and bytes.

## Why the setting exists (the measured downside)

The first cut wrapped one key per index per slot, which is what the deliverable literally says. The sparse-tenant row below is why that is not the default: one KMS wrap per record and +11% slot bytes. `PerKey` fixes both and costs one thing: the provider binds the wrapped key to the index that carries it, not to the other indexes of the slot. Each record is still bound to its own index by the payload AAD (a test reads each record as another tenant of the same key and is refused), and a shared lane already holds one key identity for all its members (a BYOK index has a dedicated lane), so a node that replays the lane reads every member's records regardless. `PerIndex` remains for a deployment that wants the provider's per-index context on every wrap, at the cost below.

## Tests (`crates/seal/tests/slots.rs`, 28; `compare.rs`, 2)

Each behaviour test runs under both scopes. Nothing was claimed from a green run alone; the mutations below were applied one at a time to `slot.rs` and the suite rerun.

| behaviour | held by |
|---|---|
| Stored slot holds no keyed plaintext; unkeyed index stored as given; replay (reopen, `read_log_sealed`, head-driven open) returns exactly what was committed; a reopened writer keeps sealing | `every_acknowledged_slot_is_ciphertext_and_replay_opens_it`, `a_head_driven_open_replays_sealed_slots_too` |
| Every single-bit flip (two bits, every byte of both payloads, wrapper included, 348 flips) refused; truncated, extended and empty forms refused | `a_tampered_payload_is_refused_whichever_byte_changes` |
| A payload moved to another ordinal, lane, activation, domain, sequence, position or index refused; a record that lost its wrapper or gained a second refused | `a_sealed_payload_moved_...` |
| Same-key tenants cannot read each other's records | `a_shared_key_still_binds_each_record_to_its_index` |
| Wrong key material refused (`Corrupt`); revoked key makes replay `Denied`; plaintext in a keyed index refused, never read as plaintext | `a_wrongly_keyed_slot_...`, `a_keyed_index_never_reads_plaintext_...` |
| No wrapped key and no ciphertext repeats over 240 sealings (120 slots, and one position sealed 120 times) | `no_data_key_or_nonce_repeats_across_slots` |
| Refused commit reseals under a new key (2 wraps); lost reply resolved by identity (1 wrap, 1 slot) | `a_refused_commit_seals_again_...` |
| Sealing adds no request: same create, read, CAS and resolve counts as the plain writer; wraps 1 / 1 / 0 (`PerKey`), 1 / 50 / 0 (`PerIndex`), 1 / 11 / 0 with ten own-key indexes | `sealing_adds_no_request_and_the_wraps_follow_the_dek_scope` |
| Replay honours what each payload recorded, not the sealer's setting now; older slot still opens after one index's policy generation moves | `a_lane_may_hold_slots_of_both_scopes_...`, `an_index_that_rotates_...` |
| Cold replay costs one provider unwrap per data key per slot; the writer never unwraps | `a_cold_replay_costs_one_provider_unwrap_...` |
| Memory: buffers dropped above 1 MiB; a sealed form that passes a bound is refused whole, writing nothing, writer usable | `memory_is_bounded_by_the_slot_...` |

Mutations seen red (A to H), suite of 25 at the time (I, added after review: the slot header digest left out of the AAD, fails the new op-rewrite and tail-drop test): A seal does nothing (10 tests fail); B `seq` left out of the AAD (moved-payload test fails); C constant nonce (nonce test fails, after the test compared ciphertext without its tag; before that it **survived**); D ordinal left out of the AAD and E ordinal left out of the key context (each alone **survives**, by design: either binding alone refuses a payload moved to another slot, because a data key is fresh per slot; D and E together fail the moved-payload test); F index left out of the AAD (shared-key test fails); G policy generation made part of the key group (rotation test fails, after the test moved one index's generation; before that it survived); H the recorded scope flag ignored on open (three tests fail). The tests were written after the code and then proved by these mutations, not red-first.

## One-data-PUT slot log against payload + head CAS, same oracle

`cargo test -p hsearch-seal --test compare -- --nocapture`. 400 one-record 1 KiB batches per row, per-request fault probabilities (refused before accepted / written with the reply lost) from one seed, no scripted placement, so the two protocols meet the same rates, not the same faults. Both write the identical sealed slot bytes (`encode_sealed_slot`); head CAS stores them as the payload object. **Oracle, held for every row:** every batch is eventually acknowledged, replay finds exactly the acknowledged batches, in order, each decrypting, none that was not acknowledged.

| protocol | faults | mutating requests | per acknowledged batch | payload attempts per batch | orphan payloads | reopens | KMS wraps |
|---|---|---|---|---|---|---|---|
| slot log (1 PUT) | 0% / 0% | 402 | 1.00 | 1.00 | 0 | 1 | 400 |
| payload + head CAS | 0% / 0% | 801 | 2.00 | 1.00 | 0 | 0 | 400 |
| slot log (1 PUT) | 5% / 5% | 427 | 1.07 | 1.06 | 0 | 1 | 425 |
| payload + head CAS | 5% / 5% | 908 | 2.27 | 1.16 | 20 | 20 | 464 |
| slot log (1 PUT) | 15% / 5% | 486 | 1.22 | 1.21 | 0 | 1 | 484 |
| payload + head CAS | 15% / 5% | 1172 | 2.93 | 1.57 | 67 | 67 | 627 |

Reading: the slot log is one PUT a batch and a refused attempt is simply repeated. In head CAS a refused head CAS leaves the payload behind as an orphan, the retry at the same ordinal meets it and the writer reads that as another writer, so **every orphan forced a reopen** (reopens equal orphans), and each re-attempt reseals, so wraps rise with payload attempts (627 against 484 at 15% refusals). Sealing does not change which protocol wins; it makes each extra attempt cost a KMS wrap as well as a PUT.

## Dirty-only batching, sparse and hot tenants

Real `Batcher` (default policy: 4096 records, 4 MiB, 250 ms) on a virtual clock, each closed batch encoded plain and sealed.

| workload | data key | slots | records per slot | KMS wraps per slot | wraps per record | plain slot KiB | sealed slot KiB | sealing overhead |
|---|---|---|---|---|---|---|---|---|
| hot: 1 index, 1 KiB docs, 1 per ms | either | 20 | 250 | 1.0 | 0.004 | 257.4 | 261.7 | +1.7% |
| hot: 1 index, 64 KiB docs, 1 per ms | either | 8 | 62 | 1.0 | 0.016 | 4001.9 | 4003.1 | +0.0% |
| warm: 20 indexes, 1 KiB docs, 1 per ms | PerKey | 20 | 250 | 1.0 | 0.004 | 257.4 | 261.7 | +1.7% |
| warm: 20 indexes, 1 KiB docs, 1 per ms | PerIndex | 20 | 250 | 20.0 | 0.080 | 257.4 | 263.5 | +2.4% |
| sparse: 1000 indexes, 1 KiB docs, 1 per ms | PerKey | 20 | 250 | 1.0 | 0.004 | 257.4 | 261.7 | +1.7% |
| sparse: 1000 indexes, 1 KiB docs, 1 per ms | PerIndex | 20 | 250 | 250.0 | 1.000 | 257.4 | 286.2 | +11.2% |
| very sparse: 1000 indexes, 1 per 10 ms | PerKey | 80 | 25 | 1.0 | 0.040 | 25.8 | 26.3 | +2.0% |
| very sparse: 1000 indexes, 1 per 10 ms | PerIndex | 80 | 25 | 25.0 | 1.000 | 25.8 | 28.7 | +11.2% |

The wrap count is the number of data keys in the slot. A refused attempt seals again, wraps included: three attempts of the sparse slot triple its wraps (asserted). Dirty-only holds: an idle lane seals and requests nothing.

## CPU per MiB sealed

`cargo run --release -p hsearch-seal --example slot_bench` (median of seven runs; the in-memory KMS makes a wrap an in-process call, so wrap cost here is a floor, not a KMS round trip). "Seal" includes the slot encode.

AES backend on the host: `x86_64, aes=hardware, ghash=hardware (AES-NI, PCLMULQDQ, VAES)`.

| shape | data key | plain encode MiB/s | seal MiB/s | open MiB/s | seal CPU ms per MiB | wraps per slot |
|---|---|---|---|---|---|---|
| hot, 1 KiB x 250 | PerKey | 2361 | 921 | 1726 | 1.09 | 1 |
| hot, 64 KiB x 62 | PerKey | 1688 | 976 | 2149 | 1.02 | 1 |
| hot, ~1 MiB x 4 | PerKey | 1684 | 992 | 2071 | 1.01 | 1 |
| sparse, 250 indexes, 1 KiB x 250 | PerKey | 1817 | 634 | 1060 | 1.58 | 1 |
| sparse, 250 indexes, 1 KiB x 250 | PerIndex | 1866 | 172 | 475 | 5.80 | 250 |
| tiny, 100 B x 4096 | PerKey | 1355 | 152 | 181 | 6.58 | 1 |

Hardware rows were rerun after the slot header digest was added to the AAD (the digest costs SHA-256 over about 60 B per record: visible only in the tiny-record row). The forced-software rows below predate the digest and were not rerun.

Forced software AES (`--cfg aes_backend="soft" --cfg polyval_backend="soft"`, `SLOT_BENCH_MB=12`): `aes=soft ghash=soft`. Hot 64 KiB: 102 MiB/s seal, 117 open (9.8 ms per MiB); hot 1 KiB: 96 and 106; sparse `PerKey`: 87 and 92. About eight to ten times slower than hardware (the block sealer measured the same in M05.06); a node on that path is a capacity problem, which the backend report states. **aarch64 was not built or run.**

## What this costs and what it does not cover (read before relying on it)

0. **Limits of "a tampered slot is refused", found by the independent review.** (a) A slot whose **only** sealed record is rewritten as a delete, or a slot of deletes, has no sealed record left to fail, so that rewrite is not detected; closing it needs a slot-level authentication tag, a format change (the AEAD envelope capability bit). (b) A keyed index's `External` record is passed through unopened (its object is sealed and checked by `SealedBackend`, which this layer does not verify), and a sealed record relabelled to an *unkeyed* index id is delivered to that index as its document (injection, not disclosure). (c) Under `PerKey`, policy generation is not part of the key group: bumping one index's generation does not make its records unreadable while another index's wrapper carries the key, and bumping the carrier's makes the others unreadable; `PerIndex` has neither effect, and a revocation lever that must be per index should use it. (d) Any `Denied` or `Corrupt` aborts `open_sealed` for the whole lane: one revoked key among several key identities in a lane makes the lane unopenable. A shared lane holds one key identity, so this bites `PerIndex` or per-index scopes only. (e) The key provider is called synchronously on the commit path; a network provider must be wrapped off the executor, and under `PerIndex` a sparse slot makes up to 250 serial wraps before its PUT. (f) The commit path also allocates a `Vec<RecordSpec>` per slot and two small maps in `seal` and `open`; allocation was not measured, and the memory bound above excludes that vector (about 80 B per record). (g) `DekScope` is a constructor setting and a trait override; wiring it to cluster and index settings is part of the integration follow-up.

1. **Provider calls.** One wrap per data key per slot on the writer and **one unwrap per data key per slot on a cold replay** (asserted: 5 slots, 5 unwraps, however many tenants). The writer's own key manager also caches each slot's data key on create (`create_object_key`), which the writer never reads back; one entry per slot, unmeasured effect on the cache. An uncached create would be a `hsearch-security` change, not made here.
2. **Inline bound.** The sealed form must fit the 1 MiB inline record bound and the 8 MiB slot body: a record that fits only in the clear is refused whole (`Format(TooLarge)`, nothing written, writer usable). With 4096 records of 2,010 B the plain slot fits and the sealed one does not. `Batcher` counts plaintext bytes; **the committer must reserve `SlotSealer::overhead` (about 150 B with the test KMS, up to roughly 1.4 KB for the largest wrapper the provider contract allows) when it chooses the inline bound and the batch target, and spill what no longer fits.** Not done here (the committer is outside this task's files).
3. **Integration is not wired.** A writer given a cipher seals; nothing in `hsearch-commit`, `hsearch-store` or `hsearch-node` gives it one yet, and `hsearch-store`'s recovery still reads slots with plain `read_log`, which shows sealed bytes. "Every committed slot of a keyed index is encrypted" therefore holds for any lane writer opened with `open_sealed` and is not yet true of a running node. That wiring touches `crates/commit`, `crates/store/src/recover.rs` and node setup: its own task.
4. **In the clear by design:** a record's index, sequence and operation, the slot position and commit token, and an `External` record's length and **plaintext SHA-256 digest** (the format has no room to seal it; a known document can be confirmed against it). The external object itself is sealed by `SealedBackend`. A format change to hold the digest sealed is a follow-up.
5. **Memory.** A sealing writer holds the caller's plaintext batch, one sealed arena and the encoded slot: up to three times the slot body (each at most 8 MiB, defaults 4 MiB) against two for the plain writer. The arena is dropped above 1 MiB.
6. **Not measured:** a real KMS call's latency and price; an object store; aarch64; the effect of per-slot cache entries; concurrent writers.
7. **Not proven:** that GCM nonce and key discipline is correct beyond the tests above. Nothing here was independently cryptographically reviewed; the security-format reviewer role should read `slot.rs` and the AAD before this is relied on.
