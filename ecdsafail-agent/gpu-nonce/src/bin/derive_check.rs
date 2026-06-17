// M3 de-risk step 1: validate GPU-derived inputs + measured-gate RNG == CPU dump.
//
// Builds the FS prefix exactly like main.rs (DIALOG_TAIL_NONCE=none; full_len counts
// the 2*NONCE_BITS tail ops the kernel appends), then runs derive_check for the baked
// nonce and compares against /tmp/phase_m1 (phase_ref's dump for the SAME nonce).
//
// Compares: survivor count n, batch-0's 64 inputs (reg-packed tx/ty/ox/oy/ex), and
// the full measured-gate RNG buffer (1,352,725 u64s) -- byte-exact.

use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use quantum_ecc::point_add::build;
use alloy_primitives::U256;
#[path="../keccak.rs"] mod keccak;
#[path="../gtable.rs"] mod gtable;

const SRC: &str = concat!(
    include_str!("../field.cuh"), "\n",
    include_str!("../points.cu"), "\n",
    include_str!("../gcd.cu"), "\n",
    include_str!("../keccak.cu"), "\n",
    include_str!("../hunt_phase.cu"));

const NONCE_BITS: u64 = 48;
const DUMP: &str = "/tmp/phase_m1";

fn read_u64s(path: &str) -> Vec<u64> {
    let b = std::fs::read(path).unwrap();
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn main() {
    let nonce: u64 = std::env::var("NONCE").ok().and_then(|s| s.parse().ok()).unwrap_or(3480010331559);
    std::env::set_var("DIALOG_TAIL_NONCE", "none");
    let base_ops = build();
    let full_len = base_ops.len() as u64 + 2 * NONCE_BITS;
    let mut pk = keccak::Shake256::new();
    pk.absorb(b"quantum_ecc-fiat-shamir-v2");
    pk.absorb(&full_len.to_le_bytes());
    for op in &base_ops {
        pk.absorb(&[op.kind as u8]);
        pk.absorb(&op.q_control2.0.to_le_bytes()); pk.absorb(&op.q_control1.0.to_le_bytes());
        pk.absorb(&op.q_target.0.to_le_bytes()); pk.absorb(&op.c_target.0.to_le_bytes());
        pk.absorb(&op.c_condition.0.to_le_bytes()); pk.absorb(&op.r_target.0.to_le_bytes());
    }
    let (st0, buf0, pos0) = (pk.st.to_vec(), pk.buf.to_vec(), pk.pos as i32);
    drop(base_ops);
    let tbl = gtable::build_gtable(&gtable::curve());
    eprintln!("FS prefix built; nonce={}", nonce);

    // CPU golden from /tmp/phase_m1 (must have been dumped for THIS nonce).
    let rng_golden = read_u64s(&format!("{DUMP}/rng.bin"));
    let n_rng = rng_golden.len() as u64;
    eprintln!("CPU dump: n_rng={}", n_rng);

    // CPU init_q -> reconstruct batch-0 reg-packed inputs from reg_q + golden? Simpler:
    // we recompute the expected reg-packed inputs directly from /tmp/phase_m1's init_q
    // using reg_q.bin (reg0/reg1 qubit lists) and the bit-slice for shot s.
    // But reg2/reg3 are CLASSICAL bits (in init_b), and ex is in golden.bin (reg0/reg1 final).
    // Instead we recompute t/o/e on the CPU here, mirroring phase_ref's derivation, and
    // compare to GPU. That cross-checks both the dump and the GPU against an independent
    // host derivation.
    let cv = gtable::curve();
    use sha3::digest::{Update, ExtendableOutput, XofReader};
    // rebuild the SAME finalized XOF on host = absorb nonce tail into the prefix.
    let mut hasher = sha3::Shake256::default();
    // Re-derive the full op stream WITH the tail for an independent host XOF.
    std::env::set_var("DIALOG_TAIL_NONCE", &nonce.to_string());
    let ops_tail = build();
    hasher.update(b"quantum_ecc-fiat-shamir-v2");
    hasher.update(&(ops_tail.len() as u64).to_le_bytes());
    for op in &ops_tail {
        hasher.update(&[op.kind as u8]);
        hasher.update(&op.q_control2.0.to_le_bytes()); hasher.update(&op.q_control1.0.to_le_bytes());
        hasher.update(&op.q_target.0.to_le_bytes()); hasher.update(&op.c_target.0.to_le_bytes());
        hasher.update(&op.c_condition.0.to_le_bytes()); hasher.update(&op.r_target.0.to_le_bytes());
    }
    assert_eq!(ops_tail.len() as u64, full_len, "tail-build op count != full_len");
    let mut xof = hasher.finalize_xof();
    let mut host_inputs = [[0u64; 20]; 64];
    let mut nsv = 0usize;
    for _ in 0..9024usize {
        let mut rb = [[0u8; 32]; 2];
        xof.read(&mut rb[0]); xof.read(&mut rb[1]);
        let k1 = U256::from_le_bytes(rb[0]); let k2 = U256::from_le_bytes(rb[1]);
        let t = cv.mul(cv.gx, cv.gy, k1); let o = cv.mul(cv.gx, cv.gy, k2);
        if t.0 == o.0 { continue; }
        if t.0.is_zero() && t.1.is_zero() { continue; }
        if o.0.is_zero() && o.1.is_zero() { continue; }
        let e = cv.add(t.0, t.1, o.0, o.1);
        if nsv < 64 {
            let pack = |v: U256| { let l = v.as_limbs(); [l[0],l[1],l[2],l[3]] };
            let row = &mut host_inputs[nsv];
            row[0..4].copy_from_slice(&pack(t.0)); row[4..8].copy_from_slice(&pack(t.1));
            row[8..12].copy_from_slice(&pack(o.0)); row[12..16].copy_from_slice(&pack(o.1));
            row[16..20].copy_from_slice(&pack(e.0));
        }
        nsv += 1;
    }
    eprintln!("host survivors n={}", nsv);

    // ---- GPU ----
    let ctx = CudaContext::new(0).unwrap();
    let stream = ctx.default_stream();
    let ptx = match compile_ptx(SRC) { Ok(p)=>p, Err(e)=>{ eprintln!("NVRTC ERR:\n{:?}", e); std::process::exit(1);} };
    let m = ctx.load_module(ptx).unwrap();
    let f = m.load_function("derive_check").unwrap();

    let dst0 = stream.memcpy_stod(&st0).unwrap();
    let dbuf = stream.memcpy_stod(&buf0).unwrap();
    let dtbl = stream.memcpy_stod(&tbl).unwrap();
    let mut d_inputs = stream.alloc_zeros::<u64>(20*64).unwrap();
    let mut d_rng = stream.alloc_zeros::<u64>(n_rng as usize).unwrap();
    let mut d_n = stream.alloc_zeros::<u64>(1).unwrap();
    let (nb, xk) = (NONCE_BITS as i32, 6i32);

    let cfg = LaunchConfig { grid_dim:(1,1,1), block_dim:(1,1,1), shared_mem_bytes:0 };
    let t0 = std::time::Instant::now();
    {
        let mut lb = stream.launch_builder(&f);
        lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&nonce)
          .arg(&dtbl).arg(&mut d_inputs).arg(&mut d_rng).arg(&n_rng).arg(&mut d_n);
        unsafe { lb.launch(cfg).unwrap(); }
    }
    stream.synchronize().unwrap();
    eprintln!("derive_check kernel time: {:?}", t0.elapsed());

    let g_inputs = stream.memcpy_dtov(&d_inputs).unwrap();
    let g_rng = stream.memcpy_dtov(&d_rng).unwrap();
    let g_n = stream.memcpy_dtov(&d_n).unwrap()[0];

    let mut fails = 0;
    if g_n != nsv as u64 { eprintln!("SURVIVOR COUNT mismatch: gpu={} host={}", g_n, nsv); fails+=1; }

    let names = ["tx","ty","ox","oy","ex"];
    for s in 0..64 {
        for fld in 0..5 {
            for j in 0..4 {
                let gi = g_inputs[s*20 + fld*4 + j];
                let hi = host_inputs[s][fld*4 + j];
                if gi != hi {
                    if fails < 12 { eprintln!("input shot {s} {} limb{j}: gpu={:#x} host={:#x}", names[fld], gi, hi); }
                    fails += 1;
                }
            }
        }
    }

    let mut rng_fails = 0;
    for k in 0..n_rng as usize {
        if g_rng[k] != rng_golden[k] {
            if rng_fails < 8 { eprintln!("rng[{k}]: gpu={:#018x} cpu={:#018x}", g_rng[k], rng_golden[k]); }
            rng_fails += 1;
        }
    }
    if rng_fails > 0 { eprintln!("RNG mismatches: {}/{}", rng_fails, n_rng); fails += rng_fails; }

    if fails == 0 {
        println!("PASS  (nonce {} : n={}, batch-0 64 inputs byte-exact, {} RNG words byte-exact)", nonce, g_n, n_rng);
    } else {
        println!("FAIL  ({} mismatches)", fails);
        std::process::exit(1);
    }
}
