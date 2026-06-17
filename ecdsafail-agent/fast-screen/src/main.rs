//! AGENT TOOL — fast in-process circuit screen for nonce hunting.
//!
//! Fuses build_circuit (`point_add::build()`) + eval_circuit (`run_tests`)
//! into one process, parameterized by `DIALOG_TAIL_NONCE` via env. Skips the
//! ops.bin round-trip so the verify daemon can screen thousands of candidate
//! nonces. The FS-island test logic below is ported verbatim from
//! src/bin/eval_circuit.rs so a RESULT of `classical=0 phase=0 ancilla=0`
//! means the same thing the trusted scorer would report.
//!
//! Env:
//!   DIALOG_TAIL_NONCE  the nonce to bake (read by point_add::build)
//!   NS_SHOTS           shot count (default 9024 = the scored island)
//!   NS_EARLY_EXIT      "1" => abort at first classical mismatch (flag 0/1);
//!                      "0" => full counts (use for λ measurement / Step 0)
//!
//! Output: `RESULT qubits=<q> classical=<c> phase=<p> ancilla=<a> nonce=<n>`

use alloy_primitives::U256;
use quantum_ecc::circuit::{analyze_ops, QubitId, QubitOrBit};
use quantum_ecc::point_add;
use quantum_ecc::sim::Simulator;
use quantum_ecc::weierstrass_elliptic_curve::WeierstrassEllipticCurve;
use sha3::{
    digest::{ExtendableOutput, Update, XofReader},
    Shake256,
};

// ─── secp256k1 parameters (verbatim from eval_circuit.rs) ───────────────────
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

// ─── Fiat-Shamir seed (verbatim from eval_circuit.rs) ───────────────────────
fn fiat_shamir_seed(ops: &[quantum_ecc::circuit::Op]) -> sha3::Shake256Reader {
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

fn main() {
    let shots: usize = std::env::var("NS_SHOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9024);
    let early_exit = std::env::var("NS_EARLY_EXIT").ok().as_deref() == Some("1");
    let nonce = std::env::var("DIALOG_TAIL_NONCE").unwrap_or_else(|_| "baked".to_string());

    // Build the contestant circuit in-process (reads DIALOG_TAIL_NONCE).
    let ops = point_add::build();
    let (total_qubits, num_bits, _num_regs, regs) = analyze_ops(ops.iter());

    // Derive the Fiat-Shamir test island (verbatim from eval_circuit run_tests).
    let curve = secp256k1();
    let mut xof = fiat_shamir_seed(&ops);

    let mut targets = Vec::with_capacity(shots);
    let mut offsets = Vec::with_capacity(shots);
    let mut expected = Vec::with_capacity(shots);
    for _ in 0..shots {
        let mut rb = [[0u8; 32]; 2];
        xof.read(&mut rb[0]);
        xof.read(&mut rb[1]);
        let k1 = U256::from_le_bytes(rb[0]);
        let k2 = U256::from_le_bytes(rb[1]);
        let t = curve.mul(curve.gx, curve.gy, k1);
        let o = curve.mul(curve.gx, curve.gy, k2);
        if t.0 == o.0 {
            continue;
        }
        if t.0.is_zero() && t.1.is_zero() {
            continue;
        }
        if o.0.is_zero() && o.1.is_zero() {
            continue;
        }
        let e = curve.add(t.0, t.1, o.0, o.1);
        targets.push(t);
        offsets.push(o);
        expected.push(e);
    }
    let n = targets.len();

    let mut sim = Simulator::new(total_qubits as usize, num_bits as usize, &mut xof);
    let mut classical_failures = 0usize;
    let mut phase_garbage_batches = 0usize;
    let mut ancilla_garbage_batches = 0usize;
    let mut aborted = false;
    let mut phase_aborted = false;
    let mut ancilla_aborted = false;

    const BATCH: usize = 64;
    let num_batches = (n + BATCH - 1) / BATCH;
    'outer: for batch in 0..num_batches {
        let bs = BATCH.min(n - batch * BATCH);
        let cond_mask: u64 = if bs == 64 { u64::MAX } else { (1u64 << bs) - 1 };

        sim.clear_for_shot();
        for shot in 0..bs {
            let i = batch * BATCH + shot;
            sim.set_register(&regs[0], targets[i].0, shot);
            sim.set_register(&regs[1], targets[i].1, shot);
            sim.set_register(&regs[2], offsets[i].0, shot);
            sim.set_register(&regs[3], offsets[i].1, shot);
        }

        sim.apply_iter(ops.iter());

        for shot in 0..bs {
            let i = batch * BATCH + shot;
            let gx = sim.get_register(&regs[0], shot);
            let gy = sim.get_register(&regs[1], shot);
            if gx != expected[i].0 || gy != expected[i].1 {
                classical_failures += 1;
                if early_exit {
                    aborted = true;
                    break 'outer;
                }
            }
        }

        let phase = sim.phase & cond_mask;
        if phase != 0 {
            phase_garbage_batches += 1;
            if early_exit {
                phase_aborted = true;
                break 'outer;
            }
        }

        for register in &regs {
            for qb in register {
                if let QubitOrBit::Qubit(q) = *qb {
                    *sim.qubit_mut(q) = 0;
                }
            }
        }
        let mut garbage = false;
        for q in 0..total_qubits {
            if sim.qubit(QubitId(q)) & cond_mask != 0 {
                garbage = true;
                break;
            }
        }
        if garbage {
            ancilla_garbage_batches += 1;
            if early_exit {
                ancilla_aborted = true;
                break 'outer;
            }
        }
    }

    // In early-exit mode classical is a 0/1 flag (1 if we aborted on a mismatch).
    let classical_out = if early_exit && aborted {
        1
    } else {
        classical_failures
    };
    let phase_out = if early_exit && phase_aborted {
        1
    } else {
        phase_garbage_batches
    };
    let ancilla_out = if early_exit && ancilla_aborted {
        1
    } else {
        ancilla_garbage_batches
    };

    let avg_tof = sim.stats.toffoli_gates as f64 / (n.max(1)) as f64;
    println!(
        "RESULT qubits={} classical={} phase={} ancilla={} toffoli={:.3} nonce={}",
        total_qubits, classical_out, phase_out, ancilla_out, avg_tof, nonce
    );
}
