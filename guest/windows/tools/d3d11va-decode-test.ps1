# D3D11VA H.264 decode on DXVK -> NVK on RM, app-local (no driver install):
# d3d11va_decode_test.exe and FFmpeg's d3d11va hwaccel, each frame's MD5
# compared with the software decode.
#
#   d3d11va-decode-test.ps1 [-Dir C:\Users\Public\vdec] [-Clip base] [-Mode hw|bench] [-Tool app|ffmpeg]
#
# Dir holds, next to each other:
#   d3d11.dll, dxgi.dll          the Helios DXVK fork with third_party/patches/dxvk
#                                0001+0002, built with MinGW (docs/d3d11-video-decode.md)
#   vulkan-1.dll                 guest/nvk-rm/windows/vulkan_shim.c (only NVK is visible)
#   vulkan_nouveau.dll,          NVK on RM with patch 0035, built with
#   librmclient.dll              MESON_ARGS=-Dvideo-codecs=h264dec
#   d3d11va_decode_test.exe      tools/d3d11va_decode_test.c
#   ffmpeg.exe + its DLLs        a shared Windows FFmpeg (BtbN win64-lgpl-shared)
#   test-<Clip>.mp4, sw-<Clip>.md5   `ffmpeg -i test-<Clip>.mp4 -f framemd5 sw-<Clip>.md5`
#
# hw:    decode, read back, compare ("frames N/N, bit-exact N"; the test app also
#        checks the video processor: "blt ok N/N")
# bench: decode only, frames stay on the GPU
param([string]$Dir = "C:\Users\Public\vdec", [string]$Clip = "base",
      [string]$Mode = "hw", [string]$Tool = "app")
$ErrorActionPreference = "Continue"
Set-Location $Dir
$env:NVK_RM = "1"
$env:NVK_EXPERIMENTAL = "video"
$env:DXVK_LOG_PATH = $Dir
$env:DXVK_LOG_LEVEL = "info"

if ($Tool -eq "app") {
  $exe = ".\d3d11va_decode_test.exe"
  $a = @("test-$Clip.mp4", "sw-$Clip.md5")
  if ($Mode -eq "bench") { $a += "-bench" }
} else {
  $exe = ".\ffmpeg.exe"
  $a = @("-y", "-hide_banner", "-loglevel", "warning", "-hwaccel", "d3d11va")
  if ($Mode -eq "bench") {
    $a[3] = "info"
    $a += @("-nostats", "-benchmark", "-hwaccel_output_format", "d3d11", "-i", "test-$Clip.mp4", "-f", "null", "-")
  } else {
    $a += @("-i", "test-$Clip.mp4", "-pix_fmt", "yuv420p", "-f", "framemd5", "d3d-$Clip.md5")
  }
}
$p = Start-Process -FilePath $exe -ArgumentList $a -NoNewWindow -PassThru `
       -RedirectStandardError "$Tool-$Clip.err" -RedirectStandardOutput "$Tool-$Clip.out"
if (-not $p.WaitForExit(120000)) { "TIMEOUT, killing pid $($p.Id)"; Stop-Process -Id $p.Id -Force }
Get-Content "$Tool-$Clip.out"
Get-Content "$Tool-$Clip.err" -Tail 10
if ($Tool -eq "ffmpeg" -and $Mode -ne "bench") {
  $sw = @(Get-Content "sw-$Clip.md5" | Where-Object { $_ -notmatch '^#' })
  $hw = @()
  if (Test-Path "d3d-$Clip.md5") { $hw = @(Get-Content "d3d-$Clip.md5" | Where-Object { $_ -notmatch '^#' }) }
  $same = 0
  for ($i = 0; $i -lt [Math]::Min($sw.Count, $hw.Count); $i++) { if ($sw[$i] -eq $hw[$i]) { $same++ } }
  "${Clip}: frames $($hw.Count)/$($sw.Count), bit-exact $same"
}
