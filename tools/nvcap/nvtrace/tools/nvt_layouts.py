"""Struct layouts for the calls nvtrace captures.

One description serves two consumers:

  * tools/gen_layouts.py turns it into C tables (src/layouts_gen.h) that tell
    the capture DLL which pointers to follow and how many bytes to copy, for
    x86 and x64 separately;
  * tools/ntrdump.py uses it to name the fields of a captured argument struct.

Layouts follow the MSVC rules for the Windows SDK headers: natural alignment,
UINT64 8-aligned on both x86 and x64, pointers and HANDLEs 4 or 8 bytes.
x64 offsets were checked against the WSL dxgkrnl uapi header (which spells out
the x64 padding); x86 offsets follow the same rules and agree with Wine's
wow64win thunk structs where Wine has one.
"""

# ---------------------------------------------------------------- types

class Prim:
    def __init__(self, name, size, align=None, ptr=False):
        self.name = name
        self._size = size
        self._align = align if align is not None else size
        self.ptr = ptr

    def size(self, P):
        return P if self._size == "P" else self._size

    def align(self, P):
        return P if self._align == "P" else self._align


U8 = Prim("u8", 1)
U16 = Prim("u16", 2)
U32 = Prim("u32", 4)
I32 = Prim("i32", 4)
U64 = Prim("u64", 8)
H = Prim("h", 4)            # D3DKMT_HANDLE
BOOL = Prim("bool", 4)
GPUVA = Prim("gpuva", 8)
PTR = Prim("ptr", "P", "P", ptr=True)
HANDLE = Prim("handle", "P", "P")
SIZE_T = Prim("size_t", "P", "P")
WCHAR = Prim("wchar", 2)


class Arr:
    def __init__(self, t, n):
        self.t = t
        self.n = n
        self.name = "%s[%d]" % (t.name, n)

    def size(self, P):
        return self.t.size(P) * self.n

    def align(self, P):
        return self.t.align(P)


class Ptr:
    """A pointer field that the capture follows.

    size=   field name holding a byte count, or an int
    count=  field name holding an element count (with elem=)
    elem=   element type (Struct or Prim); a Struct's own Ptr fields are
            followed for every element
    target= a Struct the pointer points to (one element)
    wstr=   NUL-terminated UTF-16 string
    mode=   'in' (copy before the call only), 'out' (after only), 'inout'
    chase=  scan the copied bytes for further user pointers and copy those
    """

    def __init__(self, size=None, count=None, elem=None, target=None, wstr=False,
                 mode="inout", chase=False, mult=1):
        self.size_spec = size
        self.count = count
        self.elem = elem
        self.target = target
        self.wstr = wstr
        self.mode = mode
        self.chase = chase
        self.mult = mult
        self.name = "ptr"
        self.ptr = True

    def size(self, P):
        return P

    def align(self, P):
        return P


class Union:
    """Anonymous union member: its fields share one offset and are visible in
    the parent namespace. Members may be (name, type) or Struct (anonymous)."""

    def __init__(self, *members):
        self.members = members
        self.name = "union"

    def _items(self):
        return self.members

    def size(self, P):
        s = 0
        for m in self.members:
            t = m if isinstance(m, Struct) else m[1]
            s = max(s, t.size(P))
        a = self.align(P)
        return (s + a - 1) // a * a

    def align(self, P):
        a = 1
        for m in self.members:
            t = m if isinstance(m, Struct) else m[1]
            a = max(a, t.align(P))
        return a


class Struct:
    def __init__(self, name, fields, pack=None):
        self.name = name
        self.fields = fields
        self.pack = pack
        self._cache = {}

    def layout(self, P):
        """Returns (size, align, [(name, offset, type)]) with unions and
        anonymous structs flattened (first name wins on duplicates)."""
        if P in self._cache:
            return self._cache[P]
        off = 0
        maxa = 1
        out = []
        for f in self.fields:
            if isinstance(f, Union):
                t = f
            else:
                t = f[1]
            a = t.align(P)
            if self.pack:
                a = min(a, self.pack)
            off = (off + a - 1) // a * a
            maxa = max(maxa, a)
            if isinstance(f, Union):
                for m in f.members:
                    if isinstance(m, Struct):
                        _, _, sub = m.layout(P)
                        for (n, o, st) in sub:
                            out.append((n, off + o, st))
                    else:
                        out.append((m[0], off, m[1]))
            else:
                if isinstance(t, Struct) and f[0] is None:
                    _, _, sub = t.layout(P)
                    for (n, o, st) in sub:
                        out.append((n, off + o, st))
                else:
                    out.append((f[0], off, t))
            off += t.size(P)
        size = (off + maxa - 1) // maxa * maxa
        res = (size, maxa, out)
        self._cache[P] = res
        return res

    def size(self, P):
        return self.layout(P)[0]

    def align(self, P):
        return self.layout(P)[1]

    def field(self, P, name):
        for (n, o, t) in self.layout(P)[2]:
            if n == name:
                return o, t
        raise KeyError("%s.%s" % (self.name, name))


S = Struct
LUID = S("LUID", [("LowPart", U32), ("HighPart", I32)])

# ---------------------------------------------------------------- structs

ADAPTERINFO = S("D3DKMT_ADAPTERINFO", [
    ("hAdapter", H), ("AdapterLuid", LUID), ("NumOfSources", U32),
    ("bPrecisePresentRegionsPreferred", BOOL)])

UNICODE_STRING = S("UNICODE_STRING", [
    ("Length", U16), ("MaximumLength", U16),
    ("Buffer", Ptr(size="Length", mode="in"))])

OBJECT_ATTRIBUTES = S("OBJECT_ATTRIBUTES", [
    ("Length", U32), ("RootDirectory", HANDLE),
    ("ObjectName", Ptr(target=UNICODE_STRING, mode="in")),
    ("Attributes", U32), ("SecurityDescriptor", PTR),
    ("SecurityQualityOfService", PTR)])

IO_STATUS_BLOCK = S("IO_STATUS_BLOCK", [("Status", SIZE_T), ("Information", SIZE_T)])

ESCAPE = S("D3DKMT_ESCAPE", [
    ("hAdapter", H), ("hDevice", H), ("Type", U32), ("Flags", U32),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize", chase=True)),
    ("PrivateDriverDataSize", U32), ("hContext", H)])

QUERYADAPTERINFO = S("D3DKMT_QUERYADAPTERINFO", [
    ("hAdapter", H), ("Type", U32),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("PrivateDriverDataSize", U32)])

OPENADAPTERFROMLUID = S("D3DKMT_OPENADAPTERFROMLUID", [("AdapterLuid", LUID), ("hAdapter", H)])
OPENADAPTERFROMHDC = S("D3DKMT_OPENADAPTERFROMHDC", [
    ("hDc", HANDLE), ("hAdapter", H), ("AdapterLuid", LUID), ("VidPnSourceId", U32)])
OPENADAPTERFROMDEVICENAME = S("D3DKMT_OPENADAPTERFROMDEVICENAME", [
    ("pDeviceName", Ptr(wstr=True, mode="in")), ("hAdapter", H), ("AdapterLuid", LUID)])
