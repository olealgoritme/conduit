# H.264 decode on NVK-on-RM through FFmpeg's Vulkan hwaccel (patch 0035), checked
# frame by frame against the software decode.
#
#   vk-video-decode.ps1 [-Dir C:\Users\Public\vid] [-Clip base] [-Mode hw|bench]
#
# Dir holds: ffmpeg.exe and its DLLs (a shared Windows build with --enable-vulkan,
# e.g. BtbN's win64-lgpl-shared), vulkan-1.dll built from vulkan_shim.c (FFmpeg
# loads vulkan-1.dll from its own directory, so it only sees NVK),
# vulkan_nouveau.dll built with -Dvideo-codecs=h264dec, librmclient.dll,
# test-<Clip>.mp4 and sw-<Clip>.md5 (`ffmpeg -i test-<Clip>.mp4 -f framemd5
# sw-<Clip>.md5`, made anywhere: H.264 decoding is bit-exact by definition).
#
# hw:    decode, download, compare per-frame MD5s ("frames N/N, bit-exact N")
# bench: decode only, frames stay in VRAM; prints FFmpeg's -benchmark times
param([string]$Dir = "C:\Users\Public\vid", [string]$Clip = "base", [string]$Mode = "hw")
$ErrorActionPreference = "Continue"
Set-Location $Dir
$env:NVK_RM = "1"
$env:NVK_EXPERIMENTAL = "video"
$env:NVK_SHIM_LOG = "$Dir\shim.log"
$ffargs = @("-y", "-hide_banner", "-loglevel", "warning", "-init_hw_device", "vulkan=vk:0",
            "-hwaccel", "vulkan", "-hwaccel_device", "vk")
if ($Mode -eq "bench") {
  $ffargs[3] = "info"
  $ffargs += @("-nostats", "-benchmark", "-hwaccel_output_format", "vulkan",
               "-i", "test-$Clip.mp4", "-f", "null", "-")
} else {
  $ffargs += @("-i", "test-$Clip.mp4", "-pix_fmt", "yuv420p", "-f", "framemd5", "nvk-$Clip.md5")
}
Remove-Item "nvk-$Clip.md5" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath .\ffmpeg.exe -ArgumentList $ffargs -NoNewWindow -PassThru `
       -RedirectStandardError "ff-$Clip-$Mode.err" -RedirectStandardOutput "ff-$Clip-$Mode.out"
if (-not $p.WaitForExit(90000)) { "TIMEOUT, killing pid $($p.Id)"; Stop-Process -Id $p.Id -Force }
Get-Content "ff-$Clip-$Mode.err" -Tail 25
if ($Mode -ne "bench") {
  $sw = @(Get-Content "sw-$Clip.md5" | Where-Object { $_ -notmatch '^#' })
  $hw = @()
  if (Test-Path "nvk-$Clip.md5") { $hw = @(Get-Content "nvk-$Clip.md5" | Where-Object { $_ -notmatch '^#' }) }
  $same = 0
  for ($i = 0; $i -lt [Math]::Min($sw.Count, $hw.Count); $i++) { if ($sw[$i] -eq $hw[$i]) { $same++ } }
  "${Clip}: frames $($hw.Count)/$($sw.Count), bit-exact $same"
}
