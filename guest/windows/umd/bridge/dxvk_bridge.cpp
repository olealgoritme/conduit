// Helios UMD <-> DXVK engine bridge implementation.
//
// Wraps DXVK's DxvkInstance/DxvkAdapter/DxvkDevice behind the opaque
// HeliosDxvkDevice. The DXVK engine references a frontend-provided
// `Logger::s_instance` global (normally defined in src/d3d11/d3d11_main.cpp,
// which we do not build) — we provide it here.

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <mmsystem.h>
#include <sddl.h>

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <share.h>
#include <tlhelp32.h>

#include "dxvk_bridge.h"

// ⚠ These two resolve to `umd_common/bridge/`, not to this directory —
// `build.rs` adds it to the include path (`DECISIONS.md` D3b: one copy of the
// source, shared with the D3D12 bridge). `bridge_guard.h` is NOT included here:
// it needs `dxvk::DxvkError` for its engine arm, so it comes after the DXVK
// headers below.
#include "bridge_common.h"
#include "bridge_util.h"

#include "bridge_dxbc.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstring>
#include <memory>
#include <mutex>
#include <optional>
#include <thread>
#include <type_traits>
#include <d3d11.h>
#include <dxgi.h>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

#include "dxvk_instance.h"
#include "dxvk_adapter.h"
#include "dxvk_device.h"
#include "dxvk_fence.h"
#include "../src/util/util_error.h"
#include "dxbc/dxbc_container.h"

// DXVK's full D3D11 COM implementation (built as libhelios_d3d11_static.a). We
// instantiate D3D11DXGIDevice from our DxvkDevice and forward the d3d10umddi DDI
// to ID3D11Device / ID3D11DeviceContext.
#include "d3d11_device.h"
#include "d3d11_context_def.h"
#include "d3d11_texture.h"
#include "d3d11_context_imm.h"
#include "dxvk_helios_feed_trace.h"
#include "dxvk_helios_producer.h"
#include "dxvk_helios_backend.h"

// After the DXVK headers: see the include-order note in this header.
#include "bridge_icd_anchor.h"
#include "bridge_icd_backend.h"
#include "bridge_icd_exports.h"
#include "helios_icd_interface.h"

// ── the shared bridge_guard, with this bridge's one engine-specific arm ──────
//
// `dxvk::DxvkError` is not a `std::exception`, so without this arm every DXVK
// failure would log "unknown exception". The macro is the header's documented
// customization point. Preserve the engine's refusal reason so an invalid
// external-image template can be distinguished from an allocation failure.
// It must be defined AFTER `util_error.h` above, because it is expanded when
// `bridge_guard.h` is preprocessed.
//
// ⛔ Must not allocate — a `std::string` built inside a `std::bad_alloc`
// handler can throw again. DxvkError::message() returns a borrowed const
// reference; assert that contract before formatting into fixed stack storage.
#define HELIOS_BRIDGE_ENGINE_CATCH(what)                          \
  catch (const dxvk::DxvkError& error) {                          \
    static_assert(std::is_same_v<decltype(error.message()), const std::string&>); \
    char msg[512];                                                \
    std::snprintf(msg, sizeof(msg), "%s: DxvkError: %.320s",       \
      (what), error.message().c_str());                           \
    ::helios_bridge::umd_log(msg);                                \
  }
#include "bridge_guard.h"

namespace dxbc_spv::dxbc {
  util::md5::Digest hashDxbcBinary(const void* data, size_t size);
}

namespace dxvk {
  // Frontend-provided global the DXVK engine links against. The string is the
  // log file name DXVK writes engine diagnostics to.
  Logger Logger::s_instance("helios_umd_dxvk.log");
}

namespace helios_bridge {

// Non-dispatchable Vulkan handles are pointer-shaped only in 64-bit builds;
// in x86 they remain uint64_t and must never truncate through uintptr_t.
static std::uint64_t memory_handle_bits(VkDeviceMemory memory) {
#if VK_USE_64_BIT_PTR_DEFINES
  return reinterpret_cast<std::uint64_t>(memory);
#else
  return static_cast<std::uint64_t>(memory);
#endif
}

  // The rotate-sample instrument reads rows as std::uint32_t, so it is only
  // valid against a 32-bit-per-pixel format.
  bool is_32bpp_dxgi_format(DXGI_FORMAT format) {
    switch (format) {
    case DXGI_FORMAT_R8G8B8A8_TYPELESS:
    case DXGI_FORMAT_R8G8B8A8_UNORM:
    case DXGI_FORMAT_R8G8B8A8_UNORM_SRGB:
    case DXGI_FORMAT_R8G8B8A8_UINT:
    case DXGI_FORMAT_R8G8B8A8_SNORM:
    case DXGI_FORMAT_R8G8B8A8_SINT:
    case DXGI_FORMAT_B8G8R8A8_TYPELESS:
    case DXGI_FORMAT_B8G8R8A8_UNORM:
    case DXGI_FORMAT_B8G8R8A8_UNORM_SRGB:
    case DXGI_FORMAT_B8G8R8X8_TYPELESS:
    case DXGI_FORMAT_B8G8R8X8_UNORM:
    case DXGI_FORMAT_B8G8R8X8_UNORM_SRGB:
    case DXGI_FORMAT_R10G10B10A2_TYPELESS:
    case DXGI_FORMAT_R10G10B10A2_UNORM:
    case DXGI_FORMAT_R10G10B10A2_UINT:
    case DXGI_FORMAT_R11G11B10_FLOAT:
    case DXGI_FORMAT_R16G16_TYPELESS:
    case DXGI_FORMAT_R16G16_FLOAT:
    case DXGI_FORMAT_R16G16_UNORM:
    case DXGI_FORMAT_R16G16_UINT:
    case DXGI_FORMAT_R16G16_SNORM:
    case DXGI_FORMAT_R16G16_SINT:
    case DXGI_FORMAT_R32_TYPELESS:
    case DXGI_FORMAT_R32_FLOAT:
    case DXGI_FORMAT_R32_UINT:
    case DXGI_FORMAT_R32_SINT:
      return true;
    default:
      return false;
    }
  }

  // ⚠ `bridge_log_budget`, `PeriodicStat`, `qpc_elapsed_us` and `ComRelease<T>`
  // moved to `umd_common/bridge/{bridge_common,bridge_util}.h` at stage S1
  // (`DECISIONS.md` D3b) — same namespace, same signatures, so every use site
  // below is unchanged. They are engine-agnostic and the D3D12 bridge gets them
  // from the same file rather than a copy.

  /// The direct-scanout PRIMARY create's QI failure — the one whose silent zero
  /// R401 now reports to the runtime.
  ///
  /// Stays here: it is a DXVK-path counter, not shared machinery.
  std::atomic<std::uint32_t> g_scanoutPrimaryQiFailed{0};

  // Minimal IDXGIAdapter the D3D11DXGIDevice constructor stores (it is not
  // queried during construction — the Dxvk objects are passed directly).
  class HeliosStubAdapter : public IDXGIAdapter {
    std::atomic<ULONG> m_ref{1};
  public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** ppv) override {
      if (!ppv) return E_POINTER;
      if (riid == __uuidof(IUnknown) || riid == __uuidof(IDXGIObject) ||
          riid == __uuidof(IDXGIAdapter)) {
        *ppv = static_cast<IDXGIAdapter*>(this);
        AddRef();
        return S_OK;
      }
      *ppv = nullptr;
      return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++m_ref; }
    ULONG STDMETHODCALLTYPE Release() override {
      ULONG r = --m_ref;
      if (!r) delete this;
      return r;
    }
    HRESULT STDMETHODCALLTYPE SetPrivateData(REFGUID, UINT, const void*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE SetPrivateDataInterface(REFGUID, const IUnknown*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE GetPrivateData(REFGUID, UINT*, void*) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE GetParent(REFIID, void** pp) override { if (pp) *pp = nullptr; return E_NOINTERFACE; }
    HRESULT STDMETHODCALLTYPE EnumOutputs(UINT, IDXGIOutput**) override { return DXGI_ERROR_NOT_FOUND; }
    HRESULT STDMETHODCALLTYPE GetDesc(DXGI_ADAPTER_DESC* d) override { if (d) std::memset(d, 0, sizeof(*d)); return S_OK; }
    HRESULT STDMETHODCALLTYPE CheckInterfaceSupport(REFGUID, LARGE_INTEGER*) override { return DXGI_ERROR_UNSUPPORTED; }
  };
}

namespace helios_bridge {
  // Per-process log path under C:\ProgramData\Helios (see umd_log_path() in
  // lib.rs). The restricted IddCx host process cannot write C:\Windows\Temp, so
  // its DXVK-bridge log lines vanished; ProgramData is standard-user writable and
  // the per-pid name keeps each process's file owned by that process.
  // Magic static, matching `shader_bytecode_dump_path()` in this same anonymous
  // namespace — the lazy `if (path[0] == 0)` form it used was an
  // unsynchronised write to shared storage, and the file already contradicted
  // itself on this point.
  const char* umd_log_file() {
    static const std::string path = [] {
      CreateDirectoryA("C:\\ProgramData\\Helios", nullptr);
      char buf[MAX_PATH] = {};
      _snprintf_s(buf, sizeof(buf), _TRUNCATE,
                  "C:\\ProgramData\\Helios\\umd-%lu.log",
                  (unsigned long)GetCurrentProcessId());
      return std::string(buf);
    }();
    return path.c_str();
  }

  void umd_log(const char* msg) {
    // _fsopen with _SH_DENYNO, NOT fopen_s: fopen_s opens in _SH_SECURE
    // (deny-sharing) mode, and the Rust side holds a persistent handle to the
    // same umd-<pid>.log since e88f2c6 — fopen_s then fails on EVERY call and
    // all bridge logging (incl. rotate-perf telemetry) silently vanishes
    // (found 18th session: DriverStore UMD had the strings, logs had no
    // [dxvk-bridge] lines).
    FILE* f = _fsopen(umd_log_file(), "a", _SH_DENYNO);
    if (f) {
      fprintf(f, "[dxvk-bridge] %s\n", msg);
      fclose(f);
    }
  }

  // ⚠ `bridge_guard` moved to `umd_common/bridge/bridge_guard.h` at stage S1
  // (`DECISIONS.md` D3b), together with the ~30 lines of comment recording why
  // it exists and why its compile-time assert is not optional (commit
  // `ead692e`, the truncation that crash-looped dwm and LogonUI at cold boot).
  // Read it there; the check that it is still the only one is
  // `grep -rnE '^[[:space:]]*static_assert\(' umd/bridge umd12/bridge
  // umd_common/bridge` -> exactly one hit, in that file. ⚠ The anchor is
  // load-bearing: without it this comment counts itself.
  //
  // The DXVK-specific arm stays here, as the header's one customization point.
  // `dxvk::DxvkError` is not a `std::exception`, so without this the generic
  // arms would not name it and every DXVK failure would log "unknown
  // exception". It expands to the identical catch clause the guard had before
  // the move -- S1 is a move, and a behaviour change here would be a defect.
  //
  // ⛔ Must not allocate: a `std::string` built inside a `std::bad_alloc`
  // handler can throw again. That is also why `DxvkError::message()` (returns
  // `std::string`) is not called.

}

using namespace helios_bridge;

// Scan-out row-pitch alignment. The host reconstructs the DWM primary from a
// linear stride, and 256 is the cross-adapter alignment the QEMU fork's
// reconstruction and the KMD's SET_SCANOUT_BLOB both assume. It is NOT a
// hardware requirement of this device and must not be "optimised" to the
// natural row length. R822.
static constexpr std::uint32_t kScanoutPitchAlign = 256u;

// The 32bpp scan-out formats, and the bytes-per-pixel the pitch arithmetic
// needs. Mirrors forward.rs's `matches!(a.Format as u32, 28 | 87 | 88)`:
// R8G8B8A8_UNORM (28), B8G8R8A8_UNORM (87), B8G8R8X8_UNORM (88).
struct ScanoutFormat {
  std::uint32_t dxgiValue;
  std::uint32_t bytesPerPixel;

  static std::optional<ScanoutFormat> from_dxgi(std::uint32_t format) {
    switch (format) {
      case 28u:
      case 87u:
      case 88u:
        return ScanoutFormat{ format, 4u };
      default:
        return std::nullopt;
    }
  }
};

class TimerResolution {
public:
  explicit TimerResolution(bool enabled) noexcept {
    if (enabled) {
      const auto result = timeBeginPeriod(1);
      active = result == TIMERR_NOERROR;
      if (!active)
        record_failure("timeBeginPeriod", result);
    }
  }

  ~TimerResolution() {
    if (active) {
      const auto result = timeEndPeriod(1);
      if (result != TIMERR_NOERROR)
        record_failure("timeEndPeriod", result);
    }
  }

  TimerResolution(const TimerResolution&) = delete;
  TimerResolution& operator=(const TimerResolution&) = delete;

private:
  bool active = false;

  static void record_failure(const char* operation, MMRESULT result) noexcept {
    static std::atomic<std::uint32_t> failures { 0 };
    char message[128];
    std::snprintf(message, sizeof(message),
      "timer-resolution: %s failed code=%u failures=%u", operation,
      unsigned(result), unsigned(failures.fetch_add(1, std::memory_order_relaxed) + 1));
    umd_log(message);
  }
};

struct HeliosDxvkDeviceImpl {
  explicit HeliosDxvkDeviceImpl(bool timer_enabled) : timer_resolution(timer_enabled) { }

  // Declared first so the request outlives all DXVK workers, including failed initialization.
  TimerResolution timer_resolution;
  dxvk::Rc<dxvk::DxvkInstance> instance;
  dxvk::Rc<dxvk::DxvkAdapter>  adapter;
  dxvk::Rc<dxvk::DxvkDevice>   device;
  ID3D11Device*        d3d11   = nullptr; // QI'd from D3D11DXGIDevice; holds it alive
  ID3D11DeviceContext* context = nullptr; // immediate context
  std::uint32_t venus_ctx_id = 0;
  // The ICD under DXVK and its backend-neutral table (helios_icd_interface.h):
  // the Venus ICD's helios_venus_* exports wrapped (venus_icd_api), or NVK's
  // helios_icd_interface_v2.
  helios_bridge::IcdBackend backend = helios_bridge::IcdBackend::Venus;
  helios_icd_api icd = {};
  // WSI borrows its handle only until Present returns. Keep a duplicate for
  // exact object comparison and an imported semaphore for the helper device.
  std::mutex vehicle_semaphore_mutex;
  HANDLE vehicle_semaphore_handle = nullptr;
  dxvk::Rc<dxvk::DxvkFence> vehicle_semaphore;


  // One unnamed, registered timeline per device. Resource publications use
  // exact allocation bindings; the fence signal stays folded into frame work.
  // Initialization enters Vulkan with this lock dropped.
  std::mutex present_order_mutex;
  std::condition_variable present_order_ready;
  // Remaining producer timeline state is guarded by present_order_mutex.
  bool present_fence_initializing = false;
  dxvk::Rc<dxvk::DxvkFence> present_fence;
  std::uint64_t present_value    = 0;
  bool          present_fence_failed = false;

  // Registration failure is terminal; it never permits an unordered read.
  std::uint64_t present_stream_cookie = 0;

  // Flush gate: the CS sequence number the last gate covered (UINT64_MAX:
  // none yet), so an empty flush sends nothing. Guarded by flush_gate_mutex.
  std::mutex flush_gate_mutex;
  std::uint64_t flush_gate_seq = UINT64_MAX;

