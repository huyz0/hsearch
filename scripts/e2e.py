#!/usr/bin/env python3
"""End to end run of the first slice over plain HTTP (urllib, no SDK).

Starts `hsearch-node` as a subprocess against the S3-compatible store named by the
HSEARCH_S3_* environment, then: bulk-indexes N documents and measures latency; kills
the node with SIGKILL in the middle of a bulk; restarts it on an empty data directory;
checks that every acknowledged document survived (the one property the script can
observe from outside; a bulk of more than 1000 operations is several log slots, so an
unacknowledged bulk may be partly durable, which is allowed and reported); restarts once
more and compares a fixed query set. The kill happens against a healthy store, so this is a
node-crash test, not a store fault test; the durability boundaries inside the write path
are covered by the crash matrix in crates/index/tests/rustfs.rs. Prints a report; exits non-zero on any failure.

    scripts/e2e.sh            # builds, starts RustFS if needed, runs this
    N=100000 BATCH=5000 python3 scripts/e2e.py path/to/hsearch-node
"""
import atexit, json, os, platform, shutil, signal, statistics, subprocess, sys, tempfile
import threading, time, urllib.error, urllib.request

N = int(os.environ.get("N", "100000"))
BATCH = int(os.environ.get("BATCH", "5000"))
MAPPING = {"mappings": {"properties": {"title": {"type": "text"}, "tag": {"type": "keyword"},
                                      "n": {"type": "integer"}, "ok": {"type": "boolean"},
                                      "at": {"type": "date"}}}}


def doc_lines(lo, hi):
    out = []
    for i in range(lo, hi):
        out.append(json.dumps({"index": {"_id": f"d{i}"}}))
        out.append(json.dumps({"title": f"quick fox item{i}", "tag": f"t{i % 3}", "n": i,
                               "ok": i % 2 == 0, "at": 1_700_000_000_000 + i}))
    return "\n".join(out) + "\n"


LIVE = []


@atexit.register
def _reap():
    for node in LIVE:
        if node.proc.poll() is None:
            node.proc.kill()


class Node:
    def __init__(self, binary, bucket, data):
        env = dict(os.environ, HSEARCH_S3_BUCKET=bucket)
        self.proc = subprocess.Popen([binary, "--listen", "127.0.0.1:0", "--data-dir", data],
                                     env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        line = self.proc.stdout.readline()
        if "listening on http://" not in line:
            self.proc.kill()
            raise SystemExit(f"node did not start: {line}{self.proc.stdout.read()}")
        self.base = line.strip().split("listening on ")[1]
        # Keep draining the pipe so a node that logs later can never block on it.
        threading.Thread(target=lambda: [None for _ in self.proc.stdout], daemon=True).start()
        LIVE.append(self)

    def call(self, method, path, body=None, timeout=300):
        if body is None:
            data = None
        else:
            data = (body if isinstance(body, str) else json.dumps(body)).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, json.loads(r.read() or b"null")
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read() or b"null")

    def kill9(self):
        self.proc.send_signal(signal.SIGKILL)
        self.proc.wait()

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


def total(node, query):
    s, out = node.call("POST", "/orders/_search", {"query": query, "size": 0})
    assert s == 200, out
    return out["hits"]["total"]["value"]


def answers(node):
    s, page = node.call("POST", "/orders/_search", {"query": {"term": {"tag": "t2"}},
                                                     "sort": [{"n": "desc"}], "size": 5, "_source": ["n"]})
    assert s == 200, page
    return [total(node, {"match_all": {}}), total(node, {"term": {"tag": "t0"}}),
            total(node, {"range": {"n": {"gte": 1000, "lt": 51000}}}),
            total(node, {"bool": {"filter": [{"term": {"ok": True}}, {"term": {"tag": "t1"}}]}}),
            total(node, {"match": {"title": "item99999"}}), page["hits"]["hits"]]


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p))]


