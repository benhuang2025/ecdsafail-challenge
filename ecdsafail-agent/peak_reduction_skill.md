---
name: ecdsafail-peak-reduction
description: >-
  Durable technique for lowering PEAK QUBITS in the ecdsafail / reversible
  secp256k1 point-add circuit (peak qubits is one factor of score = avg executed
  Toffoli × peak qubits). Use whenever the goal is to drop the qubit count by 1+
  to beat the frontier. The method is independent of the current frontier number
  — it re-applies to whatever phase is the peak high-water binder this week.
  Keywords: peak qubits, high-water, ancilla lifetime, free-and-recompute,
  freed-tail, qubit recycling, range-bound, guard bit, sign-guard, TRACE_PEAK.
---

# ecdsafail — Peak-Qubit Reduction

> **Subagent 调用**：本文档由独立 subagent 单独运行。按下述流程执行峰值 qubit 优化，完成一轮后将结果（是否找到改进、新分数）汇报给主 agent，然后关闭。

Two complementary levers have repeatedly cut the qubit factor on this challenge
(1221→1220→1218). Both survive frontier updates: the *number* changes, the
*method* does not. When someone else advances the frontier, re-run them against
the new peak binder.

- **Lever A — free-and-recompute idle ancillae** (the frontier's 1220→1218 win):
  shorten an ancilla's lifetime across the widest region by freeing it and
  recomputing it value-exact later.
- **Lever B — range-bound a register to drop a guard qubit** (our 1221→1220
  idea): prove a register's top (sign/overflow guard) bit is always |0⟩ by
  constraining operation order/range, then delete it.

Both keep avg-Toffoli flat (or nearly so) and drop peak by ≥1 — directly cutting
the score.

Work is on host **zan3** (`ssh zan3`, dir `/home/ubuntu/ben/ecdsafail-challenge`,
`export PATH=$HOME/.cargo/bin:$PATH`). Edit ONLY `src/point_add/`. NEVER `git push`
(local commits only). Always `ecdsafail sync` / `git fetch` to the current
frontier before optimizing — the leaderboard moves several times a day.

# Lever A — free-and-recompute idle ancillae

## Core insight

`peak_qubits` (in `src/point_add/mod.rs`) = the **maximum number of simultaneously
live qubits** over the whole circuit. It is set by the single *widest* region:
a persistent floor (target_x/y, GCD work regs, accumulator, …) PLUS the transient
ancillae held live across that region.

So peak is NOT lowered by making a phase cheaper — it is lowered by **shortening
how long ancillae stay allocated across the widest region**. If even ONE ancilla
that is *idle* across the wide region can be freed there and recomputed later,
the freed slot is reused by the region's own allocations and **peak drops by 1**.
Dropping the peak by 1 is worth ~0.08% of score (≈ avg_toffoli) — a large win
here.

## The mechanism (harness primitives, `src/point_add/mod.rs`)

- `b.alloc_qubit()` / `b.alloc_qubits(n)` — allocate; pulls from the free pool if
  non-empty (reusing a freed slot, NOT raising peak), else extends and may raise
  peak.
- `b.free(q)` / `b.free_vec(qs)` — return a qubit to the free pool;
  `active_qubits -= 1`. The qubit MUST hold |0⟩ (be uncomputed) before freeing.
- `b.reacquire(q)` / `b.reacquire_vec(qs)` — pull a SPECIFIC previously-freed
  qubit back; `active_qubits += 1`, updates peak.
- `peak_qubits` = max of `active_qubits` over time. This is the score's qubit
  factor.

The trick: between `free(q)` and `reacquire(q)`, the slot `q` occupied is
available to the wide region. Net peak = max over time, so freeing across the
widest stretch is what matters.

## Recipe

1. **Sync first.** `cd /home/ubuntu/ben/ecdsafail-challenge && git fetch origin`
   (or `ecdsafail sync`). Optimize the live frontier, not a stale base.

2. **Find the peak binder(s).** Build with tracing:
   ```
   TRACE_PEAK=1 cargo run --release --quiet --bin build_circuit 2>&1 | grep -i peak
   ```
   (`peak_log` records every phase within 10 of peak; `peak_phase` names the
   binder; `TRACE_EACH_PEAK=1` logs each new peak as it's set.) On a mature base
   the peak is a **multi-binder floor** — several phases all sit at the same N
   (e.g. apply_chunk_add/sub_ripple, double_y/halve_y, round84
   fold/unfold/square). A single-phase cut then does NOTHING; you must lower the
   shared floor.

3. **Lower the shared floor = free a PERSISTENT ancilla, or free an idle ancilla
   in EVERY co-peak phase.** Look for an ancilla (usually a *control* or an
   intermediate) that is **dead across part of the wide region**:
   - In a ripple/carry adder, controls for high bit positions are unused across
     the long pure-propagation tail (their control index < the tail start).
   - An intermediate that is only read in the active region, not the tail.

4. **Free it before the wide region, recompute it after — value/phase-EXACT.**
   - `b.free(q)` once `q` is uncomputed / dead, BEFORE the wide stretch.
   - Do the wide stretch (it reuses the slot → peak −1).
   - `b.reacquire(q)` then **re-derive** `q` from still-live registers using cheap
     CX/CCX. Aim for **0 net Toffoli** (CX-only re-derivation, or a single CCX
     that is itself uncomputed). The arithmetic and truncation MUST be byte-
     identical to the non-freed path — you are only tightening ancilla lifetime,
     not changing the function.
   - **Recompute order:** derive base controls first, then dependents.

5. **Gate behind an env flag, default OFF.** Add an `fn xxx_enabled()` reading an
   env var (`== Some("1")`), branch on it, and have the default path stay BYTE-
   IDENTICAL to before. Bake the flag ON via `set_default_env(...)` in
   `mod.rs` only once verified. This makes the lever safe to ship incrementally
   and easy to bisect.

6. **Verify value/phase-exactness.**
   ```
   cargo run --release --quiet --bin build_circuit
   cargo run --release --quiet --bin eval_circuit   # must be qubits = N-1, 0/0/0 gate
   TRACE_PEAK=1 ... build_circuit | grep -i peak    # confirm the floor moved to N-1
   ```
   A value-exact lifetime change does NOT alter the op stream's *function*, so the
   baked `DIALOG_TAIL_NONCE` should still pass if and only if the op stream bytes
   are unchanged on the tested inputs. If the change reorders/edits ops, the nonce
   goes stale (expected) → re-find a clean nonce with the GPU search (see the
   ecdsafail project memory / `agent/gpu-nonce`). If `eval_circuit` shows >0
   classical mismatches that weren't there before, the recompute is NOT exact —
   fix the re-derivation, do not ship.

## Canonical worked example (the 1220→1218 win)

File: `src/point_add/arith/const_arith.rs`, the **freed-tail fold** lever.

The secp256k1 fused fold (`double_y`/`halve_y`) ripple normally keeps all 8 fold
ancillae (`e, d, h, xed, eord, n10` + two overflow holders) live across the wide
high tail — which is the double_y/halve_y high-water (`floor + 8 + 34 + W`).

- `DIALOG_GCD_FOLD_FREED_TAIL=1` (`fold_freed_tail_enabled`): the 4 controls
  *derived from e,d* (`h=e&d, xed=e^d, eord=e|d, n10=¬e&d`) are dead in the tail
  (their fold positions all sit at index ≤ `hi_delta=33`). Free them before the
  tail, recompute (free CX from `e,d` + one AND for `h`) only for the carry-
  uncompute pass → tail high-water `+8 → +4` → fold floor −4, value/phase-exact.
- `DIALOG_GCD_FOLD_FREED_TAIL_ED=1` (`fold_freed_tail_ed_enabled`): ALSO free the
  two BASE controls `e,d` across the tail — they're recomputable from the live
  overflow lanes via `d = ovf1 & s2`, `e = ovf1 ^ d ^ ovf2` (same relation in
  forward double_y and inverse halve_y). High-water `+4 → +2` → fold floor −2 more
  → **1220 → 1218** (at `W=DIALOG_GCD_FOLD_CARRY_TRUNC_W=19`). Cost = a handful of
  CX + 1 CCX/call to re-derive `d`.

The recompute sequence (see `const_arith.rs` ~lines 1086–1108): after the tail,
`reacquire(d)` and re-derive d FIRST, then `reacquire(e)` and re-derive e, then
`reacquire(h,xed,eord,n10)` and re-derive each from the now-live e,d. Both flags
default OFF ⇒ byte-identical base.

# Lever B — range-bound a register to drop a guard qubit

## Core insight

Many registers carry a top **guard bit** (a sign or overflow holder) that exists
ONLY because some *intermediate* value could transiently leave the intended range
(go negative, or exceed `2^k`). If you can prove the running value stays in
`[0, 2^k)` at every step, that guard bit is **always |0⟩** and can be deleted —
the register shrinks by 1, and every phase that holds it live drops by 1. This is
value-exact: same final value, one fewer qubit.

The usual way to win the proof is **operation reordering**: a sequence of
add/subtract terms whose partial sums dip negative (forcing a sign bit) can often
be reordered so the partial sums never dip — e.g. apply all the *additive* terms
before the *subtractive* ones. The final result is identical (addition commutes);
only the transient range — and thus the needed bit-width — changes.

## Recipe

1. Find a register with a top bit that is a sign/overflow guard, not a data bit.
   Tell-tales: `alloc_qubits(k+1)` where only `[..k]` is ever read as data; a
   comment calling the top bit a sign/overflow/carry guard; the top bit fed only
   into a conditional correction.
2. Enumerate the operations that build the value and their term signs. Check
   whether a reorder (adds-first, or grouping) keeps every partial sum in
   `[0, 2^k)`. Prove it for the worst-case operand (here: secp256k1 constants,
   `p = 2^256-2^32-977`), not just typical inputs.
3. Reorder the ops AND shrink the alloc to `k`. Drop the guard bit and any logic
   that only maintained it.
4. Verify value-exactness: `eval_circuit` must stay 0/0/0 (the reorder must not
   change the computed function on ANY input, including the hard ones) and
   `TRACE_PEAK` must show the floor moved down by 1.

## Worked example (our 1221→1220 idea)

File: `src/point_add/arith/multiply.rs`, `round84_fold_hi_into_lo_aggregate`. The
fold/unfold quotient was `alloc_qubits(34)` with Solinas terms ordered
`[(0,true),(4,false),(5,false),(10,true),(32,true)]` — the early subtractive
terms `(4,false),(5,false)` make the running quotient dip negative, so bit 33 is a
**sign guard**. Reordering **adds-first** to
`[(0,true),(10,true),(32,true),(4,false),(5,false)]` keeps the running quotient in
`[0, 2^33)`, so bit 33 is provably |0⟩ → `alloc_qubits(33)`. Value-exact, peak
1221→1220.

**Caveat / status:** on the CURRENT frontier this exact cut is SUBSUMED — 9191f81
already borrows `quotient[33]` as scratch
(`ROUND84_CORRECTION_WRAP_BORROW_QUOTIENT_TOP`), so re-doing the 34→33 shrink nets
0 qubits (and panics if applied naively — the borrow indexes `quotient[33]`). The
*method* (range-bound a guard bit away) is still reusable on OTHER registers; just
don't re-apply it to this specific quotient on a base that already borrows the top.

# Lever C — peak-bounded segmentation + land-exactly + co-descend

## Core insight

A wide operation (here: the round84 Solinas **square**) can be **segmented**
(row-windowed) so its peak is set by a tunable segment width, not by the full
operand width. This turns the square into a *bounded* binder you can dial. The
structural play (Teddy Pender's 1226q route, commit `08c5068`):
1. **Make ONE phase the single global binder** — the peak-bounded square — and
   restructure everything else (apply, GCD transient peaks) to sit *underneath*
   it.
2. **Co-descend every other co-binder below it** — e.g. `apply` ripple via
   `DIALOG_GCD_APPLY_CHUNKED_F_BLOCKS` (more, smaller chunks → lower apply peak).
3. **Tighten the bound so the peak lands EXACTLY at the binder, no waste** — e.g.
   `SQUARE_ROW_MAX_SEG` (199→194→193→191 across commits). Each notch that the
   binder can absorb drops the global peak 1-for-1, until the next wall.

So the recipe isn't just "free a qubit" — it's "identify the global binder, push
all co-binders strictly below it, then shrink the binder's own bound to the wall."
`TRACE_PEAK` shows whether you have ONE binder (good — tighten it) or a cluster of
co-binders (must co-descend ALL of them first).

## Knobs (round84 square / apply)

- `SQUARE_ROW_MAX_SEG=<n>` — segment width of the peak-bounded square; the primary
  peak dial once the square is the binder. Lower = lower peak (until the segment
  is too small to stay value-correct → re-hunt territory). Frontier trail:
  199→194→193→191→188 (`420e0c2`) → **186 (`461a4a3`)**. Try 184 next.
- `DIALOG_GCD_APPLY_CHUNKED_F_BLOCKS=<n>` — chunk count for the apply ripple;
  raise to push the apply peak below the square binder. Frontier trail: 11→12
  (`420e0c2`) → **16 (`461a4a3`)**. Larger values give finer chunks (lower apply
  peak), but too large hits diminishing returns or a new co-binder wall.
- `ROUND84_QPROD_VENT_PAD=1` — pads the round84 quotient×c product so it vents /
  round-trips below peak (used in the 1220→1218 stack).

**Current frontier values (461a4a3):** `SQUARE_ROW_MAX_SEG=186`,
`DIALOG_GCD_APPLY_CHUNKED_F_BLOCKS=16`. Always run `TRACE_PEAK=1 build_circuit`
first — if multiple co-binders are tied, decrementing only one knob doesn't drop
the global peak.

# Lever D — keep-alive instead of uncompute-recompute (the dual of Lever A)

## Core insight

Lever A frees an idle ancilla and recomputes it later. But recompute is NOT always
the peak-cheapest choice: if the uncompute+recompute round-trip itself **transiently
allocates a WIDER intermediate** than just holding the value, the round-trip is what
spikes the peak. In that case do the OPPOSITE — **keep the wide intermediate LIVE**
across the middle operation and skip the uncompute/recompute entirely.

Commit `997dbd6` (1221→1220): `ROUND84_KEEP_QUOTIENT_PRODUCT=1` keeps the 66-bit
quotient×c product live across the middle subtraction, skipping the fold-uncompute +
unfold-recompute. Because the recompute would have re-expanded the product to full
width (above peak), holding it is −1 peak. Value-exact.

**So Lever A and Lever D are duals — pick by comparing two transient widths:**
- recompute's peak transient < holding's footprint → **free + recompute** (Lever A).
- holding's footprint < recompute's peak transient → **keep alive** (Lever D).

`TRACE_PEAK` on both variants tells you which wins; they're both value-exact, so
flip the env flag and measure.

# Lever E — buy peak-relief more cheaply (top-clean carry split)

Peak relief is often bought by *streaming* high bits through controlled suffix
adders so they don't all live at once — but that streaming costs Toffoli. The same
live-scratch reduction can be expressed as a **top-clean carry split**: keep only
~2 streamed suffix bits and replace the removed stream bits with coherent
top-clean MAJ/UMA carry positions in the materialized prefix. Live scratch demand
stays flat (peak unchanged) while emitted Toffoli drops (`2a87f33`). So when a
phase holds peak down via a stream-suffix map, check whether a top-clean
prefix/all-to-2 suffix split holds the SAME peak for fewer gates — a peak-neutral
Toffoli win that also frees budget to spend elsewhere (see the carry-relief
exchange in `ecdsafail-toffoli-reduction`).

# Lever F — in-place decode to free a persistent block (the 1218→1215 win)

A register block can be kept alive in a **compressed** form and **decoded in place**
only for the phase that needs it, freeing the persistent full-width block exactly
across the peak. Commit `420e0c2` (1218→1215): `DIALOG_GCD_K2_APPLY_INPLACE_RAW_BLOCK=1`
— "decode the current K2 pair block **in place** from its 5 compressed cells + one
zero lane, freeing the persistent 6-lane raw block during the apply (clean scratch
allocated only around the chunked add/sub)." That frees a qubit that was sitting at
peak → −1 (combined with `SQUARE_ROW_MAX_SEG` 191→188 and
`DIALOG_GCD_APPLY_CHUNKED_F_BLOCKS` 11→12, net −3 → 1215).

This is the persistent-qubit version of Lever A: instead of holding a full block
live across the whole circuit, store it compressed (fewer lanes) and reconstruct
the wide form transiently, in place, only where used — so the wide block never
counts at peak. Look for any persistent multi-lane block (raw GCD blocks, K2
pairs, materialized operands) that could be carried compressed and decoded in
place at its single use site. Pairs naturally with Lever C (tighten the bound /
co-descend) to convert the freed lane into an actual peak drop.

# Lever G — borrowed-carry fusion (in-place scratch reuse, value-exact)

## Core insight

A truncated-carry `cadd`/`csub` normally **allocates a fresh carry/borrow vector**
(`b.alloc_qubits(last+1)`) just for the ripple, then frees it. If an adjacent
`inner_scratch` register is live and has sufficient width, the carry vector can
instead **borrow from that scratch** — no fresh allocation needed, peak drops by the
borrowed length (up to `last+1`). After the operation, only the *owned* (truly fresh)
suffix is freed; the borrowed prefix was never yours to free.

The mechanism (`const_arith.rs`, commit `461a4a3`):
```rust
// helper: split carry allocation into borrowed prefix + owned suffix
fn borrowed_const_fold_carries(b, need, borrowed) -> (full_vec, owned_vec)

// callers: cadd_nbit_const_direct_trunc_fast_borrowed_carries(…, borrowed_carries)
//          csub_nbit_const_direct_trunc_fast_borrowed_carries(…, borrowed_carries)
```
Gate on `DIALOG_GCD_SPECIAL_FOLD_BORROW_CARRIES=1`; the caller passes `inner_scratch`
as `borrowed_carries` when the env flag is set, else `&[]` (old behavior).

## When it fires

- `DIALOG_GCD_SPECIAL_FOLD_BORROW_CARRIES=1` (commit `461a4a3`): the GCD special
  `cadd`/`csub` (truncated const adds inside the special-accumulator path) borrows
  from `inner_scratch` → the carry vector's fresh allocation shrinks → peak −k for
  k borrowed slots.
- **Comparator paths** (commits `98dd2ad`, `bfd3fa6`): `cmp_lt_phase_conditioned_borrowed_carries()`
  recycles existing scratch carries inside the comparator instead of allocating
  fresh ones. The comparator variant also uses HMR replay (see toffoli-reduction
  Lever H), so it simultaneously saves Toffoli AND avoids fresh carry allocation.
  Look for any `cmp_lt` or `cmod_add/sub` that allocates a fresh carry/borrow
  vector — it's a borrowed-carry candidate.
- **Combined peak drop with other Lever C tuning.** `461a4a3` shipped it together
  with `APPLY_CHUNKED_F_BLOCKS` 12→16 + `SQUARE_ROW_MAX_SEG` 188→186, netting an
  overall peak reduction. The borrowed-carry flag alone accounts for part of the drop.

## Reuse recipe

1. Find a `cadd`/`csub` (or similar truncated ripple) that allocates a fresh carry
   vector AND has an adjacent scratch register with at least `w` live qubits.
2. Add a `_borrowed_carries` variant: same logic, but first `min(need, borrowed.len())`
   slots come from `borrowed`; only the tail is freshly allocated.
3. Gate behind an env flag (`== Some("1")`); default path unchanged.
4. Verify: `eval_circuit` must be 0/0/0 (value-exact — borrowed qubits are already
   |0⟩ scratch, so no new hard inputs; no re-hunt IF the op bytes are unchanged).
   Check `TRACE_PEAK` shows the drop.

**Key property:** value-exact if the borrowed qubits are genuinely |0⟩ at that
point. If they're not (intermediate dirty scratch), this is data corruption — verify
carefully. Borrowed qubits are NOT freed at the end; only the `owned` suffix is.

# Where to look next (re-apply the methods to the current frontier binder)

The 1218 floor is shared by several binders. The fold path already freed e,d.
Re-run step 2–4 against the OTHER co-peak phases:
- `apply_chunk_add/sub_ripple` (the dialog-GCD apply) — does its ripple hold
  controls/carries live across a wide tail that could be freed+recomputed?
- `compressed_block_apply_double_y / reverse_halve_y` — same question.
- `round84_inplace_solinas_fold / unfold / square_forward / square_inverse` —
  any ancilla idle across the wide product/segment region?
A qubit that is freeable in EVERY co-peak phase (or a truly persistent one
removed/shared once) drops the whole floor to 1217.

## Discipline / gotchas

- **Value/phase-exact only.** This lever must not change the computed function or
  the truncation. If it does, you've left the "free win" regime and must re-find a
  clean nonce and re-validate 0/0/0.
- **Env-gate, default OFF, byte-identical base.** Always.
- **Free requires |0⟩.** Only free an uncomputed qubit; only reacquire one you
  freed.
- **Rebuild tools after any src edit.** `agent/fast-screen` and `agent/gpu-nonce`
  link the crate — `cargo clean` + rebuild or they give STALE numbers. Then
  `validate_pipeline` must be 32/32.
- **Don't re-do exhausted axes.** The round84 quotient 34→33 cut is SUBSUMED
  (frontier borrows quotient[33]); raw truncation knobs are at the hard-input
  cliff. See `agent/task.md` / `agent/shinka.md` on zan3 for the live exhausted
  list.
