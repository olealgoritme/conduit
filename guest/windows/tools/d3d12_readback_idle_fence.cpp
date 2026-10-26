// d3d12_readback_idle_fence.cpp -- ECL fence lifetime regression
// (standalone DX12 harness)
//
// Goal: reproduce, with public D3D12 API calls only, the synchronization
// sequence that D3D9On12 / D3D12TranslationLayer performs for a dynamic
// texture Lock(0) that must read back GPU contents:
//
//   queue image -> CPU-visible staging copy(ies), each repeated copy separated
//       by explicit COPY_DEST -> COPY_SOURCE -> COPY_DEST transitions
//       (the in-game diagnostic does the same with Vulkan
//        TRANSFER_WRITE -> TRANSFER_WRITE barriers between every I2B copy)
//   wait on a fence value that covers that copy work (event, as Microsoft does)
//   CPU overwrites the staging mapping immediately after the wait
//   queue the pre-recorded staging -> image upload and the independent
//       image -> readback copy, then validate every pixel
//
// Reference sequences that this file follows:
//   * D3D12TranslationLayer/src/ImmediateContext.cpp MapDynamicTexture()
//     (RESOURCE_USAGE_DYNAMIC + non-CPU-visible heap + MAP_TYPE_READ):
//     readback copy on the graphics queue, SetLastCopyCommandListID(), then
//     MapUnderlyingSynchronize() -> WaitForFenceValue() -> Map.
//   * D3D12TranslationLayer/src/CommandListManager.cpp
//     WaitForFenceValueInternal() / EnqueueSetEvent():
//         if (GetCompletedValue() >= FenceValue) return success;   // no event
//         SetEventOnCompletion(FenceValue, m_hWaitEvent);
//         WaitForSingleObject(m_hWaitEvent, INFINITE);
//     with m_hWaitEvent = CreateEvent(nullptr, FALSE, FALSE, nullptr)  (auto reset)
//   * D3D9On12/src/9on12Resource.cpp Resource::Lock(): the CPU pointer is
//     returned only after that wait; the client writes pixels immediately.
//   * D3D12TranslationLayer/src/ImmediateContext.cpp UnmapDynamicTexture():
//     staging -> image copy on the same graphics queue after the CPU writes.
//   * D3D12TranslationLayer/src/Resource.cpp GetD3D12HeapType(): staging with
//     CPU read access uses D3D12_HEAP_TYPE_READBACK, whose properties are
//     resolved through GetCustomHeapProperties() (typically CUSTOM /
//     WRITE_BACK / MEMORY_POOL_L0, i.e. both CPU readable and GPU writable).
//
// The optional idle_before_read_ms argument (0..15000, default 0) adds a
// normal CPU idle gap before each round's readback submission, after warmup
// work and command-list recording. No drain is inserted before it. This is
// outside the wait -> memcpy -> upload critical window. Zero skips Sleep.
//
// What it guards: the UMD makes the RM fence for a queue's next
// ExecuteCommandLists ahead of time (Nvk12EclFencePrefetch, vkd3d patch 0009).
// An RM fence times out on the host a fixed time after it is made (5 s, 1 s
// with the backend's safe mode) and then counts as fired. vkd3d patch 0012
// replaces a prepared fence that is 250 ms or older at commit, so an idle gap
// between ECLs cannot hand the next batch an expired fence. With an expired
// fence the native D3D12 fence completes before image -> staging finishes, the
// pending readback overwrites the CPU write, and the upload loses the pattern
// (D3D9On12's dynamic-texture Lock(0), e.g. a font atlas). The byte oracle is
// the verdict; the native fence counter alone cannot prove GPU completion.
//
// Stress copies lengthen a valid readback batch so a premature completion
// overwrites the pattern consistently; one copy can pass despite the ordering
// bug because the copy finishes before the CPU writes. Expected on the current
// driver: PASS for idle 0, 1200 and 6000 ms, with Nvk12EclFencePrefetch on or
// off.
//
// Deliberate deviations from the production path:
//   * the wait timeout is bounded instead of INFINITE;
//   * the staging mapping is taken once and kept for the whole run, so no
//     Map/Unmap driver calls sit inside the critical window;
//   * the readback copies are repeated in one command list (stress control).
//
// Hard rules encoded below:
//   * After the wait returns, the only thing that happens is the CPU memcpy,
//     immediately followed by ExecuteCommandLists of the pre-recorded upload
//     list and its Signal. No GetCompletedValue, no QPC, no printf precedes
//     the memcpy; a driver query would let GPU completion "repair" a
//     premature event before the overwrite and hide the defect. Only the two CPU-side driver calls of the
//     upload submission sit between the memcpy and the GPU upload; the window
//     metric is therefore labelled wait+write+submit, not a wait latency.
//   * The upload/verification list is recorded and closed BEFORE the critical
//     wait, so after the memcpy only ExecuteCommandLists + Signal remain.
//   * Command allocators / lists / resources are never reset or released on a
//     possibly early fence. Objects stay alive until a bounded drain proves
//     their work complete; otherwise they are deliberately leaked.
//   * No pre-wait drain: readback copies are queued behind earlier GPU work.
//   * RESULT PASS is printed only after every round verified, every wait was
//     satisfied, and the final bounded drain completed.
//
// Exit codes: 0 PASS, 2 usage, 3 setup/device-removal/unsupported, 4 wait
//             failure (timeout, device removal during a wait, provably early
//             event, incomplete verification or incomplete drain),
//             5 observed data corruption.
//
// Build (x86 or x64 with the matching MinGW cross compiler):
//   i686-w64-mingw32-g++ -std=c++17 -O2 -static d3d12_readback_idle_fence.cpp
//       -o d3d12_readback_idle_fence-x86.exe -ld3d12 -ldxgi -ldxguid
//   x86_64-w64-mingw32-g++ -std=c++17 -O2 -static d3d12_readback_idle_fence.cpp
//       -o d3d12_readback_idle_fence-x64.exe -ld3d12 -ldxgi -ldxguid
//   (mingw-w64 headers new enough to declare ID3D12Device9)
//
// Usage:
//   d3d12_readback_idle_fence <rounds> <readback_copies> <warmup_batches>
//       [on12_creator:0|1] [timeout_ms] [idle_before_read_ms]
//   defaults: on12_creator=0 timeout_ms=10000 idle_before_read_ms=0
//   on12_creator=1 creates the queue through ID3D12Device9::CreateCommandQueue1
//   with the D3D12TranslationLayer creator GUID (unsupported: setup error).
//
//   d3d12_readback_idle_fence-x86.exe 1 128 672 1 10000 0      no-idle control
//   d3d12_readback_idle_fence-x86.exe 1 128 672 1 10000 1200   past the 1 s safe-mode lifetime
//   d3d12_readback_idle_fence-x86.exe 1 128 672 1 10000 6000   past the 5 s host lifetime
//
// The process prints the path of the loaded Helios D3D12 UMD; a registry
// change to the UMD path takes effect only after the display adapter restarts.

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d12.h>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

