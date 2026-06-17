#!/bin/bash
# status.sh — health + winner check across the fleet. Run ON zan3.
export PATH=/usr/local/cuda/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin
ZAN4=ubuntu@10.23.175.200
ZAN5=ubuntu@10.23.147.174

check_local() {
  echo "=== zan3 (local) ==="
  echo "  scan=$(pgrep -xc gpu-nonce) verify=$(pgrep -fc 'ben/temp.*fast-scree[n]')"
  echo "  candidates=$(grep -ch CLEAN_CANDIDATE /tmp/hunt_scan.log 2>/dev/null) WINNER=$(cat /tmp/hunt_WINNER 2>/dev/null || echo none)"
}
check_remote() {
  local H="$1" N="$2"
  echo "=== $N ($H) ==="
  ssh -o ConnectTimeout=10 "$H" "echo \"  scan=\$(pgrep -xc gpu-nonce) verify=\$(pgrep -fc 'ben/temp.*fast-scree[n]')\"; echo \"  candidates=\$(grep -ch CLEAN_CANDIDATE /tmp/hunt_scan.log 2>/dev/null) WINNER=\$(cat /tmp/hunt_WINNER 2>/dev/null || echo none)\"" 2>/dev/null
}
check_local
check_remote "$ZAN4" zan4
check_remote "$ZAN5" zan5
