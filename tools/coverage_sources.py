#!/usr/bin/env python3
"""Bounded immutable Git source/byte-LF delta proof, never a coverage score.

No diff display, working-tree filter, compiler, runner or Docker interface lives
here. Evidence assumes a trusted coordinator, not hostile local administration.
"""
from __future__ import annotations

from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import time


class Refused(ValueError):
    pass


def need(condition, reason):
    if not condition:
        raise Refused(reason)


@dataclass(frozen=True)
class Limits:
    paths: int = 10000
    blob_bytes: int = 64 * 1024**2
    total_bytes: int = 256 * 1024**2
    lines: int = 2000000
    operations: int = 8000000
    frontier_cells: int = 250000
    proof_bytes: int = 4 * 1024**2
    metadata_bytes: int = 8 * 1024**2
    commits: int = 10000
    seconds: int = 60

    def validate(self):
        defaults = Limits()
        for name, value in asdict(self).items():
            need(type(value) is int and 0 < value <= getattr(defaults, name), 'invalid_limit:' + name)


class Budget:
    def __init__(self, limits=Limits()):
        limits.validate()
        self.limits = limits
        self.deadline = time.monotonic() + limits.seconds
        self.operations = self.bytes = self.lines = self.cells = 0
        self.object_bytes = self.metadata_bytes = 0

    def remaining(self):
        left = self.deadline - time.monotonic()
        need(left > 0, 'proof_deadline')
        return left

    def step(self, count=1):
        self.operations += count
        need(self.operations <= self.limits.operations, 'delta_work_limit')
        self.remaining()

    def charge(self, raw):
        need(len(raw) <= self.limits.blob_bytes, 'blob_byte_limit')
        count = len(raw)
        lines = raw.count(b'\n') + int(bool(raw) and not raw.endswith(b'\n'))
        self.bytes += count
        self.lines += lines
        need(self.bytes <= self.limits.total_bytes, 'total_byte_limit')
        need(self.lines <= self.limits.lines, 'line_inventory_limit')
        self.remaining()

    def cell(self):
        # Charge before allocating each frontier entry, including the working
        # frontier not yet retained in trace. Cumulative across the proof.
        self.cells += 1
        need(self.cells <= self.limits.frontier_cells, 'delta_frontier_limit')
        self.remaining()


def physical_lines(raw):
    """Keep LF in terminated lines, making final-LF and CR changes explicit."""
    need(type(raw) is bytes, 'blob_type')
    pieces = raw.split(b'\n')
    return [piece + b'\n' for piece in pieces[:-1]] + ([pieces[-1]] if pieces[-1] else [])


def added_lines(before, after, budget=None):
    """Exact minimal insertion indices, deterministic bounded Myers v1.

    Forward predecessor ties prefer deletion. Equal prefixes are removed first.
    Work and ALL retained frontier cells share the whole-proof budget.
    """
    budget = budget or Budget()
    budget.charge(before); budget.charge(after)
    left, right = physical_lines(before), physical_lines(after)
    prefix = 0
    while prefix < min(len(left), len(right)):
        budget.step()
        if left[prefix] != right[prefix]: break
        prefix += 1
    left, right = left[prefix:], right[prefix:]
    n, m = len(left), len(right)
    if not n:
        budget.step(m)
        return list(range(prefix + 1, prefix + m + 1))
    if not m: return []
    budget.cell()
    frontier, trace = {1: 0}, []
    for distance in range(n + m + 1):
        trace.append(frontier)
        next_frontier = {}
        for diagonal in range(-distance, distance + 1, 2):
            budget.step()
            insertion = diagonal == -distance or (diagonal != distance and frontier[diagonal - 1] < frontier[diagonal + 1])
            x = frontier[diagonal + 1] if insertion else frontier[diagonal - 1] + 1
            y = x - diagonal
            while x < n and y < m:
                budget.step()
                if left[x] != right[y]: break
                x += 1; y += 1
            budget.cell()
            next_frontier[diagonal] = x
            if x >= n and y >= m:
                added = []
                for depth in range(distance, -1, -1):
                    budget.step()
                    previous = trace[depth]
                    diagonal = x - y
                    insertion = diagonal == -depth or (diagonal != depth and previous[diagonal - 1] < previous[diagonal + 1])
                    old_diagonal = diagonal + 1 if insertion else diagonal - 1
                    old_x = previous[old_diagonal]
                    old_y = old_x - old_diagonal
                    while x > old_x and y > old_y:
                        budget.step(); x -= 1; y -= 1
                    if depth and insertion: added.append(prefix + y)
                    x, y = old_x, old_y
                budget.remaining()
                return sorted(added)
        frontier = next_frontier
    raise Refused('delta_unreachable')


