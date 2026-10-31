#!/usr/bin/env python3
"""Generate src/layouts_gen.h from tools/nvt_layouts.py.

    tools/gen_layouts.py > src/layouts_gen.h

The header holds, for x64 and x86 separately, the list of hooked functions
with their argument count and a tree of pointer descriptors: which pointer to
follow, where its byte or element count lives, and which nested pointers to
follow inside every element.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import nvt_layouts as L  # noqa: E402

FLAG_IN, FLAG_OUT, FLAG_CHASE = 1, 2, 4
KIND_FIXED, KIND_FIELD, KIND_WSTR, KIND_FIELD1 = 0, 1, 2, 3


class Node:
    def __init__(self):
        self.children = []


def resolve(fields, name):
    for (n, o, t) in fields:
        if n == name:
            return o, t
    raise KeyError(name)


def build(fields, name, off, ptr, P, label, labels):
    """One pointer descriptor node (and its children)."""
    n = Node()
    n.off = off
    n.label = labels.setdefault(label, len(labels))
    mode = {"in": FLAG_IN, "out": FLAG_OUT, "inout": FLAG_IN | FLAG_OUT}[ptr.mode]
    n.flags = mode | (FLAG_CHASE if ptr.chase else 0)
    n.szoff = 0
    n.szw = 0
    n.elem = 0
    target = None
    if ptr.wstr:
        n.kind = KIND_WSTR
        n.fixed = 0
    elif ptr.target is not None:
        target = ptr.target
        n.kind = KIND_FIXED
        n.fixed = target.size(P)
    elif ptr.count is not None:
        o, t = resolve(fields, ptr.count)
        n.kind = KIND_FIELD1 if ptr.count_add == 1 else KIND_FIELD
        if ptr.count_add not in (0, 1):
            raise ValueError("count_add must be 0 or 1 (%s)" % label)
        n.szoff = o
        n.szw = t.size(P)
        n.fixed = ptr.elem.size(P)
        if isinstance(ptr.elem, L.Struct):
            target = ptr.elem
            n.elem = ptr.elem.size(P)
    elif isinstance(ptr.size_spec, str) and ptr.size_spec != "P":
        o, t = resolve(fields, ptr.size_spec)
        n.kind = KIND_FIELD
        n.szoff = o
        n.szw = t.size(P)
        n.fixed = ptr.mult
    elif ptr.size_spec == "P":
        n.kind = KIND_FIXED
        n.fixed = P
    else:
        n.kind = KIND_FIXED
        n.fixed = int(ptr.size_spec)
    if n.szw not in (0, 1, 2, 4, 8):
        raise ValueError("bad size width for %s" % label)
    if target is not None and isinstance(target, L.Struct):
        _, _, tf = target.layout(P)
        sep = "[]." if n.elem else "."
        for (fname, foff, ftype) in tf:
            if isinstance(ftype, L.Ptr):
                n.children.append(build(tf, fname, foff, ftype, P, label + sep + fname, labels))
    return n


def flatten(roots):
    """Breadth-first so every node's children are contiguous."""
    out = []
    queue = list(roots)
    for r in roots:
        r.index = None
    order = []
    while queue:
        n = queue.pop(0)
        order.append(n)
        queue.extend(n.children)
    for i, n in enumerate(order):
        n.index = i
    for n in order:
        out.append(n)
    return out


def gen(P, labels):
    fns = []
    nodes_all = []
    for fn in L.FUNCS:
        al = fn.arg_layout(P)
        roots = []
        for (aname, aoff, atype) in al:
            if isinstance(atype, L.Ptr):
                roots.append(build(al, aname, aoff, atype, P, aname, labels))
        flat = flatten(roots)
        base = len(nodes_all)
        nodes_all.extend(flat)
        arg0 = L.arg0_struct(fn)
        fns.append((fn, base, len(roots), arg0.size(P) if arg0 is not None else 0, flat, base))
    return fns, nodes_all