namespace {

constexpr UINT kWidth = 1024;
constexpr UINT kHeight = 2048;
constexpr UINT kPixelBytes = 4;

enum {
    kExitPass = 0,
    kExitUsage = 2,
    kExitSetup = 3,
    kExitWait = 4,
    kExitCorruption = 5,
};

#define CHECK_HR(expr)                                                             \
    do {                                                                           \
        HRESULT hr_ = (expr);                                                      \
        if (FAILED(hr_)) {                                                         \
            std::printf("SETUP_ERROR line=%d hr=0x%08lx %s\n", __LINE__,           \
                        static_cast<unsigned long>(hr_), #expr);                   \
            return kExitSetup;                                                     \
        }                                                                          \
    } while (0)

struct RunParams {
    unsigned rounds = 1;
    unsigned copies = 1;
    unsigned warmup = 0;
    bool on12_creator = false;
    DWORD timeout_ms = 10000;
    // CPU idle gap inserted once per round immediately before the readback
    // command list submission. 0 = no Sleep call, original timing preserved.
    DWORD idle_before_read_ms = 0;
};

struct WaitOutcome {
    bool event_registered = false;   // SetEventOnCompletion() was used
    bool device_removed = false;     // GetCompletedValue() == UINT64_MAX
    bool set_event_failed = false;
    UINT64 completed_precheck = 0;   // GetCompletedValue() before registration
    DWORD status = WAIT_OBJECT_0;    // WaitForSingleObject() result
};

struct NativeSample {
    UINT64 value = 0;
    bool device_removed = false;
};

struct CompareResult {
    size_t bad = 0;
    size_t zeros = 0;
    size_t first = static_cast<size_t>(-1);
};

struct Objects {
    ID3D12Device* device = nullptr;
    ID3D12CommandQueue* queue = nullptr;
    ID3D12CommandQueue* sync_queue = nullptr;
    ID3D12Fence* fence = nullptr;
    HANDLE event = nullptr;
    ID3D12Resource* texture = nullptr;
    ID3D12Resource* zero = nullptr;
    ID3D12Resource* staging = nullptr;
    std::vector<ID3D12CommandAllocator*> allocators;
    std::vector<ID3D12GraphicsCommandList*> lists;
    std::vector<ID3D12Resource*> readbacks;

    // Only release when every submitted fence point is known complete. On a
    // failed or incomplete wait the GPU may still be writing these resources;
    // releasing or resetting them then would manufacture a use-after-free on
    // top of the behaviour under investigation, so we deliberately leak and
    // let process exit reclaim the address space.
    bool drained = false;

    ~Objects() {
        if (!drained) {
            std::printf("CLEANUP_SKIPPED reason=pending_or_unproven_gpu_work "
                        "(COM objects intentionally not released)\n");
            return;
        }
        for (ID3D12GraphicsCommandList* list : lists) list->Release();
        for (ID3D12CommandAllocator* alloc : allocators) alloc->Release();
        for (ID3D12Resource* res : readbacks) res->Release();
        if (staging) staging->Release();
        if (zero) zero->Release();
        if (texture) texture->Release();
        if (event) CloseHandle(event);
        if (fence) fence->Release();
        if (queue) queue->Release();
        if (sync_queue) sync_queue->Release();
        if (device) device->Release();
    }
};

D3D12_RESOURCE_DESC buffer_desc(UINT64 bytes) {
    D3D12_RESOURCE_DESC desc = {};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    desc.Width = bytes;
    desc.Height = 1;
    desc.DepthOrArraySize = 1;
    desc.MipLevels = 1;
    desc.SampleDesc.Count = 1;
    desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    return desc;
}

void transition(ID3D12GraphicsCommandList* list, ID3D12Resource* resource,
                D3D12_RESOURCE_STATES before, D3D12_RESOURCE_STATES after) {
    D3D12_RESOURCE_BARRIER barrier = {};
    barrier.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    barrier.Flags = D3D12_RESOURCE_BARRIER_FLAG_NONE;
    barrier.Transition.pResource = resource;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = before;
    barrier.Transition.StateAfter = after;
    list->ResourceBarrier(1, &barrier);
}

// Byte pattern for the pixels the CPU writes during the critical window.
// Never zero, so "zeros" in a mismatch report is meaningful, and different per
// round, so a stale round cannot masquerade as a fresh one.
inline unsigned char pattern_byte(size_t index, unsigned round) {
    return static_cast<unsigned char>((index * 31u + (index >> 12) * 17u + round * 37u) % 251u + 1u);
}

// buffer_bytes covers full rows including the last row's padding, so writing
// the poison byte beyond the visible pixels is always inside our allocation
// even when GetCopyableFootprints() omits final-row padding from TotalBytes.
void fill_expected(std::vector<unsigned char>& buffer, UINT row_pitch, unsigned round) {
    const UINT row_bytes = kWidth * kPixelBytes;
    for (UINT y = 0; y < kHeight; ++y) {
        unsigned char* row = buffer.data() + static_cast<size_t>(y) * row_pitch;
        if (row_pitch > row_bytes) {
            std::memset(row + row_bytes, 0xCD, row_pitch - row_bytes);
        }
        for (UINT x = 0; x < row_bytes; ++x) {
            row[x] = pattern_byte(static_cast<size_t>(y) * row_pitch + x, round);
        }
    }
}

CompareResult compare_pixels(const unsigned char* data, UINT row_pitch,
                             const std::vector<unsigned char>& expected) {
    CompareResult result;
    const UINT row_bytes = kWidth * kPixelBytes;
    for (UINT y = 0; y < kHeight; ++y) {
        const unsigned char* got = data + static_cast<size_t>(y) * row_pitch;
        const unsigned char* exp = expected.data() + static_cast<size_t>(y) * row_pitch;
        for (UINT x = 0; x < row_bytes; ++x) {
            if (got[x] == 0) result.zeros++;
            if (got[x] != exp[x]) {
                if (result.first == static_cast<size_t>(-1)) {
                    result.first = static_cast<size_t>(y) * row_pitch + x;
                }
                result.bad++;
            }
        }
    }
    return result;
}

NativeSample sample_native(ID3D12Fence* fence) {
    NativeSample sample;
    sample.value = fence->GetCompletedValue();
    // UINT64_MAX is the documented device-removal sentinel, not a value that
    // can be reached by real progress. Never treat it as completion.
    sample.device_removed = (sample.value == UINT64_MAX);
    return sample;
}

// Exact ordering of Microsoft's wait, with a bounded timeout.
// Returns with *nothing* called after the wait: the caller's very next
// statement is the CPU memcpy in the reproduction mode.
WaitOutcome wait_ms_style(ID3D12Fence* fence, HANDLE event, UINT64 target, DWORD timeout_ms) {
    WaitOutcome outcome;
    const NativeSample precheck = sample_native(fence);
    outcome.completed_precheck = precheck.value;
    if (precheck.device_removed) {
        outcome.device_removed = true;
        outcome.status = WAIT_FAILED;
        return outcome;
    }
    if (outcome.completed_precheck >= target) {
        return outcome;  // CommandListManager fast path: no event involved
    }
    HRESULT hr = fence->SetEventOnCompletion(target, event);
    if (FAILED(hr)) {
        outcome.set_event_failed = true;
        outcome.status = WAIT_FAILED;
        return outcome;
    }
    outcome.event_registered = true;
    outcome.status = WaitForSingleObject(event, timeout_ms);
    return outcome;
}

bool drain(ID3D12Fence* fence, HANDLE event, UINT64 target, DWORD timeout_ms) {
    const WaitOutcome outcome = wait_ms_style(fence, event, target, timeout_ms);
    if (outcome.status != WAIT_OBJECT_0 || outcome.device_removed) return false;
    const NativeSample done = sample_native(fence);
    if (done.device_removed) return false;
    return done.value >= target;
}

bool parse_u32(const char* text, unsigned lo, unsigned hi, unsigned& out) {
    if (!text || !*text) return false;
    char* end = nullptr;
    errno = 0;
    unsigned long value = std::strtoul(text, &end, 0);
    if (end == text || *end != '\0' || errno != 0) return false;
    if (value < lo || value > hi) return false;
    out = static_cast<unsigned>(value);
    return true;
}

void usage(const char* exe) {
    std::printf(
        "usage: %s <rounds:1-64> <readback_copies:1-256> <warmup_batches:0-1024>\n"
        "          [on12_creator:0|1] [timeout_ms:100-120000]\n"
        "          [idle_before_read_ms:0-15000]\n"
        "  rounds         lock/write/upload/validate cycles (>=1)\n"
        "  readback_copies image->staging copies per cycle, each pair separated by\n"
        "                 COPY_DEST/COPY_SOURCE transitions; 1 = baseline control\n"
        "  warmup_batches zero-fill batches queued before the first cycle (not drained)\n"
        "  on12_creator   1 = create the queue with the D3D12TranslationLayer creator\n"
        "                 GUID via ID3D12Device9::CreateCommandQueue1; unavailable -> error\n"
        "  timeout_ms     bounded wait timeout for every fence wait\n"
        "  idle_before_read_ms  CPU Sleep() once per round immediately before the\n"
        "                 readback list is submitted (after warmup ECLs and after\n"
        "                 both lists are closed; outside the wait/memcpy/upload\n"
        "                 window); 0 = disabled, default. Safe-mode fence expiry is\n"
        "                 expected at 1 s vs 5 s normal; try 0, 1200, 6000\n",
        exe ? exe : "d3d12_readback_idle_fence");
}

int run(const RunParams& params) {
    const UINT64 bytes = static_cast<UINT64>(kWidth) * kHeight * kPixelBytes;

    // ---------------------------------------------------------------- device
    ID3D12Device* device = nullptr;
    CHECK_HR(D3D12CreateDevice(nullptr, D3D_FEATURE_LEVEL_11_0, IID_ID3D12Device,
                               reinterpret_cast<void**>(&device)));

    D3D12_COMMAND_QUEUE_DESC queue_desc = {};
    queue_desc.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
    queue_desc.Priority = D3D12_COMMAND_QUEUE_PRIORITY_NORMAL;
    queue_desc.Flags = D3D12_COMMAND_QUEUE_FLAG_NONE;

    ID3D12CommandQueue* queue = nullptr;
    ID3D12CommandQueue* sync_queue = nullptr;
    if (params.on12_creator) {
        // Creator GUID as used by D3D12TranslationLayer CommandListManager.cpp
        // (from Microsoft DirectX-Headers d3d12compatibility.h).
        static const GUID creator = {
            0xfffcbb7f, 0x15d3, 0x42a2, {0x84, 0x1e, 0x9d, 0x8d, 0x32, 0xf3, 0x7d, 0xdd}};
        CHECK_HR(device->CreateCommandQueue(&queue_desc, IID_ID3D12CommandQueue,
                                           reinterpret_cast<void**>(&sync_queue)));
        ID3D12Device9* newer = nullptr;
        HRESULT hr = device->QueryInterface(IID_ID3D12Device9, reinterpret_cast<void**>(&newer));
        if (FAILED(hr) || !newer) {
            std::printf("SETUP_ERROR on12_creator_unsupported ID3D12Device9 hr=0x%08lx "
                        "(matrix case on12_creator=1 cannot be run on this UMD)\n",
                        static_cast<unsigned long>(hr));
            return kExitSetup;
        }
        hr = newer->CreateCommandQueue1(&queue_desc, creator, IID_ID3D12CommandQueue,
                                        reinterpret_cast<void**>(&queue));
        newer->Release();
        if (FAILED(hr)) {
            std::printf("SETUP_ERROR CreateCommandQueue1 hr=0x%08lx\n",
                        static_cast<unsigned long>(hr));
            return kExitSetup;
        }
    } else {
        CHECK_HR(device->CreateCommandQueue(&queue_desc, IID_ID3D12CommandQueue,
                                           reinterpret_cast<void**>(&queue)));
    }

    ID3D12Fence* fence = nullptr;
    CHECK_HR(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_ID3D12Fence,
                                 reinterpret_cast<void**>(&fence)));
    // Auto-reset event, exactly as CommandListManager.cpp:30.
    HANDLE wait_event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!wait_event) {
        std::printf("SETUP_ERROR CreateEvent gle=%lu\n", static_cast<unsigned long>(GetLastError()));
        return kExitSetup;
    }

