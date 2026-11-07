"""Component controls; fresh synthetic data only, no operation-policy imports.

Diagnostic authorities below are explicit test doubles, not deployment proof.
Linux tests exercise real FD scandir/stat/open; Windows exercises refusal.
"""
import copy
from dataclasses import asdict, replace
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from tools.retention_context_v1 import (
    CursorPool, CursorProof, Domain, LinuxClock, Original, Receipt, Refused,
    Sample, Session, decode, digest, encode,
)
from tools.retention_context_v1.native import observe_host

ROOT = Path(__file__).resolve().parents[2]
P = 'a'*64
DOMAIN = Domain('b'*64, 'c'*64, P, 'd'*64)


class FixedClock:
    def __init__(self):
        self.now = Sample(100, 200, 300, DOMAIN)

    def sample(self):
        return self.now


class MemoryStore:
    def __init__(self):
        self.raw = None
        self.acknowledged = True
        self.fail = False
        self.hook = lambda: None

    def load(self):
        return Receipt(self.raw, self.acknowledged)

    def compare_exchange(self, expected, replacement):
        if self.fail:
            raise OSError('synthetic uncertain CAS')
        if self.raw != expected:
            return Receipt(self.raw, False)
        self.raw = replacement
        self.hook()
        return Receipt(self.raw, self.acknowledged)


class FileStore(MemoryStore):
    """Fresh test-only single-writer disk CAS. No physical persistence claim."""
    def __init__(self, path):
        super().__init__()
        self.path = Path(path)

    def load(self):
        raw = self.path.read_bytes() if self.path.exists() else None
        return Receipt(raw, True)

    def compare_exchange(self, expected, replacement):
        if self.load().raw != expected:
            return Receipt(self.load().raw, False)
        temporary = self.path.with_suffix('.pending')
        with temporary.open('xb') as stream:
            stream.write(replacement)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, self.path)
        return Receipt(replacement, True)


class DiagnosticAuthority:
    """Tests deliberately do not claim proc/no-rewind/libc work qualification."""
    def verify(self, facts, *directory):
        if directory:
            return CursorProof(P, 'e'*64, 255, 8192, 1114112, 65536, 24, 24)
        return Domain(facts['boot'], facts['namespace'], P, 'f'*64)


class PortableRealTestClock:
    def sample(self):
        m = time.monotonic_ns()
        return Sample(m, m, time.time_ns(), DOMAIN)


def fixture(clock=None, duration=1200):
    clock = clock or FixedClock()
    store = MemoryStore()
    original = Original.begin(P, '1'*64, '2'*64, clock, duration)
    session = Session.publish(original, store, clock)
    return clock, store, original, session


