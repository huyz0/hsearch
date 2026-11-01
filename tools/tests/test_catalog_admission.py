"""Original independent work/count/isolation oracles for the synthetic model."""
from pathlib import Path
import runpy
import unittest

MODEL = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'catalog_admission.py'))
Admission, Limits, Refused = (MODEL[name] for name in ['Admission', 'Limits', 'Refused'])


class AdmissionModelControls(unittest.TestCase):
    def test_explicit_empty_or_wrong_typed_tenant_policies_do_not_silently_become_defaults(self):
        for field in ['request_shares', 'rpc_shares']:
            for policy in [{}, False, [], 0]:
                with self.assertRaisesRegex(Refused, 'tenant_policy_bound'):
                    Admission(**{field: policy})
        self.assertEqual(Admission().request_pool.reserved, {'a': 1, 'b': 1})

    def test_late_deadline_observation_never_creates_immortal_receipts(self):
        maximum = 2**63 - 1
        model = Admission(Limits(terminal_receipts=2, receipt_ticks=maximum - 100))
        requests = [model.reserve(tenant, [0], deadline_ticks=1) for tenant in ['a', 'b']]
        model.advance(5000)
        self.assertEqual([model.terminal[r]['expires'] for r in requests], [maximum - 99] * 2)
        boundary = Admission(Limits(terminal_receipts=2, receipt_ticks=maximum - 100, deadline_ticks=maximum))
        for tenant in ['a', 'b']: boundary.reserve(tenant, [0], deadline_ticks=1)
        boundary.advance(5000); boundary.advance(maximum - 5000)
        self.assertEqual(boundary.terminal, {})
        self.assertEqual(sum(boundary.receipt_pool.used.values()), 0)
        default = Admission(Limits(deadline_ticks=maximum))
        request = default.reserve('a', [0], deadline_ticks=1)
        default.advance(maximum)
        self.assertEqual(default.terminal, {})  # Retention already elapsed when expiry is observed.
        self.assertEqual(sum(default.receipt_pool.used.values()), 0)

    def test_owned_malformed_status_releases_all_reservations_but_foreign_call_does_not(self):
        model = Admission(); request = model.reserve('a', [0, 1], request_bytes=5, decoded_bytes=7)
        for partition in [0, 1]: model.start_rpc('a', request, partition)
        with self.assertRaisesRegex(Refused, 'tenant_mismatch'):
            model.finish_rpc('b', request, 0, success=1)
        self.assertIn(request, model.requests)
        with self.assertRaisesRegex(Refused, 'partition_state'):
            model.finish_rpc('a', request, 0, success=1)
        self.assertNotIn(request, model.requests)
        self.assertEqual(model.request_pool.used['a'], 0)
        self.assertEqual(model.rpc_pool.used['a'], 0)
        self.assertEqual(model.queue['a'], 0)
        self.assertEqual(model.terminal[request]['reason'], 'response_refused')
        with self.assertRaises(Refused): model.complete('a', request)

    def test_count_and_merged_bucket_overflow_reclaim_all_inflight_work(self):
        for first_count, first_bucket in [(2**63 - 1, 0), (0, 2**63 - 1)]:
            model = Admission(); request = model.reserve('a', [0, 1, 2])
            for partition in [0, 1, 2]: model.start_rpc('a', request, partition)
            model.finish_rpc('a', request, 0, count=first_count, buckets={'same': first_bucket})
            with self.assertRaisesRegex(Refused, 'overflow'):
                model.finish_rpc('a', request, 1, count=1, buckets={'same': 1})
            self.assertNotIn(request, model.requests)
            self.assertEqual(model.rpc_pool.used['a'], 0)
            self.assertEqual(model.request_pool.used['a'], 0)
            self.assertEqual(model.queue['a'], 0)
            with self.assertRaises(Refused): model.complete('a', request)

    def test_logical_receipt_expiry_overflow_refuses_before_admission(self):
        model = Admission(Limits(receipt_ticks=2**63 - 1))
        with self.assertRaisesRegex(Refused, 'receipt_expiry_overflow'):
            model.reserve('a', [0])
        self.assertEqual(model.requests, {})
        self.assertEqual(sum(model.receipt_pool.used.values()), 0)

    def test_idle_tenant_receipt_share_survives_cancellation_churn(self):
        model = Admission(Limits(terminal_receipts=4))
        for _ in range(3): model.cancel('a', model.reserve('a', [0]))
        with self.assertRaisesRegex(Refused, 'reserved_capacity'): model.reserve('a', [0])
        other = model.reserve('b', [0])
        self.assertEqual(model.receipt_pool.used, {'a': 3, 'b': 1})
        model.cancel('b', other)

    def test_complete_model_response_bytes_are_checked_without_a_false_exact_result(self):
        model = Admission(Limits(response_bytes=16))
        request = model.reserve('a', [0]); model.start_rpc('a', request, 0)
        model.finish_rpc('a', request, 0, count=1)
        with self.assertRaisesRegex(Refused, 'response_bytes'): model.complete('a', request)
        self.assertEqual(model.terminal[request]['reason'], 'response_bytes')
        self.assertEqual(sum(model.request_pool.used.values()), 0)

    def test_total_targets_and_concurrency_are_separate(self):
        model = Admission()
        with self.assertRaisesRegex(Refused, 'selected_partitions'): model.reserve('a', list(range(65)))
        self.assertEqual(model.requests, {})
        request = model.reserve('a', list(range(64)))
        for partition in range(16): model.start_rpc('a', request, partition)
        with self.assertRaisesRegex(Refused, 'inflight_rpcs'): model.start_rpc('a', request, 16)
        self.assertEqual(model.requests[request].attempts, 16)
        model.cancel('a', request)
        self.assertEqual(sum(model.rpc_pool.used.values()), 0)

    def test_idle_tenant_reservations_survive_request_and_rpc_skew(self):
        model = Admission()
        requests = [model.reserve('a', list(range(16))) for _ in range(3)]
        with self.assertRaisesRegex(Refused, 'reserved_capacity'): model.reserve('a', [0])
        other = model.reserve('b', [0])
        for partition in range(16): model.start_rpc('a', requests[0], partition)
        for partition in range(8): model.start_rpc('a', requests[1], partition)
        with self.assertRaisesRegex(Refused, 'reserved_capacity'): model.start_rpc('a', requests[1], 8)
        model.start_rpc('b', other, 0)
        self.assertEqual(model.rpc_pool.used, {'a': 24, 'b': 1})
        for request in requests: model.cancel('a', request)
        self.assertEqual(model.rpc_pool.used['b'], 1)
        model.finish_rpc('b', other, 0, count=7)
        self.assertEqual(model.complete('b', other)['count'], {'value': 7, 'relation': 'eq', 'complete': True})

    def test_attempts_include_failed_calls_and_never_extend_deadline(self):
        model = Admission(Limits(attempts=2, retries_per_partition=1))
        request = model.reserve('a', [0], deadline_ticks=4)
        for _ in range(2):
            model.start_rpc('a', request, 0, remote_bytes=3)
            model.finish_rpc('a', request, 0, success=False, remote_bytes=2)
        with self.assertRaisesRegex(Refused, 'remote_attempts'): model.start_rpc('a', request, 0)
        with self.assertRaisesRegex(Refused, 'incomplete_scope'): model.complete('a', request)
        model.advance(4)
        self.assertNotIn(request, model.requests)
        self.assertEqual(model.terminal[request]['reason'], 'deadline')
        self.assertEqual(model.terminal[request]['attempts'], 2)
        self.assertEqual(model.terminal[request]['remote_bytes'], 10)
        self.assertEqual(sum(model.request_pool.used.values()), 0)

    def test_exact_counts_and_buckets_need_every_selected_partition(self):
        model = Admission(); request = model.reserve('a', [2, 5])
        model.start_rpc('a', request, 2); model.finish_rpc('a', request, 2, count=3, buckets={'red': 2, 'blue': 1})
        with self.assertRaisesRegex(Refused, 'incomplete_scope'): model.complete('a', request)
        model.start_rpc('a', request, 5); model.finish_rpc('a', request, 5, count=4, buckets={'red': 1, 'green': 3})
        expected = {'count': {'value': 7, 'relation': 'eq', 'complete': True}, 'buckets': {'blue': 1, 'green': 3, 'red': 3}}
        self.assertEqual(model.complete('a', request), expected)
        self.assertEqual(sum(model.rpc_pool.used.values()), 0)

    def test_worker_and_coordinator_bucket_overflow_never_truncates_to_success(self):
        for worker, coordinator, payload in [(1, 8, {'a': 1, 'b': 1}), (8, 1, {'a': 1, 'b': 1})]:
            model = Admission(Limits(worker_buckets=worker, coordinator_buckets=coordinator))
            request = model.reserve('a', [0]); model.start_rpc('a', request, 0)
            with self.assertRaises(Refused): model.finish_rpc('a', request, 0, count=2, buckets=payload)
            self.assertNotIn(request, model.requests)
            self.assertEqual(model.terminal[request]['reason'], 'response_refused')
            self.assertEqual(sum(model.rpc_pool.used.values()), 0)
            with self.assertRaises(Refused): model.complete('a', request)

    def test_byte_cpu_and_queue_limit_plus_one_have_no_hidden_admission(self):
        model = Admission(Limits(request_bytes=10, decoded_bytes=10, tenant_queue_bytes=15, global_queue_bytes=30, remote_bytes=5, cpu_work=5))
        for args in [{'request_bytes': 11}, {'decoded_bytes': 11}, {'request_bytes': 10, 'decoded_bytes': 6}]:
            with self.assertRaises(Refused): model.reserve('a', [0], **args)
        self.assertEqual(model.requests, {})
        request = model.reserve('a', [0], request_bytes=10, decoded_bytes=5)
        with self.assertRaises(Refused): model.start_rpc('a', request, 0, remote_bytes=6)
        with self.assertRaises(Refused): model.start_rpc('a', request, 0, cpu_work=6)
        self.assertEqual(model.requests[request].attempts, 0)
        model.start_rpc('a', request, 0, remote_bytes=5, cpu_work=5)
        with self.assertRaises(Refused): model.finish_rpc('a', request, 0, remote_bytes=1)
        self.assertEqual(model.queue['a'], 0)

    def test_boolean_ids_costs_and_wrong_tenant_cannot_alias_or_release(self):
        model = Admission(); request = model.reserve('a', [1])
        for call in [lambda: model.start_rpc('a', request, True), lambda: model.start_rpc('a', request, 1, cpu_work=True),
                     lambda: model.cancel('b', request), lambda: model.reserve('a', [True])]:
            with self.assertRaises(Refused): call()
        self.assertIn(request, model.requests)
        model.cancel('a', request); model.cancel('a', request)
        self.assertEqual(model.request_pool.used['a'], 0)
        with self.assertRaises(Refused): model.cancel('b', request)

    def test_terminal_receipt_capacity_and_expiry_are_bounded(self):
        model = Admission(Limits(terminal_receipts=2, receipt_ticks=3))
        first = model.reserve('a', [0]); model.cancel('a', first)
        second = model.reserve('b', [0]); model.cancel('b', second)
        with self.assertRaisesRegex(Refused, 'idempotency_retention'): model.reserve('a', [0])
        model.advance(3)
        self.assertEqual(model.terminal, {})
        self.assertNotEqual(model.reserve('a', [0]), first)