    // The Helios D3D12 UMD of this process's architecture (helios_kmd_render.inx).
    const char* const umd_name =
        sizeof(void*) == 8 ? "helios_umd12.dll" : "helios_umd12_32.dll";
    const char* const exe_name = sizeof(void*) == 8 ? "d3d12_readback_idle_fence-x64.exe"
                                                    : "d3d12_readback_idle_fence-x86.exe";
    char module_path[MAX_PATH] = {};
    HMODULE umd = GetModuleHandleA(umd_name);
    if (umd) GetModuleFileNameA(umd, module_path, sizeof(module_path));

    std::printf("UMD=%s rounds=%u copies=%u warmup=%u on12_creator=%d "
                "timeout_ms=%lu idle_before_read_ms=%lu\n",
                umd ? module_path : "(not loaded)", params.rounds,
                params.copies, params.warmup, params.on12_creator ? 1 : 0,
                static_cast<unsigned long>(params.timeout_ms),
                static_cast<unsigned long>(params.idle_before_read_ms));
    std::printf("REPLAY %s %u %u %u %d %lu %lu\n", exe_name, params.rounds, params.copies,
                params.warmup, params.on12_creator ? 1 : 0,
                static_cast<unsigned long>(params.timeout_ms),
                static_cast<unsigned long>(params.idle_before_read_ms));