OID = re.compile(r'[0-9a-f]{40}\Z')
SHA256 = re.compile(r'[0-9a-f]{64}\Z')
REDIRECTS = {'GIT_DIR', 'GIT_WORK_TREE', 'GIT_COMMON_DIR', 'GIT_OBJECT_DIRECTORY',
             'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_INDEX_FILE', 'GIT_GRAFT_FILE',
             'GIT_SHALLOW_FILE', 'GIT_NAMESPACE', 'GIT_CONFIG', 'GIT_CONFIG_PARAMETERS', 'GIT_CONFIG_COUNT'}


def oid(value):
    need(type(value) is str and OID.fullmatch(value), 'invalid_object_id')
    return value


def source_path(value):
    need(type(value) is str and 0 < len(value) <= 4096 and '\\' not in value and ':' not in value,
         'unsupported_source_path')
    try: encoded = value.encode('utf-8')
    except UnicodeError: raise Refused('unsupported_path_encoding') from None
    need(len(encoded) <= 4096 and all(ord(c) >= 32 and ord(c) != 127 for c in value) and
         all(part.lower() not in {'', '.', '..', '.git'} for part in value.split('/')), 'unsupported_source_path')
    return value


def canonical(value, maximum=Limits().proof_bytes):
    # Do not first allocate an oversized JSON string and only then check it.
    encoder = json.JSONEncoder(sort_keys=True, ensure_ascii=True, separators=(',', ':'), allow_nan=False)
    encoded = bytearray()
    for part in encoder.iterencode(value):
        block = part.encode('ascii')
        need(len(encoded) + len(block) <= maximum, 'proof_output_limit')
        encoded.extend(block)
    return bytes(encoded)


def object_hash(kind, raw):
    return hashlib.sha1(kind.encode('ascii') + b' ' + str(len(raw)).encode('ascii') + b'\0' + raw).hexdigest()


