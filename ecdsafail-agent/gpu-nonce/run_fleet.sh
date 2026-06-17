#!/bin/bash
# Fleet launcher for the GPU dual (classical+phase) nonce hunt (hunt_dual).
# Runs ONE independent hunt_dual process per local GPU on disjoint nonce ranges.
# Independent processes scale linearly and sidestep the broken HUNT_GPUS=N
# in-kernel multi-GPU sync (where only 2-3/8 GPUs engaged).
#
# Usage:   run_fleet.sh [NGPUS] [START] [COUNT_PER_GPU]
# Example: bash run_fleet.sh 8 200000000 50000000
# Prereq:  circuit_prep has dumped /tmp/phase_circuit for the CURRENT circuit
#          (re-run circuit_prep after ANY src/point_add change or candidate knob).
# Status:  grep -h DUAL_CLEAN_CANDIDATE /tmp/fleet_g*.log     # winners
#          for g in $(seq 0 7); do tail -1 /tmp/fleet_g$g.log; done   # progress
# Stop:    pkill -f 'release/hunt_dual'
set -u
cd "$(dirname "$0")"
export PATH=$HOME/.cargo/bin:$PATH
NGPUS=${1:-8}
START=${2:-200000000}
COUNT=${3:-50000000}        # nonces per GPU
STRIDE=100000000            # disjoint range width per GPU (must exceed COUNT)
pkill -f 'release/hunt_dual' 2>/dev/null; sleep 2
for ((g=0; g<NGPUS; g++)); do
  s=$((START + g*STRIDE))
  CUDA_VISIBLE_DEVICES=$g HUNT_GPUS=1 HUNT_START=$s HUNT_COUNT=$COUNT \
    HUNT_BATCH=65536 HUNT_BS=32 \
    setsid nohup numactl --cpunodebind=0 --membind=0 ./target/release/hunt_dual \
    > /tmp/fleet_g$g.log 2>&1 < /dev/null &
done
sleep 5
echo "launched $(pgrep -fc 'release/hunt_dual') hunt_dual procs on $NGPUS GPUs (START=$START COUNT/gpu=$COUNT)"
echo "winners: grep -h DUAL_CLEAN_CANDIDATE /tmp/fleet_g*.log"
echo "stop:    pkill -f 'release/hunt_dual'"