    // --------------------------------------------------------------- objects
    Objects objects;
    objects.device = device;
    objects.queue = queue;
    objects.sync_queue = sync_queue;
    objects.fence = fence;
    objects.event = wait_event;

    D3D12_RESOURCE_DESC texture_desc = {};
    texture_desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    texture_desc.Width = kWidth;
    texture_desc.Height = kHeight;
    texture_desc.DepthOrArraySize = 1;
    texture_desc.MipLevels = 1;
    texture_desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    texture_desc.SampleDesc.Count = 1;

    D3D12_HEAP_PROPERTIES default_heap = {};
    default_heap.Type = D3D12_HEAP_TYPE_DEFAULT;
    CHECK_HR(device->CreateCommittedResource(
        &default_heap, D3D12_HEAP_FLAG_NONE, &texture_desc,
        D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_ID3D12Resource,
        reinterpret_cast<void**>(&objects.texture)));

    D3D12_PLACED_SUBRESOURCE_FOOTPRINT footprint = {};
    UINT rows = 0;
    UINT64 row_bytes = 0;
    UINT64 total_bytes = 0;
    device->GetCopyableFootprints(&texture_desc, 0, 1, 0, &footprint, &rows, &row_bytes,
                                  &total_bytes);
    const UINT row_pitch = footprint.Footprint.RowPitch;
    if (rows != kHeight || row_pitch < kWidth * kPixelBytes) {
        std::printf("SETUP_ERROR footprint rows=%u pitch=%u need_rows=%u need_pitch=%u\n", rows,
                    row_pitch, kHeight, kWidth * kPixelBytes);
        return kExitSetup;
    }

