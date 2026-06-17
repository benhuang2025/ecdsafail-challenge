//! AGENT TOOL — validation harness for the analytical square-cleanup pre-screen.
//!
//! Per shot, computes lambda = (qy-py)/(qx-px) mod p (the value squared in the
//! point-add: x3 = lambda^2 - px - qx), then calls the UPSTREAM
//! `square_row_window_cleanup_summary(lambda, ...)` (we CALL it, never copy it),
//! and sums the square-cleanup carry-escape mismatches across all 9024 shots.
//!
//! Those mismatches are "soft phase-risk events" per the filter's own comment,
//! so the hypothesis is: analytical square-mismatch total ≈ the eval's PHASE
//! count. This binary prints the analytical total; compare it against
//! `fast-screen`'s simulated `phase=` for the same nonce to validate.
//!
//! Env: DIALOG_TAIL_NONCE (the nonce), NS_SHOTS (default 9024).
//! Reads SQUARE_ROW_MAX_SEG / SQUARE_ROW_WINDOW_CLEAN_COMPARE_BITS from env
//! AFTER build() (the set_var block sets them).

use alloy_primitives::U256;
use quantum_ecc::circuit::analyze_ops;
use quantum_ecc::point_add;
use quantum_ecc::point_add::dialog_gcd_classical_filter::square_row_window_cleanup_summary;
use quantum_ecc::weierstrass_elliptic_curve::WeierstrassEllipticCurve;
use sha3::{
    digest::{ExtendableOutput, Update, XofReader},
    Shake256,
};

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

fn sub_mod_p(a: U256, b: U256, p: U256) -> U256 {
    if a >= b {
        a - b
    } else {
        p - (b - a)
    }
}

/// inv(a) mod p via Fermat: a^(p-2) mod p. (p is prime.)
fn mod_inv(a: U256, p: U256) -> U256 {
    let mut result = U256::from(1u64);
    let mut base = a % p;
    let mut exp = p - U256::from(2u64);
    while exp > U256::ZERO {
        if exp.bit(0) {
            result = result.mul_mod(base, p);
        }
        base = base.mul_mod(base, p);
        exp >>= 1;
    }
    result
}

fn main() {
    let shots: usize = std::env::var("NS_SHOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9024);
    let nonce = std::env::var("DIALOG_TAIL_NONCE").unwrap_or_else(|_| "baked".to_string());

    let ops = point_add::build();
    let (_tq, _nb, _nr, _regs) = analyze_ops(ops.iter());

    // square config (set by build()'s set_var block)
    let max_seg: usize = std::env::var("SQUARE_ROW_MAX_SEG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(158);
    let clean_bits: usize = std::env::var("SQUARE_ROW_WINDOW_CLEAN_COMPARE_BITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18);

    let curve = secp256k1();
    let p = curve.modulus;
    let mut xof = fiat_shamir_seed(&ops);

    let mut n = 0usize;
    let mut total_sq_mismatch = 0usize;
    let mut shots_with_mismatch = 0usize;
    for _ in 0..shots {
        let mut rb = [[0u8; 32]; 2];
        xof.read(&mut rb[0]);
        xof.read(&mut rb[1]);
        let k1 = U256::from_le_bytes(rb[0]);
        let k2 = U256::from_le_bytes(rb[1]);
        let t = curve.mul(curve.gx, curve.gy, k1); // (px, py)
        let o = curve.mul(curve.gx, curve.gy, k2); // (qx, qy)
        if t.0 == o.0 {
            continue;
        }
        if t.0.is_zero() && t.1.is_zero() {
            continue;
        }
        if o.0.is_zero() && o.1.is_zero() {
            continue;
        }
        n += 1;
        // lambda = (qy - py) / (qx - px) mod p
        let num = sub_mod_p(o.1, t.1, p);
        let den = sub_mod_p(o.0, t.0, p);
        let lambda = num.mul_mod(mod_inv(den, p), p);
        let s = square_row_window_cleanup_summary(lambda, max_seg, clean_bits, &[], &[]);
        total_sq_mismatch += s.mismatches;
        if s.mismatches > 0 {
            shots_with_mismatch += 1;
        }
    }

    println!(
        "SQCHECK nonce={} shots={} max_seg={} clean_bits={} sq_mismatch_total={} shots_with_mismatch={}",
        nonce, n, max_seg, clean_bits, total_sq_mismatch, shots_with_mismatch
    );
}
