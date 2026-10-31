"""Capture rules: which pointed-to buffers inside DDI argument structures get copied.

Every DXGKARG_* / DXGK_* structure that is reachable from a wrapped DDI argument
gets a rule set.  Structures listed in EXPLICIT use exactly those rules; all
others get automatic rules from gen.py (pointer field pX paired with a field
XSize / NumX / XCount, or a single pointed-to record).

Rule tuple: (field path, kind, length field path, when[, extras])
  kinds:
    bytes     pointer + length in bytes
    count     pointer + element count (element size from the pointee type)
    one       pointer to one element (extras mult=N for a fixed N)
    ptrarray  pointer to an array of pointers + count; each pointee captured
    written   pointer advanced by the callee: captures [value before, value after)
    range     pointer + [start field, end field) byte offsets (extras end=...)
    mdl       pointer to an MDL: header plus its PFN array
    adl       DXGK_ADL at this field: PFN array unless the ADL is contiguous
    product   pointer + (length field * end field * mult) bytes
  when: PRE (before the call), POST (after), BOTH
  extras: cond=[(field, value), ...] all must match (e.g. paging Operation)
          maxcount=N caps the element count
"""

PRE, POST, BOTH = 1, 2, 3

QAI = {  # DXGK_QUERYADAPTERINFOTYPE
    "QUERYSEGMENT": 2, "QUERYSEGMENT2": 4, "QUERYSEGMENT3": 5, "QUERYSEGMENT4": 11, "QUERYSEGMENT5": 44,
    "QUERYMMUS": 46,
}

OP = {  # DXGK_BUILDPAGINGBUFFER_OPERATION
    "TRANSFER": 0, "FILL": 1, "DISCARD_CONTENT": 2, "READ_PHYSICAL": 3, "WRITE_PHYSICAL": 4,
    "MAP_APERTURE_SEGMENT": 5, "UNMAP_APERTURE_SEGMENT": 6, "SPECIAL_LOCK_TRANSFER": 7,
    "VIRTUAL_TRANSFER": 8, "VIRTUAL_FILL": 9, "INIT_CONTEXT_RESOURCE": 10, "UPDATE_PAGE_TABLE": 11,
    "FLUSH_TLB": 12, "UPDATE_CONTEXT_ALLOCATION": 13, "COPY_PAGE_TABLE_ENTRIES": 14,
    "NOTIFY_RESIDENCY": 15, "SIGNAL_MONITORED_FENCE": 16, "MAP_APERTURE_SEGMENT2": 17,
    "NOTIFY_FENCE_RESIDENCY": 18, "MAP_MMU": 19, "UNMAP_MMU": 20, "NOTIFY_RESIDENCY2": 21,
    "NOTIFY_ALLOC": 22,
}

_RENDER = [
    ("pCommand", "bytes", "CommandLength", PRE),
    ("pDmaBuffer", "written", None, POST),
    ("pDmaBufferPrivateData", "written", None, POST),
    ("pAllocationList", "count", "AllocationListSize", PRE),
    ("pPatchLocationListIn", "count", "PatchLocationListInSize", PRE),
    ("pPatchLocationListOut", "written", None, POST),
]

