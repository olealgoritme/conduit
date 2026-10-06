H="Ole Algoritme@127.0.0.1"; b0=""; d0=""; fails=0
while true; do
  st=$(virsh -c qemu:///session domstate win11 2>/dev/null | head -1)
  [ "$st" = running ] || { echo "$(date +%T) ALERT: VM state $st"; exit 1; }
  systemctl --user is-active -q conduit-backend@win11 || { echo "$(date +%T) ALERT: backend not active"; exit 1; }
  free=$(df -BG --output=avail / | tail -1 | tr -dc 0-9); [ "$free" -lt 8 ] && { echo "$(date +%T) ALERT: host disk ${free}G free"; exit 1; }
  r=$(timeout 12 ssh -o ConnectTimeout=6 -o BatchMode=yes -p 2222 "$H" '(gcim Win32_OperatingSystem).LastBootUpTime.ToString("s"); "DUMP:" + (Get-ChildItem C:\Windows\Minidump -EA 0 | Sort LastWriteTime -Desc | Select -First 1).Name' 2>/dev/null | tr -d '\r')
  if [ -z "$r" ]; then fails=$((fails+1)); [ $fails -ge 6 ] && { echo "$(date +%T) ALERT: no SSH for 2 min"; exit 1; }
  else fails=0; b=$(echo "$r" | sed -n 1p); d=$(echo "$r" | grep '^DUMP:' | cut -c6-)
    [ -n "$b0" ] && [ "$b" != "$b0" ] && { echo "$(date +%T) ALERT: rebooted ($b), dump $d"; exit 1; }
    [ -n "$d" ] && [ -n "$d0" ] && [ "$d" != "$d0" ] && { echo "$(date +%T) ALERT: new dump $d"; exit 1; }
    b0=$b; [ -n "$d" ] && d0=$d; fi
  sleep 20
done