# gpu-nonce — GPU clean-nonce search for the secp256k1 point-add route

GPU tooling for the Fiat-Shamir clean-nonce hunt. It implements **both stages of the
two-stage "Lever B" pipeline — the current hunt method**. For the method itself and how to
drive it see [LEVER_B_EXPLAINED.md](./LEVER_B_EXPLAINED.md) and
[../AGENT_RUNBOOK.md](../AGENT_RUNBOOK.md) §Phase 6. Lives under `agent/` (local-only, GPL field
code never distributed).

## Two-stage pipeline (driven by `leverB_hunt.sh`)
- **Stage-1 — `gpu-nonce` (`src/hunt.cu`)**: analytical GCD-replay prefilter, ~8.6k–23.5k
  nonce/s/GPU, **no op-loop**. Rejects ~99.9% of nonces (classically-hard) *conservatively* —
  0 false-hard (never drops a truly-clean nonce); false-clean is harmless (stage-2 re-checks).
- **Stage-2 — `verify_dual` (`src/bin/verify_dual.rs` + `hunt_phase.cu`)**: exact GPU op-loop
  over the full ~10.3M-op circuit, **classical + phase**, per-batch early-exit, run on stage-1
  survivors only. Emits `DUAL_CLEAN_CANDIDATE`. ~169 nonce/s/GPU on the raw stream (8 GPU ≈ 1,355) —
  but in the pipeline it only sees the ~0.1% that pass stage-1, so the system scans at stage-1 rate.
- `circuit_prep` dumps the op-stream to `/tmp/phase_circuit` for `verify_dual` to replay.
- The single winner is re-certified by the official CPU `eval_circuit` (0/0/0) before bake.

> ⚠️ **GCD-axis knob hunts** (e.g. `DIALOG_GCD_COMPARE_BITS=N`): the env must be exported into
> **circuit_prep + stage-1 + verify_dual** or the whole run silently uses the DEFAULT circuit.
> Tell = dump `n_ops`. See LEVER_B_EXPLAINED.md / AGENT_RUNBOOK §Phase 6.

## Build / run
    export PATH=/usr/local/cuda/bin:$HOME/.cargo/bin:$PATH
    export LD_LIBRARY_PATH=/usr/local/cuda-12.8/targets/x86_64-linux/lib:$LD_LIBRARY_PATH
    # validate everything (kG + GCD + integrated stage-1) — bit-exact vs CPU:
    cargo run --release --bin validate_pipeline
    # run a hunt (single host, 8 GPU) — the normal entry point:
    NGPU=8 START=<n0> CHUNK=20000000 CHUNKS=<n> bash leverB_hunt.sh
    # fleet = run the above per host on disjoint START ranges (manual; leverB is single-host).

## Component validation (bit-exact vs CPU)
- field arith: lifted VERBATIM from VanitySearch GPUMath.h (**GPL-3.0**, (c) 2019 J-L Pons),
  LOCAL-ONLY (never distributed/pushed) so copyleft is not triggered. In `src/field.cuh`.
- point ops + scalar mult: own Jacobian code (`src/points.cu`). **16/16 k·G match CPU curve.mul.**
- GCD filter: port of `dialog_gcd_classical_filter.rs` (`src/gcd.cu`). **20000/20000 match CPU.**
- Keccak/SHAKE256: own host (`src/keccak.rs`) + device (`src/keccak.cu`), matches the `sha3` crate.
- Integrated stage-1: **32/32 per-nonce hard-count match CPU** (`src/bin/validate_pipeline.rs`).
- **After ANY `src/point_add` change**: `cargo clean -p quantum_ecc && cargo build --release`
  (`include_str!` embeds `gcd.cu`; a stale binary silently mis-filters). Re-run `validate_pipeline`.

## Filter scope (stage-1 only models the GCD truncation)
`gcd.cu` models **only** the GCD-truncation hard inputs, not the full circuit (apply/fold/round84
add more). So a stage-1 GCD-clean nonce can still fail classical/phase in the full op-loop — which
is exactly why stage-2 `verify_dual` re-checks every survivor. (Measured on the cb45 knob: of
stage-1 GCD-clean survivors only ~18% are classical-clean, and the phase wall is far thicker still.)