EXPLICIT = {
    "DXGKARG_RENDER": _RENDER,
    "DXGKARG_PRESENT": [
        ("pDmaBuffer", "written", None, POST),
        ("pDmaBufferPrivateData", "written", None, POST),
        ("pAllocationList", "one", None, PRE, {"mult": 2}),   # [0]=dst, [1]=src
        ("pPatchLocationListOut", "written", None, POST),
        ("pDstSubRects", "count", "SubRectCnt", PRE),
        ("pPrivateDriverData", "bytes", "PrivateDriverDataSize", PRE),
    ],
    "DXGKARG_PATCH": [
        ("pDmaBuffer", "range", "DmaBufferSubmissionStartOffset", POST,
         {"end": "DmaBufferSubmissionEndOffset"}),
        ("pDmaBufferPrivateData", "range", "DmaBufferPrivateDataSubmissionStartOffset", POST,
         {"end": "DmaBufferPrivateDataSubmissionEndOffset"}),
        ("pAllocationList", "count", "AllocationListSize", PRE),
        ("pPatchLocationList", "count", "PatchLocationListSize", PRE),
    ],
    "DXGKARG_SUBMITCOMMAND": [
        ("pDmaBufferPrivateData", "range", "DmaBufferPrivateDataSubmissionStartOffset", PRE,
         {"end": "DmaBufferPrivateDataSubmissionEndOffset"}),
    ],
    "DXGKARG_SUBMITCOMMANDVIRTUAL": [
        ("pDmaBufferPrivateData", "bytes", "DmaBufferPrivateDataSize", PRE),
    ],
    "DXGKARG_SUBMITCOMMANDTOHWQUEUE": [
        ("pDmaBufferPrivateData", "bytes", "DmaBufferPrivateDataSize", PRE),
    ],
    "DXGKARG_BUILDPAGINGBUFFER": [
        ("pDmaBuffer", "written", None, POST),
        ("pDmaBufferPrivateData", "written", None, POST),
        ("Transfer.Source.pMdl", "mdl", None, PRE,
         {"cond": [("Operation", OP["TRANSFER"]), ("Transfer.Source.SegmentId", 0)]}),
        ("Transfer.Destination.pMdl", "mdl", None, PRE,
         {"cond": [("Operation", OP["TRANSFER"]), ("Transfer.Destination.SegmentId", 0)]}),
        ("MapApertureSegment.pMdl", "mdl", None, PRE, {"cond": [("Operation", OP["MAP_APERTURE_SEGMENT"])]}),
        ("InitContextResource.Destination.pMdl", "mdl", None, PRE,
         {"cond": [("Operation", OP["INIT_CONTEXT_RESOURCE"]),
                   ("InitContextResource.Destination.SegmentId", 0)]}),
        ("UpdatePageTable.pPageTableEntries", "count", "UpdatePageTable.NumPageTableEntries", PRE,
         {"cond": [("Operation", OP["UPDATE_PAGE_TABLE"])]}),
        ("CopyPageTableEntries.pRanges", "count", "CopyPageTableEntries.NumRanges", PRE,
         {"cond": [("Operation", OP["COPY_PAGE_TABLE_ENTRIES"])]}),
        ("UpdateContextAllocation.pDriverPrivateData", "bytes", "UpdateContextAllocation.DriverPrivateDataSize",
         BOTH, {"cond": [("Operation", OP["UPDATE_CONTEXT_ALLOCATION"])]}),
        ("MapApertureSegment2.Adl", "adl", None, PRE, {"cond": [("Operation", OP["MAP_APERTURE_SEGMENT2"])]}),
        ("MmapMmu.Adl", "adl", None, PRE, {"cond": [("Operation", OP["MAP_MMU"])]}),
    ],
    "DXGKARG_OPENALLOCATION": [
        ("pOpenAllocation", "count", "NumAllocations", BOTH),
        ("pPrivateDriverData", "bytes", "PrivateDriverSize", PRE),
    ],
    # QueryAdapterInfo defines what Windows thinks the GPU is: keep the raw output and,
    # for the segment / MMU queries, follow the descriptor arrays the output points to.
    "DXGKARG_QUERYADAPTERINFO": [
        ("pInputData", "bytes", "InputDataSize", PRE),
        ("pOutputData", "bytes", "OutputDataSize", POST),
        ("pOutputData", "one", None, POST, {"cond": [("Type", QAI["QUERYSEGMENT"])], "elem": "DXGK_QUERYSEGMENTOUT"}),
        ("pOutputData", "one", None, POST, {"cond": [("Type", QAI["QUERYSEGMENT3"])],
                                            "elem": "DXGK_QUERYSEGMENTOUT3"}),
        ("pOutputData", "one", None, POST, {"cond": [("Type", QAI["QUERYSEGMENT4"])],
                                            "elem": "DXGK_QUERYSEGMENTOUT4"}),
        ("pOutputData", "one", None, POST, {"cond": [("Type", QAI["QUERYSEGMENT5"])],
                                            "elem": "DXGK_QUERYSEGMENTOUT5"}),
        ("pOutputData", "one", None, POST, {"cond": [("Type", QAI["QUERYMMUS"])], "elem": "DXGK_QUERYMMUSOUT"}),
    ],
    "DXGK_QUERYSEGMENTOUT": [("pSegmentDescriptor", "count", "NbSegment", POST)],
    "DXGK_QUERYSEGMENTOUT3": [("pSegmentDescriptor", "count", "NbSegment", POST)],
    "DXGK_QUERYSEGMENTOUT4": [("pSegmentDescriptor", "product", "NbSegment", POST,
                               {"end": "SegmentDescriptorStride", "mult": 1})],
    # the count comes from an earlier QUERYSEGMENTCOUNT / QUERYMMUCOUNT call: read a generous
    # fixed number of descriptors (reads past the array are harmless, see QUERY*COUNT in the log)
    "DXGK_QUERYSEGMENTOUT5": [("SegmentDescriptors", "one", None, POST, {"mult": 32})],
    "DXGK_QUERYMMUSOUT": [("MmuDescriptors", "one", None, POST, {"mult": 16})],
    "QUERY_INTERFACE": [
        ("InterfaceType", "one", None, PRE),
        ("Interface", "bytes", "Size", POST),
    ],
    "DXGK_DEVICE_INFO": [
        ("DeviceRegistryPath.Buffer", "bytes", "DeviceRegistryPath.Length", POST),
        ("TranslatedResourceList", "one", None, POST, {"mult": 64, "elem": "CM_RESOURCE_LIST"}),
    ],
    "DXGKARG_SETPOINTERSHAPE": [
        ("pPixels", "product", "Height", PRE, {"end": "Pitch", "mult": 2}),
    ],
    "DXGKARG_SETVIDPNSOURCEADDRESSWITHMULTIPLANEOVERLAY3": [
        ("ppPlanes", "ptrarray", "PlaneCount", PRE),
        ("pPostComposition", "one", None, PRE),
        ("pHDRMetaData", "one", None, PRE),
    ],
    "DXGK_MULTIPLANE_OVERLAY_PLANE3": [
        ("ppContextData", "ptrarray", "ContextCount", PRE),
        ("pDriverPrivateData", "bytes", "DriverPrivateDataSize", PRE),
    ],
    "DXGKARG_CREATEPROCESS": [
        ("pPasid", "count", "NumPasid", PRE),
        ("pProcessName", "count", "ProcessNameLength", PRE),
    ],
    "DXGKARG_CREATEDEVICE": [
        ("pInfo", "one", None, POST),
    ],
    "DXGKARG_GETSTANDARDALLOCATIONDRIVERDATA": [
        ("pCreateSharedPrimarySurfaceData", "one", None, PRE, {"cond": [("StandardAllocationType", 1)]}),
        ("pCreateShadowSurfaceData", "one", None, PRE, {"cond": [("StandardAllocationType", 2)]}),
        ("pCreateStagingSurfaceData", "one", None, PRE, {"cond": [("StandardAllocationType", 3)]}),
        ("pCreateGdiSurfaceData", "one", None, PRE, {"cond": [("StandardAllocationType", 4)]}),
        ("pCreateVirtualGpuSurfaceData", "one", None, PRE, {"cond": [("StandardAllocationType", 5)]}),
        ("pCreateFenceStorageData", "one", None, PRE, {"cond": [("StandardAllocationType", 6)]}),
        ("pAllocationPrivateDriverData", "bytes", "AllocationPrivateDriverDataSize", POST),
        ("pResourcePrivateDriverData", "bytes", "ResourcePrivateDriverDataSize", POST),
    ],
    # pointers to driver-owned state that is only meaningful as an address
    "DXGKARG_SETSCHEDULINGLOGBUFFER": [],
}

