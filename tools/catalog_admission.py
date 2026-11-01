"""Deterministic admission/work accounting model; no network/runtime guarantees."""
from dataclasses import dataclass, field, fields
from collections import deque
import json


class Refused(ValueError):
    pass


def need(condition, reason):
    if not condition:
        raise Refused(reason)


def number(value, maximum, reason, minimum=0):
    need(type(value) is int and minimum <= value <= maximum, reason)
    return value


def text(value, reason):
    need(type(value) is str and 0 < len(value) <= 128, reason)
    try:
        need(len(value.encode('utf-8')) <= 128, reason)
    except UnicodeError:
        raise Refused(reason) from None


@dataclass(frozen=True)
class Limits:
    selected_partitions: int = 64
    inflight_per_request: int = 16
    global_inflight: int = 32
    global_requests: int = 4
    attempts: int = 128
    retries_per_partition: int = 2
    remote_bytes: int = 1048576
    cpu_work: int = 10000
    request_bytes: int = 10485760
    decoded_bytes: int = 20971520
    response_bytes: int = 10485760
    tenant_queue_bytes: int = 33554432
    global_queue_bytes: int = 67108864
    deadline_ticks: int = 5000
    worker_buckets: int = 128
    coordinator_buckets: int = 512
    count: int = 2**63 - 1
    terminal_receipts: int = 1024
    receipt_ticks: int = 5000

    def __post_init__(self):
        for item in fields(self):
            number(getattr(self, item.name), 2**63 - 1, 'invalid_limit:' + item.name, 0 if item.name == 'retries_per_partition' else 1)
        need(self.selected_partitions <= 1024 and self.global_requests <= 64 and self.global_inflight <= 1024 and
             self.inflight_per_request <= self.global_inflight and self.worker_buckets <= 2048 and
             self.coordinator_buckets <= 4096 and self.terminal_receipts <= 4096, 'model_capacity_ceiling')


class ReservedPool:
    """Known model tenants retain reserved shares even while idle."""
    def __init__(self, total, reserved):
        number(total, 4096, 'pool_total', 1)
        need(type(reserved) is dict and 0 < len(reserved) <= 64, 'tenant_policy_bound')
        for key in reserved: text(key, 'tenant_identity')
        for value in reserved.values(): number(value, total, 'reserved_share', 1)
        need(sum(reserved.values()) <= total, 'overcommitted_reservations')
        self.total, self.reserved = total, dict(reserved)
        self.used = dict.fromkeys(reserved, 0)

    def check(self, tenant):
        text(tenant, 'tenant_identity')
        need(tenant in self.used, 'unknown_tenant')
        shared = sum(max(0, self.used[key] - self.reserved[key]) for key in self.used)
        extra = int(self.used[tenant] >= self.reserved[tenant])
        need(sum(self.used.values()) < self.total and shared + extra <= self.total - sum(self.reserved.values()), 'reserved_capacity')

    def acquire(self, tenant):
        self.check(tenant); self.used[tenant] += 1

    def release(self, tenant):
        need(tenant in self.used and self.used[tenant] > 0, 'unowned_release')
        self.used[tenant] -= 1


@dataclass
class Request:
    tenant: str
    selected: tuple
    queue_bytes: int
    deadline: int
    active: set = field(default_factory=set)
    attempts_by_partition: dict = field(default_factory=dict)
    completed: set = field(default_factory=set)
    buckets: dict = field(default_factory=dict)
    count: int = 0
    attempts: int = 0
    remote_bytes: int = 0
    cpu_work: int = 0
    response_bytes: int = 0


