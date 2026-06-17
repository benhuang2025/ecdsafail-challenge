#!/bin/bash
# Lever-B 2-stage nonce hunt:
#   stage-1: gcd.cu classical prefilter (gpu-nonce, ~8.6k nonce/s/GPU, 0 false-hard) -> candidates
#   stage-2: verify_dual phase op-loop on candidates only -> DUAL_CLEAN_CANDIDATE
# CPU eval_circuit re-certifies the final winner.
# Usage: NGPU=8 START=<n0> CHUNK=<nonces/gpu/chunk> CHUNKS=<n> bash leverB_hunt.sh
# Prereq: circuit_prep (no candidate, or your candidate env) -> /tmp/phase_circuit
#   (stage-2 reads that dump; stage-1 builds in-process).
set -u
cd "$(dirname "$0")"
export PATH=$HOME/.cargo/bin:$PATH
NGPU=${NGPU:-8}
# GPULIST: explicit GPU ids to use (space-separated), e.g. GPULIST="2 3 4 5 6 7".
# Defaults to 0..NGPU-1. When set, NGPU is derived from it.
GPULIST=${GPULIST:-$(seq 0 $((NGPU - 1)))}
GPUS=($GPULIST)
NGPU=${#GPUS[@]}
START=${START:-30000000000}
CHUNK=${CHUNK:-20000000}        # stage-1 nonces scanned per GPU per chunk
CHUNKS=${CHUNKS:-1}
STRIDE=$((CHUNK * 2))           # disjoint range width per GPU
PHASE_CDUMP=${PHASE_CDUMP:-/tmp/phase_circuit}
export PHASE_CDUMP

# Always rebuild the stage-2 circuit dump at the CURRENT frontier before hunting.
# A stale dump (different frontier) silently misaligns the phase XOF and yields
# false-clean winners; verify_dual now also asserts n_ops==base_ops, but refresh
# here so a hunt never starts on a stale dump. ~3min vs a multi-hour hunt.
# SKIP_PREP=1: reuse an existing $PHASE_CDUMP dump (e.g. rsync'd from another host)
# instead of rebuilding it. Use on hosts without the repo/cargo. The dump MUST be
# the SAME circuit (same frontier + knobs) the prebuilt binaries were built from.
if [ "${SKIP_PREP:-0}" = "1" ]; then
  echo "=== SKIP_PREP=1: using existing dump $PHASE_CDUMP (n_ops=$(od -An -tu8 -N24 $PHASE_CDUMP/meta.bin 2>/dev/null | tr -s ' ' | awk '{print $3}')) ==="
  [ -f "$PHASE_CDUMP/meta.bin" ] || { echo "no dump at $PHASE_CDUMP"; exit 1; }
else
  echo "=== refreshing stage-2 dump -> $PHASE_CDUMP (DIALOG_TAIL_NONCE=${DIALOG_TAIL_NONCE:-none}) ==="
  cargo run --release --quiet --bin circuit_prep 2>&1 | tail -2 || { echo "circuit_prep FAILED"; exit 1; }
fi

for it in $(seq 1 "$CHUNKS"); do
  base=$((START + (it - 1) * NGPU * STRIDE))
  echo "=== chunk $it/$CHUNKS : stage-1 scanning $((NGPU * CHUNK)) nonces from $base ==="
  pkill -f release/gpu-nonce 2>/dev/null; sleep 2
  rm -f /tmp/lb_s1_g*.log
  i=0
  for g in "${GPUS[@]}"; do
    s=$((base + i * STRIDE))
    CUDA_VISIBLE_DEVICES=$g HUNT_GPUS=1 HUNT_START=$s HUNT_COUNT=$CHUNK HUNT_BATCH=262144 \
      setsid nohup numactl --cpunodebind=0 --membind=0 ./target/release/gpu-nonce \
      > /tmp/lb_s1_g$g.log 2>&1 < /dev/null &
    i=$((i + 1))
  done
  while [ "$(pgrep -fc release/gpu-nonce)" -gt 0 ]; do sleep 10; done
  cat /tmp/lb_s1_g*.log 2>/dev/null | grep -oE "nonce=[0-9]+" | grep -oE "[0-9]+" | sort -u > /tmp/lb_cands.txt
  nc=$(wc -l < /tmp/lb_cands.txt)
  echo "  stage-1 done -> $nc classical-clean candidates"
  [ "$nc" -eq 0 ] && continue

  echo "  === stage-2: verify_dual on $nc candidates across $NGPU GPUs ==="
  rm -f /tmp/lb_split_* /tmp/lb_s2_g*.log
  split -n l/"$NGPU" -d /tmp/lb_cands.txt /tmp/lb_split_
  pkill -f release/verify_dual 2>/dev/null; sleep 2
  i=0
  for sf in /tmp/lb_split_*; do
    g=${GPUS[$i]}
    CUDA_VISIBLE_DEVICES=$g CANDS=$sf numactl --cpunodebind=0 --membind=0 \
      ./target/release/verify_dual > /tmp/lb_s2_g$g.log 2>&1 &
    i=$((i + 1))
  done
  wait
  win=$(cat /tmp/lb_s2_g*.log 2>/dev/null | grep DUAL_CLEAN_CANDIDATE | head -1)
  echo "  stage-2 totals: $(grep -h 'VERIFY DONE' /tmp/lb_s2_g*.log 2>/dev/null | sed 's/.*-> //' | paste -sd' + ')"
  if [ -n "$win" ]; then
    echo "*** DUAL-CLEAN WINNER: $win  (CPU eval_circuit must re-certify) ***"
    exit 0
  fi
done
echo "=== leverB_hunt end: no dual-clean this run ==="