OPENADAPTERFROMGDIDISPLAYNAME = S("D3DKMT_OPENADAPTERFROMGDIDISPLAYNAME", [
    ("DeviceName", Arr(WCHAR, 32)), ("hAdapter", H), ("AdapterLuid", LUID),
    ("VidPnSourceId", U32)])
CLOSEADAPTER = S("D3DKMT_CLOSEADAPTER", [("hAdapter", H)])
ENUMADAPTERS = S("D3DKMT_ENUMADAPTERS", [("NumAdapters", U32), ("Adapters", Arr(ADAPTERINFO, 16))])
ENUMADAPTERS2 = S("D3DKMT_ENUMADAPTERS2", [
    ("NumAdapters", U32),
    ("pAdapters", Ptr(count="NumAdapters", elem=ADAPTERINFO, mode="out"))])
ENUMADAPTERS3 = S("D3DKMT_ENUMADAPTERS3", [
    ("Filter", U64), ("NumAdapters", U32),
    ("pAdapters", Ptr(count="NumAdapters", elem=ADAPTERINFO, mode="out"))])

CREATEDEVICE = S("D3DKMT_CREATEDEVICE", [
    Union(("hAdapter", H), ("pAdapter", PTR)),
    ("Flags", U32), ("hDevice", H),
    ("pCommandBuffer", PTR), ("CommandBufferSize", U32),
    ("pAllocationList", PTR), ("AllocationListSize", U32),
    ("pPatchLocationList", PTR), ("PatchLocationListSize", U32)])
DESTROYDEVICE = S("D3DKMT_DESTROYDEVICE", [("hDevice", H)])

CREATECONTEXT = S("D3DKMT_CREATECONTEXT", [
    ("hDevice", H), ("NodeOrdinal", U32), ("EngineAffinity", U32), ("Flags", U32),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("PrivateDriverDataSize", U32), ("ClientHint", U32), ("hContext", H),
    ("pCommandBuffer", PTR), ("CommandBufferSize", U32),
    ("pAllocationList", PTR), ("AllocationListSize", U32),
    ("pPatchLocationList", PTR), ("PatchLocationListSize", U32),
    ("CommandBuffer", GPUVA)])
CREATECONTEXTVIRTUAL = S("D3DKMT_CREATECONTEXTVIRTUAL", [
    ("hDevice", H), ("NodeOrdinal", U32), ("EngineAffinity", U32), ("Flags", U32),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("PrivateDriverDataSize", U32), ("ClientHint", U32), ("hContext", H)])
DESTROYCONTEXT = S("D3DKMT_DESTROYCONTEXT", [("hContext", H)])

CREATEHWQUEUE = S("D3DKMT_CREATEHWQUEUE", [
    ("hHwContext", H), ("Flags", U32), ("PrivateDriverDataSize", U32),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("hHwQueue", H), ("hHwQueueProgressFence", H),
    ("HwQueueProgressFenceCPUVirtualAddress", Ptr(size=8, mode="out")),
    ("HwQueueProgressFenceGPUVirtualAddress", GPUVA)])
DESTROYHWQUEUE = S("D3DKMT_DESTROYHWQUEUE", [("hHwQueue", H)])
SUBMITCOMMANDTOHWQUEUE = S("D3DKMT_SUBMITCOMMANDTOHWQUEUE", [
    ("hHwQueue", H), ("HwQueueProgressFenceId", U64), ("CommandBuffer", GPUVA),
    ("CommandLength", U32), ("PrivateDriverDataSize", U32),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize", mode="in")),
    ("NumPrimaries", U32),
    ("WrittenPrimaries", Ptr(count="NumPrimaries", elem=H, mode="in"))])
SUBMITWAITFORSYNCOBJECTSTOHWQUEUE = S("D3DKMT_SUBMITWAITFORSYNCOBJECTSTOHWQUEUE", [
    ("hHwQueue", H), ("ObjectCount", U32),
    ("ObjectHandleArray", Ptr(count="ObjectCount", elem=H, mode="in")),
    ("FenceValueArray", Ptr(count="ObjectCount", elem=U64, mode="in"))])
SUBMITSIGNALSYNCOBJECTSTOHWQUEUE = S("D3DKMT_SUBMITSIGNALSYNCOBJECTSTOHWQUEUE", [
    ("Flags", U32), ("BroadcastHwQueueCount", U32),
    ("BroadcastHwQueueArray", Ptr(count="BroadcastHwQueueCount", elem=H, mode="in")),
    ("ObjectCount", U32),
    ("ObjectHandleArray", Ptr(count="ObjectCount", elem=H, mode="in")),
    ("FenceValueArray", Ptr(count="ObjectCount", elem=U64, mode="in"))])

CREATEPAGINGQUEUE = S("D3DKMT_CREATEPAGINGQUEUE", [
    ("hDevice", H), ("Priority", I32), ("hPagingQueue", H), ("hSyncObject", H),
    ("FenceValueCPUVirtualAddress", Ptr(size=8, mode="out")),
    ("PhysicalAdapterIndex", U32)])
DESTROYPAGINGQUEUE = S("D3DDDI_DESTROYPAGINGQUEUE", [("hPagingQueue", H)])

MAKERESIDENT = S("D3DDDI_MAKERESIDENT", [
    ("hPagingQueue", H), ("NumAllocations", U32),
    ("AllocationList", Ptr(count="NumAllocations", elem=H, mode="in")),
    ("PriorityList", Ptr(count="NumAllocations", elem=U32, mode="in")),
    ("Flags", U32), ("PagingFenceValue", U64), ("NumBytesToTrim", U64)])
EVICT = S("D3DKMT_EVICT", [
    ("hDevice", H), ("NumAllocations", U32),
    ("AllocationList", Ptr(count="NumAllocations", elem=H, mode="in")),
    ("Flags", U32), ("NumBytesToTrim", U64)])

RESERVEGPUVIRTUALADDRESS = S("D3DDDI_RESERVEGPUVIRTUALADDRESS", [
    Union(("hPagingQueue", H), ("hAdapter", H)),
    ("BaseAddress", GPUVA), ("MinimumAddress", GPUVA), ("MaximumAddress", GPUVA),
    ("Size", U64), ("ReservationType", U32), ("DriverProtection", U64),
    ("VirtualAddress", GPUVA), ("PagingFenceValue", U64)])
MAPGPUVIRTUALADDRESS = S("D3DDDI_MAPGPUVIRTUALADDRESS", [
    ("hPagingQueue", H), ("BaseAddress", GPUVA), ("MinimumAddress", GPUVA),
    ("MaximumAddress", GPUVA), ("hAllocation", H), ("OffsetInPages", U64),
    ("SizeInPages", U64), ("Protection", U64), ("DriverProtection", U64),
    ("Reserved0", U32), ("Reserved1", U64), ("VirtualAddress", GPUVA),
    ("PagingFenceValue", U64)])
FREEGPUVIRTUALADDRESS = S("D3DKMT_FREEGPUVIRTUALADDRESS", [
    ("hAdapter", H), ("BaseAddress", GPUVA), ("Size", U64)])
