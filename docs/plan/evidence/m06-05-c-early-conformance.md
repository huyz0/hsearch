# M06.05 (C): early conformance corpus

Provisional: in-memory store, one host, no RustFS, no OpenSearch server. This is the C part of M06.05; the writer wiring, native `/v1` routes and the core-v1 refresh are other parts and are not claimed here.

## What exists

`fixtures/api/conformance/early-v1/` holds a dataset and three case files (reference, difference, unsupported). `crates/node/tests/conformance.rs` replays every case over HTTP against a fresh `hsearch-node` and checks the corpus structurally. See the corpus README for the case shape.

## Result

`cargo test -p hsearch-node --test conformance`: 42 cases covering all 16 profile areas, 35 enforced and passing, 7 pending and still failing (PUT/DELETE document, `_update`, index delete, `refresh=true` on a document write, real-time GET after an unrefreshed delete, `/v1/capabilities`). Each pending case fails for the stated reason: a 405 or 404 from the route, or a 200 where the profile promises 404.

## Negative controls observed

A changed expected total fails an enforced case by name; an enforced case marked pending fails the run with "pending cases now pass"; an unknown profile area fails the structural test. Each was run red and restored.

## Limits

The reference class takes its expectations from documented OpenSearch behaviour, not from a run of OpenSearch 3.9.0, and links core-v1 cases only where a reference run exists there. Unsupported cases that currently succeed by route absence (SQL, cluster state, forcemerge) accept 400, 404 or 405 and will be tightened when the native error shape is frozen (M06.05 B). No SDK was run.
