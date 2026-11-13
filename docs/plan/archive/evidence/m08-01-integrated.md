# Observation contract and facade acceptance

Normally merged [PR17](https://github.com/huyz0/hsearch/pull/17) reviewed 370476b3d3acda90c8c6aac7f56d69e522890fca as 79f472c5aa868edcb543e37cb70981a603e9a988, identical tree 8426422f0e9dca2578c3021da4a425ae91551038. Both actual independent R1 reports clear; exact-source durable gate and strict ci-ok/admin protection passed. No formal GitHub approval was submitted.

- [PR run41358917077](https://github.com/huyz0/hsearch/actions/runs/41358917077): all seven jobs passed; decoded full proof SHA256 b2b2a18d499d3ac1987bdd5a53057640293717c39e281a572c3e68121531a9e5.
- [MAIN run41359652142](https://github.com/huyz0/hsearch/actions/runs/41359652142): all seven jobs passed; decoded full proof SHA256 83fd46b05cc54202a45fd8f1b44728497eba658b6495f326491e93ff317f572d.

Both native targets passed dev/release/allfeatures/doctests/lint/format and523/523 production lines, changed480/480; observe199/199, types324/324, unchanged engine has no coverable production lines. Both sandboxes passed288 component cases/no skips, ten actual compiler controls, storage/TLS/cleanup isolation, source/projection/negative controls, five fresh cold diagnostic sessions,14search/4analyzer/34mutation reference steps and18fresh catalog samples per architecture.

Actual dev/release layout records in each native/sandbox job preserve per-target measurements: record<=512bytes, queuecapacity1024, payload=capacity*record<=524288bytes, max32fields. These are queue payload bounds, not allocator/channel/projection/process RSS or fleet guarantees. The local x86 measured400bytes/409600bytes.

Typed v1 owner facts distinguish committed/applied/searchable/archived, full incarnation-scoped coordinates, receipt visibility and downstream error. The original Success-only guard natively failed the fixed12/7/5/9 regression; minimal repair passed with both testfiles byte-identical. Producer-side input minimization, fixed projection and bounded metric dimensions were independently reviewed. Caller provenance and owner-supplied fact truth remain outside the facade.

Acceptance record SHA256 3441cd9a41b8c67504de356fd238ecf0173d3838aa1b1fefc02d6ab0e24fa6f8; packet e6cedf4acfc0e3c480f17903fa6573a3b9d5aa7f22b194474d66448711d8f112; gate checks 69599ad8485810515ef5d3912cebd99e508cf6d0f8376b0f7c7666f820871577. Actual raw review, official run/job/check identities and logs remain retained by the coordinator.

This accepts types and a facade seam. SDK/OTLP/exporter/runtime/provider/fleet/license/release/deployment remain unaccepted. Original M01.09 ARM diagnostic cause is UNCONFIRMED and M01.08 stays open; every original failed review/run/gate remains preserved.
