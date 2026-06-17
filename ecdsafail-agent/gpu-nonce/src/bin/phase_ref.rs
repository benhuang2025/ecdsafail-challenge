// CPU reference + dumper for the GPU phase-sim M1 unit test.
//
// Given DIALOG_TAIL_NONCE=N (and optionally PHASE_BATCH=b) in env:
//   - build() -> ops
//   - analyze_ops -> num_qubits, num_bits, 4 registers
//   - replicate the Fiat-Shamir seed (Shake256 over the op stream)
//   - derive 9024-shot inputs (k1,k2 -> t,o,e), with skip/compaction
//   - run the FULL op-loop in Rust to produce per-batch classical / phase / ancilla
//     verdicts, and confirm the totals (printed) match eval_circuit's official counts
//   - for the chosen batch (default 0): dump ops(compact), initial qubits[]/bits[],
//     the measured-gate RNG buffer, the per-shot register-qubit index lists,
//     and golden = per-shot final reg0/reg1 + final phase + per-shot ancilla-clean flag
//
// Dump dir: /tmp/phase_m1/
//
// NOTE: input-derivation stays on CPU; the GPU kernel only consumes the dumps and
// re-runs the op-loop. This isolates the op-loop port (the M1 risk).

use alloy_primitives::U256;
use quantum_ecc::circuit::{analyze_ops, OperationType, Op, QubitOrBit, NO_BIT, NO_QUBIT};
use quantum_ecc::point_add::build;
use quantum_ecc::weierstrass_elliptic_curve::WeierstrassEllipticCurve;
use sha3::{
    digest::{ExtendableOutput, Update, XofReader},
    Shake256,
};
use std::fs;
use std::io::Write;

const NUM_TESTS: usize = 9024;
const BATCH: usize = 64;
const DUMP_DIR: &str = "/tmp/phase_m1";

fn secp256k1() -> WeierstrassEllipticCurve {
    WeierstrassEllipticCurve {
        modulus: U256::from_str_radix(
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F",
            16,
        )
        .unwrap(),
        a: U256::from(0),
        b: U256::from(7),
        gx: U256::from_str_radix(
            "79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798",
            16,
        )
        .unwrap(),
        gy: U256::from_str_radix(
            "483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8",
            16,
        )
        .unwrap(),
        order: U256::from_str_radix(
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141",
            16,
        )
        .unwrap(),
    }
}

fn fiat_shamir_seed(ops: &[Op]) -> sha3::Shake256Reader {
    let mut hasher = Shake256::default();
    hasher.update(b"quantum_ecc-fiat-shamir-v2");
    hasher.update(&(ops.len() as u64).to_le_bytes());
    for op in ops {
        hasher.update(&[op.kind as u8]);
        hasher.update(&op.q_control2.0.to_le_bytes());
        hasher.update(&op.q_control1.0.to_le_bytes());
        hasher.update(&op.q_target.0.to_le_bytes());
        hasher.update(&op.c_target.0.to_le_bytes());
        hasher.update(&op.c_condition.0.to_le_bytes());
        hasher.update(&op.r_target.0.to_le_bytes());
    }
    hasher.finalize_xof()
}

// Compact op for the kernel: kind:u8 + 5 i32 operands (-1 = NONE).
// Fields: kind, q_control2, q_control1, q_target, c_target, c_condition.
// (r_target / Append handled on CPU; kernel ignores Append/Register/Debug.)
#[repr(C)]
#[derive(Clone, Copy)]
struct CompactOp {
    kind: u8,
    _pad: [u8; 3],
    qc2: i32,
    qc1: i32,
    qt: i32,
    ct: i32,
    cc: i32,
}

fn idx_q(id: quantum_ecc::circuit::QubitId) -> i32 {
    if id == NO_QUBIT { -1 } else { id.0 as i32 }
}
fn idx_b(id: quantum_ecc::circuit::BitId) -> i32 {
    if id == NO_BIT { -1 } else { id.0 as i32 }
}

