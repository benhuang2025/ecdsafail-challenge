# gcd.cu re-alignment to ecdsafail frontier 84d5b0e — diagnosis notes

Host: zan3 (8x RTX5090). Repo: /home/ubuntu/ben/temp/ecdsafail-challenge @ 84d5b0e.
Work dir: ecdsafail-agent/gpu-nonce/. NEVER edit src/**. NEVER git push.

## Goal
Re-align the analytical GPU prefilter gcd.cu so its per-nonce "classical-clean" set ==
phase_ref (CPU op-loop) clean set EXACTLY. 0 false-clean mandatory; minimize false-hard.
Then two-stage: gcd.cu stage-1 (~8.6k/s) pre-cull -> phase op-loop stage-2 on survivors.

## Ground truth
- phase_ref.rs: build() with DIALOG_TAIL_NONCE=N -> 9024-shot FS island -> full op-loop ->
  `classical_failures` = total per-shot reg0/reg1 mismatches over all 141 batches.
  This is the AUTHORITATIVE per-nonce classical hard count. nonce-clean <=> classical_failures==0.
- Note: build() with DIALOG_TAIL_NONCE=N appends identity X;X tail (96 ops, no RNG) so the
  FS island for nonce N matches hunt.cu's appended-tail convention (report M3 confirmed byte-exact).

## CPU analytical model (what gcd.cu was ported from)
dialog_gcd_classical_filter.rs::check_gcd_factor(factor,cfg):
  build_gcd_transcript(258 steps, truncated_gcd_step_logged) THEN
  check_tail_pair_codec(log) THEN terminal_u==1 else NonConvergence.
Per-nonce: for each of 9024 shots, dx & c factors; nonce hard-count = #shots where dx OR c hard.

## Baked env on 84d5b0e (src/point_add/mod.rs) relevant to transcript+codecs
ACTIVE_ITERATIONS=258 COMPARE_BITS=46 WIDTH_MARGIN=10 WIDTH_SLOPE_X1000=1015
K2=1 ODD_U_LOWBIT_FASTPATH=1 SKIP_ZERO_EDGE_CSHIFT=1
RAW_TOBITVECTOR_MATERIALIZED_SUB=0 (=> body = active-width sub, NO ^1; matches gcd.cu)
RAW_TOBITVECTOR_VARIABLE_WIDTH=1 PA9024_COMPARE_SCHEDULE=1 (margin 0, floor 1)
BODY_CARRY_BAND_TRIMS="0,3,3,3,3,3,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,3,3,3"
BINDER_NOTCH_STEPS=8,9,10 NOTCH_EXTRA=3 NOTCH_MAP=11:1,12:1,13:1
TOBITVECTOR_CSWAP_BODY_TRIM=0  (=> cswap_width = active_width)
CODECS ACTIVE: K5_HEAD11_CODEC=1, K5_TAIL3_TOP32_CODEC=1
  (K5_TAIL3_FIXED_LAST=0; no TAIL6/TAIL7/HEAD_PAIR/ODD_SINGLETON/ODD_TAIL_TRIPLE/TAIL_PAIR*)

## gcd.cu current state (as found)
- Comment says "REALIGNED to 674d0d8" — STALE, not 84d5b0e.
- Implements: terminal u==1 + TAIL3_TOP32 codec. TAIL3_SUP table == CPU table (verified equal).
- MISSING: K5_HEAD11_CODEC (first-5-step pattern membership). This is a hard-reject the CPU
  model applies and gcd.cu omits => gcd.cu UNDER-rejects (false-clean) relative to CPU model.
- aw/cb/bw uploaded from DialogGcdFilterConfig::from_env() in the *binary's* env. validate_pipeline
  sets only DIALOG_TAIL_NONCE=none + ACCEPT_U1_TERMINAL=1; it does NOT replicate the build()
  set_default_env block, so from_env() picks up MAX_GCD_ITERS=402 etc -> WRONG schedule unless
  the binary inherits the baked env. <-- KEY SUSPECT for misalignment.

## Open questions to resolve empirically
Q1: Does CPU check_gcd_factor (with correct baked env) match phase_ref op-loop per-nonce?
    (Is the analytical model even the right target, or does it diverge from the op-loop?)
Q2: What env does the validate/gpu binary actually see? Need build()'s set_default_env to run,
    OR replicate the baked DIALOG_GCD_* values in the harness env, so from_env() schedule is right.
Q3: After fixing env + adding HEAD11 codec to gcd.cu, does gcd-clean set == phase_ref-clean set?
