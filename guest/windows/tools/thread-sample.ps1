# thread-sample.ps1: where one thread of a running process spends its time, by
# sampling its instruction pointer. No install, no ETW: the thread is suspended
# for a few microseconds about once a millisecond, its context read, and
# resumed. Good for "which thread is the bottleneck and what is it doing" on a
# game that is CPU-bound in the guest (a thread near 100 %).
#
#   thread-sample.ps1 -Process cs2                      # per-thread CPU over 5 s
#   thread-sample.ps1 -Process cs2 -Thread D3D11RenderThread [-Samples 3000] [-Raw]
#
# Without -Thread it lists the process's threads by CPU over -Seconds (name
# from SetThreadDescription, start module). With -Thread (a thread name or id)
# it samples that thread and prints the module and module+offset histograms of
# the instruction pointer, and the first frame in -Focus (default helios_umd.dll)
# found on the instruction pointer or by scanning the top 4 KiB of the stack
# for return addresses (a heuristic: it can name a stale frame).
#
# Wall-clock samples: they equal CPU samples only for a thread that is nearly
# always running. A thread in the kernel samples as the user-mode syscall stub
# (ntdll Nt*/win32u NtGdiDd*), which says which call it was in.
#
# -Raw adds one line per sample ("module+offset module+offset ...", the
# instruction pointer first) for symbolizing on the host, e.g. with the
# package's PDBs next to the DLLs:
#   llvm-symbolizer --obj=helios_umd.dll 0x$(printf %x $((0x180000000 + 0xOFFSET)))
#
# -Qpc prefixes every raw line with "@<ms>", the QPC time of the sample in
# milliseconds: the same clock as PresentMon's CPUStartQPCTime (run PresentMon
# with --qpc_time_ms), so tools/pm-sample-split.py can split the samples into
# slow and fast frames. Run one sampler per thread, in parallel.
#
# "== syscall stubs" names the ntdll/win32u offsets that are system-call stubs
# (NtDelayExecution = Sleep, NtYieldExecution = SwitchToThread/Sleep(0),
# NtWaitForAlertByThreadId = SRW lock / condition variable / WaitOnAddress, ...).
param(
    [Parameter(Mandatory = $true)][string]$Process,
    [string]$Thread = '',
    [int]$Samples = 3000,
    [int]$Seconds = 5,
    [string]$Focus = 'helios_umd.dll',
    [switch]$Raw,
    [switch]$Qpc
)

Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class ThreadSample {
 [DllImport("kernel32.dll")] static extern IntPtr OpenThread(int a, bool i, int id);
 [DllImport("kernel32.dll")] static extern IntPtr OpenProcess(int a, bool i, int id);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
 [DllImport("kernel32.dll")] static extern int GetThreadDescription(IntPtr h, out IntPtr d);
 [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr p);
 [DllImport("kernel32.dll")] static extern uint SuspendThread(IntPtr h);
 [DllImport("kernel32.dll")] static extern uint ResumeThread(IntPtr h);
 [DllImport("kernel32.dll")] static extern bool GetThreadContext(IntPtr h, IntPtr c);
 [DllImport("kernel32.dll")] static extern bool ReadProcessMemory(IntPtr p, IntPtr a, byte[] b, int n, out IntPtr r);
 [DllImport("ntdll.dll")] static extern int NtQueryInformationThread(IntPtr h, int c, out IntPtr v, int l, IntPtr r);
 [DllImport("winmm.dll")] static extern uint timeBeginPeriod(uint p);
 [DllImport("winmm.dll")] static extern uint timeEndPeriod(uint p);
 [DllImport("kernel32.dll")] static extern bool QueryPerformanceCounter(out long c);
 [DllImport("kernel32.dll", CharSet = CharSet.Ansi)] static extern IntPtr GetModuleHandleA(string n);
 [DllImport("kernel32.dll", CharSet = CharSet.Ansi)] static extern IntPtr LoadLibraryA(string n);
 [DllImport("kernel32.dll", CharSet = CharSet.Ansi)] static extern IntPtr GetProcAddress(IntPtr m, string n);
 // Address of an export of a system DLL; system DLLs load at the same address
 // in every process of a boot, so this names stubs in the sampled process.
 public static long Export(string dll, string name) {
  IntPtr m = GetModuleHandleA(dll); if (m == IntPtr.Zero) m = LoadLibraryA(dll);
  if (m == IntPtr.Zero) return 0;
  return GetProcAddress(m, name).ToInt64();
 }
 const int QUERY_LIMITED = 0x0800, QUERY = 0x0040, SUSPEND_RESUME = 0x0002, GET_CONTEXT = 0x0008;
 public static string Name(int id) {
  IntPtr h = OpenThread(QUERY_LIMITED, false, id); if (h == IntPtr.Zero) return "";
  IntPtr d; string s = "";
  if (GetThreadDescription(h, out d) >= 0 && d != IntPtr.Zero) { s = Marshal.PtrToStringUni(d); LocalFree(d); }
  CloseHandle(h); return s;
 }
 public static long Start(int id) {
  IntPtr h = OpenThread(QUERY, false, id); if (h == IntPtr.Zero) return 0;
  IntPtr v; NtQueryInformationThread(h, 9 /* ThreadQuerySetWin32StartAddress */, out v, IntPtr.Size, IntPtr.Zero);
  CloseHandle(h); return v.ToInt64();
 }
 // Each sample: the QPC time, the instruction pointer, then up to 11 stack words that fall in [lo, hi).
 public static List<long[]> Run(int pid, int tid, int n, long[] lo, long[] hi) {
  const int STACK = 4096, CONTEXT_BYTES = 1232, CONTEXT_CONTROL = 0x100001;
  var res = new List<long[]>();
  IntPtr th = OpenThread(SUSPEND_RESUME | GET_CONTEXT | QUERY, false, tid);
  IntPtr ph = OpenProcess(0x0010 /* VM_READ */ | 0x0400, false, pid);
  if (th == IntPtr.Zero || ph == IntPtr.Zero) return res;
  IntPtr raw = Marshal.AllocHGlobal(CONTEXT_BYTES + 16);
  IntPtr ctx = new IntPtr((raw.ToInt64() + 15) & ~15L);
  byte[] stk = new byte[STACK];
  timeBeginPeriod(1);
  try {
   for (int i = 0; i < n; i++) {
    System.Threading.Thread.Sleep(1);
    for (int k = 0; k < CONTEXT_BYTES; k += 8) Marshal.WriteInt64(ctx, k, 0);
    Marshal.WriteInt32(ctx, 0x30, CONTEXT_CONTROL);
    if (SuspendThread(th) == 0xFFFFFFFF) break;
    long now; QueryPerformanceCounter(out now);
    bool ok = GetThreadContext(th, ctx);
    long rip = Marshal.ReadInt64(ctx, 0xF8), rsp = Marshal.ReadInt64(ctx, 0x98);
    IntPtr got; bool rd = ok && ReadProcessMemory(ph, new IntPtr(rsp), stk, STACK, out got);
    ResumeThread(th);
    if (!ok) continue;
    var s = new List<long> { now, rip };
    if (rd) for (int o = 0; o + 8 <= STACK && s.Count < 13; o += 8) {
     long v = BitConverter.ToInt64(stk, o);
     for (int m = 0; m < lo.Length; m++) if (v >= lo[m] && v < hi[m]) { s.Add(v); break; }
    }
    res.Add(s.ToArray());
   }
  } finally { timeEndPeriod(1); Marshal.FreeHGlobal(raw); CloseHandle(th); CloseHandle(ph); }
  return res;
 }
}
"@

