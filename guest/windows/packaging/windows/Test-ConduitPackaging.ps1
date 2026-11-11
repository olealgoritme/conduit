[CmdletBinding()]
param()

# Regression checks for the Conduit parts of the package scripts: every script
# parses, the uninstall order is safe, the data folder gets its ACL, locked
# files are scheduled for deletion, and only SYSTEM's virtiofs.exe counts as a
# mount. Works in a scratch folder under %TEMP%; needs administrator rights
# (ACL owner changes, MoveFileEx at restart), as on the CI runners.
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
. (Join-Path $here "Helios-PackageCommon.ps1")

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Conduit packaging check failed: $Message" }
}

# Every package script parses.
$scripts = @(Get-ChildItem -LiteralPath $here -Filter "*.ps1" -File) +
    @(Get-ChildItem -LiteralPath (Join-Path $here "..\..\ci\windows") -Filter "*.ps1" -File) +
    @(Get-ChildItem -LiteralPath (Join-Path $here "..\..\tools\conduit-shell-menu") -Filter "*.ps1" -File)
foreach ($script in $scripts) {
    $tokens = $null; $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($script.FullName, [ref]$tokens, [ref]$errors)
    Assert-True (@($errors).Count -eq 0) "$($script.Name) does not parse: $(@($errors) -join '; ')"
}

function Get-FunctionFromScript([string]$Path, [string]$Name) {
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$null, [ref]$null)
    $f = $ast.Find({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $Name }, $true)
    Assert-True ($null -ne $f) "$Name is missing from $Path"
    return $f.Extent.Text
}

# Uninstall: the state check comes before any change, and the mount loop is
# stopped before virtiofs.exe is.
$uninstall = Get-Content -LiteralPath (Join-Path $here "Uninstall-Helios.ps1") -Raw
$stateCheck = $uninstall.IndexOf('throw "No package-managed Helios installation')
$firstChange = $uninstall.IndexOf("Unregister-ScheduledTask")
Assert-True ($stateCheck -gt 0 -and $stateCheck -lt $firstChange) "uninstall changes things before the state check"
$stopTask = $uninstall.IndexOf('Stop-ScheduledTask -TaskName "ConduitShares"')
$stopLoop = $uninstall.IndexOf("*Mount-ConduitShares.ps1*")
$stopFs = $uninstall.IndexOf("Name='virtiofs.exe'")
Assert-True ($stopTask -gt 0 -and $stopLoop -gt $stopTask -and $stopFs -gt $stopLoop) "uninstall stops virtiofs.exe before the mount loop"
Assert-True ($uninstall.Contains("VirtioFsSvcStartMode")) "uninstall does not restore VirtioFsSvc"

# Install: the context menu files have their own error handling, and the
# logon task allows one copy per user.
$install = Get-Content -LiteralPath (Join-Path $here "Install-Helios.ps1") -Raw
Assert-True ($install.Contains("-MultipleInstances Parallel")) "the tray task is not Parallel"
Assert-True ($install.Contains("The context menu command was not installed (the tray app was)")) "context menu copy failures are not separate"
Assert-True ($install.Contains("The virtio-input driver was not installed")) "a vioinput failure is not just a warning"