  // Hand-off ledger (helios_handoff): this device's record, its local
  // timeline and last point. Guarded by flush_gate_mutex.
  std::uint32_t handoff_device = UINT32_MAX;
  bool handoff_failed = false;
  dxvk::Rc<dxvk::DxvkFence> handoff_fence;
  std::uint64_t handoff_value = 0;

  ~HeliosDxvkDeviceImpl() {
    if (vehicle_semaphore_handle) CloseHandle(vehicle_semaphore_handle);
    if (context) context->Release();
    if (d3d11) d3d11->Release();
  }
};

namespace {

  // The body every plain `create_*_shader` forwarder shares.
  //
  // Six bodies were identical apart from one COM interface type, one
  // `ID3D11Device` method and a two-letter dump tag. That is the shape where a
  // fix (a new dump, a changed refusal) lands in five of six and the sixth
  // behaves differently only under the workload that binds that stage.
  //
  // `Create` is a lambda rather than a pointer-to-member-function because the
  // six `ID3D11Device::Create*Shader` overloads have six different out-param
  // types; the lambda pins the pairing at each call site, where it is visible.
  template <typename Iface, typename Create>
  std::size_t create_shader_impl(const HeliosDxvkDeviceImpl* impl,
                                 const char* dump_tag,
                                 const char* name,
                                 const std::uint8_t* code,
                                 std::size_t len,
                                 Create create) {
    if (!impl || !impl->d3d11 || !code || !len)
      return 0;
    Iface* shader = nullptr;
    return bridge_guard(name, std::size_t(0), [&]() -> std::size_t {
      auto bytecode = prepare_shader_bytecode(code, len);
      if (!bytecode)
        return 0;
      dump_shader_bytecode(dump_tag, "raw", code, len);
      dump_shader_bytecode(dump_tag, "wrapped", bytecode.data(), bytecode.len());
      HRESULT hr = create(impl->d3d11, bytecode.data(), bytecode.len(), &shader);
      if (FAILED(hr)) {
        char msg[96];
        std::snprintf(msg, sizeof(msg), "%s returned failure", name);
        umd_log(msg);
        return 0;
      }
      return reinterpret_cast<std::size_t>(shader);
    });
  }

}

// Out-of-line ctor/dtor, defined where HeliosDxvkDeviceImpl is complete so the
// header (and the cxx glue) need no DXVK headers.
HeliosDxvkDevice::HeliosDxvkDevice() noexcept = default;
HeliosDxvkDevice::~HeliosDxvkDevice() = default;

std::size_t HeliosDxvkDevice::d3d11_device_ptr() const {
  return impl ? reinterpret_cast<std::size_t>(impl->d3d11) : 0;
}
std::size_t HeliosDxvkDevice::d3d11_context_ptr() const {
  return impl ? reinterpret_cast<std::size_t>(impl->context) : 0;
}

std::uint32_t HeliosDxvkDevice::venus_context_id() const {
  return impl ? impl->venus_ctx_id : 0;
}

std::uint32_t HeliosDxvkDevice::icd_backend() const {
  return impl ? static_cast<std::uint32_t>(impl->backend) : 0;
}

namespace {
  // The texture's image and the memory it is bound to, or false.
  bool texture_image_memory(std::size_t d3d11_resource_ptr, VkImage* image,
                            VkDeviceMemory* memory, VkDeviceSize* offset) {
    if (!d3d11_resource_ptr)
      return false;
    auto* texture = dxvk::GetCommonTexture(
      reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr));
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;
    auto img = texture->GetImage();
    auto info = img->storage()->getMemoryInfo();
    *image = img->handle();
    *memory = info.memory;
    *offset = info.offset;
    return *image != VK_NULL_HANDLE && *memory != VK_NULL_HANDLE;
  }
}

// ---- Cross-process hand-off ledger (docs/shared-surfaces.md section 4) -----
//
// One page set per Windows session ("Local\\HeliosHandoffLedger"), shared by
// every process with the Helios UMD. At a hand-off (a flush of a device that
// holds cross-process shared resources) the releaser writes, for each of those
// resources keyed by its KMD resource id, "device record d, point p"; a fence
// worker of the releaser's DXVK device stores `completed = p` into record d
// when the GPU finished everything recorded before the hand-off. A reader of a
// shared image samples the slot when it records the read and its submission
// worker waits until record d's `completed` reaches p (DXVK patch 0007). So the
// releaser never waits, the acquirer waits only for that producer, and nothing
// heads the adapter's queue, on Venus and NVK alike.
namespace helios_handoff {
  constexpr std::uint32_t kMagic   = 0x4C474448u; // 'HDGL'
  constexpr std::uint32_t kDevices = 65536u;      // never reused within a session
  constexpr std::uint32_t kSlots   = 32768u;      // keys, never freed
  constexpr std::uint32_t kProbe   = 64u;
  constexpr std::uint64_t kPointMask = (1ull << 48) - 1ull;

  struct Device {
    std::atomic<std::uint32_t> pid;
    std::uint32_t reserved;
    std::atomic<std::uint64_t> completed;
  };
  struct Slot {
    std::atomic<std::uint32_t> key;
    std::uint32_t reserved;
    std::atomic<std::uint64_t> packed; // device << 48 | point
  };
  struct Table {
    std::atomic<std::uint32_t> magic;
    std::uint32_t version;
    std::atomic<std::uint32_t> next_device;
    std::uint32_t reserved;
    Device devices[kDevices];
    Slot slots[kSlots];
  };
  static_assert(sizeof(Device) == 16 && sizeof(Slot) == 16, "ledger layout");

  Table* table() {
    static Table* s_table = []() -> Table* {
      // Every principal of the session (DWM runs as its own user) may map it.
      SECURITY_ATTRIBUTES sa = { sizeof(sa), nullptr, FALSE };
      PSECURITY_DESCRIPTOR sd = nullptr;
      if (ConvertStringSecurityDescriptorToSecurityDescriptorW(
            L"D:(A;;GA;;;WD)(A;;GA;;;SY)(A;;GA;;;AC)S:(ML;;NW;;;LW)", SDDL_REVISION_1, &sd, nullptr))
        sa.lpSecurityDescriptor = sd;
      HANDLE mapping = CreateFileMappingW(INVALID_HANDLE_VALUE, sa.lpSecurityDescriptor ? &sa : nullptr,
        PAGE_READWRITE, 0, DWORD(sizeof(Table)), L"Local\\HeliosHandoffLedger");
      if (sd)
        LocalFree(sd);
      if (!mapping)
        return nullptr;
      auto* t = static_cast<Table*>(MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, sizeof(Table)));
      // The view keeps the section alive; the handle is not needed.
      CloseHandle(mapping);
      if (!t)
        return nullptr;
      std::uint32_t zero = 0;
      t->magic.compare_exchange_strong(zero, kMagic);
      if (t->magic.load() != kMagic)
        return nullptr;
      return t;
    }();
    return s_table;
  }

  bool enabled() {
    static const bool on = []() {
      const char* v = std::getenv("HELIOS_HANDOFF_LEDGER");
      return !(v && v[0] == '0');
    }();
    return on;
  }

  std::uint32_t claim_device() {
    Table* t = table();
    if (!t)
      return UINT32_MAX;
    const std::uint32_t d = t->next_device.fetch_add(1);
    if (d >= kDevices)
      return UINT32_MAX;
    t->devices[d].completed.store(0);
    t->devices[d].pid.store(GetCurrentProcessId());
    return d;
  }

  Slot* find(Table* t, std::uint32_t key, bool claim) {
    std::uint32_t h = (key * 2654435761u) % kSlots;
    for (std::uint32_t i = 0; i < kProbe; i++, h = (h + 1) % kSlots) {
      Slot& s = t->slots[h];
      std::uint32_t k = s.key.load(std::memory_order_acquire);
      if (k == key)
        return &s;
      if (k == 0) {
        if (!claim)
          return nullptr;
        if (s.key.compare_exchange_strong(k, key) || k == key)
          return &s;
      }
    }
    return nullptr;
  }

  // dxvk_helios_backend.h hooks.
  std::uint64_t sample(std::uint32_t key) {
    Table* t = table();
    if (!t || !key)
      return 0;
    Slot* s = find(t, key, false);
    if (!s)
      return 0;
    const std::uint64_t packed = s->packed.load(std::memory_order_acquire);
    const std::uint32_t d = std::uint32_t(packed >> 48);
    const std::uint64_t point = packed & kPointMask;
    if (!point || d >= kDevices)
      return 0;
    const Device& dev = t->devices[d];
    if (dev.pid.load(std::memory_order_relaxed) == GetCurrentProcessId())
      return 0; // our own hand-off: same-process order is DXVK's
    if (dev.completed.load(std::memory_order_acquire) >= point)
      return 0;
    return packed;
  }

  bool wait(std::uint32_t, std::uint64_t packed, std::uint64_t timeout_ns) {
    Table* t = table();
    if (!t)
      return true;
    const std::uint32_t d = std::uint32_t(packed >> 48);
    const std::uint64_t point = packed & kPointMask;
    if (d >= kDevices)
      return true;
    const Device& dev = t->devices[d];
    LARGE_INTEGER f, t0, now;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t0);
    for (std::uint32_t spin = 0;; spin++) {
      if (dev.completed.load(std::memory_order_acquire) >= point)
        return true;
      QueryPerformanceCounter(&now);
      const double ns = double(now.QuadPart - t0.QuadPart) * 1e9 / double(f.QuadPart);
      if (ns >= double(timeout_ns)) {
        // A publisher that is gone completes nothing: do not wait for it.
        const DWORD pid = dev.pid.load(std::memory_order_relaxed);
        HANDLE p = pid ? OpenProcess(SYNCHRONIZE, FALSE, pid) : nullptr;
        const bool alive = p && WaitForSingleObject(p, 0) == WAIT_TIMEOUT;
        if (p)
          CloseHandle(p);
        return !alive;
      }
      if (spin < 64)
        YieldProcessor();
      else if (spin < 256)
        SwitchToThread();
      else
        Sleep(1);
    }
  }

  const dxvk::HeliosHandoffHooks kHooks = { &sample, &wait };

  void install_hooks_once() {
    static const bool done = []() {
      if (enabled() && table())
        dxvk::heliosSetHandoffHooks(&kHooks);
      return true;
    }();
    (void)done;
  }
}

namespace {
  // The ledger key of a shared texture: its KMD resource id. An import has it
  // from the open; our own resource asks the ICD (Venus blob / NVK IMPORT_RM
  // id, cached by the ICD) once and keeps it on the image.
  std::uint32_t handoff_key(HeliosDxvkDeviceImpl& d, std::size_t resource) {
    auto* texture = dxvk::GetCommonTexture(reinterpret_cast<ID3D11Resource*>(resource));
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return 0;
    auto image = texture->GetImage();
    std::uint32_t key = image->heliosHandoffKey();
    if (key || !d.icd.memory_res_id || d.device == nullptr)
      return key;
    VkImage vkImage = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    VkDeviceSize offset = 0;
    if (!texture_image_memory(resource, &vkImage, &memory, &offset) || offset != 0)
      return 0;
    std::uint32_t res = 0;
    if (d.icd.memory_res_id(d.device->vkd()->device(), memory, vkImage, &res, nullptr) != VK_SUCCESS)
      return 0;
    image->setHeliosHandoffKey(res);
    return res;
  }
}

void HeliosDxvkDevice::handoff_register(std::size_t d3d11_resource_ptr) const noexcept {
  bridge_guard("handoff_register", false, [&]() -> bool {
    if (!impl || !helios_handoff::enabled())
      return false;
    helios_handoff::install_hooks_once();
    return handoff_key(*impl, d3d11_resource_ptr) != 0;
  });
}

std::int32_t HeliosDxvkDevice::handoff_publish(const std::size_t* resources,
                                               std::uint32_t resource_count) const noexcept {
  if (!impl || !impl->context || impl->device == nullptr || !helios_handoff::enabled())
    return -1;
  return bridge_guard("handoff_publish", std::int32_t(-2), [&]() -> std::int32_t {
    auto* table = helios_handoff::table();
    if (!table)
      return -1;
    helios_handoff::install_hooks_once();
    auto* immediate = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);
    std::lock_guard gate(impl->flush_gate_mutex);
    const std::uint64_t seq = immediate->HeliosFlushSequence();
    if (seq == impl->flush_gate_seq)
      return 0;
    if (impl->handoff_failed)
      return -1; // ledger full: the caller falls back
    if (impl->handoff_fence == nullptr) {
      impl->handoff_device = helios_handoff::claim_device();
      if (impl->handoff_device == UINT32_MAX) {
        impl->handoff_failed = true;
        return -1;
      }
      dxvk::DxvkFenceCreateInfo info = { };
      info.initialValue = 0;
      impl->handoff_fence = impl->device->createFence(info);
    }
    const std::uint64_t point = ++impl->handoff_value;
    if (point > helios_handoff::kPointMask)
      return -1;
    const std::uint64_t packed = (std::uint64_t(impl->handoff_device) << 48) | point;
    std::uint32_t published = 0;
    bool overflow = false;
    for (std::uint32_t i = 0; resources && i < resource_count; i++) {
      const std::uint32_t key = handoff_key(*impl, resources[i]);
      if (!key)
        continue;
      auto* slot = helios_handoff::find(table, key, true);
      if (!slot) {
        overflow = true;
        continue;
      }
      slot->packed.store(packed, std::memory_order_release);
      published++;
    }
    impl->flush_gate_seq = immediate->HeliosSignalFlushPoint(impl->handoff_fence, point);
    auto* device_record = &table->devices[impl->handoff_device];
    impl->handoff_fence->enqueueWait(point, [device_record, point]() {
      std::uint64_t cur = device_record->completed.load(std::memory_order_relaxed);
      while (cur < point && !device_record->completed.compare_exchange_weak(
               cur, point, std::memory_order_release, std::memory_order_relaxed)) { }
    });
    static std::atomic<std::uint64_t> s_handoffs{0};
    const auto n = s_handoffs.fetch_add(1, std::memory_order_relaxed) + 1;
    if (n <= 4 || (n % 4096u) == 0) {
      char msg[192];
      std::snprintf(msg, sizeof(msg),
        "handoff: point %llu of device record %u on %u shared resource(s)%s (#%llu)",
        static_cast<unsigned long long>(point), impl->handoff_device, published,
        overflow ? ", LEDGER FULL for some" : "", static_cast<unsigned long long>(n));
      umd_log(msg);
    }
    return overflow ? -1 : 1;
  });
}


