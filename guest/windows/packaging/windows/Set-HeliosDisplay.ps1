<#
.SYNOPSIS
  Make the Helios adapter the ONLY active display, selected by adapter.

.DESCRIPTION
  After Helios binds, Windows keeps the Microsoft Basic Display adapter (the
  Bochs/std VGA or QXL device: PCI vendor 1234 or 1B36) as DISPLAY1 and adds Helios
  as an extended second monitor, so the desktop is not on the Helios screen.
  DisplaySwitch.exe cannot be used: its /internal and /external choices are
  Windows' own guess about which monitor is which, and on this device /external
  picked the Basic Display and /internal picked Helios.

  This script asks Windows for every display path (QueryDisplayConfig), finds the
  adapter whose PCI vendor is virtio (VEN_1AF4, which is Helios), and applies a
  topology containing only that adapter's path (SetDisplayConfig, saved to the
  display database, so it persists across reboots). It then reads the active
  topology back and fails if anything else is still active.

  SetDisplayConfig is picky, so several strategies are tried in turn. Each is
  VALIDATED first (SDC_VALIDATE, which changes nothing); the first one that
  validates is applied. The return code of every attempt is printed.

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
  Print the display paths, validate each strategy, change nothing. Exits nonzero
  if no strategy validates.

.NOTES
  Exit codes: 0 done / already correct / opted out / dry run with a valid
  strategy, 1 failed (including a dry run where nothing validates), 2 not in an
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

    // DISPLAYCONFIG_MODE_INFO is 64 bytes: type, id, adapter, then a 48-byte union
    // (twelve uints here). For a SOURCE mode (infoType 1) the union is
    // width, height, pixelFormat, position.x, position.y; other types are only
    // copied whole.
    [StructLayout(LayoutKind.Sequential)]
    public struct ModeInfo
    {
        public uint infoType; public uint id; public LUID adapterId;
        public uint u0, u1, u2, u3, u4, u5, u6, u7, u8, u9, u10, u11;
    }
    public const uint MODE_TYPE_SOURCE = 1;

    public class Config
    {
        public PathInfo[] Paths;
        public ModeInfo[] Modes;
    }

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
    public const uint PATH_ACTIVE = 1;
    public const uint MODE_IDX_INVALID = 0xFFFFFFFF;
    public const uint SDC_TOPOLOGY_SUPPLIED = 0x10;
    public const uint SDC_USE_SUPPLIED_DISPLAY_CONFIG = 0x20;
    public const uint SDC_VALIDATE = 0x40;
    public const uint SDC_APPLY = 0x80;
    public const uint SDC_SAVE_TO_DATABASE = 0x200;
    public const uint SDC_ALLOW_CHANGES = 0x400;
    public const uint SDC_ALLOW_PATH_ORDER_CHANGES = 0x2000;

    public static Config Query(bool activeOnly)
    {
        uint flags = activeOnly ? QDC_ONLY_ACTIVE_PATHS : QDC_ALL_PATHS;
        uint np, nm;
        int r = GetDisplayConfigBufferSizes(flags, out np, out nm);
        if (r != 0) throw new InvalidOperationException("GetDisplayConfigBufferSizes failed: " + r);
        PathInfo[] paths = new PathInfo[np];
        ModeInfo[] modes = new ModeInfo[nm];
        r = QueryDisplayConfig(flags, ref np, paths, ref nm, modes, IntPtr.Zero);
        if (r != 0) throw new InvalidOperationException("QueryDisplayConfig failed: " + r);
        Config c = new Config();
        c.Paths = new PathInfo[np];
        Array.Copy(paths, c.Paths, (int)np);
        c.Modes = new ModeInfo[nm];
        Array.Copy(modes, c.Modes, (int)nm);
        return c;
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

    // Nested struct fields cannot be assigned from PowerShell (it edits a copy),
    // so every path edit is done here.

    // A path as SetDisplayConfig wants it for a topology: active, status cleared,
    // source and target modes left for Windows to pick.
    public static PathInfo TopologyPath(PathInfo p)
    {
        p.flags = PATH_ACTIVE;
        p.sourceInfo.modeInfoIdx = MODE_IDX_INVALID;
        p.sourceInfo.statusFlags = 0;
        p.targetInfo.modeInfoIdx = MODE_IDX_INVALID;
        p.targetInfo.statusFlags = 0;
        return p;
    }

    // The path with its source and target mode re-based to a two-entry mode array
    // [source, target], plus that array; null if the path has no usable modes.
    public static bool WithOwnModes(Config c, int pathIndex, out PathInfo[] paths, out ModeInfo[] modes)
    {
        paths = null; modes = null;
        PathInfo p = c.Paths[pathIndex];
        uint s = p.sourceInfo.modeInfoIdx, t = p.targetInfo.modeInfoIdx;
        if (s == MODE_IDX_INVALID || t == MODE_IDX_INVALID || s >= c.Modes.Length || t >= c.Modes.Length)
            return false;
        modes = new ModeInfo[] { c.Modes[s], c.Modes[t] };
        if (modes[0].infoType == MODE_TYPE_SOURCE) { modes[0].u3 = 0; modes[0].u4 = 0; }
        p.sourceInfo.modeInfoIdx = 0;
        p.targetInfo.modeInfoIdx = 1;
        paths = new PathInfo[] { p };
        return true;
    }

    // Every path and every mode, unchanged, except: paths with keep[i] == false
    // lose PATH_ACTIVE (their mode indices stay valid into the same array), and
    // the source mode of every kept path moves to desktop position (0,0), so one
    // active source is at the origin once the others are gone.
    public static void ActivateOnly(Config c, bool[] keep, out PathInfo[] paths, out ModeInfo[] modes)
    {
        paths = (PathInfo[])c.Paths.Clone();
        modes = (ModeInfo[])c.Modes.Clone();
        for (int i = 0; i < paths.Length; i++)
        {
            if (!keep[i]) { paths[i].flags &= ~PATH_ACTIVE; continue; }
            uint s = paths[i].sourceInfo.modeInfoIdx;
            if (s != MODE_IDX_INVALID && s < modes.Length && modes[s].infoType == MODE_TYPE_SOURCE)
            {
                modes[s].u3 = 0;
                modes[s].u4 = 0;
            }
        }
    }

    // "WxH at (x,y)" for a path's source mode, or "no mode".
    public static string SourceModeText(Config c, int pathIndex)
    {
        uint s = c.Paths[pathIndex].sourceInfo.modeInfoIdx;
        if (s == MODE_IDX_INVALID || s >= c.Modes.Length || c.Modes[s].infoType != MODE_TYPE_SOURCE)
            return "no source mode";
        ModeInfo m = c.Modes[s];
        return m.u0 + "x" + m.u1 + " at (" + (int)m.u3 + "," + (int)m.u4 + ")";
    }

    public static PathInfo[] One(PathInfo p) { return new PathInfo[] { p }; }

    public static int Set(PathInfo[] paths, ModeInfo[] modes, uint flags)
    {
        uint nm = (modes == null) ? 0u : (uint)modes.Length;
        return SetDisplayConfig((uint)paths.Length, paths, nm, nm == 0 ? null : modes, flags);
    }
}
"@