def child(path, proof, clock_mode, sample=None):
    # Same executable and imported module in a genuinely new interpreter.
    code = '''
import sys,json
from tools.tests.test_retention_context_v1 import *
store=FileStore(sys.argv[1])
if sys.argv[3]=='fixed':
 c=FixedClock(); s=json.loads(sys.argv[4]); c.now=Sample(s[0],s[1],s[2],DOMAIN)
else:
 c=LinuxClock(DiagnosticAuthority()) if sys.platform=='linux' else PortableRealTestClock()
try:
 session=Session(store,c,sys.argv[2]); print(json.dumps(dict(ok=True,remaining=session.check(),credits=session.credits())))
except Refused:
 print(json.dumps(dict(ok=False)))
'''
    result = subprocess.run([sys.executable, '-B', '-c', code, str(path), proof, clock_mode,
                             json.dumps(sample)], cwd=ROOT, capture_output=True, timeout=15,
                            env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
    if result.returncode or len(result.stdout)+len(result.stderr) > 8192:
        raise AssertionError('fresh child refused or exceeded bounded diagnostics: '+result.stderr[:256].decode(errors='replace'))
    return json.loads(result.stdout)


class CodecControls(unittest.TestCase):
    def test_roundtrip_and_numeric_edges(self):
        value = dict(x=[None, True, False, -(2**63), 2**63-1, 'é[]\\"'])
        self.assertEqual(decode(encode(value)), value)
        for value in [2**63, -(2**63)-1, 1.0, object(), {'x': '\ud800'}, {1: 2}]:
            with self.subTest(kind=type(value).__name__), self.assertRaises(Refused):
                encode(value)

    def test_lexical24_is_not_review32(self):
        self.assertEqual(decode(b'['*24+b'0'+b']'*24), [[[[[[[[[[[[[[[[[[[[[[[[0]]]]]]]]]]]]]]]]]]]]]]]])
        for n in [25, 32]:
            with self.subTest(depth=n), self.assertRaises(Refused):
                decode(b'['*n+b'0'+b']'*n)

    def test_object_nodes_and_byte_bound(self):
        self.assertEqual(len(decode(encode([0]*49999))), 49999)
        with self.assertRaises(Refused):
            encode([0]*50000)
        self.assertEqual(len(encode('x'*(1048576-2))), 1048576)
        with self.assertRaises(Refused):
            encode('x'*1048576)

    def test_strict_decode(self):
        for raw in [b'', b'{"x":1,"x":2}', b'{ "x":1}', b'NaN', b'1.0', b'\xff',
                    b'1'*100, b'[}', b'"unterminated', b'0 '*1048576]:
            with self.subTest(raw=raw[:32]), self.assertRaises(Refused):
                decode(raw)


class ContextControls(unittest.TestCase):
    def test_fixed_endpoints_and_positive_new_handle(self):
        clock, store, original, session = fixture()
        session.reserve(raw_bytes=16, audit_bytes=100, rescans=1)
        clock.now = replace(clock.now, monotonic_ns=110, boottime_ns=215, wall_ns=320)
        recovered = Session(store, clock, digest(asdict(original)))
        self.assertEqual(recovered.command_timeout_ns(9999), 1180)
        self.assertEqual(recovered.credits()['raw_bytes'], 16)
        self.assertEqual(original_from_raw(store.raw), asdict(original))

    def test_equality_expiry_each_original_clock(self):
        for name, value in [('monotonic_ns', 1300), ('boottime_ns', 1400), ('wall_ns', 1500)]:
            clock, store, original, session = fixture()
            clock.now = replace(clock.now, **{name: value})
            with self.subTest(clock=name), self.assertRaises(Refused):
                session.check()
            clock.now = original.start
            with self.assertRaises(Refused):
                Session(store, clock, digest(asdict(original)))
            self.assertTrue(decode(store.raw)['denied'])

    def test_regression_and_unknown_domain(self):
        for sample in [Sample(99, 200, 300, DOMAIN), Sample(100, 199, 300, DOMAIN),
                       Sample(100, 200, 299, DOMAIN),
                       Sample(100, 200, 300, replace(DOMAIN, boot='0'*64)),
                       Sample(100, 200, 300, replace(DOMAIN, namespace='0'*64)),
                       Sample(100, 200, 300, replace(DOMAIN, evidence='0'*64))]:
            clock, store, original, session = fixture()
            clock.now = sample
            with self.subTest(sample=sample), self.assertRaises(Refused):
                Session(store, clock, digest(asdict(original)))

    def test_wall_rollback_forward_and_observed_highwater(self):
        clock, store, original, session = fixture()
        clock.now = replace(clock.now, wall_ns=1200)
        self.assertEqual(session.check(), 300)
        clock.now = replace(clock.now, wall_ns=1199)
        with self.assertRaises(Refused):
            Session(store, clock, digest(asdict(original)))

    def test_original_duration_cannot_refresh_or_rebind(self):
        clock, store, original, session = fixture()
        with self.assertRaises(Refused):
            Session.publish(original, store, clock)
        with self.assertRaises(Refused):
            Session(store, clock, '0'*64)
        for value in [0, -1, 1200000000001, True]:
            with self.subTest(duration=value), self.assertRaises(Refused):
                Original.begin(P, P, P, clock, value)

    def test_missing_legacy_corrupt_uncertain_proof(self):
        for raw, ack in [(None, True), (b'{}', True), (b'{', True), (b'{}', False)]:
            store = MemoryStore(); store.raw = raw; store.acknowledged = ack
            with self.subTest(raw=raw, ack=ack), self.assertRaises(Refused):
                Session(store, FixedClock(), P)

    def test_raw_aggregate512_reservations_no_refund_or_reset(self):
        clock, store, original, session = fixture()
        for i in range(512):
            session.reserve(raw_bytes=16)
            if i in (0, 100, 300):
                # A process-error/timeout/unused reservation gets no refund.
                session = Session(store, clock, digest(asdict(original)))
        self.assertEqual(session.credits()['raw_bytes'], 8192)
        self.assertEqual(session.credits()['raw_reservations'], 512)
        self.assertLess(len(store.raw), 4096)
        with self.assertRaises(Refused):
            session.reserve(raw_bytes=1)
        with self.assertRaises(Refused):
            Session(store, clock, digest(asdict(original)))

    def test_credit_units_names_caps_and_false_integers(self):
        for charge in [dict(raw_bytes=17), dict(raw_bytes=0), dict(raw_reservations=0),
                       dict(entries=True), dict(audit_bytes=268435457), dict(rescans=33),
                       dict(entries=4097), dict(other=1), dict(audit_bytes=-1)]:
            _, _, _, session = fixture()
            with self.subTest(charge=charge), self.assertRaises(Refused):
                session.reserve(**charge)

    def test_terminal_cas_crossing_deadline_poisoned_and_durable(self):
        clock, store, original, session = fixture()
        store.hook = lambda: setattr(clock, 'now', replace(clock.now, monotonic_ns=1300))
        with self.assertRaises(Refused):
            session.check()
        self.assertTrue(decode(store.raw)['denied'])
        clock.now = original.start
        with self.assertRaises(Refused):
            Session(store, clock, digest(asdict(original)))

    def test_unknown_checkpoint_ack_blocks_same_and_fresh_handles(self):
        clock, store, original, session = fixture()
        store.acknowledged = False
        with self.assertRaises(Refused):
            session.reserve(raw_bytes=16)
        store.acknowledged = True
        with self.assertRaises(Refused):
            session.check()
        # The integration must retain an uncertain acknowledgment. Do not
        # reinterpret its disappearance as component-proved recovery.

    def test_r3_fresh_process_counterexample_original_end_survives_wall_slack(self):
        with tempfile.TemporaryDirectory(prefix='context-counterexample-') as tmp:
            clock = FixedClock(); store = FileStore(Path(tmp)/'proof.json')
            original = Original.begin(P, P, P, clock, 1200)
            Session.publish(original, store, clock)
            result = child(store.path, digest(asdict(original)), 'fixed', [1301, 300, 400])
            self.assertFalse(result['ok'])
            self.assertEqual(original_from_raw(store.path.read_bytes()), asdict(original))

    def test_truly_fresh_process_positive_original_credits_and_absence_expiry(self):
        clock = LinuxClock(DiagnosticAuthority()) if sys.platform == 'linux' else PortableRealTestClock()
        with tempfile.TemporaryDirectory(prefix='context-real-clock-') as tmp:
            store = FileStore(Path(tmp)/'proof.json')
            original = Original.begin(P, P, P, clock, 8000000000)
            session = Session.publish(original, store, clock)
            session.reserve(raw_bytes=16, audit_bytes=42)
            result = child(store.path, digest(asdict(original)), 'real')
            self.assertTrue(result['ok']); self.assertLess(result['remaining'], 8000000000)
            self.assertEqual(result['credits']['raw_bytes'], 16)
            self.assertEqual(original_from_raw(store.path.read_bytes()), asdict(original))
            short = FileStore(Path(tmp)/'short.json')
            original = Original.begin(P, P, P, clock, 1000000000)
            Session.publish(original, short, clock)
            time.sleep(1.1)
            self.assertFalse(child(short.path, digest(asdict(original)), 'real')['ok'])


def original_from_raw(raw):
    return decode(raw)['original']


class NativeCursorControls(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='context-real-iterator-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        _, _, _, self.session = fixture(duration=1200000000000)
        self.pool = CursorPool(self.session, DiagnosticAuthority(), 67108864, 96)
        self.addCleanup(self.pool.close)

    def opened(self, **kw):
        fd = os.open(self.root, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
        try:
            return self.pool.open(fd, **kw)
        finally:
            os.close(fd)

    def unsupported(self):
        if sys.platform != 'linux':
            with self.assertRaises(Refused):
                observe_host()
            return True
        return False

    def populate(self, n):
        for i in range(n):
            (self.root/('entry%05d'%i)).touch()

    def test_real_os_cap_minus_exact_plus_and_empty_charged_probe(self):
        if self.unsupported(): return
        for count, cap in [(0, 0), (1, 2), (2, 2), (3, 2)]:
            with self.subTest(count=count, cap=cap):
                # Separate actual directories and operation contexts.
                with tempfile.TemporaryDirectory(dir=self.root) as tmp:
                    for i in range(count): (Path(tmp)/str(i)).touch()
                    _, _, _, session = fixture(duration=1200000000000)
                    pool = CursorPool(session, DiagnosticAuthority(), 67108864, 96)
                    fd = os.open(tmp, os.O_RDONLY | os.O_DIRECTORY)
                    try:
                        cursor = pool.open(fd, cap=cap)
                        before = session.credits()['audit_bytes']
                        got = []
                        if count > cap:
                            with self.assertRaisesRegex(Refused, 'cursor-overflow'):
                                while True:
                                    item = cursor.next()
                                    if item is None: break
                                    got.append(item)
                        else:
                            while True:
                                item = cursor.next()
                                if item is None: break
                                got.append(item)
                        self.assertEqual(len(got), min(count, cap))
                        self.assertEqual(cursor.probes, min(count, cap)+1)
                        # Overflow is terminal, so inspect the durable debt
                        # without requesting new permission from that session.
                        debt = decode(session.store.raw)['credits']['audit_bytes']
                        self.assertGreater(debt, before)
                        self.assertEqual(pool.fds_used, 0)
                    finally:
                        pool.close(); os.close(fd)

    def test_r3_eager_counterexample_real1000_never_path_iterdir(self):
        if self.unsupported(): return
        self.populate(1000)
        calls = []
        actual = os.scandir
        class Observed:
            def __init__(self, fd): self.inner = actual(fd)
            def __next__(self): calls.append(1); return next(self.inner)
            def close(self): self.inner.close()
        with (patch('pathlib.Path.iterdir', side_effect=AssertionError('eager boundary forbidden')),
              patch('os.scandir', Observed)):
            cursor = self.opened(cap=1)
            self.assertEqual(len(calls), 0)
            self.assertIsNotNone(cursor.next())
            with self.assertRaisesRegex(Refused, 'cursor-overflow'): cursor.next()
        self.assertEqual(len(calls), 2)
        self.assertEqual(cursor.delivered, 1)

    def test_nested_operation_aggregate_entry_cap(self):
        if self.unsupported(): return
        directory = self.root/'nested'; directory.mkdir(); (directory/'leaf').touch()
        self.session.reserve(entries=4095)
        parent = self.opened()
        entry = parent.next()
        child_cursor = parent.open_child(entry)
        with self.assertRaises(Refused): child_cursor.next()
        self.assertEqual(self.session.poisoned, True)

    def test_fd_memory_depth_work_and_stream_capacity(self):
        if self.unsupported(): return
        self.pool.fd_limit = 1
        with self.assertRaisesRegex(Refused, 'cursor-capacity'): self.opened()
        self.pool.fd_limit = 96; self.pool.memory_bytes = 1
        with self.assertRaisesRegex(Refused, 'cursor-capacity'): self.opened()
        self.pool.memory_bytes = 67108864
        cursor = self.opened()
        self.session.reserve(audit_bytes=268435456-self.session.credits()['audit_bytes'])
        with self.assertRaises(Refused): cursor.next()
        self.assertTrue(cursor.closed)

    def test_early_close_error_close_and_rescan_debt(self):
        if self.unsupported(): return
        self.populate(1)
        cursor = self.opened(); cursor.next()
        before = self.session.credits()['audit_bytes']
        cursor.close(); cursor.close()
        self.assertEqual(self.pool.fds_used, 0)
        with self.assertRaises(Refused): cursor.next()
        other = self.opened(); other.next(); other.close()
        self.assertGreater(self.session.credits()['audit_bytes'], before)

    def test_real_alias_and_changed_child_identity_refuse(self):
        if self.unsupported(): return
        (self.root/'outside-link').symlink_to(self.root, target_is_directory=True)
        cursor = self.opened()
        with self.assertRaisesRegex(Refused, 'cursor-alias'): cursor.next()
        self.assertTrue(cursor.closed)

    def test_native_unknown_profile_refuses(self):
        if self.unsupported(): return
        class Unknown:
            def verify(self, *args): raise Refused('native-work-unproved')
        self.pool.authority = Unknown()
        with self.assertRaisesRegex(Refused, 'native-work-unproved'): self.opened()
        self.assertEqual(self.pool.fds_used, 0)

    def test_native_observation_is_not_domain_authority(self):
        if self.unsupported(): return
        facts = observe_host()
        self.assertEqual(len(facts['boot']), 64)
        class Unknown:
            def verify(self, *args): raise Refused('initial-domain-and-no-rewind-unproved')
        with self.assertRaises(Refused): LinuxClock(Unknown()).sample()

    def test_actual_nested_depth24_and25_refusal(self):
        if self.unsupported(): return
        path = self.root
        for _ in range(24):
            path = path/'d'; path.mkdir()
        cursor = self.opened()
        for _ in range(23):
            cursor = cursor.open_child(cursor.next())
        self.assertEqual(cursor.depth, 24)
        with self.assertRaises(Refused): cursor.open_child(cursor.next())
        self.pool.close()
        self.assertEqual(self.pool.fds_used, 0)

    def test_rescan32_aggregate_and_no_recovery_reset(self):
        if self.unsupported(): return
        for _ in range(32):
            self.opened().close()
        self.assertEqual(self.session.credits()['rescans'], 32)
        with self.assertRaises(Refused): self.opened()

    def test_changed_child_inode_and_forged_outside_name(self):
        if self.unsupported(): return
        path = self.root/'child'; path.mkdir()
        cursor = self.opened(); entry = cursor.next()
        with self.assertRaisesRegex(Refused, 'child-name'):
            cursor.open_child(replace(entry, name='../outside'))
        # Keep the old inode allocated so the replacement cannot reuse it.
        path.rename(self.root/'old-child'); path.mkdir()
        with self.assertRaisesRegex(Refused, 'child-identity'): cursor.open_child(entry)
        self.assertEqual(self.pool.fds_used, 3)

    def test_name_unknown_and_insufficient_worst_case_reservation(self):
        if self.unsupported(): return
        class Insufficient:
            def verify(self, *args):
                return CursorProof(P, 'e'*64, 255, 1, 1114112, 65536, 24, 24)
        self.pool.authority = Insufficient()
        with self.assertRaises(Refused): self.opened()
        self.assertEqual(self.pool.fds_used, 0)

    def test_actual_iterator_stat_error_closes_and_retains_debt(self):
        if self.unsupported(): return
        self.populate(1)
        cursor = self.opened()
        actual = os.stat
        def fail(path, *args, **kwargs):
            if kwargs.get('dir_fd') == cursor.fd: raise OSError('synthetic stat failure')
            return actual(path, *args, **kwargs)
        with patch('os.stat', fail), self.assertRaises(OSError): cursor.next()
        self.assertTrue(cursor.closed)
        self.assertEqual(self.pool.fds_used, 0)
        self.assertTrue(decode(self.session.store.raw)['denied'])

    def test_close_failure_keeps_capacity_uncertain_and_no_new_stream(self):
        if self.unsupported(): return
        cursor = self.opened()
        actual = cursor.iterator
        class CloseFailure:
            def close(self): actual.close(); raise OSError('synthetic lost close ack')
        cursor.iterator = CloseFailure()
        with self.assertRaisesRegex(Refused, 'cursor-close-uncertain'): cursor.close()
        self.assertEqual(self.pool.fds_used, 3)
        with self.assertRaises(Refused): self.opened()
        with self.assertRaises(Refused): self.pool.close()
        # Both real handles were closed; only conservative uncertainty remains.
        self.pool.active.remove(cursor)

    def test_deadline_cross_during_next_and_eof_close_never_returns_entry(self):
        if self.unsupported(): return
        for eof in (False, True):
            _, store, original, session = fixture()
            pool = CursorPool(session, DiagnosticAuthority(), 67108864, 96)
            with tempfile.TemporaryDirectory(dir=self.root) as tmp:
                if not eof: (Path(tmp)/'file').touch()
                fd = os.open(tmp, os.O_RDONLY | os.O_DIRECTORY)
                cursor = pool.open(fd); os.close(fd)
                actual = cursor.iterator
                class Late:
                    def __next__(self):
                        value = next(actual)
                        session.clock.now = replace(session.clock.now, monotonic_ns=1300)
                        return value
                    def close(self):
                        actual.close()
                        if eof: session.clock.now = replace(session.clock.now, monotonic_ns=1300)
                cursor.iterator = Late()
                with self.assertRaises(Refused): cursor.next()
                self.assertEqual(pool.fds_used, 0)
                self.assertTrue(decode(store.raw)['denied'])


if __name__ == '__main__':
    unittest.main()