    // Safe allocation size: TotalBytes may omit the final row's padding, while
    // fill_expected()/memcpy use whole pitched rows. Buffers are therefore
    // created at least row_pitch*kHeight bytes so no CPU write can leave the
    // allocation; only the footprint rows are ever copied.
    UINT64 buffer_bytes = static_cast<UINT64>(row_pitch) * kHeight;
    if (total_bytes > buffer_bytes) buffer_bytes = total_bytes;
    if (buffer_bytes < bytes) {
        std::printf("SETUP_ERROR buffer_bytes=%llu < image_bytes=%llu\n",
                    static_cast<unsigned long long>(buffer_bytes),
                    static_cast<unsigned long long>(bytes));
        return kExitSetup;
    }

    D3D12_RESOURCE_DESC staging_desc = buffer_desc(buffer_bytes);

    // Constant zero source for the warmup fill of the texture.
    D3D12_HEAP_PROPERTIES upload_heap = {};
    upload_heap.Type = D3D12_HEAP_TYPE_UPLOAD;
    CHECK_HR(device->CreateCommittedResource(
        &upload_heap, D3D12_HEAP_FLAG_NONE, &staging_desc,
        D3D12_RESOURCE_STATE_GENERIC_READ, nullptr, IID_ID3D12Resource,
        reinterpret_cast<void**>(&objects.zero)));
    {
        void* zero_map = nullptr;
        D3D12_RANGE empty_range = {0, 0};
        CHECK_HR(objects.zero->Map(0, &empty_range, &zero_map));
        std::memset(zero_map, 0, static_cast<size_t>(buffer_bytes));
        D3D12_RANGE written = {0, static_cast<SIZE_T>(buffer_bytes)};
        objects.zero->Unmap(0, &written);
    }

    // CPU-visible, GPU-writable staging: what the translation layer gets from
    // GetD3D12HeapType(STAGING, CPU_ACCESS_READ) resolved via
    // GetCustomHeapProperties(). Use the device's own answer so the heap really
    // matches the production path on this UMD, and fall back to the explicit
    // CUSTOM/WRITE_BACK/L0 form only if the query says "not CPU accessible".
    D3D12_HEAP_PROPERTIES staging_heap =
        device->GetCustomHeapProperties(0, D3D12_HEAP_TYPE_READBACK);
    bool staging_heap_fallback = false;
    if (staging_heap.CPUPageProperty == D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE) {
        staging_heap = {};
        staging_heap.Type = D3D12_HEAP_TYPE_CUSTOM;
        staging_heap.CPUPageProperty = D3D12_CPU_PAGE_PROPERTY_WRITE_BACK;
        staging_heap.MemoryPoolPreference = D3D12_MEMORY_POOL_L0;
        staging_heap.CreationNodeMask = 1;
        staging_heap.VisibleNodeMask = 1;
        staging_heap_fallback = true;
    }
    std::printf("STAGING_HEAP type=%d cpu_page=%d pool=%d fallback=%d pitch=%u "
                "footprint_total=%llu buffer_bytes=%llu\n",
                static_cast<int>(staging_heap.Type),
                static_cast<int>(staging_heap.CPUPageProperty),
                static_cast<int>(staging_heap.MemoryPoolPreference),
                staging_heap_fallback ? 1 : 0, row_pitch,
                static_cast<unsigned long long>(total_bytes),
                static_cast<unsigned long long>(buffer_bytes));

    CHECK_HR(device->CreateCommittedResource(
        &staging_heap, D3D12_HEAP_FLAG_NONE, &staging_desc, D3D12_RESOURCE_STATE_COPY_DEST,
        nullptr, IID_ID3D12Resource, reinterpret_cast<void**>(&objects.staging)));

    void* mapped = nullptr;
    {
        // Mapped once for the whole run: no Map/Unmap calls inside the
        // critical wait -> write -> upload window.
        D3D12_RANGE all = {0, static_cast<SIZE_T>(buffer_bytes)};
        CHECK_HR(objects.staging->Map(0, &all, &mapped));
    }

    D3D12_TEXTURE_COPY_LOCATION image = {};
    image.pResource = objects.texture;
    image.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
    image.SubresourceIndex = 0;

    D3D12_TEXTURE_COPY_LOCATION staging_span = {};
    staging_span.pResource = objects.staging;
    staging_span.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
    staging_span.PlacedFootprint = footprint;

    D3D12_TEXTURE_COPY_LOCATION zero_span = staging_span;
    zero_span.pResource = objects.zero;

    // ------------------------------------------------------- command helpers
    auto make_list = [&objects](D3D12_COMMAND_LIST_TYPE type) -> ID3D12GraphicsCommandList* {
        ID3D12CommandAllocator* allocator = nullptr;
        if (FAILED(objects.device->CreateCommandAllocator(
                type, IID_ID3D12CommandAllocator, reinterpret_cast<void**>(&allocator)))) {
            return nullptr;
        }
        objects.allocators.push_back(allocator);
        ID3D12GraphicsCommandList* list = nullptr;
        if (FAILED(objects.device->CreateCommandList(
                0, type, allocator, nullptr, IID_ID3D12GraphicsCommandList,
                reinterpret_cast<void**>(&list)))) {
            return nullptr;
        }
        objects.lists.push_back(list);
        return list;
    };
    auto submit = [&objects](ID3D12GraphicsCommandList* list) -> HRESULT {
        HRESULT hr = list->Close();
        if (FAILED(hr)) return hr;
        ID3D12CommandList* commands[] = {list};
        objects.queue->ExecuteCommandLists(1, commands);
        return S_OK;
    };

