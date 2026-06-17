#!/bin/bash
# node_hunt.sh — run ON a hunt node (zan3/zan4/zan5). Launches GPU scan + CPU verify.
# Assumes the repo is already synced AND built on this node (deploy.sh does that).
#
# Usage:  node_hunt.sh <range_start> <verify_parallelism> <gpu_ids|auto>
#   e.g.  node_hunt.sh 1          110 0,1
#         node_hunt.sh 100000001  120 auto
#
# Hard-won gotchas baked in:
#  - explicit PATH (a non-login ssh shell has a minimal PATH; numactl/tail/nvcc go missing)
#  - written as a FILE and run via setsid (never inline nested-ssh heredocs — quoting hell)
#  - GPU auto-detect for shared nodes (picks cards with <2GB used)
#  - scan stdout is followed with `tail -F` so the verify daemon streams candidates live
#  - first candidates take ~200s (host-bound first batch ~4800 nonce/s) — that's normal
set -uo pipefail
export PATH=/usr/local/cuda/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
export LD_LIBRARY_PATH=/usr/local/cuda-12.8/targets/x86_64-linux/lib:${LD_LIBRARY_PATH:-}
REPO=/home/ubuntu/ben/temp/ecdsafail-challenge
cd "$REPO" || { echo "no repo at $REPO"; exit 1; }

START="${1:-1}"; VP="${2:-100}"; GPUS="${3:-auto}"
FS="$REPO/ecdsafail-agent/fast-screen/target/release/fast-screen"
GN="$REPO/ecdsafail-agent/gpu-nonce/target/release/gpu-nonce"

# --- pick GPUs ---
if [ "$GPUS" = auto ]; then
  GPUS=$(nvidia-smi --query-gpu=index,memory.used --format=csv,noheader,nounits \
         | awk -F', ' '$2<2000{print $1}' | head -2 | paste -sd,)
  [ -z "$GPUS" ] && GPUS=0
fi
NG=$(echo "$GPUS" | tr ',' '\n' | grep -c .)
echo "node_hunt: start=$START verify_P=$VP gpus=$GPUS (n=$NG)"

# --- clean any prior hunt on this node ---
pkill -9 -x gpu-nonce 2>/dev/null
pkill -9 -f "ben/temp.*fast-scree[n]" 2>/dev/null
pkill -9 -f hunt_vone 2>/dev/null
pkill -9 -f "tail -n +1 -F /tmp/hunt_scan" 2>/dev/null
sleep 2
rm -f /tmp/hunt_WINNER /tmp/hunt_scan.log

# --- per-candidate verifier (a winner = all three counters 0 over the full island) ---
cat > /tmp/hunt_vone.sh <<VEOF
#!/bin/bash
r=\$(DIALOG_TAIL_NONCE="\$1" NS_EARLY_EXIT=1 NS_SHOTS=9024 "$FS" 2>/dev/null)
echo "\$r" | grep -q "classical=0 phase=0 ancilla=0" && echo "\$1" > /tmp/hunt_WINNER
VEOF
chmod +x /tmp/hunt_vone.sh

# --- GPU scan (prefilter) ---
CUDA_VISIBLE_DEVICES=$GPUS HUNT_START=$START HUNT_COUNT=100000000 \
  HUNT_BATCH=1048576 HUNT_BS=64 HUNT_GPUS=$NG \
  numactl --interleave=all "$GN" > /tmp/hunt_scan.log 2>&1 &
echo "scan_pid=$!"

# --- verify daemon: stream candidates -> $VP parallel verifiers ---
setsid bash -c "tail -n +1 -F /tmp/hunt_scan.log \
  | stdbuf -oL grep --line-buffered CLEAN_CANDIDATE \
  | sed -u 's/.*nonce=//' \
  | xargs -P $VP -I@ /tmp/hunt_vone.sh @" \
  > /tmp/hunt_verifyd.log 2>&1 </dev/null &
echo "verifyd_pid=$!"
echo "node_hunt launched OK"
