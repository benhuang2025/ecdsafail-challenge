# GPU Phase-Prefilter — Prototype Plan

**Goal.** Enrich the *phase axis* of the nonce hunt on GPU, which is currently
**un-enriched** (the existing GPU filter `gcd.cu` only models the classical
GCD-clean axis). Today CPU `fast-screen`/`eval_circuit` is the bottleneck *because*
it is the only thing that runs the full circuit and catches phase failures. A GPU
kernel that runs the full bit-sliced circuit sim and checks `phase == 0` makes the
GPU reject phase-failing candidates, so the CPU only ever verifies candidates that
are clean on **both** axes → far fewer CPU verifies, hunt throughput collapses onto
the GPU.

**Scope / compliance.** This is a **hunt accelerator only**. The official certifier
stays CPU `eval_circuit` (we may not touch `sim.rs`/`bin/*`). The single winning
nonce is always re-certified on CPU. New code lives entirely under
`ecdsafail-agent/gpu-nonce/` (untracked, free to edit). Same contract the existing
GPU filter already operates under.

---

## 0. Why this is the highest-leverage lever right now

- The frontier's circuit knobs/maps/truncations are exhausted (see
  `ecdsafail-frontier-axes-exhausted` memory). The *meta*-bottleneck per the runbook
  is **verify throughput**, which gates which optimizations are huntable at all.
- The CPU verify is heavy for a structural reason: the measured-uncompute-heavy
  frontier emits **1,352,725 measured gates (Hmr+R) per shot-batch** out of 10.3M
  ops, each consuming an 8-byte SHAKE256 read. Enriching the phase axis on GPU is
  worth more than any single circuit win because it expands the *huntable set*.
- Note: this does **not** rescue *over-cliff* candidates (P(winner) too small no
  matter the throughput). It only widens the *sub-cliff, verify-bound* regime.

---

## 1. Measured facts this plan is built on (frontier 7bab51f)

| fact | value |
|---|---|
| total ops / shot | 10,301,716 |
| op mix | CX 43% · X 18% · CCX 15% · CZ 8% · Hmr 8% · R 5% · Swap 3% · Push/PopCond 11k each · Z 7.7k |
| measured gates (R+Hmr) | 1,352,725 / batch (→ SHAKE load) |
| max qubit id | 1169 (state ≈ 1170 qubits + `num_bits` classical bits) |
| ops.bin size | 577 MB (56 B/op) |
| shots | 9024 |
| CPU verify throughput | ~40 nonce/s across ~470 cores (fleet) |

**Simulator model (`src/sim.rs`)** — *not* a statevector sim. Bit-sliced classical
sim: `qubits: Vec<u64>` packs **64 shots per u64**; gates are bitwise
(`v = cond & qubit(c1) & qubit(c2); qubit(t) ^= v`). A single `phase: u64` accumulates
the phase parity of 64 shots. Measured gates read an 8-byte SHAKE256 `rng_val`:
`phase ^= qubit(t) & rng_val & cond; qubit(t) &= !cond`. A circuit is phase-clean iff
`phase == 0` on every shot. This whole model is exactly what GPUs are good at.

---

## 2. What is already on GPU and reusable (large head start)

From `ecdsafail-agent/gpu-nonce/`:

- **`hunt.cu`** already derives, *per nonce, per shot*, the full point-add input:
  rebuild the Fiat–Shamir SHAKE state (`shake_init_from(st0,buf0,pos0)`), absorb the
  nonce bits as fake ops, finalize, `squeeze_scalars` → two scalars → `scalarmul_jac`
  on the precomputed `gtable` → affine points (xt,yt),(xo,yo) → classical λ, and the
  GCD factors dx, c. **This is exactly the per-shot input the CPU circuit consumes.**
- **`keccak.cu`** — device SHAKE256 (`Shake`/`Xof`, `keccak_f`), bit-for-bit vs
  `keccak.rs`. Available if we want the *exact* measured-gate RNG.