OFFSETS = [
    ("CREATECONTEXT", ["hContext", "pCommandBuffer", "CommandBufferSize", "pAllocationList",
                       "AllocationListSize", "pPatchLocationList", "PatchLocationListSize"]),
    ("CREATEDEVICE", ["hDevice", "pCommandBuffer", "CommandBufferSize", "pAllocationList",
                      "AllocationListSize", "pPatchLocationList", "PatchLocationListSize"]),
    ("RENDER", ["hContext", "CommandOffset", "CommandLength", "AllocationCount",
                "PatchLocationCount", "pNewCommandBuffer", "NewCommandBufferSize",
                "pNewAllocationList", "NewAllocationListSize", "pNewPatchLocationList",
                "NewPatchLocationListSize"]),
    ("OBJECT_ATTRIBUTES", ["RootDirectory", "ObjectName"]),
    ("UNICODE_STRING", ["Length", "Buffer"]),
    ("IO_STATUS_BLOCK", ["Status", "Information"]),
    ("ESCAPE", ["pPrivateDriverData", "PrivateDriverDataSize"]),
    # GPU memory tracking (src/gpumem.c)
    ("LOCK", ["hAllocation", "pData"]),
    ("LOCK2", ["hAllocation", "pData"]),
    ("UNLOCK", ["NumAllocations", "phAllocations"]),
    ("UNLOCK2", ["hAllocation"]),
    ("CREATEALLOCATION", ["hResource", "NumAllocations", "pAllocationInfo"]),
    ("ALLOCATIONINFO", ["hAllocation", "pSystemMem"]),
    ("ALLOCATIONINFO2", ["hAllocation", "pSystemMem", "GpuVirtualAddress"]),
    ("DESTROYALLOCATION", ["hResource", "phAllocationList", "AllocationCount"]),
    ("MAPGPUVIRTUALADDRESS", ["hAllocation", "OffsetInPages", "SizeInPages", "VirtualAddress"]),
    ("RESERVEGPUVIRTUALADDRESS", ["Size", "VirtualAddress"]),
    ("FREEGPUVIRTUALADDRESS", ["BaseAddress", "Size"]),
    ("UPDATEGPUVIRTUALADDRESS", ["NumOperations", "Operations"]),
    ("UPDATEGPUVA_OPERATION", ["OperationType", "BaseAddress", "Size", "hAllocation",
                               "AllocationOffset", "SourceAddress", "CopySize", "DestAddress"]),
    ("SUBMITCOMMAND", ["Commands", "CommandLength", "BroadcastContext"]),
    ("SUBMITCOMMANDTOHWQUEUE", ["hHwQueue", "HwQueueProgressFenceId", "CommandBuffer",
                                "CommandLength"]),
    ("NOTIFY_WORK_SUBMISSION", ["hDoorbell"]),
    ("DESTROY_DOORBELL", ["hDoorbell"]),
    ("CREATE_DOORBELL", ["hHwQueue", "hRingBuffer", "hRingBufferControl",
                         "DoorbellCPUVirtualAddress", "DoorbellSecondaryCPUVirtualAddress",
                         "DoorbellStatusCPUVirtualAddress",
                         "HwQueueProgressFenceLastQueuedValueCPUVirtualAddress", "hDoorbell"]),
    ("CREATEHWQUEUE", ["HwQueueProgressFenceCPUVirtualAddress"]),
    ("CREATEPAGINGQUEUE", ["FenceValueCPUVirtualAddress"]),
    ("CREATESYNCHRONIZATIONOBJECT2", ["FenceValueCPUVirtualAddress"]),
    ("CREATENATIVEFENCE", ["CurrentValueCpuVa"]),
    ("OPENNATIVEFENCEFROMNTHANDLE", ["CurrentValueCpuVa"]),
    ("OPENSYNCOBJECTFROMNTHANDLE2", ["FenceValueCPUVirtualAddress"]),
    ("OPENRESOURCE", ["NumAllocations", "pOpenAllocationInfo"]),
    ("OPENRESOURCEFROMNTHANDLE", ["NumAllocations", "pOpenAllocationInfo2"]),
    ("OPENALLOCATIONINFO2", ["hAllocation", "GpuVirtualAddress"]),
]

SIZES = ["ALLOCATIONINFO", "ALLOCATIONINFO2", "UPDATEGPUVA_OPERATION", "OPENALLOCATIONINFO2",
         "IO_STATUS_BLOCK"]


def emit(out, P, fns, nodes, labels):
    for (sname, fields) in OFFSETS:
        st = getattr(L, sname)
        for f in fields:
            out.append("#define NVT_OFF_%s_%s %d" % (sname, f, st.field(P, f)[0]))
    for sname in SIZES:
        out.append("#define NVT_SIZE_%s %d" % (sname, getattr(L, sname).size(P)))
    out.append("static const nvt_pdesc nvt_pdescs[] = {")
    for n in nodes:
        child = (n.children[0].index if n.children else 0)
        out.append("    {%d, %d, %d, %d, 0x%x, %d, %d, %d, %d, %d}," % (
            n.off, n.szoff, n.kind, n.szw, n.flags, len(n.children), n.fixed, n.elem,
            n.label, child))
    out.append("};")
    out.append("static const nvt_fdesc nvt_fdescs[] = {")
    for (fn, base, nroots, a0, flat, _b) in fns:
        flags = "|".join(fn.flags) if fn.flags else "0"
        out.append('    {"%s", "%s", %d, %d, %d, %s, %d},' % (
            fn.module, fn.name, len(fn.args), base, nroots, flags, a0))
    out.append("};")


def main():
    labels = {}
    fns64, nodes64 = gen(8, labels)
    fns32, nodes32 = gen(4, labels)
    # child indexes are relative to each function's flat list; make global
    for fns, nodes in ((fns64, nodes64), (fns32, nodes32)):
        for (fn, base, nroots, a0, flat, _b) in fns:
            for n in flat:
                n.index += base
    out = ["/* Generated by tools/gen_layouts.py from tools/nvt_layouts.py. Do not edit. */",
           "#ifndef NVT_LAYOUTS_GEN_H", "#define NVT_LAYOUTS_GEN_H", ""]
    out.append("static const char *const nvt_labels[] = {")
    for name, idx in sorted(labels.items(), key=lambda kv: kv[1]):
        out.append('    "%s",' % name)
    out.append("};")
    out.append("#define NVT_NLABELS %d" % len(labels))
    out.append("")
    out.append("#ifdef _WIN64")
    emit(out, 8, fns64, nodes64, labels)
    out.append("#else")
    emit(out, 4, fns32, nodes32, labels)
    out.append("#endif")
    out.append("")
    out.append("#endif")
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()