    UINT64 point = 0;

    // ---------------------------------------------------------------- warmup
    // Zero-fill the texture so any lost readback shows up as zeros, and leave
    // the work in flight: the first readback is queued behind it, exactly like
    // a font lock that happens while earlier frames are still executing.
    for (unsigned i = 0; i <= params.warmup; ++i) {
        ID3D12GraphicsCommandList* list = make_list(queue_desc.Type);
        if (!list) {
            std::printf("SETUP_ERROR CreateCommandList warmup\n");
            return kExitSetup;
        }
        if (i) {
            transition(list, objects.texture, D3D12_RESOURCE_STATE_COPY_SOURCE,
                       D3D12_RESOURCE_STATE_COPY_DEST);
        }
        list->CopyTextureRegion(&image, 0, 0, 0, &zero_span, nullptr);
        transition(list, objects.texture, D3D12_RESOURCE_STATE_COPY_DEST,
                   D3D12_RESOURCE_STATE_COPY_SOURCE);
        CHECK_HR(submit(list));
        CHECK_HR(objects.queue->Signal(objects.fence, ++point));
        // Translation-layer presents can signal the queue without recording a
        // command list; keep that fence-value shape.
        if (i && i % 2 == 0) CHECK_HR(objects.queue->Signal(objects.fence, ++point));
    }

    // ----------------------------------------------------------------- rounds
    std::vector<unsigned char> expected(static_cast<size_t>(buffer_bytes));
    unsigned rounds_with_mismatch = 0;
    unsigned rounds_with_early_wait = 0;
    unsigned rounds_with_wait_failure = 0;

