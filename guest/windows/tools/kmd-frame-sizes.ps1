<#
.SYNOPSIS
  Report the kernel stack frame size of chosen KMD functions, straight from the
  built .sys + linker .map. No PDB and no debugger needed.

.DESCRIPTION
  The x64 kernel stack is 24 KB and dxgkrnl's own frames sit above ours on the
  boot path. 22.22.181.0 shipped a DxgkDdiStartDevice / VirtioGpu::init pair
  totalling 18800 bytes and would not boot (0xc0000001 / Startup Repair, with NO
  crash dump and NO bugcheck 1001 event -- an early double fault cannot write
  one). 22.22.180.0's 17936 bytes is the known-good ceiling for that nested pair.

  Constructors called by init are simultaneously live too. In particular,
  VirtQueue::new has a large by-value return slot, so omitting it understated
  22.22.227.0's pre-fix chain by 4168 bytes. The default chains below include
  both that constructor and the fixed present-stream/scanout-refresh allocators
  explicitly.

  A function with a frame larger than one page is compiled with a __chkstk
  probe, so its prologue reads:

      movl  $0xNNNN, %eax
      callq <__chkstk>          # llvm-objdump has no name for the thunk and
      subq  %rax, %rsp          # prints it as __GSHandlerCheck_EH4+0xb8

  and 0xNNNN is the frame size. This script finds each symbol's address in the
  .map and reads that prologue out of `llvm-objdump -d`, anchoring on the
  `subq %rax, %rsp` rather than on the unnamed thunk. A sub-page frame has no
  __chkstk probe, only a direct `subq $0xNN, %rsp`, which is read instead.

  The budget applies to a CHAIN of simultaneously live frames, not to the sum of
  everything measured, so -Chains declares which symbols call which. Exits 1 if
  the deepest declared chain is over the ceiling, and ALSO exits 1 if a declared chain
  could not be measured (one of its symbols missing from the .map, no address, not in
  the disassembly): a fully inlined chain used to be skipped as INCOMPLETE and the
  script still printed a headroom figure and exited 0, passing without measuring.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File Z:\tools\kmd-frame-sizes.ps1