class Admission:
    def __init__(self, limits=Limits(), *, request_shares=None, rpc_shares=None):
        self.limits = limits
        self.request_pool = ReservedPool(limits.global_requests, request_shares or {'a': 1, 'b': 1})
        self.rpc_pool = ReservedPool(limits.global_inflight, rpc_shares or {'a': 8, 'b': 8})
        need(set(self.request_pool.used) == set(self.rpc_pool.used), 'inconsistent_tenant_policies')
        self.receipt_pool = ReservedPool(limits.terminal_receipts, dict.fromkeys(self.request_pool.used, 1))
        self.queue = dict.fromkeys(self.request_pool.used, 0)
        self.requests, self.terminal, self.now, self.sequence = {}, {}, 0, 0
        self.events = deque(maxlen=1024)
        self.dropped_events = 0

    def event(self, kind, request, **details):
        if len(self.events) == self.events.maxlen: self.dropped_events += 1
        self.events.append({'tick': self.now, 'event': kind, 'request': request, **details})

    def reserve(self, tenant, selected, *, request_bytes=0, decoded_bytes=0, deadline_ticks=100):
        text(tenant, 'tenant_identity')
        need(tenant in self.queue, 'unknown_tenant')
        need(type(selected) in {tuple, list} and 0 < len(selected) <= self.limits.selected_partitions, 'selected_partitions')
        for partition in selected: number(partition, 2**32 - 1, 'partition_identity')
        need(len(set(selected)) == len(selected), 'duplicate_partition')
        number(request_bytes, self.limits.request_bytes, 'request_bytes')
        number(decoded_bytes, self.limits.decoded_bytes, 'decoded_bytes')
        number(deadline_ticks, self.limits.deadline_ticks, 'deadline_ticks', 1)
        queued = request_bytes + decoded_bytes
        need(self.queue[tenant] + queued <= self.limits.tenant_queue_bytes and
             sum(self.queue.values()) + queued <= self.limits.global_queue_bytes, 'queue_bytes')
        need(len(self.requests) + len(self.terminal) < self.limits.terminal_receipts, 'idempotency_retention')
        self.request_pool.check(tenant)
        self.receipt_pool.check(tenant)
        number(self.now + deadline_ticks, 2**63 - 1, 'deadline_overflow')
        number(self.now + deadline_ticks + self.limits.receipt_ticks, 2**63 - 1, 'receipt_expiry_overflow')
        number(self.sequence + 1, 2**63 - 1, 'request_identity_overflow')
        self.sequence += 1
        identity = 'request-' + str(self.sequence)
        self.request_pool.acquire(tenant)
        self.receipt_pool.acquire(tenant)
        self.queue[tenant] += queued
        self.requests[identity] = Request(tenant, tuple(selected), queued, self.now + deadline_ticks)
        self.event('admitted', identity, tenant=tenant, selected=len(selected))
        return identity

    def request(self, tenant, identity):
        text(tenant, 'tenant_identity'); text(identity, 'request_identity')
        need(identity in self.requests, 'request_closed_or_unknown')
        value = self.requests[identity]
        need(value.tenant == tenant, 'tenant_mismatch')
        return value

    def start_rpc(self, tenant, identity, partition, *, remote_bytes=0, cpu_work=0):
        number(partition, 2**32 - 1, 'partition_identity')
        request = self.request(tenant, identity)
        need(partition in request.selected and partition not in request.completed and partition not in request.active, 'partition_state')
        need(self.now < request.deadline, 'deadline')
        number(remote_bytes, self.limits.remote_bytes, 'remote_bytes')
        number(cpu_work, self.limits.cpu_work, 'cpu_work')
        need(request.attempts < self.limits.attempts and
             request.attempts_by_partition.get(partition, 0) < 1 + self.limits.retries_per_partition, 'remote_attempts')
        need(request.remote_bytes + remote_bytes <= self.limits.remote_bytes and request.cpu_work + cpu_work <= self.limits.cpu_work, 'total_work')
        need(len(request.active) < self.limits.inflight_per_request, 'inflight_rpcs')
        self.rpc_pool.check(tenant)
        self.rpc_pool.acquire(tenant)
        request.active.add(partition)
        request.attempts += 1
        request.attempts_by_partition[partition] = request.attempts_by_partition.get(partition, 0) + 1
        request.remote_bytes += remote_bytes; request.cpu_work += cpu_work
        self.event('rpc_started', identity, partition=partition, attempt=request.attempts)

    def finish_rpc(self, tenant, identity, partition, *, success=True, count=0, buckets=None, response_bytes=0, remote_bytes=0, cpu_work=0):
        number(partition, 2**32 - 1, 'partition_identity')
        request = self.request(tenant, identity)
        need(partition in request.active, 'partition_state')
        try:
            need(type(success) is bool, 'partition_state')
            number(count, self.limits.count, 'count')
            number(response_bytes, self.limits.response_bytes, 'response_bytes')
            number(remote_bytes, self.limits.remote_bytes, 'remote_bytes')
            number(cpu_work, self.limits.cpu_work, 'cpu_work')
            need(request.response_bytes + response_bytes <= self.limits.response_bytes and
                 request.remote_bytes + remote_bytes <= self.limits.remote_bytes and
                 request.cpu_work + cpu_work <= self.limits.cpu_work, 'total_work')
            buckets = {} if buckets is None else buckets
            need(type(buckets) is dict and len(buckets) <= self.limits.worker_buckets, 'worker_buckets')
            for key, value in buckets.items():
                text(key, 'bucket_key')
                number(value, self.limits.count, 'bucket_count')
            merged = dict(request.buckets)
            if success:
                need(request.count + count <= self.limits.count, 'count_overflow')
                need(len(set(merged) | set(buckets)) <= self.limits.coordinator_buckets, 'coordinator_buckets')
                for key, value in buckets.items():
                    number(merged.get(key, 0) + value, self.limits.count, 'bucket_overflow')
                    merged[key] = merged.get(key, 0) + value
        except (Refused, UnicodeError):
            self._close(identity, 'response_refused')
            raise
        request.active.remove(partition); self.rpc_pool.release(tenant)
        request.remote_bytes += remote_bytes; request.cpu_work += cpu_work; request.response_bytes += response_bytes
        if success:
            request.completed.add(partition); request.count += count; request.buckets = merged
        self.event('rpc_finished', identity, partition=partition, success=success)

    def _close(self, identity, reason, *, terminal_tick=None):
        terminal_tick = self.now if terminal_tick is None else terminal_tick
        expires = number(terminal_tick + self.limits.receipt_ticks, 2**63 - 1, 'receipt_expiry_overflow')
        request = self.requests.pop(identity)
        for _partition in request.active: self.rpc_pool.release(request.tenant)
        self.request_pool.release(request.tenant)
        self.queue[request.tenant] -= request.queue_bytes
        self.terminal[identity] = {'tenant': request.tenant, 'reason': reason, 'expires': expires,
                                   'attempts': request.attempts, 'remote_bytes': request.remote_bytes, 'cpu_work': request.cpu_work}
        self.event('closed', identity, reason=reason)

    def cancel(self, tenant, identity):
        text(tenant, 'tenant_identity'); text(identity, 'request_identity')
        if identity in self.terminal:
            need(self.terminal[identity]['tenant'] == tenant, 'tenant_mismatch')
            return
        self.request(tenant, identity)
        self._close(identity, 'cancelled')

    def complete(self, tenant, identity):
        request = self.request(tenant, identity)
        need(not request.active and request.completed == set(request.selected), 'incomplete_scope')
        result = {'count': {'value': request.count, 'relation': 'eq', 'complete': True}, 'buckets': dict(sorted(request.buckets.items()))}
        if len(json.dumps(result, ensure_ascii=False, separators=(',', ':')).encode('utf-8')) > self.limits.response_bytes:
            self._close(identity, 'response_bytes')
            raise Refused('response_bytes')
        self._close(identity, 'complete')
        return result

    def advance(self, ticks):
        number(ticks, self.limits.deadline_ticks, 'clock_step')
        number(self.now + ticks, 2**63 - 1, 'clock_overflow')
        self.now += ticks
        for identity, request in tuple(self.requests.items()):
            if request.deadline <= self.now: self._close(identity, 'deadline', terminal_tick=request.deadline)
        for identity, receipt in tuple(self.terminal.items()):
            if receipt['expires'] <= self.now:
                self.receipt_pool.release(receipt['tenant'])
                del self.terminal[identity]
