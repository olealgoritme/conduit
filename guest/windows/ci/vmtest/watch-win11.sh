#!/usr/bin/env bash
# win11 watchdog for a Monitor: state, backend, disk, MAP_BLOB refusals, reboot, dumps, app crash/hang, TDR, DWM, unresponsive guest.
H="Ole Algoritme@127.0.0.1"; BL=~/.local/share/conduit/vms/win11/logs/backend.log
last_dump=""; last_boot=""; last_state=""; last_be=""; fails=0; dlow=0; ra=0; rs=0
mapn=$(grep -c "Frontend internal error" $BL 2>/dev/null)
while true; do
  st=$(virsh -c qemu:///session domstate win11 2>/dev/null | head -1)
  [ "$st" != "$last_state" ] && { echo "[$(date +%T)] VM state: $st $(virsh -c qemu:///session domstate win11 --reason 2>/dev/null | head -1)"; last_state=$st; }
  be=$(systemctl --user is-active conduit-backend@win11 2>/dev/null)
  [ "$be" != "$last_be" ] && { echo "[$(date +%T)] backend: $be"; last_be=$be; }
  free=$(df -BG --output=avail / | tail -1 | tr -dc 0-9)
  if [ "$free" -lt 12 ]; then [ $dlow = 0 ] && echo "[$(date +%T)] HOST DISK LOW: ${free}G free"; dlow=1; else dlow=0; fi
  n=$(grep -c "Frontend internal error" $BL 2>/dev/null); [ "${n:-0}" -gt "${mapn:-0}" ] && { echo "[$(date +%T)] MAP_BLOB refusals: $n"; mapn=$n; }
  if [ "$st" = running ]; then
    r=$(timeout 20 ssh -o ConnectTimeout=6 -o BatchMode=yes -p 2222 "$H" "(gcim Win32_OperatingSystem).LastBootUpTime.ToString('s'); 'DUMP:' + (Get-ChildItem C:\Windows\Minidump -EA 0 | Sort LastWriteTime -Desc | Select -First 1).Name; Get-WinEvent -FilterHashtable @{LogName='Application';Id=1000,1002} -MaxEvents 40 -EA 0 | ? { \$_.RecordId -gt $ra } | % { 'EA ' + \$_.RecordId + ' ' + \$_.TimeCreated.ToString('T') + ' id' + \$_.Id + ' ' + (\$_.Properties[0].Value) + ' ' + (\$_.Properties[3].Value) + ' ' + (\$_.Properties[6].Value) + ' +' + (\$_.Properties[7].Value) }; Get-WinEvent -FilterHashtable @{LogName='System';Id=4101} -MaxEvents 10 -EA 0 | ? { \$_.RecordId -gt $rs } | % { 'ES ' + \$_.RecordId + ' ' + \$_.TimeCreated.ToString('T') + ' TDR' }; iex (Get-Content -Raw C:\Users\Public\dwmcheck.ps1)" 2>/dev/null | tr -d '\r')
    if [ -z "$r" ]; then fails=$((fails+1)); [ $fails -eq 4 ] && echo "[$(date +%T)] guest unresponsive for 60 s (possible hang)"
    else [ $fails -ge 4 ] && echo "[$(date +%T)] guest responsive again"; fails=0
      b=$(echo "$r" | sed -n 1p); d=$(echo "$r" | grep "^DUMP:" | head -1 | cut -c6-)
      [ -n "$last_boot" ] && [ "$b" != "$last_boot" ] && echo "[$(date +%T)] win11 REBOOTED (boot $b)"
      [ -n "$d" ] && [ -n "$last_dump" ] && [ "$d" != "$last_dump" ] && echo "[$(date +%T)] NEW DUMP $d"
      echo "$r" | grep -q NODWM && echo "[$(date +%T)] DWM NOT RUNNING"; st2=$(echo "$r" | grep DWMSTALL); if [ -n "$st2" ]; then [ "$stall" != 1 ] && echo "[$(date +%T)] $st2 (DWM stopped presenting?)"; stall=1; else stall=0; fi
      ev=$(echo "$r" | grep '^EA ' | sort -k2 -n)
      [ "$ra" -gt 0 ] && echo "$ev" | grep -v -e '^$' -e 'SearchHost.exe' | cut -d' ' -f3- | sed "s/^/[$(date +%T)] APP /"
      m=$(echo "$ev" | awk 'NF{print $2}' | sort -n | tail -1); [ -n "$m" ] && ra=$m; [ "$ra" -eq 0 ] && ra=1
      es=$(echo "$r" | grep '^ES ' | sort -k2 -n)
      [ "$rs" -gt 0 ] && echo "$es" | grep -v '^$' | cut -d' ' -f3- | sed "s/^/[$(date +%T)] /"
      m=$(echo "$es" | awk 'NF{print $2}' | sort -n | tail -1); [ -n "$m" ] && rs=$m; [ "$rs" -eq 0 ] && rs=1
      last_boot=$b; [ -n "$d" ] && last_dump=$d; fi
  fi
  sleep 15
done