bool HeliosDxvkDevice::get_resource_foreign_identity(
    std::size_t d3d11_resource_ptr,
    std::uint32_t* resource_id,
    std::uint32_t* ctx_id,
    std::uint64_t* size,
    std::uint64_t* modifier,
    std::uint32_t* stride,
    std::uint32_t* offset,
    std::uint32_t* fourcc) const noexcept {
  return bridge_guard("get_resource_foreign_identity", false, [&]() -> bool {
    *resource_id = 0; *ctx_id = 0; *size = 0; *modifier = 0;
    *stride = 0; *offset = 0; *fourcc = 0;
    if (!impl || impl->backend != helios_bridge::IcdBackend::NvkRm
     || !impl->icd.memory_res_id)
      return false;
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    VkDeviceSize memory_offset = 0;
    if (!texture_image_memory(d3d11_resource_ptr, &image, &memory, &memory_offset)
     || memory_offset != 0)
      return false;
    helios_icd_layout layout = {};
    std::uint32_t res = 0;
    const VkResult vr = impl->icd.memory_res_id(
      impl->device->vkd()->device(), memory, image, &res, &layout);
    static std::atomic<std::uint32_t> s_logs{0};
    if (bridge_log_budget(s_logs, 64, 512)) {
      char msg[288];
      std::snprintf(msg, sizeof(msg),
        "nvk resource id: vr=%d res_id=%u %ux%u stride=%u offset=%u fourcc=0x%08x "
        "modifier=0x%016llx size=%llu",
        int(vr), res, layout.width, layout.height, layout.stride, layout.offset,
        layout.fourcc, static_cast<unsigned long long>(layout.modifier),
        static_cast<unsigned long long>(layout.size));
      umd_log(msg);
    }
    if (vr != VK_SUCCESS || !res)
      return false;
    *resource_id = res;
    *ctx_id = impl->icd.ctx_id ? impl->icd.ctx_id(impl->instance->handle()) : 0;
    *size = layout.size;
    *modifier = layout.modifier;
    *stride = layout.stride;
    *offset = layout.offset;
    *fourcc = layout.fourcc;
    return *ctx_id != 0;
  });
}

std::int32_t HeliosDxvkDevice::nvk_scanout_present(
    std::size_t d3d11_resource_ptr) const noexcept {
  return bridge_guard("nvk_scanout_present", std::int32_t(-1), [&]() -> std::int32_t {
    if (!impl || impl->backend != helios_bridge::IcdBackend::NvkRm
     || !impl->icd.scanout_present)
      return -1;
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    VkDeviceSize memory_offset = 0;
    if (!texture_image_memory(d3d11_resource_ptr, &image, &memory, &memory_offset)
     || memory_offset != 0)
      return -2;
    const VkResult vr = impl->icd.scanout_present(
      impl->device->vkd()->device(), memory, image);
    if (vr != VK_SUCCESS) {
      static std::atomic<std::uint32_t> s_fail{0};
      const std::uint32_t n = s_fail.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n <= 8 || (n % 512u) == 0) {
        char msg[128];
        std::snprintf(msg, sizeof(msg), "nvk scanout present failed vr=%d (x%u)", int(vr), n);
        umd_log(msg);
      }
      return -3;
    }
    return 0;
  });
}

std::uint32_t HeliosDxvkDevice::nvk_icd_caps() const noexcept {
  return impl && impl->backend == helios_bridge::IcdBackend::NvkRm ? impl->icd.caps : 0u;
}

namespace {
  // An RM fence for everything submitted to DXVK's graphics queue so far
  // (helios_icd_interface.h queue_rm_fence). The queue is DXVK's: take it the
  // way external submitters must (lockSubmission also drains DXVK's own
  // submission queue first, so the frame's command buffers are in it).
  VkResult queue_rm_fence(const HeliosDxvkDeviceImpl& d, std::uint32_t* fence,
                          std::uint64_t* value) {
    if (d.backend != helios_bridge::IcdBackend::NvkRm || !d.icd.queue_rm_fence
     || !(d.icd.caps & HELIOS_ICD_CAP_RM_FENCE) || d.device == nullptr)
      return VK_ERROR_FEATURE_NOT_PRESENT;
    const VkQueue queue = d.device->queues().graphics.queueHandle;
    d.device->lockSubmission();
    const VkResult vr = d.icd.queue_rm_fence(d.device->vkd()->device(), queue, fence, value);
    d.device->unlockSubmission();
    return vr;
  }
}

std::int32_t HeliosDxvkDevice::nvk_scanout_present_fenced(
    std::size_t d3d11_resource_ptr) const noexcept {
  return bridge_guard("nvk_scanout_present_fenced", std::int32_t(-1), [&]() -> std::int32_t {
    if (!impl || impl->backend != helios_bridge::IcdBackend::NvkRm
     || !impl->icd.scanout_present_fenced || !impl->icd.queue_rm_fence)
      return 1;
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    VkDeviceSize memory_offset = 0;
    if (!texture_image_memory(d3d11_resource_ptr, &image, &memory, &memory_offset)
     || memory_offset != 0)
      return -2;
    std::uint32_t fence = 0;
    std::uint64_t value = 0;
    VkResult vr = queue_rm_fence(*impl, &fence, &value);
    if (vr == VK_ERROR_FEATURE_NOT_PRESENT)
      return 1;
    if (vr != VK_SUCCESS || fence == 0) {
      static std::atomic<std::uint32_t> s_fail{0};
      const std::uint32_t n = s_fail.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n <= 8 || (n % 512u) == 0) {
        char msg[128];
        std::snprintf(msg, sizeof(msg), "nvk queue_rm_fence failed vr=%d (x%u)", int(vr), n);
        umd_log(msg);
      }
      return 1;
    }
    // Takes the fence whatever it answers.
    vr = impl->icd.scanout_present_fenced(impl->device->vkd()->device(), memory, image, fence);
    if (vr != VK_SUCCESS) {
      static std::atomic<std::uint32_t> s_fail{0};
      const std::uint32_t n = s_fail.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n <= 8 || (n % 512u) == 0) {
        char msg[128];
        std::snprintf(msg, sizeof(msg), "nvk scanout_present_fenced failed vr=%d (x%u)",
                      int(vr), n);
        umd_log(msg);
      }
      return -3;
    }
    static std::atomic<std::uint32_t> s_ok{0};
    const std::uint32_t n = s_ok.fetch_add(1, std::memory_order_relaxed) + 1;
    if (n == 1 || (n % 4096u) == 0) {
      char msg[160];
      std::snprintf(msg, sizeof(msg),
        "nvk fenced scanout present #%u (timeline value %llu, %s)", n,
        static_cast<unsigned long long>(value),
        (impl->icd.caps & HELIOS_ICD_CAP_SCANOUT_FENCE_KMD) ? "the KMD flips on the fence"
                                                            : "NVK's flip thread waits");
      umd_log(msg);
    }
    return 0;
  });
}

std::int32_t HeliosDxvkDevice::nvk_present_fence(std::uint32_t* fence_handle,
                                                 std::uint64_t* value) const noexcept {
  return bridge_guard("nvk_present_fence", std::int32_t(1), [&]() -> std::int32_t {
    *fence_handle = 0;
    *value = 0;
    if (!impl)
      return 1;
    const VkResult vr = queue_rm_fence(*impl, fence_handle, value);
    if (vr != VK_SUCCESS || *fence_handle == 0) {
      *fence_handle = 0;
      return 1;
    }
    return 0;
  });
}

void HeliosDxvkDevice::nvk_rm_fence_close(std::uint32_t fence_handle) const noexcept {
  bridge_guard("nvk_rm_fence_close", false, [&]() -> bool {
    if (impl && fence_handle && impl->icd.rm_fence_close && impl->device != nullptr)
      impl->icd.rm_fence_close(impl->device->vkd()->device(), fence_handle);
    return true;
  });
}

namespace {
  // The present stream without a producer allocation (a flush names none):
  // the same one-time registration publish_present_order makes, through the
  // Venus ICD's producer table directly. True when present_fence is live.
  bool ensure_flush_stream(HeliosDxvkDeviceImpl& d) {
    bool initialize = false;
    {
      std::unique_lock lock(d.present_order_mutex);
      while (d.present_fence_initializing)
        d.present_order_ready.wait(lock);
      if (d.present_fence_failed)
        return false;
      if (d.present_fence != nullptr)
        return true;
      d.present_fence_initializing = true;
      initialize = true;
    }
    (void)initialize;
    dxvk::Rc<dxvk::DxvkFence> fence;
    std::uint64_t cookie = 0;
    bool ok = false;
    try {
      // The Venus ICD module from the device's own dispatch, as
      // HeliosProducerBinding resolves it.
      HMODULE module = nullptr;
      const auto entry = d.device->vkd()->vkGetSemaphoreCounterValue;
      if (entry)
        GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
          | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
          reinterpret_cast<LPCWSTR>(entry), &module);
      const auto get = module ? reinterpret_cast<helios_get_producer_api_fn>(
        reinterpret_cast<void*>(GetProcAddress(module, "helios_venus_producer_interface"))) : nullptr;
      helios_producer_api_v1 api = { };
      if (get && get(HELIOS_PRODUCER_ABI, &api) == VK_SUCCESS
       && api.version == HELIOS_PRODUCER_ABI && api.size == sizeof(api) && api.stream) {
        dxvk::DxvkFenceCreateInfo fenceInfo = { };
        fenceInfo.sharedType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
        fence = d.device->createFence(fenceInfo);
        std::uint32_t ctx = 0;
        ok = api.stream(reinterpret_cast<uintptr_t>(d.device->vkd()->device()),
                        std::uint64_t(uintptr_t(fence->handle())), &ctx, &cookie) == VK_SUCCESS
          && ctx == d.venus_ctx_id && cookie != 0;
      }
    } catch (const dxvk::DxvkError&) {
      ok = false;
    }
    {
      std::lock_guard lock(d.present_order_mutex);
      d.present_fence_initializing = false;
      if (ok) {
        d.present_fence = std::move(fence);
        d.present_stream_cookie = cookie;
      } else {
        d.present_fence_failed = true;
      }
    }
    d.present_order_ready.notify_all();
    char msg[160];
    std::snprintf(msg, sizeof(msg), ok
      ? "flush-gate: present stream registered for flush points ctx=%u cookie=%llu"
      : "flush-gate: no present stream (ctx=%u cookie=%llu): flushes use the wire rung",
      d.venus_ctx_id, static_cast<unsigned long long>(cookie));
    umd_log(msg);
    return ok;
  }
}

std::int32_t HeliosDxvkDevice::flush_gate_point(std::uint32_t mode,
                                                const std::size_t* resources,
                                                std::uint32_t resource_count,
                                                std::uint32_t* ctx_id,
                                                std::uint32_t* value32,
                                                std::uint64_t* cookie,
                                                std::uint32_t* fence,
                                                std::uint64_t* fence_value) const noexcept {
  *ctx_id = 0; *value32 = 0; *cookie = 0; *fence = 0; *fence_value = 0;
  if (!impl || !impl->context || impl->device == nullptr)
    return -1;
  return bridge_guard("flush_gate_point", std::int32_t(-2), [&]() -> std::int32_t {
    auto* immediate = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);
    std::lock_guard gate(impl->flush_gate_mutex);
    const std::uint64_t seq = immediate->HeliosFlushSequence();
    if (seq == impl->flush_gate_seq)
      return 0;

    if (mode == kFlushGateStream) {
      if (impl->backend != helios_bridge::IcdBackend::Venus || !impl->venus_ctx_id
       || !ensure_flush_stream(*impl))
        return -1;
      std::uint64_t value = 0;
      dxvk::Rc<dxvk::DxvkFence> streamFence;
      {
        std::lock_guard lock(impl->present_order_mutex);
        if (impl->present_fence_failed || impl->present_fence == nullptr
         || impl->present_value >= UINT32_MAX - 1)
          return -1;
        // The stream's values are shared with present markers: the KMD wants
        // each tag above the last one submitted, so the increment and the
        // recording stay in one order with publish_present_order.
        value = ++impl->present_value;
        streamFence = impl->present_fence;
        // The shared allocations this point is published on (each with an
        // operation the signal's command list retains), so an importer's
        // read waits for this point (flush-gate.md section 9).
        std::vector<dxvk::Rc<dxvk::HeliosProducerBinding>> producers;
        std::vector<dxvk::Rc<dxvk::HeliosProducerOperation>> operations;
        for (std::uint32_t i = 0; resources && i < resource_count; i++) {
          auto* texture = dxvk::GetCommonTexture(reinterpret_cast<ID3D11Resource*>(resources[i]));
          if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
            continue;
          auto producer = texture->GetImage()->storage()->heliosProducer();
          if (producer == nullptr)
            continue;
          bool known = false;
          for (const auto& p : producers)
            known |= p.ptr() == producer.ptr();
          if (known)
            continue;
          producers.push_back(producer);
          operations.push_back(new dxvk::HeliosProducerOperation(producer));
        }
        impl->flush_gate_seq = operations.empty()
          ? immediate->HeliosSignalFlushPoint(streamFence, value)
          : immediate->HeliosSignalFlushPointTracked(streamFence, value, operations);
        static std::atomic<std::uint64_t> s_published{0}, s_publishFailed{0};
        for (const auto& producer : producers) {
          std::uint64_t epoch = 0;
          if (producer->publish(streamFence->handle(), value, &epoch)) {
            s_published.fetch_add(1, std::memory_order_relaxed);
          } else {
            producer->abort();
            const auto n = s_publishFailed.fetch_add(1, std::memory_order_relaxed) + 1;
            if (n <= 8 || (n % 1024u) == 0) {
              char msg[128];
              std::snprintf(msg, sizeof(msg), "flush-gate: publishing point %llu failed (x%llu)",
                static_cast<unsigned long long>(value), static_cast<unsigned long long>(n));
              umd_log(msg);
            }
          }
        }
        const auto published = s_published.load(std::memory_order_relaxed);
        if (!producers.empty() && (published <= 4 || (published % 4096u) == 0)) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "flush-gate: point %llu published on %zu shared allocation(s) (%llu so far)",
            static_cast<unsigned long long>(value), producers.size(),
            static_cast<unsigned long long>(published));
          umd_log(msg);
        }
      }
      *ctx_id = impl->venus_ctx_id;
      *value32 = std::uint32_t(value);
      *cookie = impl->present_stream_cookie;
      return 1;
    }

    if (mode == kFlushGateWire || mode == kFlushGateRmFence) {
      if (mode == kFlushGateRmFence && (impl->backend != helios_bridge::IcdBackend::NvkRm
       || !impl->icd.queue_rm_fence || !(impl->icd.caps & HELIOS_ICD_CAP_RM_FENCE)))
        return -1;
      if (!immediate->HeliosWaitFrameSubmitted())
        return -2;
      impl->flush_gate_seq = seq;
      if (mode == kFlushGateWire)
        return 1;
      const VkResult vr = queue_rm_fence(*impl, fence, fence_value);
      if (vr != VK_SUCCESS || *fence == 0) {
        *fence = 0;
        return -1;
      }
      return 1;
    }
    return -1;
  });
}

void HeliosDxvkDevice::nvk_scanout_release() const noexcept {
  bridge_guard("nvk_scanout_release", false, [&]() -> bool {
    if (impl && impl->backend == helios_bridge::IcdBackend::NvkRm
     && impl->icd.scanout_release && impl->device != nullptr)
      impl->icd.scanout_release(impl->device->vkd()->device());
    return true;
  });
}

std::uint64_t HeliosDxvkDevice::feed_trace_timestamp_ns() const noexcept {
  return dxvk::helios_feed::timestampNs();
}

void HeliosDxvkDevice::feed_trace_render_callback(
    std::uint64_t duration_ns) const noexcept {
  dxvk::helios_feed::umdRenderCallback(duration_ns);
}

void HeliosDxvkDevice::feed_trace_present_callback(
    std::uint64_t duration_ns) const noexcept {
  dxvk::helios_feed::umdPresentCallback(duration_ns);
}

