// M2 prep: bit interval-coloring + slot remapping + v2 op-stream dump.
//
// Same input-derivation and golden op-loop as phase_ref.rs, but additionally:
//   - compute each classical bit's live interval [first ref op .. last ref op]
//     (a "ref" = any op using the bit as c_target or c_condition; register bits
//      are referenced at init => interval start = -1)
//   - interval-color bits onto >=peak slots (greedy free-list sweep), report peak
//   - remap c_target/c_condition in the op stream to slot ids
//   - emit a per-op "zero slot(s)" flag set when a slot is (re)used by a NEW
//     occupant at the op that begins that occupant's interval, AND the prior
//     occupant of the slot was a different bit (stale-bit correctness).
//   - dump v2 compact ops + remapped initial bit slots + meta + golden.
//
// Dump dir: /tmp/phase_m2/
//
// env: DIALOG_TAIL_NONCE, DIALOG_GCD_TOBITVECTOR_CSWAP_BODY_TRIM, PHASE_BATCH.

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
const DUMP_DIR: &str = "/tmp/phase_m2";

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

// v2 compact op for the kernel: 24 bytes.
//   u8 kind; u8 zflag (bit0: zero slot_t, bit1: zero slot_c); u8 pad[2];
//   i32 qc2; i32 qc1; i32 qt; i32 slot_t; i32 slot_c;  (-1 = NONE)
#[repr(C)]
#[derive(Clone, Copy)]
struct CompactOp2 {
    kind: u8,
    zflag: u8,
    _pad: [u8; 2],
    qc2: i32,
    qc1: i32,
    qt: i32,
    st: i32, // slot for c_target
    sc: i32, // slot for c_condition
}

