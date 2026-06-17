// M2 validation harness: load /tmp/phase_m2 dumps, NVRTC-compile + launch
// phase_sim_v2b.cu (1 thread = full batch, write_out=1), compare to Rust golden.

use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use std::time::Instant;

const SRC: &str = include_str!("../phase_sim_v2b.cu");
const DUMP: &str = "/tmp/phase_m2";

fn read_u64s(path: &str) -> Vec<u64> {
    let b = std::fs::read(path).unwrap();
    assert!(b.len() % 8 == 0, "{path} not u64-aligned");
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn main() {
    let meta = read_u64s(&format!("{DUMP}/meta2.bin"));
    let num_qubits = meta[0];
    let num_slots = meta[1];
    let bs = meta[2];
    let n_rng = meta[3];
    let chosen_batch = meta[4];
    let n_ops = meta[5];
    eprintln!("num_qubits={} num_slots={} bs={} n_rng={} chosen_batch={} n_ops={}",
        num_qubits, num_slots, bs, n_rng, chosen_batch, n_ops);

    let ops_raw = std::fs::read(format!("{DUMP}/ops3.bin")).unwrap();
    let n_ops_hdr = u64::from_le_bytes(ops_raw[0..8].try_into().unwrap());
    assert_eq!(n_ops_hdr, n_ops);
    let ops_blob: Vec<u8> = ops_raw[8..].to_vec();
    assert_eq!(ops_blob.len() as u64, n_ops * 12, "ops blob size mismatch");

    let init_q = read_u64s(&format!("{DUMP}/init_q.bin"));
    let init_slots = read_u64s(&format!("{DUMP}/init_b_slots.bin"));
    assert_eq!(init_q.len() as u64, num_qubits);
    assert_eq!(init_slots.len() as u64, num_slots);
    let mut rng = read_u64s(&format!("{DUMP}/rng.bin"));
    assert_eq!(rng.len() as u64, n_rng);
    if rng.is_empty() { rng.push(0); }

    let reg_q_bytes = std::fs::read(format!("{DUMP}/reg_q.bin")).unwrap();
    let reg_q: Vec<u32> = reg_q_bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    assert_eq!(reg_q.len(), 512);

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

    let ctx = CudaContext::new(0).expect("cuda ctx");
    let stream = ctx.default_stream();
    eprintln!("compiling phase_sim_v2b.cu ...");
    let ptx = match compile_ptx(SRC) { Ok(p)=>p, Err(e)=>{ eprintln!("NVRTC ERR:\n{:?}",e); std::process::exit(1);} };
    let m = ctx.load_module(ptx).unwrap();
    let f = m.load_function("phase_sim_v2b").unwrap();

    let d_ops = stream.memcpy_stod(&ops_blob).unwrap();
    let d_iq = stream.memcpy_stod(&init_q).unwrap();
    let d_is = stream.memcpy_stod(&init_slots).unwrap();
    let d_rng = stream.memcpy_stod(&rng).unwrap();
    let d_regq = stream.memcpy_stod(&reg_q).unwrap();

    let mut d_out0 = stream.alloc_zeros::<u64>(64*4).unwrap();
    let mut d_out1 = stream.alloc_zeros::<u64>(64*4).unwrap();
    let mut d_phase = stream.alloc_zeros::<u64>(1).unwrap();
    let mut d_anc = stream.alloc_zeros::<u64>(1).unwrap();
    let write_out: i32 = 1;

    let cfg = LaunchConfig{ grid_dim:(1,1,1), block_dim:(1,1,1), shared_mem_bytes:0 };

    let t0 = Instant::now();
    {
        let mut lb = stream.launch_builder(&f);
        lb.arg(&d_ops).arg(&n_ops)
          .arg(&d_iq).arg(&d_is)
          .arg(&d_rng).arg(&n_rng).arg(&bs)
          .arg(&d_regq)
          .arg(&mut d_out0).arg(&mut d_out1).arg(&mut d_phase).arg(&mut d_anc)
          .arg(&write_out);
        unsafe { lb.launch(cfg).unwrap(); }
    }
    stream.synchronize().unwrap();
    let dt = t0.elapsed();

    let out0 = stream.memcpy_dtov(&d_out0).unwrap();
    let out1 = stream.memcpy_dtov(&d_out1).unwrap();
    let phase = stream.memcpy_dtov(&d_phase).unwrap();
    let anc = stream.memcpy_dtov(&d_anc).unwrap();
    let gpu_phase = phase[0];
    let gpu_dirty = anc[0];

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
        // golden ancilla is per-shot clean flag (1=clean); gpu_dirty bit s set = dirty
        let gpu_clean = ((gpu_dirty >> s) & 1) == 0;
        if (gpu_clean as u8) != g_anc[s] {
            if fails < 8 { eprintln!("shot {s} ancilla: gpu_clean={} cpu_clean={}", gpu_clean as u8, g_anc[s]); }
            fails += 1;
        }
    }

    let phase_ok = gpu_phase == g_phase;
    if !phase_ok {
        eprintln!("PHASE mismatch: gpu={:#018x} cpu={:#018x}", gpu_phase, g_phase);
        fails += 1;
    }

    eprintln!("kernel time (single-thread 64-shot batch): {:?}", dt);
    eprintln!("golden phase word = {:#018x}  gpu phase = {:#018x}", g_phase, gpu_phase);

    if fails == 0 {
        println!("PASS  (batch {} : all 64 shots reg0/reg1/phase/ancilla match)", chosen_batch);
    } else {
        println!("FAIL  ({} mismatches on batch {})", fails, chosen_batch);
        std::process::exit(1);
    }
}