#>
[CmdletBinding()]
param(
    [string]   $Package = 'C:\Users\Rupansh\helios-vgpu\kmd_render\target\debug\helios_kmd_render_package',
    [string]   $ObjDump = 'C:\Program Files\LLVM\bin\llvm-objdump.exe',
    # Substrings matched against the mangled names in the .map. Rust v0 mangling
    # prefixes each identifier with its byte length, so '14bring_up_venus' means
    # the 14-character name `bring_up_venus` — get the length wrong and the
    # symbol simply is not found. The defaults are the boot path: the two
    # functions whose sum is the budget, then the venus bring-up chain they
    # call, which R608 split into per-stage frames.
    [string[]] $Symbols = @(
        '9lifecycle20dxgkddi_start_device',
        '9VirtioGpu4init',
        # The event queue is built from init (before DRIVER_OK), beside the
        # control queue's constructor.
        '14new_event_ring',
        # MSI-X bring-up (virtio/msi.rs). `probe_granted` -> `listed_messages`
        # runs BEFORE init (sequential, never nested in it) and carries the
        # ~150-byte DXGK_DEVICE_INFO; `program_vectors` is a leaf under init.
        '13probe_granted',
        '15listed_messages',
        '15program_vectors',
        # Message-interrupt health (virtio/msi.rs): the transport-up reset and the end-of-start
        # verdict, both called from StartDevice (never nested in init) and noinline.
        '15on_transport_up',
        '12finish_start',
        # VirtQueue::new's concrete PciTransport argument. Deliberately WITHOUT
        # the const queue size before it (`Kj40_` for 64): that prefix made this
        # symbol vanish from the gate when the ring size changed, silently
        # dropping its by-value return slot from the budget.
        'E3newNtNtNtB5_9transport3pci12PciTransport',
        '24allocate_present_streams',
        # The RM gate table (virtio/gpu/rm_gates.rs): a RmGateSlot is ~1.1 KiB (128
        # points) built by value in this `inline(never)` frame, called from init like
        # the present-stream allocator.
        '17allocate_rm_gates',
        '30allocate_scanout_refresh_state',
        # The RM window account (virtio/gpu/nvrm_tables.rs): built in its own frame, returns
        # one Box pointer (the window account and both table bounds in one allocation).
        '18new_window_account',
        '14bring_up_venus',
        '26allocate_host_visible_blob',
        '9VenusRing8bring_up',
        '9VenusRing13into_instance',
        '13VenusInstance11into_device',
        '13VenusInstance29create_device_with_ext_ladder',
        # The device-lost instrument's DDI wrappers (ddi/traced.rs) and the DDIs they front. Each is
        # `#[inline(never)]` so the symbol exists; DxgkDdiStartDevice is deliberately NOT wrapped.
        '6traced11stop_device',
        '9lifecycle19dxgkddi_stop_device',
        '6traced14destroy_device',
        '6device22dxgkddi_destroy_device',
        '6traced19build_paging_buffer',
        '19build_paging_buffer27dxgkddi_build_paging_buffer',
        '19build_paging_buffer25build_paging_buffer_inner'
    ),
    # Call chains to sum. The 24 KB budget applies to a CHAIN of simultaneously
    # live frames, never to the sum of every symbol measured, so the chains are
    # declared rather than inferred. Each entry is a comma-separated list of
    # $Symbols entries, outermost first.
    [string[]] $Chains = @(
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init',
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init,E3newNtNtNtB5_9transport3pci12PciTransport',
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init,14new_event_ring,E3newNtNtNtB5_9transport3pci12PciTransport',
        '9lifecycle20dxgkddi_start_device,13probe_granted,15listed_messages',
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init,15program_vectors',
        '9lifecycle20dxgkddi_start_device,15on_transport_up',
        '9lifecycle20dxgkddi_start_device,12finish_start',
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init,24allocate_present_streams',
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init,17allocate_rm_gates',
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init,30allocate_scanout_refresh_state',
        '9lifecycle20dxgkddi_start_device,9VirtioGpu4init,18new_window_account',
        '9lifecycle20dxgkddi_start_device,14bring_up_venus,26allocate_host_visible_blob,13VenusInstance11into_device,13VenusInstance29create_device_with_ext_ladder',
        '6traced11stop_device,9lifecycle19dxgkddi_stop_device',
        '6traced14destroy_device,6device22dxgkddi_destroy_device',
        '6traced19build_paging_buffer,19build_paging_buffer27dxgkddi_build_paging_buffer,19build_paging_buffer25build_paging_buffer_inner'
    ),
    [int]      $Window  = 24
)

$ErrorActionPreference = 'Stop'

$sys = Join-Path $Package 'helios_kmd_render.sys'
$map = Join-Path $Package 'helios_kmd_render.map'
foreach ($f in @($sys, $map, $ObjDump)) {
    if (-not (Test-Path $f)) { throw "missing: $f" }
}

Write-Host ("image     : {0}" -f $sys)
Write-Host ("version   : {0}" -f (Get-Item $sys).VersionInfo.FileVersion)

$dis = & $ObjDump -d $sys
# "18000e340: 48 83 ec ..." -> index by the address text llvm-objdump prints.
$index = @{}
for ($i = 0; $i -lt $dis.Count; $i++) {
    $line = $dis[$i]
    $c = $line.IndexOf(':')
    if ($c -gt 0 -and $c -le 16) {
        $addr = $line.Substring(0, $c)
        if ($addr -match '^[0-9a-f]+$' -and -not $index.ContainsKey($addr)) { $index[$addr] = $i }
    }
}

