// align_scan: scan [START, START+COUNT) with gcd.cu `hunt` count-mode kernel.
// Emits per-nonce analytical hard-count and the gcd-CLEAN nonce set (hard==0).
// Used to compare gcd.cu's classical-clean set vs the op-loop ground truth.
use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use alloy_primitives::U256;
use quantum_ecc::point_add::build;
use quantum_ecc::point_add::dialog_gcd_classical_filter::DialogGcdFilterConfig;
#[path="../keccak.rs"] mod keccak;
#[path="../gtable.rs"] mod gtable;

const SRC: &str = concat!(include_str!("../field.cuh"),"\n",include_str!("../points.cu"),"\n",include_str!("../gcd.cu"),"\n",include_str!("../keccak.cu"),"\n",include_str!("../hunt.cu"));
const NONCE_BITS: u64 = 48;
const TARGET_SHOTS: usize = 9024;
fn envu(k:&str,d:u64)->u64{ std::env::var(k).ok().and_then(|s|s.parse().ok()).unwrap_or(d) }

fn main(){
    let start = envu("START",1);
    let count = envu("COUNT",4096) as usize;
    std::env::set_var("DIALOG_TAIL_NONCE","none");
    std::env::set_var("DIALOG_GCD_FILTER_ACCEPT_U1_TERMINAL","1");
    let base_ops = build();
    let fcfg = DialogGcdFilterConfig::from_env();
    let iters=fcfg.active_iterations;
    let (mut aw,mut cb,mut bw)=(vec![0i32;iters],vec![0i32;iters],vec![0i32;iters]);
    for s in 0..iters{let a=fcfg.active_width(s);aw[s]=a as i32;cb[s]=fcfg.compare_bits_for_step(s,a) as i32;bw[s]=fcfg.body_carry_trunc_width(a,s) as i32;}
    eprintln!("iters={} k2={} odd_u={} compare_bits={}",iters,fcfg.k2,fcfg.odd_u_lowbit_fastpath,fcfg.compare_bits);

    let cv=gtable::curve();
    let ctx=CudaContext::new(0).unwrap(); let stream=ctx.default_stream();
    let ptx=match compile_ptx(SRC){Ok(p)=>p,Err(e)=>{eprintln!("NVRTC ERR\n{:?}",e);std::process::exit(1);}};
    let m=ctx.load_module(ptx).unwrap();
    let tbl=gtable::build_gtable(&cv); let dtbl=stream.memcpy_stod(&tbl).unwrap();
    let full_len = base_ops.len() as u64 + 2*NONCE_BITS;
    let mut pk = keccak::Shake256::new();
    pk.absorb(b"quantum_ecc-fiat-shamir-v2"); pk.absorb(&full_len.to_le_bytes());
    for op in &base_ops {
        pk.absorb(&[op.kind as u8]);
        pk.absorb(&op.q_control2.0.to_le_bytes()); pk.absorb(&op.q_control1.0.to_le_bytes());
        pk.absorb(&op.q_target.0.to_le_bytes()); pk.absorb(&op.c_target.0.to_le_bytes());
        pk.absorb(&op.c_condition.0.to_le_bytes()); pk.absorb(&op.r_target.0.to_le_bytes());
    }
    let st0=pk.st; let buf0=pk.buf; let pos0=pk.pos as i32;
    let _=U256::ZERO;

    let f=m.load_function("hunt").unwrap();
    let dst0=stream.memcpy_stod(&st0.to_vec()).unwrap();
    let dbuf=stream.memcpy_stod(&buf0.to_vec()).unwrap();
    let daw=stream.memcpy_stod(&aw).unwrap(); let dcb=stream.memcpy_stod(&cb).unwrap(); let dbw=stream.memcpy_stod(&bw).unwrap();
    let mut dout=stream.alloc_zeros::<i32>(count).unwrap();
    let bs=64u32; let cfg=LaunchConfig{grid_dim:(((count as u32)+bs-1)/bs,1,1),block_dim:(bs,1,1),shared_mem_bytes:0};
    let (nb,xk,it,k2,odd,nstart_i,ni,ts,cm)=(NONCE_BITS as i32,6i32,iters as i32,if fcfg.k2{1i32}else{0},if fcfg.odd_u_lowbit_fastpath{1i32}else{0},start,count as i32,TARGET_SHOTS as i32,1i32);
    let t0=std::time::Instant::now();
    let mut lb=stream.launch_builder(&f);
    lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&daw).arg(&dcb).arg(&dbw).arg(&it).arg(&k2).arg(&odd).arg(&nstart_i).arg(&ni).arg(&ts).arg(&cm).arg(&dtbl).arg(&mut dout);
    unsafe{lb.launch(cfg).unwrap();}
    stream.synchronize().unwrap();
    let dt=t0.elapsed();
    let g=stream.memcpy_dtov(&dout).unwrap();
    let mut clean=Vec::new();
    let mut sum=0i64;
    for i in 0..count { let nonce=start+i as u64; sum+=g[i] as i64; if g[i]==0 { clean.push(nonce); } }
    eprintln!("scanned={} time={:?} ({:.0} nonce/s)",count,dt,count as f64/dt.as_secs_f64());
    eprintln!("gcd mean hard/nonce = {:.3}",sum as f64/count as f64);
    eprintln!("gcd-CLEAN count = {} ({:.4}%)",clean.len(),100.0*clean.len() as f64/count as f64);
    // dump full per-nonce hard to stdout: "nonce hard"
    let path=std::env::var("OUT").unwrap_or("/tmp/gcd_scan.txt".into());
    let mut s=String::new();
    for i in 0..count { s.push_str(&format!("{} {}\n", start+i as u64, g[i])); }
    std::fs::write(&path,s).unwrap();
    eprintln!("wrote per-nonce hard -> {}",path);
    eprintln!("gcd-clean nonces (<=80): {:?}", &clean[..clean.len().min(80)]);
}
