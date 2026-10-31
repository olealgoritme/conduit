# nvcap: capture tools for NVIDIA's Windows driver stack

Work in progress. Tools to record how NVIDIA's own Windows driver (GeForce 610.74) talks to an
RTX 5090, captured in a Windows 11 VM that has the real GPU passed through. The recordings are
the blueprint for running NVIDIA's drivers in a conduit guest on top of the host's RM.

| Directory | Captures |
|---|---|
| `nvtrace/` | User mode: every D3DKMT call (win32u `NtGdiDdDDI*`) and NVIDIA device IOCTL made by NVIDIA's user-mode drivers, with private data, plus test programs (Vulkan, D3D11, D3D12, CUDA) and a decoder |
| `qemu-trace/` | Device side: a QEMU patch that traps the GPU's BAR0 and logs every register access, interrupt and reset, the FSP boot packets and the GSP RPC queues, plus a decoder |
| `vm/` | The passthrough VM: `vm.sh` (QEMU command line), `switch.sh vm|host` (hands the GPU between host and VM), `ps.sh` (PowerShell over SSH) |

Each directory has its own README.
