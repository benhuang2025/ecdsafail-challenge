#!/bin/bash
# stop.sh — cleanly stop the hunt on all fleet nodes. Run ON zan3.
ZAN4=ubuntu@10.23.175.200
ZAN5=ubuntu@10.23.147.174
KILL='pkill -9 -x gpu-nonce 2>/dev/null; pkill -9 -f "ben/temp.*fast-scree[n]" 2>/dev/null; pkill -9 -f hunt_vone 2>/dev/null; pkill -9 -f "tail -n +1 -F /tmp/hunt_scan" 2>/dev/null; pkill -9 -f "xargs -P" 2>/dev/null'
echo "stopping zan3..."; eval "$KILL"; sleep 1
echo "  zan3: scan=$(pgrep -xc gpu-nonce) verify=$(pgrep -fc 'ben/temp.*fast-scree[n]')"
for H in "$ZAN4" "$ZAN5"; do
  echo "stopping $H..."
  ssh -o ConnectTimeout=10 "$H" "$KILL; sleep 1; echo \"  scan=\$(pgrep -xc gpu-nonce) verify=\$(pgrep -fc 'ben/temp.*fast-scree[n]')\"" 2>/dev/null
done
echo "stopped."
