# win11 test harness

The scripts the Windows work was tested with on the development host (RTX
5090, win11 guest at 5120x1440@240). They are kept as they ran, so they hold
that host's layout: a working directory (`$VMTEST_DIR`, default
`~/.cache/conduit-vmtest`; the ssh config `t/sshcfg` and the result folders live in it), the guest user
(set `WIN_SSH=user@127.0.0.1`, SSH on port 2222) and the guest folders
(`C:\Users\Public\t`, `C:\Users\Public\heaven-umd`, `C:\Users\Public\lvl5`).
Adjust those before using them elsewhere. Context: [docs/HANDOFF.md](../../../../docs/HANDOFF.md).

| Script | What |
|---|---|
| `win11-cycle.sh` | The only way the VM was restarted: graceful shutdown (flush, forced after 90 s), backend stopped, optional new backend / QEMU, `PRESTART="cmd"` hook for host installs while the VM is off, hugepage reservation, start, `oom_score_adj -900` for QEMU, then checks binary, flags, conduit-venus, SSH and the Helios mode. `--verify` runs only the checks. |
| `watch-win11.sh` | Watchdog (run as a monitor while anything runs in the guest): reboots, new dumps, TDRs, app hangs, DWM stalls, host disk space. |
| `longloop.sh` | Long-running version of the watchdog; exits on a reboot alert. |
| `install.sh oemNN.inf` | Install a staged driver package, restart DWM and the shell, print driver version, mode and knobs. A new package: `pnputil /add-driver W:\...\helios_kmd_render.inf /install`. |
| `hvwin.sh API [ENV]` | Windowed Unigine Heaven (`direct3d11`, `opengl`, ...); env `WW WH TESS QUAL DXCFG`. |
| `stages.sh VMNAME [SECS]` | Per-frame stage timing of the windowed copy and the foreign flip (`conduit trace VMNAME stages` with the guest's `StgRing` read over SSH every second; needs `StageTrace=1` in the guest): the stage table, the raw collection and a Perfetto trace in `$VMTEST_DIR/win/stages-<time>/`. |
| `blrow.sh LABEL ENV Knob=Val...` | One A/B row for the windowed (composed) Present: set KMD knobs, restart the device, fresh DWM + shell, Heaven, PresentMon 10 s, screenshot, KMD counters (`PrDdiBlt*`, `BltMirror*`, `BltAsync*`, `Gb*`). |
| `gbrows.sh` | The three guest-blob rows (GuestBlob 0, 1, 1 + BltAsync + ForeignCopy). Loops that split arguments must stay in bash scripts: the agent shell is zsh and does not word-split. |
| `stress.ps1`, `stressrun.sh` | 10-minute window stress in the user session (move, resize, minimise/restore, open/close windows) with counters before and after. |
| `pass.sh` | A clean performance pass. |
| `abrun.sh`, `abrow.sh` | Older A/B rows (flip knobs). |
| `shot.ps1` | In-session screenshot (run through `lvl5\run-in-session.ps1`). |
| `outs.ps1` | DXGI adapters/outputs, GDI and monitor probe. |
| `etw.ps1`, `hvetw.ps1` | DxgKrnl ETW captures (Heaven). |
| `hvpm.sh`, `hvclose.sh` | PresentMon on Heaven; close Heaven by window. |
| `pmpace.ps1 -Csv F [-Process X] [-SkipSec S] [-Label L]` | Frame pacing from a PresentMon v2 (or v1) CSV, one `PACE` line: frames, median / p99 / p99.9 / max frame time, 1 % and 0.1 % low fps (1000 / mean of the slowest 1 % / 0.1 %), hitches (> 2x median, > 50 ms), median `MsCPUBusy` / `MsGPUTime`, `disp_p99` of `MsBetweenDisplayChange`. Windows PowerShell 5.1. |
| `apiset.sh LABEL [heaven bm-dx12 bm-vk bm-gl vkcube]` | Per-API pacing set: Heaven D3D11 (`hvwin.sh`), Basemark GPU D3D12 / Vulkan / GL (command-line run, JSON report), vkcube; PresentMon v2 + `pmpace.ps1` per run, results in `$VMTEST_DIR/win/apiset-LABEL-*/`. Env `PMSEC WARM WW WH FS APPENV BM_ROOT BM_PIPE VKCUBE VKPM`. |
| `vkrun.ps1 -Exe X [-Dir D] [-ArgLine A] [-EnvList "K=V;K=V"] [-TimeoutSec N]` | Guest side: run an exe in the interactive session (transient scheduled task, elevated when its manifest says `requireAdministrator`, so the env vars survive) with stdout/stderr redirected, then report the Vulkan loader's driver selection (`VK_LOADER_DEBUG=driver`), the Vulkan driver DLLs loaded in the process, the Mesa log and any `vkframes-*` files. |
| `rtest.ps1` | Restart test. |
| `host-deploy4-build.sh` | Builds a staged host deploy (backend, conduit-venus, virglrenderer with patches) with an `install.sh` that checks everything and keeps `.prev` backups (`--rollback`). |