    for (unsigned round = 0; round < params.rounds; ++round) {
        const NativeSample removed = sample_native(objects.fence);
        if (removed.device_removed) {
            std::printf("SETUP_ERROR device_removed (fence=UINT64_MAX) round=%u\n", round + 1);
            return kExitSetup;
        }
        HRESULT removed_reason = objects.device->GetDeviceRemovedReason();
        if (FAILED(removed_reason)) {
            std::printf("SETUP_ERROR device_removed hr=0x%08lx round=%u\n",
                        static_cast<unsigned long>(removed_reason), round + 1);
            return kExitSetup;
        }

        // Everything that can be prepared is prepared before the critical
        // interval: pattern, readback resource, and both command lists --
        // including the upload/verification list, which is recorded and closed
        // here and only executed after the CPU write.
        fill_expected(expected, row_pitch, round);

        ID3D12Resource* readback = nullptr;
        D3D12_HEAP_PROPERTIES readback_heap = {};
        readback_heap.Type = D3D12_HEAP_TYPE_READBACK;
        CHECK_HR(device->CreateCommittedResource(
            &readback_heap, D3D12_HEAP_FLAG_NONE, &staging_desc,
            D3D12_RESOURCE_STATE_COPY_DEST, nullptr, IID_ID3D12Resource,
            reinterpret_cast<void**>(&readback)));
        objects.readbacks.push_back(readback);

        ID3D12GraphicsCommandList* read_list = make_list(queue_desc.Type);
        ID3D12GraphicsCommandList* upload_list = make_list(queue_desc.Type);
        if (!read_list || !upload_list) {
            std::printf("SETUP_ERROR CreateCommandList round=%u\n", round + 1);
            return kExitSetup;
        }

        // --- pre-recorded upload + independent verification, executed later.
        // Recorded now, closed now, so the critical window after the wait only
        // contains the memcpy and then ExecuteCommandLists + Signal.
        D3D12_TEXTURE_COPY_LOCATION result_span = staging_span;
        result_span.pResource = readback;
        transition(upload_list, objects.staging, D3D12_RESOURCE_STATE_COPY_DEST,
                   D3D12_RESOURCE_STATE_COPY_SOURCE);
        transition(upload_list, objects.texture, D3D12_RESOURCE_STATE_COPY_SOURCE,
                   D3D12_RESOURCE_STATE_COPY_DEST);
        upload_list->CopyTextureRegion(&image, 0, 0, 0, &staging_span, nullptr);
        transition(upload_list, objects.texture, D3D12_RESOURCE_STATE_COPY_DEST,
                   D3D12_RESOURCE_STATE_COPY_SOURCE);
        upload_list->CopyTextureRegion(&result_span, 0, 0, 0, &image, nullptr);
        transition(upload_list, objects.staging, D3D12_RESOURCE_STATE_COPY_SOURCE,
                   D3D12_RESOURCE_STATE_COPY_DEST);
        CHECK_HR(upload_list->Close());

        const UINT64 read_point_before_signal = point;
        const NativeSample completed_at_start = sample_native(objects.fence);
        if (completed_at_start.device_removed) {
            std::printf("SETUP_ERROR device_removed (fence=UINT64_MAX) round=%u\n", round + 1);
            return kExitSetup;
        }

        // --- readback: image -> CPU-visible staging. Repeated copies are
        // separated by explicit COPY_DEST -> COPY_SOURCE -> COPY_DEST
        // transitions, mirroring the in-game diagnostic's TRANSFER_WRITE ->
        // TRANSFER_WRITE barrier between every repeated I2B copy. No
        // overlapping writes: each copy fully precedes the next.
        for (unsigned copy = 0; copy < params.copies; ++copy) {
            if (copy) {
                transition(read_list, objects.staging, D3D12_RESOURCE_STATE_COPY_DEST,
                           D3D12_RESOURCE_STATE_COPY_SOURCE);
                transition(read_list, objects.staging, D3D12_RESOURCE_STATE_COPY_SOURCE,
                           D3D12_RESOURCE_STATE_COPY_DEST);
            }
            read_list->CopyTextureRegion(&staging_span, 0, 0, 0, &image, nullptr);
        }

        // --- legal CPU idle gap before new GPU work. upload_list is already
        // recorded and closed above; read_list is fully recorded above and is
        // closed by submit() immediately after this idle. The warmup ECLs were
        // submitted earlier, and nothing is drained here. The sleep is outside
        // the wait -> memcpy -> upload critical window and no fence sample is
        // taken during it. 0 means the call is skipped, which preserves the
        // original candidate's timing exactly.
        if (params.idle_before_read_ms) {
            Sleep(params.idle_before_read_ms);
        }

        CHECK_HR(submit(read_list));
        CHECK_HR(objects.queue->Signal(objects.fence, ++point));
        const UINT64 read_point = point;

        // --- critical window starts here --------------------------------
        LARGE_INTEGER window_begin = {};
        QueryPerformanceCounter(&window_begin);

        const WaitOutcome wait =
            wait_ms_style(objects.fence, wait_event, read_point, params.timeout_ms);

        // The only operation between the wait returning and the
        // ExecuteCommandLists below is this memcpy. Nothing can observe or
        // "repair" an early event before the CPU data is in place.
        if (wait.status == WAIT_OBJECT_0) {
            std::memcpy(mapped, expected.data(), static_cast<size_t>(buffer_bytes));
        }

        if (wait.device_removed) {
            std::printf("SETUP_ERROR device_removed_during_wait round=%u\n", round + 1);
            return kExitSetup;
        }
        if (wait.set_event_failed || wait.status != WAIT_OBJECT_0) {
            rounds_with_wait_failure++;
            std::printf("ROUND %u FAIL wait read_target=%llu status=%lu gle=%lu "
                        "set_event_failed=%d timeout_ms=%lu\n",
                        round + 1, static_cast<unsigned long long>(read_point),
                        static_cast<unsigned long>(wait.status),
                        static_cast<unsigned long>(GetLastError()),
                        wait.set_event_failed ? 1 : 0,
                        static_cast<unsigned long>(params.timeout_ms));
            std::printf("RESULT WAIT_FAILURE round=%u phase=readback\n", round + 1);
            return kExitWait;
        }

        // Execute the pre-recorded upload list. Nothing but these two CPU-side
        // driver calls separates the memcpy from the staging -> image copy.
        {
            ID3D12CommandList* commands[] = {upload_list};
            objects.queue->ExecuteCommandLists(1, commands);
        }
        CHECK_HR(objects.queue->Signal(objects.fence, ++point));
        const UINT64 verify_point = point;

        // Clock read is kept out of the wait -> memcpy path entirely; the
        // measured interval is wait + CPU write + upload submit, not a wait
        // latency.
        LARGE_INTEGER window_end = {};
        QueryPerformanceCounter(&window_end);
        LARGE_INTEGER frequency = {};
        QueryPerformanceFrequency(&frequency);
        const double wait_write_submit_us =
            frequency.QuadPart ? 1e6 * static_cast<double>(window_end.QuadPart -
                                                          window_begin.QuadPart) /
                                     static_cast<double>(frequency.QuadPart)
                               : 0.0;

        // --- sampling, strictly AFTER the CPU write and the upload submit.
        const NativeSample native_after_write = sample_native(objects.fence);
        if (native_after_write.device_removed) {
            std::printf("SETUP_ERROR device_removed_after_write round=%u\n", round + 1);
            return kExitSetup;
        }

        const bool early_wait =
            wait.event_registered && native_after_write.value < read_point;
        if (early_wait) rounds_with_early_wait++;

        // --- verification wait -------------------------------------------
        const WaitOutcome verify =
            wait_ms_style(objects.fence, wait_event, verify_point, params.timeout_ms);
        if (verify.device_removed) {
            std::printf("SETUP_ERROR device_removed_during_verify round=%u\n", round + 1);
            return kExitSetup;
        }
        if (verify.status != WAIT_OBJECT_0) {
            rounds_with_wait_failure++;
            std::printf("ROUND %u FAIL verify_wait verify_target=%llu status=%lu\n",
                        round + 1, static_cast<unsigned long long>(verify_point),
                        static_cast<unsigned long>(verify.status));
            std::printf("RESULT WAIT_FAILURE round=%u phase=verify\n", round + 1);
            return kExitWait;
        }
        const NativeSample verify_sample = sample_native(objects.fence);
        if (verify_sample.device_removed) {
            std::printf("SETUP_ERROR device_removed_during_verify round=%u\n", round + 1);
            return kExitSetup;
        }
        // The UMD-reported counter reaching the target is only the API contract
        // we are investigating, not independent proof of physical completion.
        // Failing on it can therefore only produce a conservative verdict.
        if (verify_sample.value < verify_point) {
            rounds_with_wait_failure++;
            std::printf("ROUND %u FAIL verify_wait_incomplete verify_target=%llu "
                        "verify_done=%llu\n",
                        round + 1, static_cast<unsigned long long>(verify_point),
                        static_cast<unsigned long long>(verify_sample.value));
            std::printf("RESULT WAIT_FAILURE round=%u phase=verify_incomplete\n", round + 1);
            return kExitWait;
        }

        void* result_map = nullptr;
        D3D12_RANGE read_range = {0, static_cast<SIZE_T>(buffer_bytes)};
        CHECK_HR(readback->Map(0, &read_range, &result_map));
        CompareResult cmp =
            compare_pixels(static_cast<const unsigned char*>(result_map), row_pitch, expected);

        std::printf(
            "ROUND %u copies=%u pending_before=%llu read_target=%llu "
            "precheck=%llu wait_path=%s wait_status=%lu wait_write_submit_us=%.1f "
            "native_done_after_write=%llu early_wait=%d verify_target=%llu "
            "verify_done=%llu bad=%llu zeros=%llu first=%lld\n",
            round + 1, params.copies,
            static_cast<unsigned long long>(read_point_before_signal -
                                            completed_at_start.value),
            static_cast<unsigned long long>(read_point),
            static_cast<unsigned long long>(wait.completed_precheck),
            wait.event_registered ? "event" : "already_complete",
            static_cast<unsigned long>(wait.status), wait_write_submit_us,
            static_cast<unsigned long long>(native_after_write.value), early_wait ? 1 : 0,
            static_cast<unsigned long long>(verify_point),
            static_cast<unsigned long long>(verify_sample.value),
            static_cast<unsigned long long>(cmp.bad),
            static_cast<unsigned long long>(cmp.zeros),
            static_cast<long long>(cmp.first));
        if (cmp.bad) {
            rounds_with_mismatch++;
            // Forensics only, and only after the verdict data is captured. A
            // late GPU write into the staging buffer is supporting evidence
            // for an early event; the verdict does not depend on it.
            Sleep(250);
            CompareResult staging_after = compare_pixels(static_cast<const unsigned char*>(mapped),
                                                         row_pitch, expected);
            CompareResult result_after =
                compare_pixels(static_cast<const unsigned char*>(result_map), row_pitch, expected);
            const NativeSample done_later = sample_native(objects.fence);
            std::printf(
                "FORENSICS round=%u staging_bad_after_250ms=%llu staging_first=%lld "
                "readback_bad_after_250ms=%llu later_native_done=%llu read_target=%llu\n",
                round + 1, static_cast<unsigned long long>(staging_after.bad),
                static_cast<long long>(staging_after.first),
                static_cast<unsigned long long>(result_after.bad),
                static_cast<unsigned long long>(done_later.value),
                static_cast<unsigned long long>(read_point));
            std::printf("RESULT DATA_CORRUPTION round=%u early_wait=%d\n", round + 1,
                        early_wait ? 1 : 0);
            return kExitCorruption;
        }
        if (early_wait) {
            // The pixel oracle passed but the event provably fired before the
            // tracked point was reached. That is still a wait failure.
            std::printf("RESULT EARLY_WAIT round=%u read_target=%llu "
                        "native_done_after_write=%llu\n",
                        round + 1, static_cast<unsigned long long>(read_point),
                        static_cast<unsigned long long>(native_after_write.value));
            return kExitWait;
        }

        D3D12_RANGE empty_range = {0, 0};
        readback->Unmap(0, &empty_range);
    }

