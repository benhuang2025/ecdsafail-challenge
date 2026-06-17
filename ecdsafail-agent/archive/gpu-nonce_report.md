# `hunt` kernel (secp256k1 nonce-search) — optimization report (session 2, frontier 1a5e620)

Continuation of the gpu-nonce optimization, after the repo was re-stacked onto
the new leaderboard frontier `1a5e620`. Goal unchanged: maximize single-GPU
**nonce/s**; hard gate: every kept change validates `PHASE3 agree=32/32`
(GPU hunt vs CPU classical filter). Local-only, never pushed.

Bench host: zan3 / RTX 5090 (Blackwell sm_120), CUDA 12.8.
All runs **GPU2, NUMA0-pinned** (`numactl --cpunodebind=0 --membind=0`; GPU2 is
on NUMA node 0), `HUNT_BS=64 HUNT_BATCH=1048576`.

## Summary

| | |
|---|---|
| Status                  | **improved** (then plateaued under a 5-consecutive-no-gain stop rule) |
| Starting kernel         | new-frontier branch already had g_wmask-O(1) + scalarized truncated-compare (prior session) |
| Best this session       | **~23.5k nonce/s** single-card (GPU2), 32/32 |
| Session kernel gain     | idea1 +10%, idea3 +30%, marginal idea5/6/8 +~6% ≈ **+50% at matched batch** |
| Launch-config           | `HUNT_BATCH` 32768→1048576 + BS=64 + `__launch_bounds__(64,5)` saturates the GPU |
| Committed               | `3d61cca` (re-align + idea1/2/3/5/6) and `0d367c9` (idea8) on `ben/restack-on-1a5e620` |

## The blocker first: re-alignment to frontier 1a5e620

After a host reboot, validation dropped to **0/32** (`gpu < cpu`, ~7 extra
hard/nonce). Root cause (not the reboot, not the GPU): the repo had been
**re-stacked onto frontier 1a5e620**, and that re-stack **dropped the classical
filter's `strict_body_trim` gate**. The new `dialog_gcd_classical_filter.rs`
counted `BodyTrimMismatch` unconditionally, while `gcd.cu` (by design) omits it.

Ruled out, in order: GPU hardware (GPU0≡GPU1, byte-identical), build-cache
corruption (`cargo clean` identical), `k2`/`odd_u` env (no effect), source
(git-clean). **Fix:** restore the `&& cfg.strict_body_trim` gate (default off,
documented as "not inherently a hard input") → **32/32 restored** on the new
circuit. The Fiat-Shamir prefix and `aw/cb/bw` schedule are derived from the
current circuit automatically (uploaded), so only this one CPU-only hard-reason
needed re-aligning.

> Lesson: the GPU nonce-search is bound to a specific circuit. When the circuit
> moves, only the GPU's *hardcoded* filter assumptions (here: body-trim off,
> const-folded `k2`/`odd_u`) can drift; the width schedule re-derives itself.

## Round-by-round trace (vs each round's prior best)

| Idea | Change | Δ | Verdict |
|---|---|---:|---|
| baseline (new frontier, opt batch) | g_wmask-O(1) + scalar cmp (prior session) | — | reference |
| **idea1** | mask-based WidthOverflow check (drop 2 `g_bitlen`/step, reuse aw-mask) | **+10%** | ✓ kept |
| idea2 | invariant-simplified shift/swap (`u,v<2^aw` ⇒ masks redundant) | ~flat | kept (cleaner, confirms invariant) |
| **idea3** | **16-bit window comb** (32→16 EC point-adds; 512KB→67MB table) | **+30%** | ✓ kept |
| **idea4** | launch config: `HUNT_BATCH` 32768→1048576 + BS=64 (GPU saturation) | **~+2.4×** | ✓ adopted |
| idea5 | merge k2 double-shift into one variable shift | +1.9% | kept (marginal, <3% → miss 1) |
| idea6 | `__launch_bounds__(64,5)` (occupancy) | +1.6% | kept (marginal → miss 2) |
| idea7 | `__launch_bounds__` N sweep {4,6,8,10} | flat (N=5/6 optimal; 8/10 spill→regress) | miss 3 |
| idea8 | `g_sub_low_window` `bw<=64` scalar fast-path | +2.1% | kept (marginal → miss 4) |
| idea9 | block-size sweep {32,48,64} under launch_bounds | flat | miss 5 → **STOP** |