def main():
    binary = sys.argv[1]
    for k in ("HSEARCH_S3_ENDPOINT", "HSEARCH_S3_ACCESS_KEY", "HSEARCH_S3_SECRET_KEY"):
        if k not in os.environ:
            raise SystemExit(f"{k} is not set (see scripts/rustfs.sh)")
    bucket = f"e2e-{int(time.time())}-{os.getpid()}"
    work = tempfile.mkdtemp(prefix="hsearch-e2e-")
    report = [f"hardware: {platform.machine()}, {os.cpu_count()} CPUs, {platform.platform()}",
              f"binary: {binary}", f"documents: {N}, bulk size {BATCH}, bucket {bucket}"]
    ok = True

    def check(cond, what):
        nonlocal ok
        report.append(("PASS " if cond else "FAIL ") + what)
        ok &= bool(cond)

    # 1. bulk load and query latency.
    node = Node(binary, bucket, f"{work}/a")
    assert node.call("PUT", "/orders", MAPPING)[0] == 200
    lat = []
    for lo in range(0, N, BATCH):
        t = time.time()
        s, out = node.call("POST", "/orders/_bulk?refresh=true", doc_lines(lo, min(N, lo + BATCH)))
        lat.append((time.time() - t) * 1000)
        if (s, out.get("errors")) != (200, False):
            check(False, f"bulk at {lo} failed: {s} {str(out)[:200]}")
    report.append(f"bulk latency ms (n={len(lat)}, {BATCH} docs each): p50 {statistics.median(lat):.0f}, "
                  f"p95 {pct(lat, .95):.0f}, max {max(lat):.0f}; total {sum(lat) / 1000:.1f}s")
    queries = {"match_all": {"match_all": {}}, "term": {"term": {"tag": "t1"}},
               "range": {"range": {"n": {"gte": 1000, "lt": 51000}}}, "match": {"match": {"title": "fox"}},
               "bool": {"bool": {"filter": [{"term": {"ok": True}}, {"term": {"tag": "t1"}}]}}}
    for name, q in queries.items():
        times = []
        for _ in range(30):
            t = time.time()
            s, _ = node.call("POST", "/orders/_search", {"query": q, "size": 10})
            times.append((time.time() - t) * 1000)
            assert s == 200
        report.append(f"search {name} ms (n=30): p50 {statistics.median(times):.1f}, p95 {pct(times, .95):.1f}")
    live = answers(node)
    check(live[0] == N and live[1] == -(-N // 3), f"live totals: match_all {live[0]}, tag t0 {live[1]}")
    node.stop()

    # 2. kill -9 in the middle of a bulk, restart on an empty data directory.
    node = Node(binary, bucket, f"{work}/b")
    t = time.time()
    check(total(node, {"match_all": {}}) == N, "restart on an empty data dir serves every document")
    report.append(f"restart plus first answer (full log replay): {time.time() - t:.1f}s")
    base, inflight = N, 4 * BATCH
    acked = []

    def writer():
        for lo in range(base, base + inflight, BATCH):
            try:
                s, out = node.call("POST", "/orders/_bulk", doc_lines(lo, lo + BATCH), timeout=60)
                if s == 200 and out["errors"] is False:
                    acked.append(lo)
            except Exception:
                return

    th = threading.Thread(target=writer)
    th.start()
    deadline = time.time() + 120
    while not acked and th.is_alive() and time.time() < deadline:
        time.sleep(0.005)
    if not acked:
        node.kill9()
        th.join()
        report.append("FAIL no bulk was acknowledged before the kill")
        print("\n".join(report))
        sys.exit(1)
    # Let the next bulk get under way (a bulk takes about `typical` ms), then kill mid-bulk.
    time.sleep(0.4 * statistics.median(lat) / 1000)
    node.kill9()
    th.join()
    report.append(f"kill -9 after {len(acked)} acknowledged bulk(s) of {inflight // BATCH} sent")
    shutil.rmtree(f"{work}/b")
    node = Node(binary, bucket, f"{work}/c")
    after = total(node, {"match_all": {}})
    lo_bound, hi_bound = N + len(acked) * BATCH, N + inflight
    check(lo_bound <= after <= hi_bound, f"after kill: {after} documents, between {lo_bound} acknowledged and {hi_bound} sent")
    # Every acknowledged document is present (counted by the unique `n` of each document).
    present = sum(total(node, {"range": {"n": {"gte": lo, "lt": lo + BATCH}}}) for lo in acked)
    check(present == len(acked) * BATCH, f"all {present} acknowledged documents present")
    # A bulk is split into slots of at most 1000 operations; the in-flight one may be partly durable.
    # It was never acknowledged, so that is allowed; only an acknowledged bulk must be whole.
    report.append(f"unacknowledged documents that became durable: {after - lo_bound} "
                  f"(allowed: they were never acknowledged)")
    s, out = node.call("POST", "/orders/_bulk?refresh=true", doc_lines(10**6, 10**6 + 10))
    check(s == 200 and out["errors"] is False, "the restarted node accepts new writes")
    after2 = answers(node)
    node.stop()

    # 3. restart once more, nothing killed, same answers.
    shutil.rmtree(f"{work}/c")
    node = Node(binary, bucket, f"{work}/d")
    check(answers(node) == after2, "a second restart answers the fixed query set identically")
    node.stop()
    shutil.rmtree(work, ignore_errors=True)
    print("\n".join(report))
    sys.exit(0 if ok else 1)


main()