bool HeliosDxvkDevice::recycle_deferred_command_list(
    std::size_t deferred_context_ptr,
    std::size_t command_list_ptr) const noexcept {
  return bridge_guard("recycle_deferred_command_list", false, [&]() -> bool {
    if (!impl || !impl->d3d11 || !deferred_context_ptr || !command_list_ptr)
      return false;

    // The UMD passes only its owned deferred COM context (created through this
    // bridge) and the owned command list its FinishCommandList just returned.
    // Do not probe them with GetType/GetDevice here: those methods AddRef and
    // Release the shared device on every handoff, defeating this hot-path
    // optimization. D3D11CommandList::IsReusableBy is the narrow contract
    // guard for a same-device but wrong-DC handoff.
    auto* context = reinterpret_cast<ID3D11DeviceContext*>(deferred_context_ptr);
    auto* commandList = reinterpret_cast<ID3D11CommandList*>(command_list_ptr);
    return static_cast<dxvk::D3D11DeferredContext*>(context)
      ->RecycleCommandList(static_cast<dxvk::D3D11CommandList*>(commandList));
  });
}

bool HeliosDxvkDevice::enable_deferred_context_ddi_logical_reset(
    std::size_t deferred_context_ptr) const noexcept {
  return bridge_guard("enable_deferred_context_ddi_logical_reset", false, [&]() -> bool {
    if (!impl || !impl->d3d11 || !deferred_context_ptr)
      return false;

    // This is called exactly once, immediately after this bridge created the
    // private deferred context. Do not QI or GetDevice on the hot DDI route.
    auto* context = reinterpret_cast<ID3D11DeviceContext*>(deferred_context_ptr);
    static_cast<dxvk::D3D11DeferredContext*>(context)
      ->EnableHeliosDdiLogicalReset();
    return true;
  });
}

bool HeliosDxvkDevice::set_resource_kmt_handles(
    std::size_t d3d11_resource_ptr,
    std::uint32_t local,
    std::uint32_t global) const noexcept {
  return bridge_guard("set_resource_kmt_handles", false, [&]() -> bool {
    if (!d3d11_resource_ptr || !local)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto image = texture->GetImage();
    // The producer binding (escape 0x13, keyed on a Venus timeline) exists
    // only on Venus; NVK presents are CPU-complete (value-0 markers).
    if (impl->backend == helios_bridge::IcdBackend::Venus) {
      dxvk::Rc<dxvk::HeliosProducerBinding> producer = new dxvk::HeliosProducerBinding(
        impl->device->vkd()->device(), impl->device->vkd()->vkGetSemaphoreCounterValue, local);
      if (!image->storage()->setHeliosProducer(producer))
        return false;
      if (image->heliosStagingImage() != nullptr
       && !image->heliosStagingImage()->storage()->setHeliosProducer(producer))
        return false;
    }
    image->storage()->setKmtHandles(local, global);

    static std::atomic<std::uint32_t> s_setKmtLogs{0};
    if (bridge_log_budget(s_setKmtLogs, 64, 512)) {
      char msg[160];
      std::snprintf(msg, sizeof(msg),
        "set_resource_kmt_handles resource=%p local=0x%08x global=0x%08x",
        resource, local, global);
      umd_log(msg);
    }
    return true;
  });
}

bool HeliosDxvkDevice::get_resource_memory_info(
    std::size_t d3d11_resource_ptr,
    std::uint64_t* memory,
    std::uint64_t* size,
    std::uint64_t* offset,
    std::uint32_t* resource_id) const noexcept {
  return bridge_guard("get_resource_memory_info", false, [&]() -> bool {
    if (memory)
      *memory = 0;
    if (size)
      *size = 0;
    if (offset)
      *offset = 0;
    if (resource_id)
      *resource_id = 0;

    if (!d3d11_resource_ptr)
      return false;
    // The Venus blob id and its exact-size identity have no NVK counterpart:
    // an NVK texture is named by get_resource_foreign_identity instead.
    if (!impl || impl->backend != helios_bridge::IcdBackend::Venus)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto info = texture->GetImage()->storage()->getMemoryInfo();
    const auto rawMemory = memory_handle_bits(info.memory);
    const auto venusId = venus_memory_id_from_handle(info.memory);
    std::uint32_t resourceId = 0;
    if (impl->icd.memory_res_id)
      impl->icd.memory_res_id(impl->device->vkd()->device(), info.memory,
                              texture->GetImage()->handle(), &resourceId, nullptr);
    if (memory)
      *memory = venusId;
    if (size)
      *size = info.size;
    if (offset)
      *offset = info.offset;
    if (resource_id)
      *resource_id = resourceId;

    static std::atomic<std::uint32_t> s_memInfoLogs{0};
    if (bridge_log_budget(s_memInfoLogs, 64, 512)) {
      char msg[256];
      std::snprintf(msg, sizeof(msg),
        "get_resource_memory_info resource=%p memory_raw=0x%llx venus_id=0x%llx res_id=%u size=%llu offset=%llu",
        resource,
        static_cast<unsigned long long>(rawMemory),
        static_cast<unsigned long long>(venusId),
        resourceId,
        static_cast<unsigned long long>(info.size),
        static_cast<unsigned long long>(info.offset));
      umd_log(msg);
    }
    return venusId != 0 && info.size != 0;
  });
}

bool HeliosDxvkDevice::get_resource_alloc_identity(
    std::size_t d3d11_resource_ptr,
    std::uint64_t* venus_alloc_size,
    std::uint32_t* memory_type_index,
    std::uint64_t* global_vidmm_tracker) const noexcept {
  return bridge_guard("get_resource_alloc_identity", false, [&]() -> bool {
    if (venus_alloc_size)
      *venus_alloc_size = 0;
    if (memory_type_index)
      *memory_type_index = 0;
    if (global_vidmm_tracker)
      *global_vidmm_tracker = 0;

    if (!d3d11_resource_ptr)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    if (!impl || impl->backend != helios_bridge::IcdBackend::Venus
     || !impl->icd.memory_alloc_info)
      return false;
    auto info = texture->GetImage()->storage()->getMemoryInfo();
    const bool valid = impl->icd.memory_alloc_info(
      info.memory, venus_alloc_size, memory_type_index) == VK_TRUE;
    if (valid && global_vidmm_tracker) {
      *global_vidmm_tracker =
        venus_memory_vidmm_global_identity_from_handle(info.memory);
    }
    return valid;
  });
}

bool HeliosDxvkDevice::transfer_resource_ownership(
    std::size_t d3d11_resource_ptr) const noexcept {
  return bridge_guard("transfer_resource_ownership", false, [&]() -> bool {
    if (!d3d11_resource_ptr)
      return false;

    auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr);
    auto* texture = dxvk::GetCommonTexture(resource);
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;

    auto info = texture->GetImage()->storage()->getMemoryInfo();
    const auto resourceId = impl && impl->icd.transfer_ownership
      ? impl->icd.transfer_ownership(info.memory) : 0u;

    static std::atomic<std::uint32_t> s_xferOwnLogs{0};
    if (bridge_log_budget(s_xferOwnLogs, 64, 512)) {
      char msg[192];
      std::snprintf(msg, sizeof(msg),
        "transfer_resource_ownership resource=%p memory=0x%llx res_id=%u",
        resource,
        static_cast<unsigned long long>(memory_handle_bits(info.memory)),
        resourceId);
      umd_log(msg);
    }
    return resourceId != 0;
  });
}

std::size_t HeliosDxvkDevice::open_ddi_texture2d(
    std::uint32_t width,
    std::uint32_t height,
    std::uint32_t format,
    std::uint32_t bind_flags,
    std::uint32_t misc_flags,
    std::uint32_t global,
    std::uint32_t renderer_resource_id,
    std::uint64_t venus_alloc_size,
    std::uint32_t memory_type_index,
    std::uint64_t global_vidmm_tracker,
    bool scanout_linear,
    bool linear_scanout_target,
    bool cross_context_optimal,
    bool dedicated_present_buffer,
    std::size_t source_image_create_info,
    bool source_external_ownership,
    bool foreign,
    std::uint64_t foreign_modifier,
    std::uint32_t foreign_stride,
    std::uint32_t foreign_offset) const {
  if (!impl || !impl->d3d11 || !global || !renderer_resource_id || !width || !height)
    return 0;
  bool nvk_blank = false;
  if (impl->backend != helios_bridge::IcdBackend::Venus) {
    // NVK opens another NVK process's surface (a foreign resource: the KMD's
    // layout trailer) by resource id (shared-surfaces.md, NVK patch 0031,
    // DXVK patch 0002). A surface Venus made cannot be imported (no
    // host-Vulkan -> RM direction, dxvk-on-nvk.md 3.7): such apps stay on the
    // deny-list.
    const bool nvk_can_open = foreign
      && (impl->icd.caps & HELIOS_ICD_CAP_SHARED_IMPORT) != 0
      && !scanout_linear && !linear_scanout_target && !source_image_create_info;
    // DWM on NVK (DwmIcd=nvk, docs/dwm-on-nvk.md): a failed open of a window's
    // surface takes DWM down (dwmcore 0x8898008d), and every Venus app's and
    // every KMD-made (GDI, cursor) surface is one NVK cannot import yet. DWM
    // gets a blank texture of the same size instead: that window composes
    // black, the desktop stays up. Any other NVK process still sees the open
    // fail.
    if (!nvk_can_open && helios_bridge::is_dwm_process()) {
      nvk_blank = true;
    } else if (!nvk_can_open) {
      static std::atomic<std::uint32_t> s_nvkOpen{0};
      const std::uint32_t n = s_nvkOpen.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n <= 8 || (n % 512u) == 0) {
        char msg[192];
        std::snprintf(msg, sizeof(msg),
          "OpenDdiTexture2D REFUSED on NVK: res_id=%u foreign=%d icd caps 0x%x (x%u)",
          renderer_resource_id, int(foreign), impl->icd.caps, n);
        umd_log(msg);
      }
      return 0;
    }
  }

  return bridge_guard("open_ddi_texture2d", std::size_t(0), [&]() -> std::size_t {
      {
        static std::atomic<std::uint32_t> s_openBeginLogs{0};
        if (bridge_log_budget(s_openBeginLogs, 64, 512)) {
          char msg[256];
          std::snprintf(msg, sizeof(msg),
            "OpenDdiTexture2D begin %ux%u fmt=%u bind=0x%08x misc=0x%08x global=0x%08x renderer_res=%u alloc_size=%llu mem_type=%u vidmm_identity=0x%016llx",
            width, height, format, bind_flags, misc_flags, global, renderer_resource_id,
            static_cast<unsigned long long>(venus_alloc_size), memory_type_index,
            static_cast<unsigned long long>(global_vidmm_tracker));
          umd_log(msg);
        }
      }

      // A Venus process opening an NVK-made surface with ForeignImport off
      // (the default): the import would throw, and an E_FAIL from this open
      // takes DWM down (dwmcore 0x8898008d when a windowed NVK swap chain is
      // created: DXGI has DWM open the buffers whatever the present path).
      // Hand back an ordinary blank texture of the same size instead: the
      // window composes black, and the NVK app shows its frames on scanout 0
      // as designed (NvkPresent auto picks scanout without ForeignImport).
      if (nvk_blank || (foreign && impl->backend == helios_bridge::IcdBackend::Venus
          && !dxvk::heliosForeignImport())) {
        D3D11_TEXTURE2D_DESC td = { };
        td.Width = width;
        td.Height = height;
        td.MipLevels = 1;
        td.ArraySize = 1;
        td.Format = static_cast<DXGI_FORMAT>(format);
        td.SampleDesc.Count = 1;
        td.Usage = D3D11_USAGE_DEFAULT;
        td.BindFlags = bind_flags & (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET);
        ID3D11Texture2D* placeholder = nullptr;
        HRESULT phr = static_cast<dxvk::D3D11Device*>(impl->d3d11)->CreateTexture2D(
            &td, nullptr, &placeholder);
        ID3D11Resource* res = nullptr;
        if (SUCCEEDED(phr) && placeholder) {
          phr = placeholder->QueryInterface(__uuidof(ID3D11Resource),
                                            reinterpret_cast<void**>(&res));
          placeholder->Release();
        }
        static std::atomic<std::uint32_t> s_placeholders{0};
        const std::uint32_t n = s_placeholders.fetch_add(1, std::memory_order_relaxed) + 1;
        if (n <= 8 || (n % 512u) == 0) {
          char msg[240];
          std::snprintf(msg, sizeof(msg),
            nvk_blank
              ? "OpenDdiTexture2D res_id=%u %ux%u (foreign=%d) on NVK DWM, not importable: blank placeholder hr=0x%08lx (x%u)"
              : "OpenDdiTexture2D foreign res_id=%u %ux%u (foreign=%d) with ForeignImport off: blank placeholder hr=0x%08lx (x%u)",
            renderer_resource_id, width, height, int(foreign), static_cast<unsigned long>(phr), n);
          umd_log(msg);
        }
        return (SUCCEEDED(phr) && res) ? reinterpret_cast<std::size_t>(res) : std::size_t(0);
      }

      dxvk::D3D11_COMMON_TEXTURE_DESC desc = { };
      desc.Width = width;
      desc.Height = height;
      desc.Depth = 1;
      desc.MipLevels = 1;
      desc.ArraySize = 1;
      desc.Format = static_cast<DXGI_FORMAT>(format);
      desc.SampleDesc.Count = 1;
      desc.SampleDesc.Quality = 0;
      desc.Usage = D3D11_USAGE_DEFAULT;
      desc.BindFlags = bind_flags;
      desc.CPUAccessFlags = 0;
      desc.MiscFlags = misc_flags | D3D11_RESOURCE_MISC_SHARED;
      desc.TextureLayout = D3D11_TEXTURE_LAYOUT_UNDEFINED;

      // Typed venus import identity (C1): the resid plus the creator's exact
      // allocation size/memory type from the KMD's open-identity record. The
      // HANDLE parameter still carries the resid value only to select Import
      // mode in the shared-texture path; the import itself reads the typed info.
      dxvk::D3D11_HELIOS_IMPORT_INFO importInfo = { };
      importInfo.ResourceId      = renderer_resource_id;
      importInfo.AllocSize       = venus_alloc_size;
      // memory_type_index 0 is a real venus type; the identity is only recorded
      // as a (size, type) pair, so size == 0 means "no recorded identity" and
      // the type must not be applied as an override either.
      importInfo.MemoryTypeIndex = venus_alloc_size ? memory_type_index : ~0u;
      // Rebuild a flagged primary as the creator's plain LINEAR+DMA_BUF image.
      importInfo.ScanoutLinear   = scanout_linear;
      importInfo.LinearScanoutTarget = linear_scanout_target;
      importInfo.CrossContextOptimal = cross_context_optimal;
      importInfo.DedicatedPresentBuffer = dedicated_present_buffer;
      // Same-thread vehicle v3 borrow. DXVK deep-copies this template and its
      // supported pNext/array data during the synchronous texture constructor.
      importInfo.SourceCreateInfo =
        reinterpret_cast<const VkImageCreateInfo*>(source_image_create_info);
      importInfo.SourceExternalOwnership = source_external_ownership;
      // An NVK-made resource (the KMD's foreign layout trailer): import it as
      // an explicit DRM-modifier image (DXVK patch, ForeignImport knob).
      importInfo.Foreign         = foreign;
      importInfo.ForeignModifier = foreign_modifier;
      importInfo.ForeignStride   = foreign_stride;
      importInfo.ForeignOffset   = foreign_offset;

      // static_cast, matching the sibling context downcast in this file. Zero
      // runtime change today (the base sits at offset 0), but if an upstream DXVK
      // rebase inserts a base class into D3D11Device this becomes a compile error
      // instead of a silently mis-offset `this`. R823.
      auto* device = static_cast<dxvk::D3D11Device*>(impl->d3d11);
      auto* texture = new dxvk::D3D11Texture2D(
          device, &desc, nullptr,
          reinterpret_cast<HANDLE>(static_cast<std::uintptr_t>(renderer_resource_id)),
          &importInfo);

      ID3D11Resource* resource = nullptr;
      HRESULT hr = texture->QueryInterface(
          __uuidof(ID3D11Resource),
          reinterpret_cast<void**>(&resource));

      static std::atomic<std::uint32_t> s_openDoneLogs{0};
      if (bridge_log_budget(s_openDoneLogs, 64, 512)) {
        char msg[224];
        std::snprintf(msg, sizeof(msg),
          "OpenDdiTexture2D %ux%u fmt=%u bind=0x%08x misc=0x%08x global=0x%08x renderer_res=%u hr=0x%08lx resource=%p",
          width, height, format, bind_flags, misc_flags, global, renderer_resource_id,
          static_cast<unsigned long>(hr), resource);
        umd_log(msg);
      }

      if (FAILED(hr) || !resource)
        return 0;

      // The payload WDDM allocation and this imported Venus memory now share
      // one system-wide VidMm tracker. Opening it here gives this process a
      // reference before the creator can close its VkDeviceMemory. A shrunk
      // payload is not safe to expose without that lifetime reference (or the
      // ICD's full-size fallback), so retention failure rejects the open.
      if (global_vidmm_tracker) {
        auto* common = dxvk::GetCommonTexture(resource);
        const bool retained = common && common->GetImage() &&
          common->GetImage()->storage() && venus_memory_open_vidmm_tracker(
            common->GetImage()->storage()->getMemoryInfo().memory,
            global_vidmm_tracker);
        if (!retained) {
          char msg[192];
          std::snprintf(msg, sizeof(msg),
            "OpenDdiTexture2D VidMm tracker open failed renderer_res=%u identity=0x%016llx",
            renderer_resource_id,
            static_cast<unsigned long long>(global_vidmm_tracker));
          umd_log(msg);
          resource->Release();
          return 0;
        }
      }

      return reinterpret_cast<std::size_t>(resource);
  });
}