UPDATEGPUVA_OPERATION = S("D3DDDI_UPDATEGPUVIRTUALADDRESS_OPERATION", [
    ("OperationType", U32),
    Union(S("map", [("BaseAddress", GPUVA), ("Size", U64), ("hAllocation", H),
                    ("AllocationOffset", U64), ("AllocationSize", U64),
                    ("Protection", U64), ("DriverProtection", U64)]),
          S("copy", [("SourceAddress", GPUVA), ("CopySize", U64), ("DestAddress", GPUVA)]))])
UPDATEGPUVIRTUALADDRESS = S("D3DKMT_UPDATEGPUVIRTUALADDRESS", [
    ("hDevice", H), ("hContext", H), ("hFenceObject", H), ("NumOperations", U32),
    ("Operations", Ptr(count="NumOperations", elem=UPDATEGPUVA_OPERATION, mode="in")),
    ("Reserved0", U32), ("Reserved1", U64), ("FenceValue", U64), ("Flags", U32)])
UPDATEALLOCPROPERTY = S("D3DDDI_UPDATEALLOCPROPERTY", [
    ("hPagingQueue", H), ("hAllocation", H), ("SupportedSegmentSet", U32),
    ("PreferredSegment", U32), ("Flags", U32), ("PagingFenceValue", U64),
    ("PropertyMaskValue", U32)])

LOCK = S("D3DKMT_LOCK", [
    ("hDevice", H), ("hAllocation", H), ("PrivateDriverData", U32), ("NumPages", U32),
    ("pPages", Ptr(count="NumPages", elem=U32, mode="in")), ("pData", PTR),
    ("Flags", U32), ("GpuVirtualAddress", GPUVA)])
UNLOCK = S("D3DKMT_UNLOCK", [
    ("hDevice", H), ("NumAllocations", U32),
    ("phAllocations", Ptr(count="NumAllocations", elem=H, mode="in"))])
LOCK2 = S("D3DKMT_LOCK2", [("hDevice", H), ("hAllocation", H), ("Flags", U32), ("pData", PTR)])
UNLOCK2 = S("D3DKMT_UNLOCK2", [("hDevice", H), ("hAllocation", H)])

ALLOCATIONINFO = S("D3DDDI_ALLOCATIONINFO", [
    ("hAllocation", H), ("pSystemMem", PTR),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("PrivateDriverDataSize", U32), ("VidPnSourceId", U32), ("Flags", U32)])
ALLOCATIONINFO2 = S("D3DDDI_ALLOCATIONINFO2", [
    ("hAllocation", H), Union(("hSection", HANDLE), ("pSystemMem", PTR)),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("PrivateDriverDataSize", U32), ("VidPnSourceId", U32), ("Flags", U32),
    ("GpuVirtualAddress", GPUVA), Union(("Priority", U32), ("Unused", SIZE_T)),
    ("Reserved", Arr(SIZE_T, 5))])


