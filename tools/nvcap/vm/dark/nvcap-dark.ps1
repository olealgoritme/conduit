# nvcap-dark.ps1: keep the passed-through NVIDIA GPU's display outputs dark.
#
# Run in the logged-on desktop session (SetDisplayConfig needs it): the
# night scripts register it as a logon task and a 5-minute repeat task, and
# also run it once after every boot.
#
#   -Mode topology  (default) deactivate every active display path whose
#                   adapter is the NVIDIA GPU (VEN_10DE) with SetDisplayConfig,
#                   saved to the topology database, as long as at least one
#                   other path (QEMU VGA) stays active; otherwise fall back
#                   to dpms
#   -Mode dpms      SC_MONITORPOWER off (monitors sleep until input or the
#                   next mode set)
#   -Mode status    only log the active paths
# -Log FILE appends the log there as well (default: stdout only).
#
# Trade-off of topology mode: with the NVIDIA output inactive, desktop and
# test windows are shown through the QEMU VGA, so NVIDIA's own scanout and
# flip programming is not exercised after logon (the boot-time mode set still
# is) and a fullscreen switch has no NVIDIA output to take.
param(
    [ValidateSet('topology', 'dpms', 'status')] [string]$Mode = 'topology',
    [string]$Log = ''
)
$ErrorActionPreference = 'Stop'

function Say([string]$m) {
    $line = '[{0}] {1}' -f (Get-Date -Format 'HH:mm:ss'), $m
    [Console]::Out.WriteLine($line)
    if ($Log) { Add-Content -Path $Log -Value $line -ErrorAction SilentlyContinue }
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class NvcapDisplay {
    [StructLayout(LayoutKind.Sequential)] public struct LUID { public uint Low; public int High; }
    [StructLayout(LayoutKind.Sequential)] public struct RATIONAL { public uint N; public uint D; }
    [StructLayout(LayoutKind.Sequential)] public struct PATH_SOURCE {
        public LUID adapterId; public uint id; public uint modeInfoIdx; public uint statusFlags;
    }
    [StructLayout(LayoutKind.Sequential)] public struct PATH_TARGET {
        public LUID adapterId; public uint id; public uint modeInfoIdx; public uint outputTechnology;
        public uint rotation; public uint scaling; public RATIONAL refreshRate;
        public uint scanLineOrdering; public int targetAvailable; public uint statusFlags;
    }
    [StructLayout(LayoutKind.Sequential)] public struct PATH_INFO {
        public PATH_SOURCE sourceInfo; public PATH_TARGET targetInfo; public uint flags;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] public struct ADAPTER_NAME {
        public uint type; public uint size; public LUID adapterId; public uint id;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string path;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] public struct TARGET_NAME {
        public uint type; public uint size; public LUID adapterId; public uint id;
        public uint flags; public uint outputTechnology; public ushort edidManufactureId;
        public ushort edidProductCodeId; public uint connectorInstance;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string friendlyName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string devicePath;
    }

    public const uint QDC_ONLY_ACTIVE_PATHS = 2;
    public const uint PATH_ACTIVE = 1;
    public const uint MODE_IDX_INVALID = 0xffffffff;
    public const uint SDC_USE_SUPPLIED_DISPLAY_CONFIG = 0x20, SDC_APPLY = 0x80,
                      SDC_SAVE_TO_DATABASE = 0x200, SDC_ALLOW_CHANGES = 0x400;
    const int MODE_INFO_SIZE = 64;   // DISPLAYCONFIG_MODE_INFO

    [DllImport("user32.dll")] static extern int GetDisplayConfigBufferSizes(uint flags, out uint np, out uint nm);
    [DllImport("user32.dll")] static extern int QueryDisplayConfig(uint flags, ref uint np,
        [In, Out] PATH_INFO[] paths, ref uint nm, IntPtr modes, IntPtr topology);
    [DllImport("user32.dll")] static extern int SetDisplayConfig(uint np, [In] PATH_INFO[] paths,
        uint nm, IntPtr modes, uint flags);
    [DllImport("user32.dll")] static extern int DisplayConfigGetDeviceInfo(ref ADAPTER_NAME r);
    [DllImport("user32.dll")] static extern int DisplayConfigGetDeviceInfo(ref TARGET_NAME r);
    [DllImport("user32.dll")] static extern bool PostMessage(IntPtr h, uint msg, IntPtr wp, IntPtr lp);

    public static PATH_INFO[] Paths;
    public static IntPtr Modes = IntPtr.Zero;
    public static uint NumModes;

    public static int Query() {
        uint np, nm;
        int rc = GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, out np, out nm);
        if (rc != 0) return rc;
        Paths = new PATH_INFO[np];
        if (Modes != IntPtr.Zero) Marshal.FreeHGlobal(Modes);
        Modes = Marshal.AllocHGlobal((int)Math.Max(nm, 1) * MODE_INFO_SIZE);
        rc = QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, ref np, Paths, ref nm, Modes, IntPtr.Zero);
        if (rc == 0) { Array.Resize(ref Paths, (int)np); NumModes = nm; }
        return rc;
    }

    public static string AdapterPath(LUID id) {
        ADAPTER_NAME r = new ADAPTER_NAME();
        r.type = 4; r.size = (uint)Marshal.SizeOf(typeof(ADAPTER_NAME)); r.adapterId = id;
        return DisplayConfigGetDeviceInfo(ref r) == 0 ? r.path : "?";
    }

    public static string TargetName(LUID id, uint target) {
        TARGET_NAME r = new TARGET_NAME();
        r.type = 2; r.size = (uint)Marshal.SizeOf(typeof(TARGET_NAME)); r.adapterId = id; r.id = target;
        return DisplayConfigGetDeviceInfo(ref r) == 0 ? r.friendlyName : "?";
    }

    // Deactivate the given paths, keep the rest; returns the SetDisplayConfig status.
    public static int Deactivate(bool[] off) {
        PATH_INFO[] p = (PATH_INFO[])Paths.Clone();
        for (int i = 0; i < p.Length; i++) {
            if (!off[i]) continue;
            p[i].flags &= ~PATH_ACTIVE;
            p[i].sourceInfo.modeInfoIdx = MODE_IDX_INVALID;
            p[i].targetInfo.modeInfoIdx = MODE_IDX_INVALID;
        }
        return SetDisplayConfig((uint)p.Length, p, NumModes, Modes,
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE);
    }

    public static void MonitorsOff() {
        // HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2 = off; posted, never blocks
        PostMessage((IntPtr)0xffff, 0x0112, (IntPtr)0xF170, (IntPtr)2);
    }
}
'@

