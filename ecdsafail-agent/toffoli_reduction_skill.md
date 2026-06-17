---
name: ecdsafail-toffoli-reduction
description: >-
  Durable levers for lowering AVG EXECUTED TOFFOLI in the ecdsafail / reversible
  secp256k1 point-add circuit (Toffoli is one factor of score = avg executed
  Toffoli × peak qubits), while keeping peak qubits flat. Use whenever the qubit
  peak is at a wall and the win must come from cheaper gates. Frontier-number
  independent — re-apply to whatever phase dominates the Toffoli budget.
  Keywords: Toffoli, CCX, measured uncompute, Hmr, exact adder, truncated
  comparator, carry truncation, ancilla cleanup, value-exact.
---

# ecdsafail — Toffoli-Count Reduction (peak-flat, value-exact-or-island-exact)

> **Subagent 调用**：本文档由独立 subagent 单独运行。按下述流程执行 Toffoli 减少优化，完成一轮后将结果（是否找到改进、新分数）汇报给主 agent，然后关闭。

These levers cut the avg-executed-Toffoli factor without raising peak qubits.
They are the complement of the peak-reduction skill (`ecdsafail-peak-reduction`):
when the qubit peak is walled, this is where the score win comes from. Each is a
real frontier win from the last 10 commits; re-apply the *method* to new phases.

Work on host **zan3** (`ssh zan3`, dir `/home/ubuntu/ben/ecdsafail-challenge`,
`export PATH=$HOME/.cargo/bin:$PATH`). Edit ONLY `src/point_add/`. NEVER
`git push`. `git fetch`/`ecdsafail sync` to the live frontier first.

## EMITTED vs EXECUTED Toffoli — what is actually scored

The scored term is **avg EXECUTED Toffoli over the 9024 Fiat–Shamir shots**, NOT
the emitted/static gate count. A gate that is present in the circuit but only
*fires on a fraction of shots* (data-dependent or measurement-conditioned) costs
its firing fraction, not 1. This opens a whole axis distinct from shrinking the
static circuit: **make expensive gates fire less often** (Levers E, B) — exact on
every shot, but cheaper on average. When you measure a lever, read the
`avg executed Toffoli` line, not the emitted-ops count; an "emit-T" delta and an
"exec-T" delta can differ a lot.

## Two correctness regimes — know which one a lever is in

- **Value-exact** (preferred): the change computes the SAME function on ALL inputs
  (only gate scheduling / ancilla handling differs). It adds ZERO new hard inputs.
  Levers B (measured uncompute) and C (exact-adder swap) are here.
- **Island-exact** (truncation): the change drops high bits that are *provably
  zero on the hunted Fiat–Shamir island* but not on all inputs. It SAVES Toffoli
  but ADDS hard inputs → raises λ → needs a fresh clean-nonce hunt (see the
  `ecdsafail-clean-nonce` skill). Levers A (truncated comparator) and D (carry
  notches) are here.

Either way, ANY change to the op stream re-rolls the FS island ⇒ the baked
`DIALOG_TAIL_NONCE` goes stale ⇒ re-hunt + re-validate 0/0/0.

## Lever A — truncated-suffix cleanup comparator (island-exact)

Row-windowed / chunked operations leave an inter-window carry/borrow ancilla that
must be cleared after each window. The baseline recomputes that carry with a
**full-width** comparator (~2n CCX). Instead recompute it from a **high suffix**
of the segment only — the dropped low bits don't affect the boundary carry on the
island. Fewer CCX, peak-flat.
- `SQUARE_ROW_WINDOW_CLEAN_COMPARE_BITS=<k>` (round84 square boundary cleanup;
  commits `1a5e620` introduced it, tuned 22/18/22 across the stack; **`5a783e4`
  cut 22→21** as a standalone frontier win — confirmed still huntable with the
  GCD-based pre-filter).
- `DIALOG_GCD_APPLY_CLEAN_COMPARE_BITS=<k>` (apply ripple cleanup).
- `DIALOG_GCD_COMPARE_BITS=<k>` (GCD compare width).
Lower k = fewer Toffoli but more carry-escape hard inputs → re-hunt. Find the
smallest k whose first GCD-clean candidate still hits a fully clean island.