std::size_t HeliosDxvkDevice::create_ddi_scanout_texture2d(
    std::uint32_t width,
    std::uint32_t height,
    std::uint32_t format,
    std::uint32_t bind_flags,
    std::uint32_t misc_flags,
    bool kmd_transfer_source,
    std::uint64_t* out_row_pitch,
    std::uint64_t* out_offset) const {
  if (out_row_pitch) *out_row_pitch = 0;
  if (out_offset)    *out_offset = 0;
  if (!impl || !impl->d3d11 || !width || !height)
    return 0;

  // R822: the pitch below assumes 4 bytes per pixel, and that was true only
  // because of a check in ANOTHER LANGUAGE -- forward.rs gates the caller on
  // `matches!(a.Format as u32, 28 | 87 | 88)`, while this function accepted any
  // DXGI_FORMAT and static_cast it straight into the texture desc. Validated
  // here as well, so the arithmetic and its precondition live together. The
  // Rust-side check stays as defence in depth.
  const auto scanoutFormat = ScanoutFormat::from_dxgi(format);
  if (!scanoutFormat) {
    static std::atomic<std::uint32_t> s_badFormat{0};
    const std::uint32_t n = s_badFormat.fetch_add(1, std::memory_order_relaxed) + 1;
    if (n <= 8 || (n % 512) == 0) {
      char msg[160];
      std::snprintf(msg, sizeof(msg),
        "CreateDdiScanoutTexture2D REFUSED: fmt=%u is not a 32bpp scan-out "
        "format (x%u)", format, n);
      umd_log(msg);
    }
    return 0;
  }

  return bridge_guard("create_ddi_scanout_texture2d", std::size_t(0), [&]() -> std::size_t {
      {
        static std::atomic<std::uint32_t> s_scanBeginLogs{0};
        if (bridge_log_budget(s_scanBeginLogs, 64, 512)) {
          char msg[224];
          std::snprintf(msg, sizeof(msg),
            "CreateDdiScanoutTexture2D begin %ux%u fmt=%u bind=0x%08x misc=0x%08x kmdTransfer=%u",
            width, height, format, bind_flags, misc_flags,
            kmd_transfer_source ? 1u : 0u);
          umd_log(msg);
        }
      }

      // Build a plain 2D DEFAULT-usage description. The scan-out primary is a
      // device-local render target the host scans out of; sharing is driven by
      // the D3D11_HELIOS_CREATE_INFO marker (Export + DMA_BUF), not by the desc's
      // D3D11 MiscFlags, so we do not force MISC_SHARED here.
      dxvk::D3D11_COMMON_TEXTURE_DESC desc = { };
      desc.Width          = width;
      desc.Height         = height;
      desc.Depth          = 1;
      desc.MipLevels      = 1;
      desc.ArraySize      = 1;
      desc.Format         = static_cast<DXGI_FORMAT>(format);
      desc.SampleDesc.Count   = 1;
      desc.SampleDesc.Quality = 0;
      desc.Usage          = D3D11_USAGE_DEFAULT;
      desc.BindFlags      = bind_flags;
      desc.CPUAccessFlags = 0;
      desc.MiscFlags      = misc_flags;
      desc.TextureLayout  = D3D11_TEXTURE_LAYOUT_UNDEFINED;

      dxvk::D3D11_HELIOS_CREATE_INFO createInfo = { };
      createInfo.DirectOptimalScanout = true;
      createInfo.KmdTransferSource = kmd_transfer_source;

      // static_cast, matching the sibling context downcast in this file. Zero
      // runtime change today (the base sits at offset 0), but if an upstream DXVK
      // rebase inserts a base class into D3D11Device this becomes a compile error
      // instead of a silently mis-offset `this`. R823.
      auto* device = static_cast<dxvk::D3D11Device*>(impl->d3d11);
      // Fresh Export create: no imported vkImage, no shared handle, no import
      // identity. The last argument marks it as the DWM scan-out primary.
      auto* texture = new dxvk::D3D11Texture2D(
          device, &desc, nullptr,
          INVALID_HANDLE_VALUE,
          nullptr,       // pHeliosImport
          &createInfo);  // pHeliosCreate → DirectOptimalScanout

      ID3D11Resource* resource = nullptr;
      HRESULT hr = texture->QueryInterface(
          __uuidof(ID3D11Resource),
          reinterpret_cast<void**>(&resource));
      if (FAILED(hr) || !resource) {
        // Counted, not just logged: `open_ddi_texture2d` has no counter on its
        // equivalent branch either, and a failing QI here returns a silent zero
        // that reads exactly like "no primary was asked for".
        const std::uint32_t n =
          g_scanoutPrimaryQiFailed.fetch_add(1, std::memory_order_relaxed) + 1;
        char qimsg[128];
        std::snprintf(qimsg, sizeof(qimsg),
          "CreateDdiScanoutTexture2D: QI(ID3D11Resource) failed (x%u)", n);
        umd_log(qimsg);
        return 0;
      }

      // Arithmetic DELIBERATELY unchanged: (width * bpp + 255) & ~255 gives 7680
      // for a 1896-wide primary, which is what the frozen host reconstruction
      // expects. What changes is that `bpp` now comes from the validated
      // descriptor instead of a bare 4, and the 256 has a name and a reason.
      const std::uint64_t pitch =
          (std::uint64_t(width) * scanoutFormat->bytesPerPixel + (kScanoutPitchAlign - 1))
          & ~(std::uint64_t(kScanoutPitchAlign) - 1);
      if (out_row_pitch) *out_row_pitch = pitch;
      if (out_offset)    *out_offset = 0;
      static std::atomic<std::uint32_t> s_scanDoneLogs{0};
      if (bridge_log_budget(s_scanDoneLogs, 64, 512)) {
        char msg[192];
        std::snprintf(msg, sizeof(msg),
          "CreateDdiScanoutTexture2D OPTIMAL %ux%u fmt=%u logicalPitch=%llu resource=%p",
          width, height, format, static_cast<unsigned long long>(pitch), resource);
        umd_log(msg);
      }
      return reinterpret_cast<std::size_t>(resource);
  });
}

