"""Owned unbuffered combined-pipe capture with crash-conservative accounting."""
import subprocess
import threading
import time
from ._bytes import Refused, need


class RawBudget:
    def __init__(self,load,save,check=lambda:None):
        self.load,self.save,self.check=load,save,check
        self.lock=threading.Lock()

    @property
    def used(self):
        value=self.load()
        need(type(value)is int and 0<=value<=8192,'raw-state')
        return value

    def reserve(self):
        with self.lock:
            self.check()
            value=self.used
            need(value<8192,'aggregate-raw-exhausted')
            count=min(16,8192-value)
            self.save(value,value+count)  # Durable CAS before any pipe byte is consumed.
            self.check()
            return count


def bounded_capture(command,*,timeout,budget):
    budget.check()
    child=subprocess.Popen(command,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE,bufsize=0)
    output={};errors=[];lock=threading.Lock()
    def read(stream,key):
        chunks=[]
        try:
            while True:
                count=budget.reserve()
                raw=stream.read(count)
                if not raw:break
                need(len(raw)<=count,'raw-stream-contract')
                chunks.append(raw)
        except BaseException as error:
            with lock:errors.append(error)
            if child.poll()is None:child.kill()
        finally:
            stream.close();output[key]=b''.join(chunks)
    threads=[threading.Thread(target=read,args=(stream,key),daemon=True)
             for stream,key in [(child.stdout,'out'),(child.stderr,'err')]]
    for thread in threads:thread.start()
    primary=None
    try:child.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        primary=Refused('transport-deadline-unknown')
        child.kill();child.wait(timeout=3)
    finally:
        for thread in threads:thread.join(timeout=3)
    need(not any(t.is_alive() for t in threads),'transport-pipe-unknown')
    if primary:raise primary
    if errors:raise Refused('transport-capture-refused') from errors[0]
    budget.check()
    need(child.returncode==0,'transport-command-failed')
    return output['out'].decode('utf-8',errors='strict')