$p = Get-Process $Process -ErrorAction Stop | Select-Object -First 1
$mods = @($p.Modules | ForEach-Object {
    [pscustomobject]@{ N = $_.ModuleName; B = $_.BaseAddress.ToInt64(); E = $_.BaseAddress.ToInt64() + $_.ModuleMemorySize }
})
function Where-Addr([long]$a) {
    foreach ($m in $mods) { if ($a -ge $m.B -and $a -lt $m.E) { return '{0}+{1:x}' -f $m.N, ($a - $m.B) } }
    '?{0:x}' -f $a
}
function Top($items, [int]$n) {
    $items | Group-Object | Sort-Object Count -Descending | Select-Object -First $n |
        ForEach-Object { '{0,6} {1}' -f $_.Count, $_.Name }
}

if ($Thread -eq '') {
    $before = @{}
    foreach ($t in $p.Threads) { $before[$t.Id] = $t.TotalProcessorTime.TotalMilliseconds }
    Start-Sleep -Seconds $Seconds
    $p.Refresh()
    $rows = foreach ($t in $p.Threads) {
        if (-not $before.ContainsKey($t.Id)) { continue }
        $start = Where-Addr ([ThreadSample]::Start($t.Id))
        [pscustomobject]@{
            Id     = $t.Id
            Cpu    = [math]::Round(($t.TotalProcessorTime.TotalMilliseconds - $before[$t.Id]) / ($Seconds * 10), 1)
            Name   = [ThreadSample]::Name($t.Id)
            Module = $start.Split('+')[0]
        }
    }
    $rows | Sort-Object Cpu -Descending | Select-Object -First 25 | Format-Table -AutoSize | Out-String -Width 200
    return
}

$tid = 0
if ($Thread -match '^\d+$') { $tid = [int]$Thread }
else { $tid = ($p.Threads | Where-Object { [ThreadSample]::Name($_.Id) -eq $Thread } | Select-Object -First 1).Id }
if (-not $tid) { Write-Error "no thread '$Thread' in $Process"; exit 1 }

