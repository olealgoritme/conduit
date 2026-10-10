# Mounts the host's shared folders (virtiofs devices tagged "conduit-NAME") as
# drive letters. Runs as SYSTEM from the "ConduitShares" startup task and keeps
# scanning, so a folder added while Windows runs appears within seconds.
#
# The VirtIO-FS service (virtiofs.exe) serves one device per process, so each
# tag gets its own `virtiofs.exe -t TAG -m X:`. The device can be opened any
# number of times, so an already mounted tag is told from a new one by the
# command line of the running virtiofs.exe processes: one tag, one process,
# one drive letter. Extra processes for a tag are stopped. Drive letters count
# down from Z: and are remembered per tag (HKLM\SOFTWARE\Conduit\ShareDrives),
# which the tray app also reads.
#
# Only virtiofs.exe processes that run the expected executable as SYSTEM
# count as mounts (and only those are ever stopped): one a user started
# cannot pose as a share or get a share's drive letter taken away.
#
# The mounts belong to SYSTEM, so every user of this Windows sees the shared
# folders as drives, with the same access to the host folders.
param(
    [int]$ScanSeconds = 5,
    [string]$VirtioFsExe = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$logDir = Join-Path $env:ProgramData "Conduit"
$logPath = Join-Path $logDir "shares.log"
$regPath = "HKLM:\SOFTWARE\Conduit\ShareDrives"

# The log folder (also the tray's): SYSTEM and administrators may change it,
# users may read it. A folder a user made beforehand gets these rules too.
function Protect-DataDirectory([string]$Dir) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    if ((Get-Item -LiteralPath $Dir -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "$Dir is a link, not a folder"
    }
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetOwner([Security.Principal.SecurityIdentifier]"S-1-5-32-544")
    $acl.SetAccessRuleProtection($true, $false)
    $inherit = [Security.AccessControl.InheritanceFlags]"ContainerInherit, ObjectInherit"
    foreach ($rule in @(@("S-1-5-18", "FullControl"), @("S-1-5-32-544", "FullControl"), @("S-1-5-32-545", "ReadAndExecute"))) {
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            [Security.Principal.SecurityIdentifier]$rule[0], $rule[1], $inherit, "None", "Allow")))
    }
    Set-Acl -LiteralPath $Dir -AclObject $acl
    & (Join-Path $env:SystemRoot "System32\icacls.exe") (Join-Path $Dir "*") /reset /T /L /C /Q | Out-Null
}
Protect-DataDirectory $logDir
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

    // Tags of all virtiofs devices.
    public static string[] Tags() {
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
                    IntPtr h = CreateFileW(path, 0xC0000000u, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
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

# Is this virtiofs.exe process one of ours: the expected executable, run by
# SYSTEM?
function Test-OurInstance($Process, [string]$Exe) {
    if (-not $Process.ExecutablePath -or
        -not [string]::Equals([IO.Path]::GetFullPath([string]$Process.ExecutablePath),
            [IO.Path]::GetFullPath($Exe), [StringComparison]::OrdinalIgnoreCase)) {
        return $false
    }
    # By SID: account names are localized.
    $owner = Invoke-CimMethod -InputObject $Process -MethodName GetOwnerSid -ErrorAction SilentlyContinue
    return ($owner -and $owner.ReturnValue -eq 0 -and $owner.Sid -eq "S-1-5-18")
}

# The running virtiofs.exe processes (ours only, see Test-OurInstance) that
# were given a tag: tag -> list of @{ Pid; Letter }, oldest first.
function Get-Instances([string]$Exe) {
    $by = @{}
    $procs = @(Get-CimInstance Win32_Process -Filter "Name='virtiofs.exe'" -ErrorAction Stop |
        Sort-Object CreationDate | Where-Object { Test-OurInstance $_ $Exe })
    foreach ($p in $procs) {
        $cl = [string]$p.CommandLine
        if ($cl -notmatch '\s-t\s+"?([^\s"]+)') { continue }
        $tag = $Matches[1]
        $letter = ""
        if ($cl -match '\s-m\s+"?([A-Za-z]:)') { $letter = $Matches[1].ToUpperInvariant() }
        if (-not $by.ContainsKey($tag)) { $by[$tag] = New-Object System.Collections.ArrayList }
        [void]$by[$tag].Add(@{ Pid = [int]$p.ProcessId; Letter = $letter })
    }
    return $by
}

function Get-Remembered([string]$Tag) {
    try { return [string](Get-ItemPropertyValue -LiteralPath $regPath -Name $Tag -ErrorAction Stop) } catch { return "" }
}

# One tag keeps one instance: the one on the remembered letter, else the
# oldest. The others are stale (an earlier scan mounted the tag again) and go.
function Remove-StaleInstances($By) {
    foreach ($tag in @($By.Keys)) {
        $list = @($By[$tag])
        $remembered = Get-Remembered $tag
        $keep = $list | Where-Object { $_.Letter -and $_.Letter -eq $remembered } | Select-Object -First 1
        if (-not $keep) { $keep = $list[0] }
        foreach ($i in $list) {
            if ($i.Pid -eq $keep.Pid) { continue }
            Write-Log "stopping stale virtiofs.exe for $tag at $($i.Letter) (pid $($i.Pid))"
            Stop-Process -Id $i.Pid -Force -ErrorAction SilentlyContinue
        }
        if ($keep.Letter -and $keep.Letter -ne $remembered) {
            if (-not (Test-Path -LiteralPath $regPath)) { New-Item -Path $regPath -Force | Out-Null }
            Set-ItemProperty -LiteralPath $regPath -Name $tag -Value $keep.Letter
            Write-Log "$tag is mounted at $($keep.Letter) (pid $($keep.Pid))"
        }
        $By[$tag] = @($keep)
    }
}

$started = @{}    # tag -> time of the last mount attempt
Write-Log "scanning for conduit-* virtiofs devices every $ScanSeconds s"
while ($true) {
    try {
        $exe = Find-VirtioFs
        if (-not $exe) {
            Write-Log "virtiofs.exe not found (install the VirtIO guest tools: virtio-win-guest-tools)"
        } else {
            $instances = Get-Instances $exe
            Remove-StaleInstances $instances
            foreach ($tag in [ConduitFs]::Tags()) {
                if ($tag -notlike "conduit-?*") { continue }
                if ($instances.ContainsKey($tag)) { continue }
                # A mount that fails right away is retried every 30 s, not every scan.
                $prev = $started[$tag]
                if ($prev -and ((Get-Date) - $prev).TotalSeconds -lt 30) { continue }
                $started[$tag] = Get-Date
                $letter = Get-DriveLetter $tag
                if (-not $letter) { Write-Log "no free drive letter for $tag"; continue }
                $p = Start-Process -FilePath $exe -ArgumentList @("-t", $tag, "-m", $letter) `
                    -WindowStyle Hidden -PassThru
                Write-Log "mounting $tag at $letter (pid $($p.Id))"
            }
        }
    } catch {
        Write-Log "scan failed: $($_.Exception.Message)"
    }
    Start-Sleep -Seconds $ScanSeconds
}
