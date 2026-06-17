use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use alloy_primitives::U256;
use quantum_ecc::weierstrass_elliptic_curve::WeierstrassEllipticCurve;
use quantum_ecc::point_add::build;
use quantum_ecc::point_add::dialog_gcd_classical_filter::{check_gcd_factor, point_add_gcd_factors, DialogGcdFilterConfig};
use quantum_ecc::circuit::OperationType;
use sha3::{Shake256, digest::{Update, ExtendableOutput, XofReader}};
#[path="../keccak.rs"] mod keccak;
#[path="../gtable.rs"] mod gtable;

const SRC: &str = concat!(include_str!("../field.cuh"),"\n",include_str!("../points.cu"),"\n",include_str!("../gcd.cu"),"\n",include_str!("../keccak.cu"),"\n",include_str!("../hunt.cu"));
const NONCE_BITS: u64 = 48;
const TARGET_SHOTS: usize = 9024;

fn curve() -> WeierstrassEllipticCurve { WeierstrassEllipticCurve{
    modulus:U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F",16).unwrap(),
    a:U256::from(0u64),b:U256::from(7u64),
    gx:U256::from_str_radix("79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798",16).unwrap(),
    gy:U256::from_str_radix("483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8",16).unwrap(),
    order:U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141",16).unwrap()}}
fn lm(k:&U256)->[u64;4]{*k.as_limbs()}

fn main(){
    let cv=curve();
    std::env::set_var("DIALOG_TAIL_NONCE","none");
    std::env::set_var("DIALOG_GCD_FILTER_ACCEPT_U1_TERMINAL","1");
    eprintln!("build() ...");
    let base_ops = build();
    let fcfg = DialogGcdFilterConfig::from_env();
    let iters=fcfg.active_iterations;
    let (mut aw,mut cb,mut bw)=(vec![0i32;iters],vec![0i32;iters],vec![0i32;iters]);
    for s in 0..iters{let a=fcfg.active_width(s);aw[s]=a as i32;cb[s]=fcfg.compare_bits_for_step(s,a) as i32;bw[s]=fcfg.body_carry_trunc_width(a,s) as i32;}

    let ctx=CudaContext::new(0).unwrap(); let stream=ctx.default_stream();
    let ptx=match compile_ptx(SRC){Ok(p)=>p,Err(e)=>{eprintln!("NVRTC ERR\n{:?}",e);std::process::exit(1);}};
    let m=ctx.load_module(ptx).unwrap();
    let tbl=gtable::build_gtable(&cv); let dtbl=stream.memcpy_stod(&tbl).unwrap();

    // ---- Phase 3: full pipeline validation vs CPU ----
    // host keccak prefix (domain + full_len + base_ops), matching nonce-search
    let full_len = base_ops.len() as u64 + 2*NONCE_BITS;
    let mut pk = keccak::Shake256::new();
    pk.absorb(b"quantum_ecc-fiat-shamir-v2");
    pk.absorb(&full_len.to_le_bytes());
    for op in &base_ops {
        pk.absorb(&[op.kind as u8]);
        pk.absorb(&op.q_control2.0.to_le_bytes()); pk.absorb(&op.q_control1.0.to_le_bytes());
        pk.absorb(&op.q_target.0.to_le_bytes()); pk.absorb(&op.c_target.0.to_le_bytes());
        pk.absorb(&op.c_condition.0.to_le_bytes()); pk.absorb(&op.r_target.0.to_le_bytes());
    }
    let st0=pk.st; let buf0=pk.buf; let pos0=pk.pos as i32;

    // CPU reference: sha3 prefix (absorbing, cloned per nonce)
    let mut sref = Shake256::default();
    Update::update(&mut sref, b"quantum_ecc-fiat-shamir-v2");
    Update::update(&mut sref, &full_len.to_le_bytes());
    for op in &base_ops {
        Update::update(&mut sref,&[op.kind as u8]);
        Update::update(&mut sref,&op.q_control2.0.to_le_bytes()); Update::update(&mut sref,&op.q_control1.0.to_le_bytes());
        Update::update(&mut sref,&op.q_target.0.to_le_bytes()); Update::update(&mut sref,&op.c_target.0.to_le_bytes());
        Update::update(&mut sref,&op.c_condition.0.to_le_bytes()); Update::update(&mut sref,&op.r_target.0.to_le_bytes());
    }
    let cpu_hard = |nonce:u64| -> i32 {
        let mut h = sref.clone();
        for i in 0..NONCE_BITS { let q:u64=if (nonce>>i)&1==1 {1}else{0};
            for _ in 0..2 { Update::update(&mut h,&[OperationType::X as u8]);
                Update::update(&mut h,&u64::MAX.to_le_bytes()); Update::update(&mut h,&u64::MAX.to_le_bytes());
                Update::update(&mut h,&q.to_le_bytes()); Update::update(&mut h,&u64::MAX.to_le_bytes());
                Update::update(&mut h,&u64::MAX.to_le_bytes()); Update::update(&mut h,&u64::MAX.to_le_bytes()); } }
        let mut xof=h.finalize_xof(); let mut rb=[[0u8;32];2]; let mut hard=0i32;
        for _ in 0..TARGET_SHOTS { xof.read(&mut rb[0]); xof.read(&mut rb[1]);
            let k1=U256::from_le_bytes(rb[0]); let k2=U256::from_le_bytes(rb[1]);
            let t=cv.mul(cv.gx,cv.gy,k1); let o=cv.mul(cv.gx,cv.gy,k2);
            if t.0==o.0 || (t.0.is_zero()&&t.1.is_zero()) || (o.0.is_zero()&&o.1.is_zero()) {continue;}
            let e=cv.add(t.0,t.1,o.0,o.1); let (dx,c)=point_add_gcd_factors(t.0,o.0,e.0);
            if check_gcd_factor(dx,&fcfg).is_err()||check_gcd_factor(c,&fcfg).is_err(){hard+=1;} }
        hard
    };

    let nstart=1u64; let n=32usize;
    // GPU hunt count-mode
    let f=m.load_function("hunt").unwrap();
    let dst0=stream.memcpy_stod(&st0.to_vec()).unwrap();
    let dbuf=stream.memcpy_stod(&buf0.to_vec()).unwrap();
    let daw=stream.memcpy_stod(&aw).unwrap(); let dcb=stream.memcpy_stod(&cb).unwrap(); let dbw=stream.memcpy_stod(&bw).unwrap();
    let mut dout=stream.alloc_zeros::<i32>(n).unwrap();
    let bs=32u32; let cfg=LaunchConfig{grid_dim:(((n as u32)+bs-1)/bs,1,1),block_dim:(bs,1,1),shared_mem_bytes:0};
    let (nb,xk,it,k2,odd,nstart_i,ni,ts,cm)=(NONCE_BITS as i32,6i32,iters as i32,if fcfg.k2{1i32}else{0},if fcfg.odd_u_lowbit_fastpath{1i32}else{0},nstart,n as i32,TARGET_SHOTS as i32,1i32);
    let mut lb=stream.launch_builder(&f);
    lb.arg(&dst0).arg(&dbuf).arg(&pos0).arg(&nb).arg(&xk).arg(&daw).arg(&dcb).arg(&dbw).arg(&it).arg(&k2).arg(&odd).arg(&nstart_i).arg(&ni).arg(&ts).arg(&cm).arg(&dtbl).arg(&mut dout);
    unsafe{lb.launch(cfg).unwrap();}
    let g=stream.memcpy_dtov(&dout).unwrap();
    eprintln!("comparing {} nonces (GPU hunt vs CPU, parallel)...",n);
    let cpu_hard_ref = &cpu_hard;
    let mut cpu = vec![0i32; n];
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..n).map(|i| { let nonce=nstart+i as u64; sc.spawn(move || cpu_hard_ref(nonce)) }).collect();
        for (i,h) in hs.into_iter().enumerate() { cpu[i]=h.join().unwrap(); }
    });
    let mut agree=0; for i in 0..n { let nonce=nstart+i as u64; if cpu[i]==g[i]{agree+=1;} else {println!("NONCE {} DISAGREE gpu={} cpu={}",nonce,g[i],cpu[i]);} }
    println!("PHASE3 full pipeline: agree={}/{}",agree,n);
    let gsum:i64=g.iter().map(|&v|v as i64).sum();
    println!("GPU mean hard/nonce over {} = {:.2}", n, gsum as f64/n as f64);
}
