# Mounts the host's shared folders (virtiofs devices tagged "conduit-NAME") as
# drive letters. Runs as SYSTEM from the "ConduitShares" startup task and keeps
# scanning, so a folder added while Windows runs appears within seconds.
#
# The VirtIO-FS service (virtiofs.exe) serves one device per process, so each
# tag gets its own `virtiofs.exe -t TAG -m X:`. A device that a running
# virtiofs.exe holds cannot be opened a second time; that is how an already
# mounted tag is told from a new one. Drive letters count down from Z: and are
# remembered per tag (HKLM\SOFTWARE\Conduit\ShareDrives), which the tray app
# also reads.
param(
    [int]$ScanSeconds = 5,
    [string]$VirtioFsExe = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$logDir = Join-Path $env:ProgramData "Conduit"
$logPath = Join-Path $logDir "shares.log"
$regPath = "HKLM:\SOFTWARE\Conduit\ShareDrives"
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
if ((Test-Path -LiteralPath $logPath) -and (Get-Item -LiteralPath $logPath).Length -gt 256KB) {
    Remove-Item -LiteralPath $logPath -Force -ErrorAction SilentlyContinue
}
function Write-Log([string]$Message) {
    $line = "{0:s} {1}" -f (Get-Date), $Message
    Add-Content -LiteralPath $logPath -Value $line -ErrorAction SilentlyContinue
}

Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class ConduitFs {
    [StructLayout(LayoutKind.Sequential)]
    struct DevIf { public int cbSize; public Guid InterfaceClassGuid; public int Flags; public IntPtr Reserved; }

    [DllImport("setupapi.dll", SetLastError = true)]
    static extern IntPtr SetupDiGetClassDevsW(ref Guid g, IntPtr e, IntPtr h, int flags);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiEnumDeviceInterfaces(IntPtr set, IntPtr info, ref Guid g, int index, ref DevIf d);
    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool SetupDiGetDeviceInterfaceDetailW(IntPtr set, ref DevIf d, IntPtr detail, int size, out int required, IntPtr info);
    [DllImport("setupapi.dll", SetLastError = true)]
    static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr sa, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(IntPtr h, uint code, IntPtr inBuf, int inSize, byte[] outBuf, int outSize, out int returned, IntPtr overlapped);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr h);

    static readonly Guid VirtFs = new Guid("a1b0cbf2-f488-4b29-b89f-16c78e68c737");
    const uint IOCTL_VIRTFS_GET_VOLUME_NAME = 0x226000;

    // Tags of the virtiofs devices nothing has open (not mounted yet).
    public static string[] FreeTags() {
        List<string> tags = new List<string>();
        Guid g = VirtFs;
        IntPtr set = SetupDiGetClassDevsW(ref g, IntPtr.Zero, IntPtr.Zero, 0x12);
        if (set == new IntPtr(-1)) return tags.ToArray();
        try {
            for (int i = 0; ; i++) {
                DevIf d = new DevIf();
                d.cbSize = Marshal.SizeOf(typeof(DevIf));
                if (!SetupDiEnumDeviceInterfaces(set, IntPtr.Zero, ref g, i, ref d)) break;
                int need;
                SetupDiGetDeviceInterfaceDetailW(set, ref d, IntPtr.Zero, 0, out need, IntPtr.Zero);
                if (need <= 0) continue;
                IntPtr buf = Marshal.AllocHGlobal(need);
                try {
                    Marshal.WriteInt32(buf, IntPtr.Size == 8 ? 8 : 6);
                    if (!SetupDiGetDeviceInterfaceDetailW(set, ref d, buf, need, out need, IntPtr.Zero)) continue;
                    string path = Marshal.PtrToStringUni(new IntPtr(buf.ToInt64() + 4));
                    // Exclusive, like virtiofs.exe: fails while a service holds the device.
                    IntPtr h = CreateFileW(path, 0xC0000000u, 0, IntPtr.Zero, 3, 0, IntPtr.Zero);
                    if (h == new IntPtr(-1)) continue;
                    try {
                        byte[] name = new byte[512];
                        int got;
                        if (DeviceIoControl(h, IOCTL_VIRTFS_GET_VOLUME_NAME, IntPtr.Zero, 0, name, name.Length, out got, IntPtr.Zero)) {
                            string s = Encoding.Unicode.GetString(name, 0, got);
                            int nul = s.IndexOf('\0');
                            if (nul >= 0) s = s.Substring(0, nul);
                            tags.Add(s);
                        }
                    } finally { CloseHandle(h); }
                } finally { Marshal.FreeHGlobal(buf); }
            }
        } finally { SetupDiDestroyDeviceInfoList(set); }
        return tags.ToArray();
    }
}
"@