def _createallocation(name, info):
    return S(name, [
        ("hDevice", H), ("hResource", H), ("hGlobalShare", H),
        ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize", mode="in")),
        ("PrivateRuntimeDataSize", U32),
        Union(("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
              ("pStandardAllocation", PTR)),
        ("PrivateDriverDataSize", U32), ("NumAllocations", U32),
        ("pAllocationInfo", Ptr(count="NumAllocations", elem=info)),
        ("Flags", U32), ("hPrivateRuntimeResourceHandle", HANDLE)])


CREATEALLOCATION = _createallocation("D3DKMT_CREATEALLOCATION", ALLOCATIONINFO)
CREATEALLOCATION2 = _createallocation("D3DKMT_CREATEALLOCATION(2)", ALLOCATIONINFO2)

DESTROYALLOCATION = S("D3DKMT_DESTROYALLOCATION", [
    ("hDevice", H), ("hResource", H),
    ("phAllocationList", Ptr(count="AllocationCount", elem=H, mode="in")),
    ("AllocationCount", U32)])
DESTROYALLOCATION2 = S("D3DKMT_DESTROYALLOCATION2", [
    ("hDevice", H), ("hResource", H),
    ("phAllocationList", Ptr(count="AllocationCount", elem=H, mode="in")),
    ("AllocationCount", U32), ("Flags", U32)])
SETALLOCATIONPRIORITY = S("D3DKMT_SETALLOCATIONPRIORITY", [
    ("hDevice", H), ("hResource", H),
    ("phAllocationList", Ptr(count="AllocationCount", elem=H, mode="in")),
    ("AllocationCount", U32),
    ("pPriorities", Ptr(count="AllocationCount", elem=U32, mode="in"))])
GETALLOCATIONPRIORITY = S("D3DKMT_GETALLOCATIONPRIORITY", [
    ("hDevice", H), ("hResource", H),
    ("phAllocationList", Ptr(count="AllocationCount", elem=H, mode="in")),
    ("AllocationCount", U32),
    ("pPriorities", Ptr(count="AllocationCount", elem=U32, mode="out"))])
QUERYALLOCATIONRESIDENCY = S("D3DKMT_QUERYALLOCATIONRESIDENCY", [
    ("hDevice", H), ("hResource", H),
    ("phAllocationList", Ptr(count="AllocationCount", elem=H, mode="in")),
    ("AllocationCount", U32),
    ("pResidencyStatus", Ptr(count="AllocationCount", elem=U32, mode="out"))])
OFFERALLOCATIONS = S("D3DKMT_OFFERALLOCATIONS", [
    ("hDevice", H),
    ("pResources", Ptr(count="NumAllocations", elem=H, mode="in")),
    ("HandleList", Ptr(count="NumAllocations", elem=H, mode="in")),
    ("NumAllocations", U32), ("Priority", U32), ("Flags", U32)])
RECLAIMALLOCATIONS2 = S("D3DKMT_RECLAIMALLOCATIONS2", [
    ("hPagingQueue", H), ("NumAllocations", U32),
    ("pResources", Ptr(count="NumAllocations", elem=H, mode="in")),
    ("HandleList", Ptr(count="NumAllocations", elem=H, mode="in")),
    ("pResults", Ptr(count="NumAllocations", elem=U32, mode="out")),
    ("PagingFenceValue", U64)])
CHANGEVIDEOMEMORYRESERVATION = S("D3DKMT_CHANGEVIDEOMEMORYRESERVATION", [
    ("hProcess", HANDLE), ("hAdapter", H), ("MemorySegmentGroup", U32),
    ("Reservation", U64), ("PhysicalAdapterIndex", U32)])
QUERYVIDEOMEMORYINFO = S("D3DKMT_QUERYVIDEOMEMORYINFO", [
    ("hProcess", HANDLE), ("hAdapter", H), ("MemorySegmentGroup", U32),
    ("Budget", U64), ("CurrentUsage", U64), ("CurrentReservation", U64),
    ("AvailableForReservation", U64), ("PhysicalAdapterIndex", U32)])
FLUSHHEAPTRANSITIONS = S("D3DKMT_FLUSHHEAPTRANSITIONS", [("hAdapter", H)])
INVALIDATECACHE = S("D3DKMT_INVALIDATECACHE", [
    ("hDevice", H), ("hAllocation", H), ("Offset", SIZE_T), ("Length", SIZE_T)])
MARKDEVICEASERROR = S("D3DKMT_MARKDEVICEASERROR", [("hDevice", H), ("Reason", U32)])

SYNCINFO = S("D3DDDI_SYNCHRONIZATIONOBJECTINFO", [
    ("Type", U32),
    Union(("InitialState", BOOL), ("MaxCount", U32), ("Reserved", Arr(U32, 16)))])
SYNCINFO2 = S("D3DDDI_SYNCHRONIZATIONOBJECTINFO2", [
    ("Type", U32), ("Flags", U32),
    Union(S("monitored", [("InitialFenceValue", U64),
                          ("FenceValueCPUVirtualAddress", Ptr(size=8, mode="out")),
                          ("FenceValueGPUVirtualAddress", GPUVA),
                          ("EngineAffinity", U32)]),
          ("Reserved", Arr(U64, 8))),
    ("SharedHandle", H)])
CREATESYNCHRONIZATIONOBJECT = S("D3DKMT_CREATESYNCHRONIZATIONOBJECT", [
    ("hDevice", H), ("Info", SYNCINFO), ("hSyncObject", H)])
CREATESYNCHRONIZATIONOBJECT2 = S("D3DKMT_CREATESYNCHRONIZATIONOBJECT2", [
    ("hDevice", H), (None, SYNCINFO2), ("hSyncObject", H)])
DESTROYSYNCHRONIZATIONOBJECT = S("D3DKMT_DESTROYSYNCHRONIZATIONOBJECT", [("hSyncObject", H)])
WAITFORSYNCHRONIZATIONOBJECT = S("D3DKMT_WAITFORSYNCHRONIZATIONOBJECT", [
    ("hContext", H), ("ObjectCount", U32), ("ObjectHandleArray", Arr(H, 32))])
WAITFORSYNCHRONIZATIONOBJECT2 = S("D3DKMT_WAITFORSYNCHRONIZATIONOBJECT2", [
    ("hContext", H), ("ObjectCount", U32), ("ObjectHandleArray", Arr(H, 32)),
    Union(("FenceValue", U64), ("Reserved", Arr(U64, 8)))])
SIGNALSYNCHRONIZATIONOBJECT = S("D3DKMT_SIGNALSYNCHRONIZATIONOBJECT", [
    ("hContext", H), ("ObjectCount", U32), ("ObjectHandleArray", Arr(H, 32)),
    ("Flags", U32)])
SIGNALSYNCHRONIZATIONOBJECT2 = S("D3DKMT_SIGNALSYNCHRONIZATIONOBJECT2", [
    ("hContext", H), ("ObjectCount", U32), ("ObjectHandleArray", Arr(H, 32)),
    ("Flags", U32), ("BroadcastContextCount", U32), ("BroadcastContext", Arr(H, 64)),
    Union(("FenceValue", U64), ("CpuEventHandle", HANDLE), ("Reserved", Arr(U64, 8)))])
WAITFROMCPU = S("D3DKMT_WAITFORSYNCHRONIZATIONOBJECTFROMCPU", [
    ("hDevice", H), ("ObjectCount", U32),
    ("ObjectHandleArray", Ptr(count="ObjectCount", elem=H, mode="in")),
    ("FenceValueArray", Ptr(count="ObjectCount", elem=U64, mode="in")),
    ("hAsyncEvent", HANDLE), ("Flags", U32)])
SIGNALFROMCPU = S("D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMCPU", [
    ("hDevice", H), ("ObjectCount", U32),
    ("ObjectHandleArray", Ptr(count="ObjectCount", elem=H, mode="in")),
    ("FenceValueArray", Ptr(count="ObjectCount", elem=U64, mode="in")),
    ("Flags", U32)])
WAITFROMGPU = S("D3DKMT_WAITFORSYNCHRONIZATIONOBJECTFROMGPU", [
    ("hContext", H), ("ObjectCount", U32),
    ("ObjectHandleArray", Ptr(count="ObjectCount", elem=H, mode="in")),
    Union(("MonitoredFenceValueArray", Ptr(count="ObjectCount", elem=U64, mode="in")),
          ("FenceValue", U64), ("Reserved", Arr(U64, 8)))])
SIGNALFROMGPU = S("D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMGPU", [
    ("hContext", H), ("ObjectCount", U32),
    ("ObjectHandleArray", Ptr(count="ObjectCount", elem=H, mode="in")),
    Union(("MonitoredFenceValueArray", Ptr(count="ObjectCount", elem=U64, mode="in")),
          ("Reserved", Arr(U64, 8)))])
SIGNALFROMGPU2 = S("D3DKMT_SIGNALSYNCHRONIZATIONOBJECTFROMGPU2", [
    ("ObjectCount", U32),
    ("ObjectHandleArray", Ptr(count="ObjectCount", elem=H, mode="in")),
    ("Flags", U32), ("BroadcastContextCount", U32),
    ("BroadcastContextArray", Ptr(count="BroadcastContextCount", elem=H, mode="in")),
    Union(("FenceValue", U64), ("CpuEventHandle", HANDLE),
          ("MonitoredFenceValueArray", Ptr(count="ObjectCount", elem=U64, mode="in")),
          ("Reserved", Arr(U64, 8)))])
NATIVEFENCEINFO = S("D3DDDI_NATIVEFENCEINFO", [
    ("InitialFenceValue", U64), ("EngineAffinity", U32), ("FenceType", U32),
    ("FenceFlags", U32),
    (None, S("D3DDDI_NATIVEFENCEMAPPING", [
        ("CurrentValueCpuVa", Ptr(size=8, mode="out")), ("CurrentValueGpuVa", GPUVA),
        ("MonitoredValueGpuVa", GPUVA), ("MappingReserved", Arr(U8, 32))])),
    ("InfoReserved", Arr(U8, 28))])
CREATENATIVEFENCE = S("D3DKMT_CREATENATIVEFENCE", [
    ("hDevice", H), ("hSyncObject", H), ("PrivateDriverData", Arr(U8, 64)),
    (None, NATIVEFENCEINFO), ("Flags", U32), ("Reserved", Arr(U8, 28))])

SUBMITCOMMAND = S("D3DKMT_SUBMITCOMMAND", [
    ("Commands", GPUVA), ("CommandLength", U32), ("Flags", U32),
    ("PresentHistoryToken", U64), ("BroadcastContextCount", U32),
    ("BroadcastContext", Arr(H, 64)),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize", mode="in")),
    ("PrivateDriverDataSize", U32), ("NumPrimaries", U32),
    ("WrittenPrimaries", Arr(H, 16)), ("NumHistoryBuffers", U32),
    ("HistoryBufferArray", Ptr(count="NumHistoryBuffers", elem=H, mode="in"))])
RENDER = S("D3DKMT_RENDER", [
    Union(("hDevice", H), ("hContext", H)),
    ("CommandOffset", U32), ("CommandLength", U32), ("AllocationCount", U32),
    ("PatchLocationCount", U32), ("pNewCommandBuffer", PTR),
    ("NewCommandBufferSize", U32), ("pNewAllocationList", PTR),
    ("NewAllocationListSize", U32), ("pNewPatchLocationList", PTR),
    ("NewPatchLocationListSize", U32), ("Flags", U32), ("PresentHistoryToken", U64),
    ("BroadcastContextCount", U32), ("BroadcastContext", Arr(H, 64)),
    ("QueuedBufferCount", U32), ("NewCommandBuffer", GPUVA),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize", mode="in")),
    ("PrivateDriverDataSize", U32)])

GETDEVICESTATE = S("D3DKMT_GETDEVICESTATE", [
    ("hDevice", H), ("StateType", U32),
    Union(("ExecutionState", U32), ("State", Arr(U64, 6)))])
QUERYSTATISTICS = S("D3DKMT_QUERYSTATISTICS", [
    ("Type", U32), ("AdapterLuid", LUID), ("hProcess", HANDLE),
    ("QueryResult", Arr(U64, 0x308 // 8)), ("QueryIds", Arr(U32, 2))])
QUERYCLOCKCALIBRATION = S("D3DKMT_QUERYCLOCKCALIBRATION", [
    ("hAdapter", H), ("NodeOrdinal", U32), ("PhysicalAdapterIndex", U32),
    (None, S("DXGK_GPUCLOCKDATA", [("GpuFrequency", U64), ("GpuClockCounter", U64),
                                   ("CpuClockCounter", U64), ("ClockFlags", U32)], pack=1))])

SETVIDPNSOURCEOWNER = S("D3DKMT_SETVIDPNSOURCEOWNER", [
    ("hDevice", H),
    ("pType", Ptr(count="VidPnSourceCount", elem=U32, mode="in")),
    ("pVidPnSourceId", Ptr(count="VidPnSourceCount", elem=U32, mode="in")),
    ("VidPnSourceCount", U32)])
SETVIDPNSOURCEOWNER1 = S("D3DKMT_SETVIDPNSOURCEOWNER1", [
    ("hDevice", H),
    ("pType", Ptr(count="VidPnSourceCount", elem=U32, mode="in")),
    ("pVidPnSourceId", Ptr(count="VidPnSourceCount", elem=U32, mode="in")),
    ("VidPnSourceCount", U32), ("Flags", U32)])
GETSCANLINE = S("D3DKMT_GETSCANLINE", [
    ("hAdapter", H), ("VidPnSourceId", U32), ("InVerticalBlank", BOOL), ("ScanLine", U32)])
WAITFORVERTICALBLANKEVENT = S("D3DKMT_WAITFORVERTICALBLANKEVENT", [
    ("hAdapter", H), ("hDevice", H), ("VidPnSourceId", U32)])
WAITFORVERTICALBLANKEVENT2 = S("D3DKMT_WAITFORVERTICALBLANKEVENT2", [
    ("hAdapter", H), ("hDevice", H), ("ObjectCount", U32), ("ObjectHandleArray", Arr(H, 8))])
GETSHAREDRESOURCEADAPTERLUID = S("D3DKMT_GETSHAREDRESOURCEADAPTERLUID", [
    ("hGlobalShare", H), ("hNtHandle", HANDLE), ("AdapterLuid", LUID)])

OPENALLOCATIONINFO = S("D3DDDI_OPENALLOCATIONINFO", [
    ("hAllocation", H),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("PrivateDriverDataSize", U32)])
OPENALLOCATIONINFO2 = S("D3DDDI_OPENALLOCATIONINFO2", [
    ("hAllocation", H),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize")),
    ("PrivateDriverDataSize", U32), ("GpuVirtualAddress", GPUVA),
    ("Reserved", Arr(SIZE_T, 6))])


def _openresource(name, info):
    return S(name, [
        ("hDevice", H), ("hGlobalShare", H), ("NumAllocations", U32),
        ("pOpenAllocationInfo", Ptr(count="NumAllocations", elem=info)),
        ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize")),
        ("PrivateRuntimeDataSize", U32),
        ("pResourcePrivateDriverData", Ptr(size="ResourcePrivateDriverDataSize")),
        ("ResourcePrivateDriverDataSize", U32),
        ("pTotalPrivateDriverDataBuffer", Ptr(size="TotalPrivateDriverDataBufferSize", mode="out")),
        ("TotalPrivateDriverDataBufferSize", U32), ("hResource", H)])


OPENRESOURCE = _openresource("D3DKMT_OPENRESOURCE", OPENALLOCATIONINFO)
OPENRESOURCE2 = _openresource("D3DKMT_OPENRESOURCE(2)", OPENALLOCATIONINFO2)
QUERYRESOURCEINFO = S("D3DKMT_QUERYRESOURCEINFO", [
    ("hDevice", H), ("hGlobalShare", H),
    ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize", mode="out")),
    ("PrivateRuntimeDataSize", U32), ("TotalPrivateDriverDataSize", U32),
    ("ResourcePrivateDriverDataSize", U32), ("NumAllocations", U32)])
OPENRESOURCEFROMNTHANDLE = S("D3DKMT_OPENRESOURCEFROMNTHANDLE", [
    ("hDevice", H), ("hNtHandle", HANDLE), ("NumAllocations", U32),
    ("pOpenAllocationInfo2", Ptr(count="NumAllocations", elem=OPENALLOCATIONINFO2)),
    ("PrivateRuntimeDataSize", U32),
    ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize")),
    ("ResourcePrivateDriverDataSize", U32),
    ("pResourcePrivateDriverData", Ptr(size="ResourcePrivateDriverDataSize")),
    ("TotalPrivateDriverDataBufferSize", U32),
    ("pTotalPrivateDriverDataBuffer", Ptr(size="TotalPrivateDriverDataBufferSize", mode="out")),
    ("hResource", H), ("hKeyedMutex", H),
    ("pKeyedMutexPrivateRuntimeData", Ptr(size="KeyedMutexPrivateRuntimeDataSize")),
    ("KeyedMutexPrivateRuntimeDataSize", U32), ("hSyncObject", H)])
QUERYRESOURCEINFOFROMNTHANDLE = S("D3DKMT_QUERYRESOURCEINFOFROMNTHANDLE", [
    ("hDevice", H), ("hNtHandle", HANDLE),
    ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize", mode="out")),
    ("PrivateRuntimeDataSize", U32), ("TotalPrivateDriverDataSize", U32),
    ("ResourcePrivateDriverDataSize", U32), ("NumAllocations", U32)])
OPENSYNCOBJECTFROMNTHANDLE = S("D3DKMT_OPENSYNCOBJECTFROMNTHANDLE", [
    ("hNtHandle", HANDLE), ("hSyncObject", H)])
OPENSYNCOBJECTFROMNTHANDLE2 = S("D3DKMT_OPENSYNCOBJECTFROMNTHANDLE2", [
    ("hNtHandle", HANDLE), ("hDevice", H), ("Flags", U32), ("hSyncObject", H),
    Union(S("monitored", [("FenceValueCPUVirtualAddress", Ptr(size=8, mode="out")),
                          ("FenceValueGPUVirtualAddress", GPUVA),
                          ("EngineAffinity", U32)]),
          ("Reserved", Arr(U64, 8)))])
OPENNTHANDLEFROMNAME = S("D3DKMT_OPENNTHANDLEFROMNAME", [
    ("dwDesiredAccess", U32),
    ("pObjAttrib", Ptr(target=OBJECT_ATTRIBUTES, mode="in")),
    ("hNtHandle", HANDLE)])

CREATEKEYEDMUTEX = S("D3DKMT_CREATEKEYEDMUTEX", [
    ("InitialValue", U64), ("hSharedHandle", H), ("hKeyedMutex", H)])
CREATEKEYEDMUTEX2 = S("D3DKMT_CREATEKEYEDMUTEX2", [
    ("InitialValue", U64), ("hSharedHandle", H), ("hKeyedMutex", H),
    ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize", mode="in")),
    ("PrivateRuntimeDataSize", U32), ("Flags", U32)])
OPENKEYEDMUTEX = S("D3DKMT_OPENKEYEDMUTEX", [("hSharedHandle", H), ("hKeyedMutex", H)])
OPENKEYEDMUTEX2 = S("D3DKMT_OPENKEYEDMUTEX2", [
    ("hSharedHandle", H), ("hKeyedMutex", H),
    ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize")),
    ("PrivateRuntimeDataSize", U32)])
DESTROYKEYEDMUTEX = S("D3DKMT_DESTROYKEYEDMUTEX", [("hKeyedMutex", H)])
ACQUIREKEYEDMUTEX = S("D3DKMT_ACQUIREKEYEDMUTEX", [
    ("hKeyedMutex", H), ("Key", U64), ("pTimeout", Ptr(size=8, mode="in")),
    ("FenceValue", U64)])
ACQUIREKEYEDMUTEX2 = S("D3DKMT_ACQUIREKEYEDMUTEX2", [
    ("hKeyedMutex", H), ("Key", U64), ("pTimeout", Ptr(size=8, mode="in")),
    ("FenceValue", U64),
    ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize")),
    ("PrivateRuntimeDataSize", U32)])
RELEASEKEYEDMUTEX = S("D3DKMT_RELEASEKEYEDMUTEX", [
    ("hKeyedMutex", H), ("Key", U64), ("FenceValue", U64)])
RELEASEKEYEDMUTEX2 = S("D3DKMT_RELEASEKEYEDMUTEX2", [
    ("hKeyedMutex", H), ("Key", U64), ("FenceValue", U64),
    ("pPrivateRuntimeData", Ptr(size="PrivateRuntimeDataSize", mode="in")),
    ("PrivateRuntimeDataSize", U32)])

CONTEXTPRIORITY = S("D3DKMT_CONTEXTSCHEDULINGPRIORITY", [("hContext", H), ("Priority", I32)])
GETPROCESSDEVICEREMOVALSUPPORT = S("D3DKMT_GETPROCESSDEVICEREMOVALSUPPORT", [
    ("hProcess", HANDLE), ("AdapterLuid", LUID), ("Support", BOOL)])
CHECKOCCLUSION = S("D3DKMT_CHECKOCCLUSION", [("hWnd", HANDLE)])
ADAPTERSOURCE = S("D3DKMT_ADAPTER_VIDPNSOURCE", [("hAdapter", H), ("VidPnSourceId", U32)])
GETRESOURCEPRESENTPRIVATEDRIVERDATA = S("D3DKMT_GETRESOURCEPRESENTPRIVATEDRIVERDATA", [
    ("hResource", H), ("PrivateDriverDataSize", U32),
    ("pPrivateDriverData", Ptr(size="PrivateDriverDataSize", mode="out"))])
CREATEDCFROMMEMORY = S("D3DKMT_CREATEDCFROMMEMORY", [
    ("pMemory", PTR), ("Format", U32), ("Width", U32), ("Height", U32), ("Pitch", U32),
    ("hDeviceDc", HANDLE), ("pColorTable", PTR), ("hDc", HANDLE), ("hBitmap", HANDLE)])
DESTROYDCFROMMEMORY = S("D3DKMT_DESTROYDCFROMMEMORY", [("hDc", HANDLE), ("hBitmap", HANDLE)])
CONFIGURESHAREDRESOURCE = S("D3DKMT_CONFIGURESHAREDRESOURCE", [
    ("hDevice", H), ("hResource", H), ("IsDwm", BOOL), ("hProcess", HANDLE),
    ("AllowAccess", BOOL)])
CHECKSHAREDRESOURCEACCESS = S("D3DKMT_CHECKSHAREDRESOURCEACCESS", [
    ("hResource", H), ("ClientPid", U32)])
ISFEATUREENABLED = S("D3DKMT_ISFEATUREENABLED", [
    ("hAdapter", H), ("FeatureId", U32), ("ResultVersion", U16), ("ResultValue", U16)])
GETSHAREDPRIMARYHANDLE = S("D3DKMT_GETSHAREDPRIMARYHANDLE", [
    ("hAdapter", H), ("VidPnSourceId", U32), ("hSharedPrimary", H)])
POLLDISPLAYCHILDREN = S("D3DKMT_POLLDISPLAYCHILDREN", [("hAdapter", H), ("Flags", U32)])


class Raw:
    """A struct whose fields nvtrace does not name: copy `size` bytes."""

    def __init__(self, name, size64, size32=None):
        self.name = name
        self.s64 = size64
        self.s32 = size32 if size32 is not None else size64

    def size(self, P):
        return self.s64 if P == 8 else self.s32

    def layout(self, P):
        return (self.size(P), 8, [])


# ---------------------------------------------------------------- functions

class Fn:
    """A hooked function. `args` are (name, type) in argument order; each is
    one pointer-sized stack slot. `flags` are C flag names (src/nvtrace.h)."""

    def __init__(self, module, name, args, flags=()):
        self.module = module
        self.name = name
        self.args = args
        self.flags = flags

    def arg_layout(self, P):
        """[(name, offset, type)] of the argument slots (one pointer-sized
        slot per argument, as nvtrace stores them in a CALL record)."""
        return [(n, i * P, t) for i, (n, t) in enumerate(self.args)]


def kmt(name, struct, mode="inout", extra_flags=()):
    """NtGdiDdDDI<name>(<struct> *)."""
    return Fn("win32u.dll", "NtGdiDdDDI" + name,
              [("pData", Ptr(target=struct, mode=mode))], extra_flags)


FUNCS = [
    kmt("Escape", ESCAPE),
    kmt("QueryAdapterInfo", QUERYADAPTERINFO),
    kmt("OpenAdapterFromLuid", OPENADAPTERFROMLUID),
    kmt("OpenAdapterFromHdc", OPENADAPTERFROMHDC),
    kmt("OpenAdapterFromDeviceName", OPENADAPTERFROMDEVICENAME),
    kmt("OpenAdapterFromGdiDisplayName", OPENADAPTERFROMGDIDISPLAYNAME),
    kmt("CloseAdapter", CLOSEADAPTER),
    kmt("EnumAdapters", ENUMADAPTERS),
    kmt("EnumAdapters2", ENUMADAPTERS2),
    kmt("EnumAdapters3", ENUMADAPTERS3),
    kmt("CreateDevice", CREATEDEVICE, extra_flags=("NVT_FN_CTXTRACK",)),
    kmt("DestroyDevice", DESTROYDEVICE),
    kmt("CreateContext", CREATECONTEXT, extra_flags=("NVT_FN_CTXTRACK",)),
    kmt("CreateContextVirtual", CREATECONTEXTVIRTUAL),
    kmt("DestroyContext", DESTROYCONTEXT),
    kmt("CreateHwContext", Raw("D3DKMT_CREATEHWCONTEXT", 0x40, 0x30)),
    kmt("DestroyHwContext", Raw("D3DKMT_DESTROYHWCONTEXT", 4)),
    kmt("CreateHwQueue", CREATEHWQUEUE),
    kmt("DestroyHwQueue", DESTROYHWQUEUE),
    kmt("SubmitCommandToHwQueue", SUBMITCOMMANDTOHWQUEUE),
    kmt("SubmitWaitForSyncObjectsToHwQueue", SUBMITWAITFORSYNCOBJECTSTOHWQUEUE),
    kmt("SubmitSignalSyncObjectsToHwQueue", SUBMITSIGNALSYNCOBJECTSTOHWQUEUE),
    kmt("SubmitPresentToHwQueue", Raw("D3DKMT_SUBMITPRESENTTOHWQUEUE", 0x400)),
    kmt("SubmitPresentBltToHwQueue", Raw("D3DKMT_SUBMITPRESENTBLTTOHWQUEUE", 0x400)),
    kmt("CreatePagingQueue", CREATEPAGINGQUEUE),
    kmt("DestroyPagingQueue", DESTROYPAGINGQUEUE),
    kmt("MakeResident", MAKERESIDENT),
    kmt("Evict", EVICT),
    kmt("ReserveGpuVirtualAddress", RESERVEGPUVIRTUALADDRESS),
    kmt("MapGpuVirtualAddress", MAPGPUVIRTUALADDRESS),
    kmt("FreeGpuVirtualAddress", FREEGPUVIRTUALADDRESS),
    kmt("UpdateGpuVirtualAddress", UPDATEGPUVIRTUALADDRESS),
    kmt("UpdateAllocationProperty", UPDATEALLOCPROPERTY),
    kmt("Lock", LOCK),
    kmt("Unlock", UNLOCK),
    kmt("Lock2", LOCK2),
    kmt("Unlock2", UNLOCK2),
    kmt("CreateAllocation", CREATEALLOCATION),
    kmt("CreateAllocation2", CREATEALLOCATION2),
    kmt("DestroyAllocation", DESTROYALLOCATION),
    kmt("DestroyAllocation2", DESTROYALLOCATION2),
    kmt("SetAllocationPriority", SETALLOCATIONPRIORITY),
    kmt("GetAllocationPriority", GETALLOCATIONPRIORITY),
    kmt("QueryAllocationResidency", QUERYALLOCATIONRESIDENCY),
    kmt("OfferAllocations", OFFERALLOCATIONS),
    kmt("ReclaimAllocations2", RECLAIMALLOCATIONS2),
    kmt("ChangeVideoMemoryReservation", CHANGEVIDEOMEMORYRESERVATION),
    kmt("QueryVideoMemoryInfo", QUERYVIDEOMEMORYINFO),
    kmt("FlushHeapTransitions", FLUSHHEAPTRANSITIONS),
    kmt("InvalidateCache", INVALIDATECACHE),
    kmt("MarkDeviceAsError", MARKDEVICEASERROR),
    kmt("CreateSynchronizationObject", CREATESYNCHRONIZATIONOBJECT),
    kmt("CreateSynchronizationObject2", CREATESYNCHRONIZATIONOBJECT2),
    kmt("DestroySynchronizationObject", DESTROYSYNCHRONIZATIONOBJECT),
    kmt("WaitForSynchronizationObject", WAITFORSYNCHRONIZATIONOBJECT),
    kmt("WaitForSynchronizationObject2", WAITFORSYNCHRONIZATIONOBJECT2),
    kmt("SignalSynchronizationObject", SIGNALSYNCHRONIZATIONOBJECT),
    kmt("SignalSynchronizationObject2", SIGNALSYNCHRONIZATIONOBJECT2),
    kmt("WaitForSynchronizationObjectFromCpu", WAITFROMCPU),
    kmt("SignalSynchronizationObjectFromCpu", SIGNALFROMCPU),
    kmt("WaitForSynchronizationObjectFromGpu", WAITFROMGPU),
    kmt("SignalSynchronizationObjectFromGpu", SIGNALFROMGPU),
    kmt("SignalSynchronizationObjectFromGpu2", SIGNALFROMGPU2),
    kmt("CreateNativeFence", CREATENATIVEFENCE),
    kmt("OpenNativeFenceFromNtHandle", Raw("D3DKMT_OPENNATIVEFENCEFROMNTHANDLE", 0xA8)),
    kmt("SubmitCommand", SUBMITCOMMAND),
    kmt("Render", RENDER, extra_flags=("NVT_FN_RENDER",)),
    kmt("Present", Raw("D3DKMT_PRESENT", 0x400)),
    kmt("PresentMultiPlaneOverlay", Raw("D3DKMT_PRESENT_MULTIPLANE_OVERLAY", 0x200)),
    kmt("PresentMultiPlaneOverlay2", Raw("D3DKMT_PRESENT_MULTIPLANE_OVERLAY2", 0x200)),
    kmt("PresentMultiPlaneOverlay3", Raw("D3DKMT_PRESENT_MULTIPLANE_OVERLAY3", 0x200)),
    kmt("GetDeviceState", GETDEVICESTATE),
    kmt("QueryStatistics", QUERYSTATISTICS),
    kmt("QueryClockCalibration", QUERYCLOCKCALIBRATION),
    kmt("SetVidPnSourceOwner", SETVIDPNSOURCEOWNER),
    kmt("SetVidPnSourceOwner1", SETVIDPNSOURCEOWNER1),
    kmt("GetScanLine", GETSCANLINE),
    kmt("WaitForVerticalBlankEvent", WAITFORVERTICALBLANKEVENT),
    kmt("WaitForVerticalBlankEvent2", WAITFORVERTICALBLANKEVENT2),
    kmt("GetSharedResourceAdapterLuid", GETSHAREDRESOURCEADAPTERLUID),
    kmt("OpenResource", OPENRESOURCE),
    kmt("OpenResource2", OPENRESOURCE2),
    kmt("QueryResourceInfo", QUERYRESOURCEINFO),
    kmt("OpenResourceFromNtHandle", OPENRESOURCEFROMNTHANDLE),
    kmt("QueryResourceInfoFromNtHandle", QUERYRESOURCEINFOFROMNTHANDLE),
    kmt("OpenSyncObjectFromNtHandle", OPENSYNCOBJECTFROMNTHANDLE),
    kmt("OpenSyncObjectFromNtHandle2", OPENSYNCOBJECTFROMNTHANDLE2),
    kmt("OpenNtHandleFromName", OPENNTHANDLEFROMNAME),
    kmt("OpenSyncObjectNtHandleFromName", OPENNTHANDLEFROMNAME),
    kmt("CreateKeyedMutex", CREATEKEYEDMUTEX),
    kmt("CreateKeyedMutex2", CREATEKEYEDMUTEX2),
    kmt("OpenKeyedMutex", OPENKEYEDMUTEX),
    kmt("OpenKeyedMutex2", OPENKEYEDMUTEX2),
    kmt("DestroyKeyedMutex", DESTROYKEYEDMUTEX),
    kmt("AcquireKeyedMutex", ACQUIREKEYEDMUTEX),
    kmt("AcquireKeyedMutex2", ACQUIREKEYEDMUTEX2),
    kmt("ReleaseKeyedMutex", RELEASEKEYEDMUTEX),
    kmt("ReleaseKeyedMutex2", RELEASEKEYEDMUTEX2),
    kmt("SetContextSchedulingPriority", CONTEXTPRIORITY),
    kmt("GetContextSchedulingPriority", CONTEXTPRIORITY),
    kmt("SetContextInProcessSchedulingPriority", CONTEXTPRIORITY),
    kmt("GetContextInProcessSchedulingPriority", CONTEXTPRIORITY),
    kmt("GetProcessDeviceRemovalSupport", GETPROCESSDEVICEREMOVALSUPPORT),
    kmt("CheckOcclusion", CHECKOCCLUSION),
    kmt("CheckVidPnExclusiveOwnership", ADAPTERSOURCE),
    kmt("CheckMonitorPowerState", ADAPTERSOURCE),
    kmt("GetResourcePresentPrivateDriverData", GETRESOURCEPRESENTPRIVATEDRIVERDATA),
    kmt("CreateDCFromMemory", CREATEDCFROMMEMORY),
    kmt("DestroyDCFromMemory", DESTROYDCFROMMEMORY),
    kmt("ConfigureSharedResource", CONFIGURESHAREDRESOURCE),
    kmt("CheckSharedResourceAccess", CHECKSHAREDRESOURCEACCESS),
    kmt("IsFeatureEnabled", ISFEATUREENABLED),
    kmt("GetSharedPrimaryHandle", GETSHAREDPRIMARYHANDLE),
    kmt("PollDisplayChildren", POLLDISPLAYCHILDREN),
    Fn("win32u.dll", "NtGdiDdDDIShareObjects", [
        ("cObjects", U32),
        ("hObjects", Ptr(count="cObjects", elem=H, mode="in")),
        ("pObjectAttributes", Ptr(target=OBJECT_ATTRIBUTES, mode="in")),
        ("dwDesiredAccess", U32),
        ("phSharedNtHandle", Ptr(size="P", mode="out"))]),
    Fn("win32u.dll", "NtGdiDdDDISetProcessSchedulingPriorityClass", [
        ("hProcess", HANDLE), ("Priority", U32)]),
    Fn("win32u.dll", "NtGdiDdDDIGetProcessSchedulingPriorityClass", [
        ("hProcess", HANDLE), ("pPriority", Ptr(size=4, mode="out"))]),
    Fn("win32u.dll", "NtGdiDdDDICheckExclusiveOwnership", []),

    # ntdll
    Fn("ntdll.dll", "NtCreateFile", [
        ("FileHandle", Ptr(size="P", mode="out")), ("DesiredAccess", U32),
        ("ObjectAttributes", Ptr(target=OBJECT_ATTRIBUTES, mode="in")),
        ("IoStatusBlock", Ptr(target=IO_STATUS_BLOCK, mode="out")),
        ("AllocationSize", PTR), ("FileAttributes", U32), ("ShareAccess", U32),
        ("CreateDisposition", U32), ("CreateOptions", U32), ("EaBuffer", PTR),
        ("EaLength", U32)], ("NVT_FN_OPEN",)),
    Fn("ntdll.dll", "NtOpenFile", [
        ("FileHandle", Ptr(size="P", mode="out")), ("DesiredAccess", U32),
        ("ObjectAttributes", Ptr(target=OBJECT_ATTRIBUTES, mode="in")),
        ("IoStatusBlock", Ptr(target=IO_STATUS_BLOCK, mode="out")),
        ("ShareAccess", U32), ("OpenOptions", U32)], ("NVT_FN_OPEN",)),
    Fn("ntdll.dll", "NtDeviceIoControlFile", [
        ("FileHandle", HANDLE), ("Event", HANDLE), ("ApcRoutine", PTR),
        ("ApcContext", PTR),
        ("IoStatusBlock", Ptr(target=IO_STATUS_BLOCK, mode="out")),
        ("IoControlCode", U32),
        ("InputBuffer", Ptr(size="InputBufferLength", mode="in", chase=True)),
        ("InputBufferLength", U32),
        ("OutputBuffer", Ptr(size="OutputBufferLength", chase=True)),
        ("OutputBufferLength", U32)], ("NVT_FN_DEVHANDLE",)),
    Fn("ntdll.dll", "NtFsControlFile", [
        ("FileHandle", HANDLE), ("Event", HANDLE), ("ApcRoutine", PTR),
        ("ApcContext", PTR),
        ("IoStatusBlock", Ptr(target=IO_STATUS_BLOCK, mode="out")),
        ("FsControlCode", U32),
        ("InputBuffer", Ptr(size="InputBufferLength", mode="in", chase=True)),
        ("InputBufferLength", U32),
        ("OutputBuffer", Ptr(size="OutputBufferLength", chase=True)),
        ("OutputBufferLength", U32)], ("NVT_FN_DEVHANDLE",)),
    Fn("ntdll.dll", "NtClose", [("Handle", HANDLE)], ("NVT_FN_CLOSE",)),
    Fn("ntdll.dll", "NtTerminateProcess", [("ProcessHandle", HANDLE), ("ExitStatus", U32)],
       ("NVT_FN_TERMINATE",)),
    Fn("ntdll.dll", "NtMapViewOfSection", [
        ("SectionHandle", HANDLE), ("ProcessHandle", HANDLE),
        ("BaseAddress", Ptr(size="P")), ("ZeroBits", SIZE_T), ("CommitSize", SIZE_T),
        ("SectionOffset", Ptr(size=8)), ("ViewSize", Ptr(size="P")),
        ("InheritDisposition", U32), ("AllocationType", U32),
        ("Win32Protect", U32)], ("NVT_FN_MAP",)),
    Fn("ntdll.dll", "NtMapViewOfSectionEx", [
        ("SectionHandle", HANDLE), ("ProcessHandle", HANDLE),
        ("BaseAddress", Ptr(size="P")), ("SectionOffset", Ptr(size=8)),
        ("ViewSize", Ptr(size="P")), ("AllocationType", U32),
        ("PageProtection", U32), ("ExtendedParameters", PTR),
        ("ParameterCount", U32)], ("NVT_FN_MAP",)),
    Fn("ntdll.dll", "NtUnmapViewOfSection", [
        ("ProcessHandle", HANDLE), ("BaseAddress", PTR)], ("NVT_FN_MAP",)),
    Fn("ntdll.dll", "NtUnmapViewOfSectionEx", [
        ("ProcessHandle", HANDLE), ("BaseAddress", PTR), ("Flags", U32)], ("NVT_FN_MAP",)),
]

FUNC_BY_NAME = {f.name: f for f in FUNCS}


def arg0_struct(fn):
    """The struct an NtGdiDdDDI* function's first argument points to."""
    if fn.args and isinstance(fn.args[0][1], Ptr) and fn.args[0][1].target is not None:
        return fn.args[0][1].target
    return None
