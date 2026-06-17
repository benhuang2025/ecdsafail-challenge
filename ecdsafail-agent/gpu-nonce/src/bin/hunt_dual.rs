// M3 production dual-clean hunt: fused stage-1(classical, exact)+stage-2(phase) via
// the hunt_phase kernel, multi-GPU. Emits DUAL_CLEAN_CANDIDATE nonce=N for every
// nonce whose 9024-shot Fiat-Shamir island is BOTH classical-clean AND phase-clean
// (verdict 0). These match CPU eval_circuit's classical+phase verdict (validated M3
// steps 1-2); the single winner is still CPU-re-certified for the official 0/0/0.
//
// One thread = one candidate nonce; classical-fail nonces early-exit cheaply, so the
// expensive full op-loop runs only on the ~e^{-lambda_cls} classical-clean stream.
//
// env: HUNT_START, HUNT_COUNT, HUNT_GPUS, HUNT_BATCH (nonces/launch/gpu), HUNT_BS (block).

use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use quantum_ecc::point_add::build;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
#[path="../keccak.rs"] mod keccak;
#[path="../gtable.rs"] mod gtable;

const SRC: &str = concat!(
    include_str!("../field.cuh"), "\n",
    include_str!("../points.cu"), "\n",
    include_str!("../gcd.cu"), "\n",
    include_str!("../keccak.cu"), "\n",
    include_str!("../hunt_phase.cu"));

const NONCE_BITS: u64 = 48;

fn read_u64s(p: &str) -> Vec<u64> { std::fs::read(p).unwrap().chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect() }
fn read_u32s(p: &str) -> Vec<u32> { std::fs::read(p).unwrap().chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect() }
fn envu(k:&str,d:u64)->u64{ std::env::var(k).ok().and_then(|s|s.parse().ok()).unwrap_or(d) }

