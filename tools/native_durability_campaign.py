#!/usr/bin/env python3
"""Fixed M02.10 native campaign; ordinary check and object qualification stay distinct."""
import argparse
import copy
import hashlib
import json
import os
import platform
from pathlib import Path
import re
import runpy
import tempfile

ROOT = Path(__file__).resolve().parents[1]
RESEARCH = ROOT / 'crates/testkit/research'
decode = runpy.run_path(str(RESEARCH / 'native_verify.py'))['decode']
CAP = 1024**2
TOTAL = 32 * CAP
TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')
CANDIDATES = {'raft-rs': 'matched_raft_workload_experiment', 'openraft': 'matched_openraft_workload_experiment'}
# Exact original frozen envelope bytes, including historical status; not observed qualification.
ENVELOPE_TEXT = """{
  "schema_version": 1,
  "task": "M02.10",
  "status": "frozen_requirements_before_io_not_observed_qualification",
  "execution_scope": "coordinator-research",
  "pins": {
    "openraft": "8815cdba2826f74e848acef361ad03f93bb1c3f8",
    "raft-rs": "10c6e9db6792b85c81784e44fc278f895d5f0ab0",
    "rust": "1.97.1",
    "rustfs": "rustfs/rustfs:1.0.4@sha256:ed9210d6411d77ffceb70d94b964a3cb7d225983368bc25bebe01e995293dcc2"
  },
  "features": {
    "openraft": [
      "serde",
      "storage-v2"
    ],
    "raft": [
      "protobuf-codec"
    ],
    "default_features": false
  },
  "targets": [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu"
  ],
  "profiles": [
    "dev",
    "release"
  ],
  "fresh_repetitions_per_profile": 3,
  "source_worker": {
    "cpu_millis": 2000,
    "memory_bytes": 4294967296,
    "pids": 256,
    "scratch_bytes": 8589934592,
    "deadline_seconds": 1200,
    "diagnostic_bytes": 1048576
  },
  "object_worker": {
    "cpu_millis": 2000,
    "memory_bytes": 4294967296,
    "pids": 256,
    "scratch_bytes": 19327352832,
    "deadline_seconds": 1200,
    "diagnostic_bytes": 1048576
  },
  "artifacts": {
    "files": 256,
    "file_bytes": 1048576,
    "total_bytes": 33554432
  },
  "fixture": {
    "path": "crates/testkit/tests/fixtures/durability-traces.json",
    "sha256": "fc130ba2339e0d3232c0eb9013088eaa5b0346e98a7e6b21992e4298f69c8169",
    "generator": "lcg32-fisher-yates-v1",
    "seeds": [
      7,
      19,
      43
    ],
    "traces": 9,
    "events": 816,
    "offer_window_us": 500000,
    "logical_payload_bytes": 1024,
    "skewed_hot_count": 102,
    "skewed_total": 128
  },
  "batching": {
    "groupings": [
      "stream",
      "compatible"
    ],
    "wait_us": [
      0,
      10000,
      50000,
      250000
    ],
    "logical_batch_bytes": 16384,
    "encoded_batch_bytes": 32768,
    "root_page_error_bytes": 4096,
    "response_header_bytes": 16384,
    "queues": 16,
    "pending_records_per_queue": 32,
    "pending_records_global": 512,
    "logical_pending_bytes": 524288,
    "encoded_pending_bytes": 1048576,
    "one_request_per_lane": true,
    "global_inflight_requests": 4,
    "final_drain": "natural_first_dirty_deadline_no_offer_end_flush",
    "tie_order": "timer_before_arrival",
    "object_cases": 144,
    "native_cases_per_candidate": 18
  },
  "cas": {
    "rounds": 8,
    "contenders_per_round": 4,
    "total_attempts": 32,
    "global_inflight_ceiling": 4,
    "fresh_etag_per_sequential_round": true,
    "distinct_generation_contender_body": true,
    "conforming_successes": 8,
    "conforming_conflicts": 24,
    "all_participants_join_before_resolution_and_next_round": true
  },
  "native_history": {
    "nodes": 4,
    "operations": 128,
    "transport_attempts": 4096,
    "message_bytes": 1048576,
    "serialized_bytes": 16777216,
    "seconds": 15,
    "pending_inbound_messages_per_node": 64,
    "pending_inbound_messages_global": 256
  },
  "native_sample_per_candidate": {
    "seconds": 90,
    "transport_attempts": 32768,
    "serialized_bytes": 67108864,
    "file_write_bytes": 536870912,
    "sync_calls": 65536,
    "file_write_calls": 65536
  },
  "group_scale": {
    "counts": [
      1,
      4,
      16
    ],
    "voters_per_group": 3,
    "aggregate_attempts_per_case": 4096,
    "aggregate_serialized_bytes_per_case": 16777216,
    "scope": "whole_process_observations_no_exact_per_group_allocation_or_fleet_extrapolation",
    "idle_measurement_ms": 100
  },
  "protoc": {
    "package": "protobuf-build",
    "package_version": "0.14.1",
    "crate_sha256": "2df9942df2981178a930a72d442de47e2f0df18ad68e50a30f816f1848215ad0",
    "upstream_source_commit": "8d1dfba02caec5b9d00b0e210a3ee9a4ee36dfb8",
    "expected_version": "libprotoc 3.9.0",
    "native": {
      "x86_64": {
        "path": "bin/protoc-linux-x86_64",
        "sha256": "30c6f9b5c03fb4ad7c2f0ad2efa0297c1f4e349726b6c968a029a277e864d48e",
        "elf_machine": 62
      },
      "aarch64": {
        "path": "bin/protoc-linux-aarch_64",
        "sha256": "6f94b4459494b1873ad712cf5bb8e95bab72026c3be8fe95df816e989498b763",
        "elf_machine": 183
      }
    },
    "arm_execution_status": "NOT_RUN"
  },
  "scope": "Frozen local public research requirements, not runtime authority/API/durability/failure-domain/provider/fleet/crypto/format/release acceptance; source/tool/matrix/full CI/review gates remain.",
  "object_store_accounting": {
    "dataset_bytes": 10737418240,
    "mode": "read_only_stat_walk_no_contents",
    "sample_interval_ms": 500,
    "separate_from_worker_scratch": true
  },
  "qualifying_codegen_scope": "Every required native experiment/check entrypoint explicitly selects and verifies PROTOC; raw Cargo/editor diagnostics do not establish qualification."
}
"""
ENVELOPE = json.loads(ENVELOPE_TEXT)
FIXTURE = 'crates/testkit/tests/fixtures/durability-traces.json'
FIXTURE_SHA = 'fc130ba2339e0d3232c0eb9013088eaa5b0346e98a7e6b21992e4298f69c8169'
PROTOC = {'x86_64-unknown-linux-gnu': '30c6f9b5c03fb4ad7c2f0ad2efa0297c1f4e349726b6c968a029a277e864d48e',
          'aarch64-unknown-linux-gnu': '6f94b4459494b1873ad712cf5bb8e95bab72026c3be8fe95df816e989498b763'}