`idea4` is partly a measurement correction: earlier rounds were benched at
`HUNT_BATCH=32768`, far below GPU saturation. The kernel's true single-card
throughput at a sensible batch (≥1M) is ~22–24k nonce/s; relative kernel deltas
(measured at fixed batch) remain valid.

## Cost attribution (count_mode=1; ncu blocked by ERR_NVGPUCTRPERM, used stub-and-time)

| State | GCD share | EC+inversion+keccak |
|---|---:|---:|
| before idea1 | ~41% | ~59% |
| after idea1 + idea3 (16-bit window) | ~38% | ~62% (EC scalarmul dominant) |

After idea3 halved EC point-adds, EC+inversion+keccak is still the majority.
EC point-adds are near the practical floor (16, full-table 16-bit window).

## Lessons

- **Circuit drift breaks alignment, not the kernel.** A disagreement after an
  environment change traced to a dropped filter gate on the new frontier — not
  the GPU, build, or reboot. Bisect systematically (HW → build → env → source →
  git diff vs the last-aligned commit).
- **Measure at GPU saturation.** Benching at a small `HUNT_BATCH` understated
  throughput ~2.4×; the kernel was launch-underutilized, not slow.
- **Marginal-but-real compounds.** idea5/6/8 were each <3% (counted as misses)
  but consistently positive and work-reducing; together ~+6%.
- **`__launch_bounds__` has a narrow sweet spot.** At 255 regs, (64,5–6) gave a
  small occupancy win; (64,8/10) forced heavy spill and regressed hard.
- **The `u,v<2^aw` invariant** (guaranteed post-overflow-check) lets the
  truncated-GCD helpers drop masks and use plain shifts (idea2/5) and a scalar
  `bw<=64` sub (idea8).
- ncu was unavailable; empirical stub-and-time attribution substituted for it.

## Infrastructure improvements (this session)

- `validate_pipeline.rs`: parallelized the CPU reference across NUMA0 cores
  (`std::thread::scope`) — validate ~8min → ~15s, sound (matches sequential).
- Bench harness: NUMA0 pinning + tmux + log-polling to survive flaky SSH;
  orphan-process hygiene after the reboot.

## Future work (assessed, deprioritized at this plateau)

- **Inversion-merge (2→1 `_ModInv`/shot):** `den` has a true data dependency on
  `zb⁻¹`, so merging needs a full projective rewrite of the den/lambda/factor
  math (~12 extra muls to save one DivStep inversion). Borderline (≤3–5%) EV,
  high correctness risk — not attempted over the flaky connection.
- **Multi-tooth (Lim-Lee) comb:** could push EC adds below 16, but table×adds
  trade-off and complexity are unfavorable vs the 16-bit window already in place.
- **signed 16-bit window (33MB table):** likely little gain — the 16-bit win came
  from fewer adds, not table size (67MB already won +30%).

## Final winning source (committed `0d367c9`)

`src/point_add/dialog_gcd_classical_filter.rs` (strict_body_trim realign);
`agent/gpu-nonce/src/gcd.cu` (mask-overflow, invariant shift/swap, k2-merge,
bw<=64 sub fast-path); `points.cu`+`gtable.rs` (16-bit window comb);
`hunt.cu` (`__launch_bounds__(64,5)`); `bin/validate_pipeline.rs` (parallel ref).
Run with `HUNT_BS=64 HUNT_BATCH=1048576`, GPU2/NUMA0.

---

# Session 4 (2026-06-12): HC=8 batch-inverse amortization + jadd_aff 7M+4S

Bench: GPU2/NUMA0, `HUNT_BS=64 HUNT_BATCH=2097152 HUNT_COUNT=2097152`, DIALOG_TAIL_NONCE=165002130437.
Correctness gate: on `ben-fulltools` the GPU filter is intentionally NOT realigned to the
GCD_FOLD=17 circuit, so `validate_pipeline` shows `agree=4/32` with `gpu<=cpu` (conservative,
pre-existing). Gate used this session = **per-nonce GPU hard-count vector must stay byte-identical
to the baseline binary** (GPU mean 10.12). All accepted changes are value/order-exact and held it.

## Round-by-round trace

