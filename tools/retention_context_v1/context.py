"""Original clock endpoints and acknowledged constant-size aggregate credit CAS."""
from dataclasses import asdict, dataclass
from typing import Protocol

from .codec import decode, digest, encode, fields, integer, need, pin, Refused

MAX_DURATION_NS = 1200000 * 1000000
CAPS = dict(raw_bytes=8192, raw_reservations=512, audit_bytes=268435456,
            rescans=32, entries=4096)


@dataclass(frozen=True)
class Domain:
    boot: str
    namespace: str
    profile: str
    evidence: str

    def validate(self):
        for value in asdict(self).values():
            pin(value)


@dataclass(frozen=True)
class Sample:
    monotonic_ns: int
    boottime_ns: int
    wall_ns: int
    domain: Domain

    def validate(self):
        for n in (self.monotonic_ns, self.boottime_ns, self.wall_ns):
            integer(n, 1)
        need(type(self.domain) is Domain, 'domain-type')
        self.domain.validate()


class Clock(Protocol):
    def sample(self) -> Sample: ...


@dataclass(frozen=True)
class Original:
    version: int
    clock_api: str
    units: str
    operation: str
    source: str
    request: str
    start: Sample
    monotonic_end_ns: int
    boottime_end_ns: int
    wall_end_ns: int

    def validate(self):
        need(type(self.version) is int and self.version == 1, 'context-version')
        need(self.clock_api == 'linux-monotonic-boottime-realtime-v1' and
             self.units == 'integer-nanoseconds', 'clock-api-units')
        for p in (self.operation, self.source, self.request):
            pin(p)
        need(type(self.start) is Sample, 'sample-type')
        self.start.validate()
        durations = []
        for endpoint, start in zip((self.monotonic_end_ns, self.boottime_end_ns, self.wall_end_ns),
                                   (self.start.monotonic_ns, self.start.boottime_ns, self.start.wall_ns)):
            integer(endpoint, 1)
            durations.append(endpoint-start)
        need(len(set(durations)) == 1 and 0 < durations[0] <= MAX_DURATION_NS, 'original-duration')

    @classmethod
    def begin(cls, operation, source, request, clock, duration_ns):
        integer(duration_ns, 1, MAX_DURATION_NS)
        start = clock.sample()
        start.validate()
        value = cls(1, 'linux-monotonic-boottime-realtime-v1', 'integer-nanoseconds',
                    operation, source, request, start, start.monotonic_ns+duration_ns,
                    start.boottime_ns+duration_ns, start.wall_ns+duration_ns)
        value.validate()
        return value


@dataclass(frozen=True)
class Receipt:
    """The integration attests durable acknowledgment, not just a successful write."""
    raw: bytes
    acknowledged: bool


class Store(Protocol):
    def load(self) -> Receipt: ...
    def compare_exchange(self, expected: bytes | None, replacement: bytes) -> Receipt: ...


def sample_from(value):
    fields(value, 'monotonic_ns boottime_ns wall_ns domain')
    fields(value['domain'], 'boot namespace profile evidence')
    sample = Sample(value['monotonic_ns'], value['boottime_ns'], value['wall_ns'], Domain(**value['domain']))
    sample.validate()
    return sample


def original_from(value):
    fields(value, 'version clock_api units operation source request start monotonic_end_ns boottime_end_ns wall_end_ns')
    value = dict(value)
    value['start'] = sample_from(value['start'])
    original = Original(**value)
    original.validate()
    return original


def state_from(raw, expected_pin):
    state = decode(raw)
    fields(state, 'original observed credits denied revision')
    original = original_from(state['original'])
    need(digest(asdict(original)) == pin(expected_pin), 'original-proof-pin')
    observed = sample_from(state['observed'])
    need(observed.domain == original.start.domain, 'observed-domain')
    need(all(a >= b for a, b in zip((observed.monotonic_ns, observed.boottime_ns, observed.wall_ns),
                                   (original.start.monotonic_ns, original.start.boottime_ns, original.start.wall_ns))),
         'observed-regression')
    fields(state['credits'], 'raw_bytes raw_reservations audit_bytes rescans entries')
    for name, cap in CAPS.items():
        integer(state['credits'][name], 0, cap)
    need(state['credits']['raw_bytes'] <= 16*state['credits']['raw_reservations'], 'raw-credit-history')
    need(type(state['denied']) is bool, 'denied-type')
    integer(state['revision'])
    return state, original, observed