fn idx_q(id: quantum_ecc::circuit::QubitId) -> i32 {
    if id == NO_QUBIT { -1 } else { id.0 as i32 }
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

    // ---- interval coloring of classical bits ----
    // first_ref[b], last_ref[b]: op-index range over which bit b is live.
    // register bits (reg2/reg3) are written at init => first_ref = 0 effectively,
    //   represented as ref index 0 with an "init" flag (-1 sentinel start).
    const NONE_REF: i64 = i64::MAX;
    let mut first_ref = vec![NONE_REF; num_bits];
    let mut last_ref = vec![-1i64; num_bits];
    let mut touch = |b: usize, oi: i64, fr: &mut Vec<i64>, lr: &mut Vec<i64>| {
        if oi < fr[b] { fr[b] = oi; }
        if oi > lr[b] { lr[b] = oi; }
    };
    // register bits referenced at init (-1)
    for r in 2..4usize {
        for qb in &regs[r] {
            if let QubitOrBit::Bit(id) = qb {
                touch(id.0 as usize, -1, &mut first_ref, &mut last_ref);
            }
        }
    }
    for (oi, op) in ops.iter().enumerate() {
        let oi = oi as i64;
        if op.c_target != NO_BIT { touch(op.c_target.0 as usize, oi, &mut first_ref, &mut last_ref); }
        if op.c_condition != NO_BIT { touch(op.c_condition.0 as usize, oi, &mut first_ref, &mut last_ref); }
    }

    // Bits that are never referenced: leave unmapped (slot = -1). Count them.
    let mut live_bits: Vec<usize> = (0..num_bits).filter(|&b| first_ref[b] != NONE_REF).collect();
    let unused = num_bits - live_bits.len();
    eprintln!("referenced bits={} unused={}", live_bits.len(), unused);

    // Sort intervals by start, then end. Greedy slot assignment via a free list
    // ordered by the slot's current end. We sweep op timeline using events.
    // Build start events sorted by start asc; maintain min-heap of (end, slot) busy;
    // free slots reusable when their end < current start.
    live_bits.sort_by_key(|&b| (first_ref[b], last_ref[b]));

    let mut slot_of = vec![-1i64; num_bits];
    let mut slot_prev_occupant: Vec<i64> = Vec::new(); // last bit that used each slot
    let mut slot_end: Vec<i64> = Vec::new();           // current end for each slot
    let mut free_slots: Vec<usize> = Vec::new();       // reusable slot ids
    // busy slots tracked by (end, slot); we lazily release. Simpler: since events
    // are by start, on each new bit free any slot whose end < this start.
    // Maintain busy as Vec and scan—but n is ~800k, peak ~770, scanning is fine.
    let mut busy: Vec<usize> = Vec::new(); // slot ids currently occupied

    let mut peak = 0usize;
    // zero-flag: does this bit's slot need zeroing at interval start because a
    // DIFFERENT prior occupant left stale contents? true unless slot is brand new.
    let mut bit_needs_zero = vec![false; num_bits];

    for &b in &live_bits {
        let start = first_ref[b];
        // release slots whose end < start
        let mut still_busy = Vec::with_capacity(busy.len());
        for &s in &busy {
            if slot_end[s] < start { free_slots.push(s); }
            else { still_busy.push(s); }
        }
        busy = still_busy;
        // assign
        let s = if let Some(s) = free_slots.pop() {
            // reused slot: prior occupant differs from b (always, since b new) ->
            // stale bits possible. Mark zero unless this bit's first op fully writes
            // all 64 shots unconditionally (rare; be safe -> always zero on reuse).
            bit_needs_zero[b] = slot_prev_occupant[s] != b as i64;
            s
        } else {
            let s = slot_end.len();
            slot_end.push(0);
            slot_prev_occupant.push(-1);
            // brand new slot: contents already 0 (kernel zero-inits) -> no zero needed
            bit_needs_zero[b] = false;
            s
        };
        slot_of[b] = s as i64;
        slot_end[s] = last_ref[b];
        slot_prev_occupant[s] = b as i64;
        busy.push(s);
        peak = peak.max(busy.len());
    }
    let num_slots = slot_end.len();
    eprintln!("PEAK simultaneously-live bits = {} ; num_slots allocated = {}", peak, num_slots);

    // For each bit, the op index where its interval STARTS (the op that first
    // references it). At that op, if bit_needs_zero, the kernel must zero the slot
    // BEFORE applying the op. We attach the flag to that op. For register bits
    // (start = -1) the slot is set at init; no per-op zeroing (handled by init).
    // Build: zero_at_op[oi] -> set of slots to zero before op oi.
    // Since a slot reused mid-stream: zero the new occupant's slot at its first op.
    let mut zero_slot_target = vec![false; ops.len()]; // zero slot_of[c_target] before op
    let mut zero_slot_cond = vec![false; ops.len()];   // zero slot_of[c_condition] before op
    // We need to mark the FIRST op that references each needs-zero bit.
    // Recompute first-ref op per bit and which field it appears in there.
    {
        let mut done = vec![false; num_bits];
        for (oi, op) in ops.iter().enumerate() {
            if op.c_target != NO_BIT {
                let b = op.c_target.0 as usize;
                if bit_needs_zero[b] && !done[b] && first_ref[b] == oi as i64 {
                    zero_slot_target[oi] = true;
                    done[b] = true;
                }
            }
            if op.c_condition != NO_BIT {
                let b = op.c_condition.0 as usize;
                if bit_needs_zero[b] && !done[b] && first_ref[b] == oi as i64 {
                    zero_slot_cond[oi] = true;
                    done[b] = true;
                }
            }
        }
        // sanity: every needs-zero bit got a flag
        for b in 0..num_bits {
            if bit_needs_zero[b] { assert!(done[b], "needs-zero bit {b} got no flag op"); }
        }
    }
    let n_zero: usize = zero_slot_target.iter().filter(|&&x|x).count()
        + zero_slot_cond.iter().filter(|&&x|x).count();
    eprintln!("slot-zeroing events = {}", n_zero);

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

    let mut qubits = vec![0u64; num_qubits];
    let mut bits = vec![0u64; num_bits];

    let mut dump_initial_q = vec![0u64; num_qubits];
    let mut dump_initial_b = vec![0u64; num_bits];
    let mut dump_rng: Vec<u64> = Vec::new();
    let mut golden_reg0 = [U256::ZERO; 64];
    let mut golden_reg1 = [U256::ZERO; 64];
    let mut golden_phase: u64 = 0;
    let mut golden_ancilla_clean = [false; 64];

    let mut classical_failures = 0usize;
    let mut phase_garbage_batches = 0usize;
    let mut ancilla_garbage_batches = 0usize;

    for batch in 0..num_batches {
        let bs = BATCH.min(n - batch * BATCH);
        let cond_mask: u64 = if bs == 64 { u64::MAX } else { (1u64 << bs) - 1 };
        let is_chosen = batch == chosen_batch;

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

        let pmask = phase & cond_mask;
        if pmask != 0 { phase_garbage_batches += 1; }
        if is_chosen { golden_phase = phase; }

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

    // ---- dumps (v2) ----
    fs::create_dir_all(DUMP_DIR).unwrap();

    // v2 compact ops
    let mut compact: Vec<CompactOp2> = Vec::with_capacity(ops.len());
    for (oi, op) in ops.iter().enumerate() {
        let st = if op.c_target != NO_BIT { slot_of[op.c_target.0 as usize] as i32 } else { -1 };
        let sc = if op.c_condition != NO_BIT { slot_of[op.c_condition.0 as usize] as i32 } else { -1 };
        let mut zflag = 0u8;
        if zero_slot_target[oi] { zflag |= 1; }
        if zero_slot_cond[oi] { zflag |= 2; }
        compact.push(CompactOp2 {
            kind: op.kind as u8,
            zflag,
            _pad: [0; 2],
            qc2: idx_q(op.q_control2),
            qc1: idx_q(op.q_control1),
            qt: idx_q(op.q_target),
            st,
            sc,
        });
    }
    let mut f = fs::File::create(format!("{DUMP_DIR}/ops2.bin")).unwrap();
    f.write_all(&(compact.len() as u64).to_le_bytes()).unwrap();
    for c in &compact {
        f.write_all(&[c.kind, c.zflag]).unwrap();
        f.write_all(&c._pad).unwrap();
        f.write_all(&c.qc2.to_le_bytes()).unwrap();
        f.write_all(&c.qc1.to_le_bytes()).unwrap();
        f.write_all(&c.qt.to_le_bytes()).unwrap();
        f.write_all(&c.st.to_le_bytes()).unwrap();
        f.write_all(&c.sc.to_le_bytes()).unwrap();
    }

    // 12-byte compact ops (diagnostic: halve op-stream bandwidth).
    // layout: u8 kind; u8 zflag; i16 qc2; i16 qc1; i16 qt; i16 st; i16 sc  (-1 = NONE)
    // qubit ids <=1169 and slot ids <=766 both fit in i16.
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/ops3.bin")).unwrap();
        f.write_all(&(compact.len() as u64).to_le_bytes()).unwrap();
        for c in &compact {
            f.write_all(&[c.kind, c.zflag]).unwrap();
            f.write_all(&(c.qc2 as i16).to_le_bytes()).unwrap();
            f.write_all(&(c.qc1 as i16).to_le_bytes()).unwrap();
            f.write_all(&(c.qt as i16).to_le_bytes()).unwrap();
            f.write_all(&(c.st as i16).to_le_bytes()).unwrap();
            f.write_all(&(c.sc as i16).to_le_bytes()).unwrap();
        }
    }

    // meta2: num_qubits, num_slots, bs, n_rng, chosen_batch, n_ops
    let chosen_bs = BATCH.min(n - chosen_batch * BATCH);
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/meta2.bin")).unwrap();
        f.write_all(&(num_qubits as u64).to_le_bytes()).unwrap();
        f.write_all(&(num_slots as u64).to_le_bytes()).unwrap();
        f.write_all(&(chosen_bs as u64).to_le_bytes()).unwrap();
        f.write_all(&(dump_rng.len() as u64).to_le_bytes()).unwrap();
        f.write_all(&(chosen_batch as u64).to_le_bytes()).unwrap();
        f.write_all(&(ops.len() as u64).to_le_bytes()).unwrap();
    }

    // initial state: qubits as-is; bits REMAPPED into slot array (num_slots u64)
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/init_q.bin")).unwrap();
        for &v in &dump_initial_q { f.write_all(&v.to_le_bytes()).unwrap(); }
        // slot init: scatter each referenced bit's initial value into its slot.
        // Only register bits have nonzero init; others start 0. Multiple bits map
        // to the same slot only if disjoint in time, and at init only the bits
        // whose interval includes init (-1) are set => exactly register bits.
        let mut slot_init = vec![0u64; num_slots];
        for r in 2..4usize {
            for qb in &regs[r] {
                if let QubitOrBit::Bit(id) = qb {
                    let s = slot_of[id.0 as usize];
                    assert!(s >= 0);
                    slot_init[s as usize] = dump_initial_b[id.0 as usize];
                }
            }
        }
        let mut f = fs::File::create(format!("{DUMP_DIR}/init_b_slots.bin")).unwrap();
        for &v in &slot_init { f.write_all(&v.to_le_bytes()).unwrap(); }
    }

    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/rng.bin")).unwrap();
        for &v in &dump_rng { f.write_all(&v.to_le_bytes()).unwrap(); }
    }

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

    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/golden.bin")).unwrap();
        for shot in 0..64usize {
            for &limb in golden_reg0[shot].as_limbs() { f.write_all(&limb.to_le_bytes()).unwrap(); }
            for &limb in golden_reg1[shot].as_limbs() { f.write_all(&limb.to_le_bytes()).unwrap(); }
            f.write_all(&[golden_ancilla_clean[shot] as u8]).unwrap();
        }
        f.write_all(&golden_phase.to_le_bytes()).unwrap();
    }

    eprintln!("DUMPED v2 chosen_batch={} bs={} n_rng={} num_slots={} -> {DUMP_DIR}",
        chosen_batch, chosen_bs, dump_rng.len(), num_slots);
}
