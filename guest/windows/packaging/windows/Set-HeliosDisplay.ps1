<#
.SYNOPSIS
  Make the Helios adapter the ONLY active display, selected by adapter.

.DESCRIPTION
  After Helios binds, Windows keeps the Microsoft Basic Display / QXL adapter as
  DISPLAY1 and adds Helios as an extended second monitor, so the desktop is not on
  the Helios screen. DisplaySwitch.exe cannot be used: its /internal and /external
  choices are Windows' own guess about which monitor is which, and on this device
  /external picked the Basic Display and /internal picked Helios.

  This script asks Windows for every display path (QueryDisplayConfig), finds the
  adapter whose PCI vendor is virtio (VEN_1AF4, which is Helios; QXL is VEN_1B36),
  and applies a topology containing only that adapter's path (SetDisplayConfig,
  saved to the display database, so it persists across reboots). It then reads the
  active topology back and fails if anything else is still active.

  It does NOT disable the Basic Display device. That adapter stays enabled and is
  only inactive in the topology, so it remains the picture if Helios fails.

  It must run in the interactive user session: SetDisplayConfig does nothing from
  session 0 (a service, SSH or SYSTEM task). The installer registers a logon task
  that runs it there. It is idempotent: if Helios is already the only active
  display it changes nothing.

  Opt out with DWORD HKLM\SOFTWARE\Helios\ManageDisplay = 0.

.PARAMETER WaitSeconds
  How long to wait for Helios to show up as an available display path (it is not
  there until the driver has started). Default 60.

.PARAMETER DryRun
  Print the display paths and what would be done; change nothing.

.NOTES
  Exit codes: 0 done or already correct (or opted out), 1 failed, 2 not in an
  interactive session, 3 Helios adapter or display never appeared.