# Field types that are pointers without a '*' in their spelling.
POINTER_TYPEDEFS = {"PVOID", "LPVOID", "PUCHAR", "PBYTE", "PULONG", "PUINT", "PMDL", "PCHAR", "PWCHAR",
                    "PWSTR", "PUINT8", "PUINT32", "PUINT64", "PULONG64", "PULONGLONG"}

# Records never followed automatically (kernel objects, or handled by another rule kind).
NEVER_FOLLOW = {"_DEVICE_OBJECT", "_DRIVER_OBJECT", "_MDL", "_IRP", "_KDPC", "_KEVENT", "_FILE_OBJECT",
                "_EPROCESS", "_ETHREAD", "_KTHREAD", "_DXGKRNL_INTERFACE", "_DRIVER_INITIALIZATION_DATA"}


def length_param_names(fname, pname):
    """Candidate names of the parameter that holds the byte length of buffer parameter pname."""
    c = [pname + "Size", pname + "Length", pname + "Len", pname + "SizeInBytes"]
    if pname.endswith("Buffer"):
        c += [pname + "Size", pname[:-6] + "BufferSize", pname[:-6] + "Length", "Length", "BufferLength"]
    if pname in ("Buffer", "pBuffer"):
        c += ["Length", "BufferLength", "Size"]
    return set(c)
