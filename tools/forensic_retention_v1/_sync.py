"""Incoming strict directory durability primitive. Failure remains failure."""
from pathlib import Path
import os
from ._bytes import need

def sync_directory(path):
    path=Path(path).absolute();need(path.resolve()==path and path.is_dir(),'stream42-sync-directory')
    if os.name=='nt':
        # Directory handles need WRITE access for FlushFileBuffers. Never open
        # a volume or request elevated privileges; only this exact directory.
        import ctypes
        from ctypes import wintypes as W
        kernel=ctypes.WinDLL('kernel32',use_last_error=True)
        kernel.CreateFileW.argtypes=[W.LPCWSTR,W.DWORD,W.DWORD,W.LPVOID,W.DWORD,W.DWORD,W.HANDLE];kernel.CreateFileW.restype=W.HANDLE
        kernel.FlushFileBuffers.argtypes=[W.HANDLE];kernel.FlushFileBuffers.restype=W.BOOL
        kernel.CloseHandle.argtypes=[W.HANDLE];kernel.CloseHandle.restype=W.BOOL
        handle=kernel.CreateFileW(str(path),0x40000000,7,None,3,0x02000000,None)
        if handle==ctypes.c_void_p(-1).value:raise ctypes.WinError(ctypes.get_last_error())
        primary=False
        try:
            if not kernel.FlushFileBuffers(handle):raise ctypes.WinError(ctypes.get_last_error())
        except BaseException:primary=True;raise
        finally:
            closed=kernel.CloseHandle(handle)
            if not closed and not primary:raise ctypes.WinError(ctypes.get_last_error())
    else:
        fd=os.open(path,os.O_RDONLY|getattr(os,'O_DIRECTORY',0))
        try:os.fsync(fd)
        finally:os.close(fd)
