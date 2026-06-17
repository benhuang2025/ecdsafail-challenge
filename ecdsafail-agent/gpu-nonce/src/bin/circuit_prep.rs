// M3 nonce-independent circuit prep for hunt_phase.
//
// Emits the slot-remapped op stream + register index tables that hunt_phase needs.
// These depend ONLY on the circuit (build() ops), NOT on the nonce, so they are
// computed once on the host and uploaded to all GPUs.
//
// Dumps to /tmp/phase_circuit/:
//   ops2.bin         : u64 count + 24 B/op compact v2 ops (kind,zflag,pad2,qc2,qc1,qt,slot_t,slot_c)
//   reg_q.bin        : 512 u32 = reg0[256] | reg1[256] qubit indices
//   reg_s.bin        : 512 u32 = reg2[256] | reg3[256] SLOT indices (classical bits remapped)
//   meta.bin         : num_qubits u64, num_slots u64, n_ops u64
//
// IMPORTANT: build() must be invoked with DIALOG_TAIL_NONCE=none so the op stream
// (and thus the slot coloring) is byte-identical to what main.rs hashes for the FS
// prefix. The nonce tail (identity X ops) does not reference any classical bit, so
// it never changes the coloring; we still strip it to keep ops2 == the FS-prefix ops.

use quantum_ecc::circuit::{analyze_ops, OperationType, QubitOrBit, NO_BIT, NO_QUBIT};
use quantum_ecc::point_add::build;
use std::fs;
use std::io::Write;

fn idx_q(id: quantum_ecc::circuit::QubitId) -> i32 {
    if id == NO_QUBIT { -1 } else { id.0 as i32 }
}