class Session:
    """Single logical writer. CAS conflicts/unknown acknowledgments poison this handle.

    The Store must prevent rollback/rebind of acknowledged records. A hash is an
    identity, not authentication or durable antirollback storage.
    """
    def __init__(self, store, clock, expected_pin):
        self.store, self.clock, self.expected_pin = store, clock, pin(expected_pin)
        self.poisoned = False
        self.check()

    @classmethod
    def publish(cls, original, store, clock):
        original.validate()
        state = dict(original=asdict(original), observed=asdict(original.start),
                     credits={k: 0 for k in CAPS}, denied=False, revision=0)
        raw = encode(state)
        receipt = store.compare_exchange(None, raw)
        need(type(receipt) is Receipt and receipt.acknowledged is True and receipt.raw == raw,
             'original-publication-uncertain')
        return cls(store, clock, digest(asdict(original)))

    def _read(self):
        need(not self.poisoned, 'session-poisoned')
        receipt = self.store.load()
        need(type(receipt) is Receipt and receipt.acknowledged is True, 'proof-unacknowledged')
        state, original, observed = state_from(receipt.raw, self.expected_pin)
        need(not state['denied'], 'terminal-denial')
        return receipt.raw, state, original, observed

    def _write(self, raw, state):
        integer(state['revision'], 0, 2**63-2)
        state['revision'] += 1
        replacement = encode(state)
        receipt = self.store.compare_exchange(raw, replacement)
        need(type(receipt) is Receipt and receipt.acknowledged is True and receipt.raw == replacement,
             'checkpoint-uncertain')

    def _sample(self, state, original, observed, now):
        now.validate()
        need(now.domain == original.start.domain, 'clock-domain')
        current = (now.monotonic_ns, now.boottime_ns, now.wall_ns)
        previous = (observed.monotonic_ns, observed.boottime_ns, observed.wall_ns)
        need(all(a >= b for a, b in zip(current, previous)), 'clock-regression')
        remaining = min(end-tick for end, tick in zip(
            (original.monotonic_end_ns, original.boottime_end_ns, original.wall_end_ns), current))
        need(remaining > 0, 'original-deadline')
        state['observed'] = asdict(now)
        return remaining

    def reserve(self, **charges):
        """Conservative, no-refund debit before I/O; failed/EOF work keeps its debt."""
        raw = state = None
        try:
            before = self.clock.sample()
            before.validate()
            raw, state, original, observed = self._read()
            self._sample(state, original, observed, before)
            self._sample(state, original, before, self.clock.sample())
            need(set(charges).issubset(CAPS) and 'raw_reservations' not in charges, 'credit-name')
            if 'raw_bytes' in charges:
                integer(charges['raw_bytes'], 1, 16)
                charges['raw_reservations'] = 1
            for name, count in charges.items():
                integer(count, 0, CAPS[name])
                state['credits'][name] += count
                integer(state['credits'][name], 0, CAPS[name])
            self._write(raw, state)
            # Storage itself can finish late. No caller may infer permission
            # from a pre-CAS time sample alone.
            after = self.clock.sample()
            remaining = self._sample(state, original, sample_from(state['observed']), after)
            return remaining
        except Exception:
            if state is not None and raw is not None:
                try:
                    receipt = self.store.load()
                    latest, _, _ = state_from(receipt.raw, self.expected_pin)
                    if receipt.acknowledged is True:
                        latest['denied'] = True
                        self._write(receipt.raw, latest)
                except Exception:
                    pass  # Integration must reconcile an uncertain CAS; never create a new proof.
            self.poisoned = True
            raise

    def check(self):
        return self.reserve()

    def deny(self):
        """Retain a terminal error if acknowledgment remains available."""
        try:
            raw, state, _, _ = self._read()
            state['denied'] = True
            self._write(raw, state)
        finally:
            self.poisoned = True

    def command_timeout_ns(self, command_cap_ns):
        integer(command_cap_ns, 1)
        return min(command_cap_ns, self.check())

    def credits(self):
        self.check()
        return dict(self._read()[1]['credits'])
