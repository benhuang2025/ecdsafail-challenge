# Lever B: 2-stage GPU nonce-hunt (gcd.cu prefilter -> phase op-loop)

Host zan3 (8x RTX5090). Repo @ 35ceb01. Workdir ecdsafail-agent/gpu-nonce/.
NEVER edit src/**. NEVER git push.

Goal: stage-1 gcd.cu (~8640/s, analytical, no op-loop) rejects classically-hard nonces
CONSERVATIVELY (0 false-hard: never reject a truly-clean nonce). false-clean harmless
(stage-2 re-checks). System throughput = gcd.cu rate ~= 43x over hunt_dual (~200/s).

Ground truth = OP-LOOP (phase_ref.rs classical_failures: per-shot gx!=exp.0||gy!=exp.1),
NOT the CPU model dialog_gcd_classical_filter.rs (stale, last touched 674d0d8).

## Key facts established (reading phase)
- gcd.cu `check_gcd_factor`: hard iff terminal u!=1 OR !TAIL3_ok OR !HEAD11_ok.
  HEAD11_MASK[512] + TAIL3_SUP[32] are HARDCODED from 674d0d8 -> stale on 35ceb01.
  aw/cb/bw uploaded from DialogGcdFilterConfig::from_env() (auto-current, picked up after build()).
- hunt.cu `hunt` kernel: per shot derives dx=Px-Qx, cc=Qx-Rx (== point_add_gcd_factors),
  hard iff check_gcd_factor(dx)||check_gcd_factor(cc). count_mode=1 counts all 9024.
- align_scan.rs: runs hunt kernel count-mode over [START,COUNT), dumps per-nonce hard count
  + gcd-CLEAN set. Calls build() (runs set_default_env) THEN from_env() -> schedule current.
- phase_ref.rs (DIALOG_TAIL_NONCE=N): full op-loop, prints FULL-RUN VERDICT classical_failures.
  nonce clean (classical) <=> classical_failures==0. Authoritative.
- 35ceb01 baked env: ACTIVE_ITERATIONS=258, COMPARE_BITS=46, WIDTH_MARGIN=10, SLOPE=1015,
  K2=1, ODD_U=1, PA9024_COMPARE_SCHEDULE=1 margin 0, TOBITVECTOR_CSWAP_BODY_TRIM=0,
  RAW_TOBITVECTOR_MATERIALIZED_SUB=0. Active codecs: K5_HEAD11_CODEC=1, K5_TAIL3_TOP32_CODEC=1
  (TAIL3_FIXED_LAST=0; no TAIL6/7/HEAD_PAIR/etc). Plus many fold/special-fold/clean-step-bits
  configs that the analytical gcd.cu replay does NOT model (it only does truncated-GCD steps).

## Plan
1. Per-shot diag harness: K nonces x 9024 shots, gcd.cu decision vs op-loop truth. Measure
   false-hard / false-clean. Root-cause false-hard.
2. Make gcd.cu conservative: regen HEAD11/TAIL3 tables for 35ceb01, fix replay if needed.
3. Wire 2-stage pipeline.
4. Self-calibration in circuit_prep.
5. Benchmark 8 GPUs.

## Progress log
- (in progress) Phase 1 harness build.
