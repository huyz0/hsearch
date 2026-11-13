# ARM cold-diagnostic incident: proposed acceptance change

Status: M01.15 proposal, not accepted. On 5 November 2026 (Australia/Sydney), the user directed preparation of an independently reviewed gate change that closes the old incident as unresolved and requires current ARM qualification plus future failure capture. This authorizes preparation of the replacement contract; it is not evidence that its checks or reviews passed.

## Incident and limits of the evidence

At commit `996b84b`, [run 41283307456](https://github.com/huyz0/hsearch/actions/runs/41283307456) failed the ARM cold semantic corpus. Diagnostics fell back, and dirty-fixture cleanup masked the primary failure. The retained original logs do not contain the failed routed response's reason or the CLI/RPC/server details needed to establish a backend cause. Later successful runs cannot reconstruct that missing historical payload.

[PR9](https://github.com/huyz0/hsearch/pull/9) integrated bounded CLI failure evidence and primary-error-preserving cleanup. [PR20](https://github.com/huyz0/hsearch/pull/20), merged as `0660c853185a3f92b25dc3ddbe924fd1b3278ff2`, added bounded diagnostic witnesses before the cold assertions. [Its main run](https://github.com/huyz0/hsearch/actions/runs/41830170229) passed all seven jobs, including native ARM/x86 and both sandbox jobs. These are current-behavior observations, not a historical-cause proof or qualification of a future changed tree.

The original M01.09 completion criterion was: “Original failure has an evidenced cause and regression control, cold diagnostics remain bounded, and full relevant checks plus independent reviews pass.” The original source/criteria remain accessible at [the accepted pre-change tree](https://github.com/huyz0/hsearch/blob/0660c853185a3f92b25dc3ddbe924fd1b3278ff2/docs/plan/tasks.json). Original failures, findings, issued review packets and three-round source history remain unchanged. M01.15 reviews this changed acceptance policy, not another repair of frozen M01.09 source.

## Replacement gate

After independent review, exact-head qualification and normal protected integration of M01.15, M01.09 can close operationally through this disposition. Its historical cause stays **unconfirmed**. No cause-specific repair or regression is claimed. Until that integration, M01.09 stays in progress and M01.08 remains dependency-blocked.

The replacement requires all of the following:

1. Both independent roles review the exact ledger, validator and disposition, including the explicit change from historical proof to risk acceptance. Original incident evidence and missing payload are part of the review basis.
2. The reviewed head passes the complete selected native Linux x86_64/aarch64 and sandbox matrix, plan/API/docs checks, existing production coverage floors, and protected `ci-ok`. No previous run substitutes for this head's checks.
3. The real semantic corpus retains all five fresh cold sessions per architecture, each observing actual E0308 at line 2. Invalid Cargo input fails and restored valid input passes. Existing source-bound isolation, bounded output and primary-error/unknown-edit cleanup controls remain strict.
4. Each cold response emits its bounded witness before the semantic assertion into the retained job log. It includes actual status/reason and diagnostics, source/CLI identity attribution and a digest of the complete bounded response. Existing 4 KiB per-record and 64 KiB per-run witness bounds remain; missing, oversized or incomplete emission prevents corpus qualification. CLI failure prefixes and primary errors remain bounded and visible. A digest alone is not a cause explanation.
5. Existing seeded wrong/empty diagnostics, escaping overflow, closed-sink, primary-error cleanup, unexpected-edit and compiler controls still fail or pass for their stated independent oracle. New plan controls refuse premature completion, a claim that the historical cause is confirmed, and removal of the incident/baseline or whole-M01 product edges.

After integration, record a factual local acceptance record with the reviewed tree, independent verdicts, exact run/job IDs and observed corpus/witness results. Only then mark M01.15 and the composite M01.09 goal done. This document alone is not that acceptance record.

## Residual risk and reopening

The old fault may recur because its cause is unknown. Current native qualification and retained future witnesses reduce the risk of another unexplained failure; they do not prove the old fault is eliminated. Any new semantic fallback, wrong/missing diagnostic, incomplete witness or masked primary error fails qualification and opens a new incident with its own source-bound evidence and review budget. Link a new failure to the old event only if evidence supports that causal relationship. Do not add blind retries, increase settling time or weaken the semantic/compiler assertions to obtain a pass.

## Preserved delivery boundaries

The M01.08 dependency on M01.09 stays. Closing this incident only makes the frozen agent-corpus task eligible; it does not accept its concurrency/recovery, token-cost or escaped-defect requirements. M02/M03 and later product tasks keep whole-M01 and their other original prerequisites. Engine research, coverage-component acceptance, provider/fleet tests, security, runtime durability and release/deployment remain separate. M02.10's private candidate and exhausted review history are outside this change and remain untouched.