$frames = @{}
$problems = @()   # filled by the chain loop: a symbol outside every chain (informational) may be absent
foreach ($sym in $Symbols) {
    $hit = Select-String -Path $map -Pattern ([regex]::Escape($sym)) -SimpleMatch |
           Select-Object -First 1
    if (-not $hit) { Write-Host ("{0,-40} SYMBOL NOT IN .map" -f $sym); continue }

    # map line: "0001:0000d340  <mangled>  000000018000e340 f  <obj>"
    $va = ($hit.Line -split '\s+' | Where-Object { $_ -match '^[0-9a-fA-F]{16}$' } | Select-Object -First 1)
    if (-not $va) { Write-Host ("{0,-40} NO VA IN MAP LINE" -f $sym); continue }

    $key = ($va.TrimStart('0')).ToLower()
    if (-not $index.ContainsKey($key)) { Write-Host ("{0,-40} VA {1} NOT IN DISASSEMBLY" -f $sym, $va); continue }

    $start = $index[$key]
    $frame = $null
    $small = $null
    $pending = $null
    for ($i = $start; $i -lt [Math]::Min($start + $Window, $dis.Count); $i++) {
        $t = $dis[$i]
        if ($t -match 'movl\s+\$0x([0-9a-f]+),\s*%eax') { $pending = [Convert]::ToInt32($matches[1], 16); continue }
        if ($t -match 'subq\s+%rax,\s*%rsp') { $frame = $pending; break }
        # Sub-page frame: no __chkstk probe, just a direct adjustment. Take the
        # FIRST one in the prologue; later ones belong to inlined blocks.
        if ($null -eq $small -and $t -match 'subq\s+\$0x([0-9a-f]+),\s*%rsp') {
            $small = [Convert]::ToInt32($matches[1], 16)
        }
    }

    if ($null -ne $frame) {
        $frames[$sym] = $frame
        Write-Host ("{0,-46} {1,6} bytes  (0x{1:x}, __chkstk)" -f $sym, $frame)
    } elseif ($null -ne $small) {
        $frames[$sym] = $small
        Write-Host ("{0,-46} {1,6} bytes  (0x{1:x})" -f $sym, $small)
    } else {
        $frames[$sym] = 0
        Write-Host ("{0,-46} {1,6} bytes  (leaf, no rsp adjustment)" -f $sym, 0)
    }
}

Write-Host ""
Write-Host "--- chains (the 24 KB budget applies to these, not to the sum above) ---"
$worst = 0
foreach ($chain in $Chains) {
    $parts = $chain -split ','
    $sum = 0
    $missing = $false
    $leafInlined = $false
    for ($i = 0; $i -lt $parts.Count; $i++) {
        $part = $parts[$i]
        if ($frames.ContainsKey($part)) { $sum += $frames[$part] }
        elseif ($i -eq $parts.Count - 1 -and $i -gt 0) {
            # The LAST symbol of a chain missing from the .map was inlined into its caller
            # (typically a generic from a dependency crate, e.g. PciTransport::new, which we
            # cannot mark #[inline(never)]): its frame is part of the caller's measured frame,
            # so the chain is still bounded. A missing symbol anywhere else is unmeasured.
            $leafInlined = $true
        }
        else { $missing = $true }
    }
    $names = ($parts | ForEach-Object { ($_ -replace '^[0-9]+', '') }) -join ' -> '
    $note = if ($missing) { '  (INCOMPLETE: a symbol was not measured)' }
            elseif ($leafInlined) { '  (last symbol inlined into its caller: included in the caller frame)' }
            else { '' }
    Write-Host ("{0,6} bytes  {1}{2}" -f $sum, $names, $note)
    if ($missing) { $problems += "chain not measured: $names" }
    if (-not $missing -and $sum -gt $worst) { $worst = $sum }
}
Write-Host ""
if ($problems.Count -gt 0) {
    Write-Host "** UNMEASURED: the gate did not measure everything it declares (a symbol was inlined away or renamed) **"
    $problems | ForEach-Object { Write-Host ("   " + $_) }
    exit 1
}
if ($worst -gt 17936) {
    Write-Host ("DEEPEST CHAIN {0} bytes  ** OVER the 17936-byte 22.22.180.0 ceiling **" -f $worst)
    exit 1
}
Write-Host ("DEEPEST CHAIN {0} bytes  (ceiling 17936, headroom {1})" -f $worst, (17936 - $worst))