fn main() {
    let DUMP_DIR: &str = Box::leak(std::env::var("PHASE_CDUMP").unwrap_or_else(|_| "/tmp/phase_circuit".to_string()).into_boxed_str());

    std::env::set_var("DIALOG_TAIL_NONCE", "none");
    let ops = build();
    eprintln!("n_ops={}", ops.len());
    let (num_qubits, num_bits, num_regs, regs) = analyze_ops(ops.iter());
    eprintln!("num_qubits={} num_bits={} num_regs={}", num_qubits, num_bits, num_regs);
    assert_eq!(regs.len(), 4);
    for r in &regs { assert_eq!(r.len(), 256); }
    let num_qubits = num_qubits as usize;
    let num_bits = num_bits as usize;

    // ---- interval coloring of classical bits (identical to phase_prep.rs) ----
    const NONE_REF: i64 = i64::MAX;
    let mut first_ref = vec![NONE_REF; num_bits];
    let mut last_ref = vec![-1i64; num_bits];
    let mut touch = |b: usize, oi: i64, fr: &mut Vec<i64>, lr: &mut Vec<i64>| {
        if oi < fr[b] { fr[b] = oi; }
        if oi > lr[b] { lr[b] = oi; }
    };
    for r in 2..4usize {
        for qb in &regs[r] {
            if let QubitOrBit::Bit(id) = qb { touch(id.0 as usize, -1, &mut first_ref, &mut last_ref); }
        }
    }
    for (oi, op) in ops.iter().enumerate() {
        let oi = oi as i64;
        if op.c_target != NO_BIT { touch(op.c_target.0 as usize, oi, &mut first_ref, &mut last_ref); }
        if op.c_condition != NO_BIT { touch(op.c_condition.0 as usize, oi, &mut first_ref, &mut last_ref); }
    }
    let mut live_bits: Vec<usize> = (0..num_bits).filter(|&b| first_ref[b] != NONE_REF).collect();
    eprintln!("referenced bits={} unused={}", live_bits.len(), num_bits - live_bits.len());
    live_bits.sort_by_key(|&b| (first_ref[b], last_ref[b]));

    let mut slot_of = vec![-1i64; num_bits];
    let mut slot_prev_occupant: Vec<i64> = Vec::new();
    let mut slot_end: Vec<i64> = Vec::new();
    let mut free_slots: Vec<usize> = Vec::new();
    let mut busy: Vec<usize> = Vec::new();
    let mut peak = 0usize;
    let mut bit_needs_zero = vec![false; num_bits];
    for &b in &live_bits {
        let start = first_ref[b];
        let mut still_busy = Vec::with_capacity(busy.len());
        for &s in &busy {
            if slot_end[s] < start { free_slots.push(s); } else { still_busy.push(s); }
        }
        busy = still_busy;
        let s = if let Some(s) = free_slots.pop() {
            bit_needs_zero[b] = slot_prev_occupant[s] != b as i64;
            s
        } else {
            let s = slot_end.len();
            slot_end.push(0); slot_prev_occupant.push(-1);
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
    eprintln!("PEAK live bits = {} ; num_slots = {}", peak, num_slots);

    let mut zero_slot_target = vec![false; ops.len()];
    let mut zero_slot_cond = vec![false; ops.len()];
    {
        let mut done = vec![false; num_bits];
        for (oi, op) in ops.iter().enumerate() {
            if op.c_target != NO_BIT {
                let b = op.c_target.0 as usize;
                if bit_needs_zero[b] && !done[b] && first_ref[b] == oi as i64 { zero_slot_target[oi] = true; done[b] = true; }
            }
            if op.c_condition != NO_BIT {
                let b = op.c_condition.0 as usize;
                if bit_needs_zero[b] && !done[b] && first_ref[b] == oi as i64 { zero_slot_cond[oi] = true; done[b] = true; }
            }
        }
        for b in 0..num_bits { if bit_needs_zero[b] { assert!(done[b]); } }
    }

    // ---- dumps ----
    fs::create_dir_all(DUMP_DIR).unwrap();
    let mut f = fs::File::create(format!("{DUMP_DIR}/ops2.bin")).unwrap();
    f.write_all(&(ops.len() as u64).to_le_bytes()).unwrap();
    for (oi, op) in ops.iter().enumerate() {
        let st = if op.c_target != NO_BIT { slot_of[op.c_target.0 as usize] as i32 } else { -1 };
        let sc = if op.c_condition != NO_BIT { slot_of[op.c_condition.0 as usize] as i32 } else { -1 };
        let mut zflag = 0u8;
        if zero_slot_target[oi] { zflag |= 1; }
        if zero_slot_cond[oi] { zflag |= 2; }
        f.write_all(&[op.kind as u8, zflag, 0, 0]).unwrap();
        f.write_all(&idx_q(op.q_control2).to_le_bytes()).unwrap();
        f.write_all(&idx_q(op.q_control1).to_le_bytes()).unwrap();
        f.write_all(&idx_q(op.q_target).to_le_bytes()).unwrap();
        f.write_all(&st.to_le_bytes()).unwrap();
        f.write_all(&sc.to_le_bytes()).unwrap();
    }

    // reg_q: reg0|reg1 qubit indices
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/reg_q.bin")).unwrap();
        for r in 0..2usize {
            for qb in &regs[r] {
                if let QubitOrBit::Qubit(q) = qb { f.write_all(&(q.0 as u32).to_le_bytes()).unwrap(); }
                else { panic!("reg{r} expected qubits"); }
            }
        }
    }
    // reg_s: reg2|reg3 SLOT indices
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/reg_s.bin")).unwrap();
        for r in 2..4usize {
            for qb in &regs[r] {
                if let QubitOrBit::Bit(b) = qb {
                    let s = slot_of[b.0 as usize];
                    assert!(s >= 0, "reg{r} bit unmapped");
                    f.write_all(&(s as u32).to_le_bytes()).unwrap();
                } else { panic!("reg{r} expected bits"); }
            }
        }
    }
    {
        let mut f = fs::File::create(format!("{DUMP_DIR}/meta.bin")).unwrap();
        f.write_all(&(num_qubits as u64).to_le_bytes()).unwrap();
        f.write_all(&(num_slots as u64).to_le_bytes()).unwrap();
        f.write_all(&(ops.len() as u64).to_le_bytes()).unwrap();
    }
    eprintln!("DUMPED -> {DUMP_DIR}  (num_slots={} n_ops={})", num_slots, ops.len());
}
