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
| `blrow.sh LABEL ENV Knob=Val...` | One A/B row for the windowed (composed) Present: set KMD knobs, restart the device, fresh DWM + shell, Heaven, PresentMon 10 s, screenshot, KMD counters (`PrDdiBlt*`, `BltMirror*`, `BltAsync*`, `Gb*`). |
| `gbrows.sh` | The three guest-blob rows (GuestBlob 0, 1, 1 + BltAsync + ForeignCopy). Loops that split arguments must stay in bash scripts: the agent shell is zsh and does not word-split. |
| `stress.ps1`, `stressrun.sh` | 10-minute window stress in the user session (move, resize, minimise/restore, open/close windows) with counters before and after. |
| `pass.sh` | A clean performance pass. |
| `abrun.sh`, `abrow.sh` | Older A/B rows (flip knobs). |
| `shot.ps1` | In-session screenshot (run through `lvl5\run-in-session.ps1`). |
| `outs.ps1` | DXGI adapters/outputs, GDI and monitor probe. |
| `etw.ps1`, `hvetw.ps1` | DxgKrnl ETW captures (Heaven). |
| `hvpm.sh`, `hvclose.sh` | PresentMon on Heaven; close Heaven by window. |
| `rtest.ps1` | Restart test. |
| `host-deploy4-build.sh` | Builds a staged host deploy (backend, conduit-venus, virglrenderer with patches) with an `install.sh` that checks everything and keeps `.prev` backups (`--rollback`). |