function Show-Paths([string]$what) {
    $rc = [NvcapDisplay]::Query()
    if ($rc -ne 0) { Say "$what QueryDisplayConfig failed: $rc"; return $null }
    $nv = @()
    $i = 0
    foreach ($p in [NvcapDisplay]::Paths) {
        $ad = [NvcapDisplay]::AdapterPath($p.targetInfo.adapterId)
        $isnv = $ad -match 'VEN_10DE'
        $nv += $isnv
        $name = [NvcapDisplay]::TargetName($p.targetInfo.adapterId, $p.targetInfo.id)
        Say ("$what path {0}: target {1} '{2}' available={3} nvidia={4} adapter {5}" -f `
             $i, $p.targetInfo.id, $name, $p.targetInfo.targetAvailable, $isnv, $ad)
        $i++
    }
    Say "$what $($nv.Count) active path(s), $(@($nv | Where-Object { $_ }).Count) on NVIDIA"
    return , $nv
}

try {
    $nv = Show-Paths 'before:'
    if ($Mode -eq 'status' -or $null -eq $nv) { exit 0 }
    if ($Mode -eq 'dpms') {
        [NvcapDisplay]::MonitorsOff(); Say 'dpms: SC_MONITORPOWER off posted'; exit 0
    }
    $nvCount = @($nv | Where-Object { $_ }).Count
    if ($nvCount -eq 0) { Say 'topology: no active NVIDIA output, nothing to do'; exit 0 }
    if ($nvCount -eq $nv.Count) {
        Say 'topology: every active path is on NVIDIA (no QEMU VGA path to keep): dpms fallback'
        [NvcapDisplay]::MonitorsOff(); Say 'dpms: SC_MONITORPOWER off posted'; exit 0
    }
    $rc = [NvcapDisplay]::Deactivate([bool[]]$nv)
    Say "topology: SetDisplayConfig (NVIDIA paths inactive, saved) returned $rc"
    if ($rc -ne 0) {
        [NvcapDisplay]::MonitorsOff(); Say 'dpms fallback: SC_MONITORPOWER off posted'; exit 0
    }
    Start-Sleep -Seconds 2
    $null = Show-Paths 'after:'
} catch {
    Say "error: $($_.Exception.Message)"
    try { [NvcapDisplay]::MonitorsOff(); Say 'dpms fallback: SC_MONITORPOWER off posted' } catch { }
    exit 1
}