function Find-VirtioFs {
    if ($VirtioFsExe -and (Test-Path -LiteralPath $VirtioFsExe -PathType Leaf)) { return $VirtioFsExe }
    $svc = Get-CimInstance Win32_Service -Filter "Name='VirtioFsSvc'" -ErrorAction SilentlyContinue
    if ($svc -and $svc.PathName -match '^\s*"([^"]+)"|^\s*(\S+)') {
        $p = if ($Matches[1]) { $Matches[1] } else { $Matches[2] }
        if (Test-Path -LiteralPath $p -PathType Leaf) { return $p }
    }
    foreach ($p in @(
        (Join-Path $env:ProgramFiles "Virtio-Win\VioFS\virtiofs.exe"),
        (Join-Path $env:ProgramFiles "Virtio-Win\virtiofs.exe")
    )) {
        if (Test-Path -LiteralPath $p -PathType Leaf) { return $p }
    }
    return $null
}

function Get-DriveLetter([string]$Tag) {
    if (-not (Test-Path -LiteralPath $regPath)) { New-Item -Path $regPath -Force | Out-Null }
    $key = Get-Item -LiteralPath $regPath
    $mine = @{}
    foreach ($n in $key.GetValueNames()) { $mine[$n] = [string]$key.GetValue($n) }
    $used = @([IO.DriveInfo]::GetDrives() | ForEach-Object { $_.Name.Substring(0, 2).ToUpperInvariant() })
    $taken = @($mine.GetEnumerator() | Where-Object { $_.Key -ne $Tag } | ForEach-Object { $_.Value })
    if ($mine.ContainsKey($Tag)) {
        $remembered = $mine[$Tag]
        # A letter taken by something that is not a share of ours since: pick again.
        if ($used -notcontains $remembered) { return $remembered }
    }
    foreach ($c in [char[]]"ZYXWVUTSRQPONMLKJIHGFED") {
        $l = "$c`:"
        if ($used -notcontains $l -and $taken -notcontains $l) {
            Set-ItemProperty -LiteralPath $regPath -Name $Tag -Value $l
            return $l
        }
    }
    return $null
}

$started = @{}    # tag -> @{ Process; At }
Write-Log "scanning for conduit-* virtiofs devices every $ScanSeconds s"
while ($true) {
    try {
        $exe = Find-VirtioFs
        if (-not $exe) {
            Write-Log "virtiofs.exe not found (install the VirtIO guest tools: virtio-win-guest-tools)"
        } else {
            foreach ($tag in [ConduitFs]::FreeTags()) {
                if ($tag -notlike "conduit-?*") { continue }
                $prev = $started[$tag]
                if ($prev -and ((Get-Date) - $prev.At).TotalSeconds -lt 30) { continue }
                $letter = Get-DriveLetter $tag
                if (-not $letter) { Write-Log "no free drive letter for $tag"; continue }
                $p = Start-Process -FilePath $exe -ArgumentList @("-t", $tag, "-m", $letter) `
                    -WindowStyle Hidden -PassThru
                $started[$tag] = @{ Process = $p; At = Get-Date }
                Write-Log "mounting $tag at $letter (pid $($p.Id))"
            }
        }
    } catch {
        Write-Log "scan failed: $($_.Exception.Message)"
    }
    Start-Sleep -Seconds $ScanSeconds
}