- **`points.cu` / `gtable`** — EC scalarmul + batch inversion, validated.
- **`main.rs`** — multi-GPU orchestration, per-step width arrays (aw/cb/bw), NVRTC
  compile, candidate plumbing (`CLEAN_CANDIDATE nonce=…`). The new kernel slots into
  the same launch loop.

So the prototype does **not** re-derive inputs or re-implement keccak/EC — it reuses
hunt.cu's derivation and adds the missing piece: **run the actual op stream + check
phase.**

---

## 3. The new kernel `phase_sim` — architecture

**Decision: one nonce per *block*, shots bit-sliced across the block, op stream
streamed once per block.** (Per-thread streaming of 577 MB is fatal: 9024 threads ×
577 MB = TB/nonce.)

- **Shot packing.** Each thread holds `W` shots bit-sliced in a `uint32`/`uint64`
  (W=32 or 64), mirroring the CPU. A block of `T` threads covers `T·W` shots; for 9024
  shots use e.g. T=256, W=64 (16384 lanes, 9024 active). Phase is a per-lane word.
- **State.** `qubits[1170]` + `bits[num_bits]` as W-bit words per thread → ~1170×8 =
  ~9.4 KB/thread for W=64. Lives in **local memory** (L1/L2-cached). This is the main
  occupancy risk — see §6 (mitigations: smaller W, qubit-id remap to shrink the live
  set, or tile the op stream).
- **Op stream.** Re-encode compactly off-device: `kind`(5b) + `q_control2/1/target`
  (11b each) + `c_target/c_condition`(≤16b) ≈ **8–12 B/op → ~100 MB** instead of 577 MB.
  Stream once per block; because every block reads the *same* stream in near-lockstep,
  L2 reuse is high. Threads in a block cooperatively load each op (broadcast via shared
  mem) and apply to their own shot-lanes.
- **Input init.** Reuse hunt.cu derivation per shot-lane → `write_register`-equivalent:
  scatter the operand bits (offset_x/offset_y classical bits, initial tx/ty) into the
  declared register lanes. (Read `Register`/`AppendToRegister` ops + `eval_circuit`'s
  input-write path to mirror byte layout.)
- **Op loop.** Port the `src/sim.rs` match arms verbatim into device code, bit-sliced:
  CCX, CX, X, Swap, CCZ, CZ, Z, Neg, BitInvert, BitStore0/1, Hmr, R, PushCondition,
  PopCondition (condition stack in local mem), Register/Append/DebugPrint = no-ops.
  Keep a per-lane `phase` word; `Push/PopCondition` maintain `current_base_condition`.
- **Measured gates (Hmr/R) — the RNG choice:**
  - **Prefilter (recommended first):** a cheap per-lane PRNG (xorshift/philox), seeded
    per (nonce, lane). Phase garbage = a freed qubit that is `1` on some shot →
    `phase ^= qubit & rng` flips on ~½ of RNG draws regardless of source, so a cheap
    RNG still *detects* the failure. Avoids the 1.35M-SHAKE/batch keccak floor entirely.
    Drive false-negatives down by running k≥2 independent seeds and OR-ing the reject.
  - **Exact (later):** per-shot SHAKE256 stream via keccak.cu (heavier; needed only if
    we ever want GPU to *replace* CPU cert rather than prefilter).
- **Accept test.** Reject the nonce if **any** shot-lane has `phase != 0` after the
  full stream (and optionally check output-register == expected + freed-qubit-clean).
  Output: 0 = phase-clean candidate, else first failing shot.

---

## 4. Pipeline integration — two GPU stages, then CPU cert

```
GPU stage 1  (existing hunt.cu, 1 nonce/thread, cheap)
   → classical GCD-clean candidates           [enriches classical axis]
GPU stage 2  (new phase_sim, 1 nonce/block, heavy, cheap-rng, staged shots)
   → ALSO phase-clean candidates               [enriches phase axis ← NEW]
CPU verify   (fast-screen / eval_circuit, full 9024, exact SHAKE)
   → confirm a handful of dual-clean candidates → WINNER
CPU eval_circuit on the winner                 → official 0/0/0 cert
```