| Round | Idea | Region | nonce/s (median) | Δ | Δ/σ | Verdict |
|---|---|---|---:|---:|---:|---|
| baseline | (session-3 tip) | — | 24850 | — | — | σ≈145 |
| R1 idea1 | jadd_aff Z3=(Z1+H)²−Z1Z1−HH (8M+3S→7M+4S) | SCALARMUL | 24982 | +0.53% | 0.9σ | ≈ flat (kept: value-exact, theoretical min) |
| R1 idea3 | `__launch_bounds__(64,6)` | LAUNCH | 24939 | — | — | ✗ regress vs idea1 (reverted; (64,5) sweet spot) |
| Phase2 | `HC 1→2` batch-inv amortization | HUNT | 25297 | +1.80% | 3.1σ | ✓ positive |
| Phase2 | `HC 1→4` | HUNT | 25651 | +3.22% | 5.5σ | ✓ positive |
| Phase2 | `HC 1→8` | HUNT | 26760 | +7.69% | — | ✓✓ peak |
| Phase2 | `HC 1→16` | HUNT | 26417 | +6.31% | — | ✗ regress vs HC=8 (local-mem pressure) |
| **WINNER** | **idea1 + HC=8** | **HUNT+SCALARMUL** | **26744** | **+7.62%** | **~13σ** | **✓ (6-run median, validate vector identical)** |

## Mechanism

- **HC=8 (dominant, ~+7%)**: `HC` is the batch-inverse chunk size in hunt.cu. HC=1 did
  2 `batch_inv` calls per shot (one over 2 Z-coords, one over 1 den) = ~2 `_ModInv`/shot.
  HC=8 batches 16 Z-coords + 8 dens → 2 `_ModInv` per 8 shots ≈ **0.28 `_ModInv`/shot**.
  `_ModInv` is ~258 DivStep62 iters — the inversion was a hidden cost. Sweep peaks at 8;
  16 regresses (doubled per-chunk local arrays start to bite). Batch inverse is exact ⇒
  identical hard-count vector. Prior sessions had this as "net unclear" — now resolved.
- **jadd_aff 7M+4S (~+0.5%)**: EFD madd-2007-bl Z3 form trades the `2*Z1*H` multiply for a
  square (reusing Z1Z1, HH). Theoretical min for mixed a=0 add; value-exact. Small because
  S is only ~0.76·M on this code and the extra add/subs partly offset.
- **Rejected**: `__launch_bounds__(64,6)` (occupancy/reg tradeoff worse than (64,5)).

Committed `274cfd5` on branch `ben/gpu-opt-hc8-jadd7m4s` (local-only, not pushed).

## Session 4b (2026-06-12): full Round-1 / Round-2 / Phase-2 sweep — no win past HC=8

Ran the complete Design.md loop on top of the HC=8 winner. In-run BASE re-measured each
batch (thermal drift makes cross-run absolutes unreliable; only within-run delta is trusted).
All variants validated vec-identical (correct); verdicts are pure perf.