**Current frontier values (461a4a3):** `SQUARE_ROW_WINDOW_CLEAN_COMPARE_BITS=21`,
`DIALOG_GCD_APPLY_CLEAN_COMPARE_BITS=19`, `DIALOG_GCD_COMPARE_BITS=46`. Each knob
is an independent axis — try each individually, screen with a FAIL row first.

## Lever B — measurement-based uncompute instead of coherent recompute (value-exact)

A cleanup bit that is coherently *recomputed* to uncompute it costs CCX. Instead
**measure** it out and replay the predicate classically:
`Hmr(cout)` (measure-and-reset in the Hadamard basis), then replay the comparator
predicate **conditioned on the measurement result** to cancel the phase. The
replay is classically-controlled ⇒ 0 Toffoli, phase-exact. As long as the
comparator borrows an already-clean high tail, peak is unchanged.
- `SQUARE_ROW_WINDOW_MEASURED_CARRY_CLEAR=1` (commit `d636d62` — recovered ~387
  avg-Toffoli at 1218q, value-exact).
- `DIALOG_GCD_FUSED_OVFCLEAR_MEASURED=1` (commit `bde4caa`) — fuse the GCD
  overflow-clear uncompute with a measurement, collapsing its Toffoli to 0.
  Enabled in current frontier base.
This is the cleanest class: value-exact (no new hard inputs) AND Toffoli-negative.
Look for every place a cleanup/scratch bit is coherently recomputed purely to
return it to |0⟩ — those are measured-uncompute candidates.

## Lever H — phase-conditioned comparator replay (value-exact, executed-Toffoli only)

An extension of Lever B to entire **comparator paths**, not just single cleanup
bits. Instead of running a full `cmp_lt` coherently:

1. **Measure out the comparison flag** via HMR (Hadamard-measure-reset).
2. **Replay the comparator predicate** classically (CZ / CX tree conditioned on
   the measurement result) to cancel the global phase.
3. Result: the Toffoli cost of the comparison gate collapses to near 0 on most
   shots; peak is flat (no new scratch).

Committed in `98dd2ad` and `bfd3fa6`; baked into the frontier base:
- `MOD_FAST_FLAG_CONDITIONAL_REPLAY=1` — modular-inversion comparison flags.
- `DIALOG_GCD_REVERSE_BRANCH_CONDITIONAL_REPLAY=1` — branch-direction comparison.
- `DIALOG_GCD_SPECIAL_CLEAN_CONDITIONAL_REPLAY=1` — overflow-clean comparator.
- `DIALOG_GCD_APPLY_BOUNDARY_CONDITIONAL_REPLAY=1` — apply-phase boundary comparison.

**All four are already ON in the 461a4a3 baseline** — they are not tuning knobs
for the next win; they are committed wins that show the class. Apply this pattern
to any remaining comparator (or `cmod_add`/`cmod_sub`) that is still coherent:
find it with `TRACE_PEAK`, check it emits measurable Toffoli, write a
`_phase_conditioned` variant, gate behind an env flag.

The key primitive is `cmp_lt_phase_conditioned()` /
`cmp_lt_phase_conditioned_borrowed_carries()` in `compare.rs` — study these for
the pattern before writing a new variant.

## Lever C — exact-adder swap (removing a truncation can REDUCE Toffoli) (value-exact)

Counterintuitive but real: a *truncated / top-clean'd* adder can cost MORE Toffoli
than an EXACT full-carry adder, because the truncation needs carry-escape
correction logic. Swapping back to the exact adder is value-exact (drops zero
value bits, no new hard inputs, peak unchanged) AND recovers Toffoli.
- `DIALOG_GCD_APPLY_FINAL_TOPCLEAN=0` (commit `98e1322`: apply-final adder
  exact-full-carry instead of top-clean default → recovered ~2,597 avg-Toffoli,
  value-exact). Bonus: fewer truncations ⇒ FEWER hard inputs ⇒ easier nonce hunt.
Audit every truncated adder/comparator: if its correction overhead exceeds the
bits it saves, the exact variant is a free Toffoli win AND a λ reduction.