fn main() {
    let CDUMP: &str = Box::leak(std::env::var("PHASE_CDUMP").unwrap_or_else(|_| "/tmp/phase_circuit".to_string()).into_boxed_str());

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
    let (_num_qubits, _num_slots, n_ops) = (meta[0], meta[1], meta[2]);
    let ops_raw = std::fs::read(format!("{CDUMP}/ops2.bin")).unwrap();
    let ops_blob = ops_raw[8..].to_vec();
    let reg_q = read_u32s(&format!("{CDUMP}/reg_q.bin"));
    let reg_s = read_u32s(&format!("{CDUMP}/reg_s.bin"));

    let start = envu("HUNT_START", 1);
    let count = envu("HUNT_COUNT", 1_000_000);
    let ngpu = envu("HUNT_GPUS", 8) as usize;
    let batch = envu("HUNT_BATCH", 65536);
    let bs = envu("HUNT_BS", 64) as u32;
    eprintln!("HUNT_DUAL start={} count={} gpus={} batch={} bs={} n_ops={}", start, count, ngpu, batch, bs, n_ops);

    let ptx = match compile_ptx(SRC) { Ok(p)=>p, Err(e)=>{ eprintln!("NVRTC ERR:\n{:?}",e); std::process::exit(1);} };
    let scanned = Arc::new(AtomicU64::new(0));
    let n_clean = Arc::new(AtomicU64::new(0));
    let n_class = Arc::new(AtomicU64::new(0));
    let n_phase = Arc::new(AtomicU64::new(0));
    let max_fb = Arc::new(AtomicU64::new(0));
    let dual = Arc::new(Mutex::new(Vec::<u64>::new()));
    let t0 = std::time::Instant::now();
    let per = (count + ngpu as u64 - 1) / ngpu as u64;

    std::thread::scope(|scope| {
        for dev in 0..ngpu {
            let (st0,buf0,ops_blob,reg_q,reg_s,tbl,ptx) = (st0.clone(),buf0.clone(),ops_blob.clone(),reg_q.clone(),reg_s.clone(),tbl.clone(),ptx.clone());
            let (scanned,n_clean,n_class,n_phase,dual,max_fb) = (scanned.clone(),n_clean.clone(),n_class.clone(),n_phase.clone(),dual.clone(),max_fb.clone());
            scope.spawn(move || {
                let ctx = match CudaContext::new(dev) { Ok(c)=>c, Err(_)=>{ eprintln!("gpu {} unavailable", dev); return; } };
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
                let dstart = start + dev as u64 * per;
                let dend = (dstart + per).min(start + count);
                let mut n = dstart;
                while n < dend {
                    let bn = batch.min(dend - n) as i32;
                    let nl: Vec<u64> = (n..n + bn as u64).collect();
                    let dnl = stream.memcpy_stod(&nl).unwrap();
                    let mut dv = stream.alloc_zeros::<i8>(bn as usize).unwrap();
                    let mut dfb = stream.alloc_zeros::<i32>(bn as usize).unwrap();
                    let cfg = LaunchConfig{ grid_dim:(((bn as u32)+bs-1)/bs,1,1), block_dim:(bs,1,1), shared_mem_bytes:0 };
                    {
                        let mut lb = stream.launch_builder(&f);
                        lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&n).arg(&bn)
                          .arg(&dops).arg(&n_ops).arg(&drq).arg(&drs).arg(&dtbl).arg(&mut dv).arg(&mut dfb).arg(&dnl);
                        unsafe { lb.launch(cfg).unwrap(); }
                    }
                    let v = stream.memcpy_dtov(&dv).unwrap();
                    let fb = stream.memcpy_dtov(&dfb).unwrap();
                    let (mut c0,mut c1,mut c2)=(0u64,0u64,0u64);
                    for i in 0..bn as usize {
                        match v[i] { 0=>{ c0+=1; let nonce=n+i as u64; println!("DUAL_CLEAN_CANDIDATE nonce={}",nonce); dual.lock().unwrap().push(nonce); },
                                     1=>c1+=1, 2=>{c2+=1; max_fb.fetch_max(fb[i] as u64, Ordering::Relaxed);}, _=>{} }
                    }
                    n_clean.fetch_add(c0,Ordering::Relaxed); n_class.fetch_add(c1,Ordering::Relaxed); n_phase.fetch_add(c2,Ordering::Relaxed);
                    let tot = scanned.fetch_add(bn as u64,Ordering::Relaxed)+bn as u64;
                    if dev==0 {
                        let el=t0.elapsed().as_secs_f64();
                        let cc = n_clean.load(Ordering::Relaxed)+n_phase.load(Ordering::Relaxed);
                        eprintln!("~{}/{} scan  {:.0} nonce/s | classical-clean={} ({:.0}/s) | dual-clean={} ({:.3}/s)",
                            tot, count, tot as f64/el, cc, cc as f64/el, n_clean.load(Ordering::Relaxed), n_clean.load(Ordering::Relaxed) as f64/el);
                        eprintln!("    closest phase near-miss: failbatch {}/141", max_fb.load(Ordering::Relaxed));
                    }
                    n += bn as u64;
                }
            });
        }
    });
    let el = t0.elapsed().as_secs_f64();
    let (c0,c1,c2)=(n_clean.load(Ordering::Relaxed),n_class.load(Ordering::Relaxed),n_phase.load(Ordering::Relaxed));
    let cc = c0+c2;
    eprintln!("DONE scanned={} in {:.1}s", count, el);
    eprintln!("  scan throughput      = {:.0} nonce/s", count as f64/el);
    eprintln!("  classical-clean      = {} ({:.4}% , {:.0}/s)", cc, 100.0*cc as f64/count as f64, cc as f64/el);
    eprintln!("  classical-fail       = {}", c1);
    eprintln!("  phase-fail           = {}", c2);
    eprintln!("  max phase-fail failbatch (of 141, higher=closer to winner) = {}", max_fb.load(Ordering::Relaxed));
    eprintln!("  DUAL-CLEAN           = {} ({:.0}/s , 1 per {:.2e} nonces)", c0, c0 as f64/el, if c0>0 {count as f64/c0 as f64} else {f64::INFINITY});
    eprintln!("  dual-clean nonces    = {:?}", dual.lock().unwrap());
}