#>
param(
    [int]$WaitSeconds = 60,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$manage = (Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Helios" -Name "ManageDisplay" -ErrorAction SilentlyContinue)
if ($manage -and $manage.PSObject.Properties.Name -contains "ManageDisplay" -and [int]$manage.ManageDisplay -eq 0) {
    Write-Host "HKLM\SOFTWARE\Helios\ManageDisplay is 0: leaving the display topology alone."
    exit 0
}

if ([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) {
    Write-Warning "Running in session 0: SetDisplayConfig has no effect here. Run this in the logged-on user's session."
    exit 2
}

Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class HeliosDisplayConfig
{
    [StructLayout(LayoutKind.Sequential)]
    public struct LUID { public uint LowPart; public int HighPart; }

    [StructLayout(LayoutKind.Sequential)]
    public struct SourceInfo
    {
        public LUID adapterId; public uint id; public uint modeInfoIdx; public uint statusFlags;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct Rational { public uint Numerator; public uint Denominator; }

    [StructLayout(LayoutKind.Sequential)]
    public struct TargetInfo
    {
        public LUID adapterId; public uint id; public uint modeInfoIdx;
        public uint outputTechnology; public uint rotation; public uint scaling;
        public Rational refreshRate; public uint scanLineOrdering;
        public int targetAvailable; public uint statusFlags;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PathInfo { public SourceInfo sourceInfo; public TargetInfo targetInfo; public uint flags; }

    // DISPLAYCONFIG_MODE_INFO is 64 bytes: type, id, adapter, then a 48-byte union.
    [StructLayout(LayoutKind.Sequential, Size = 64)]
    public struct ModeInfo { public uint infoType; public uint id; public LUID adapterId; }

    [DllImport("user32.dll")]
    static extern int GetDisplayConfigBufferSizes(uint flags, out uint numPaths, out uint numModes);

    [DllImport("user32.dll")]
    static extern int QueryDisplayConfig(uint flags, ref uint numPaths,
        [Out] PathInfo[] paths, ref uint numModes, [Out] ModeInfo[] modes, IntPtr topologyId);

    [DllImport("user32.dll")]
    static extern int SetDisplayConfig(uint numPaths, [In] PathInfo[] paths,
        uint numModes, [In] ModeInfo[] modes, uint flags);

    [DllImport("user32.dll")]
    static extern int DisplayConfigGetDeviceInfo(IntPtr requestPacket);

    const uint QDC_ALL_PATHS = 1;
    const uint QDC_ONLY_ACTIVE_PATHS = 2;
    const uint PATH_ACTIVE = 1;
    const uint MODE_IDX_INVALID = 0xFFFFFFFF;
    const uint SDC_TOPOLOGY_SUPPLIED = 0x10;
    const uint SDC_APPLY = 0x80;
    const uint SDC_SAVE_TO_DATABASE = 0x200;
    const uint SDC_ALLOW_PATH_ORDER_CHANGES = 0x2000;
    const uint SDC_VALIDATE = 0x40;

    public static PathInfo[] Query(bool activeOnly)
    {
        uint np, nm;
        int r = GetDisplayConfigBufferSizes(activeOnly ? QDC_ONLY_ACTIVE_PATHS : QDC_ALL_PATHS, out np, out nm);
        if (r != 0) throw new InvalidOperationException("GetDisplayConfigBufferSizes failed: " + r);
        PathInfo[] paths = new PathInfo[np];
        ModeInfo[] modes = new ModeInfo[nm];
        r = QueryDisplayConfig(activeOnly ? QDC_ONLY_ACTIVE_PATHS : QDC_ALL_PATHS,
            ref np, paths, ref nm, modes, IntPtr.Zero);
        if (r != 0) throw new InvalidOperationException("QueryDisplayConfig failed: " + r);
        PathInfo[] trimmed = new PathInfo[np];
        Array.Copy(paths, trimmed, (int)np);
        return trimmed;
    }

    // DISPLAYCONFIG_ADAPTER_NAME: header (type, size, adapterId, id) = 20 bytes,
    // then WCHAR adapterDevicePath[128].
    public static string AdapterPath(LUID adapter)
    {
        int size = 20 + 128 * 2;
        IntPtr p = Marshal.AllocHGlobal(size);
        try
        {
            for (int i = 0; i < size; i++) Marshal.WriteByte(p, i, 0);
            Marshal.WriteInt32(p, 0, 4);          // DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME
            Marshal.WriteInt32(p, 4, size);
            Marshal.WriteInt32(p, 8, (int)adapter.LowPart);
            Marshal.WriteInt32(p, 12, adapter.HighPart);
            if (DisplayConfigGetDeviceInfo(p) != 0) return "";
            return Marshal.PtrToStringUni(new IntPtr(p.ToInt64() + 20));
        }
        finally { Marshal.FreeHGlobal(p); }
    }

    public static bool SameAdapter(LUID a, LUID b) { return a.LowPart == b.LowPart && a.HighPart == b.HighPart; }

    // Apply a topology made of exactly these paths, saved to the display database.
    public static int ApplyOnly(PathInfo[] chosen, bool validateOnly)
    {
        PathInfo[] paths = new PathInfo[chosen.Length];
        for (int i = 0; i < chosen.Length; i++)
        {
            paths[i] = chosen[i];
            paths[i].flags |= PATH_ACTIVE;
            paths[i].sourceInfo.modeInfoIdx = MODE_IDX_INVALID;
            paths[i].targetInfo.modeInfoIdx = MODE_IDX_INVALID;
        }
        uint flags = SDC_TOPOLOGY_SUPPLIED | SDC_ALLOW_PATH_ORDER_CHANGES;
        flags |= validateOnly ? SDC_VALIDATE : (SDC_APPLY | SDC_SAVE_TO_DATABASE);
        return SetDisplayConfig((uint)paths.Length, paths, 0, null, flags);
    }
}
"@

function Get-HeliosDisplayState {
    $all = [HeliosDisplayConfig]::Query($false)
    $adapters = @{}
    foreach ($path in $all) {
        $key = "{0}:{1}" -f $path.sourceInfo.adapterId.HighPart, $path.sourceInfo.adapterId.LowPart
        if (-not $adapters.ContainsKey($key)) {
            $adapters[$key] = [HeliosDisplayConfig]::AdapterPath($path.sourceInfo.adapterId)
        }
    }
    [pscustomobject]@{ Paths = $all; Adapters = $adapters }
}

function Test-HeliosAdapterPath([string]$DevicePath) {
    # Helios is the virtio device: PCI vendor 1AF4 (QXL / Basic Display is 1B36).
    return $DevicePath -match "ven_1af4"
}

$deadline = (Get-Date).AddSeconds($WaitSeconds)
$chosen = $null
do {
    $state = Get-HeliosDisplayState
    $heliosPaths = @($state.Paths | Where-Object {
        $key = "{0}:{1}" -f $_.sourceInfo.adapterId.HighPart, $_.sourceInfo.adapterId.LowPart
        (Test-HeliosAdapterPath $state.Adapters[$key]) -and $_.targetInfo.targetAvailable -ne 0
    })
    if ($heliosPaths.Count -gt 0) {
        # One display: the first available Helios path.
        $chosen = $heliosPaths[0]
        break
    }
    if ($DryRun) { break }
    Start-Sleep -Seconds 3
} while ((Get-Date) -lt $deadline)

Write-Host "Display adapters seen by Windows:"
foreach ($entry in $state.Adapters.GetEnumerator()) { Write-Host ("  {0}  {1}" -f $entry.Key, $entry.Value) }

if (-not $chosen) {
    Write-Warning "No available display path on a Helios (VEN_1AF4) adapter. Is the Helios driver started?"
    exit 3
}

function Test-OnlyHeliosActive {
    $active = [HeliosDisplayConfig]::Query($true)
    if ($active.Count -eq 0) { return $false }
    $adapters = @{}
    foreach ($path in $active) {
        $key = "{0}:{1}" -f $path.sourceInfo.adapterId.HighPart, $path.sourceInfo.adapterId.LowPart
        if (-not $adapters.ContainsKey($key)) {
            $adapters[$key] = [HeliosDisplayConfig]::AdapterPath($path.sourceInfo.adapterId)
        }
        if (-not (Test-HeliosAdapterPath $adapters[$key])) { return $false }
    }
    return $true
}

if (Test-OnlyHeliosActive) {
    Write-Host "Helios is already the only active display."
    exit 0
}

if ($DryRun) {
    Write-Host "Dry run: would activate only the Helios path (target id $($chosen.targetInfo.id))."
    $rc = [HeliosDisplayConfig]::ApplyOnly(@($chosen), $true)
    Write-Host "SetDisplayConfig validation returned $rc."
    exit 0
}

$rc = [HeliosDisplayConfig]::ApplyOnly(@($chosen), $false)
if ($rc -ne 0) {
    Write-Warning "SetDisplayConfig failed with $rc; the topology was not changed."
    exit 1
}
Start-Sleep -Seconds 2
if (-not (Test-OnlyHeliosActive)) {
    Write-Warning "SetDisplayConfig succeeded but a non-Helios display is still active."
    exit 1
}
Write-Host "Helios is now the only active display; the topology is saved."
exit 0