## Lever D — carry-truncation notches (island-exact)

Drop one more high carry bit from a modular fold/reduction ripple. Saves Toffoli
(shorter ripple), peak-flat, but adds carry-escape hard inputs → re-hunt.
- `KAL_FOLD_CARRY_TRUNC_W` (in-place fold modular reduction; `00fb66d` 18→17).
- `KAL_DOUBLE_CARRY_TRUNC_W` (double_y fold).
- `ROUND84_INPLACE_QUOTIENT_CARRY_TRUNC_W` (round84 quotient carry; `f4404de`=20).
- `DIALOG_GCD_FOLD_CARRY_TRUNC_W` (fused fold; **`461a4a3` cut 19→18** as a
  frontier win — confirmed in combination with `APPLY_CHUNKED_F_BLOCKS` + 
  `SQUARE_ROW_MAX_SEG` + `SPECIAL_FOLD_BORROW_CARRIES`).

**Trajectory (KAL_FOLD_CARRY_TRUNC_W):** 21 (`da55bbb`) → 20 → 19 → 18 → 17 (`00fb66d`).
**Current frontier (461a4a3):** `DIALOG_GCD_FOLD_CARRY_TRUNC_W=18`, `KAL_FOLD_CARRY_TRUNC_W` not present (removed or absorbed).
The next notch (→17) may still be possible depending on the new base's λ — screen
first, then judge re-hunt cost.

These are at/near the **cliff** on a mature base — each extra dropped bit raises λ
steeply, and the Toffoli headroom can be too tight to afford the re-hunt cost.
Cheap to *try* (flip the env, measure avg-Toffoli on a FAIL row) but low-odds for a
net win once the base is tuned. Don't grind here if λ is already high.

## Lever E — data-dependent gate eliding (value-exact, executed-Toffoli only)

Skip gates whose control is *known zero on this shot*. The binary-GCD / Kaliski
path has conditional double/halve shifts gated by an edge bit; on shots where that
edge bit is 0 the shift is a no-op, so eliding it removes its EXECUTED Toffoli
while staying exact on every shot. Emitted count unchanged; AVERAGE executed drops
by the gate's fire fraction. (`zero-edge conditional-shift skip`, commits
`7822c37`/`b8ce940`.) Look for any conditional sub-circuit whose controlling bit
is frequently 0 across the island — eliding it is a free executed-Toffoli win.

## Lever F — measured-fast adders on NON-peak-setting adders (value-exact)

