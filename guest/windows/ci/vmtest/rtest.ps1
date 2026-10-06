$K='HKLM:\SYSTEM\CurrentControlSet\Services\helios_kmd_render'
$r=Get-ItemProperty $K; "pre: GenSalt=$($r.GenSalt) GenEpoch=$($r.GenEpoch) StartN=$($r.StartN) dwm=$((Get-Process dwm).Id) heaven=$((Get-Process Heaven -EA 0).Id) explorer=$((Get-Process explorer -EA 0 | Select -First 1).Id)"
$id=(Get-PnpDevice -Class Display -Status OK | ? Service -eq helios_kmd_render).InstanceId
pnputil /restart-device "$id" | Select-String 'restarted|fail' | % Line
Start-Sleep 25
$r=Get-ItemProperty $K; "post: GenSalt=$($r.GenSalt) GenEpoch=$($r.GenEpoch) StartN=$($r.StartN) StopUnbSt=$($r.StopUnbSt) NvRef=$($r.NvRef) NvOpen=$($r.NvOpen) dwm=$((Get-Process dwm).Id) heaven=$((Get-Process Heaven -EA 0).Id) explorer=$((Get-Process explorer -EA 0 | Select -First 1).Id)"
$a=$r.VpPres; Start-Sleep 5; $b=(Get-ItemProperty $K).VpPres; "VpPres $a -> $b"
Get-Content C:\ProgramData\Helios\helios_icd_diag.log -Tail 400 -EA 0 | Select-String 'device-lost|generation|epoch' | Select -Last 8 | % { $_.Line.Substring(0,[Math]::Min(180,$_.Line.Length)) }
