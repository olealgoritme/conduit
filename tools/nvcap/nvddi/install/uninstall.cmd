@echo off
rem nvddi uninstall (run elevated).  nvddi.sys cannot unload while Windows runs:
rem this disables it now and removes it; the image leaves memory at the next reboot.
setlocal
net session >nul 2>&1 || (echo run this from an elevated prompt & exit /b 1)
set P=HKLM\SYSTEM\CurrentControlSet\Services\nvddi\Parameters
nvddictl.exe stop 2>nul
reg add %P% /v Enabled /t REG_DWORD /d 0 /f >nul 2>&1
sc config nvddi start= disabled >nul 2>&1
sc delete nvddi
del /f "%SystemRoot%\System32\drivers\nvddi.sys" 2>nul
if exist "%SystemRoot%\System32\drivers\nvddi.sys" echo note: driver file in use, it is gone after the reboot once deleted again
certutil -delstore TrustedPublisher "nvddi test signing" >nul 2>&1
certutil -delstore Root "nvddi test signing" >nul 2>&1
echo nvddi removed. Reboot to unload it (logs stay in C:\nvddi).