std::size_t HeliosDxvkDevice::create_vertex_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11VertexShader>(
      impl.get(), "vs", "CreateVertexShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11VertexShader** out) {
        return d->CreateVertexShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_pixel_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11PixelShader>(
      impl.get(), "ps", "CreatePixelShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11PixelShader** out) {
        return d->CreatePixelShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_geometry_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11GeometryShader>(
      impl.get(), "gs", "CreateGeometryShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11GeometryShader** out) {
        return d->CreateGeometryShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_shader_sig(
    std::uint32_t kind,
    const std::uint8_t* code,
    std::size_t len,
    const std::uint32_t* sig_words,
    std::size_t sig_words_len) const {
  if (!impl || !impl->d3d11 || !code || !len || !sig_words || sig_words_len < 2)
    return 0;
  const std::uint32_t n_in = sig_words[0];
  const std::uint32_t n_out = sig_words[1];
  if (!signature_count_ok("create_shader_sig", n_in) ||
      !signature_count_ok("create_shader_sig", n_out))
    return 0;
  // Widen BEFORE adding: `n_in + n_out` was evaluated in std::uint32_t and only
  // then promoted, so a wrapped sum could satisfy the check the indexing below
  // depends on.
  if (sig_words_len != 2 + (std::size_t(n_in) + std::size_t(n_out)) * kSigEntryWords) {
    umd_log("create_shader_sig: signature word count mismatch");
    return 0;
  }
  const std::uint32_t* in_entries = sig_words + 2;
  const std::uint32_t* out_entries = in_entries + std::size_t(n_in) * kSigEntryWords;
  return bridge_guard("create_shader_sig", std::size_t(0), [&]() -> std::size_t {
      auto bytecode = prepare_shader_bytecode_with_sigs(
          code, len, in_entries, n_in, out_entries, n_out);
      if (!bytecode)
        return 0;
      const char* stage = kind == 0 ? "vs-sig" : kind == 1 ? "ps-sig" : "gs-sig";
      dump_shader_bytecode(stage, "raw", code, len);
      dump_shader_bytecode(stage, "wrapped", bytecode.data(), bytecode.len());
      HRESULT hr = E_FAIL;
      void* shader = nullptr;
      switch (kind) {
        case 0:
          hr = impl->d3d11->CreateVertexShader(bytecode.data(), bytecode.len(), nullptr,
                                               reinterpret_cast<ID3D11VertexShader**>(&shader));
          break;
        case 1:
          hr = impl->d3d11->CreatePixelShader(bytecode.data(), bytecode.len(), nullptr,
                                              reinterpret_cast<ID3D11PixelShader**>(&shader));
          break;
        case 2:
          hr = impl->d3d11->CreateGeometryShader(bytecode.data(), bytecode.len(), nullptr,
                                                 reinterpret_cast<ID3D11GeometryShader**>(&shader));
          break;
        default:
          umd_log("create_shader_sig: unknown shader kind");
          return 0;
      }
      if (FAILED(hr)) {
        umd_log("create_shader_sig: shader creation returned failure");
        return 0;
      }
      return reinterpret_cast<std::size_t>(shader);
  });
}

std::size_t HeliosDxvkDevice::create_tess_shader_sig(
    std::uint32_t kind,
    const std::uint8_t* code,
    std::size_t len,
    const std::uint32_t* sig_words,
    std::size_t sig_words_len) const {
  if (!impl || !impl->d3d11 || !code || !len || !sig_words || sig_words_len < 3)
    return 0;
  const std::uint32_t n_in = sig_words[0];
  const std::uint32_t n_out = sig_words[1];
  const std::uint32_t n_patch = sig_words[2];
  if (!signature_count_ok("create_tess_shader_sig", n_in) ||
      !signature_count_ok("create_tess_shader_sig", n_out) ||
      !signature_count_ok("create_tess_shader_sig", n_patch))
    return 0;
  if (sig_words_len !=
      3 + (std::size_t(n_in) + std::size_t(n_out) + std::size_t(n_patch)) * kSigEntryWords) {
    umd_log("create_tess_shader_sig: signature word count mismatch");
    return 0;
  }
  const std::uint32_t* in_entries = sig_words + 3;
  const std::uint32_t* out_entries = in_entries + std::size_t(n_in) * kSigEntryWords;
  const std::uint32_t* patch_entries = out_entries + std::size_t(n_out) * kSigEntryWords;
  return bridge_guard("create_tess_shader_sig", std::size_t(0), [&]() -> std::size_t {
      auto bytecode = prepare_shader_bytecode_with_tess_sigs(
          code, len, in_entries, n_in, out_entries, n_out, patch_entries, n_patch);
      if (!bytecode)
        return 0;
      const char* stage = kind == 0 ? "hs-sig" : "ds-sig";
      dump_shader_bytecode(stage, "raw", code, len);
      dump_shader_bytecode(stage, "wrapped", bytecode.data(), bytecode.len());
      HRESULT hr = E_FAIL;
      void* shader = nullptr;
      switch (kind) {
        case 0:
          hr = impl->d3d11->CreateHullShader(bytecode.data(), bytecode.len(), nullptr,
                                             reinterpret_cast<ID3D11HullShader**>(&shader));
          break;
        case 1:
          hr = impl->d3d11->CreateDomainShader(bytecode.data(), bytecode.len(), nullptr,
                                               reinterpret_cast<ID3D11DomainShader**>(&shader));
          break;
        default:
          umd_log("create_tess_shader_sig: unknown shader kind");
          return 0;
      }
      if (FAILED(hr)) {
        umd_log("create_tess_shader_sig: shader creation returned failure");
        return 0;
      }
      return reinterpret_cast<std::size_t>(shader);
  });
}

bool HeliosDxvkDevice::rotate_resource_backings(
    const std::size_t* d3d11_resource_ptrs,
    std::size_t count) const {
  if (!impl || !impl->d3d11 || !impl->context || !d3d11_resource_ptrs || count < 2)
    return false;
  return bridge_guard("rotate_resource_backings", false, [&]() -> bool {
      // Collect the DXVK images first; refuse the whole rotation if any entry
      // is not a storage-backed texture (a partial rotation would corrupt the
      // swapchain identity mapping). Rc refs: the swap executes later on the
      // CS thread and must not race resource destruction.
      std::vector<dxvk::Rc<dxvk::DxvkImage>> images;
      images.reserve(count);
      for (std::size_t i = 0; i < count; ++i) {
        auto* resource = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptrs[i]);
        auto* texture = resource ? dxvk::GetCommonTexture(resource) : nullptr;
        if (!texture || texture->GetImage() == nullptr || texture->GetImage()->storage() == nullptr) {
          umd_log("rotate_resource_backings: entry without image storage");
          return false;
        }
        images.push_back(texture->GetImage());
      }

      // DXGI may only rotate identities within one compatible swapchain ring.
      // invalidateImage replaces the storage but intentionally leaves the
      // stable DxvkImage's format/shape metadata in place, so accepting a
      // mixed ring would attach (for example) packed RGB10A2 storage to a
      // BGRA8 image and make every downstream user reinterpret its bytes.
      const auto& expected = images[0]->info();
      for (std::size_t i = 1; i < images.size(); ++i) {
        const auto& actual = images[i]->info();
        if (actual.type        != expected.type
         || actual.format      != expected.format
         || actual.sampleCount != expected.sampleCount
         || actual.extent.width  != expected.extent.width
         || actual.extent.height != expected.extent.height
         || actual.extent.depth  != expected.extent.depth
         || actual.numLayers   != expected.numLayers
         || actual.mipLevels   != expected.mipLevels) {
          char msg[320];
          std::snprintf(msg, sizeof(msg),
            "rotate_resource_backings: incompatible slot %zu/%zu "
            "expected fmt=%u extent=%ux%ux%u samples=%u layers=%u mips=%u; "
            "got fmt=%u extent=%ux%ux%u samples=%u layers=%u mips=%u",
            i, images.size(),
            static_cast<unsigned>(expected.format),
            expected.extent.width, expected.extent.height, expected.extent.depth,
            static_cast<unsigned>(expected.sampleCount),
            expected.numLayers, expected.mipLevels,
            static_cast<unsigned>(actual.format),
            actual.extent.width, actual.extent.height, actual.extent.depth,
            static_cast<unsigned>(actual.sampleCount),
            actual.numLayers, actual.mipLevels);
          umd_log(msg);
          return false;
        }
      }

      // CS-side identity rotation (18th session), mirroring upstream
      // D3D11SwapChain::RotateBackBuffers: swap the storages ON the CS thread
      // via DxvkContext::invalidateImage. No GPU drain is needed — every
      // already-recorded command holds its own storage ref and keeps targeting
      // the pre-rotation memory; the swap applies in CS order for everything
      // recorded after this DDI. The two rejected designs, for the record:
      //  - whole-device event-query drain (bring-up shim): 15-25 ms per
      //    present, dominated by Sleep(1) timer quantization;
      //  - per-image waitForResource on the present thread: WEDGES dwm — the
      //    bound backbuffer RTV is re-recorded into every new open cmdlist, so
      //    isInUse(Read) never clears for a bound render target (proven live
      //    with a dwm minidump, thread 1 parked in synchronizeUntil).
      LARGE_INTEGER qpcFreq, qpcT0;
      QueryPerformanceFrequency(&qpcFreq);
      QueryPerformanceCounter(&qpcT0);

      // InjectCsOrderedAfterPending dispatches the open recording chunk and
      // appends the swap on the ordered CS queue WITHOUT waiting for the CS
      // thread: the earlier SynchronizeCsThread variant blocked the present
      // thread behind the whole CS queue — up to 1.9 s per present during
      // login churn (rotate-perf) — the owner-visible "occasional dips".
      auto* immediateContext = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);

      immediateContext->InjectCsOrderedAfterPending([
        cImages = std::move(images)
      ] (dxvk::DxvkContext* ctx) {
        auto first = cImages[0]->storage();

        for (std::size_t i = 0; i + 1 < cImages.size(); ++i) {
          ctx->invalidateImage(cImages[i], cImages[i + 1]->storage(),
            cImages[i + 1]->info().layout);
        }

        ctx->invalidateImage(cImages[cImages.size() - 1u],
          std::move(first), cImages[0]->info().layout);
      });

      // Drain-cost telemetry (measure-first, PSC WS2): same key and format as
      // the old whole-device drain so before/after numbers compare directly.
      // One log line per 32 rotations.
      {
        LARGE_INTEGER qpcT1;
        QueryPerformanceCounter(&qpcT1);
        static PeriodicStat s_drainStat(32u);
        if (const auto sample = s_drainStat.record(qpc_elapsed_us(qpcFreq, qpcT0, qpcT1))) {
          char msg[128];
          std::snprintf(msg, sizeof(msg),
                        "rotate-perf: n=%u drain_avg_us=%llu drain_max_us=%llu",
                        sample->n,
                        static_cast<unsigned long long>(sample->avg_us),
                        static_cast<unsigned long long>(sample->max_us));
          umd_log(msg);
        }
      }

      // Debug instrument (registry-gated, off by default): sample the ring
      // buffers — write-side ground truth for "does the composed frame carry
      // pixels". Records after the injected swap, so slots are POST-rotation
      // identities. HKLM\SOFTWARE\Helios!RotateSample (DWORD) = sample every
      // Nth rotation.
      static std::atomic<std::uint32_t> s_sampleEvery{~0u};
      static std::atomic<std::uint32_t> s_rotateCount{0};
      std::uint32_t sampleEvery = s_sampleEvery.load(std::memory_order_relaxed);
      if (sampleEvery == ~0u) {
        DWORD value = 0, size = sizeof(value);
        if (RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", "RotateSample",
                         RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, nullptr, &value, &size) != ERROR_SUCCESS)
          value = 0;
        sampleEvery = value;
        s_sampleEvery.store(sampleEvery, std::memory_order_relaxed);
      }
      if (sampleEvery && (s_rotateCount.fetch_add(1) % sampleEvery) == 0) {
        // Sample EVERY buffer in the ring, not just the presented one: a
        // nonzero count appearing in a slot other than [0] means content lands
        // in a buffer the present/rotation bookkeeping does not associate with
        // the presented allocation (ring misalignment), while all-zero across
        // the whole ring means the composition draws genuinely write nothing.
        for (std::size_t s = 0; s < count; ++s) {
          auto* res = reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptrs[s]);
          ID3D11Texture2D* tex = nullptr;
          if (FAILED(res->QueryInterface(__uuidof(ID3D11Texture2D),
                                         reinterpret_cast<void**>(&tex)))) {
            char skipmsg[128];
            std::snprintf(skipmsg, sizeof(skipmsg),
                          "rotate-sample: slot=%zu/%zu SKIPPED (not a Texture2D)", s, count);
            umd_log(skipmsg);
            continue;
          }
          D3D11_TEXTURE2D_DESC td = {};
          tex->GetDesc(&td);
          // The old code forced MipLevels/ArraySize to 1 on the staging desc while
          // copying from the real one. With ArraySize > 1 the descriptions
          // mismatch, CopyResource is a silent no-op, and the tool reports
          // nonzero=0/N — the exact false conclusion ("the composition draws write
          // nothing") it exists to test for. And the rows below are read as
          // std::uint32_t, i.e. 32bpp is assumed: against a 16bpp ring the 4-byte
          // column stride reads past the last row, which is an OOB read, not
          // merely a wrong number. Skip and SAY SO instead.
          if (td.MipLevels != 1 || td.ArraySize != 1 || !is_32bpp_dxgi_format(td.Format)) {
            char skipmsg[192];
            std::snprintf(skipmsg, sizeof(skipmsg),
                          "rotate-sample: slot=%zu/%zu SKIPPED (mips=%u array=%u fmt=%u — "
                          "needs a single-subresource 32bpp texture)",
                          s, count, td.MipLevels, td.ArraySize,
                          static_cast<unsigned>(td.Format));
            umd_log(skipmsg);
            tex->Release();
            continue;
          }
          D3D11_TEXTURE2D_DESC sd = td;
          sd.BindFlags = 0;
          sd.MiscFlags = 0;
          sd.Usage = D3D11_USAGE_STAGING;
          sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
          ID3D11Texture2D* staging = nullptr;
          if (SUCCEEDED(impl->d3d11->CreateTexture2D(&sd, nullptr, &staging)) && staging) {
            impl->context->CopyResource(staging, tex);
            D3D11_MAPPED_SUBRESOURCE map = {};
            if (SUCCEEDED(impl->context->Map(staging, 0, D3D11_MAP_READ, 0, &map)) &&
                map.pData != nullptr) {
              const auto* base = static_cast<const std::uint8_t*>(map.pData);
              std::uint32_t nonzero = 0, samples = 0;
              for (UINT y = 0; y < td.Height; y += 64) {
                const auto* row = reinterpret_cast<const std::uint32_t*>(base + std::size_t(y) * map.RowPitch);
                for (UINT x = 0; x < td.Width; x += 64) {
                  ++samples;
                  nonzero += row[x] != 0;
                }
              }
              char msg[160];
              std::snprintf(msg, sizeof(msg),
                            "rotate-sample: slot=%zu/%zu %ux%u nonzero=%u/%u center=0x%08x",
                            s, count, td.Width, td.Height, nonzero, samples,
                            reinterpret_cast<const std::uint32_t*>(
                                base + std::size_t(td.Height / 2) * map.RowPitch)[td.Width / 2]);
              umd_log(msg);
              impl->context->Unmap(staging, 0);
            } else {
              char skipmsg[128];
              std::snprintf(skipmsg, sizeof(skipmsg),
                            "rotate-sample: slot=%zu/%zu SKIPPED (staging Map returned no data)",
                            s, count);
              umd_log(skipmsg);
            }
            staging->Release();
          } else {
            char skipmsg[128];
            std::snprintf(skipmsg, sizeof(skipmsg),
                          "rotate-sample: slot=%zu/%zu SKIPPED (staging CreateTexture2D failed)",
                          s, count);
            umd_log(skipmsg);
          }
          tex->Release();
        }
      }

      // The storage swap itself (resource[i] takes resource[i+1]'s, the last
      // takes the first's) executes in the injected CS command above.
      return true;
  });
}

std::int32_t HeliosDxvkDevice::dxgi_blt_convert(
    std::size_t dst_resource_ptr,
    std::uint32_t dst_subresource,
    std::uint32_t dst_x,
    std::uint32_t dst_y,
    std::size_t src_resource_ptr,
    std::uint32_t src_subresource,
    bool use_src_box,
    std::uint32_t src_left,
    std::uint32_t src_top,
    std::uint32_t src_right,
    std::uint32_t src_bottom) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  return bridge_guard("dxgi_blt_convert", -1, [&]() -> std::int32_t {
      auto* dstTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(dst_resource_ptr));
      auto* srcTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(src_resource_ptr));
      if (!dstTex || !dstTex->GetImage() || !srcTex || !srcTex->GetImage()) {
        umd_log("dxgi_blt_convert: non-texture resource");
        return -1;
      }
      if (dst_subresource >= dstTex->CountSubresources()
       || src_subresource >= srcTex->CountSubresources()) {
        umd_log("dxgi_blt_convert: subresource out of range");
        return -1;
      }

      dxvk::Rc<dxvk::DxvkImage> dstImage = dstTex->GetImage();
      dxvk::Rc<dxvk::DxvkImage> srcImage = srcTex->GetImage();
      const auto* dstFormat = dstImage->formatInfo();
      const auto* srcFormat = srcImage->formatInfo();
      if (!dstFormat || !srcFormat
       || dstFormat->aspectMask != VK_IMAGE_ASPECT_COLOR_BIT
       || srcFormat->aspectMask != VK_IMAGE_ASPECT_COLOR_BIT
       || dstImage->info().sampleCount != VK_SAMPLE_COUNT_1_BIT
       || srcImage->info().sampleCount != VK_SAMPLE_COUNT_1_BIT) {
        umd_log("dxgi_blt_convert: needs single-sampled color images");
        return -1;
      }

      const VkImageSubresourceLayers dstLayers = dxvk::vk::makeSubresourceLayers(
        dstTex->GetSubresourceFromIndex(dstFormat->aspectMask, dst_subresource));
      const VkImageSubresourceLayers srcLayers = dxvk::vk::makeSubresourceLayers(
        srcTex->GetSubresourceFromIndex(srcFormat->aspectMask, src_subresource));
      const VkExtent3D srcMip = srcTex->MipLevelExtent(srcLayers.mipLevel);
      const VkExtent3D dstMip = dstTex->MipLevelExtent(dstLayers.mipLevel);

      VkOffset3D srcOffset = { 0, 0, 0 };
      VkExtent3D extent = srcMip;
      if (use_src_box) {
        if (src_right <= src_left || src_bottom <= src_top
         || src_right > srcMip.width || src_bottom > srcMip.height) {
          umd_log("dxgi_blt_convert: invalid source rectangle");
          return -1;
        }
        srcOffset = {
          static_cast<std::int32_t>(src_left),
          static_cast<std::int32_t>(src_top),
          0,
        };
        extent = { src_right - src_left, src_bottom - src_top, 1u };
      }
      if (dst_x > dstMip.width || dst_y > dstMip.height
       || extent.width > dstMip.width - dst_x
       || extent.height > dstMip.height - dst_y
       || extent.depth != 1u) {
        umd_log("dxgi_blt_convert: destination region out of bounds");
        return -1;
      }

      static_cast<dxvk::D3D11ImmediateContext*>(impl->context)->HeliosConvertImage(
        dstImage, dstLayers,
        VkOffset3D { static_cast<std::int32_t>(dst_x), static_cast<std::int32_t>(dst_y), 0 },
        srcImage, srcLayers, srcOffset, extent);
      static std::atomic<std::uint32_t> s_converted{0};
      const std::uint32_t n = s_converted.fetch_add(1, std::memory_order_relaxed) + 1;
      if (n <= 4 || (n % 16384u) == 0) {
        char msg[192];
        std::snprintf(msg, sizeof(msg),
          "dxgi_blt_convert: queued #%u src_vk=%u dst_vk=%u extent=%ux%u",
          n,
          static_cast<unsigned>(srcImage->info().format),
          static_cast<unsigned>(dstImage->info().format),
          extent.width, extent.height);
        umd_log(msg);
      }
      return 0;
  });
}

// Keep backend failures separate from bounded completion timeouts.
static std::atomic<std::uint32_t> s_gateFailures{0};
static std::atomic<std::uint32_t> s_gateNoContext{0};

// Producers whose present could not be published, by stage. Every one of these
// means a consumer will read that surface UNORDERED -- the black-frame defect --
// so none of them may be silent.
static std::atomic<std::uint32_t> s_publishNoResource{0};
static std::atomic<std::uint32_t> s_publishFenceFailed{0};
static std::atomic<std::uint32_t> s_publishSlotFailed{0};
static std::atomic<std::uint64_t> s_publishOk{0};
static std::atomic<std::uint32_t> s_presentStreamRegistered{0};
static std::atomic<std::uint32_t> s_presentStreamUnavailable{0};

