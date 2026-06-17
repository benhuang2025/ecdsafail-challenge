// leverB_diag: per-shot comparison of gcd.cu's classical-hard decision vs the
// op-loop ground truth (phase_ref-style per-shot reg0/reg1 vs expected).
//
// For each nonce N in NONCES (comma sep) or [START, START+COUNT):
//   - build() with DIALOG_TAIL_NONCE=N -> ops (nonce-tail appended, FS island = hunt's)
//   - derive 9024 shots: (t,o,e) and factors dx=Px-Qx, c=Qx-Rx
//   - op-loop -> per-shot classical truth (gx!=exp.0 || gy!=exp.1)
//   - gcd.cu test_gcd kernel on all (dx,c) factors -> per-factor hard
//   - per-shot gcd_hard = dx_hard || c_hard; compare to op truth.
//
// Reports: total shots, op-hard, gcd-hard, FALSE-HARD (gcd hard & op clean),
// FALSE-CLEAN (gcd clean & op hard), enrichment (true-hard rejected / true-hard).
// Also per-nonce: op classical_failures vs gcd hard-count, and whether nonce-level
// clean verdict agrees.
use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use alloy_primitives::U256;
use quantum_ecc::circuit::{analyze_ops, OperationType, QubitOrBit, NO_BIT};
use quantum_ecc::point_add::build;
use quantum_ecc::point_add::dialog_gcd_classical_filter::{
    DialogGcdFilterConfig, DialogApplyFilterConfig, point_add_gcd_factors, check_gcd_factor,
    check_point_add_apply_hazards,
};
use quantum_ecc::weierstrass_elliptic_curve::WeierstrassEllipticCurve;
use sha3::{digest::{ExtendableOutput, Update, XofReader}, Shake256};

const SRC: &str = concat!(
    include_str!("../field.cuh"), "\n",
    include_str!("../points.cu"), "\n",
    include_str!("../gcd.cu"), "\n",
    include_str!("../keccak.cu"), "\n",
    include_str!("../hunt.cu"));
const NUM_TESTS: usize = 9024;
const BATCH: usize = 64;

fn secp() -> WeierstrassEllipticCurve {
    WeierstrassEllipticCurve {
        modulus: U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F",16).unwrap(),
        a: U256::from(0), b: U256::from(7),
        gx: U256::from_str_radix("79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798",16).unwrap(),
        gy: U256::from_str_radix("483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8",16).unwrap(),
        order: U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141",16).unwrap(),
    }
}

fn fs_seed(ops: &[quantum_ecc::circuit::Op]) -> sha3::Shake256Reader {
    let mut h = Shake256::default();
    h.update(b"quantum_ecc-fiat-shamir-v2");
    h.update(&(ops.len() as u64).to_le_bytes());
    for op in ops {
        h.update(&[op.kind as u8]);
        h.update(&op.q_control2.0.to_le_bytes()); h.update(&op.q_control1.0.to_le_bytes());
        h.update(&op.q_target.0.to_le_bytes()); h.update(&op.c_target.0.to_le_bytes());
        h.update(&op.c_condition.0.to_le_bytes()); h.update(&op.r_target.0.to_le_bytes());
    }
    h.finalize_xof()
}

