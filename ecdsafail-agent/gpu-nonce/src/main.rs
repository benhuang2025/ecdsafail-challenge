// Multi-GPU clean-nonce hunt. env: SQUARE_ROW_MAX_SEG, HUNT_START, HUNT_COUNT, HUNT_BATCH, HUNT_GPUS.
use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use quantum_ecc::point_add::build;
use quantum_ecc::point_add::dialog_gcd_classical_filter::DialogGcdFilterConfig;
use std::sync::{Arc, Mutex};
mod keccak;
mod gtable;

const SRC: &str = concat!(include_str!("field.cuh"),"\n",include_str!("points.cu"),"\n",include_str!("gcd.cu"),"\n",include_str!("keccak.cu"),"\n",include_str!("hunt.cu"));
const NONCE_BITS: u64 = 48;
const TARGET_SHOTS: usize = 9024;
fn envu(k:&str,d:u64)->u64{ std::env::var(k).ok().and_then(|s|s.parse().ok()).unwrap_or(d) }

fn main(){
    std::env::set_var("DIALOG_TAIL_NONCE","none");
    std::env::set_var("DIALOG_GCD_FILTER_ACCEPT_U1_TERMINAL","1");
    eprintln!("build() (MAX_SEG={:?}) ...", std::env::var("SQUARE_ROW_MAX_SEG").ok());
    let base_ops=build();
    let fcfg=DialogGcdFilterConfig::from_env();
    let iters=fcfg.active_iterations;
    let (mut aw,mut cb,mut bw)=(vec![0i32;iters],vec![0i32;iters],vec![0i32;iters]);
    for s in 0..iters{let a=fcfg.active_width(s);aw[s]=a as i32;cb[s]=fcfg.compare_bits_for_step(s,a) as i32;bw[s]=fcfg.body_carry_trunc_width(a,s) as i32;}
    let full_len=base_ops.len() as u64+2*NONCE_BITS;
    eprintln!("base_len={} full_len={} iters={} compare_bits={}", base_ops.len(), full_len, iters, fcfg.compare_bits);
    let mut pk=keccak::Shake256::new();
    pk.absorb(b"quantum_ecc-fiat-shamir-v2"); pk.absorb(&full_len.to_le_bytes());
    for op in &base_ops { pk.absorb(&[op.kind as u8]);
        pk.absorb(&op.q_control2.0.to_le_bytes()); pk.absorb(&op.q_control1.0.to_le_bytes());
        pk.absorb(&op.q_target.0.to_le_bytes()); pk.absorb(&op.c_target.0.to_le_bytes());
        pk.absorb(&op.c_condition.0.to_le_bytes()); pk.absorb(&op.r_target.0.to_le_bytes()); }
    let (st0,buf0,pos0)=(pk.st.to_vec(),pk.buf.to_vec(),pk.pos as i32);
    drop(base_ops);
    let tbl=gtable::build_gtable(&gtable::curve()); eprintln!("gtable built ({} u64)",tbl.len());

    let (k2,odd)=(if fcfg.k2{1i32}else{0},if fcfg.odd_u_lowbit_fastpath{1i32}else{0});
    let start=envu("HUNT_START",1); let count=envu("HUNT_COUNT",8_000_000); let batch=envu("HUNT_BATCH",65536);
    let ngpu=envu("HUNT_GPUS",8) as usize;
    eprintln!("HUNT start={} count={} batch={} gpus={}",start,count,batch,ngpu);
    let ptx=match compile_ptx(SRC){Ok(p)=>p,Err(e)=>{eprintln!("NVRTC ERR\n{:?}",e);std::process::exit(1);}};
    let found=Arc::new(Mutex::new(Vec::<u64>::new()));
    let scanned=Arc::new(std::sync::atomic::AtomicU64::new(0));
    let t0=std::time::Instant::now();
    let per=(count+ngpu as u64-1)/ngpu as u64;
    std::thread::scope(|scope|{
        for dev in 0..ngpu {
            let (st0,buf0,aw,cb,bw,ptx,found,scanned,tbl)=(st0.clone(),buf0.clone(),aw.clone(),cb.clone(),bw.clone(),ptx.clone(),found.clone(),scanned.clone(),tbl.clone());
            scope.spawn(move ||{
                let ctx=match CudaContext::new(dev){Ok(c)=>c,Err(_)=>{eprintln!("gpu {} unavailable",dev);return;}};
                let stream=ctx.default_stream(); let m=ctx.load_module(ptx).unwrap(); let f=m.load_function("hunt").unwrap();
                let dst0=stream.memcpy_stod(&st0).unwrap(); let dbuf=stream.memcpy_stod(&buf0).unwrap();
                let daw=stream.memcpy_stod(&aw).unwrap(); let dcb=stream.memcpy_stod(&cb).unwrap(); let dbw=stream.memcpy_stod(&bw).unwrap();
                let dtbl=stream.memcpy_stod(&tbl).unwrap();
                let (nb,xk,it,ts,cm)=(NONCE_BITS as i32,6i32,iters as i32,TARGET_SHOTS as i32,0i32);
                let dstart=start+dev as u64*per; let dend=(dstart+per).min(start+count);
                let mut n=dstart;
                while n<dend {
                    let bn=(batch.min(dend-n)) as usize;
                    let mut dout=stream.alloc_zeros::<i32>(bn).unwrap();
                    let bs=std::env::var("HUNT_BS").ok().and_then(|v|v.parse().ok()).unwrap_or(32u32); let cfg=LaunchConfig{grid_dim:(((bn as u32)+bs-1)/bs,1,1),block_dim:(bs,1,1),shared_mem_bytes:0};
                    let ni=bn as i32;
                    let mut lb=stream.launch_builder(&f);
                    lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&daw).arg(&dcb).arg(&dbw).arg(&it).arg(&k2).arg(&odd).arg(&n).arg(&ni).arg(&ts).arg(&cm).arg(&dtbl).arg(&mut dout);
                    unsafe{lb.launch(cfg).unwrap();}
                    let g=stream.memcpy_dtov(&dout).unwrap();
                    for i in 0..bn { if g[i]==0 { let nonce=n+i as u64; println!("CLEAN_CANDIDATE nonce={}",nonce); found.lock().unwrap().push(nonce); } }
                    let tot=scanned.fetch_add(bn as u64,std::sync::atomic::Ordering::Relaxed)+bn as u64;
                    if dev==0 { let el=t0.elapsed().as_secs_f64(); eprintln!("progress ~{}/{}  ({:.0} nonce/s total)  candidates={}",tot,count,tot as f64/el,found.lock().unwrap().len()); }
                    n+=bn as u64;
                }
            });
        }
    });
    let f=found.lock().unwrap();
    eprintln!("DONE scanned={} candidates={} list={:?}",count,f.len(),*f);
}
