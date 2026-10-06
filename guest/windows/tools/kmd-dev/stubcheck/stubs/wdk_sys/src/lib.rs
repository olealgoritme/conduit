#![no_std]
#![allow(non_camel_case_types,non_snake_case,non_upper_case_globals,dead_code)]
use core::ffi::c_void;
pub type BOOLEAN = u8;
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DDDIFORMAT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DDDI_PATCHLOCATIONLIST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DDDI_VIDEO_PRESENT_TARGET_ID { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_HVIDPN { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_HVIDPNSOURCEMODESET { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_HVIDPNTARGETMODESET { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_HVIDPNTOPOLOGY { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_MONITOR_SOURCE_MODE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_PREEMPTION_CAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_VIDEO_SIGNAL_INFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_VIDPN_HW_CAPABILITY { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_VIDPN_PRESENT_PATH { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_VIDPN_PRESENT_PATH_ROTATION_SUPPORT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_VIDPN_PRESENT_PATH_SCALING_SUPPORT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_VIDPN_SOURCE_MODE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct D3DKMDT_VIDPN_TARGET_MODE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DEVICE_POWER_STATE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DRIVER_INITIALIZATION_DATA { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARGCB_NOTIFY_INTERRUPT_DATA { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARGCB_RELEASEHANDLEDATA { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_BUILDPAGINGBUFFER { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_CALIBRATEGPUCLOCK { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_COMMITVIDPN { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_CREATEALLOCATION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_CREATECONTEXT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_CREATEDEVICE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_CREATEPROCESS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_DESTROYALLOCATION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_ENUMVIDPNCOFUNCMODALITY { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_ESCAPE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_FORMATHISTORYBUFFER { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_HISTORYBUFFERPRECISION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_MAPCPUHOSTAPERTURE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_OPENALLOCATION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_PREEMPTCOMMAND { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_PRESENT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_QUERYADAPTERINFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_QUERYCURRENTFENCE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_RECOMMENDMONITORMODES { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_SUBMITCOMMAND { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKARG_SUBMITCOMMANDVIRTUAL { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKCB_READ_DEVICE_SPACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKCB_WRITE_DEVICE_SPACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGKRNL_INTERFACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_64_BIT_ONLY_CAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_ADAPTER_PERFDATACAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_ALLOCATIONINFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_ALLOCATIONLIST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_BUILDPAGINGBUFFER_FILLVIRTUAL { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_BUILDPAGINGBUFFER_TRANSFERVIRTUAL { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_BUILDPAGINGBUFFER_UPDATEPAGETABLE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_CHILD_CONTAINER_ID { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_CHILD_DESCRIPTOR { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_CHILD_STATUS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_CPUHOSTAPERTURE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_DEVICE_DESCRIPTOR { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_DEVICE_INFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_DIRTY_BIT_TRACKING_CAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_DRIVERCAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_ENGINESTATUS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_ENGINE_TYPE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_GPUENGINETOPOLOGY { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_GPUMMUCAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_GPUVERSION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_HARDWARERESERVEDRANGES { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_IOMMU_CAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_NODEMETADATA { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_PAGE_TABLE_LEVEL_DESC { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_PHYSICAL_MEMORY_CAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_QUERYPAGETABLELEVELDESCIN { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_QUERYSEGMENTOUT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_QUERYSEGMENTOUT3 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_QUERYSEGMENTOUT4 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_SEGMENTDESCRIPTOR { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_SEGMENTDESCRIPTOR3 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_SEGMENTDESCRIPTOR4 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_START_INFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_VIDPNSOURCEMODESET_INTERFACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_VIDPNTARGETMODESET_INTERFACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_VIDPNTOPOLOGY_INTERFACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_VIDPN_INTERFACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct DXGK_WDDMDEVICECAPS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct GUID { }
pub type HANDLE = *mut c_void;
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_DXGKARG_QUERYDEPENDENTENGINEGROUP { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_CREATEHWCONTEXT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_CREATEHWQUEUE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_DESCRIBEALLOCATION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_GETROOTPAGETABLESIZE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_GETSCANLINE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_GETSTANDARDALLOCATIONDRIVERDATA { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_ISSUPPORTEDVIDPN { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_PRESENT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_QUERYCURRENTFENCE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_QUERYENGINESTATUS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_QUERYVIDPNHWCAPABILITY { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_RENDER { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_RENDERGDI { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_RESETENGINE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct INOUT_PDXGKARG_UPDATEMONITORLINKINFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_BOOLEAN { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_DXGK_INTERRUPT_TYPE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_HANDLE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_CANCELCOMMAND { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_CLOSEALLOCATION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_COLLECTDBGINFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_COMMITVIDPN_CONST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_ENUMVIDPNCOFUNCMODALITY_CONST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_MAPCPUHOSTAPERTURE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_OPENALLOCATION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_PATCH { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_RECOMMENDFUNCTIONALVIDPN_CONST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_RECOMMENDMONITORMODES_CONST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SETPOINTERPOSITION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SETPOINTERSHAPE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SETROOTPAGETABLE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SETSTABLEPOWERSTATE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SETVIDPNSOURCEADDRESS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SETVIDPNSOURCEVISIBILITY { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SETVIRTUALMACHINEDATA { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SUBMITCOMMAND { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SUBMITCOMMANDTOHWQUEUE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_SWITCHTOHWCONTEXTLIST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_UNMAPCPUHOSTAPERTURE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PDXGKARG_UPDATEACTIVEVIDPNPRESENTPATH_CONST { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_CONST_PVOID { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_DXGK_EVENT_TYPE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_OUT_PDXGK_PRE_START_INFO { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_PQUERY_INTERFACE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_PVOID { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_UCHAR { }
#[repr(C)] #[derive(Clone,Copy)] pub struct IN_ULONG { }
#[repr(C)] #[derive(Clone,Copy)] pub struct KDPC { }
#[repr(C)] #[derive(Clone,Copy)] pub struct KEVENT { }
pub type KIRQL = u8;
#[repr(C)] #[derive(Clone,Copy)] pub struct LARGE_INTEGER { pub QuadPart: i64 }
pub type LONG = i32;
#[repr(C)] #[derive(Clone,Copy)] pub struct LPCGUID { }
#[repr(C)] #[derive(Clone,Copy)] pub struct MDL { }
pub type NTSTATUS = i32;
#[repr(C)] #[derive(Clone,Copy)] pub struct OUT_PDXGKARG_CALIBRATEGPUCLOCK { }
#[repr(C)] #[derive(Clone,Copy)] pub struct OUT_PDXGKARG_GETNODEMETADATA { }
#[repr(C)] #[derive(Clone,Copy)] pub struct OUT_PULONG { }
#[repr(C)] #[derive(Clone,Copy)] pub struct PDEVICE_OBJECT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct PDRIVER_OBJECT { }
#[repr(C)] #[derive(Clone,Copy)] pub struct PDXGKARG_SYSTEM_DISPLAY_ENABLE_FLAGS { }
#[repr(C)] #[derive(Clone,Copy)] pub struct PDXGK_DISPLAY_INFORMATION { }
pub type PHYSICAL_ADDRESS = LARGE_INTEGER;
pub type PMDL = *mut c_void;
#[repr(C)] #[derive(Clone,Copy)] pub struct POBJECT_TYPE { }
#[repr(C)] #[derive(Clone,Copy)] pub struct POWER_ACTION { }
#[repr(C)] #[derive(Clone,Copy)] pub struct PSIZE_T { }
#[repr(C)] #[derive(Clone,Copy)] pub struct PUNICODE_STRING { }
#[repr(C)] #[derive(Clone,Copy)] pub struct PVIDEO_REQUEST_PACKET { }
pub type PVOID = *mut c_void;
#[repr(C)] #[derive(Clone,Copy)] pub struct RTL_QUERY_REGISTRY_TABLE { }
pub type SIZE_T = usize;
pub type UCHAR = u8;
pub type UINT = u32;
pub type UINT32 = u32;
pub type ULONG = u32;
pub type ULONG64 = u64;
pub type ULONG_PTR = usize;
pub type USHORT = u16;
pub type WCHAR = u16;
#[repr(C)] #[derive(Clone,Copy)] pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_2 { }
#[repr(C)] #[derive(Clone,Copy)] pub struct _DXGKARG_BUILDPAGINGBUFFER__bindgen_ty_1__bindgen_ty_3 { }
pub const DXGK_ALLOCATION_LIST_SIZE_GDICONTEXT: u32 = 0;
pub const DXGK_PRESENT_DESTINATION_INDEX: u32 = 0;
pub const DXGK_PRESENT_SOURCE_INDEX: u32 = 0;
pub const DXGK_WHICHSPACE_CONFIG: u32 = 0;
pub const DxgkInitialize: u32 = 0;
pub const STATUS_BUFFER_TOO_SMALL: i32 = 0;
pub const STATUS_DEVICE_BUSY: i32 = 0;
pub const STATUS_DEVICE_DOES_NOT_EXIST: i32 = 0;
pub const STATUS_DEVICE_NOT_READY: i32 = 0;
pub const STATUS_GRAPHICS_INVALID_VIDPN: i32 = 0;
pub const STATUS_INSUFFICIENT_RESOURCES: i32 = 0;
pub const STATUS_INVALID_DEVICE_REQUEST: i32 = 0;
pub const STATUS_INVALID_HANDLE: i32 = 0;
pub const STATUS_INVALID_PARAMETER: i32 = 0;
pub const STATUS_IO_TIMEOUT: i32 = 0;
pub const STATUS_NOT_IMPLEMENTED: i32 = 0;
pub const STATUS_NOT_SUPPORTED: i32 = 0;
pub const STATUS_NO_MEMORY: i32 = 0;
pub const STATUS_SUCCESS: i32 = 0;
pub const STATUS_TIMEOUT: i32 = 0;
pub const STATUS_UNSUCCESSFUL: i32 = 0;
pub mod _D3DDDIFORMAT { pub const D3DDDIFMT_A8B8G8R8: u32 = 0; pub const D3DDDIFMT_A8R8G8B8: u32 = 0; }
pub mod _D3DKMDT_STANDARDALLOCATION_TYPE {  }
pub mod _DXGK_PAGETABLEUPDATEMODE { pub const DXGK_PAGETABLEUPDATE_GPU_PHYSICAL: u32 = 0; }
pub mod _D3DKMDT_COMPUTE_PREEMPTION_GRANULARITY { pub const D3DKMDT_COMPUTE_PREEMPTION_DMA_BUFFER_BOUNDARY: u32 = 0; }
pub mod _D3DKMDT_GRAPHICS_PREEMPTION_GRANULARITY { pub const D3DKMDT_GRAPHICS_PREEMPTION_DMA_BUFFER_BOUNDARY: u32 = 0; }
pub mod _DXGK_QUERYADAPTERINFOTYPE {  }
pub mod _DXGK_INTERRUPT_TYPE { pub const DXGK_INTERRUPT_CRTC_VSYNC: u32 = 0; pub const DXGK_INTERRUPT_DMA_COMPLETED: u32 = 0; pub const DXGK_INTERRUPT_DMA_PREEMPTED: u32 = 0; }
pub mod _DXGK_WDDMVERSION {  }
pub mod _TIMER_TYPE { pub const SynchronizationTimer: u32 = 0; }
pub mod DXGKARGCB_GETHANDLEDATA { pub const default: u32 = 0; }
pub mod _DXGK_HANDLE_TYPE { pub const DXGK_HANDLE_ALLOCATION: u32 = 0; }
pub mod _MEMORY_CACHING_TYPE { pub const MmCached: u32 = 0; pub const MmNonCached: u32 = 0; pub const MmWriteCombined: u32 = 0; pub const Type: u32 = 0; }
pub mod _DXGK_CHILD_DEVICE_TYPE { pub const TypeVideoOutput: u32 = 0; }
pub mod _DXGK_CHILD_DEVICE_HPD_AWARENESS { pub const HpdAwarenessAlwaysConnected: u32 = 0; }
pub mod _D3DKMDT_VIDEO_OUTPUT_TECHNOLOGY { pub const D3DKMDT_VOT_DISPLAYPORT_EXTERNAL: u32 = 0; pub const D3DKMDT_VOT_DVI: u32 = 0; pub const D3DKMDT_VOT_HD15: u32 = 0; pub const D3DKMDT_VOT_HDMI: u32 = 0; pub const D3DKMDT_VOT_INTERNAL: u32 = 0; }
pub mod _D3DKMDT_MONITOR_ORIENTATION_AWARENESS { pub const D3DKMDT_MOA_NONE: u32 = 0; }
pub mod _DXGK_CHILD_STATUS_TYPE { pub const StatusConnection: u32 = 0; }
pub mod _DEVICE_POWER_STATE { pub const PowerDeviceD0: u32 = 0; }
pub mod _DXGK_VIDPN_INTERFACE_VERSION { pub const DXGK_VIDPN_INTERFACE_VERSION_V1: u32 = 0; }
pub mod _D3DKMDT_VIDEO_SIGNAL_STANDARD { pub const D3DKMDT_VSS_OTHER: u32 = 0; }
pub mod _D3DDDI_VIDEO_SIGNAL_SCANLINE_ORDERING { pub const D3DDDI_VSSLO_PROGRESSIVE: u32 = 0; }
pub mod _D3DKMDT_VIDPN_SOURCE_MODE_TYPE { pub const D3DKMDT_RMT_GRAPHICS: u32 = 0; }
pub mod _D3DKMDT_COLOR_BASIS { pub const D3DKMDT_CB_SCRGB: u32 = 0; pub const D3DKMDT_CB_SRGB: u32 = 0; }
pub mod _D3DKMDT_PIXEL_VALUE_ACCESS_MODE { pub const D3DKMDT_PVAM_DIRECT: u32 = 0; }
pub mod _D3DKMDT_MODE_PREFERENCE { pub const D3DKMDT_MP_NOTPREFERRED: u32 = 0; pub const D3DKMDT_MP_PREFERRED: u32 = 0; }
pub mod _D3DKMDT_MONITOR_CAPABILITIES_ORIGIN { pub const D3DKMDT_MCO_DRIVER: u32 = 0; }
pub mod _D3DKMDT_ENUMCOFUNCMODALITY_PIVOT_TYPE { pub const D3DKMDT_EPT_ROTATION: u32 = 0; pub const D3DKMDT_EPT_SCALING: u32 = 0; pub const D3DKMDT_EPT_VIDPNSOURCE: u32 = 0; pub const D3DKMDT_EPT_VIDPNTARGET: u32 = 0; }
pub mod _D3DKMDT_VIDPN_PRESENT_PATH_SCALING { pub const D3DKMDT_VPPS_UNPINNED: u32 = 0; }
pub mod _D3DKMDT_VIDPN_PRESENT_PATH_ROTATION { pub const D3DKMDT_VPPR_UNPINNED: u32 = 0; }
pub mod ntddk { pub unsafe fn DbgPrint() {} pub unsafe fn IoAllocateMdl() {} pub unsafe fn IoFreeMdl() {} pub unsafe fn KeAcquireSpinLockRaiseToDpc() {} pub unsafe fn KeCancelTimer() {} pub unsafe fn KeClearEvent() {} pub unsafe fn KeDelayExecutionThread() {} pub unsafe fn KeFlushQueuedDpcs() {} pub unsafe fn KeGetCurrentIrql() {} pub unsafe fn KeInitializeDpc() {} pub unsafe fn KeInitializeEvent() {} pub unsafe fn KeInitializeMutex() {} pub unsafe fn KeInitializeTimerEx() {} pub unsafe fn KeQueryInterruptTimePrecise() {} pub unsafe fn KeReleaseMutex() {} pub unsafe fn KeReleaseSpinLock() {} pub unsafe fn KeSetEvent() {} pub unsafe fn KeSetTimerEx() {} pub unsafe fn KeWaitForSingleObject() {} pub unsafe fn MmAllocateContiguousMemory() {} pub unsafe fn MmBuildMdlForNonPagedPool() {} pub unsafe fn MmFreeContiguousMemory() {} pub unsafe fn MmGetPhysicalAddress() {} pub unsafe fn MmMapIoSpace() {} pub unsafe fn MmMapLockedPagesSpecifyCache() {} pub unsafe fn MmUnmapIoSpace() {} pub unsafe fn MmUnmapLockedPages() {} pub unsafe fn ObDereferenceObjectDeferDelete() {} pub unsafe fn ObReferenceObjectByHandle() {} pub unsafe fn ObfDereferenceObject() {} pub unsafe fn PsCreateSystemThread() {} pub unsafe fn PsTerminateSystemThread() {} pub unsafe fn RtlQueryRegistryValues() {} pub unsafe fn RtlWriteRegistryValue() {} pub unsafe fn ZwClose() {} }