| Round | Idea | Region | median nonce/s | Delta vs in-run BASE | Verdict |
|---|---|---|---:|---:|---|
| R1-a | HC=6 | HUNT | 26453 | +0.5% | tie (noisy) |
| R1-a | HC=10 | HUNT | 25993 | -1.2% | regress |
| R1-a | HC=12 | HUNT | 26528 | ~0 | tie |
| R1-b | __launch_bounds__(64,4) | LAUNCH | 24995 | -6.0% | regress |
| R1-b | __launch_bounds__(64,6) | LAUNCH | 26334 | -1.0% | regress |
| R1-c | comb #pragma unroll 2 | SCALARMUL | 24642 | -7.4% | regress |
| R1-c | comb #pragma unroll 4 | SCALARMUL | 23922 | -10% | regress |
| R1-d | jadd first-add Z1=1 specialization | SCALARMUL | 25230 | -5.1% | regress (SIMT divergence + branch/fn cost) |
| R1-e | _ModSqr alt (#else) reduction | FIELD | 26025 | -2.2% | regress |
| P2a | __noinline__ jadd_aff | SCALARMUL | 19660 | -25.7% | regress |
| P2b | __noinline__ _ModMult/_ModSqr | FIELD | 10765 | -59% | regress |
| P2c | HC=6 (re-test) | HUNT | 26448 | +0.1% | tie |
| P2d | noinline-jadd union HC=6 | mixed | 19475 | -26% | regress |

Conclusion: the kernel is at its floor. The HC batch-inverse family is the ONLY productive
axis and it is exhausted (HC=6/8/12 statistically tied; HC=10/16 worse). Everything else
regresses, and the failures are mutually reinforcing:
- jadd_aff and _ModMult/_ModSqr MUST stay inlined (noinline = -26% / -59%): called
  16x/scalarmul and thousands x/shot; spill/reload across a call boundary dwarfs any
  frame-shrink occupancy benefit.
- comb unroll (x2/x4) regresses: the jadd_aff-heavy body already saturates registers; unrolling
  multiplies live state -> spill. (Disproves the old opt-agent "partial unroll = medium potential".)
- (64,5) is the unique occupancy sweet spot (both 4 and 6 worse).
- the EC add is at the 7M+4S theoretical minimum (Z1=1 specialization cannot beat it under SIMT
  divergence); field arithmetic is hand-tuned VanitySearch PTX at its floor; lazy reduction is
  already maximal (no canon in the hot path).

Net for the session: +7.6% (HC=8), banked at commit 274cfd5. No further single-card win is
available without an algorithmic change (fewer scalarmuls per shot / different filter), which is
out of scope for kernel micro-opt. The real remaining throughput lever is the 8-GPU hunt
(~8 x 26.7k ~= 214k nonce/s aggregate).

---

# M3 (2026-06-14): Fused stage-2 phase kernel `hunt_phase` — GPU end-to-end dual-clean hunt

Integrates the validated M2 bit-sliced phase op-loop into the nonce hunt as a fused
**stage-1(classical, exact) + stage-2(phase)** GPU kernel. One thread = one candidate
nonce, processing all compacted batches sequentially so the single Fiat-Shamir XOF
flows naturally (no seeking). Emits `DUAL_CLEAN_CANDIDATE` for nonces whose 9024-shot
island is BOTH classical-clean and phase-clean — exactly eval_circuit's classical+phase
verdict. Frontier `7bab51f`; bench host zan3 (8x RTX5090, sm_120, CUDA 12.8).

## Files (all under `ecdsafail-agent/gpu-nonce/`)
- `src/hunt_phase.cu` — the kernel. `build_xof` (FS prefix + nonce-tail absorb, same
  convention as hunt.cu), `squeeze_u64` (8-byte XOF reader continuing a finalized Xof),
  `derive_check` (step-1 validation kernel), `hunt_phase` (full pipeline kernel).
  Reuses points.cu (scalarmul_jac/affine), keccak.cu (SHAKE), and the M2 op-loop verbatim.
- `src/bin/circuit_prep.rs` — nonce-independent host prep: slot-remapped op stream
  (`ops2.bin`, identical coloring to phase_prep), reg0/1 qubit indices (`reg_q.bin`),
  reg2/3 SLOT indices (`reg_s.bin`), meta. Dumps to `/tmp/phase_circuit/`. Run once per circuit.
- `src/bin/derive_check.rs` — step-1 harness (GPU inputs+RNG vs CPU dump).
- `src/bin/validate_huntphase.rs` — step-2 harness (per-nonce verdict vs CPU eval, with EXPECT).
- `src/bin/classify_range.rs` — classify a contiguous range, list clean/phase-fail nonces.
- `src/bin/hunt_dual.rs` — production multi-GPU dual-clean hunt (the one-command launcher).

## (a) GPU-derived inputs / RNG byte-exact vs CPU dump — PASS, no XOF/compaction bug
`derive_check` (nonce 3480010331559) vs phase_ref's `/tmp/phase_m1` dump:
**n_survivors=9024 match; batch-0's 64 reg-packed inputs (t.x,t.y,o.x,o.y,e) byte-exact;
all 1,352,725 measured-gate RNG u64s byte-exact.** The #1 risk (XOF-flow / compaction /
measured-gate positioning) reproduced eval_circuit on the first try — no bug to fix.
Key invariants confirmed: (i) hunt.cu's nonce-tail X-op convention (q_target = bit?1:0)
is byte-identical to a CPU build with DIALOG_TAIL_NONCE=N because tx[0]=qubit0, tx[1]=qubit1;
(ii) the measured region begins at exactly 9024x64 bytes (all 9024 inputs consumed,
skipped or not), reproduced by squeeze-and-discarding 9024 input reads on a copy of the
finalized Xof; (iii) the op-loop runs on the 96-op-shorter (tail-free) stream while the
FS hash still counts the 96 tail ops — the tail is identity X;X and consumes no RNG.