fn main() {
    let chosen_batch: usize = std::env::var("PHASE_BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    eprintln!("build() with DIALOG_TAIL_NONCE={:?} GCD_TRIM={:?}",
        std::env::var("DIALOG_TAIL_NONCE").ok(),
        std::env::var("DIALOG_GCD_TOBITVECTOR_CSWAP_BODY_TRIM").ok());
    let ops = build();
    eprintln!("n_ops={}", ops.len());

    let (num_qubits, num_bits, num_regs, regs) = analyze_ops(ops.iter());
    eprintln!("num_qubits={} num_bits={} num_regs={}", num_qubits, num_bits, num_regs);
    assert_eq!(regs.len(), 4, "expected 4 registers");
    for r in &regs { assert_eq!(r.len(), 256); }

    let num_qubits = num_qubits as usize;
    let num_bits = num_bits as usize;

    // --- inputs ---
    let curve = secp256k1();
    let mut xof = fiat_shamir_seed(&ops);
    let mut targets = Vec::with_capacity(NUM_TESTS);
    let mut offsets = Vec::with_capacity(NUM_TESTS);
    let mut expected = Vec::with_capacity(NUM_TESTS);
    for _ in 0..NUM_TESTS {
        let mut rb = [[0u8; 32]; 2];
        xof.read(&mut rb[0]);
        xof.read(&mut rb[1]);
        let k1 = U256::from_le_bytes(rb[0]);
        let k2 = U256::from_le_bytes(rb[1]);
        let t = curve.mul(curve.gx, curve.gy, k1);
        let o = curve.mul(curve.gx, curve.gy, k2);
        if t.0 == o.0 { continue; }
        if t.0.is_zero() && t.1.is_zero() { continue; }
        if o.0.is_zero() && o.1.is_zero() { continue; }
        let e = curve.add(t.0, t.1, o.0, o.1);
        targets.push(t);
        offsets.push(o);
        expected.push(e);
    }
    let n = targets.len();
    let num_batches = (n + BATCH - 1) / BATCH;
    eprintln!("survivors n={} num_batches={}", n, num_batches);

    // helper closures replicating set/get register on local arrays
    let set_reg = |qubits: &mut [u64], bits: &mut [u64], reg: &[QubitOrBit], val: U256, shot: usize| {
        for (i, item) in reg.iter().enumerate() {
            let bv = val.bit(i);
            match item {
                QubitOrBit::Qubit(id) => {
                    if bv { qubits[id.0 as usize] |= 1 << shot; }
                    else  { qubits[id.0 as usize] &= !(1 << shot); }
                }
                QubitOrBit::Bit(id) => {
                    if bv { bits[id.0 as usize] |= 1 << shot; }
                    else  { bits[id.0 as usize] &= !(1 << shot); }
                }
            }
        }
    };
    let get_reg = |qubits: &[u64], bits: &[u64], reg: &[QubitOrBit], shot: usize| -> U256 {
        let mut v = U256::ZERO;
        for (i, item) in reg.iter().enumerate() {
            let bv = match item {
                QubitOrBit::Qubit(id) => (qubits[id.0 as usize] >> shot) & 1,
                QubitOrBit::Bit(id) => (bits[id.0 as usize] >> shot) & 1,
            };
            v.set_bit(i, bv != 0);
        }
        v
    };

    // The op-loop, ported from sim.rs apply_iter. `next8` reads from `xof` (the
    // SAME reader that produced inputs — measured gates continue the stream).
    // For batch>0, eval_circuit reuses one Simulator across all batches, so the
    // xof keeps advancing batch after batch. We must replay batches 0..chosen
    // to keep RNG aligned, but we only DUMP the chosen batch.
    //
    // To dump the RNG buffer for the chosen batch we record every next8 value
    // consumed during that batch (in order).
    let mut qubits = vec![0u64; num_qubits];
    let mut bits = vec![0u64; num_bits];

    // golden accumulators (for chosen batch)
    let mut dump_initial_q = vec![0u64; num_qubits];
    let mut dump_initial_b = vec![0u64; num_bits];
    let mut dump_rng: Vec<u64> = Vec::new();
    let mut golden_reg0 = [U256::ZERO; 64];
    let mut golden_reg1 = [U256::ZERO; 64];
    let mut golden_phase: u64 = 0;
    let mut golden_ancilla_clean = [false; 64]; // per shot: true if clean

    // full-run verdict counters (to compare vs eval_circuit)
    let mut classical_failures = 0usize;
    let mut phase_garbage_batches = 0usize;
    let mut ancilla_garbage_batches = 0usize;

    for batch in 0..num_batches {
        let bs = BATCH.min(n - batch * BATCH);
        let cond_mask: u64 = if bs == 64 { u64::MAX } else { (1u64 << bs) - 1 };
        let is_chosen = batch == chosen_batch;

        // clear
        for e in qubits.iter_mut() { *e = 0; }
        for e in bits.iter_mut() { *e = 0; }
        let mut phase: u64 = 0;

        for shot in 0..bs {
            let i = batch * BATCH + shot;
            set_reg(&mut qubits, &mut bits, &regs[0], targets[i].0, shot);
            set_reg(&mut qubits, &mut bits, &regs[1], targets[i].1, shot);
            set_reg(&mut qubits, &mut bits, &regs[2], offsets[i].0, shot);
            set_reg(&mut qubits, &mut bits, &regs[3], offsets[i].1, shot);
        }

        if is_chosen {
            dump_initial_q.copy_from_slice(&qubits);
            dump_initial_b.copy_from_slice(&bits);
        }

        // apply op-loop
        let mut cond_stack: Vec<u64> = Vec::new();
        let mut base_cond: u64 = u64::MAX;
        for op in &ops {
            let mut cond = base_cond;
            if op.c_condition != NO_BIT {
                cond &= bits[op.c_condition.0 as usize];
            }
            match op.kind {
                OperationType::CCX => {
                    let v = cond & qubits[op.q_control1.0 as usize] & qubits[op.q_control2.0 as usize];
                    qubits[op.q_target.0 as usize] ^= v;
                }
                OperationType::CX => {
                    let v = cond & qubits[op.q_control1.0 as usize];
                    qubits[op.q_target.0 as usize] ^= v;
                }
                OperationType::Swap => {
                    let mut a = qubits[op.q_control1.0 as usize];
                    let mut b = qubits[op.q_target.0 as usize];
                    a ^= b;
                    b ^= cond & a;
                    a ^= b;
                    qubits[op.q_control1.0 as usize] = a;
                    qubits[op.q_target.0 as usize] = b;
                }
                OperationType::X => {
                    qubits[op.q_target.0 as usize] ^= cond;
                }
                OperationType::CCZ => {
                    let v = cond & qubits[op.q_target.0 as usize]
                        & qubits[op.q_control1.0 as usize] & qubits[op.q_control2.0 as usize];
                    phase ^= v;
                }
                OperationType::CZ => {
                    let v = cond & qubits[op.q_target.0 as usize] & qubits[op.q_control1.0 as usize];
                    phase ^= v;
                }
                OperationType::Z => {
                    phase ^= cond & qubits[op.q_target.0 as usize];
                }
                OperationType::Neg => {
                    phase ^= cond;
                }
                OperationType::Hmr => {
                    let mut buf = [0u8; 8];
                    xof.read(&mut buf);
                    let rng = u64::from_le_bytes(buf);
                    if is_chosen { dump_rng.push(rng); }
                    let ct = op.c_target.0 as usize;
                    bits[ct] &= !cond;
                    bits[ct] ^= rng & cond;
                    phase ^= qubits[op.q_target.0 as usize] & rng & cond;
                    qubits[op.q_target.0 as usize] &= !cond;
                }
                OperationType::R => {
                    let mut buf = [0u8; 8];
                    xof.read(&mut buf);
                    let rng = u64::from_le_bytes(buf);
                    if is_chosen { dump_rng.push(rng); }
                    phase ^= qubits[op.q_target.0 as usize] & rng & cond;
                    qubits[op.q_target.0 as usize] &= !cond;
                }
                OperationType::BitInvert => {
                    bits[op.c_target.0 as usize] ^= cond;
                }
                OperationType::BitStore0 => {
                    bits[op.c_target.0 as usize] &= !cond;
                }
                OperationType::BitStore1 => {
                    bits[op.c_target.0 as usize] |= cond;
                }
                OperationType::AppendToRegister
                | OperationType::Register
                | OperationType::DebugPrint => {}
                OperationType::PushCondition => {
                    cond_stack.push(base_cond);
                    base_cond &= bits[op.c_condition.0 as usize];
                }
                OperationType::PopCondition => {
                    if let Some(v) = cond_stack.pop() { base_cond = v; }
                }
            }
        }

        // classical check
        for shot in 0..bs {
            let i = batch * BATCH + shot;
            let gx = get_reg(&qubits, &bits, &regs[0], shot);
            let gy = get_reg(&qubits, &bits, &regs[1], shot);
            if gx != expected[i].0 || gy != expected[i].1 {
                classical_failures += 1;
            }
            if is_chosen {
                golden_reg0[shot] = gx;
                golden_reg1[shot] = gy;
            }
        }

        // phase check
        let pmask = phase & cond_mask;
        if pmask != 0 { phase_garbage_batches += 1; }
        if is_chosen { golden_phase = phase; }

        // optional per-batch failure log (to locate a failing batch index)
        if std::env::var("PHASE_LOG_FAILS").is_ok() {
            let mut cfail = 0;
            for shot in 0..bs {
                let i = batch * BATCH + shot;
                let gx = get_reg(&qubits, &bits, &regs[0], shot);
                let gy = get_reg(&qubits, &bits, &regs[1], shot);
                if gx != expected[i].0 || gy != expected[i].1 { cfail += 1; }
            }
            if cfail != 0 || pmask != 0 {
                eprintln!("BATCH {batch}: classical_fail={cfail} phase={:#018x}", pmask);
            }
        }

        // ancilla check: zero the 4 regs' qubits, then any nonzero qubit (under cond_mask) is garbage
        let mut qcheck = qubits.clone();
        for register in &regs {
            for qb in register {
                if let QubitOrBit::Qubit(q) = qb {
                    qcheck[q.0 as usize] = 0;
                }
            }
        }
        let mut batch_dirty_mask: u64 = 0;
        for q in 0..num_qubits {
            batch_dirty_mask |= qcheck[q] & cond_mask;
        }
        if batch_dirty_mask != 0 { ancilla_garbage_batches += 1; }
        if is_chosen {
            // per-shot ancilla-clean: shot s clean iff bit s of dirty mask is 0
            for shot in 0..bs {
                golden_ancilla_clean[shot] = ((batch_dirty_mask >> shot) & 1) == 0;
            }
            for shot in bs..64 { golden_ancilla_clean[shot] = true; }
        }
    }

    eprintln!(
        "FULL-RUN VERDICT: classical_failures={} phase_garbage_batches={} ancilla_garbage_batches={}",
        classical_failures, phase_garbage_batches, ancilla_garbage_batches
    );

    // ---- dumps ----
    fs::create_dir_all(DUMP_DIR).unwrap();

    // compact ops
    let mut compact: Vec<CompactOp> = Vec::with_capacity(ops.len());
    for op in &ops {
        compact.push(CompactOp {
            kind: op.kind as u8,
            _pad: [0; 3],
            qc2: idx_q(op.q_control2),
            qc1: idx_q(op.q_control1),
            qt: idx_q(op.q_target),
            ct: idx_b(op.c_target),
            cc: idx_b(op.c_condition),
        });
    }
    let mut f = fs::File::create(format!("{DUMP_DIR}/ops.bin")).unwrap();
    f.write_all(&(compact.len() as u64).to_le_bytes()).unwrap();
    for c in &compact {
        f.write_all(&[c.kind]).unwrap();
        f.write_all(&c._pad).unwrap();
        f.write_all(&c.qc2.to_le_bytes()).unwrap();
        f.write_all(&c.qc1.to_le_bytes()).unwrap();
        f.write_all(&c.qt.to_le_bytes()).unwrap();
        f.write_all(&c.ct.to_le_bytes()).unwrap();
        f.write_all(&c.cc.to_le_bytes()).unwrap();
    }

    // meta: num_qubits, num_bits, bs(=live shots for chosen batch), n_rng
    let chosen_bs = BATCH.min(n - chosen_batch * BATCH);
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/meta.bin")).unwrap();
        f.write_all(&(num_qubits as u64).to_le_bytes()).unwrap();
        f.write_all(&(num_bits as u64).to_le_bytes()).unwrap();
        f.write_all(&(chosen_bs as u64).to_le_bytes()).unwrap();
        f.write_all(&(dump_rng.len() as u64).to_le_bytes()).unwrap();
        f.write_all(&(chosen_batch as u64).to_le_bytes()).unwrap();
    }

    // initial state
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/init_q.bin")).unwrap();
        for &v in &dump_initial_q { f.write_all(&v.to_le_bytes()).unwrap(); }
        let mut f = fs::File::create(format!("{DUMP_DIR}/init_b.bin")).unwrap();
        for &v in &dump_initial_b { f.write_all(&v.to_le_bytes()).unwrap(); }
    }

    // rng buffer
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/rng.bin")).unwrap();
        for &v in &dump_rng { f.write_all(&v.to_le_bytes()).unwrap(); }
    }

    // register qubit-index lists for reg0 and reg1 (256 each, u32) — for kernel reg readout & ancilla
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/reg_q.bin")).unwrap();
        for r in 0..2usize {
            for qb in &regs[r] {
                if let QubitOrBit::Qubit(q) = qb {
                    f.write_all(&(q.0 as u32).to_le_bytes()).unwrap();
                } else {
                    panic!("reg{r} expected qubits");
                }
            }
        }
    }

    // golden
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/golden.bin")).unwrap();
        // per shot: reg0 (4 u64 LE), reg1 (4 u64 LE), ancilla_clean (u8)
        for shot in 0..64usize {
            for &limb in golden_reg0[shot].as_limbs() { f.write_all(&limb.to_le_bytes()).unwrap(); }
            for &limb in golden_reg1[shot].as_limbs() { f.write_all(&limb.to_le_bytes()).unwrap(); }
            f.write_all(&[golden_ancilla_clean[shot] as u8]).unwrap();
        }
        // final phase (u64)
        f.write_all(&golden_phase.to_le_bytes()).unwrap();
    }

    eprintln!("DUMPED chosen_batch={} bs={} n_rng={} -> {DUMP_DIR}", chosen_batch, chosen_bs, dump_rng.len());
}