$sdc = [HeliosDisplayConfig]

function Get-AdapterTable($paths) {
    $table = @{}
    foreach ($path in $paths) {
        foreach ($luid in @($path.sourceInfo.adapterId, $path.targetInfo.adapterId)) {
            $key = "{0}:{1}" -f $luid.HighPart, $luid.LowPart
            if (-not $table.ContainsKey($key)) { $table[$key] = [HeliosDisplayConfig]::AdapterPath($luid) }
        }
    }
    return $table
}

function Get-AdapterKey($luid) { return "{0}:{1}" -f $luid.HighPart, $luid.LowPart }

function Test-HeliosAdapterPath([string]$DevicePath) {
    # Helios is the virtio device: PCI vendor 1AF4. The Basic Display adapter is
    # Bochs/std VGA (1234) or QXL (1B36); do not rely on either of those.
    return $DevicePath -match "ven_1af4"
}

function Show-Paths($title, $config, $adapters) {
    Write-Host $title
    $i = 0
    foreach ($path in $config.Paths) {
        $isHelios = Test-HeliosAdapterPath $adapters[(Get-AdapterKey $path.sourceInfo.adapterId)]
        Write-Host ("  [{0}] adapter {1}{2} source {3} target {4} flags 0x{5:x} targetAvailable {6}; source mode {7}" -f `
            $i, (Get-AdapterKey $path.sourceInfo.adapterId), $(if ($isHelios) { " (Helios)" } else { "" }),
            $path.sourceInfo.id, $path.targetInfo.id, $path.flags, $path.targetInfo.targetAvailable,
            $sdc::SourceModeText($config, $i))
        $i++
    }
}

function Test-OnlyHeliosActive {
    $active = [HeliosDisplayConfig]::Query($true)
    if ($active.Paths.Count -eq 0) { return $false }
    $adapters = Get-AdapterTable $active.Paths
    foreach ($path in $active.Paths) {
        if (-not (Test-HeliosAdapterPath $adapters[(Get-AdapterKey $path.sourceInfo.adapterId)])) { return $false }
    }
    return $true
}

# ---- Wait for an available Helios display path ------------------------------
$deadline = (Get-Date).AddSeconds($WaitSeconds)
$chosen = $null
do {
    $all = [HeliosDisplayConfig]::Query($false)
    $adapters = Get-AdapterTable $all.Paths
    $heliosPaths = @($all.Paths | Where-Object {
        (Test-HeliosAdapterPath $adapters[(Get-AdapterKey $_.sourceInfo.adapterId)]) -and $_.targetInfo.targetAvailable -ne 0
    })
    if ($heliosPaths.Count -gt 0) { $chosen = $heliosPaths[0]; break }
    if ($DryRun) { break }
    Start-Sleep -Seconds 3
} while ((Get-Date) -lt $deadline)

Write-Host "Display adapters seen by Windows:"
foreach ($entry in $adapters.GetEnumerator()) { Write-Host ("  {0}  {1}" -f $entry.Key, $entry.Value) }

if (-not $chosen) {
    Write-Warning "No available display path on a Helios (VEN_1AF4) adapter. Is the Helios driver started?"
    exit 3
}

$active = [HeliosDisplayConfig]::Query($true)
Show-Paths "Active display paths now:" $active (Get-AdapterTable $active.Paths)

if (Test-OnlyHeliosActive) {
    Write-Host "Helios is already the only active display."
    exit 0
}

# ---- Strategies, each validated before it is applied ------------------------
$strategies = New-Object System.Collections.ArrayList

# D: keep EVERY active path and mode (so every mode index stays valid), clear
# PATH_ACTIVE on the non-Helios paths and put the Helios source at (0,0): the
# recipe that normally works when the other adapter is the primary at the origin.
$activeAdapters = Get-AdapterTable $active.Paths
$keep = New-Object 'bool[]' $active.Paths.Count
$anyHelios = $false
for ($i = 0; $i -lt $active.Paths.Count; $i++) {
    $keep[$i] = [bool](Test-HeliosAdapterPath $activeAdapters[(Get-AdapterKey $active.Paths[$i].sourceInfo.adapterId)])
    if ($keep[$i]) { $anyHelios = $true }
}
if ($anyHelios) {
    $dPaths = $null
    $dModes = $null
    $sdc::ActivateOnly($active, $keep, [ref]$dPaths, [ref]$dModes)
    [void]$strategies.Add([pscustomobject]@{
        Name = "D: all active paths and modes, others deactivated, Helios source at (0,0)"
        Paths = $dPaths; Modes = $dModes
        Flags = $sdc::SDC_USE_SUPPLIED_DISPLAY_CONFIG -bor $sdc::SDC_ALLOW_CHANGES })
}

# A: Helios is active already (as a second monitor): hand back only its active
# path with its own source and target mode, and let Windows drop the rest.
for ($i = 0; $i -lt $active.Paths.Count; $i++) {
    $a = Get-AdapterTable @($active.Paths[$i])
    if (-not (Test-HeliosAdapterPath $a[(Get-AdapterKey $active.Paths[$i].sourceInfo.adapterId)])) { continue }
    $ownPaths = $null
    $ownModes = $null
    if ($sdc::WithOwnModes($active, $i, [ref]$ownPaths, [ref]$ownModes)) {
        [void]$strategies.Add([pscustomobject]@{
            Name = "A: only Helios' active path with its own modes (source at 0,0)"
            Paths = $ownPaths; Modes = $ownModes
            Flags = $sdc::SDC_USE_SUPPLIED_DISPLAY_CONFIG -bor $sdc::SDC_ALLOW_CHANGES -bor $sdc::SDC_ALLOW_PATH_ORDER_CHANGES })
    }
    break
}
# B: the Helios path with no modes: Windows chooses them.
$topologyPaths = $sdc::One($sdc::TopologyPath($chosen))
[void]$strategies.Add([pscustomobject]@{
    Name = "B: Helios path, supplied display config, Windows picks modes"
    Paths = $topologyPaths; Modes = $null
    Flags = $sdc::SDC_USE_SUPPLIED_DISPLAY_CONFIG -bor $sdc::SDC_ALLOW_CHANGES -bor $sdc::SDC_ALLOW_PATH_ORDER_CHANGES })
# C: topology only.
[void]$strategies.Add([pscustomobject]@{
    Name = "C: Helios path as the supplied topology"
    Paths = $topologyPaths; Modes = $null
    Flags = $sdc::SDC_TOPOLOGY_SUPPLIED -bor $sdc::SDC_ALLOW_PATH_ORDER_CHANGES })

$winner = $null
foreach ($s in $strategies) {
    $rc = $sdc::Set($s.Paths, $s.Modes, ($s.Flags -bor $sdc::SDC_VALIDATE))
    Write-Host ("Validate {0}: {1}{2}" -f $s.Name, $rc, $(if ($rc -eq 0) { " (ok)" } else { "" }))
    if ($rc -eq 0 -and -not $winner) { $winner = $s }
}

if (-not $winner) {
    Write-Warning "No SetDisplayConfig strategy validated; the topology was not changed."
    exit 1
}
if ($DryRun) {
    Write-Host "Dry run: would apply strategy '$($winner.Name)'. Nothing was changed."
    exit 0
}

$rc = $sdc::Set($winner.Paths, $winner.Modes, ($winner.Flags -bor $sdc::SDC_APPLY -bor $sdc::SDC_SAVE_TO_DATABASE))
if ($rc -ne 0) {
    Write-Warning "SetDisplayConfig ($($winner.Name)) failed with $rc; the topology was not changed."
    exit 1
}
Start-Sleep -Seconds 2
if (-not (Test-OnlyHeliosActive)) {
    $after = [HeliosDisplayConfig]::Query($true)
    Show-Paths "Active display paths after applying:" $after (Get-AdapterTable $after.Paths)
    Write-Warning "SetDisplayConfig succeeded but a non-Helios display is still active."
    exit 1
}
Write-Host "Helios is now the only active display ($($winner.Name)); the topology is saved."
exit 0
