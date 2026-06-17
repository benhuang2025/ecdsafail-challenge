#!/bin/bash
# deploy.sh — one command to deploy a nonce hunt across the LAN fleet (zan3/zan4/zan5).
# RUN THIS ON zan3 (the control node). It reaches zan4/zan5 over the LAN.
#
# Usage:
#   deploy.sh '<sed-expr applying the candidate knob to src/point_add/mod.rs>'
#
# Example (COMPARE_BITS 46->44):
#   deploy.sh 's/set_var("DIALOG_GCD_COMPARE_BITS", "46")/set_var("DIALOG_GCD_COMPARE_BITS", "44")/'
#
# What it does:
#   1. git checkout mod.rs (clean base = current frontier), apply your knob, UNFREEZE the nonce
#   2. build on zan3 (build_circuit + gpu-nonce + fast-screen)
#   3. rsync source to zan4/zan5 and build there (parallel)
#   4. launch node_hunt.sh on all three with DISJOINT nonce ranges
#   5. print how to watch / stop
#
# IMPORTANT — run Step-0/Step-1 (screen + lambda) FIRST. Only deploy a candidate you've
# confirmed is score-negative, peak-neutral, and sub-cliff (lambda ~ baseline). See README.md.
set -uo pipefail
REPO=/home/ubuntu/ben/temp/ecdsafail-challenge
HUNT="$REPO/ecdsafail-agent/hunt"
export PATH=/usr/local/cuda/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin
export LD_LIBRARY_PATH=/usr/local/cuda-12.8/targets/x86_64-linux/lib
# Force zan3's own id_ed25519 for zan4/zan5: when this script runs detached (setsid),
# a stale FORWARDED ssh-agent socket would otherwise be offered first and hit MaxAuthTries
# -> "Permission denied" before falling back to the local key. Unset it.
unset SSH_AUTH_SOCK
SSHO="-o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o ConnectTimeout=12"
CONFIG_SED="${1:?usage: deploy.sh '<sed-expr for the candidate knob>'}"

# Fleet (edit here if hosts change). zan3 = local.
ZAN4=ubuntu@10.23.175.200
ZAN5=ubuntu@10.23.147.174

cd "$REPO"
echo "===[1/5] apply config + unfreeze nonce on zan3 ==="
git fetch origin -q 2>/dev/null || true
git checkout -- src/point_add/mod.rs
sed -i "$CONFIG_SED" src/point_add/mod.rs
# unfreeze the baked tail nonce so gpu-nonce/fast-screen can vary it per candidate
sed -i 's/std::env::set_var("DIALOG_TAIL_NONCE", "3452376")/set_default_env("DIALOG_TAIL_NONCE", "3452376")/' src/point_add/mod.rs
echo "config applied; nonce unfrozen=$(grep -c 'set_default_env("DIALOG_TAIL_NONCE"' src/point_add/mod.rs)"

echo "===[2/5] build on zan3 ==="
cargo build --release --quiet --bin build_circuit
( cd ecdsafail-agent/gpu-nonce && cargo build --release -q )
( cd ecdsafail-agent/fast-screen && cargo build --release -q )

echo "===[3/5] sync + build zan4, zan5 (parallel) ==="
for H in "$ZAN4" "$ZAN5"; do
  (
    rsync -a --exclude target --exclude .git --exclude '*.log' --exclude '*.bin' \
      -e "ssh $SSHO" "$REPO/" "$H:$REPO/" >/dev/null 2>&1
    scp $SSHO -q "$HUNT/node_hunt.sh" "$H:/tmp/node_hunt.sh"
    ssh $SSHO "$H" "export PATH=/usr/local/cuda/bin:\$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin; \
              export LD_LIBRARY_PATH=/usr/local/cuda-12.8/targets/x86_64-linux/lib; \
              cd $REPO && cargo build --release -q --bin build_circuit \
              && (cd ecdsafail-agent/gpu-nonce && cargo build --release -q) \
              && (cd ecdsafail-agent/fast-screen && cargo build --release -q) && echo '$H built'"
  ) &
done
wait

echo "===[4/5] launch hunts with disjoint nonce ranges ==="
# zan3 (local): GPUs 0,1 ; ~110 verify cores
bash "$HUNT/node_hunt.sh" 1 110 0,1
# zan4 (shared GPU -> auto-pick free): range 100M
ssh $SSHO "$ZAN4" "setsid bash /tmp/node_hunt.sh 100000001 120 auto </dev/null >/tmp/node_hunt.out 2>&1 & echo zan4-launched"
# zan5 (8x GPU): range 200M
ssh $SSHO "$ZAN5" "setsid bash /tmp/node_hunt.sh 200000001 120 auto </dev/null >/tmp/node_hunt.out 2>&1 & echo zan5-launched"

echo "===[5/5] deployed. ==="
echo "watch:  bash $HUNT/status.sh"
echo "stop:   bash $HUNT/stop.sh"
echo "winner appears at /tmp/hunt_WINNER on whichever node finds it."
