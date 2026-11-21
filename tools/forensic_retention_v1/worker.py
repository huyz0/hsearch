"""Fixed read-only integrity worker; no extraction, semantic imports or writes."""
import os
from pathlib import Path
import sys

from . import protocol as F
from ._bytes import need, regular
from .storage import Archive, decode, audit


def load_inputs():
    result={}
    for kind in ['manifest','authorization','admission']:
        row=decode(regular(Path('/inputs')/(kind+'.json'),1048576))
        F.keys(row,'sequence previous kind value')
        need(row['kind']==kind,'worker-input-kind')
        result[kind]=row['value']
    return result


def probe():
    root = Path('/sys/fs/cgroup')
    def number(name):
        raw = (root/name).read_bytes()
        need(len(raw) <= 128, 'probe-cgroup-bound')
        return raw.decode().strip()
    cpu = number('cpu.max').split()
    need(len(cpu) == 2 and cpu[0] != 'max', 'probe-cpu')
    scratch = os.statvfs('/scratch')
    result = dict(kind='forensic-cgroup-probe-v1', cpu_millis=int(cpu[0])*1000//int(cpu[1]),
                  memory_bytes=int(number('memory.max')), swap_bytes=int(number('memory.swap.max')),
                  pids=int(number('pids.max')), scratch_bytes=scratch.f_blocks*scratch.f_frsize, uid=os.getuid())
    return result


def main(argv=None):
    args = sys.argv[1:] if argv is None else argv
    try:
        need(len(args) == 2 and args[0] in {'probe', 'audit'}, 'worker-arguments')
        F.sha(args[1])
        if args[0] == 'probe':
            result = probe()
        else:
            inputs=load_inputs()
            manifest,admission,authorization=(inputs[k] for k in ['manifest','admission','authorization'])
            need(admission == F.admission(manifest, authorization, started_ms=admission['started_ms'],
                                         deadline_ms=admission['deadline_ms']), 'worker-admission')
            meter = F.Meter(admission)
            archive = Archive('/retained',host_private=False)
            result = audit(archive, manifest, args[1], meter)
        raw = F.enc(result)
        need(len(raw) <= 8192, 'worker-output')
        sys.stdout.buffer.write(raw+b'\n')
        return 0
    except Exception:
        sys.stderr.write('forensic-worker-refused\n')
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
