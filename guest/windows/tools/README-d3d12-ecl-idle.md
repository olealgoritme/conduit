# ECL fence lifetime regression

A legal CPU idle gap between ExecuteCommandLists calls can outlive a host RM
fence made for the next batch. Previously the next ECL reused that failed fence;
the KMD's early-fired gate then allowed native D3D12 completion before the new
GPU work. D3D9On12 could return a dynamic-texture Lock(0), accept CPU font data,
and subsequently lose it when the outstanding image-to-staging copy ran.

Patch 0012 reserves the next timeline point ahead, and creates its timed RM
fence at commit using the existing fresh-fence path. Queue values, ordering,
exports and stats layout are unchanged. `Nvk12EclFencePrefetch=1` now controls
point reservation rather than early creation of an expiring host fence. This
restores one fence-create round trip to the commit path; FPS impact has not
been measured. Propagation of genuinely failed GPU fences is a separate issue.

The standalone test uses public D3D12 calls: zero image -> CPU-visible staging,
native fence wait -> immediate CPU pattern write -> staging upload -> separate
readback and full byte comparison. The staging allocation is the device's
CPU-readable custom heap, matching the D3D12TranslationLayer dynamic map path.
Copies have explicit barriers. Lists and resources are retained until bounded
waits prove completion; failures do not reset or release uncertain GPU work.
Optional private diagnostics are off in the regression runs (`diag=0`).

Build x86 with MinGW:

```sh
i686-w64-mingw32-g++ -std=c++17 -O2 -static \
  d3d12_readback_idle_fence.cpp -o d3d12_readback_idle_fence.exe \
  -ld3d12 -ldxgi -ldxguid
```

Arguments are rounds, readback copies, warmup batches, D3D9On12 creator flag,
private diagnostics, bounded wait timeout in ms, and CPU idle before readback in
ms. The idle is outside the wait/write/upload critical window. No drain is
inserted before it.

```text
# No-idle control:
d3d12_readback_idle_fence.exe 1 128 672 1 0 10000 0
# Backend safe-mode lifetime is 1 s:
d3d12_readback_idle_fence.exe 1 128 672 1 0 10000 1200
# Exceeds the normal 5 s host fence lifetime too:
d3d12_readback_idle_fence.exe 1 128 672 1 0 10000 6000
```

The creator flag 1 uses ID3D12Device9::CreateCommandQueue1 with the
D3D12TranslationLayer creator GUID; unsupported creation is a setup error.
Creator 0 uses the ordinary direct queue. Stress copies lengthen a valid
readback batch so premature completion can overwrite the pattern consistently;
there are barriers between every pair. A single-copy control can pass despite
the ordering bug because the copy finishes before the CPU writes.

PASS requires an exact comparison and a final successful bounded drain. Exit
5 means observed corruption, 4 a wait failure, 3 setup/device removal, 2 usage.
The native counter itself cannot prove physical completion; corruption is
therefore the primary oracle. No GTA executable, plugin or server is needed.

Observed before the fix on the Windows 11 Conduit VM, RTX 4070/NVIDIA 615,
6 GiB guest VRAM, safe mode, concurrent local inference GPU load:

| Existing implementation | Idle | Copies | Observed |
|---|---:|---:|---|
| Prefetch on | 0 ms | 128 | PASS |
| Prefetch on | 1200 ms | 128 | 8/8 corrupt, all 8 MiB replaced by zero |
| Prefetch off | 1200 ms | 128 | 8/8 PASS, exact byte comparison |
| Prefetch on | 6000 ms | 128 | corrupt |
| Prefetch off | 6000 ms | 128 | PASS |

The eight pairs were alternated under the same running host workload. These
are correctness results; no throughput comparison is inferred from them.

With patch 0012 in the loaded x86 UMD and prefetch enabled: the same nine-case
matrix passed all cases, including 1200 ms and 6000 ms stress. After restoring
the original uninstrumented KMD, another eight 1200 ms / 128-copy runs all
passed with exact readback bytes. Each process printed the actual patched UMD
path; changing only the registry path without restarting the display adapter
left Windows using the old DLL, and those runs were excluded from after-fix
results. No VM reboot was needed. The committed test has the same C++ tokens
as the binary used for these checks; subsequent edits only clarify comments.