class GitObjects:
    def __init__(self, root, budget):
        self.root = Path(root).absolute()
        need(self.root.resolve() == self.root and self.root.is_dir(), 'redirected_repository')
        need(not any(name in os.environ for name in REDIRECTS), 'redirecting_git_environment')
        self.budget = budget
        # Inherited display/context/config injection has no authority here.
        self.env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
        self.env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull, GIT_TERMINAL_PROMPT='0', GIT_PAGER='cat')
        top = Path(os.fsdecode(self.command('rev-parse', '--show-toplevel').strip()))
        need(top == self.root, 'repository_root_mismatch')
        self.common = Path(os.fsdecode(self.command('rev-parse', '--path-format=absolute', '--git-common-dir').strip()))
        need(self.common.resolve() == self.common and self.common.is_dir(), 'redirected_git_directory')
        for relative in ['info/grafts', 'shallow', 'objects/info/alternates', 'objects/info/http-alternates']:
            need(not (self.common / relative).exists(), 'unsupported_git_view:' + relative)
        need(self.command('rev-parse', '--show-object-format') == b'sha1\n', 'unsupported_object_format')
        need(not self.command('for-each-ref', '--format=%(refname)', 'refs/replace/'), 'replacement_refs')
        self.identity = hashlib.sha256(os.fsencode(str(self.root)) + b'\0' + os.fsencode(str(self.common))).hexdigest()

    def command(self, *arguments, limit=None):
        """Drain bounded pipes and reap only this non-shell Git child on failure."""
        limit = limit or self.budget.limits.metadata_bytes
        need(sum(len(str(a).encode('utf-8')) for a in arguments) <= 16384, 'git_argument_limit')
        remaining = self.budget.remaining()
        process = subprocess.Popen(['git', '--no-pager', '--no-replace-objects', '-C', str(self.root), *arguments],
                                   env=self.env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        chunks, errors, exceeded = [], [], threading.Event()
        def drain(pipe, target, bound):
            size = 0
            try:
                while block := pipe.read(65536):
                    size += len(block)
                    if size <= bound: target.append(block)
                    else:
                        exceeded.set()
                        break
            except OSError:
                exceeded.set()
            finally: pipe.close()
        workers = [threading.Thread(target=drain, args=(process.stdout, chunks, limit)),
                   threading.Thread(target=drain, args=(process.stderr, errors, 65536))]
        for worker in workers: worker.start()
        failure = None
        try:
            while process.poll() is None:
                if exceeded.wait(min(0.02, self.budget.remaining())):
                    raise Refused('git_output_limit')
            self.budget.remaining()
        except BaseException as error:
            failure = error
            if process.poll() is None: process.kill()
        finally:
            process.wait(timeout=max(1, min(5, remaining)))
            for worker in workers: worker.join(timeout=5)
        if failure: raise failure
        need(not any(worker.is_alive() for worker in workers), 'git_pipe_not_reaped')
        need(not exceeded.is_set(), 'git_output_limit')
        need(process.returncode == 0, 'git_object_command_failed:' + arguments[0])
        return b''.join(chunks)

    def raw(self, kind, identity, maximum):
        oid(identity)
        size_record = self.command('cat-file', '-s', identity, limit=32)
        need(re.fullmatch(rb'[0-9]+\n', size_record), 'invalid_object_size')
        size = int(size_record)
        need(size <= maximum, 'blob_byte_limit' if kind == 'blob' else 'object_metadata_limit')
        if kind == 'blob':
            self.budget.object_bytes += size
            need(self.budget.object_bytes <= self.budget.limits.total_bytes, 'total_byte_limit')
        else:
            self.budget.metadata_bytes += size
            need(self.budget.metadata_bytes <= self.budget.limits.metadata_bytes, 'object_metadata_limit')
        raw = self.command('cat-file', kind, identity, limit=maximum)
        need(len(raw) == size, 'object_size_mismatch')
        need(object_hash(kind, raw) == identity, 'object_content_hash_mismatch')
        return raw

    def commit(self, identity):
        raw = self.raw('commit', identity, min(self.budget.limits.metadata_bytes, 1024**2))
        header, separator, _message = raw.partition(b'\n\n')
        need(separator, 'invalid_commit_header')
        records = header.split(b'\n')
        need(records[0].startswith(b'tree '), 'invalid_commit_tree')
        try:
            tree = oid(records[0][5:].decode('ascii'))
            # Native Git recognizes parent records only directly after tree.
            # Later metadata may legally spell "parent" but is not ancestry.
            # Refuse that ambiguous supported-format extension explicitly.
            parents, index = [], 1
            while index < len(records) and records[index].startswith(b'parent '):
                parents.append(oid(records[index][7:].decode('ascii')))
                index += 1
            need(not any(line.startswith((b'parent ', b'tree ')) for line in records[index:]),
                 'unsupported_commit_header_order')
        except UnicodeError: raise Refused('invalid_commit_identity') from None
        need(len(parents) <= 64, 'commit_parent_limit')
        return tree, parents

    def qualify_base(self, head, base):
        tree, parents = self.commit(head)
        if base is None:
            need(not parents, 'empty_base_requires_parentless_root')
            return tree, None
        oid(base)
        pending, seen = [head], set()
        while pending:
            current = pending.pop()
            if current in seen: continue
            need(len(seen) < self.budget.limits.commits, 'ancestry_commit_limit')
            seen.add(current)
            candidate_tree, ancestors = self.commit(current)
            if current == base: return tree, candidate_tree
            pending.extend(parent for parent in ancestors if parent not in seen)
        raise Refused('base_is_not_ancestor')

    def tree(self, identity):
        if identity is None: return {}
        # Verify each raw tree object hash, not just rendered ls-tree output.
        pending, result, nodes, paths = [('', identity)], {}, 0, set()
        while pending:
            prefix, identity = pending.pop()
            nodes += 1
            need(nodes <= self.budget.limits.paths, 'tree_node_limit')
            raw = self.raw('tree', identity, self.budget.limits.metadata_bytes)
            at = 0
            while at < len(raw):
                self.budget.step()
                end = raw.find(b'\0', at)
                need(end >= 0 and end + 21 <= len(raw), 'truncated_tree_record')
                mode, space, name = raw[at:end].partition(b' ')
                need(space and mode in {b'40000', b'100644', b'100755', b'120000', b'160000'}, 'unsupported_tree_mode')
                try: name = name.decode('utf-8')
                except UnicodeError: raise Refused('unsupported_path_encoding') from None
                name = source_path(name)
                need('/' not in name, 'invalid_tree_name')
                path = source_path(prefix + name)
                child = raw[end + 1:end + 21].hex()
                at = end + 21
                need(path not in paths and len(paths) < self.budget.limits.paths, 'duplicate_or_excess_tree_path')
                paths.add(path)
                if mode == b'40000': pending.append((path + '/', child))
                else:
                    need(len(result) < self.budget.limits.paths, 'tracked_path_limit')
                    result[path] = (mode.decode('ascii'), child)
        return result


def manifest_record(raw, owner):
    need(type(raw) is bytes and type(owner) is str and 0 < len(owner) <= 128, 'invalid_source_manifest')
    return {'owner': owner, 'sha256': hashlib.sha256(raw).hexdigest(),
            'line_count': raw.count(b'\n') + int(bool(raw) and not raw.endswith(b'\n'))}


def prove(root, *, head, base, sources, deleted=(), limits=Limits()):
    """Bind immutable blobs to actual frozen source bytes; refuse partial proofs.

    sources maps all selected production paths to owner/sha256/line_count.
    Deleted paths are explicit requested old sources with no executable head.
    Caller owns production inventory completeness and source tree freezing.
    """
    budget = Budget(limits)
    oid(head)
    need(type(sources) is dict and type(deleted) in {tuple, list} and
         len(sources) + len(deleted) <= limits.paths, 'source_inventory_limit')
    need(len(set(deleted)) == len(deleted) and not set(sources).intersection(deleted), 'duplicate_source_inventory')
    for path, record in sources.items():
        source_path(path)
        need(type(record) is dict and set(record) == {'owner', 'sha256', 'line_count'} and
             type(record['owner']) is str and 0 < len(record['owner']) <= 128 and
             type(record['sha256']) is str and SHA256.fullmatch(record['sha256']) and
             type(record['line_count']) is int and 0 <= record['line_count'] <= limits.lines, 'invalid_source_manifest')
    for path in deleted: source_path(path)
    git = GitObjects(root, budget)
    need(git.command('rev-parse', '--verify', 'HEAD').strip().decode('ascii') == head, 'actual_head_mismatch')
    head_tree, base_tree = git.qualify_base(head, base)
    before_tree, after_tree = git.tree(base_tree), git.tree(head_tree)
    files = {}
    for path in sorted([*sources, *deleted]):
        budget.remaining()
        old, new = before_tree.get(path), after_tree.get(path)
        need(old is not None or new is not None, 'missing_source_path')
        need(all(entry is None or entry[0] in {'100644', '100755'} for entry in [old, new]), 'nonregular_source')
        need((new is None) == (path in deleted), 'head_source_inventory_mismatch')
        old_raw = b'' if old is None else git.raw('blob', old[1], limits.blob_bytes)
        new_raw = b'' if new is None else git.raw('blob', new[1], limits.blob_bytes)
        added = added_lines(old_raw, new_raw, budget)
        if new is not None:
            actual = manifest_record(new_raw, sources[path]['owner'])
            need(actual == sources[path], 'frozen_source_manifest_mismatch')
            current = (git.root / path)
            need(current.resolve() == current and current.is_file(), 'redirected_frozen_source')
            with current.open('rb') as source: raw = source.read(limits.blob_bytes + 1)
            need(raw == new_raw, 'frozen_source_bytes_mismatch')
        else: actual = None
        files[path] = {'base_blob': old[1] if old else None, 'head_blob': new[1] if new else None,
                       'base_mode': old[0] if old else None, 'head_mode': new[0] if new else None,
                       'source': actual, 'added_head_lines': added}
    # A second full source bookend catches mutation during the proof, not only
    # mutation before each individual file. This is not a filesystem lock.
    for path in sources:
        current = git.root / path
        need(current.resolve() == current and current.is_file(), 'redirected_frozen_source')
        with current.open('rb') as source: raw = source.read(limits.blob_bytes + 1)
        need(manifest_record(raw, sources[path]['owner']) == sources[path], 'frozen_source_changed')
    need(git.command('rev-parse', '--verify', 'HEAD').strip().decode('ascii') == head, 'actual_head_changed')
    result = {'schema_version': 1, 'algorithm': 'byte-lf-myers-delete-tie-prefix-v1',
              'repository_identity': git.identity, 'head': head, 'base': base,
              'head_tree': head_tree, 'base_tree': base_tree, 'limits': asdict(limits),
              'files': files}
    canonical(result, limits.proof_bytes)
    budget.remaining()
    return result


def verify(proof, root, *, head, base, sources, deleted=(), limits=Limits()):
    """Recompute immutable inputs; stale/extra/missing/corrupt proof refuses."""
    need(type(proof) is dict, 'proof_type')
    encoded = canonical(proof, limits.proof_bytes)
    expected = prove(root, head=head, base=base, sources=sources, deleted=deleted, limits=limits)
    need(encoded == canonical(expected, limits.proof_bytes), 'source_proof_mismatch')
    return True
