param([int]$Sec=600)
Add-Type @"
using System; using System.Runtime.InteropServices;
public class W { [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h,int x,int y,int w,int ht,bool r);
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int c); }
"@
$log="C:\Users\Public\t\stress.log"; "start $(Get-Date)" | Out-File $log
$h=(Get-Process Heaven).MainWindowHandle; $t0=Get-Date; $i=0
while(((Get-Date)-$t0).TotalSeconds -lt $Sec){
  $i++; $x=100+($i*37)%1500; $y=100+($i*23)%400
  $w=1600; $ht=900; if(($i % 20) -lt 10){ $w=1200; $ht=700 }
  [W]::MoveWindow($h,$x,$y,$w,$ht,$true) | Out-Null
  if($i % 60 -eq 0){ [W]::ShowWindow($h,6)|Out-Null; Start-Sleep -m 800; [W]::ShowWindow($h,9)|Out-Null }
  if($i % 120 -eq 0){ $n=1..3 | % { Start-Process notepad -PassThru }; $e=Start-Process explorer.exe C:\ -PassThru; Start-Sleep 2; $n | % { Stop-Process -Id $_.Id -Force -EA 0 }; "cycle $i $(Get-Date -f HH:mm:ss)" | Out-File $log -Append }
  Start-Sleep -m 250
}
"done $i $(Get-Date)" | Out-File $log -Append