def need(value, message):
    if not value:
        raise ValueError(message)


def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(',', ':')) + '\n').encode()


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def modules():
    return (runpy.run_path(str(RESEARCH / 'native_bundle.py')),
            runpy.run_path(str(RESEARCH / 'native_sample_verify.py')))


def read(path, cap=CAP, *, allow_empty=False):
    path = Path(path)
    need(path.is_absolute() and path.resolve() == path and not path.is_symlink(), 'Redirected campaign file')
    before = path.stat()
    need(path.is_file() and before.st_nlink == 1 and (0 if allow_empty else 1) <= before.st_size <= cap, 'Campaign file bound/type')
    with path.open('rb') as stream:
        raw = stream.read(cap + 1)
    after = path.stat()
    need(len(raw) == before.st_size and (before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
         (after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns), 'Campaign file changed while reading')
    return raw


def matrix(target):
    need(target in TARGETS, 'Campaign requires an actual native target')
    return [{'target': target, 'candidate': candidate, 'profile': profile, 'repetition': repetition}
            for profile in ('dev', 'release') for candidate in CANDIDATES for repetition in (1, 2, 3)]


def binding(source, image, row, root=ROOT):
    need(source['clean'] is True, 'Campaign requires clean committed source')
    need(digest(read(root / FIXTURE)) == FIXTURE_SHA, 'Frozen durability fixture changed')
    result = {**row, 'source_digest': source['digest'], 'source_head': source['head'],
              'image': image, 'lock_sha256': digest(read(root / 'Cargo.lock')),
              'fixture_sha256': FIXTURE_SHA, 'envelope_sha256': digest(ENVELOPE_TEXT.encode()),
              'execution': 'strict-nextest', 'nextest_run_id': None}
    validate_input(result)
    return result


def validate_input(value):
    need(ENVELOPE['targets'] == list(TARGETS) and ENVELOPE['profiles'] == ['dev', 'release'] and
         ENVELOPE['fresh_repetitions_per_profile'] == 3 and ENVELOPE['batching']['native_cases_per_candidate'] == 18 and
         ENVELOPE['group_scale']['counts'] == [1, 4, 16] and ENVELOPE['artifacts'] ==
         {'files': 256, 'file_bytes': CAP, 'total_bytes': TOTAL} and ENVELOPE['source_worker'] ==
         {'cpu_millis': 2000, 'memory_bytes': 4294967296, 'pids': 256, 'scratch_bytes': 8589934592,
          'deadline_seconds': 1200, 'diagnostic_bytes': CAP}, 'Original campaign envelope differs from fixed controller')
    keys = {'source_digest', 'source_head', 'lock_sha256', 'fixture_sha256', 'envelope_sha256', 'image',
            'target', 'profile', 'candidate', 'repetition', 'execution', 'nextest_run_id'}
    need(type(value) is dict and set(value) == keys, 'Closed native campaign binding fields')
    need({k: value[k] for k in ('target', 'profile', 'candidate', 'repetition')} in matrix(value['target']), 'Campaign cell')
    need(type(value['repetition']) is int, 'Campaign repetition type')
    for key in ('source_digest', 'lock_sha256', 'fixture_sha256', 'envelope_sha256'):
        need(type(value[key]) is str and re.fullmatch('[a-f0-9]{64}', value[key]), 'Campaign digest: ' + key)
    need(type(value['source_head']) is str and re.fullmatch('[a-f0-9]{40}', value['source_head']), 'Campaign head')
    need(type(value['image']) is str and re.fullmatch('sha256:[a-f0-9]{64}', value['image']), 'Campaign image')
    need(value['execution'] == 'strict-nextest' and value['nextest_run_id'] is None, 'Controller cannot invent nextest UUID')
    need(value['fixture_sha256'] == FIXTURE_SHA and value['envelope_sha256'] == digest(ENVELOPE_TEXT.encode()), 'Campaign fixture/envelope')


def sample_command(row):
    need(row in matrix(row['target']), 'Fixed sample cell required')
    return ['cargo', 'nextest', 'run', '--locked', '--offline', '--package', 'hsearch-testkit', '--all-features',
            '--test', 'native_durability', *(['--release'] if row['profile'] == 'release' else []),
            '--profile', 'hsearch', '--no-tests=fail', '-E', 'test(=' + CANDIDATES[row['candidate']] + ')',
            '--success-output', 'immediate']


def parse_sample(raw):
    need(type(raw) is bytes and len(raw) <= 512 * 1024, 'Bounded combined nextest streams required')
    samples = []
    marker = b'NATIVE_DURABILITY_SAMPLE: '
    for line in raw.split(b'\n'):
        if marker in line:
            need(len(line) <= CAP and line.count(marker) == 1, 'Native sample frame bound')
            samples.append(decode(line.split(marker, 1)[1]))
    need(len(samples) == 1, 'Exactly one complete strict sample across both streams required')
    return samples[0]


def guest_sample(output, environment, tool_proof, command):
    """Called only by compile_entry after actual graph and authenticated tool proof."""
    config_raw = read(Path('/durability-binding.json'), 4096)
    config = decode(config_raw); validate_input(config)
    need(config['lock_sha256'] == digest(read(ROOT / 'Cargo.lock')) and
         config['fixture_sha256'] == digest(read(ROOT / FIXTURE)), 'Guest lock/fixture binding differs')
    need(not any(k.startswith(('HSEARCH_DURABILITY_', 'HSEARCH_NATIVE_')) for k in os.environ), 'Ambient native sample selector')
    coverage = runpy.run_path(str(ROOT / 'tools/coverage.py'))
    coverage['nextest_policy']()
    tools = coverage['qualified_tools']()
    need(tools['native_target'] == config['target'], 'Qualified compiler is not selected native target')
    env = coverage['ordinary_environment'](tools)
    env.update(environment)
    # Correctness compiler/target variables are installed only after the outer
    # ambient selector rejection; verified PROTOC survives trusted setup.
    env.update(coverage['ordinary_environment'](tools))
    env['PROTOC'] = environment['PROTOC']
    need(digest(read(Path(env['PROTOC']), 16 * CAP)) == tool_proof['tool_actual_sha256'], 'Verified protoc changed')
    config['protoc_sha256'] = tool_proof['tool_actual_sha256']
    scratch = Path('/target/native-sample-scratch'); scratch.mkdir(mode=0o700)
    env.update(HSEARCH_DURABILITY_STRICT_SAMPLE='1', HSEARCH_DURABILITY_BINDING=encoded(config).decode().strip(),
               HSEARCH_NATIVE_ARTIFACT_ROOT=str(scratch))
    argv = coverage['cargo_command'](coverage['correctness_command'](tools, sample_command(
        {k: config[k] for k in ('target', 'candidate', 'profile', 'repetition')})))
    # compile_entry drains both streams inside the unchanged monitor/group.
    code, streams = command(argv, output, env=env, capture=512 * 1024, capture_both=True)
    diagnostics = {'stdout.log': streams['stdout'], 'stderr.log': streams['stderr'],
                   'command.json': encoded({'argv': argv, 'exit_code': code})}
    export = Path('/durability-evidence')
    need(export.is_dir() and not export.is_symlink() and not list(export.iterdir()), 'Fresh owned export required')
    for name, raw in diagnostics.items():
        with (export / name).open('xb') as stream: stream.write(raw)
    output.write(streams['stdout']); output.write(streams['stderr'])
    if code:
        return code
    sample = parse_sample(streams['stdout'] + b'\n' + streams['stderr'])
    actual = sample['runtime']['binding']
    need({**actual, 'nextest_run_id': None} == config, 'Child substituted source/sample bindings')
    bundle, _ = modules()
    destination = Path('/target/native-sample-bundle')
    receipt = bundle['collect'](sample, scratch, destination, actual)
    # Bound every retained byte before the first host-mounted payload write.
    payloads = {entry['file']: read(destination / entry['file']) for entry in receipt['files']}
    payloads['receipt.json'] = encoded(receipt)
    need(len(payloads) + len(diagnostics) <= 256 and all(0 < len(v) <= CAP for v in payloads.values()) and
         sum(map(len, payloads.values())) + sum(map(len, diagnostics.values())) <= TOTAL,
         'Whole native export including diagnostics/receipt exceeds original envelope')
    for name, raw in payloads.items():
        with (export / name).open('xb') as stream:
            stream.write(raw)
    need(read(Path('/durability-binding.json'), 4096) == config_raw, 'Input binding changed during sample')
    output.emit({'native_durability_export': 'ready', 'binding': actual, 'files': len(payloads),
                 'bytes': sum(map(len, payloads.values())), 'scope': 'Host independent verification required before disposal'})
    return 0


def verify_container(info, record, source, harness, input_path, output_path):
    """Inspect acknowledged native worker settings before start and after exit."""
    host = info['HostConfig']; config = info['Config']
    need(info['Image'] == record['image'] and config['User'] == '10001:10001', 'Native worker image/user')
    need(host['Memory'] == 4294967296 and host['MemorySwap'] == 4294967296 and host['PidsLimit'] == 256 and
         host['NanoCpus'] == 2000000000 and host['ReadonlyRootfs'] is True and not host['Privileged'] and
         host['NetworkMode'] == 'none' and not host.get('PortBindings') and not host.get('Devices') and
         not host.get('CapAdd') and set(host['CapDrop']) == {'ALL'} and
         host['SecurityOpt'] == ['no-new-privileges'] and host.get('PidMode', '') == '' and host.get('IpcMode') == 'private',
         'Native worker engine resource/isolation settings changed')
    need(not config.get('ExposedPorts') and not config.get('Volumes'), 'Native worker image injected ports/volumes')
    expected = {'/workspace': ('bind', str(source).replace('\\', '/'), False),
                '/harness': ('bind', str(harness).replace('\\', '/'), False),
                '/durability-binding.json': ('bind', str(input_path).replace('\\', '/'), False),
                '/durability-evidence': ('bind', str(output_path).replace('\\', '/'), True),
                '/cargo': ('volume', record['namespace'] + '-cargo', True),
                '/target': ('volume', record['namespace'] + '-target', True)}
    mounts = info['Mounts']; seen = set()
    for mount in mounts:
        destination = mount['Destination']
        if destination == '/tmp' and mount['Type'] == 'tmpfs':
            continue
        need(destination in expected and destination not in seen, 'Unexpected/duplicate native worker mount')
        kind, origin, writable = expected[destination]
        actual = mount.get('Name') if kind == 'volume' else mount['Source'].replace('\\', '/')
        need(mount['Type'] == kind and actual == origin and mount['RW'] is writable, 'Native worker mount differs')
        seen.add(destination)
    need(seen == set(expected) and host.get('Tmpfs') == {'/tmp': 'rw,nosuid,nodev,size=67108864'}, 'Native worker mount/tmpfs inventory')


def full40_control(index, decoded):
    """Replay complete retained context copies with a valid unrelated completed sync.

    Preserve declaration/token/order. Replace its completed transaction contents
    with the actual preceding image and adjust the physical write lengths. The
    IO-prefix parser must accept; the temporal content oracle must refuse.
    """
    bundle, verifier = modules(); failure = verifier['F']; physical = failure['V']
    results = []
    candidate = index['sample']['candidate']
    expected_reason = ('Ready declared entries absent from completed contents' if candidate == 'raft-rs'
                       else 'Append declared entries/purge absent from completed contents')
    for label, _ in bundle['cases'](index['sample']):
        original = decoded[label]
        rows = [physical['decode'](line) for name in sorted(original) if '/events-' in name for line in original[name].splitlines()]
        # Select a genuine append transaction that follows an existing image.
        ledger = physical['Transactions'](original); previous = {}; completions = {}
        chosen = None
        for ordinal, row in enumerate(rows):
            if row['kind'] == 'disk_persisted':
                name = row['directory']; token = encoded(row['transaction'])
                previous[token] = ledger.disks[name]['raw']; completions[token] = ordinal
            ledger.observe(row)
            if row['kind'] == ('ready_persisted' if candidate == 'raft-rs' else 'append_readable_and_persisted'):
                token = encoded(row['transaction'])
                if row['entries' if candidate == 'raft-rs' else 'indices'] and previous.get(token):
                    chosen = (ordinal, token); break
        need(chosen is not None, 'Full40 context lacks append/unrelated-sync replay point: ' + label)
        ordinal, token = chosen; mutated = copy.deepcopy(rows); files = dict(original)
        completed = mutated[completions[token]]; before = previous[token]
        # A literal delta reproduces the real predecessor, with valid framing.
        delta = b'HSRCHW01' + len(before).to_bytes(4, 'little') + len(before).to_bytes(4, 'little') + b'\1' + len(before).to_bytes(4, 'little') + before
        need(len(delta) <= CAP, 'Full40 controlled delta bound')
        name = next('wire-%03d.bin' % i for i in range(256) if 'artifacts/wire-%03d.bin' % i not in files)
        files['artifacts/' + name] = delta
        need(len(files) <= 256 and sum(map(len, files.values())) <= TOTAL, 'Full40 complete-copy byte/file bound')
        completed['wire'] = {'file': name, 'offset': 0, 'bytes': len(delta)}
        difference = len(before) - completed['bytes']; completed['bytes'] = len(before)
        writes = [r for r in mutated[:completions[token]] if r['kind'] == 'file_write' and encoded(r['transaction']) == token]
        need(writes and writes[-1]['bytes'] + difference > 0, 'Full40 write-length accounting')
        writes[-1]['bytes'] += difference
        check = physical['Transactions'](files)
        for row in mutated[:ordinal + 1]:
            check.observe(row)
        check.current(mutated[ordinal], mutated[ordinal]['transaction']['disk'])
        try:
            # Invoke the independent content ledger directly: the original
            # complete sample already passed order_events. Later genuine
            # deltas still name the original image; only the intended append
            # content rejection at this valid IO prefix is an acceptable result.
            failure['COMMIT']['verify'](mutated, candidate, files)
        except ValueError as error:
            need(str(error) == expected_reason, 'Full40 replay failed for a different reason: ' + str(error))
            results.append({'context': label, 'result': 'refused', 'reason': str(error), 'physical_prefix': 'valid'})
        else:
            raise ValueError('Full40 unrelated-sync false acceptance: ' + label)
        need(decoded[label] == original, 'Original full40 bytes changed')
    need(len(results) == 40, 'Full40 retained-copy control inventory')
    return {'status': 'pass', 'contexts': results, 'count': 40}


def verify_export(directory, expected):
    """Host-side stopped-CID verification; no guest summary is trusted as a pass."""
    directory = Path(directory)
    validate_input(expected)
    need(directory.resolve() == directory and directory.is_dir() and not directory.is_symlink(), 'Native export root')
    paths = list(directory.iterdir())
    need(2 <= len(paths) <= 256, 'Native export count')
    raw = {p.name: read(p, allow_empty=p.name in {'stdout.log', 'stderr.log'}) for p in paths}
    originals = dict(raw)
    need(sum(map(len, raw.values())) <= TOTAL, 'Native export aggregate bound')
    need('receipt.json' in raw, 'Native export receipt missing')
    receipt = decode(raw.pop('receipt.json'))
    diagnostics = {name: raw.pop(name) for name in ('stdout.log', 'stderr.log', 'command.json')}
    need(sum(map(len, diagnostics.values())) <= 512 * 1024 + 4096, 'Native retained diagnostic bound')
    command_receipt = decode(diagnostics['command.json'])
    need(command_receipt['exit_code'] == 0, 'Native exported command failed')
    actual = receipt['binding']
    need(actual['protoc_sha256'] == PROTOC[expected['target']], 'Exported protoc differs from qualified native pin')
    sample_from_streams = parse_sample(diagnostics['stdout.log'] + b'\n' + diagnostics['stderr.log'])
    need(sample_from_streams['runtime']['binding'] == actual, 'Retained command sample binding differs')
    need({k: v for k, v in actual.items() if k not in {'nextest_run_id', 'protoc_sha256'}} ==
         {k: v for k, v in expected.items() if k != 'nextest_run_id'}, 'Export changed admitted binding')
    need(set(raw) == {r['file'] for r in receipt['files']}, 'Unlisted exported data')
    bundle, verifier = modules()
    # The immutable loader requires an exact bundle-only directory. Materialize
    # only bounded copies; no original sample bytes or public receipt are edited.
    with tempfile.TemporaryDirectory(prefix='hsearch-native-oracle-') as temporary:
        scratch = Path(temporary).resolve(); packed = scratch / 'bundle'; packed.mkdir()
        for name, content in raw.items():
            need(re.fullmatch(r'index\.json|payload-[0-9]{3}\.bin', name), 'Native exported path')
            (packed / name).write_bytes(content)
        index, decoded = bundle['load'](packed, receipt, actual)
        need(index['sample'] == sample_from_streams, 'Packed sample differs from actual command streams')
        result = verifier['verify'](packed, receipt, actual, scratch)
        replay = full40_control(index, decoded)
    need(result['native_complete_sample'] == 'pass' and result['temporal_application']['contexts'] == 40 and
         result['unsafe_controls']['actual_artifact_negative_controls'] == 'pass' and
         len(result['unsafe_controls']['controls']) == 7, 'Incomplete strict sample verification')
    need(set(p.name for p in directory.iterdir()) == set(originals) and
         all(read(directory / name, allow_empty=name in {'stdout.log', 'stderr.log'}) == content for name, content in originals.items()),
         'Export changed during independent verification')
    return {'binding': actual, 'sample': result, 'full40_unrelated_sync': replay,
            'files': len(paths), 'bytes': sum(p.stat().st_size for p in paths)}


def publish_export(source, destination):
    """Copy only already bounded public bytes; no registry traversal or Docker cp."""
    need(not destination.exists() and destination.parent.resolve() == destination.parent, 'Fresh public sample export')
    payloads = {}
    for path in source.iterdir():
        need(len(payloads) < 256, 'Public sample copy count')
        need(path.name in {'index.json', 'receipt.json', 'stdout.log', 'stderr.log', 'command.json'} or
             re.fullmatch(r'payload-[0-9]{3}\.bin', path.name), 'Unlisted public export file')
        payloads[path.name] = read(path, allow_empty=path.name in {'stdout.log', 'stderr.log'})
        need(len(payloads) <= 256 and sum(map(len, payloads.values())) <= TOTAL, 'Public sample copy bound')
    destination.mkdir(mode=0o700)
    for name, raw in payloads.items():
        with (destination / name).open('xb') as stream: stream.write(raw)
    need(all((destination / name).read_bytes() == raw for name, raw in payloads.items()), 'Public sample transfer mismatch')


def object_campaign(sandbox, claim, source, namespace_changed, public_output, **options):
    return runpy.run_path(str(ROOT / 'tools/object_durability_campaign.py'))['run_object'](
        sandbox, claim, source, namespace_changed, public_output, **options)


def run_campaign(sandbox, claim, source, namespace_changed=lambda value: None, public_output=None, *, checkpoint=None, deadline=None, checkpoint_context=None):
    """Object144, then twelve serial fresh native workers; never retry a failure."""
    need(callable(checkpoint) and deadline is not None and type(checkpoint_context) is dict,
         'Whole campaign requires trusted checkpoint controller')
    options = {'checkpoint': checkpoint, 'deadline': deadline, 'checkpoint_context': checkpoint_context}
    target = sandbox.docker.info['Architecture'] + '-unknown-linux-gnu'
    cells = matrix(target); results = []; uuids = set(); cids = set()
    task = runpy.run_path(str(ROOT / 'tools/task.py'))
    need(task['source_fingerprint'](sandbox.source_root, source['head'], []) == source, 'Stale campaign source')
    object_result = object_campaign(sandbox, claim, source, namespace_changed, public_output, **options)
    need(object_result['object_cases'] == 144 and object_result['target'] == target, 'Incomplete/wrong-architecture object campaign')
    for row in cells:
        deadline.require(1250 + 12 * 180 + 600)
        namespace = sandbox.up('M02.10', task_claim=claim)
        namespace_changed(namespace)
        # Each up creates fresh owned cache/target volumes. No previous target
        # or sample reservation survives a successful iteration.
        record = sandbox.registry.get(namespace)
        spec = binding(source, record['image'], row, sandbox.source_root)
        path = sandbox.snapshot(record) / 'durability-binding.json'
        raw = encoded(spec); need(len(raw) <= 4096, 'Native input bound')
        with path.open('xb') as stream: stream.write(raw)
        path.chmod(0o444)
        sandbox.registry.update(namespace, native_durability_input={'bytes': len(raw), 'sha256': digest(raw)})
        sandbox.run(namespace, 'prefetch', output_budget=512*1024, stream=False, **options)
        export = sandbox.snapshot(record) / 'durability-evidence'
        try:
            result = sandbox.run(namespace, 'durability-sample', output_budget=512*1024, stream=False, artifact_directory=export, **options)
        except Exception as error:
            if public_output is not None and export.exists():
                try:
                    key = row['candidate'] + '-' + row['profile'] + '-' + str(row['repetition']) + '-failed'
                    publish_export(export, public_output / key)
                except Exception as copy_error:
                    error.add_note('Failed public export refused; original worker/snapshot retained: ' + str(copy_error)[:512])
            raise
        proof = result['native_durability']
        run_id = proof['binding']['nextest_run_id']; cid = result['container_id']
        need(run_id not in uuids and cid not in cids, 'Reused nextest process or acknowledged worker')
        uuids.add(run_id); cids.add(cid)
        need(task['source_fingerprint'](sandbox.source_root, source['head'], []) == source, 'Source changed during strict sample')
        results.append({'cell': row, 'container_id': cid, 'checkpoint': result['checkpoint']})
        sandbox.down(namespace)
        namespace_changed(None)
    need(len(results) == 12, 'Incomplete per-architecture native campaign')
    return {'native_durability_campaign': 'pass', 'target': target, 'source': source, 'samples': results,
            'sample_count': 12, 'temporal_contexts': 480, 'unsafe_controls': 84, 'full40_replays': 480,
            'full40_replay_sets': 12, 'full40_context_replays': 480, 'object144': object_result,
            'scope': 'One native architecture only. full40_replays counts individual contexts, not whole40-case sets. Other architecture, ordinary suites, coverage, reviews and CI remain separate requirements.'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.absolute()
    need(output.parent.resolve() == output.parent and not output.exists() and not output.is_relative_to(ROOT), 'New external campaign output required')
    output.mkdir(mode=0o700)
    try:
        return ci_campaign(args, output)
    except Exception as error:
        failure = {'status': 'incomplete', 'failure_type': type(error).__name__,
                   'scope': 'No retry. Successful prefix and unknown/failed worker resources retained.'}
        try:
            with (output / 'failure.json').open('xb') as handle:
                handle.write(encoded(failure)); handle.flush(); os.fsync(handle.fileno())
        except Exception as retention_error:
            error.add_note('Public failure export unavailable: '+type(retention_error).__name__)
        raise


def ci_campaign(args, output):
    # Direct entry is CI-only, using the exact existing selected native job.
    ci = runpy.run_path(str(ROOT / 'tools/ci.py'))
    plan_raw = os.environ['CI_PLAN'].encode()
    need(len(plan_raw) <= 65536, 'Bounded existing CI plan required')
    plan = ci['validate_plan'](decode(plan_raw))
    need(plan['full'] or 'hsearch-testkit' in plan['packages'], 'Durability campaign not selected')
    task = runpy.run_path(str(ROOT / 'tools/task.py'))
    head = task['git']('rev-parse', 'HEAD', root=ROOT).strip()
    need(plan['head'] == head, 'Campaign CI plan head differs')
    source = task['source_fingerprint'](ROOT, head, [])
    need(source['clean'], 'Clean campaign source required')
    cutoff = float(os.environ['HSEARCH_CAMPAIGN_DEADLINE'])
    checkpoints = runpy.run_path(str(ROOT / 'tools/campaign_checkpoint.py'))
    clock = checkpoints['Deadline'](cutoff)
    # Root identity comes from this source, not an alternate alias of live pools.
    store = task['TaskStore'](args.state_root / 'tasks')
    owner = 'ci-' + os.environ['GITHUB_RUN_ID'] + '-' + os.environ['GITHUB_JOB'] + '-' + os.environ['GITHUB_RUN_ATTEMPT']
    try:
        clock.require(1250 + 12 * 180 + 600)
        value = task['packet']({'schema_version': 1, 'task_id': 'M02.10',
            'research_campaign': 'durability-v1', 'goal': 'Complete fixed durability campaign',
            'base_revision': head, 'owner_session': owner,
            'owned_paths': ['crates/testkit', 'tools/native_durability_campaign.py', 'tools/object_durability_campaign.py'],
            'assumptions': ['Both actual native architectures remain required.'],
            'acceptance': ['Ordinary check, object144, all12 strict cells and complete independently verified exports.'],
            'negative_cases': ['Persistence refusal retains resources and successful prefix.'],
            'commands': ['prefetch', 'check'], 'output_budget_bytes': 1024**2,
            'expected_evidence': ['Source-bound bounded raw/proof/journal references; no partial pass.']})
        sandbox = runpy.run_path(str(ROOT / 'tools/sandbox.py'))['Sandbox'](args.state_root / 'leases')
        need(sandbox.docker.info['Architecture'] == platform.machine(), 'CI host/engine architecture differs')
        need(sandbox.docker.info['NCPU'] >= 4 and sandbox.docker.info['MemTotal'] >= 12 * 1024**3 and
             sandbox.registry.free_disk() >= 39 * 1024**3, 'Clean hosted campaign capacity unavailable')
        with sandbox.registry.edit() as registry:
            need(not registry['leases'], 'Existing live or uncertain reservations block campaign preflight')
        claim = store.claim(value)
        sandbox.image()
        store.run(claim, owner, 'prefetch', args.state_root / 'leases', public_output=output, deadline=cutoff)
        result = store.run(claim, owner, 'check', args.state_root / 'leases', public_output=output, deadline=cutoff)
    except Exception:
        raise
    (output / 'campaign.json').write_bytes(encoded(result))
    print(json.dumps({'native_durability_campaign': 'pass', 'samples': 12}))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, RuntimeError, KeyError, TypeError) as error:
        print('NATIVE CAMPAIGN REFUSED: ' + type(error).__name__)
        raise SystemExit(1)