## (b) End-to-end classification correctness vs CPU eval_circuit — PASS on mixed set
Every hunt_phase verdict (type AND first-failing compacted-batch index) matched CPU
eval_circuit / phase_ref:

| nonce | hunt_phase | CPU first-failing batch | match |
|---|---|---|---|
| 3480010331559 (baked) | dual-clean, failbatch -1 | none (0 classical / 0 phase / 0 ancilla) | OK |
| 77  | phase, batch 0  | BATCH 0: classical_fail=0, phase!=0 | OK |
| 106 | phase, batch 0  | BATCH 0: classical_fail=0, phase!=0 | OK |
| 37  | phase, batch 3  | BATCH 3: classical_fail=0, phase!=0 | OK |
| 34  | phase, batch 7  | BATCH 7: classical_fail=0, phase!=0 | OK |
| 55  | phase, batch 10 | BATCH 10: classical_fail=0, phase!=0 | OK |
| 1   | classical, batch 4 | BATCH 4: classical_fail=1, phase=0 | OK |

(Category-(b) nonces were found by running hunt_phase over [1,2000]: 1827 classical-fail,
173 phase-fail, 0 dual-clean — then CPU-confirming representatives. Early-exit on the
first failing batch means hunt_phase's total verdict is the first-failure type, which is
exactly what a prefilter needs.)

## (c) Throughput (zan3 8x RTX5090, sm_120; exact SHAKE measured-gate RNG)