fn main() {
    let nonces: Vec<u64> = if let Ok(s) = std::env::var("NONCES") {
        s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
    } else {
        let start: u64 = std::env::var("START").ok().and_then(|s|s.parse().ok()).unwrap_or(1);
        let count: u64 = std::env::var("COUNT").ok().and_then(|s|s.parse().ok()).unwrap_or(20);
        (start..start+count).collect()
    };
    eprintln!("diag over {} nonces: {:?}", nonces.len(), &nonces[..nonces.len().min(8)]);

    // schedule config (after one build to apply set_default_env)
    std::env::set_var("DIALOG_TAIL_NONCE", "none");
    std::env::set_var("DIALOG_GCD_FILTER_ACCEPT_U1_TERMINAL", "1");
    let _ = build();
    let fcfg = DialogGcdFilterConfig::from_env();
    let iters = fcfg.active_iterations;
    let (mut aw, mut cb, mut bw) = (vec![0i32;iters], vec![0i32;iters], vec![0i32;iters]);
    for s in 0..iters { let a=fcfg.active_width(s); aw[s]=a as i32; cb[s]=fcfg.compare_bits_for_step(s,a) as i32; bw[s]=fcfg.body_carry_trunc_width(a,s) as i32; }
    eprintln!("iters={} k2={} odd_u={} compare_bits={}", iters, fcfg.k2, fcfg.odd_u_lowbit_fastpath, fcfg.compare_bits);

    // GPU setup
    let ctx = CudaContext::new(0).unwrap(); let stream = ctx.default_stream();
    let ptx = match compile_ptx(SRC){Ok(p)=>p,Err(e)=>{eprintln!("NVRTC ERR\n{:?}",e);std::process::exit(1);}};
    let m = ctx.load_module(ptx).unwrap();
    let f = m.load_function("test_gcd").unwrap();
    let daw=stream.memcpy_stod(&aw).unwrap(); let dcb=stream.memcpy_stod(&cb).unwrap(); let dbw=stream.memcpy_stod(&bw).unwrap();
    let (it,k2,odd) = (iters as i32, if fcfg.k2{1i32}else{0}, if fcfg.odd_u_lowbit_fastpath{1i32}else{0});

    let curve = secp();

    // totals
    let mut tot_shots=0usize; let mut tot_ophard=0usize; let mut tot_gcdhard=0usize;
    let mut tot_false_hard=0usize; let mut tot_false_clean=0usize;
    let mut tot_true_hard_rejected=0usize; // op-hard & gcd-hard
    let mut nonce_clean_agree=0usize; let mut nonce_disagree=Vec::new();
    let mut tot_cpu_hard=0usize; let mut tot_cpu_false_hard=0usize; let mut tot_cpu_true_hard=0usize;
    let mut tot_gcd_vs_cpu_underreject=0usize; // cpu hard, gcd clean (port gap)
    let mut tot_gcd_vs_cpu_overreject=0usize;  // gcd hard, cpu clean (gpu stricter than cpu)
    let mut tot_apply_hard=0usize; let mut tot_apply_false_hard=0usize; let mut tot_apply_true_hard=0usize;
    let mut tot_combined_true_hard=0usize; let mut tot_combined_false_hard=0usize;

    for &nonce in &nonces {
        std::env::set_var("DIALOG_TAIL_NONCE", nonce.to_string());
        let ops = build();
        let (num_qubits, num_bits, num_regs, regs) = analyze_ops(ops.iter());
        assert_eq!(num_regs, 4);
        let num_qubits = num_qubits as usize; let num_bits = num_bits as usize;

        // derive shots
        let mut xof = fs_seed(&ops);
        let mut targets=Vec::with_capacity(NUM_TESTS);
        let mut offsets=Vec::with_capacity(NUM_TESTS);
        let mut expected=Vec::with_capacity(NUM_TESTS);
        for _ in 0..NUM_TESTS {
            let mut rb=[[0u8;32];2]; xof.read(&mut rb[0]); xof.read(&mut rb[1]);
            let k1=U256::from_le_bytes(rb[0]); let k2v=U256::from_le_bytes(rb[1]);
            let t=curve.mul(curve.gx,curve.gy,k1); let o=curve.mul(curve.gx,curve.gy,k2v);
            if t.0==o.0 { continue; }
            if t.0.is_zero()&&t.1.is_zero() { continue; }
            if o.0.is_zero()&&o.1.is_zero() { continue; }
            let e=curve.add(t.0,t.1,o.0,o.1);
            targets.push(t); offsets.push(o); expected.push(e);
        }
        let n = targets.len();
        let num_batches = (n + BATCH - 1) / BATCH;

        // factors per shot -> flat array for GPU (2 factors/shot: dx, c)
        let mut facs: Vec<u64> = Vec::with_capacity(n*2*4);
        let mut cpu_hard = vec![false; n];
        let mut apply_hard = vec![false; n];
        let do_cpu = std::env::var("CPU_MODEL").is_ok();
        let do_apply = std::env::var("APPLY_MODEL").is_ok();
        let acfg = DialogApplyFilterConfig::from_env();
        let p = U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F",16).unwrap();
        for i in 0..n {
            let (dx,c) = point_add_gcd_factors(targets[i].0, offsets[i].0, expected[i].0);
            for limb in dx.as_limbs() { facs.push(*limb); }
            for limb in c.as_limbs() { facs.push(*limb); }
            if do_cpu {
                cpu_hard[i] = check_gcd_factor(dx,&fcfg).is_err() || check_gcd_factor(c,&fcfg).is_err();
            }
            if do_apply {
                // lambda = (Oy-Ty)/(Ox-Tx); dy chosen value-consistent so only carry-escape
                // (truncation) hazards fire: dy = lambda*dx mod p.
                let num = offsets[i].1.add_mod(p - targets[i].1, p); // Oy - Ty
                let den = offsets[i].0.add_mod(p - targets[i].0, p); // Ox - Tx
                if let Some(den_inv) = den.inv_mod(p) {
                    let lambda = num.mul_mod(den_inv, p);
                    let dy = lambda.mul_mod(dx, p);
                    apply_hard[i] = check_point_add_apply_hazards(dx, dy, lambda, c, &fcfg, &acfg).is_err();
                } else {
                    apply_hard[i] = false; // degenerate; treat as clean (conservative)
                }
            }
        }
        let nf = (n*2) as i32;
        let dfac = stream.memcpy_stod(&facs).unwrap();
        let mut dout = stream.alloc_zeros::<i32>(n*2).unwrap();
        let bs=256u32; let cfg=LaunchConfig{grid_dim:(((n*2) as u32+bs-1)/bs,1,1),block_dim:(bs,1,1),shared_mem_bytes:0};
        let mut lb=stream.launch_builder(&f);
        lb.arg(&dfac).arg(&daw).arg(&dcb).arg(&dbw).arg(&it).arg(&k2).arg(&odd).arg(&nf).arg(&mut dout);
        unsafe{lb.launch(cfg).unwrap();}
        stream.synchronize().unwrap();
        let ghard = stream.memcpy_dtov(&dout).unwrap();
        // per-shot gcd_hard
        let mut gcd_hard = vec![false; n];
        for i in 0..n { gcd_hard[i] = ghard[2*i]!=0 || ghard[2*i+1]!=0; }

        // op-loop per-shot classical truth (continue xof for measured gates)
        let mut qubits=vec![0u64;num_qubits]; let mut bits=vec![0u64;num_bits];
        let mut op_hard = vec![false; n];
        let set_reg = |qubits:&mut [u64], bits:&mut [u64], reg:&[QubitOrBit], val:U256, shot:usize| {
            for (i,item) in reg.iter().enumerate() { let bv=val.bit(i);
                match item { QubitOrBit::Qubit(id)=>{ if bv{qubits[id.0 as usize]|=1<<shot;}else{qubits[id.0 as usize]&=!(1<<shot);}}
                             QubitOrBit::Bit(id)=>{ if bv{bits[id.0 as usize]|=1<<shot;}else{bits[id.0 as usize]&=!(1<<shot);}}}}};
        let get_reg = |qubits:&[u64], bits:&[u64], reg:&[QubitOrBit], shot:usize| -> U256 {
            let mut v=U256::ZERO;
            for (i,item) in reg.iter().enumerate() { let bv=match item {
                QubitOrBit::Qubit(id)=>(qubits[id.0 as usize]>>shot)&1, QubitOrBit::Bit(id)=>(bits[id.0 as usize]>>shot)&1};
                v.set_bit(i, bv!=0);} v};

        for batch in 0..num_batches {
            let bs2 = BATCH.min(n - batch*BATCH);
            for e in qubits.iter_mut(){*e=0;} for e in bits.iter_mut(){*e=0;}
            for shot in 0..bs2 { let i=batch*BATCH+shot;
                set_reg(&mut qubits,&mut bits,&regs[0],targets[i].0,shot);
                set_reg(&mut qubits,&mut bits,&regs[1],targets[i].1,shot);
                set_reg(&mut qubits,&mut bits,&regs[2],offsets[i].0,shot);
                set_reg(&mut qubits,&mut bits,&regs[3],offsets[i].1,shot);
            }
            let mut cond_stack:Vec<u64>=Vec::new(); let mut base_cond:u64=u64::MAX;
            for op in &ops {
                let mut cond=base_cond;
                if op.c_condition!=NO_BIT { cond &= bits[op.c_condition.0 as usize]; }
                match op.kind {
                    OperationType::CCX=>{let v=cond&qubits[op.q_control1.0 as usize]&qubits[op.q_control2.0 as usize]; qubits[op.q_target.0 as usize]^=v;}
                    OperationType::CX=>{let v=cond&qubits[op.q_control1.0 as usize]; qubits[op.q_target.0 as usize]^=v;}
                    OperationType::Swap=>{let mut a=qubits[op.q_control1.0 as usize]; let mut b=qubits[op.q_target.0 as usize]; a^=b; b^=cond&a; a^=b; qubits[op.q_control1.0 as usize]=a; qubits[op.q_target.0 as usize]=b;}
                    OperationType::X=>{qubits[op.q_target.0 as usize]^=cond;}
                    OperationType::CCZ|OperationType::CZ|OperationType::Z|OperationType::Neg=>{}
                    OperationType::Hmr=>{let mut buf=[0u8;8]; xof.read(&mut buf); let rng=u64::from_le_bytes(buf); let ct=op.c_target.0 as usize; bits[ct]&=!cond; bits[ct]^=rng&cond; qubits[op.q_target.0 as usize]&=!cond;}
                    OperationType::R=>{let mut buf=[0u8;8]; xof.read(&mut buf); let _rng=u64::from_le_bytes(buf); qubits[op.q_target.0 as usize]&=!cond;}
                    OperationType::BitInvert=>{bits[op.c_target.0 as usize]^=cond;}
                    OperationType::BitStore0=>{bits[op.c_target.0 as usize]&=!cond;}
                    OperationType::BitStore1=>{bits[op.c_target.0 as usize]|=cond;}
                    OperationType::AppendToRegister|OperationType::Register|OperationType::DebugPrint=>{}
                    OperationType::PushCondition=>{cond_stack.push(base_cond); base_cond&=bits[op.c_condition.0 as usize];}
                    OperationType::PopCondition=>{if let Some(v)=cond_stack.pop(){base_cond=v;}}
                }
            }
            for shot in 0..bs2 { let i=batch*BATCH+shot;
                let gx=get_reg(&qubits,&bits,&regs[0],shot); let gy=get_reg(&qubits,&bits,&regs[1],shot);
                if gx!=expected[i].0 || gy!=expected[i].1 { op_hard[i]=true; }
            }
        }

        // per-nonce tally
        let mut n_ophard=0; let mut n_gcdhard=0; let mut n_fh=0; let mut n_fc=0; let mut n_thr=0;
        for i in 0..n {
            if op_hard[i] { n_ophard+=1; }
            if gcd_hard[i] { n_gcdhard+=1; }
            if gcd_hard[i] && !op_hard[i] { n_fh+=1; }       // FALSE-HARD (bad!)
            if !gcd_hard[i] && op_hard[i] { n_fc+=1; }        // false-clean (harmless)
            if gcd_hard[i] && op_hard[i] { n_thr+=1; }        // true-hard rejected (good)
        }
        tot_shots+=n; tot_ophard+=n_ophard; tot_gcdhard+=n_gcdhard;
        tot_false_hard+=n_fh; tot_false_clean+=n_fc; tot_true_hard_rejected+=n_thr;
        if do_cpu {
            for i in 0..n {
                if cpu_hard[i] { tot_cpu_hard+=1; }
                if cpu_hard[i] && !op_hard[i] { tot_cpu_false_hard+=1; }
                if cpu_hard[i] && op_hard[i] { tot_cpu_true_hard+=1; }
                if cpu_hard[i] && !gcd_hard[i] { tot_gcd_vs_cpu_underreject+=1; }
                if gcd_hard[i] && !cpu_hard[i] { tot_gcd_vs_cpu_overreject+=1; }
            }
        }
        if do_apply {
            for i in 0..n {
                if apply_hard[i] { tot_apply_hard+=1; }
                if apply_hard[i] && !op_hard[i] { tot_apply_false_hard+=1; }
                if apply_hard[i] && op_hard[i] { tot_apply_true_hard+=1; }
                let comb = gcd_hard[i] || apply_hard[i]; // combined filter
                if comb && op_hard[i] { tot_combined_true_hard+=1; }
                if comb && !op_hard[i] { tot_combined_false_hard+=1; }
            }
        }
        let op_clean = n_ophard==0; let gcd_clean = n_gcdhard==0;
        // gcd-clean nonce verdict must be superset: gcd says clean only if op says clean? No:
        // requirement is gcd-clean SET superset op-clean SET => if op_clean then gcd must be clean.
        // i.e. forbidden: op_clean && !gcd_clean (that's a nonce-level false-hard).
        let nonce_ok = !(op_clean && !gcd_clean);
        if nonce_ok { nonce_clean_agree+=1; } else { nonce_disagree.push(nonce); }
        eprintln!("nonce {}: shots={} op_hard={} gcd_hard={} FALSE_HARD={} false_clean={} | op_clean={} gcd_clean={} {}",
            nonce, n, n_ophard, n_gcdhard, n_fh, n_fc, op_clean, gcd_clean, if nonce_ok {"OK"} else {"*** NONCE FALSE-HARD ***"});
    }

    eprintln!("\n===== TOTALS over {} nonces =====", nonces.len());
    eprintln!("shots={} op_hard={} gcd_hard={}", tot_shots, tot_ophard, tot_gcdhard);
    eprintln!("PER-SHOT FALSE-HARD (gcd hard, op clean) = {}  <-- MUST BE 0", tot_false_hard);
    eprintln!("PER-SHOT false-clean (gcd clean, op hard) = {} (harmless)", tot_false_clean);
    let enrich = if tot_ophard>0 {100.0*tot_true_hard_rejected as f64/tot_ophard as f64} else {0.0};
    eprintln!("ENRICHMENT: true-hard rejected {}/{} = {:.2}%", tot_true_hard_rejected, tot_ophard, enrich);
    eprintln!("nonce-level: {}/{} OK (no nonce false-hard); disagree={:?}", nonce_clean_agree, nonces.len(), nonce_disagree);
    if std::env::var("CPU_MODEL").is_ok() {
        eprintln!("\n--- CPU full-model (check_gcd_factor) vs op-loop ---");
        eprintln!("cpu_hard={} cpu_FALSE_HARD(cpu hard,op clean)={} cpu_true_hard={}", tot_cpu_hard, tot_cpu_false_hard, tot_cpu_true_hard);
        let ce = if tot_ophard>0 {100.0*tot_cpu_true_hard as f64/tot_ophard as f64} else {0.0};
        eprintln!("CPU enrichment: true-hard rejected {}/{} = {:.2}%", tot_cpu_true_hard, tot_ophard, ce);
        eprintln!("--- gcd.cu vs CPU model ---");
        eprintln!("cpu hard & gcd clean (gcd UNDER-rejects vs cpu, port gap) = {}", tot_gcd_vs_cpu_underreject);
        eprintln!("gcd hard & cpu clean (gcd stricter than cpu) = {}", tot_gcd_vs_cpu_overreject);
    }
    if std::env::var("APPLY_MODEL").is_ok() {
        eprintln!("\n--- APPLY-hazard model (check_point_add_apply_hazards) vs op-loop ---");
        eprintln!("apply_hard={} apply_FALSE_HARD={} apply_true_hard={}", tot_apply_hard, tot_apply_false_hard, tot_apply_true_hard);
        let ae=if tot_ophard>0{100.0*tot_apply_true_hard as f64/tot_ophard as f64}else{0.0};
        eprintln!("APPLY enrichment alone: {}/{} = {:.2}%", tot_apply_true_hard, tot_ophard, ae);
        eprintln!("--- COMBINED (gcd OR apply) vs op-loop ---");
        eprintln!("combined_FALSE_HARD={}  <-- MUST BE 0", tot_combined_false_hard);
        let ce=if tot_ophard>0{100.0*tot_combined_true_hard as f64/tot_ophard as f64}else{0.0};
        eprintln!("COMBINED enrichment: {}/{} = {:.2}%", tot_combined_true_hard, tot_ophard, ce);
    }
}