A coherent ripple adder costs ~2 CCX/bit. A **measured-fast** adder (measurement-
based carry, Hadamard+CZ) costs ~1 Toffoli/bit. Converting an adder is value-exact
(composes to identity, ancilla-garbage 0) and peak-flat ONLY if that adder is not
the one setting the peak — so convert the *small / non-binding* adders and LEAVE
the wide peak-setting adder coherent (converting it wouldn't lower width anyway).
- `c27e8a5`: round84 fold's three small fixed-width adders → measured-fast,
  −1,434 exec-T, peak flat (large 224/256-bit fold adders left coherent).
- `b7515d5`: **big-fold split adder** — convert a wide adder to an *asymmetric
  2-block windowed* measured-fast adder (a small low block with cheap carry
  uncompute + a wide fully-fast high block) to capture most of the fast saving
  without raising peak (−2,124 exec-T); plus **measured control-ride uncompute**
  (Hadamard+CZ instead of a coherent Toffoli, −528 exec-T).
General rule: every coherent adder/uncompute that is NOT peak-binding is a
measured-fast candidate. (Lever B is the cleanup-bit special case of this.)

## Lever G — cheaper exact primitives & constant recoding (value-exact)

Replace a primitive with a Boolean-identical but cheaper one, or recode a constant:
- **Majority folding identity**: `maj(a,k,c) = c ⊕ ((a⊕c) & (k⊕c))` computes a
  controlled-const add/sub carry with fewer Toffoli than a literal 3-input
  majority, same carry (`2a87f33`).
- **NAF / signed-digit recoding** of a constant product: e.g. `977 = 2^10 − 2^5 −
  2^4 + 1` (4 signed terms) instead of `1 + 2^4 + 2^6 + 2^7 + 2^8 + 2^9` (6
  unsigned terms) → fewer add/sub passes in the reversible product/unproduct
  (`ed94ad2`, `R84_QPROD_NAF=1`). Audit every constant multiply (secp256k1's
  `c = 2^32 + 977`, the Solinas folds) for a shorter signed-digit expansion.

## GCD iteration / body tuning knobs (mixed peak/Toffoli)

The binary-GCD reduction exposes structural knobs that trade executed Toffoli vs
peak — tune them together with `TRACE_PEAK`:
- `DIALOG_GCD_ACTIVE_ITERATIONS=<n>` — fewer reduction iterations = fewer executed
  Toffoli, exact if the dropped iterations are provably redundant on the support
  (`f6f9536`=258, `2a87f33`=260).
- `DIALOG_GCD_BINDER_NOTCH_MAP=11:1,12:1,...` — per-binder-iteration notch map;
  extend by one step at a time. Trajectory: `"13:1"` → `"11:1,12:1,13:1"` → more
  steps (`ab5469b`, `2dcf00d`). Add one entry at a time; check λ after each.
- `DIALOG_GCD_BINDER_NOTCH_EXTRA=<n>` — global extra-notch count for all binder
  steps (`a4db9a5`: 2→3). Cheap to try; raises λ if too aggressive.
- `DIALOG_GCD_BINDER_NOTCH_STEPS="8,9,10,11"` — which GCD steps have fallback
  notches enabled (`73f4f48`, `f3820d0`: expanded from "9" to "8,9" to
  "8,9,10,11"). More steps = finer body width control per iteration.
- `DIALOG_GCD_BODY_CARRY_BAND_TRIMS="0,2,2,2,..."` — per-step carry-band trim
  vector; each element tunes the carry-band width for one GCD step independently
  (`bde4caa`, `8983da8`). Gives per-step precision beyond a single global trim.
  Notation: `"0,3,3,3,..."` means step 0 untrimmed, steps 1+ trimmed by 3 bits.
- `DIALOG_GCD_BODY_STEP_GIVEBACKS=10:6` + step-stream top-clean — the
  **carry-relief exchange**: spend top-clean coherent uncompute to avoid extra
  live carry lanes (peak-neutral), then spend the recovered Toffoli budget on one
  more GCD iteration / a fuller body (`2a87f33`). A peak↔Toffoli rebalance, not a
  pure cut.

**Tuning order:** ACTIVE_ITERATIONS first (global budget), then BINDER_NOTCH_MAP
(per-step fallback), then BODY_CARRY_BAND_TRIMS (per-step fine-grain). Each step
raises λ independently — screen with a FAIL row and measure hard-input count
before committing to a re-hunt.

## Workflow

1. `git fetch` to the live frontier; `TRACE_PEAK=1 build_circuit` to confirm the
   qubit peak is walled (else use `ecdsafail-peak-reduction` instead).
2. Pick a lever; flip its env in `src/point_add/mod.rs set_default_env` (or export
   it for a quick `eval_circuit`). A correctness FAIL still prints avg-executed
   Toffoli + qubits — read the score impact BEFORE hunting a nonce.
3. Value-exact lever (B, C) → the only failures should be the base's intrinsic
   hard inputs; a clean nonce exists at ~the base rate. Island-exact lever (A, D)
   → confirm the classical-mismatch count didn't blow up, then re-hunt.
4. Re-hunt clean nonce (`ecdsafail-clean-nonce` skill) → `eval_circuit` 0/0/0 +
   score < live frontier. Rebuild `agent/fast-screen` + `agent/gpu-nonce`
   (`cargo clean`) so they aren't stale; `validate_pipeline` 32/32.

## Discipline

- Prefer value-exact levers (B, C) — no λ cost, easiest to ship. Truncation levers
  (A, D) trade Toffoli for hard inputs; only worth it if the hunt stays feasible.
- Every op-stream change ⇒ stale nonce ⇒ re-hunt. Budget that cost into the win.
- Re-check the live frontier before submitting; the leaderboard moves several
  times a day and a stale base wastes the hunt.