**Key efficiency:** phase_sim (heavy: full circuit) runs **only on stage-1
survivors** — the classical filter already cut the set by `e^(−λ_cls)`, so the heavy
kernel sees a small fraction of nonces. Its cost is amortized; it is not run on every
scanned nonce.

**Staged shots:** run phase_sim on a reduced shot count first (e.g. 512 → 2048),
reject early; only escalate survivors to full 9024. A phase failure usually hits a
large fraction of shots, so a small subset rejects most failures cheaply.

---

## 5. Validation plan (do this in M1, before trusting it)

1. Pick the current baked `DIALOG_TAIL_NONCE` (known 0/0/0) → phase_sim **must pass**.
2. Collect 3–5 nonces that CPU eval shows as **phase-fail** (e.g. from a known
   over-cliff knob like `CSWAP_BODY_TRIM=1`, which gave 35 phase batches) → phase_sim
   **must reject** each.
3. Cross-check on ~100 random nonces: phase_sim verdict vs CPU `eval_circuit`
   phase-garbage verdict. Target **0 false-negatives** (never pass a phase-failer);
   false-positives are acceptable (CPU re-checks). Measure cheap-RNG false-negative
   rate; raise seed count k until it's ~0.
4. This mirrors `validate_pipeline` for the classical filter — make it a
   `validate_phase_pipeline` bin.

---

## 6. Risks / open questions (all measurable in M1–M2)

- **Local-mem pressure.** ~9.4 KB/thread state may cap occupancy → throttle. Mitigate:
  W=32; remap qubit ids to the live working set (peak 1170 but maybe fewer live at
  once — though sim needs random access, so likely full); tile the op stream with
  state in L2.
- **Op-stream bandwidth.** Even at ~100 MB compact, blocks stream it repeatedly; relies
  on L2 reuse. Measure effective nonce/s; this is the dominant unknown.
- **Net speedup is bounded, not 100×.** GPU crushes CPU on the branchy 10M-op dispatch,
  but the CPU fleet (470 cores) already has large aggregate keccak/bitwise bandwidth.
  Realistic target single-digit→low-tens ×. **Benchmark before committing further.**
- **Measured-gate semantics correctness.** Hmr is measured-*uncompute* (qubit need not
  be 0 at Hmr; the replay cancels phase) — must port the exact `phase ^= q & rng & cond`
  semantics, not a naive "assert 0". Validate per §5.
- **Cheap-RNG false-negatives.** Quantify; k-seed OR mitigates.

---

## 7. Milestones & go/no-go

- **M1 — correctness skeleton.** `phase_sim` kernel, exact SHAKE, modest shots,
  one-nonce-per-block. Reuse hunt.cu input derivation + sim.rs op semantics. Validate
  classification vs eval_circuit (§5). *Gate: 0 false-negatives on the test set.*
- **M2 — benchmark.** Measure phase_sim nonce/s on a stage-1-survivor stream on
  zan5 (8×5090). Compare to CPU 40/s. *Gate: ≥3–5× effective verify throughput (or a
  large enough phase-enrichment that CPU verify count drops ≥1 order) → continue; else
  stop, the keccak/bandwidth floor wins.*
- **M3 — optimize + integrate.** Cheap-RNG, staged shots, compact op encoding, wire
  into `main.rs` stage-1→stage-2→CPU pipeline; add `validate_phase_pipeline`.

---

## 8. First concrete coding step

Add `phase_sim.cu` + a `phase_sim` bin (clone of `validate_pipeline.rs` harness):
load ops.bin (or the compact re-encode), launch one block per test nonce, port the
sim.rs match arms, exact SHAKE, run the **baked clean nonce + the CSWAP_BODY_TRIM=1
phase-fail nonces** through it, assert the verdicts match CPU. That single
validation bin de-risks the whole idea before any perf work.