bool HeliosDxvkDevice::publish_present_order(std::size_t d3d11_resource_ptr,
                                             std::uint32_t* out_ctx_id,
                                             std::uint32_t* out_value32,
                                             std::uint64_t* out_cookie) const {
  if (out_ctx_id) *out_ctx_id = 0;
  if (out_value32) *out_value32 = 0;
  if (out_cookie) *out_cookie = 0;
  if (!impl || !impl->context)
    return false;
  // Producer streams are Venus timelines; NVK presents are CPU-complete.
  if (impl->backend != helios_bridge::IcdBackend::Venus)
    return false;

  return bridge_guard("publish_present_order", false, [&]() -> bool {
    if (!d3d11_resource_ptr)
      return false;

    auto* texture = dxvk::GetCommonTexture(
      reinterpret_cast<ID3D11Resource*>(d3d11_resource_ptr));
    if (!texture || !texture->GetImage() || !texture->GetImage()->storage())
      return false;
    auto storage = texture->GetImage()->storage();
    auto producer = storage->heliosProducer();
    if (producer == nullptr) {
      s_publishNoResource.fetch_add(1, std::memory_order_relaxed);
      umd_log("producer: missing exact allocation binding");
      return false;
    }

    // All potentially re-entrant work stays outside present_order_mutex.  At
    // most one caller initializes the timeline; concurrent callers wait for
    // that one attempt and observe either its ready state or its permanent
    // failure latch.
    bool initialize_present_fence = false;
    {
      std::unique_lock lock(impl->present_order_mutex);
      while (impl->present_fence_initializing)
        impl->present_order_ready.wait(lock);
      if (impl->present_fence_failed)
        return false;
      if (impl->present_fence == nullptr) {
        impl->present_fence_initializing = true;
        initialize_present_fence = true;
      }
    }

    if (initialize_present_fence) {
      dxvk::Rc<dxvk::DxvkFence> fence;
      std::uint64_t cookie = 0;

      // `createFence` enters Vulkan and the private registration enters the
      // ICD/KMD, so neither runs under present_order_mutex.
      try {
        dxvk::DxvkFenceCreateInfo fenceInfo = { };
        fenceInfo.sharedType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
        fence = impl->device->createFence(fenceInfo);

        uint32_t ctx = 0;
        if (producer->registerStream(impl->device->vkd()->device(), fence->handle(), &ctx, &cookie)
         && ctx == impl->venus_ctx_id) {
          const auto n = s_presentStreamRegistered.fetch_add(
              1, std::memory_order_relaxed) + 1;
          char stream_msg[192];
          std::snprintf(stream_msg, sizeof(stream_msg),
            "present-stream: registered ctx=%u cookie=%llu (x%u)",
            impl->venus_ctx_id, static_cast<unsigned long long>(cookie), n);
          umd_log(stream_msg);
        } else {
          const auto n = s_presentStreamUnavailable.fetch_add(
              1, std::memory_order_relaxed) + 1;
          char stream_msg[192];
          std::snprintf(stream_msg, sizeof(stream_msg),
            "present-stream: unavailable (old ICD/KMD or refused registration, x%u)", n);
          umd_log(stream_msg);
          throw dxvk::DxvkError("Helios: exact producer stream registration failed");
        }
      } catch (const dxvk::DxvkError& e) {
        // Latch: retrying per present would spam and never succeed.  Release
        // every concurrent waiter before returning through the old fallback.
        {
          std::lock_guard lock(impl->present_order_mutex);
          impl->present_fence_failed = true;
          impl->present_fence_initializing = false;
        }
        impl->present_order_ready.notify_all();
        s_publishFenceFailed.fetch_add(1, std::memory_order_relaxed);
        char msg[256];
        std::snprintf(msg, sizeof(msg),
          "producer: stream initialization FAILED (%s)",
          e.message().c_str());
        umd_log(msg);
        producer->abort();
        return false;
      } catch (...) {
        // bridge_guard owns the diagnostic, but must not leave concurrent
        // publishers waiting forever after an unexpected initialization error.
        {
          std::lock_guard lock(impl->present_order_mutex);
          impl->present_fence_failed = true;
          impl->present_fence_initializing = false;
        }
        impl->present_order_ready.notify_all();
        throw;
      }

      {
        std::lock_guard lock(impl->present_order_mutex);
        impl->present_fence = std::move(fence);
        impl->present_stream_cookie = cookie;
        impl->present_fence_initializing = false;
      }
      impl->present_order_ready.notify_all();

    }

    // Recording and publication share one order. The closure and its command
    // list retain an operation that fails the allocation if never submitted.
    // This preserves the folded signal and the existing present flush/batching.
    std::lock_guard lock(impl->present_order_mutex);
    if (impl->present_fence_failed || impl->present_fence == nullptr
     || impl->present_value == UINT32_MAX || !impl->venus_ctx_id) {
      producer->abort();
      return false;
    }
    const auto value = ++impl->present_value;
    dxvk::Rc<dxvk::HeliosProducerOperation> operation =
      new dxvk::HeliosProducerOperation(producer);
    static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
      ->HeliosSignalPresentFence(impl->present_fence, value, operation);
    uint64_t epoch = 0;
    if (!producer->publish(impl->present_fence->handle(), value, &epoch)) {
      producer->abort();
      s_publishSlotFailed.fetch_add(1, std::memory_order_relaxed);
      umd_log("producer: allocation epoch publication failed");
      return false;
    }
    if (out_ctx_id) *out_ctx_id = impl->venus_ctx_id;
    if (out_value32) *out_value32 = uint32_t(value);
    if (out_cookie) *out_cookie = impl->present_stream_cookie;
    s_publishOk.fetch_add(1, std::memory_order_relaxed);
    return true;
  });
}

bool HeliosDxvkDevice::set_scanout_acquire_event(std::size_t event_handle) const noexcept {
  return bridge_guard("set_scanout_acquire_event", false, [&]() -> bool {
    if (!impl || impl->device == nullptr || !event_handle)
      return false;

    impl->device->heliosScanoutAcquire().setEventHandle(
      reinterpret_cast<HANDLE>(event_handle));

    char msg[96];
    std::snprintf(msg, sizeof(msg),
      "scanout-acquire: retirement event 0x%zx delivered to DXVK signaler",
      event_handle);
    umd_log(msg);
    return true;
  });
}

std::int32_t HeliosDxvkDevice::present_frame_gate(std::uint32_t timeout_us,
                                               std::uint32_t order_mode) const {
  if (!impl || !impl->context) {
    s_gateNoContext.fetch_add(1, std::memory_order_relaxed);
    return E_FAIL;
  }
  const auto outcome = bridge_guard<std::int32_t>(
      "present_frame_gate", E_FAIL, [&]() -> std::int32_t {
    LARGE_INTEGER qpcFreq, qpcT0, qpcT1;
    QueryPerformanceFrequency(&qpcFreq);
    QueryPerformanceCounter(&qpcT0);

    auto* immediateContext = static_cast<dxvk::D3D11ImmediateContext*>(impl->context);
    bool completed;
    if (order_mode == kPresentOrderSubmitted) {
      if (!immediateContext->HeliosWaitFrameSubmitted()) {
        umd_log("present_frame_gate: command stream/submission failed");
        return E_FAIL;
      }
      completed = true;
    } else if (order_mode == kPresentOrderComplete) {
      const auto result = immediateContext->HeliosWaitFrameComplete(timeout_us);
      if (result != VK_SUCCESS && result != VK_TIMEOUT) {
        umd_log("present_frame_gate: command stream/submission failed");
        return E_FAIL;
      }
      completed = result == VK_SUCCESS;
    } else {
      return E_FAIL;
    }

    // Gate-cost telemetry (PSC WS2 discipline): one line per 128 presents.
    QueryPerformanceCounter(&qpcT1);
    // Site-local extra, deliberately NEVER reset (unlike the running max), so
    // the printed count is cumulative for the process.
    static std::atomic<std::uint32_t> s_gateTimeouts{0};
    if (!completed)
      s_gateTimeouts.fetch_add(1, std::memory_order_relaxed);
    static PeriodicStat s_gateStat(128u);
    if (const auto sample = s_gateStat.record(qpc_elapsed_us(qpcFreq, qpcT0, qpcT1))) {
      // The existing keys keep their names, order and format so before/after
      // runs compare directly; `failed` and `noctx` are APPENDED. R826.
      char msg[220];
      std::snprintf(msg, sizeof(msg),
                    "present-gate: n=%u avg_us=%llu max_us=%llu timeouts=%u "
                    "failed=%u noctx=%u mode=%u",
                    sample->n,
                    static_cast<unsigned long long>(sample->avg_us),
                    static_cast<unsigned long long>(sample->max_us),
                    s_gateTimeouts.load(std::memory_order_relaxed),
                    s_gateFailures.load(std::memory_order_relaxed),
                    s_gateNoContext.load(std::memory_order_relaxed),
                    order_mode);
      umd_log(msg);
    }
    return completed ? S_OK : S_FALSE;
  });
  if (outcome < 0)
    s_gateFailures.fetch_add(1, std::memory_order_relaxed);
  return outcome;
}

std::uint64_t HeliosDxvkDevice::flush_present_copy() const {
  return bridge_guard("flush_present_copy", std::uint64_t(0), [&]() -> std::uint64_t {
    if (!impl || !impl->context || impl->device->getDeviceStatus() != VK_SUCCESS)
      return 0;
    return static_cast<dxvk::D3D11ImmediateContext*>(impl->context)->HeliosFlushFrame();
  });
}

std::int32_t HeliosDxvkDevice::wait_present_copy(
    std::uint64_t submission_id, std::uint32_t timeout_us) const {
  return bridge_guard("wait_present_copy", -1, [&]() -> std::int32_t {
    if (!impl || !impl->context || !submission_id)
      return -1;
    const auto status = static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
      ->HeliosWaitSubmissionComplete(submission_id, timeout_us);
    return status == VK_SUCCESS ? 0 : status == VK_TIMEOUT ? 1 : -1;
  });
}

std::int32_t HeliosDxvkDevice::present_vehicle_copy(
    std::size_t dst_resource_ptr,
    std::size_t src_resource_ptr,
    std::size_t semaphore_handle,
    std::uint64_t semaphore_value) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  return bridge_guard("present_vehicle_copy", -1, [&]() -> std::int32_t {
      if (!semaphore_handle || !semaphore_value)
        return -1;
      dxvk::Rc<dxvk::DxvkFence> semaphore;
      {
        std::lock_guard lock(impl->vehicle_semaphore_mutex);
        using CompareFn = BOOL (WINAPI*)(HANDLE, HANDLE);
        static const auto compare = reinterpret_cast<CompareFn>(GetProcAddress(
          GetModuleHandleW(L"KernelBase.dll"), "CompareObjectHandles"));
        if (!compare) return -1;
        const HANDLE source = reinterpret_cast<HANDLE>(semaphore_handle);
        if (!impl->vehicle_semaphore_handle || !compare(source, impl->vehicle_semaphore_handle)) {
          HANDLE retained = nullptr;
          if (!DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(),
                &retained, 0, FALSE, DUPLICATE_SAME_ACCESS)) return -1;
          dxvk::DxvkFenceCreateInfo info = { };
          info.sharedType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_WIN32_BIT;
          info.sharedHandle = retained;
          try {
            semaphore = impl->device->createFence(info);
          } catch (...) {
            CloseHandle(retained);
            throw;
          }
          if (impl->vehicle_semaphore_handle) CloseHandle(impl->vehicle_semaphore_handle);
          impl->vehicle_semaphore_handle = retained;
          impl->vehicle_semaphore = semaphore;
        } else {
          semaphore = impl->vehicle_semaphore;
        }
      }
      auto* dstTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(dst_resource_ptr));
      auto* srcTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(src_resource_ptr));
      if (!dstTex || !dstTex->GetImage() || !srcTex || !srcTex->GetImage()) {
        umd_log("present_vehicle_copy: non-texture resource");
        return -1;
      }

      dxvk::Rc<dxvk::DxvkImage> dstImage = dstTex->GetImage();
      dxvk::Rc<dxvk::DxvkImage> srcImage = srcTex->GetImage();

      // Source the LIVE storage: device-local imports carry the creator's
      // pixels in the direct-bind staging ALIAS image; the texture's own image
      // is a private surface refreshed only when a prior read armed it (frame 1
      // would be undefined). Direct (non-staged) imports read their own image.
      if (srcImage->heliosStagingImage() != nullptr)
        srcImage = srcImage->heliosStagingImage();

      const VkExtent3D dstExtent = dstImage->info().extent;
      const VkExtent3D srcExtent = srcImage->info().extent;
      const VkExtent3D extent = {
        std::min(dstExtent.width,  srcExtent.width),
        std::min(dstExtent.height, srcExtent.height),
        1u,
      };

      static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
        ->HeliosCopyExternalFrame(dstImage, srcImage, extent, semaphore, semaphore_value);

      // Geometry mismatch is copyable (min region) but must be loud — during
      // resize churn one letterboxed frame is fine, a silent steady state of
      // them is a caller bug.
      const bool mismatch = dstExtent.width != srcExtent.width
                         || dstExtent.height != srcExtent.height;
      if (mismatch) {
        static std::atomic<std::uint32_t> s_mismatch{0};
        const std::uint32_t n = s_mismatch.fetch_add(1, std::memory_order_relaxed) + 1;
        if (n == 1 || (n % 128u) == 0) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "present_vehicle_copy: geometry mismatch dst=%ux%u src=%ux%u (x%u)",
            dstExtent.width, dstExtent.height, srcExtent.width, srcExtent.height, n);
          umd_log(msg);
        }
        return 1;
      }
      return 0;
  });
}

std::int32_t HeliosDxvkDevice::present_snapshot_copy(
    std::size_t dst_resource_ptr,
    std::size_t src_resource_ptr,
    bool windowed_blt_reservation) const {
  if (!impl || !impl->context || !dst_resource_ptr || !src_resource_ptr)
    return -1;

  return bridge_guard("present_snapshot_copy", -1, [&]() -> std::int32_t {
      auto* dstTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(dst_resource_ptr));
      auto* srcTex = dxvk::GetCommonTexture(
        reinterpret_cast<ID3D11Resource*>(src_resource_ptr));
      if (!dstTex || !dstTex->GetImage() || !srcTex || !srcTex->GetImage()) {
        umd_log("present_snapshot_copy: non-texture resource");
        return -1;
      }

      dxvk::Rc<dxvk::DxvkImage> dstImage = dstTex->GetImage();
      dxvk::Rc<dxvk::DxvkImage> srcImage = srcTex->GetImage();

      // No staging-alias substitution here, deliberately: the presented
      // primary is this device's own DXVK image, never an import, so its own
      // image IS the live storage. present_vehicle_copy's heliosStagingImage
      // arm exists only for cross-context imports.

      const VkExtent3D dstExtent = dstImage->info().extent;
      const VkExtent3D srcExtent = srcImage->info().extent;
      // Reject stale geometry before recording work: a partial snapshot
      // would retain pixels from a previous frame outside the copied region.
      const bool mismatch = dstExtent.width != srcExtent.width
                         || dstExtent.height != srcExtent.height;
      if (mismatch) {
        static std::atomic<std::uint32_t> s_mismatch{0};
        const std::uint32_t n = s_mismatch.fetch_add(1, std::memory_order_relaxed) + 1;
        if (n <= 8 || (n % 128u) == 0) {
          char msg[160];
          std::snprintf(msg, sizeof(msg),
            "present_snapshot_copy: geometry mismatch dst=%ux%u src=%ux%u (x%u)",
            dstExtent.width, dstExtent.height, srcExtent.width, srcExtent.height, n);
          umd_log(msg);
        }
        return 1;
      }
      const VkExtent3D extent = srcExtent;

      if (!static_cast<dxvk::D3D11ImmediateContext*>(impl->context)
            ->HeliosCopyPresentSnapshot(
              dstImage, srcImage, extent, windowed_blt_reservation)) {
        // A WindowedBlt reader lease is reserved by KMD Present. Without the
        // pre-arm reservation the producer list could wait on that same lease,
        // so this is a hard snapshot refusal, never a best-effort copy.
        umd_log("present_snapshot_copy: incompatible image geometry or WindowedBlt reuse reservation unavailable");
        return -1;
      }

      return 0;
  });
}

std::size_t HeliosDxvkDevice::create_hull_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11HullShader>(
      impl.get(), "hs", "CreateHullShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11HullShader** out) {
        return d->CreateHullShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_domain_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11DomainShader>(
      impl.get(), "ds", "CreateDomainShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11DomainShader** out) {
        return d->CreateDomainShader(bc, n, nullptr, out);
      });
}

std::size_t HeliosDxvkDevice::create_compute_shader(const std::uint8_t* code, std::size_t len) const {
  return create_shader_impl<ID3D11ComputeShader>(
      impl.get(), "cs", "CreateComputeShader", code, len,
      [](ID3D11Device* d, const void* bc, std::size_t n, ID3D11ComputeShader** out) {
        return d->CreateComputeShader(bc, n, nullptr, out);
      });
}

