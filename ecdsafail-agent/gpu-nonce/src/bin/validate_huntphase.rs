// M3 de-risk step 2: end-to-end hunt_phase classification vs CPU eval_circuit.
//
// Loads /tmp/phase_circuit (nonce-independent ops2/reg_q/reg_s/meta), builds the FS
// prefix like main.rs, runs hunt_phase over a list of test nonces, prints each
// verdict. Expected verdicts (from phase_ref's FULL-RUN VERDICT) are passed via
// the EXPECT env (comma list of nonce:verdict, verdict in {clean,classical,phase}).
//
// usage: NONCES=n1,n2,... [EXPECT=n1:clean,n2:phase,...] validate_huntphase

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

fn read_u64s(p: &str) -> Vec<u64> {
    std::fs::read(p).unwrap().chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}
fn read_u32s(p: &str) -> Vec<u32> {
    std::fs::read(p).unwrap().chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn main() {
    let nonces: Vec<u64> = std::env::var("NONCES").unwrap()
        .split(',').map(|s| s.trim().parse().unwrap()).collect();
    let expect: std::collections::HashMap<u64,&'static str> = std::env::var("EXPECT").ok().map(|e| {
        e.split(',').map(|kv| {
            let mut it = kv.split(':');
            let n: u64 = it.next().unwrap().trim().parse().unwrap();
            let v = match it.next().unwrap().trim() { "clean"=>"clean","classical"=>"classical","phase"=>"phase", x=>panic!("bad expect {x}") };
            (n, v)
        }).collect()
    }).unwrap_or_default();

    // FS prefix like main.rs
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
    eprintln!("circuit: num_qubits={} num_slots={} n_ops={}", num_qubits, num_slots, n_ops);
    let ops_raw = std::fs::read(format!("{CDUMP}/ops2.bin")).unwrap();
    assert_eq!(u64::from_le_bytes(ops_raw[0..8].try_into().unwrap()), n_ops);
    let ops_blob = ops_raw[8..].to_vec();
    assert_eq!(ops_blob.len() as u64, n_ops*24);
    let reg_q = read_u32s(&format!("{CDUMP}/reg_q.bin")); assert_eq!(reg_q.len(), 512);
    let reg_s = read_u32s(&format!("{CDUMP}/reg_s.bin")); assert_eq!(reg_s.len(), 512);

    let ctx = CudaContext::new(0).unwrap();
    let stream = ctx.default_stream();
    eprintln!("compiling hunt_phase.cu (NQ={} NS={}) ...", num_qubits, num_slots);
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

    // one nonce per launch (contiguous nonce_start; n=1) so we can mix arbitrary nonces.
    let mut all_ok = true;
    for &nonce in &nonces {
        let n = 1i32;
        let mut dv = stream.alloc_zeros::<i8>(1).unwrap();
        let mut dfb = stream.alloc_zeros::<i32>(1).unwrap();
        let cfg = LaunchConfig{ grid_dim:(1,1,1), block_dim:(1,1,1), shared_mem_bytes:0 };
        let t0 = std::time::Instant::now();
        {
            let mut lb = stream.launch_builder(&f);
            lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&nonce).arg(&n)
              .arg(&dops).arg(&n_ops).arg(&drq).arg(&drs).arg(&dtbl)
              .arg(&mut dv).arg(&mut dfb);
            unsafe { lb.launch(cfg).unwrap(); }
        }
        stream.synchronize().unwrap();
        let dt = t0.elapsed();
        let v = stream.memcpy_dtov(&dv).unwrap()[0];
        let fb = stream.memcpy_dtov(&dfb).unwrap()[0];
        let vs = match v { 0=>"clean", 1=>"classical", 2=>"phase", _=>"?" };
        let exp = expect.get(&nonce).copied();
        let ok = exp.map(|e| e == vs).unwrap_or(true);
        if !ok { all_ok = false; }
        let mark = match exp { Some(e) if e==vs => "OK", Some(e)=>{ eprintln!("  EXPECTED {e}"); "MISMATCH" }, None=>"(no-expect)" };
        println!("nonce={} verdict={} failbatch={} time={:?}  {}", nonce, vs, fb, dt, mark);
    }
    if all_ok { println!("ALL MATCH"); } else { println!("MISMATCH PRESENT"); std::process::exit(1); }
}
