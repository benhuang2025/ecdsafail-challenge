# Nonce-hunt deploy toolkit (zan3 / zan4 / zan5)

One-command deploy of a nonce hunt across the LAN fleet, with every hard-won gotcha baked in.

## Fleet

| node | host (from zan3) | GPUs | ~cores | notes |
|------|------------------|------|--------|-------|
| zan3 | local            | 8 (use 0,1) | ~110 | **control node** — run all scripts here |
| zan4 | ubuntu@10.23.175.200 | shared | ~120 | shared GPUs → `node_hunt.sh` auto-picks a free one |
| zan5 | ubuntu@10.23.147.174 | 8× RTX5090 | 124 | full datacenter box |

zan3 reaches zan4/zan5 over the LAN (key auth). The Mac drives everything via `ssh zan3 '...'`.
**zan3 LAN IP = `10.23.115.92`** (use this when a node needs to pull from zan3).

## Syncing code to fleet nodes (the reliable way)

`deploy.sh` PUSHES (rsync zan3 → node). That works when run **interactively** (your Mac's
ssh-agent is forwarded through zan3). But when `deploy.sh` runs **detached** (`setsid`), the
forwarded agent socket is dead, zan3 offers stale keys, hits `MaxAuthTries`, and you get
`Permission denied` before it falls back to zan3's own key. Two robust fixes:

1. **Node pulls from zan3 (simplest, always works):** on each node, in `~/ben/temp/`:
   ```bash
   scp -r ubuntu@10.23.115.92:/home/ubuntu/ben/temp/ecdsafail-challenge/ .
   ```
   The node authenticates to zan3 with its own key — no forwarded-agent dependency.
2. **Push detached:** `unset SSH_AUTH_SOCK` AND give an explicit key
   (`ssh -i ~/.ssh/id_ed25519 ...`) — `IdentitiesOnly=yes` *without* `-i` offers no key and fails.
   This also requires zan3's `id_ed25519.pub` to be in each node's `authorized_keys`.

After syncing, build + launch on the node: `cd <REPO> && cargo build --release ... && bash /tmp/node_hunt.sh <start> <P> auto` (node_hunt.sh is in the synced `ecdsafail-agent/hunt/`).

## Usage

All commands run **on zan3**. From the Mac: `ssh zan3 'bash <REPO>/ecdsafail-agent/hunt/<script> ...'`.

```bash
# 1. screen + Step-0 FIRST (see AGENT_RUNBOOK.md) — only deploy a confirmed sub-cliff candidate
# 2. deploy (example: COMPARE_BITS 46->44)
bash ecdsafail-agent/hunt/deploy.sh \
  's/set_var("DIALOG_GCD_COMPARE_BITS", "46")/set_var("DIALOG_GCD_COMPARE_BITS", "44")/'
# 3. watch
bash ecdsafail-agent/hunt/status.sh
# 4. on winner (file /tmp/hunt_WINNER on any node) -> bake (below)
# 5. stop
bash ecdsafail-agent/hunt/stop.sh
```

`deploy.sh` = apply knob + unfreeze nonce → build zan3 → rsync+build zan4/zan5 → launch all three on disjoint ranges (1 / 100M / 200M).

## Gotchas baked in (why this exists)

1. **PATH** — a non-login `ssh host 'cmd'` has a minimal PATH; `numactl`, `tail`, `nvcc`, `cargo`
   go missing → silent failures. Every script sets an explicit full PATH.
2. **No nested-ssh heredocs** — quoting `ssh a 'ssh b "..."'` mangles `$VAR`/`\$!` and breaks launches.
   Scripts are written to FILES and `scp`'d, then run with `setsid`.
3. **GPU contention (zan4)** — pass `auto` and `node_hunt.sh` picks cards with <2 GB used.
4. **First-batch latency** — the GPU scan is host-bound (~4800 nonce/s); the **first candidates take
   ~200 s**. `candidates=0` for the first few minutes is normal, not a hang.
5. **stdout buffering** — the verify daemon follows the scan log with `tail -F` + `stdbuf -oL grep`
   so candidates stream live instead of waiting for a 4 KB flush.
6. **vast.ai / container nodes are NOT in this fleet** — their `xargs -P` serialized vone.sh
   (container quirk) and self-scan on consumer GPUs was far too slow. Stick to zan-class boxes.
   For a CPU-only verify node, you can just copy the `fast-screen` binary (it builds the circuit
   in-process — needs no repo, no CUDA) and feed it candidates.

## Winner → bake → submit

A winner = a nonce where `fast-screen NS_EARLY_EXIT=1` prints `classical=0 phase=0 ancilla=0`
(full island clean). When `/tmp/hunt_WINNER` appears:

```bash
cd <REPO>
N=$(cat /tmp/hunt_WINNER)             # the winning nonce (from whichever node)
# 1. RE-FREEZE: bake N as a hard set_var so the env-less official grader uses it
sed -i "s/set_default_env(\"DIALOG_TAIL_NONCE\", \"3452376\")/std::env::set_var(\"DIALOG_TAIL_NONCE\", \"$N\")/" src/point_add/mod.rs
cargo build --release --bin build_circuit && ./target/release/build_circuit
# 2. VERIFY env-less: must be 0/0/0 AND score < current frontier
./target/release/eval_circuit          # expect "all 9024 shots OK"; read avg_tof*peak from results.tsv
# 3. commit (NO push) — user submits on the Mac with `ecdsafail submit`
git add -A && git commit -m "candidate: <knob> nonce=$N score=<...>"
```

Keep the knob edit + the baked nonce together in the commit. The grader runs with **no env**, so
the nonce MUST be a `set_var` (not `set_default_env`), else line 1469's default wins and it goes stale.