# NVIDIA UMD mode: version forms and checks, and the INF sections it reads.
Assert-True ((ConvertTo-HeliosNvidiaVersion "32.0.16.1088") -eq "610.88") "a driver file version is not converted to NVIDIA's short form"
Assert-True ((ConvertTo-HeliosNvidiaVersion "610.57.04") -eq "610.57.04") "a short version is changed"
Assert-True ((Get-HeliosNvidiaBranch "610.57.04") -eq "610") "the branch of a host RM version is wrong"
Assert-HeliosNvidiaVersion "32.0.16.1088" "610.88" "610.57.04"
foreach ($case in @(@("32.0.16.1074", "610.88", "610.57.04"), @("610.88", "610.88", "615.10"), @("610.88", "610.88", ""))) {
    $threw = $false
    try { Assert-HeliosNvidiaVersion $case[0] $case[1] $case[2] } catch { $threw = $true }
    Assert-True $threw "package $($case[0]) passed against NvUmdVersion $($case[1]) and host RM '$($case[2])'"
}
$inx = Read-HeliosInf (Join-Path $here "..\..\kmd_render\helios_kmd_render.inx")
$rtxValues = @(Get-HeliosInfHkrValues $inx @(Get-HeliosInfDirective $inx "Helios_NVIDIA_Install" "AddReg") "C:\DS")
$heliosValues = @(Get-HeliosInfHkrValues $inx @(Get-HeliosInfDirective $inx "Helios_Install" "AddReg") "C:\DS")
$rtxUmd = $rtxValues | Where-Object { $_.Name -eq "UserModeDriverName" }
$heliosUmd = $heliosValues | Where-Object { $_.Name -eq "UserModeDriverName" }
Assert-True ($rtxUmd -and $rtxUmd.Data[0] -eq "C:\DS\nvldumdx.dll") "the INF's RTX section does not register nvldumdx.dll"
Assert-True ($heliosUmd -and $heliosUmd.Data[0] -eq "<>" -and $heliosUmd.Data[1] -eq "C:\DS\helios_umd.dll") "the INF's Helios section does not register helios_umd.dll"
$pin = Get-HeliosInfHkrValues $inx @("Helios_NVIDIA_Service_AddReg") "" | Where-Object { $_.Subkey -eq "Parameters" -and $_.Name -eq "NvUmdVersion" }
Assert-True ($pin -and (ConvertTo-HeliosNvidiaVersion $pin.Data[0])) "the INF does not set Parameters\NvUmdVersion"
# The KMD reads it as REG_SZ in NVIDIA's short form (umd_version.rs): no
# AddReg type bits (FLG_ADDREG_TYPE_SZ is 0) and a three-digit major with two
# digits of minor.
Assert-True (($pin.Flags -band 0x00FF0001) -eq 0) "the INF does not write Parameters\NvUmdVersion as REG_SZ"
Assert-True ($pin.Data[0] -match '^\d{3}\.\d{2}$') "the INF's NvUmdVersion '$($pin.Data[0])' is not NVIDIA's short form"

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("conduit-packaging-{0}" -f [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    # The data folder: protected, owned by Administrators, users read-only,
    # and what was inside before takes those rules.
    $mount = Join-Path $here "Mount-ConduitShares.ps1"
    Invoke-Expression (Get-FunctionFromScript $mount "Protect-DataDirectory")
    foreach ($case in @(
        @{ Name = "common"; Run = { param($d) Protect-ConduitDataDirectory $d } },
        @{ Name = "mount"; Run = { param($d) Protect-DataDirectory $d } }
    )) {
        $dir = Join-Path $scratch "data-$($case.Name)"
        New-Item -ItemType Directory -Path $dir | Out-Null
        $planted = Join-Path $dir "ctl.log"
        Set-Content -LiteralPath $planted -Value "x"
        $fileAcl = Get-Acl -LiteralPath $planted
        $fileAcl.SetAccessRuleProtection($true, $false)
        $fileAcl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            [Security.Principal.SecurityIdentifier]"S-1-5-32-545", "FullControl", "Allow")))
        Set-Acl -LiteralPath $planted -AclObject $fileAcl
        & $case.Run $dir
        foreach ($path in @($dir, $planted)) {
            $acl = Get-Acl -LiteralPath $path
            $users = @($acl.Access | Where-Object {
                $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -eq "S-1-5-32-545" })
            Assert-True ($users.Count -ge 1) "$($case.Name): Users have no entry on $path"
            foreach ($u in $users) {
                $write = [int]$u.FileSystemRights -band [int][Security.AccessControl.FileSystemRights]"Write, Delete, ChangePermissions, TakeOwnership"
                Assert-True ($write -eq 0) "$($case.Name): Users can change $path ($($u.FileSystemRights))"
            }
        }
        $acl = Get-Acl -LiteralPath $dir
        Assert-True $acl.AreAccessRulesProtected "$($case.Name): the folder inherits from ProgramData"
        $owner = (New-Object Security.Principal.NTAccount($acl.Owner)).Translate([Security.Principal.SecurityIdentifier]).Value
        Assert-True ($owner -eq "S-1-5-32-544") "$($case.Name): owner is $($acl.Owner)"
    }

    # A free file goes now; a locked one is scheduled for the next restart.
    $free = Join-Path $scratch "free.dll"
    Set-Content -LiteralPath $free -Value "x"
    Assert-True (Remove-HeliosFileOrScheduleAtReboot $free) "a free file was not removed"
    Assert-True (-not (Test-Path -LiteralPath $free)) "a free file is still there"
    $locked = Join-Path $scratch "locked.dll"
    Set-Content -LiteralPath $locked -Value "x"
    $handle = [IO.File]::Open($locked, "Open", "Read", "None")
    try {
        Assert-True (-not (Remove-HeliosFileOrScheduleAtReboot $locked 3>$null)) "a locked file was reported removed"
    } finally { $handle.Dispose() }
    $pending = @((Get-Item -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager").GetValue("PendingFileRenameOperations", @()))
    Assert-True (@($pending | Where-Object { $_ -like "*$([IO.Path]::GetFileName($scratch))\locked.dll" }).Count -ge 1) "the locked file is not scheduled for deletion"

    # Only SYSTEM's virtiofs.exe of the expected path is a mount.
    Invoke-Expression (Get-FunctionFromScript $mount "Test-OurInstance")
    $me = Get-CimInstance Win32_Process -Filter "ProcessId=$PID"
    Assert-True (-not (Test-OurInstance $me $me.ExecutablePath)) "a process of this user counts as SYSTEM's"
    Assert-True (-not (Test-OurInstance $me (Join-Path $scratch "virtiofs.exe"))) "another executable counts"
    $system = Get-CimInstance Win32_Process -Filter "Name='svchost.exe'" | Where-Object {
        $_.ExecutablePath -and (Invoke-CimMethod -InputObject $_ -MethodName GetOwnerSid).Sid -eq "S-1-5-18"
    } | Select-Object -First 1
    Assert-True ($null -ne $system) "no SYSTEM svchost.exe to compare with"
    if ($system) {
        Assert-True (Test-OurInstance $system $system.ExecutablePath) "a SYSTEM process of the expected path does not count"
        Assert-True (-not (Test-OurInstance $system $me.ExecutablePath)) "a SYSTEM process of another path counts"
    }
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
Write-Host "Conduit packaging checks passed."
