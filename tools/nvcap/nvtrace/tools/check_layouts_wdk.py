#!/usr/bin/env python3
"""Check tools/nvt_layouts.py against the Windows Driver Kit headers.

    tools/check_layouts_wdk.py [--wdk DIR] [--keep FILE]

Generates a C file with one _Static_assert per struct field (offset) and per
struct (size), and compiles it with x86_64-w64-mingw32-gcc and
i686-w64-mingw32-gcc against the WDK's d3dkmthk.h. MinGW lays out these
structs like MSVC (natural alignment, ms bitfields), so a clean compile on
both means the x64 and x86 (WOW64) layouts match the headers.

The WDK directory needs d3dkmthk.h, d3dukmdt.h, d3dkmdt.h (WDK_INC or
--wdk). The headers are not part of this repository. Exit status 0 = all
checked layouts match, 1 = mismatch, 77 = headers or compilers missing.
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import nvt_layouts as L  # noqa: E402

DEFAULT_WDK = os.path.expanduser("~/.local/share/conduit-dev/wdk-10.0.26100.0")

# nvt struct name -> C type (None: not a C type of its own, skipped)
CNAME = {
    "D3DKMT_CREATEALLOCATION(2)": None,     # same C struct as D3DKMT_CREATEALLOCATION
    "D3DKMT_OPENRESOURCE(2)": None,
    "map": None, "copy": None, "monitored": None, "PTR": None,
    "PLANE3PTR": None, "CHECKPLANE3PTR": None, "POSTCOMPPTR": None,
    "RECT": None,
}

# (nvt struct, field) -> C member designator, or None to skip the field
CPATH = {
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "BaseAddress"): "Map.BaseAddress",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "Size"): "Map.SizeInBytes",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "hAllocation"): "Map.hAllocation",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "AllocationOffset"): "Map.AllocationOffsetInBytes",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "AllocationSize"): "Map.AllocationSizeInBytes",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "Protection"): "MapProtect.Protection",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "DriverProtection"): "MapProtect.DriverProtection",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "SourceAddress"): "Copy.SourceAddress",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "CopySize"): "Copy.SizeInBytes",
    ("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", "DestAddress"): "Copy.DestAddress",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO", "InitialState"): "SynchronizationMutex.InitialState",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO", "MaxCount"): "Semaphore.MaxCount",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO", "Reserved"): "Reserved.Reserved",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO2", "InitialFenceValue"): "MonitoredFence.InitialFenceValue",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO2", "FenceValueCPUVirtualAddress"):
        "MonitoredFence.FenceValueCPUVirtualAddress",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO2", "FenceValueGPUVirtualAddress"):
        "MonitoredFence.FenceValueGPUVirtualAddress",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO2", "EngineAffinity"): "MonitoredFence.EngineAffinity",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO2", "Reserved"): "Reserved.Reserved",
    ("D3DDDI_SYNCHRONIZATIONOBJECTINFO2", "SharedHandle"): "SharedHandle",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "Type"): "Info.Type",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "Flags"): "Info.Flags",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "InitialFenceValue"): "Info.MonitoredFence.InitialFenceValue",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "FenceValueCPUVirtualAddress"):
        "Info.MonitoredFence.FenceValueCPUVirtualAddress",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "FenceValueGPUVirtualAddress"):
        "Info.MonitoredFence.FenceValueGPUVirtualAddress",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "EngineAffinity"): "Info.MonitoredFence.EngineAffinity",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "Reserved"): "Info.Reserved.Reserved",
    ("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", "SharedHandle"): "Info.SharedHandle",
    ("D3DKMT_WAITFORSYNCHRONIZATIONOBJECT2", "FenceValue"): "Fence.FenceValue",
    ("D3DKMT_WAITFORSYNCHRONIZATIONOBJECT2", "Reserved"): "Reserved",
    ("D3DKMT_SIGNALSYNCHRONIZATIONOBJECT2", "FenceValue"): "Fence.FenceValue",
    ("D3DKMT_SIGNALSYNCHRONIZATIONOBJECT2", "CpuEventHandle"): "CpuEventHandle",
    ("D3DKMT_SIGNALSYNCHRONIZATIONOBJECT2", "Reserved"): "Reserved",
    ("D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMGPU2", "FenceValue"): "FenceValue",
    ("D3DKMT_OPENSYNCOBJECTFROMNTHANDLE2", "FenceValueCPUVirtualAddress"):
        "MonitoredFence.FenceValueCPUVirtualAddress",
    ("D3DKMT_OPENSYNCOBJECTFROMNTHANDLE2", "FenceValueGPUVirtualAddress"):
        "MonitoredFence.FenceValueGPUVirtualAddress",
    ("D3DKMT_OPENSYNCOBJECTFROMNTHANDLE2", "EngineAffinity"): "MonitoredFence.EngineAffinity",
    ("D3DKMT_OPENSYNCOBJECTFROMNTHANDLE2", "Reserved"): "Reserved",
    ("D3DKMT_CREATENATIVEFENCE", "InitialFenceValue"): "Info.InitialFenceValue",
    ("D3DKMT_CREATENATIVEFENCE", "EngineAffinity"): "Info.EngineAffinity",
    ("D3DKMT_CREATENATIVEFENCE", "FenceType"): "Info.Type",
    ("D3DKMT_CREATENATIVEFENCE", "FenceFlags"): "Info.Flags",
    ("D3DKMT_CREATENATIVEFENCE", "CurrentValueCpuVa"): "Info.NativeFenceMapping.CurrentValueCpuVa",
    ("D3DKMT_CREATENATIVEFENCE", "CurrentValueGpuVa"): "Info.NativeFenceMapping.CurrentValueGpuVa",
    ("D3DKMT_CREATENATIVEFENCE", "MonitoredValueGpuVa"): "Info.NativeFenceMapping.MonitoredValueGpuVa",
    ("D3DKMT_CREATENATIVEFENCE", "MappingReserved"): "Info.NativeFenceMapping.Reserved",
    ("D3DKMT_CREATENATIVEFENCE", "InfoReserved"): "Info.Reserved",
    ("D3DDDI_NATIVEFENCEINFO", "FenceType"): "Type",
    ("D3DDDI_NATIVEFENCEINFO", "FenceFlags"): "Flags",
    ("D3DDDI_NATIVEFENCEINFO", "CurrentValueCpuVa"): "NativeFenceMapping.CurrentValueCpuVa",
    ("D3DDDI_NATIVEFENCEINFO", "CurrentValueGpuVa"): "NativeFenceMapping.CurrentValueGpuVa",
    ("D3DDDI_NATIVEFENCEINFO", "MonitoredValueGpuVa"): "NativeFenceMapping.MonitoredValueGpuVa",
    ("D3DDDI_NATIVEFENCEINFO", "MappingReserved"): "NativeFenceMapping.Reserved",
    ("D3DDDI_NATIVEFENCEINFO", "InfoReserved"): "Reserved",
    ("D3DDDI_NATIVEFENCEMAPPING", "MappingReserved"): "Reserved",
    ("D3DKMT_OPENNATIVEFENCEFROMNTHANDLE", "CurrentValueCpuVa"): "NativeFenceMapping.CurrentValueCpuVa",
    ("D3DKMT_OPENNATIVEFENCEFROMNTHANDLE", "CurrentValueGpuVa"): "NativeFenceMapping.CurrentValueGpuVa",
    ("D3DKMT_OPENNATIVEFENCEFROMNTHANDLE", "MonitoredValueGpuVa"):
        "NativeFenceMapping.MonitoredValueGpuVa",
    ("D3DKMT_OPENNATIVEFENCEFROMNTHANDLE", "MappingReserved"): "NativeFenceMapping.Reserved",
    ("D3DKMT_GETDEVICESTATE", "ExecutionState"): "ExecutionState",
    ("D3DKMT_GETDEVICESTATE", "State"): None,
    ("D3DKMT_QUERYSTATISTICS", "QueryResult"): "QueryResult",
    ("D3DKMT_QUERYSTATISTICS", "QueryIds"): None,
    ("D3DKMT_QUERYCLOCKCALIBRATION", "GpuFrequency"): "ClockData.GpuFrequency",
    ("D3DKMT_QUERYCLOCKCALIBRATION", "GpuClockCounter"): "ClockData.GpuClockCounter",
    ("D3DKMT_QUERYCLOCKCALIBRATION", "CpuClockCounter"): "ClockData.CpuClockCounter",
    ("D3DKMT_QUERYCLOCKCALIBRATION", "ClockFlags"): "ClockData.Flags",
    ("D3DKMT_ISFEATUREENABLED", "ResultVersion"): "Result.Version",
    ("D3DKMT_ISFEATUREENABLED", "ResultValue"): "Result.Value",
    ("D3DKMT_ENUMADAPTERS3", "Filter"): "Filter",
    ("D3DKMT_CREATEDEVICE", "pAdapter"): "pAdapter",
    ("D3DDDI_ALLOCATIONINFO2", "Unused"): "Unused",
    ("D3DKMT_MULTIPLANE_OVERLAY", "PlaneFlags"): "PlaneAttributes.Flags",
    ("D3DKMT_MULTIPLANE_OVERLAY2", "PlaneFlags"): "PlaneAttributes.Flags",
    ("D3DKMT_MOVE_RECT", "SourceX"): "SourcePoint.x",
    ("D3DKMT_MOVE_RECT", "SourceY"): "SourcePoint.y",
    ("D3DKMT_POLLDISPLAYCHILDREN", "Bits"): None,
    ("D3DKMT_PRESENTHISTORYTOKEN", "Token"): "Token.MaxSize",
    ("D3DKMT_SETVIDPNSOURCEOWNER1", "Flags"): "Flags",
    ("D3DKMT_OPENNATIVEFENCEFROMNTHANDLE", "Reserved"): "Reserved",
    ("D3DKMT_OPENNATIVEFENCEFROMNTHANDLE", "PrivateDriverData"): "PrivateDriverData",
    ("D3DKMT_CREATE_DOORBELL", "hDoorbell"): "hDoorbell",
}

# prefixes for fields flattened from a named member, per struct
FLATTEN_PREFIX = {
    "D3DKMT_SUBMITPRESENTBLTTOHWQUEUE": ("PrivatePresentData.", 2),
    "D3DKMT_SUBMITPRESENTTOHWQUEUE": ("PrivatePresentData.", 1),
    "D3DKMT_MULTIPLANE_OVERLAY": ("PlaneAttributes.", 3),
    "D3DKMT_MULTIPLANE_OVERLAY2": ("PlaneAttributes.", 3),
    "D3DKMT_SETVIDPNSOURCEOWNER1": ("Version0.", 0),
    "D3DKMT_OPENNATIVEFENCEFROMNTHANDLE": ("NativeFenceMapping.", 5),
}

# structs whose nvt size is deliberately shorter than the C struct
SIZE_SKIP = set()


def structs():
    seen = {}
    for name in dir(L):
        v = getattr(L, name)
        if isinstance(v, L.Struct) and v.name not in seen:
            seen[v.name] = v
    return [seen[k] for k in sorted(seen)]


def gen(P):
    out = []
    for st in structs():
        cname = CNAME.get(st.name, st.name)
        if cname is None or not cname[0].isupper():
            continue
        size, _, fields = st.layout(P)
        pre = FLATTEN_PREFIX.get(st.name)
        done = set()
        for i, (fname, off, _t) in enumerate(fields):
            if fname in done:
                continue
            done.add(fname)
            path = CPATH.get((st.name, fname), fname)
            if path is None:
                continue
            if pre and (st.name, fname) not in CPATH and i >= pre[1]:
                path = pre[0] + fname
            out.append('_Static_assert(__builtin_offsetof(%s, %s) == %d, "%s.%s: P=%d offset %d");'
                       % (cname, path, off, cname, path, P, off))
        if st.name not in SIZE_SKIP:
            out.append('_Static_assert(sizeof(%s) == %d, "%s: P=%d size %d");' % (cname, size, cname, P, size))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--wdk", default=os.environ.get("WDK_INC", DEFAULT_WDK))
    ap.add_argument("--keep", help="write the generated C file here")
    a = ap.parse_args()
    if not os.path.exists(os.path.join(a.wdk, "d3dkmthk.h")):
        print("check_layouts_wdk: no d3dkmthk.h in %s (set WDK_INC); skipped" % a.wdk)
        return 77
    src = ["#include <windows.h>", "#include <winternl.h>", "#include <d3dkmthk.h>",
           "typedef struct { POINT SourcePoint; RECT DestRect; } NVT_MOVE_RECT_CHECK;",
           "#ifdef _WIN64"] + gen(8) + ["#else"] + gen(4) + ["#endif", ""]
    text = "\n".join(src)
    if a.keep:
        with open(a.keep, "w") as f:
            f.write(text)
    rc = 0
    with tempfile.TemporaryDirectory() as td:
        c = os.path.join(td, "check.c")
        with open(c, "w") as f:
            f.write(text)
        for cc in ("x86_64-w64-mingw32-gcc", "i686-w64-mingw32-gcc"):
            if not shutil.which(cc):
                print("check_layouts_wdk: %s not found; skipped" % cc)
                return 77
            p = subprocess.run([cc, "-fsyntax-only", "-w", "-std=gnu11", "-I", a.wdk,
                                "-D_WIN32_WINNT=0x0A00", "-DNTDDI_VERSION=0x0A000010", c],
                               capture_output=True, text=True)
            errs = [ln for ln in p.stderr.splitlines() if "error" in ln]
            n = text.count("_Static_assert")
            if p.returncode:
                rc = 1
                print("%s: %d errors" % (cc, len(errs)))
                for ln in errs:
                    print("  " + ln.split("error: ", 1)[-1])
            else:
                print("%s: %d layout assertions hold" % (cc, n // 2))
    return rc


if __name__ == "__main__":
    sys.exit(main())
