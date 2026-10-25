# Uniform atomic return regression

An original Vulkan 1.1 compute shader exercises `nir_opt_uniform_atomics`:
all active lanes use the same atomic address, add divergent positive values,
and consume the returned values. The combined exclusive-scan/reduction path
emits `last_invocation`. Before common patch 0115, NAK rejects the pipeline
with `VK_ERROR_UNKNOWN` and `Unsupported intrinsic instruction: last_invocation`.
This also caused missing character lighting in BG3 through D3D11/DXVK.

The test runs nine active masks: full, empty, single-lane and sparse masks,
with lane 31 both active and inactive. The CPU checks the final counter and
sorts each active lane's returned interval `[old, old + delta)`. Those
intervals must tile `[0, sum(delta))` exactly. The oracle does not assume
an ordering between atomics from different lanes. Inactive slots must keep
the sentinel. Each case resets the buffer and waits for its own fence
(maximum 10 seconds), with host/compute memory barriers and flush/invalidate
for non-coherent memory.

## Build

Requires Vulkan headers, glslangValidator, and a C11 compiler. Linux also
needs the Vulkan loader library. Windows uses `vulkan-1.dll` dynamically.

```sh
make                       # Linux harness and SPIR-V
make windows               # MinGW x64 harness and SPIR-V
spirv-val --target-env vulkan1.1 uniform_atomics.spv
```

The cross-build defaults to `x86_64-w64-mingw32-gcc` and searches
`/usr/include` after MinGW's headers. Override `MINGW` and `VULKAN_INC`
for another toolchain or Vulkan-Headers checkout.

## Run and compare

The default selects NVK and refuses CPU/software devices. A Linux NVIDIA
reference driver is allowed only with `--reference`. The device must report
32-lane compute subgroups. The shader uses no optional Vulkan features.

Build two NVK copies from the same base, with and without patch 0115:

```sh
NAK_UNIFORM_ATOMICS=1 MESA_SHADER_CACHE_DISABLE=true \
VK_DRIVER_FILES=/path/to/baseline/nvk.json \
./uniform_atomics_test uniform_atomics.spv
# expected: nonzero exit, pipeline creation fails

NAK_UNIFORM_ATOMICS=1 MESA_SHADER_CACHE_DISABLE=true \
VK_DRIVER_FILES=/path/to/fixed/nvk.json \
./uniform_atomics_test uniform_atomics.spv
# expected: exit 0, all nine GPU cases pass
```

Use `VK_ICD_FILENAMES` instead of `VK_DRIVER_FILES` with older loaders.
On Windows, run from a **non-elevated** PowerShell: an elevated process's
Vulkan loader ignores these driver-selection variables. Set them for the
process before running `./uniform_atomics_test.exe uniform_atomics.spv`.
Each ICD JSON must point to its own NVK DLL; use the same RM client for both
runs. `NVK_RMCLIENT_LIB` can select that client's full path. Keep the shader
cache disabled and uniform atomics enabled for both runs.

Exit 0 means every GPU result check passed; exit 1 means a Vulkan error,
timeout or incorrect result. Exit 77 means prerequisites were unavailable
(loader/compatible instance, suitable device, compute queue or 32-lane
compute subgroups). Device creation and pipeline creation errors always fail.
After a timeout the process skips destruction of objects that may still be
in flight, avoiding an unbounded cleanup wait.

Validated on a Windows 11 Conduit VM with an RTX 4070 (NVK AD104): the
matched baseline fails on `last_invocation`; the fixed driver passes all
nine cases with `NAK_UNIFORM_ATOMICS` at its enabled default.
