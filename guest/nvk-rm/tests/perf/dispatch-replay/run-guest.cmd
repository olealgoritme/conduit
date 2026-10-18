@echo off
rem dispatch-replay guest run (NVK on RM). Replays every capture under captures\ on the first NVIDIA-vendor
rem Vulkan device (NVK): results are appended to guest.tsv, outputs go to out-guest\<capture>\.
rem Copy guest.tsv and out-guest\ back to the host and compare with compare.py.
setlocal
cd /d "%~dp0"
if not exist dispatch-replay.exe ( echo dispatch-replay.exe missing & exit /b 1 )
dispatch-replay.exe --list
for /d %%c in (captures\*) do (
  echo === %%~nxc
  dispatch-replay.exe -n 15 --csv guest.tsv --label guest-nvk --dump out-guest\%%~nxc %%c
)
echo done.