namespace {

  // HKLM\SOFTWARE\Helios REG_DWORD, `fallback` when absent.
  DWORD helios_reg_dword(const char* name, DWORD fallback) {
    DWORD value = 0;
    DWORD size = sizeof(value);
    if (RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", name,
                     RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, nullptr, &value,
                     &size) == ERROR_SUCCESS)
      return value;
    return fallback;
  }

  // The per-backend DXVK environment. _putenv_s is not safe against a
  // concurrent getenv, so this runs before any instance of the backend exists:
  // once per process for the backend chosen, and once more only if NVK fails
  // and the process falls back to Venus.
  void configure_dxvk_env(helios_bridge::IcdBackend backend) {
    if (backend == helios_bridge::IcdBackend::NvkRm) {
      // DXVK only sees NVK (the instance is NVK's own, no loader), but keep
      // the filter explicit; the backend switch turns the Venus-only export
      // paths off (third_party/patches/dxvk).
      _putenv_s("DXVK_FILTER_DEVICE_NAME", "NVK");
      _putenv_s("HELIOS_DXVK_BACKEND", "nvk");
    } else {
      // Force selection of the Helios venus device if other ICDs are present.
      _putenv_s("DXVK_FILTER_DEVICE_NAME", "Virtio-GPU Venus");
      _putenv_s("HELIOS_DXVK_BACKEND", "");
      // ForeignImport=1: this Venus process (DWM above all) composes surfaces
      // NVK processes made, through explicit DRM-modifier imports. Off by
      // default: it enables VK_EXT_image_drm_format_modifier on the device.
      _putenv_s("HELIOS_DXVK_FOREIGN_IMPORT",
                helios_reg_dword("ForeignImport", 0) ? "1" : "");
    }
    // HELIOS_DXVK_KMT_SHARED is no longer forced here: the engine defaults it
    // ON (2026-08-05). Forcing it made a knob that could not be off in any
    // configuration this process ever produced, which hid the fact that the
    // engine's own default disagreed with every measurement taken.

    // Debug instrument (registry-gated, off by default): route DXVK's shader
    // dumping into every UMD-hosting process — session-0 services (dwm) cannot
    // be given process env vars any other way. HKLM\SOFTWARE\Helios!
    // ShaderDumpPath (REG_SZ) = target directory.
    char dumpPath[MAX_PATH] = {};
    DWORD size = sizeof(dumpPath);
    const bool haveDump =
        RegGetValueA(HKEY_LOCAL_MACHINE, "SOFTWARE\\Helios", "ShaderDumpPath",
                     RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, nullptr, dumpPath, &size) == ERROR_SUCCESS &&
        dumpPath[0];
    if (haveDump)
      _putenv_s("DXVK_SHADER_DUMP_PATH", dumpPath);

    char msg[MAX_PATH + 160];
    std::snprintf(msg, sizeof(msg),
      "dxvk env configured: backend=%s DXVK_FILTER_DEVICE_NAME=%s "
      "kmt-shared=default-on foreign-import=%s DXVK_SHADER_DUMP_PATH=%s",
      backend == helios_bridge::IcdBackend::NvkRm ? "nvk" : "venus",
      backend == helios_bridge::IcdBackend::NvkRm ? "NVK" : "Virtio-GPU Venus",
      std::getenv("HELIOS_DXVK_FOREIGN_IMPORT") && std::getenv("HELIOS_DXVK_FOREIGN_IMPORT")[0] == '1'
        ? "on" : "off",
      haveDump ? dumpPath : "(unset)");
    umd_log(msg);
  }

  std::mutex g_envMutex;
  bool g_envConfigured[3] = {};

  void configure_dxvk_env_once(helios_bridge::IcdBackend backend) {
    std::lock_guard lock(g_envMutex);
    const auto i = static_cast<std::size_t>(backend);
    if (i < 3 && !g_envConfigured[i]) {
      configure_dxvk_env(backend);
      g_envConfigured[i] = true;
    }
  }

  // An ICD's vk_icdGetInstanceProcAddr serves no layer queries (that is the
  // loader's job): NVK's MinGW build returns NULL for
  // vkEnumerateInstanceLayerProperties (its vk_common_ fallback is a weak
  // symbol), and DxvkInstance::initVulkanInstance calls it unconditionally,
  // a call through NULL. Answer "no layers" and forward everything else, as
  // vkd3d's 0002-helios-nvk-backend does for D3D12. One ICD per process.
  PFN_vkGetInstanceProcAddr g_nvkIcdGipa = nullptr;

  VKAPI_ATTR VkResult VKAPI_CALL nvk_icd_enumerate_instance_layers(
      uint32_t* count, VkLayerProperties* layers) {
    (void)layers;
    *count = 0;
    return VK_SUCCESS;
  }

  VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL nvk_icd_get_instance_proc_addr(
      VkInstance instance, const char* name) {
    if (!instance && name && !std::strcmp(name, "vkEnumerateInstanceLayerProperties"))
      return reinterpret_cast<PFN_vkVoidFunction>(nvk_icd_enumerate_instance_layers);
    return g_nvkIcdGipa(instance, name);
  }

  // Build the DXVK instance/adapter/device and the D3D11 COM device on
  // `backend`. Returns false (with `d` partly filled) on any failure; the
  // caller drops `d`.
  bool create_on_backend(HeliosDxvkDeviceImpl& d, helios_bridge::IcdBackend backend,
                         std::uint32_t luid_low, std::int32_t luid_high) {
    d.backend = backend;
    if (backend == helios_bridge::IcdBackend::NvkRm) {
      auto gipa = reinterpret_cast<PFN_vkGetInstanceProcAddr>(
        helios_bridge::nvk_icd_get_instance_proc_addr());
      if (!gipa || !helios_bridge::nvk_icd_api(&d.icd)) {
        helios_bridge::note_nvk_failed("the NVK ICD did not load or has no helios_icd_interface_v2");
        return false;
      }
      // NVK's own vk_icdGetInstanceProcAddr: no Vulkan loader, no registry,
      // and no Venus ICD in this instance.
      g_nvkIcdGipa = gipa;
      dxvk::DxvkInstanceImportInfo import = { };
      import.loaderProc = nvk_icd_get_instance_proc_addr;
      d.instance = new dxvk::DxvkInstance(import, dxvk::DxvkInstanceFlags());
    } else {
      d.icd = *helios_bridge::venus_icd_api();
      d.instance = new dxvk::DxvkInstance(dxvk::DxvkInstanceFlags());
    }

    if (luid_low != 0 || luid_high != 0) {
      LUID luid;
      luid.LowPart  = luid_low;
      luid.HighPart = luid_high;
      d.adapter = d.instance->findAdapterByLuid(&luid);
      if (d.adapter == nullptr)
        umd_log("findAdapterByLuid found nothing; falling back to adapter 0");
    }

    if (d.adapter == nullptr)
      d.adapter = d.instance->enumAdapters(0);

    if (d.adapter == nullptr) {
      umd_log(backend == helios_bridge::IcdBackend::NvkRm
        ? "no Vulkan adapter enumerated (NVK found no RM GPU?)"
        : "no Vulkan adapter enumerated (venus ICD not present?)");
      return false;
    }

    d.device = d.adapter->createDevice();
    if (d.device == nullptr) {
      umd_log("DxvkAdapter::createDevice returned null");
      return false;
    }

    if (backend == helios_bridge::IcdBackend::NvkRm) {
      // The holder context is NVK's (created at the first IMPORT_RM); the
      // Venus ICD anchor does not concern this device.
      d.venus_ctx_id = 0;
      char msg[192];
      std::snprintf(msg, sizeof(msg),
        "DxvkDevice created on NVK OK (icd caps 0x%x: res_id=%u scanout=%u)",
        d.icd.caps, (d.icd.caps & HELIOS_ICD_CAP_RES_ID) ? 1u : 0u,
        (d.icd.caps & HELIOS_ICD_CAP_SCANOUT) ? 1u : 0u);
      umd_log(msg);
    } else {
      d.venus_ctx_id = read_instance_venus_context_id(d.instance->handle());
      // ⛔ S4b (`ARCHITECTURE.md` §6.4). The read above is the first thing that
      // forces `resolve_helios_icd_module`, which now reconciles against the
      // process-global anchor. If it refused, a SECOND venus ICD module is live
      // in this process — `helios_umd12.dll` selected a different one — and
      // every `VkDeviceMemory`/`VkInstance` identity this device would go on to
      // stamp is derived from the wrong one.
      //
      // ⚠ This is the one place S4b can change SHIPPING D3D11 behaviour, so be
      // exact about when: `icd_anchor_poisoned()` is false unless two distinct
      // modules both exported `helios_venus_memory_alloc_info` and the two UMDs
      // disagreed. In every process on this box today — one UMD, one ICD — it
      // cannot fire. Degrading instead would be fake success: the device comes
      // up, renders, and writes cross-process allocation identities nobody can
      // resolve.
      if (helios_bridge::icd_anchor_poisoned()) {
        umd_log("REFUSING DXVK device: venus ICD anchor mismatch "
                "(two ICD modules live in this process)");
        return false;
      }
      if (!d.venus_ctx_id)
        umd_log("DXVK device created but Venus context export returned 0");
      umd_log("DxvkDevice created on venus adapter OK");
    }

    // Instantiate DXVK's full D3D11 COM device from the DxvkDevice. The DDI
    // device-funcs forward to this ID3D11Device / its immediate context.
    // `new HeliosStubAdapter()` starts at refcount 1 and is only released AFTER
    // the D3D11DXGIDevice constructor returns — but that constructor builds the
    // D3D11 device and its immediate context and can throw dxvk::DxvkError, in
    // which case the catch below returns nullptr and the Release() never runs.
    // The guard makes the zero-refcount window exit through exactly one path.
    ComRelease<HeliosStubAdapter> stubAdapter(new HeliosStubAdapter());
    auto* dxgiDevice = new dxvk::D3D11DXGIDevice(
        stubAdapter.get(), nullptr, nullptr,
        d.instance, d.adapter, d.device,
        D3D_FEATURE_LEVEL_11_0, 0);
    stubAdapter.reset(); // dxgiDevice holds its own ref now

    HRESULT hr = dxgiDevice->QueryInterface(__uuidof(ID3D11Device),
                                            reinterpret_cast<void**>(&d.d3d11));
    if (FAILED(hr) || d.d3d11 == nullptr) {
      umd_log("QueryInterface(ID3D11Device) on D3D11DXGIDevice failed");
      // dxgiDevice has refcount 0 here (QI failed) — drop it.
      delete dxgiDevice;
      return false;
    }
    // d.d3d11 now holds the one ref that keeps dxgiDevice alive.
    d.d3d11->GetImmediateContext(&d.context);
    umd_log("D3D11 COM device + immediate context created OK");
    return true;
  }

}  // namespace

namespace {

// NVK's device bring-up (instance, physical device, the device and its first
// internal shaders through NAK) needs far more stack than a Venus one: frames
// of 17 to 43 KiB in vulkan_nouveau and the UMD, more than 128 KiB in all.
// DWM creates its device on "DWM LPC Port Thread", whose stack is 128 KiB:
// DWM on NVK died there with STATUS_STACK_OVERFLOW (docs/dwm-on-nvk.md, T1).
// So a creation on a thread with less than kNvkCreateStack of stack runs on a
// helper thread with that much, and the caller waits for it.
constexpr SIZE_T kNvkCreateStack = SIZE_T(8) << 20;

template <typename F>
struct StackJob {
  F* fn;
  std::unique_ptr<HeliosDxvkDevice> out;
  static DWORD WINAPI run(LPVOID p) {
    auto* j = static_cast<StackJob*>(p);
    j->out = (*j->fn)();
    return 0;
  }
};

template <typename F>
std::unique_ptr<HeliosDxvkDevice> run_with_stack(F&& fn) {
  ULONG_PTR low = 0, high = 0;
  GetCurrentThreadStackLimits(&low, &high);
  if (high - low >= kNvkCreateStack)
    return fn();
  using Fn = std::remove_reference_t<F>;
  StackJob<Fn> job{&fn, nullptr};
  HANDLE t = CreateThread(nullptr, kNvkCreateStack, &StackJob<Fn>::run, &job,
                          STACK_SIZE_PARAM_IS_A_RESERVATION, nullptr);
  if (!t) {
    umd_log("NVK device creation: no helper thread, creating on the caller's stack");
    return fn();
  }
  static std::atomic<std::uint32_t> s_logs{0};
  if (s_logs.fetch_add(1, std::memory_order_relaxed) < 4) {
    char msg[160];
    std::snprintf(msg, sizeof(msg),
                  "NVK device creation on a helper thread (caller's stack %llu KiB < %llu KiB)",
                  static_cast<unsigned long long>((high - low) >> 10),
                  static_cast<unsigned long long>(kNvkCreateStack >> 10));
    umd_log(msg);
  }
  WaitForSingleObject(t, INFINITE);
  CloseHandle(t);
  return std::move(job.out);
}

}  // namespace

std::unique_ptr<HeliosDxvkDevice> helios_dxvk_create_device(
    std::uint32_t luid_low,
    std::int32_t  luid_high,
    bool timer_resolution) {
  // R824: configuration delivered as a process-global side effect. One process
  // (dwm) creates several D3D11 devices, so the environment is written once per
  // backend, before that backend's first instance (configure_dxvk_env_once).
  //
  // ICD selection (S3, bridge_icd_backend.h): global NVK with a deny-list,
  // Venus whenever NVK is denied, missing or failed in this process.
  helios_bridge::IcdBackend backend = helios_bridge::effective_icd_backend();

  if (backend == helios_bridge::IcdBackend::NvkRm) {
    configure_dxvk_env_once(backend);
    auto nvk = run_with_stack([&]() -> std::unique_ptr<HeliosDxvkDevice> {
      // NVK's own policy (Mesa 0032) hides its GPU from a process that Icd,
      // the deny-list or HELIOS_ICD send to Venus; the UMD chose NVK past
      // them (DwmIcd=nvk under Icd=venus: DWM got "Failed to initialize
      // DXVK", docs/dwm-on-nvk.md T2), so NVK is told the same here.
      helios_bridge::NvkPolicyScope policy;
      return bridge_guard<std::unique_ptr<HeliosDxvkDevice>>(
          "helios_dxvk_create_device(nvk)", nullptr,
          [&]() -> std::unique_ptr<HeliosDxvkDevice> {
          auto out = std::make_unique<HeliosDxvkDevice>();
          out->impl = std::make_unique<HeliosDxvkDeviceImpl>(timer_resolution);
          if (!create_on_backend(*out->impl, backend, luid_low, luid_high))
            return nullptr;
          return out;
      });
    });
    if (nvk)
      return nvk;
    helios_bridge::note_nvk_failed("DXVK device creation on NVK failed");
    backend = helios_bridge::IcdBackend::Venus;
  }

  configure_dxvk_env_once(backend);
  return bridge_guard<std::unique_ptr<HeliosDxvkDevice>>(
      "helios_dxvk_create_device", nullptr,
      [&]() -> std::unique_ptr<HeliosDxvkDevice> {
      auto out = std::make_unique<HeliosDxvkDevice>();
      out->impl = std::make_unique<HeliosDxvkDeviceImpl>(timer_resolution);
      if (!create_on_backend(*out->impl, backend, luid_low, luid_high))
        return nullptr;
      return out;
  });
}
