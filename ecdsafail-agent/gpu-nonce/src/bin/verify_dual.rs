// Lever-B stage-2 parallel verifier: reads candidate nonces (CANDS=file or NONCES=csv),
// verifies classical+phase via the hunt_phase op-loop in nonce-list mode, emits
// DUAL_CLEAN_CANDIDATE for any 0/0 nonce. Single GPU (CUDA_VISIBLE_DEVICES).
use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use quantum_ecc::point_add::build;
#[path = "../keccak.rs"]
mod keccak;
#[path = "../gtable.rs"]
mod gtable;

const SRC: &str = concat!(
    include_str!("../field.cuh"), "\n",
    include_str!("../points.cu"), "\n",
    include_str!("../gcd.cu"), "\n",
    include_str!("../keccak.cu"), "\n",
    include_str!("../hunt_phase.cu"));
const NONCE_BITS: u64 = 48;

fn read_u64s(p: &str) -> Vec<u64> {
    std::fs::read(p).unwrap().chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}
fn read_u32s(p: &str) -> Vec<u32> {
    std::fs::read(p).unwrap().chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn main() {
    let cdump: &str = Box::leak(
        std::env::var("PHASE_CDUMP").unwrap_or_else(|_| "/tmp/phase_circuit".to_string()).into_boxed_str(),
    );
    let cands: Vec<u64> = if let Ok(p) = std::env::var("CANDS") {
        std::fs::read_to_string(p).unwrap()
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect()
    } else {
        std::env::var("NONCES").unwrap().split(',').map(|s| s.trim().parse().unwrap()).collect()
    };
    eprintln!("VERIFY_DUAL: {} candidates", cands.len());

    std::env::set_var("DIALOG_TAIL_NONCE", "none");
    let base_ops = build();
    let full_len = base_ops.len() as u64 + 2 * NONCE_BITS;
    let mut pk = keccak::Shake256::new();
    pk.absorb(b"quantum_ecc-fiat-shamir-v2");
    pk.absorb(&full_len.to_le_bytes());
    for op in &base_ops {
        pk.absorb(&[op.kind as u8]);
        pk.absorb(&op.q_control2.0.to_le_bytes());
        pk.absorb(&op.q_control1.0.to_le_bytes());
        pk.absorb(&op.q_target.0.to_le_bytes());
        pk.absorb(&op.c_target.0.to_le_bytes());
        pk.absorb(&op.c_condition.0.to_le_bytes());
        pk.absorb(&op.r_target.0.to_le_bytes());
    }
    let (st0, buf0, pos0) = (pk.st.to_vec(), pk.buf.to_vec(), pk.pos as i32);
    let base_len = base_ops.len() as u64;
    drop(base_ops);
    let tbl = gtable::build_gtable(&gtable::curve());

    let meta = read_u64s(&format!("{cdump}/meta.bin"));
    let n_ops = meta[2];
    // Guard against a stale dump: the GPU op-stream MUST be the same circuit the
    // phase hash above was computed from (both built with DIALOG_TAIL_NONCE=none),
    // else the SHAKE256 phase XOF misaligns and phase verdicts are garbage
    // (this is exactly what produced the 1170q/10288316-op stale-dump false-clean).
    assert!(
        n_ops == base_len,
        "STALE DUMP: {cdump} n_ops={n_ops} != build() base_ops={base_len}; \
         rerun circuit_prep (PHASE_CDUMP={cdump}) at the current frontier"
    );
    let ops_blob = std::fs::read(format!("{cdump}/ops2.bin")).unwrap()[8..].to_vec();
    let reg_q = read_u32s(&format!("{cdump}/reg_q.bin"));
    let reg_s = read_u32s(&format!("{cdump}/reg_s.bin"));
    let bs: u32 = std::env::var("HUNT_BS").ok().and_then(|s| s.parse().ok()).unwrap_or(64);

    let ptx = match compile_ptx(SRC) {
        Ok(p) => p,
        Err(e) => { eprintln!("NVRTC ERR\n{:?}", e); std::process::exit(1); }
    };
    let ctx = CudaContext::new(0).unwrap();
    let stream = ctx.default_stream();
    let m = ctx.load_module(ptx).unwrap();
    let f = m.load_function("hunt_phase").unwrap();
    let dst0 = stream.memcpy_stod(&st0).unwrap();
    let dbuf = stream.memcpy_stod(&buf0).unwrap();
    let dops = stream.memcpy_stod(&ops_blob).unwrap();
    let drq = stream.memcpy_stod(&reg_q).unwrap();
    let drs = stream.memcpy_stod(&reg_s).unwrap();
    let dtbl = stream.memcpy_stod(&tbl).unwrap();
    let (nb, xk) = (NONCE_BITS as i32, 6i32);
    let nstart = 0u64;
    let (mut dual, mut cls, mut ph) = (0u64, 0u64, 0u64);
    // phase near-miss tracking: among classical-clean (phase-fail) candidates, how far
    // into the 141 phase batches did they get before failing? higher = closer to dual-clean.
    let mut max_ph_fb: i32 = -1;
    let mut max_ph_nonce: u64 = 0;
    let mut nearmiss_ge130: u64 = 0;
    let mut nearmiss_ge138: u64 = 0;
    let t0 = std::time::Instant::now();
    for chunk in cands.chunks(4096) {
        let nl = chunk.to_vec();
        let bn = nl.len() as i32;
        let dnl = stream.memcpy_stod(&nl).unwrap();
        let mut dv = stream.alloc_zeros::<i8>(bn as usize).unwrap();
        let mut dfb = stream.alloc_zeros::<i32>(bn as usize).unwrap();
        let cfg = LaunchConfig { grid_dim: (((bn as u32) + bs - 1) / bs, 1, 1), block_dim: (bs, 1, 1), shared_mem_bytes: 0 };
        {
            let mut lb = stream.launch_builder(&f);
            lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&nstart).arg(&bn)
                .arg(&dops).arg(&n_ops).arg(&drq).arg(&drs).arg(&dtbl).arg(&mut dv).arg(&mut dfb).arg(&dnl);
            unsafe { lb.launch(cfg).unwrap(); }
        }
        let v = stream.memcpy_dtov(&dv).unwrap();
        let fb = stream.memcpy_dtov(&dfb).unwrap();
        for i in 0..bn as usize {
            match v[i] {
                0 => { dual += 1; println!("DUAL_CLEAN_CANDIDATE nonce={}", nl[i]); }
                1 => cls += 1,
                2 => {
                    ph += 1;
                    let b = fb[i];
                    if b > max_ph_fb { max_ph_fb = b; max_ph_nonce = nl[i]; }
                    if b >= 130 { nearmiss_ge130 += 1; }
                    if b >= 138 { nearmiss_ge138 += 1; }
                }
                _ => {}
            }
        }
    }
    eprintln!(
        "PHASE NEAR-MISS: max failbatch={}/141 (nonce={}) ; phase-fails reaching >=130: {} ; >=138: {}",
        max_ph_fb, max_ph_nonce, nearmiss_ge130, nearmiss_ge138
    );
    eprintln!(
        "VERIFY DONE in {:.1}s: {} cands -> dual-clean={} classical={} phase={}",
        t0.elapsed().as_secs_f64(), cands.len(), dual, cls, ph
    );
}
