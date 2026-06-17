// M2 benchmark: throughput of phase_sim_v2 across the GPU grid.
//
// The op-loop is data-oblivious (runtime independent of input values), so we
// replicate ONE validated batch's state across the whole grid as a fair timing
// proxy. Each thread = 1 batch (64 shots). nonce/s = batches_per_s / 141.
//
// env:
//   BENCH_BLOCKS  = grid blocks (default 512)
//   BENCH_BS      = threads per block, swept if "sweep" (default 64)
//   BENCH_ITERS   = kernel launches to average (default 3)
//   BENCH_GPUS    = number of GPUs to use for the aggregate number (default 1)
//
// Loads /tmp/phase_m2 (must be a valid v2 dump). write_out=0 (bench mode).

use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::compile_ptx;
use std::time::Instant;

const SRC: &str = include_str!("../phase_sim_v2b.cu");
const DUMP: &str = "/tmp/phase_m2";
const BATCHES_PER_NONCE: f64 = 141.0;

fn read_u64s(path: &str) -> Vec<u64> {
    let b = std::fs::read(path).unwrap();
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}
fn envu(k:&str,d:u64)->u64{ std::env::var(k).ok().and_then(|s|s.parse().ok()).unwrap_or(d) }

fn main() {
    let meta = read_u64s(&format!("{DUMP}/meta2.bin"));
    let num_qubits = meta[0];
    let num_slots = meta[1];
    let bs = meta[2];
    let n_rng = meta[3];
    let n_ops = meta[5];

    let ops_raw = std::fs::read(format!("{DUMP}/ops3.bin")).unwrap();
    let ops_blob: Vec<u8> = ops_raw[8..].to_vec();
    let init_q = read_u64s(&format!("{DUMP}/init_q.bin"));
    let init_slots = read_u64s(&format!("{DUMP}/init_b_slots.bin"));
    let mut rng = read_u64s(&format!("{DUMP}/rng.bin"));
    if rng.is_empty() { rng.push(0); }
    let reg_q_bytes = std::fs::read(format!("{DUMP}/reg_q.bin")).unwrap();
    let reg_q: Vec<u32> = reg_q_bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();

    eprintln!("n_ops={} num_qubits={} num_slots={} per-thread state ~= {} bytes",
        n_ops, num_qubits, num_slots, (num_qubits+num_slots)*8);

    let blocks = envu("BENCH_BLOCKS", 512) as u32;
    let iters = envu("BENCH_ITERS", 3);
    let ngpu = envu("BENCH_GPUS", 1) as usize;
    let sweep = std::env::var("BENCH_BS").map(|v| v=="sweep").unwrap_or(false);
    let bs_list: Vec<u32> = if sweep { vec![32,64,96,128,192,256] }
        else { vec![envu("BENCH_BS",64) as u32] };

    // compile once on dev 0 (PTX is portable across the identical 5090s)
    let ctx0 = CudaContext::new(0).expect("cuda ctx 0");
    eprintln!("compiling phase_sim_v2b.cu ...");
    let ptx = match compile_ptx(SRC) { Ok(p)=>p, Err(e)=>{ eprintln!("NVRTC ERR:\n{:?}",e); std::process::exit(1);} };
    drop(ctx0);

    let run_on_gpu = |dev: usize, block_bs: u32| -> f64 {
        // returns batches/s on this single GPU
        let ctx = CudaContext::new(dev).expect("ctx");
        let stream = ctx.default_stream();
        let m = ctx.load_module(ptx.clone()).unwrap();
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
        let write_out: i32 = 0;
        let cfg = LaunchConfig{ grid_dim:(blocks,1,1), block_dim:(block_bs,1,1), shared_mem_bytes:0 };
        let total_threads = (blocks as u64) * (block_bs as u64);

        // warmup
        {
            let mut lb = stream.launch_builder(&f);
            lb.arg(&d_ops).arg(&n_ops).arg(&d_iq).arg(&d_is).arg(&d_rng).arg(&n_rng).arg(&bs)
              .arg(&d_regq).arg(&mut d_out0).arg(&mut d_out1).arg(&mut d_phase).arg(&mut d_anc).arg(&write_out);
            unsafe { lb.launch(cfg).unwrap(); }
        }
        stream.synchronize().unwrap();

        let t0 = Instant::now();
        for _ in 0..iters {
            let mut lb = stream.launch_builder(&f);
            lb.arg(&d_ops).arg(&n_ops).arg(&d_iq).arg(&d_is).arg(&d_rng).arg(&n_rng).arg(&bs)
              .arg(&d_regq).arg(&mut d_out0).arg(&mut d_out1).arg(&mut d_phase).arg(&mut d_anc).arg(&write_out);
            unsafe { lb.launch(cfg).unwrap(); }
        }
        stream.synchronize().unwrap();
        let dt = t0.elapsed().as_secs_f64();
        let batches = total_threads * iters; // each thread = 1 batch
        batches as f64 / dt
    };

    for &block_bs in &bs_list {
        // single GPU first
        let bps = run_on_gpu(0, block_bs);
        let nonce_s = bps / BATCHES_PER_NONCE;
        eprintln!("bs={:>3} blocks={} threads={} : {:.1} batches/s -> {:.3} nonce/s (1 GPU)",
            block_bs, blocks, (blocks as u64)*(block_bs as u64), bps, nonce_s);
    }

    if ngpu > 1 {
        let block_bs = bs_list[0];
        eprintln!("--- aggregate over {} GPUs (bs={}) ---", ngpu, block_bs);
        let t0 = Instant::now();
        let results: Vec<f64> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..ngpu).map(|dev| {
                scope.spawn(move || run_on_gpu(dev, block_bs))
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let _ = t0;
        let total_bps: f64 = results.iter().sum();
        let total_nonce_s = total_bps / BATCHES_PER_NONCE;
        for (d,r) in results.iter().enumerate() {
            eprintln!("  gpu{} : {:.1} batches/s ({:.3} nonce/s)", d, r, r/BATCHES_PER_NONCE);
        }
        eprintln!("AGGREGATE {} GPUs: {:.1} batches/s -> {:.2} nonce/s", ngpu, total_bps, total_nonce_s);
    }
}