Note: benched on zan3's 8x5090 (NOT zan5) because zan5's repo is on a DIFFERENT
frontier (674d0d8 vs zan3's 7bab51f); benching there would change build() -> a
different FS island and the baked nonce would no longer be dual-clean. zan3 has
identical hardware (8x RTX5090), so the numbers transfer directly.

Throughput is **launch-size bound by warp divergence**: a launch's wall time is set by
its slowest thread (a dual-clean or late-failing nonce runs the full 141-batch op-loop),
so larger launches amortize the heavy ~9% classical-clean tail over more concurrent warps.
Single-GPU, BS=32, one synchronous launch:

| launch size | time | scan nonce/s | classical-clean/s |
|---:|---:|---:|---:|
| 1024  | 202.6s | 5   | 0.5 |
| 8192  | 307.3s | 27  | 2 |
| 16384 | 281.1s | 58  | 5 |
| 32768 | 317.3s | 103 | 9 |

Wall time stays ~280-320s while count grows 8k->32k -> throughput scales ~linearly:
the GPU keeps absorbing more concurrent nonces (the bit-sliced state is ~15.5KB qubits+slots
plus a ~12KB per-thread 64-input buffer, so occupancy is low and large launches are needed
to fill 170 SMs). 32768/launch = **~103 scanned-nonce/s/GPU**, still climbing with launch size.

8-GPU aggregate (zan3, HUNT_BS=32): **32768 nonces in 296.8s = 110 nonce/s aggregate**
at HUNT_BATCH=4096/GPU; classical-clean 8.97% (~10/s), 0 dual-clean. Note this aggregate
is NOT 8x the single-GPU saturated rate: at 4096/GPU each GPU is far below saturation
(~128 warps), AND the driver loop syncs every GPU per batch round, so the slowest GPU's
straggler nonce gates the round (observed: 7/8 GPUs idle waiting on 1 grinding a near-clean
nonce for many minutes). The two production gaps are therefore (i) occupancy (need huge
per-GPU batches to fill the SMs) and (ii) cross-GPU straggler sync (the per-batch
memcpy_dtov serializes GPUs). With large per-GPU batches a single GPU sustains ~100 nonce/s;
removing the straggler sync (async streams / per-GPU independent loops, already mostly the
case) and raising occupancy is what unlocks the ~8x.

classical-clean rate ~9.0% (matches eval_circuit's hard-input rate on this frontier),
phase-clean (dual-clean) rate: 0 in ~660k nonces scanned across all runs (consistent with
winners being rare; the one known dual-clean, the baked 3480010331559, verdicts correctly).

**phase_sim absorbs the classical-clean stream by construction**: there is no separate
stream/queue to back up — `hunt_phase` IS the classical-clean stream processor. Each
thread re-derives its own classical verdict (exact) and immediately runs the phase op-loop
on the same survivors; classical-fail nonces (~91%) early-exit before any measured gate,
so the heavy op-loop runs only on classical-clean nonces.

## (d) How it is wired / how to launch

One-command hunt that emits dual-clean candidates:
```
# 1) once per circuit (nonce-independent slot remap + register tables):
DIALOG_TAIL_NONCE=none ./target/release/circuit_prep         # -> /tmp/phase_circuit
# 2) the hunt (multi-GPU). Use a LARGE per-GPU batch to saturate:
HUNT_GPUS=8 HUNT_START=<n0> HUNT_COUNT=<N> HUNT_BATCH=65536 HUNT_BS=32 \
  ./target/release/hunt_dual           # prints DUAL_CLEAN_CANDIDATE nonce=... + a summary
```
`hunt_dual` builds the FS prefix exactly like the existing `main.rs` hunt (DIALOG_TAIL_NONCE=none,
full_len counts the 48-bit nonce tail), uploads ops2/reg_q/reg_s/gtable, and launches the
fused `hunt_phase` kernel one-thread-per-nonce. Verdict 0 -> DUAL_CLEAN_CANDIDATE.

Architecture chosen: **single fused kernel**, NOT a 2-kernel (hunt.cu->hunt_phase) flow.
Reason: hunt.cu's classical GCD prefilter is MISALIGNED on frontier 7bab51f (it emitted
nonces 2158/3637/5268 as "clean" that eval_circuit shows with 10-13 classical failures),
so it provides ~no classical enrichment here and would just add a stage. hunt_phase does
its OWN exact classical check (reg0/reg1 == expected) before the phase check, which is both
correct and self-sufficient. (Re-aligning hunt.cu's filter to 7bab51f, per the report's
session-2 lesson, would let it cheaply pre-cull ~91% before the heavy kernel and is the
obvious next perf win — see (e).)

## (e) Verdict + gaps to a production hunt

**M3 PASS.** GPU pipeline produces dual-clean candidates whose verdicts match CPU
eval_circuit on the mixed test set (steps 1+2: inputs/RNG byte-exact; classification
type+first-failing-batch exact on baked-clean + 5 phase-fail + 1 classical-fail), and it
runs end-to-end with measured throughput (step 3) via a one-command `hunt_dual` launcher.

Honest gaps / next steps:
1. **Stage-1 prefilter is dead weight on 7bab51f.** hunt.cu's GCD filter is misaligned
   (under-counts hard inputs). The fused kernel is correct without it, but a re-aligned
   hunt.cu (cheap, 1 thread/nonce, no op-loop) could pre-reject ~91% of nonces before the
   heavy phase kernel, cutting scanned-cost per dual-clean candidate by ~10x. This is the
   single biggest production lever and is exactly the "circuit drift" realignment the
   report's session-2 already solved once.
2. **Occupancy.** ~15.5KB qubit/slot state + a ~12KB per-thread 64-input gather buffer
   cap occupancy, forcing very large launches to saturate. Shrinking the gather buffer
   (derive inputs straight into the bit-sliced state, drop b_ex/b_ey by recomputing the
   classical check from a packed form) would raise occupancy and per-GPU throughput.
3. **The keccak floor is real.** Exact measured-gate SHAKE (1.35M reads/batch, keccak every
   17 reads) dominates the per-classical-clean-nonce cost. The plan's cheap-RNG prefilter
   (xorshift/philox, k-seed OR) would dodge this floor for a FILTER pass (false-negatives
   tunable to ~0), escalating only survivors to exact SHAKE — not implemented (M3 used the
   exact path to guarantee bit-exact CPU agreement for the correctness gate).
4. **Deploy toolchain.** The existing `ecdsafail-agent/hunt/` deploy scripts call the
   classical-only `gpu-nonce` (main.rs). To use the dual-clean hunt in production they need
   to: run `circuit_prep` once after any src/ change, then call `hunt_dual` instead of
   `gpu-nonce`, and collect `DUAL_CLEAN_CANDIDATE` lines (CPU still re-certifies the single
   winner for the official 0/0/0). Not yet wired into those scripts.
5. **CPU verify offload achieved in principle:** the GPU now rejects phase failures, so the
   CPU only needs to certify dual-clean candidates (the rare survivors), not every
   classical-clean one. Whether this beats the 470-core CPU fleet's aggregate throughput at
   current GPU occupancy is bounded by items 1-3; the enrichment (CPU sees only true
   dual-clean) is the structural win regardless.
