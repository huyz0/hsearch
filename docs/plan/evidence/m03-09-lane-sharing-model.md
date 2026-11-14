# M03.09 lane-sharing model and revocation search

`provisional`, **modelled, not measured**. Everything between the markers below is the output of `python3 docs/plan/tools/lane_model.py --markdown` and is regenerated, not typed. Assumptions live in the `lane_sharing` block of [cost-calibration-inputs.json](../cost-calibration-inputs.json): 10M indexes, 10% active, **5% BYOK (an assumption, not a figure from any plan document)**, 1,000-index lane groups, 2 retained objects per lane, the `object-resilient` profile's 250 ms wait. It reuses the M02.07 batcher and illustrative rate card ([m02-07-cost-calibration.md](m02-07-cost-calibration.md)); the decision is [M03.09](../decisions/M03.09-commit-domain-sharing.md).

<!-- generated:begin -->
| workload | option | requests per ack (PUT / KMS) | active lanes in window | lanes at 10M | retained objects (idle) | blast radius (indexes) | hottest index share of a busy lane | USD per 30 days |
|---|---|---|---|---|---|---|---|---|
| steady_small | A one lane per index | 1.967 (0.999 / 0.969) | 21,777 | 10,000,000 | 20,000,000 | 1 | 0.0 | 41,122.06 |
| steady_small | B shared by same policy, BYOK own lane | 0.487 (0.247 / 0.24) | 2,085 | 509,501 | 1,019,002 | 1000 | 0.003 | 10,181.51 |
| steady_small | C shared, per-index payload encryption | 0.455 (0.206 / 0.25) | 1,000 | 10,001 | 20,002 | 1000 | 0.003 | 9,254.24 |
| write_storm | A one lane per index | 2.021 (1.026 / 0.995) | 451,115 | 10,000,000 | 20,000,000 | 1 | 0.0 | 420,447.98 |
| write_storm | B shared by same policy, BYOK own lane | 0.438 (0.223 / 0.216) | 23,626 | 509,501 | 1,019,002 | 1000 | 0.0 | 91,191.00 |
| write_storm | C shared, per-index payload encryption | 0.39 (0.173 / 0.218) | 1,000 | 10,001 | 20,002 | 1000 | 0.0 | 78,502.88 |
| one_hot_index | A one lane per index | 1.024 (0.52 / 0.504) | 29,851 | 10,000,000 | 20,000,000 | 1 | 0.0 | 21,451.07 |
| one_hot_index | B shared by same policy, BYOK own lane | 0.838 (0.425 / 0.412) | 2,443 | 509,501 | 1,019,002 | 1000 | 0.999 | 17,549.14 |
| one_hot_index | C shared, per-index payload encryption | 0.845 (0.417 / 0.428) | 1,000 | 10,001 | 20,002 | 1000 | 0.999 | 17,573.26 |

BYOK sensitivity (requests per write, steady_small):

| BYOK share | B | C | lanes at 10M, B | lanes at 10M, C |
|---|---|---|---|---|
| 5% | 0.487 | 0.455 | 509,501 | 10,001 |
| 25% | 0.808 | 0.653 | 2,507,501 | 10,001 |
| 50% | 1.212 | 0.905 | 5,005,001 | 10,001 |
| 100% | 1.967 | 1.405 | 10,000,001 | 10,001 |

Revocation search (histories of 5 operations ending in recovery):

- `per_index_dek`: safe over 625 histories.
- `lane_dek_default`: safe over 256 histories.
- `lane_dek_a_byok`: unsafe, first counterexample after 9 histories: ['append_a', 'append_a', 'append_b', 'revoke_key_a', 'recover'] (record of index b readable=False but its own key revoked=False).
- `per_index_dek_recovery_fails_lane`: unsafe, first counterexample after 4 histories: ['append_a', 'append_a', 'append_a', 'revoke_key_a', 'recover'] (recovery failed the whole lane on a block whose key is revoked).

<!-- generated:end -->

Workloads: `steady_small` 2,000 writes/s over 1M active indexes, 80% of writes on 1% of them; `write_storm` 20,000 writes/s uniform, 30 s; `one_hot_index` 2,000 writes/s with half of all writes on one non-BYOK index, to exercise the promotion rule. One seed each. Hot indexes are the contiguous ids 0..9,999 and groups are `index // 1000`, so the hot set lands in exactly 10 lanes: a placement artefact, and arrival-based grouping would spread differently.

`hottest index share` counts only shared lanes that saw at least 1,000 records in the window. Before that floor the column reported about 10% for `steady_small`, which was two hits on one index of about twenty in a cold lane. With the floor the skewed load is 0.3%, and only `one_hot_index` crosses the quarter threshold (one index promoted).

How to read the numbers:
- **KMS is counted as one wrap per batch**, an upper bound: PR-24 says no KMS call per request and a DEK may be cached across batches, which this model does not credit. About half of A's 1.967 requests per write and half of every USD figure is that term. The PUT-only ratio (0.999 against 0.247, about 4x) does not depend on it; the USD ratio depends on an illustrative KMS price.
- A's ratio is specific to the workload: about 0.002 writes/s per active index against a 250 ms window means almost every write is its own batch (21,777 lanes for about 60,000 events).
- Option C counts one wrap per BYOK record (stated upper bound). B and A have no matching term for per-index recovery or read unwraps, which is not modelled for any option.
- Not modelled: lane member-set maintenance (a CAS on the member object at join, leave and promotion), index creation at 10M scale, one-in-flight-append per lane, the 4 MiB cap, lane poison. Checkpoint puts are amortised per batch only.
- "Idle indexes issue no request" holds because the model emits requests only from events; the empty-trace test checks that, and the member-set and creation costs above are the places it could be false.

Revocation search: a stand-in exhaustive model of one lane with two indexes, not the M03.04 authority model. For `per_index_dek` and `lane_dek_default` the protecting key equals the index's own key by construction, so those two cases check the recovery policy (skip a revoked block versus fail the lane) and the rule that no revocable key wraps a shared lane. They do not check AAD or replay across indexes, key rotation or re-wrap, or a revoked-then-reinstated key; those belong to M03.04 and M05. The history length is 5 and "first counterexample after N histories" is only the search order.

Not done: measured request counts (M03.08), lane re-establish time after a poison, the recovery unwrap bound for option C, the M03.04 model, a real trace, any real KMS or provider price.
