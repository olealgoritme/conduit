# Sets up a Windows VM to build the Helios guest driver locally, with the
# toolchain .github/workflows/windows.yml uses for its `driver` job (the WDDM
# KMD and the x64/x86 D3D11/12 UMDs), so a change can be built without a
# GitHub Actions run. Run as an administrator; every step is skipped when its
# tool is already there, so running it again finishes an interrupted setup.
#
#   powershell -ExecutionPolicy Bypass -File Setup-BuildVm.ps1 [-Root W:\]
#
# On C: (they cannot move): Visual Studio 2022 Build Tools (C++), the Windows
# SDK and WDK 10.0.26100. On -Root: LLVM, the Vulkan SDK, Python with Meson
# and Ninja, MSYS2 (widl), Rust (rustup and cargo homes), Git, and the build
# tree. Machine-wide environment variables point the tools at -Root.
# Versions follow windows.yml's env block; change both together.
param(
    [string]$Root = "W:\"
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$PythonVersion = "3.12.10"
$MesonVersion = "1.11.2"
$RustToolchain = "nightly-2026-07-14"
$CargoMakeVersion = "0.37.24"
$RustScriptVersion = "0.36.0"
$LlvmVersion = "22.1.8"
$VulkanSdkVersion = "1.4.350.0"

$Root = $Root.TrimEnd('\') + '\'
$tools = Join-Path $Root "tools"
$downloads = Join-Path $Root "downloads"
New-Item -ItemType Directory -Force -Path $tools, $downloads, (Join-Path $Root "src"), (Join-Path $Root "helios-build"), (Join-Path $Root "out") | Out-Null

function Step([string]$Name) { Write-Host "==> $Name" }

function Get-File([string]$Url, [string]$Name) {
    $path = Join-Path $downloads $Name
    if (-not (Test-Path -LiteralPath $path)) {
        Write-Host "    downloading $Url"
        Invoke-WebRequest -Uri $Url -OutFile "$path.part" -UseBasicParsing
        Move-Item -LiteralPath "$path.part" -Destination $path
    }
    return $path
}

function Invoke-Checked([string]$File, [string[]]$Arguments) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -Wait -PassThru -NoNewWindow
    # 3010: success, reboot required (Visual Studio, SDK installers).
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) {
        throw "$File exited with $($p.ExitCode)"
    }
}

# Runs a native tool and returns its exit code. Windows PowerShell 5 turns a
# native program's stderr into a terminating error under
# $ErrorActionPreference = "Stop" when output is redirected (MSYS2's first
# start, pip and cargo all write progress there), so it is relaxed around the
# call and the exit code is what decides.
function Invoke-Native([string]$File, [string[]]$Arguments) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $File @Arguments 2>&1 | ForEach-Object { Write-Host "    $_" }
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $old
    }
}

function Set-MachineEnv([string]$Name, [string]$Value) {
    [Environment]::SetEnvironmentVariable($Name, $Value, "Machine")
    Set-Item -Path "Env:$Name" -Value $Value
}

function Add-MachinePath([string]$Dir) {
    $path = [Environment]::GetEnvironmentVariable("Path", "Machine")
    if (-not ($path.Split(';') -contains $Dir)) {
        [Environment]::SetEnvironmentVariable("Path", "$path;$Dir", "Machine")
    }
    if (-not ($env:Path.Split(';') -contains $Dir)) { $env:Path = "$env:Path;$Dir" }
}

# Builds read and write many small files; Defender scanning each one makes
# them several times slower. Only the build drive's tree is excluded.
Step "Defender exclusion for $Root"
try { Add-MpPreference -ExclusionPath $Root } catch { Write-Host "    skipped: $_" }

Step "Visual Studio 2022 Build Tools (C++ x86/x64)"
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$haveVs = (Test-Path $vswhere) -and (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
if (-not $haveVs) {
    $vs = Get-File "https://aka.ms/vs/17/release/vs_BuildTools.exe" "vs_BuildTools.exe"
    Invoke-Checked $vs @("--quiet", "--wait", "--norestart", "--nocache",
        "--add", "Microsoft.VisualStudio.Workload.VCTools",
        "--add", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "--add", "Microsoft.VisualStudio.Component.VC.ATL",
        "--add", "Microsoft.VisualStudio.Component.Windows11SDK.26100",
        "--includeRecommended")
}

Step "Windows SDK and WDK 10.0.26100"
$kitsBin = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
$haveWdk = Get-ChildItem -LiteralPath $kitsBin -Filter "Inf2Cat.exe" -File -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $haveWdk) {
    foreach ($id in @("Microsoft.WindowsSDK.10.0.26100", "Microsoft.WindowsWDK.10.0.26100")) {
        $code = Invoke-Native "winget" @("install", "--id", $id, "--exact", "--silent", "--disable-interactivity",
            "--accept-package-agreements", "--accept-source-agreements")
        if ($code -ne 0 -and $code -ne -1978335189) {   # 0x8A15002B: already installed
            throw "winget $id exited with $code"
        }
    }
}

