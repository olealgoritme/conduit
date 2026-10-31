@echo off
rem nvddi install (run elevated, from the folder with nvddi.sys, nvddictl.exe, nvddi-test.cer).
rem Needs: bcdedit /set testsigning on + reboot, Secure Boot off, Memory Integrity (HVCI) off.
setlocal
cd /d "%~dp0"
net session >nul 2>&1 || (echo run this from an elevated prompt & exit /b 1)

bcdedit /enum {current} | findstr /i "testsigning.*Yes" >nul
if errorlevel 1 (
  echo ERROR: test signing is off. Run: bcdedit /set testsigning on   then reboot.
  exit /b 1
)

echo [1/5] certificate
certutil -f -addstore Root nvddi-test.cer || exit /b 1
certutil -f -addstore TrustedPublisher nvddi-test.cer || exit /b 1

echo [2/5] driver file
copy /y nvddi.sys "%SystemRoot%\System32\drivers\nvddi.sys" || exit /b 1
if not exist C:\nvddi mkdir C:\nvddi

echo [3/5] service
rem  default: system start, group "Video Init" (loads near dxgkrnl), normal
rem  profile: 8 MB rings (256 MB total), MaxFails 2, HealthySeconds 90.
rem  "install.cmd boot" = capture profile: boot start, so nvddi attaches to
rem  \Device\DxgKrnl and wraps nvlddmkm's FIRST bring-up with no GPU restart,
rem  32 MB rings (512 MB total: no drops during bring-up) and a loose fail
rem  guard (MaxFails 50, HealthySeconds 15) for runs that reboot every few
rem  minutes. A failed hooked StartDevice still disables nvddi at once.
rem  Re-run "install.cmd" without "boot" to return to the normal profile.
set START=system
set BOOT=0
set RINGKB=8192
set TOTALMB=256
set MAXFAILS=2
set HEALTHY=90
if /i "%~1"=="boot" ( set START=boot& set BOOT=1& set RINGKB=32768& set TOTALMB=512& set MAXFAILS=50& set HEALTHY=15)
sc query nvddi >nul 2>&1 && sc stop nvddi >nul 2>&1
sc query nvddi >nul 2>&1 || sc create nvddi type= kernel start= %START% error= ignore group= "Video Init" binPath= "System32\drivers\nvddi.sys" DisplayName= "nvddi WDDM DDI tracer"
sc config nvddi start= %START% error= ignore group= "Video Init" binPath= "System32\drivers\nvddi.sys" >nul

echo [4/5] parameters
set P=HKLM\SYSTEM\CurrentControlSet\Services\nvddi\Parameters
reg add %P% /f >nul
reg add %P% /v Enabled /t REG_DWORD /d 1 /f >nul
reg add %P% /v FailCount /t REG_DWORD /d 0 /f >nul
reg query %P% /v Level >nul 2>&1 || reg add %P% /v Level /t REG_DWORD /d 3 /f >nul
reg query %P% /v Target >nul 2>&1 || reg add %P% /v Target /t REG_SZ /d nvlddmkm /f >nul
reg query %P% /v LogDir >nul 2>&1 || reg add %P% /v LogDir /t REG_SZ /d "\??\C:\nvddi" /f >nul
reg add %P% /v BootStart /t REG_DWORD /d %BOOT% /f >nul
reg add %P% /v RingKB /t REG_DWORD /d %RINGKB% /f >nul
reg add %P% /v TotalRingMB /t REG_DWORD /d %TOTALMB% /f >nul
reg add %P% /v MaxFails /t REG_DWORD /d %MAXFAILS% /f >nul
reg add %P% /v HealthySeconds /t REG_DWORD /d %HEALTHY% /f >nul
echo   profile: start=%START% RingKB=%RINGKB% TotalRingMB=%TOTALMB% MaxFails=%MAXFAILS% HealthySeconds=%HEALTHY%
reg query %P% /v ResolveGpuVa >nul 2>&1 || reg add %P% /v ResolveGpuVa /t REG_DWORD /d 1 /f >nul
rem HookImports needs HVCI (Memory Integrity) OFF; off by default
reg query %P% /v HookImports >nul 2>&1 || reg add %P% /v HookImports /t REG_DWORD /d 0 /f >nul

echo [5/5] load now
sc start nvddi
nvddictl.exe status
echo.
if "%BOOT%"=="1" (
  echo Boot-start capture profile set. Reboot once; nvddi attaches before nvlddmkm
  echo and records the whole first bring-up with no GPU restart.
) else (
  echo Loaded. If nvlddmkm registered before nvddi, restart the GPU to trace it:
  echo   powershell -ExecutionPolicy Bypass -File restart-gpu.ps1
  echo Or re-run "install.cmd boot" for first-bring-up capture from the next boot.
)
