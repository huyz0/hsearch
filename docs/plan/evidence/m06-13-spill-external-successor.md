# M06.13 evidence: spill, external payloads, sealed successors

Provisional: in-memory store on a shared Linux container, no RustFS run locally (no Docker daemon
here; `crates/store/tests/spill_rustfs.rs` runs in CI against RustFS and was only compiled locally).

## What changed (`crates/store/src/recover.rs`)

- `recover_with(.., &Spill, ..)`; `recover` is `recover_with(.., &Spill::Refuse, ..)`, so its signature and the
  `Record`/`Target` types are unchanged. `Spill::Scratch { dir, ceiling }` keeps the first
  `limits.records` digests in memory and puts the rest in a scratch file (32 bytes a record, dense array
  indexed by `seq - from`, buffered 2048 at a time, removed on return). The setting defaults to refuse (a
  measured cost, see below), per the setting rule.
- External payloads: one HEAD, then ranged GETs of at most 8 MiB, SHA-256 hashed as they arrive; length and
  digest verified; absent/short/long/altered is `NotReady::Payload(seq)`. The length counts against the bytes
  bound before any request. `NotReady::Unsupported` is gone.
- Sealed successors: after a seal the source's descriptor and the successor's head are read through
  `route`, and the same replay continues in the successor lane (chains up to 16 hops). A seal whose
  promotion did not finish ends the history at the seal.
- `Recovered` gained `followed` and `spilled`; `NotReady` gained `Payload` and `Scratch`.

## Tests

`crates/store/tests/spill.rs`, 14 tests plus `recover.rs` (one test renamed: the absent external object now
yields `Payload(1)` instead of `Unsupported(1)`). Mutations seen red: digest check, length check, chunk bound,
bytes counted before fetch, duplicate comparison, follow-the-seal, seal gap, spill read offset, flush
trigger, ceiling, record-after-seal. Survivors: short-read check and hop bound (both unreachable against a
store that honours the range contract and hash-derived targets), and the successor head tail check (no
test loses a successor slot).

## Benchmark (`cargo test --release -p hsearch-store --test spill -- --ignored --nocapture bench`)

150,000 records, 3,000 slots, in-memory store, best of 5:

| mode | ns/record | RSS growth after run |
|---|---|---|
| all in memory | 167 | 3,672 kB |
| spill, 1,000 in memory | 173 | 0 kB |
| spill, none in memory | 173 | 0 kB |

Downside: about 4% more CPU per record (buffered file appends), plus local disk of 32 bytes a record.
Upside: digest memory is constant (it was 32 bytes a record, 3.2 GB at 100M records). RSS is a coarse,
provisional reading. Requests are unchanged by the spill; each external document costs 1 HEAD plus
ceil(length / 8 MiB) GETs.

## Review findings (independent reviewer)

- Repeated external records refetched without bound: fixed, repeats are counted against the bytes bound
  (`repeated_external_records_cannot_refetch_past_the_bytes_bound`).
- `Recovered.tail` and `Record.ordinal` after a successor hop refer to the lane they were read in:
  documented on the fields.
- Known limitation, not fixable in recovery: a seal names the last sequence, and a seal at 0 is ambiguous
  between "Put 0 written" and "nothing written", so a lost final slot immediately before an unfinished
  promotion's seal is not detected without `Expect.seq`. A format change (the seal naming the next due
  sequence) would close it; raised to the coordinator.
