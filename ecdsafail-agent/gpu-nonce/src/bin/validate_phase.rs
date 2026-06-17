// M1 harness: load /tmp/phase_m1 dumps, NVRTC-compile + launch phase_sim.cu via
// cudarc, compare kernel output to the Rust golden. Prints PASS/FAIL.

use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use std::time::Instant;

const SRC: &str = include_str!("../phase_sim.cu");
const DUMP: &str = "/tmp/phase_m1";

fn read_u64s(path: &str) -> Vec<u64> {
    let b = std::fs::read(path).unwrap();
    assert!(b.len() % 8 == 0, "{path} not u64-aligned");
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn main() {
    // meta
    let meta = read_u64s(&format!("{DUMP}/meta.bin"));
    let num_qubits = meta[0];
    let num_bits = meta[1];
    let bs = meta[2];
    let n_rng = meta[3];
    let chosen_batch = meta[4];
    eprintln!("num_qubits={} num_bits={} bs={} n_rng={} chosen_batch={}",
        num_qubits, num_bits, bs, n_rng, chosen_batch);

    // ops blob (skip 8-byte count header, keep raw 24-byte records)
    let ops_raw = std::fs::read(format!("{DUMP}/ops.bin")).unwrap();
    let n_ops = u64::from_le_bytes(ops_raw[0..8].try_into().unwrap());
    let ops_blob: Vec<u8> = ops_raw[8..].to_vec();
    assert_eq!(ops_blob.len() as u64, n_ops * 24, "ops blob size mismatch");
    eprintln!("n_ops={}", n_ops);

    let init_q = read_u64s(&format!("{DUMP}/init_q.bin"));
    let init_b = read_u64s(&format!("{DUMP}/init_b.bin"));
    assert_eq!(init_q.len() as u64, num_qubits);
    assert_eq!(init_b.len() as u64, num_bits);
    let mut rng = read_u64s(&format!("{DUMP}/rng.bin"));
    assert_eq!(rng.len() as u64, n_rng);
    if rng.is_empty() { rng.push(0); }

    // reg_q: 512 u32
    let reg_q_bytes = std::fs::read(format!("{DUMP}/reg_q.bin")).unwrap();
    let reg_q: Vec<u32> = reg_q_bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    assert_eq!(reg_q.len(), 512);

    // golden: per shot 4 u64 reg0 + 4 u64 reg1 + 1 byte ancilla = 65 bytes; then 8 byte phase
    let golden = std::fs::read(format!("{DUMP}/golden.bin")).unwrap();
    let mut g_reg0 = [[0u64;4];64];
    let mut g_reg1 = [[0u64;4];64];
    let mut g_anc = [0u8;64];
    let mut off = 0;
    for s in 0..64 {
        for i in 0..4 { g_reg0[s][i]=u64::from_le_bytes(golden[off..off+8].try_into().unwrap()); off+=8; }
        for i in 0..4 { g_reg1[s][i]=u64::from_le_bytes(golden[off..off+8].try_into().unwrap()); off+=8; }
        g_anc[s]=golden[off]; off+=1;
    }
    let g_phase = u64::from_le_bytes(golden[off..off+8].try_into().unwrap());

    // ---- GPU ----
    let ctx = CudaContext::new(0).expect("cuda ctx");
    let stream = ctx.default_stream();
    eprintln!("compiling phase_sim.cu ...");
    let ptx = match compile_ptx(SRC) { Ok(p)=>p, Err(e)=>{ eprintln!("NVRTC ERR:\n{:?}",e); std::process::exit(1);} };
    let m = ctx.load_module(ptx).unwrap();
    let f = m.load_function("phase_sim").unwrap();

    let d_ops = stream.memcpy_stod(&ops_blob).unwrap();
    let d_iq = stream.memcpy_stod(&init_q).unwrap();
    let d_ib = stream.memcpy_stod(&init_b).unwrap();
    let d_rng = stream.memcpy_stod(&rng).unwrap();
    let d_regq = stream.memcpy_stod(&reg_q).unwrap();

    let q_scratch = stream.alloc_zeros::<u8>((num_qubits*64) as usize).unwrap();
    let b_scratch = stream.alloc_zeros::<u8>((num_bits*64) as usize).unwrap();

    let mut d_out0 = stream.alloc_zeros::<u64>(64*4).unwrap();
    let mut d_out1 = stream.alloc_zeros::<u64>(64*4).unwrap();
    let mut d_phase = stream.alloc_zeros::<u8>(64).unwrap();
    let mut d_anc = stream.alloc_zeros::<u8>(64).unwrap();

    let cfg = LaunchConfig{ grid_dim:(1,1,1), block_dim:(64,1,1), shared_mem_bytes:0 };

    // warm + timed launch
    let t0 = Instant::now();
    {
        let mut lb = stream.launch_builder(&f);
        lb.arg(&d_ops).arg(&n_ops)
          .arg(&d_iq).arg(&d_ib)
          .arg(&num_qubits).arg(&num_bits)
          .arg(&d_rng).arg(&n_rng).arg(&bs)
          .arg(&d_regq)
          .arg(&q_scratch).arg(&b_scratch)
          .arg(&mut d_out0).arg(&mut d_out1).arg(&mut d_phase).arg(&mut d_anc);
        unsafe { lb.launch(cfg).unwrap(); }
    }
    stream.synchronize().unwrap();
    let dt = t0.elapsed();

    let out0 = stream.memcpy_dtov(&d_out0).unwrap();
    let out1 = stream.memcpy_dtov(&d_out1).unwrap();
    let phase = stream.memcpy_dtov(&d_phase).unwrap();
    let anc = stream.memcpy_dtov(&d_anc).unwrap();

    // ---- compare ----
    let mut fails = 0;
    for s in 0..64 {
        for i in 0..4 {
            if out0[s*4+i] != g_reg0[s][i] {
                if fails < 8 { eprintln!("shot {s} reg0 limb{i}: gpu={:#x} cpu={:#x}", out0[s*4+i], g_reg0[s][i]); }
                fails += 1;
            }
            if out1[s*4+i] != g_reg1[s][i] {
                if fails < 8 { eprintln!("shot {s} reg1 limb{i}: gpu={:#x} cpu={:#x}", out1[s*4+i], g_reg1[s][i]); }
                fails += 1;
            }
        }
        if anc[s] != g_anc[s] {
            if fails < 8 { eprintln!("shot {s} ancilla: gpu={} cpu={}", anc[s], g_anc[s]); }
            fails += 1;
        }
    }

    // reconstruct gpu phase word (bit s = phase[s]) and compare to golden u64 phase
    let mut gpu_phase: u64 = 0;
    for s in 0..64 { if phase[s] & 1 != 0 { gpu_phase |= 1u64 << s; } }
    let phase_ok = gpu_phase == g_phase;
    if !phase_ok {
        eprintln!("PHASE mismatch: gpu={:#018x} cpu={:#018x}", gpu_phase, g_phase);
        fails += 1;
    }

    eprintln!("kernel time (64-shot batch): {:?}", dt);
    eprintln!("golden phase word = {:#018x}", g_phase);
    eprintln!("classical/ancilla mismatches: {}", fails - (!phase_ok as usize));

    if fails == 0 {
        println!("PASS  (batch {} : all 64 shots reg0/reg1/phase/ancilla match)", chosen_batch);
    } else {
        println!("FAIL  ({} mismatches on batch {})", fails, chosen_batch);
        std::process::exit(1);
    }
}
