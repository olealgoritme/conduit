# gen

NVIDIA kernel-ABI tables for `conduit-backend` and the guest module, one per
driver release, and the scripts that generate them (crate name `abi`).
Struct sizes, command numbers and RM privilege rules move between releases;
the backend picks the host release's table at start and refuses calls of the
wrong size rather than guessing. Every table is generated, never hand-written,
and checked in.

| script | reads | writes |
|---|---|---|
| `nvabi_gen.py` | gVisor nvproxy, plus its `CONDUIT_EXTRA` list (escapes nvproxy does not serve, e.g. `NV_ESC_RM_GET_EVENT_DATA`) | `src/versions/` (escape sizes) |
| `rmctrl_extract.py` | open-gpu-kernel-modules | `src/rmctrl/`, `guest/linux/rmctrl/` |
| `rmallow_extract.py` | open-gpu-kernel-modules | `src/rmallow/` (RM allowlist) |
| `uvm_extract.py` | open-gpu-kernel-modules | `src/uvm/` |
| `osdesc_extract.py` | open-gpu-kernel-modules | `src/osdesc/` |
| `vidmem_extract.py` | open-gpu-kernel-modules | `src/vidmem/` (`--vram-limit-mib`) |
| `devinfo_extract.py` | open-gpu-kernel-modules (`nvidia-drm` ioctl header) | `src/devinfo/`, `guest/linux/devinfo/` (`--lang c`) |
| `nvgpu_gen.py` | open-gpu-kernel-modules | `guest/linux/gen/` |
| `names_extract.py` | open-gpu-kernel-modules (several releases), `drm.h` | `src/names/table.rs` (names in traces) |

The `*_extract.py` scripts compile a small C probe against the release's own
headers (Python 3 and a C compiler; no Go). An entry a script cannot classify
stops it, so a person looks first.

## Adding a driver release

Usually automatic: the weekly `abi` workflow
(`.github/workflows/abi.yml`) runs `.github/scripts/abi_update.py` for new
releases and opens a PR. By hand:

```sh
.github/scripts/abi_update.py --gvisor ~/src/gvisor --report /tmp/abi.md
# or one table at a time, against an open-gpu-kernel-modules checkout at the tag:
host/backend/gen/vidmem_extract.py --ogkm ~/src/ogkm-615.71.09 --version 615.71.09 \
    > host/backend/gen/src/vidmem/v615_71_09.rs      # then add `pub mod` to mod.rs
cd host/backend && cargo test -p abi                  # tables vs. each other and neighbours
make guest                                            # the guest module with the new headers
```

`fixtures/*.tsv` are ioctl parameter sizes captured on real hardware;
`src/fixtures.rs` checks the tables against them.