Step "PowerShell 7 (the build scripts run under pwsh, as in windows.yml)"
$pwsh = Join-Path $env:ProgramFiles "PowerShell\7\pwsh.exe"
if (-not (Test-Path $pwsh)) {
    $release = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/PowerShell/PowerShell/releases/latest"
    $asset = $release.assets | Where-Object { $_.name -match '^PowerShell-.*-win-x64\.msi$' } | Select-Object -First 1
    $msi = Get-File $asset.browser_download_url $asset.name
    Invoke-Checked "msiexec.exe" @("/i", "`"$msi`"", "/quiet", "/norestart", "ADD_PATH=1")
}

Step "Git"
$gitDir = Join-Path $tools "git"
if (-not (Test-Path (Join-Path $gitDir "cmd\git.exe"))) {
    $release = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/git-for-windows/git/releases/latest"
    $asset = $release.assets | Where-Object { $_.name -match '^PortableGit-.*-64-bit\.7z\.exe$' } | Select-Object -First 1
    $git = Get-File $asset.browser_download_url $asset.name
    Invoke-Checked $git @("-y", "-o$gitDir")
}
Add-MachinePath (Join-Path $gitDir "cmd")

Step "LLVM $LlvmVersion"
$llvmDir = Join-Path $tools "LLVM"
if (-not (Test-Path (Join-Path $llvmDir "bin\clang-cl.exe"))) {
    $llvm = Get-File "https://github.com/llvm/llvm-project/releases/download/llvmorg-$LlvmVersion/LLVM-$LlvmVersion-win64.exe" "LLVM-$LlvmVersion-win64.exe"
    Invoke-Checked $llvm @("/S", "/D=$llvmDir")
}
Add-MachinePath (Join-Path $llvmDir "bin")
Set-MachineEnv "LIBCLANG_PATH" (Join-Path $llvmDir "bin")

Step "Python $PythonVersion, Meson $MesonVersion, Ninja"
$pyDir = Join-Path $tools "Python312"
if (-not (Test-Path (Join-Path $pyDir "python.exe"))) {
    $py = Get-File "https://www.python.org/ftp/python/$PythonVersion/python-$PythonVersion-amd64.exe" "python-$PythonVersion-amd64.exe"
    Invoke-Checked $py @("/quiet", "InstallAllUsers=1", "PrependPath=0", "Include_test=0", "Include_launcher=0", "TargetDir=$pyDir")
}
# Ahead of the Microsoft Store "python.exe" alias in WindowsApps.
$machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
foreach ($d in @((Join-Path $pyDir "Scripts"), $pyDir)) {
    if (-not ($machinePath.Split(';') -contains $d)) { $machinePath = "$d;$machinePath" }
}
[Environment]::SetEnvironmentVariable("Path", $machinePath, "Machine")
$env:Path = "$pyDir;$(Join-Path $pyDir 'Scripts');$env:Path"
$code = Invoke-Native (Join-Path $pyDir "python.exe") @("-m", "pip", "install", "--disable-pip-version-check", "--quiet", "meson==$MesonVersion", "ninja")
if ($code -ne 0) { throw "pip install meson ninja failed" }

Step "MSYS2 (widl)"
$msysDir = Join-Path $tools "msys64"
if (-not (Test-Path (Join-Path $msysDir "ucrt64\bin\widl.exe"))) {
    if (-not (Test-Path (Join-Path $msysDir "usr\bin\bash.exe"))) {
        $msys = Get-File "https://github.com/msys2/msys2-installer/releases/download/nightly-x86_64/msys2-base-x86_64-latest.sfx.exe" "msys2-base.sfx.exe"
        Invoke-Checked $msys @("-y", "-o$tools")
    }
    $bash = Join-Path $msysDir "usr\bin\bash.exe"
    Invoke-Native $bash @("-lc", "true") | Out-Null                     # first start: profile
    # The keyring explicitly: a first start that was cut off leaves it half
    # made, and pacman then calls every database signature invalid.
    $code = Invoke-Native $bash @("-lc", "rm -f /var/lib/pacman/sync/*.sig; pacman-key --init && pacman-key --populate msys2")
    if ($code -ne 0) { throw "pacman-key setup exited with $code" }
    Invoke-Native $bash @("-lc", "pacman -Syu --noconfirm") | Out-Null  # may stop after a core update;
    Invoke-Native $bash @("-lc", "pacman -Syu --noconfirm") | Out-Null  # the second run finishes it
    $code = Invoke-Native $bash @("-lc", "pacman -S --noconfirm --needed mingw-w64-ucrt-x86_64-tools")
    if ($code -ne 0) { throw "pacman -S mingw-w64-ucrt-x86_64-tools exited with $code" }
    if (-not (Test-Path (Join-Path $msysDir "ucrt64\bin\widl.exe"))) { throw "widl.exe missing after the MSYS2 install" }
}
# Appended last: MSYS2's own tools must not shadow the native ones.
Add-MachinePath (Join-Path $msysDir "ucrt64\bin")

Step "Vulkan SDK $VulkanSdkVersion"
$vkRoot = Join-Path $tools "VulkanSDK\$VulkanSdkVersion"
& (Join-Path $PSScriptRoot "..\windows\Install-VulkanSdk.ps1") -Version $VulkanSdkVersion -InstallRoot $vkRoot
Set-MachineEnv "VULKAN_SDK" $vkRoot
Set-MachineEnv "VK_SDK_PATH" $vkRoot
Add-MachinePath (Join-Path $vkRoot "Bin")

Step "Rust $RustToolchain (rust-src, i686-pc-windows-msvc)"
Set-MachineEnv "RUSTUP_HOME" (Join-Path $tools "rustup")
Set-MachineEnv "CARGO_HOME" (Join-Path $tools "cargo")
Add-MachinePath (Join-Path $tools "cargo\bin")
$rustup = Join-Path $tools "cargo\bin\rustup.exe"
if (-not (Test-Path $rustup)) {
    $init = Get-File "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe" "rustup-init.exe"
    Invoke-Checked $init @("-y", "--no-modify-path", "--profile", "minimal", "--default-toolchain", $RustToolchain)
}
$code = Invoke-Native $rustup @("toolchain", "install", $RustToolchain, "--profile", "minimal", "--component", "rust-src", "--target", "i686-pc-windows-msvc")
if ($code -ne 0) { throw "rustup toolchain install failed" }
$code = Invoke-Native $rustup @("default", $RustToolchain)
if ($code -ne 0) { throw "rustup default failed" }
Set-MachineEnv "RUST_TOOLCHAIN" $RustToolchain

Step "cargo-make $CargoMakeVersion, rust-script $RustScriptVersion"
$cargo = Join-Path $tools "cargo\bin\cargo.exe"
if (-not (Test-Path (Join-Path $tools "cargo\bin\cargo-make.exe"))) {
    $code = Invoke-Native $cargo @("install", "cargo-make", "--locked", "--version", $CargoMakeVersion)
    if ($code -ne 0) { throw "cargo install cargo-make failed" }
}
if (-not (Test-Path (Join-Path $tools "cargo\bin\rust-script.exe"))) {
    $code = Invoke-Native $cargo @("install", "rust-script", "--locked", "--version", $RustScriptVersion)
    if ($code -ne 0) { throw "cargo install rust-script failed" }
}

Step "Check"
$missing = @()
foreach ($c in @("pwsh.exe", "git.exe", "clang-cl.exe", "llvm-lib.exe", "llvm-readobj.exe", "python.exe", "meson.exe", "ninja.exe",
                 "widl.exe", "glslangValidator.exe", "cargo.exe", "cargo-make.exe", "rust-script.exe", "rustup.exe")) {
    $found = Get-Command $c -ErrorAction SilentlyContinue
    Write-Host ("    {0,-22} {1}" -f $c, $(if ($found) { $found.Source } else { "MISSING" }))
    if (-not $found) { $missing += $c }
}
if ($missing) { throw "missing after setup: $($missing -join ', ')" }
Write-Host "Build VM ready. Build with ci\vm\Build-InVm.ps1 (or guest/windows/ci/vm/win-build.sh from the Linux host)."