    std::printf("SUMMARY rounds=%u copies=%u warmup=%u on12_creator=%d "
                "timeout_ms=%lu idle_before_read_ms=%lu mismatched=%u early_wait=%u "
                "wait_failure=%u\n",
                params.rounds, params.copies, params.warmup, params.on12_creator ? 1 : 0,
                static_cast<unsigned long>(params.timeout_ms),
                static_cast<unsigned long>(params.idle_before_read_ms),
                rounds_with_mismatch, rounds_with_early_wait, rounds_with_wait_failure);

    // Bounded drain before any Release and before any PASS verdict. Every
    // object is still reachable here; a failed drain must not look like success
    // and must not release in-flight work.
    if (!drain(objects.fence, wait_event, point, params.timeout_ms)) {
        std::printf("RESULT WAIT_FAILURE phase=final_drain final_point=%llu\n",
                    static_cast<unsigned long long>(point));
        return kExitWait;
    }
    objects.drained = true;
    std::puts("RESULT PASS");
    return kExitPass;
}

}  // namespace

int main(int argc, char** argv) {
    setvbuf(stdout, nullptr, _IONBF, 0);

    RunParams params;
    bool ok = (argc >= 4 && argc <= 7);
    ok = ok && parse_u32(argv[1], 1, 64, params.rounds);
    ok = ok && parse_u32(argv[2], 1, 256, params.copies);
    ok = ok && parse_u32(argv[3], 0, 1024, params.warmup);
    unsigned flag = 0;
    if (ok && argc >= 5) {
        ok = parse_u32(argv[4], 0, 1, flag);
        params.on12_creator = flag != 0;
    }
    if (ok && argc >= 6) {
        ok = parse_u32(argv[5], 100, 120000, flag);
        params.timeout_ms = static_cast<DWORD>(flag);
    }
    if (ok && argc >= 7) {
        ok = parse_u32(argv[6], 0, 15000, flag);
        params.idle_before_read_ms = static_cast<DWORD>(flag);
    }
    if (!ok) {
        usage(argc > 0 ? argv[0] : nullptr);
        std::puts("RESULT USAGE");
        return kExitUsage;
    }

    const int code = run(params);
    if (code == kExitSetup) std::puts("RESULT SETUP_ERROR");
    return code;
}
