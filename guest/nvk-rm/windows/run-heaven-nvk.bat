@echo off
rem Unigine Heaven (D3D11) on DXVK -> NVK -> RM, from an application-local copy
rem whose bin\ holds the stage-dxvk-app.sh files. Double-click it on the desktop,
rem or start it through a scheduled task (heaven-nvk-fps.ps1).
rem
rem   run-heaven-nvk.bat [HEAVEN_DIR [WIDTH HEIGHT [prof]]]
rem   default: C:\Users\Public\heaven-nvk  1600 900   (Medium, windowed)
rem   prof: librmclient writes its per-escape table to logs\rm-prof.csv
setlocal
set HEAVEN=%~1
if "%HEAVEN%"=="" set HEAVEN=C:\Users\Public\heaven-nvk
set W=%~2
if "%W%"=="" set W=1600
set H=%~3
if "%H%"=="" set H=900

set NVK_RM=1
set DXVK_LOG_PATH=%HEAVEN%\logs
set NVK_SHIM_LOG=%HEAVEN%\logs\nvk-shim.log
set NVK_SHIM_FRAMES=%HEAVEN%\logs\frames.csv
if /i "%~4"=="prof" set CRM_WIN_PROF_FILE=%HEAVEN%\logs\rm-prof.csv
if not exist "%HEAVEN%\logs" mkdir "%HEAVEN%\logs"
rem Extra per-experiment settings (e.g. set NVK_RM_WAIT_POLL_MS=1), if any
if exist "%HEAVEN%\env.cmd" call "%HEAVEN%\env.cmd"

cd /d "%HEAVEN%\bin"
start Heaven.exe -video_app direct3d11 -sound_app null -data_path ../ -engine_config ../data/heaven_4.0.cfg -system_script heaven/unigine.cpp -video_mode -1 -video_width %W% -video_height %H% -video_fullscreen 0 -video_multisample 0 -extern_define RELEASE,LANGUAGE_EN,QUALITY_MEDIUM,TESSELLATION_NORMAL