$lo = [long[]]($mods | ForEach-Object { $_.B }); $hi = [long[]]($mods | ForEach-Object { $_.E })
$withTime = [ThreadSample]::Run($p.Id, $tid, $Samples, $lo, $hi)
$qpcFreq = [System.Diagnostics.Stopwatch]::Frequency
$times = @($withTime | ForEach-Object { $_[0] })
$res = @($withTime | ForEach-Object { ,($_[1..($_.Count - 1)]) })
"$Process thread $Thread (tid $tid): $($res.Count) samples"
'== module (instruction pointer)'
Top ($res | ForEach-Object { (Where-Addr $_[0]).Split('+')[0] }) 15
'== module+offset (instruction pointer)'
Top ($res | ForEach-Object { Where-Addr $_[0] }) 40
"== first $Focus frame (instruction pointer, else stack scan)"
Top ($res | ForEach-Object {
        $f = '(none)'
        foreach ($a in $_) { $w = Where-Addr $a; if ($w -like "$Focus+*") { $f = $w; break } }
        $f
    }) 40
'== modules anywhere in the sample'
Top ($res | ForEach-Object { $_ | ForEach-Object { (Where-Addr $_).Split('+')[0] } | Select-Object -Unique }) 15
'== syscall stubs'
$stubNames = @(
    'ntdll.dll:NtWaitForSingleObject', 'ntdll.dll:NtWaitForMultipleObjects', 'ntdll.dll:NtDelayExecution',
    'ntdll.dll:NtYieldExecution', 'ntdll.dll:NtWaitForAlertByThreadId', 'ntdll.dll:NtAlertThreadByThreadId',
    'ntdll.dll:NtRemoveIoCompletion', 'ntdll.dll:NtRemoveIoCompletionEx', 'ntdll.dll:NtSignalAndWaitForSingleObject',
    'ntdll.dll:NtWaitForWorkViaWorkerFactory', 'ntdll.dll:NtWaitForKeyedEvent', 'ntdll.dll:NtReleaseKeyedEvent',
    'ntdll.dll:NtSetEvent', 'ntdll.dll:NtResetEvent', 'ntdll.dll:NtSetTimerEx', 'ntdll.dll:NtQuerySystemTime',
    'ntdll.dll:NtFreeVirtualMemory', 'ntdll.dll:NtAllocateVirtualMemory', 'ntdll.dll:NtProtectVirtualMemory',
    'ntdll.dll:NtQueryVirtualMemory', 'ntdll.dll:NtDeviceIoControlFile', 'ntdll.dll:NtWriteFile', 'ntdll.dll:NtReadFile',
    'ntdll.dll:NtClose', 'ntdll.dll:NtFlushProcessWriteBuffers', 'ntdll.dll:NtQueryPerformanceCounter',
    'win32u.dll:NtGdiDdDDIEscape', 'win32u.dll:NtGdiDdDDIPresent', 'win32u.dll:NtGdiDdDDILock2', 'win32u.dll:NtGdiDdDDIUnlock2',
    'win32u.dll:NtGdiDdDDIWaitForSynchronizationObjectFromCpu', 'win32u.dll:NtGdiDdDDISignalSynchronizationObjectFromCpu',
    'win32u.dll:NtGdiDdDDIGetDeviceState', 'win32u.dll:NtGdiDdDDIMakeResident', 'win32u.dll:NtGdiDdDDIQueryAdapterInfo',
    'win32u.dll:NtGdiDdDDISubmitCommand', 'win32u.dll:NtGdiDdDDIRender', 'win32u.dll:NtGdiDdDDICreateAllocation',
    'win32u.dll:NtGdiDdDDIDestroyAllocation2', 'win32u.dll:NtGdiDdDDIPresentMultiPlaneOverlay3',
    'win32u.dll:NtGdiDdDDIGetScanLine', 'win32u.dll:NtGdiDdDDIWaitForVerticalBlankEvent2', 'win32u.dll:NtUserGetMessage',
    'win32u.dll:NtUserPeekMessage', 'win32u.dll:NtUserMsgWaitForMultipleObjectsEx')
$stubs = foreach ($n in $stubNames) {
    $dll, $fn = $n.Split(':')
    $a = [ThreadSample]::Export($dll, $fn)
    if ($a -ne 0) { [pscustomobject]@{ N = "$dll!$fn"; A = $a; W = (Where-Addr $a) } }
}
$seenIp = $res | ForEach-Object { $_[0] } | Sort-Object -Unique
foreach ($st in $stubs) {
    $hits = @($seenIp | Where-Object { $_ -ge $st.A -and $_ -lt $st.A + 0x20 })
    foreach ($h in $hits) { '{0} {1}' -f (Where-Addr $h), $st.N }
}
'== module bases'
$mods | Where-Object { $_.N -match 'helios|d3d11|dxgi|vulkan|rmclient|ntdll|win32u|KERNELBASE' } |
    ForEach-Object { '{0} {1:x}' -f $_.N, $_.B }
if ($Raw) {
    '== raw'
    for ($i = 0; $i -lt $res.Count; $i++) {
        $line = ($res[$i] | ForEach-Object { Where-Addr $_ }) -join ' '
        if ($Qpc) {
            $line = [string]::Format([cultureinfo]::InvariantCulture, '@{0:F3} ', $times[$i] * 1000.0 / $qpcFreq) + $line
        }
        $line
    }
}
