# M1 — GPU phase-sim op-loop validation (decoupled unit test)

Goal: prove a CUDA kernel can run the ecdsafail circuit op stream bit-sliced and
reproduce `eval_circuit`'s **classical + phase** verdict. De-risks the GPU
phase-prefilter (see `gpu_phase_prefilter_plan.md`). **Decoupled design:** a CPU
reference produces golden test vectors (initial state + RNG + verdict); the CUDA
kernel consumes them and must match. This isolates the op-loop port (the real risk)
from FS/EC/keccak/compaction (deferred to M3, reusable from hunt.cu).

Work on zan3: `cd /home/ubuntu/ben/temp/ecdsafail-challenge && export PATH=$HOME/.cargo/bin:$PATH`.
New code under `ecdsafail-agent/gpu-nonce/` (extend that crate — it already has cudarc,
keccak, `build()`). NEVER edit `src/**` or `git push`.

## The simulator model (authoritative, from src/sim.rs)

Bit-sliced classical sim: `qubits: Vec<u64>`, `bits: Vec<u64>` — **bit s of each u64 =
shot s** (64 shots/batch). Single `phase: u64` (bit s = phase parity of shot s).
A circuit is clean iff after running all ops: classical output matches AND `phase==0`
AND freed ancillae are 0.

Per-op semantics (port EXACTLY; `cond` starts = `current_base_condition`, and if
`op.c_condition != NO_BIT` then `cond &= bits[c_condition]`):

```
CCX(13): v = cond & q[c1] & q[c2];           q[t] ^= v
CX (8):  v = cond & q[c1];                    q[t] ^= v
X  (6):                                        q[t] ^= cond
Swap(10): a=q[c1]; b=q[t]; a^=b; b^=cond&a; a^=b; q[c1]=a; q[t]=b
CCZ(14): phase ^= cond & q[t] & q[c1] & q[c2]
CZ (9):  phase ^= cond & q[t] & q[c1]
Z  (7):  phase ^= cond & q[t]
Neg(0):  phase ^= cond
Hmr(12): rng=next8(xof); bits[ct] = (bits[ct]&~cond) ^ (rng&cond);
         phase ^= q[t] & rng & cond;  q[t] &= ~cond
R  (11): rng=next8(xof);  phase ^= q[t] & rng & cond;  q[t] &= ~cond
BitInvert(3): bits[ct] ^= cond
BitStore0(4): bits[ct] &= ~cond
BitStore1(5): bits[ct] |= cond
AppendToRegister(2)/Register(1)/DebugPrint(17): no-op for sim
PushCondition(15): stack.push(current_base_condition); current_base_condition &= bits[c_condition]
PopCondition(16): current_base_condition = stack.pop()
```
NO_QUBIT = NO_BIT = u64::MAX (0xFFFFFFFFFFFFFFFF). `next8(xof)` = read 8 bytes LE = u64,
sequential from the XOF stream. Op binary layout in ops.bin: 16-byte header
(8 "QECCOPS1" + u64 count), then 56 B/op: u32 kind, u32 pad, then u64×6 =
q_control2,q_control1,q_target,c_target,c_condition,r_target. Kinds: Neg0 Register1
Append2 BitInvert3 BitStore0=4 BitStore1=5 X6 Z7 CX8 CZ9 Swap10 R11 Hmr12 CCX13 CCZ14
Push15 Pop16 Debug17.

## Register layout / inputs / checks (from eval_circuit.rs run_tests + circuit.rs analyze_ops)

- `analyze_ops` (src/circuit.rs:348): scan ops; num_qubits = max(q id)+1 (=1170);
  num_bits = max(c id)+1; registers built from `AppendToRegister` ops (push Qubit(q_target)
  or Bit(c_target) onto register r_target). Expect 4 registers, each 256 wide:
  reg0=target_x (qubits), reg1=target_y (qubits), reg2=offset_x (bits), reg3=offset_y (bits).
- FS seed: `Shake256("quantum_ecc-fiat-shamir-v2" || (n_ops:u64 LE) || for each op:
  kind:u8, q_control2:u64LE, q_control1, q_target, c_target, c_condition, r_target)`.
- Inputs: for 9024 iters: read k1=xof.read(32),k2=xof.read(32) (LE U256); t=G·k1, o=G·k2
  (secp256k1, mul); skip if t.x==o.x or t∞ or o∞ (skip = continue, bytes already consumed);
  e=t+o. Compact survivors → n. Then the SAME xof feeds measured gates.
- Run in batches of 64 over the n survivors. Per batch: clear state; set_register(reg0,t.x),
  (reg1,t.y),(reg2,o.x),(reg3,o.y) per shot; apply all ops; check get_register(reg0)==e.x &&
  (reg1)==e.y (classical), phase & cond_mask ==0 (phase), then zero the 4 regs' qubits and
  assert no other qubit nonzero (ancilla). cond_mask = (1<<bs)-1 (bs=64 full).
- DIALOG_TAIL_NONCE=N appends 96 ops (`X;X` on qubit 1 if bit set else qubit 0) — identity
  on state, only perturbs the FS seed. Set it via env before build().

## Plan

1. **CPU reference + dumper** (`bin/phase_ref.rs` in gpu-nonce crate, or reuse Simulator):
   Given DIALOG_TAIL_NONCE=N: build() → ops; analyze_ops; replicate FS+inputs; for batch 0
   (first 64 survivors) set initial state; **dump** to files: (a) ops (compact: kind u8 +
   q_control2/1/target as i32 (-1=none) + c_target/c_condition i32), (b) initial qubits[]
   and bits[] (u64 each), (c) the measured-gate RNG buffer = pre-squeeze the xof continuation
   for (#Hmr+#R)*8 bytes IN ORDER, (d) golden: final reg0/reg1 per shot + final phase + per-shot
   ancilla-clean flag. Produce golden by running the op-loop **in Rust** (same arms) — and
   sanity-check the whole-run classical/phase counts match `eval_circuit` on a baked nonce.
2. **CUDA kernel** `phase_sim.cu`: 1 block; each thread = 1 shot (or bit-slice 64 shots/u64 —
   start with 1 shot/thread scalar for simplicity, optimize later). Load compact ops to global,
   initial state + RNG buffer. Run the op-loop (arms above), maintain phase + condition stack
   (small, in local). Output final reg0/reg1 + phase + ancilla flag.
3. **Harness** `bin/validate_phase.rs`: load dumps, launch kernel via cudarc (clone main.rs's
   NVRTC compile + launch pattern), compare kernel output to golden. PASS iff identical.
4. **Test set:** baked clean nonce 3480010331559 (→ all clean) + 2-3 phase-fail nonces
   (e.g. build with DIALOG_GCD_TOBITVECTOR_CSWAP_BODY_TRIM=1 which gives 35 phase batches —
   find a batch index that fails and dump THAT batch).

## Gate (report this)
M1 PASS = CUDA kernel reproduces the Rust-reference golden EXACTLY (classical + phase +
ancilla) on the clean nonce's batch 0 AND on at least one phase-failing batch. Report:
PASS/FAIL, which batches tested, any semantic mismatches found+fixed, and rough kernel
runtime per batch (for the M2 throughput estimate). Do NOT optimize for speed in M1 —
correctness only.
```
