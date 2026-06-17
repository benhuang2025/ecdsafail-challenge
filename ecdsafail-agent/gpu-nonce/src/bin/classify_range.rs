// M3: run hunt_phase over a contiguous nonce range, n threads in parallel.
// Reports per-verdict counts and lists clean / phase-fail nonces (the interesting ones).
// This is the fused stage-1(classical, re-derived exactly)+stage-2(phase) pipeline:
// classical-fail nonces early-exit cheaply; clean/phase-fail cost the full op-loop.
//
// env: START, COUNT, BS (block size), GPU (CUDA device via CUDA_VISIBLE_DEVICES outside)

use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use quantum_ecc::point_add::build;
#[path="../keccak.rs"] mod keccak;
#[path="../gtable.rs"] mod gtable;

const SRC: &str = concat!(
    include_str!("../field.cuh"), "\n",
    include_str!("../points.cu"), "\n",
    include_str!("../gcd.cu"), "\n",
    include_str!("../keccak.cu"), "\n",
    include_str!("../hunt_phase.cu"));

const NONCE_BITS: u64 = 48;
const CDUMP: &str = "/tmp/phase_circuit";

fn read_u64s(p: &str) -> Vec<u64> { std::fs::read(p).unwrap().chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect() }
fn read_u32s(p: &str) -> Vec<u32> { std::fs::read(p).unwrap().chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect() }
fn envu(k:&str,d:u64)->u64{ std::env::var(k).ok().and_then(|s|s.parse().ok()).unwrap_or(d) }

fn main() {
    let start = envu("START", 1);
    let count = envu("COUNT", 100000) as i32;
    let bs = envu("BS", 64) as u32;

    std::env::set_var("DIALOG_TAIL_NONCE", "none");
    let base_ops = build();
    let full_len = base_ops.len() as u64 + 2*NONCE_BITS;
    let mut pk = keccak::Shake256::new();
    pk.absorb(b"quantum_ecc-fiat-shamir-v2"); pk.absorb(&full_len.to_le_bytes());
    for op in &base_ops {
        pk.absorb(&[op.kind as u8]);
        pk.absorb(&op.q_control2.0.to_le_bytes()); pk.absorb(&op.q_control1.0.to_le_bytes());
        pk.absorb(&op.q_target.0.to_le_bytes()); pk.absorb(&op.c_target.0.to_le_bytes());
        pk.absorb(&op.c_condition.0.to_le_bytes()); pk.absorb(&op.r_target.0.to_le_bytes());
    }
    let (st0, buf0, pos0) = (pk.st.to_vec(), pk.buf.to_vec(), pk.pos as i32);
    drop(base_ops);
    let tbl = gtable::build_gtable(&gtable::curve());

    let meta = read_u64s(&format!("{CDUMP}/meta.bin"));
    let (num_qubits, num_slots, n_ops) = (meta[0], meta[1], meta[2]);
    let ops_raw = std::fs::read(format!("{CDUMP}/ops2.bin")).unwrap();
    let ops_blob = ops_raw[8..].to_vec();
    let reg_q = read_u32s(&format!("{CDUMP}/reg_q.bin"));
    let reg_s = read_u32s(&format!("{CDUMP}/reg_s.bin"));
    eprintln!("circuit num_qubits={} num_slots={} n_ops={}; START={} COUNT={} BS={}", num_qubits, num_slots, n_ops, start, count, bs);

    let ctx = CudaContext::new(0).unwrap();
    let stream = ctx.default_stream();
    let ptx = match compile_ptx(SRC) { Ok(p)=>p, Err(e)=>{ eprintln!("NVRTC ERR:\n{:?}",e); std::process::exit(1);} };
    let m = ctx.load_module(ptx).unwrap();
    let f = m.load_function("hunt_phase").unwrap();

    let dst0 = stream.memcpy_stod(&st0).unwrap();
    let dbuf = stream.memcpy_stod(&buf0).unwrap();
    let dops = stream.memcpy_stod(&ops_blob).unwrap();
    let drq = stream.memcpy_stod(&reg_q).unwrap();
    let drs = stream.memcpy_stod(&reg_s).unwrap();
    let dtbl = stream.memcpy_stod(&tbl).unwrap();
    let (nb, xk) = (NONCE_BITS as i32, 6i32);

    let mut dv = stream.alloc_zeros::<i8>(count as usize).unwrap();
    let mut dfb = stream.alloc_zeros::<i32>(count as usize).unwrap();
    let nonce_start = start;
    let cfg = LaunchConfig{ grid_dim:(((count as u32)+bs-1)/bs,1,1), block_dim:(bs,1,1), shared_mem_bytes:0 };
    let t0 = std::time::Instant::now();
    {
        let mut lb = stream.launch_builder(&f);
        lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&nonce_start).arg(&count)
          .arg(&dops).arg(&n_ops).arg(&drq).arg(&drs).arg(&dtbl).arg(&mut dv).arg(&mut dfb);
        unsafe { lb.launch(cfg).unwrap(); }
    }
    stream.synchronize().unwrap();
    let dt = t0.elapsed();
    let v = stream.memcpy_dtov(&dv).unwrap();
    let fb = stream.memcpy_dtov(&dfb).unwrap();

    let (mut nclean, mut nclass, mut nphase) = (0u64,0u64,0u64);
    let mut clean_list = Vec::new();
    let mut phase_list = Vec::new();
    for i in 0..count as usize {
        let nonce = start + i as u64;
        match v[i] { 0 => { nclean+=1; if clean_list.len()<50 { clean_list.push(nonce); } },
                     1 => nclass+=1,
                     2 => { nphase+=1; if phase_list.len()<50 { phase_list.push((nonce, fb[i])); } },
                     _ => {} }
    }
    println!("scanned={} time={:?} ({:.0} nonce/s)", count, dt, count as f64/dt.as_secs_f64());
    println!("clean(dual)={}  classical-fail={}  phase-fail={}", nclean, nclass, nphase);
    println!("classical-clean (clean+phase) = {} ({:.4}%)", nclean+nphase, 100.0*(nclean+nphase) as f64/count as f64);
    println!("CLEAN nonces (<=50): {:?}", clean_list);
    println!("PHASE-FAIL nonces (nonce,failbatch) (<=50): {:?}", phase_list);
}
