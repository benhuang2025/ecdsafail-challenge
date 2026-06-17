// M2 GPU phase-sim kernel: bit-sliced (1 thread = 1 batch = 64 shots packed in u64),
// classical bits remapped to <=767 slots, op stream streamed from global (L2-shared).
//
// Mirrors src/sim.rs apply_iter exactly, operating on u64 words (bit s = shot s).
//
// v2 compact op layout (24 bytes/op, from phase_prep.rs CompactOp2):
//   u8 kind; u8 zflag; u8 pad[2]; i32 qc2; i32 qc1; i32 qt; i32 st(slot_t); i32 sc(slot_c)
//   zflag bit0 -> zero slot st before applying; bit1 -> zero slot sc before applying.
//
// Each thread owns its own qubit[NQ] + slot[NS] arrays. For the benchmark these
// live in local memory (per-thread, L1/L2-cached). All threads stream the SAME ops
// blob in lockstep -> high L2 reuse.
//
// RNG: rng[k] is the k-th Hmr/R op's u64 (bit s = shot s), shared across the batch.
//
// Kinds: Neg0 Register1 Append2 BitInvert3 BitStore0=4 BitStore1=5 X6 Z7 CX8
//        CZ9 Swap10 R11 Hmr12 CCX13 CCZ14 Push15 Pop16 Debug17.

#ifndef NQ
#define NQ 1170
#endif
#ifndef NS
#define NS 767
#endif
#ifndef CONDSTACK
#define CONDSTACK 64
#endif

typedef unsigned long long u64;

extern "C" __global__ void phase_sim_v2(
    const unsigned char* __restrict__ ops_blob, // compact v2 ops, 24 bytes/op
    u64                   n_ops,
    const u64* __restrict__ init_q,             // NQ u64 (bit s = shot s) initial qubits
    const u64* __restrict__ init_slots,         // NS u64 initial slot values
    const u64* __restrict__ rng,                // n_rng u64
    u64                   n_rng,
    u64                   bs,                    // live shots (<=64); cond_mask = (1<<bs)-1
    const unsigned int* __restrict__ reg_q,     // 512 qubit indices (reg0[256]|reg1[256])
    u64* __restrict__     out_reg0,             // 4 u64 per shot (per shot of THIS batch)
    u64* __restrict__     out_reg1,             // 4 u64 per shot
    u64* __restrict__     out_phase,            // 1 u64 (phase word, bit s = shot s)
    u64* __restrict__     out_anc,              // 1 u64 (dirty mask, bit s set = shot s dirty)
    int                   write_out)            // 1 = write validation outputs, 0 = bench only
{
    unsigned int tid = blockIdx.x * blockDim.x + threadIdx.x;

    // per-thread state in local memory
    u64 q[NQ];
    u64 s[NS];
    #pragma unroll 1
    for (int i = 0; i < NQ; i++) q[i] = init_q[i];
    #pragma unroll 1
    for (int i = 0; i < NS; i++) s[i] = init_slots[i];

    u64 phase = 0;
    u64 rng_idx = 0;

    u64 cond_stack[CONDSTACK];
    int sp = 0;
    u64 base_cond = ~0ULL;

    const unsigned char* p = ops_blob;
    for (u64 oi = 0; oi < n_ops; oi++, p += 24) {
        unsigned char kind  = p[0];
        unsigned char zflag = p[1];
        int qc2 = *(const int*)(p + 4);
        int qc1 = *(const int*)(p + 8);
        int qt  = *(const int*)(p + 12);
        int st  = *(const int*)(p + 16);
        int sc  = *(const int*)(p + 20);

        // slot-zeroing at interval reuse (stale-bit correctness)
        if (zflag & 1) s[st] = 0;
        if (zflag & 2) s[sc] = 0;

        u64 cond = base_cond;
        if (sc != -1) cond &= s[sc];

        switch (kind) {
            case 13: { // CCX
                q[qt] ^= cond & q[qc1] & q[qc2];
            } break;
            case 8: { // CX
                q[qt] ^= cond & q[qc1];
            } break;
            case 10: { // Swap
                u64 a = q[qc1];
                u64 b = q[qt];
                a ^= b;
                b ^= cond & a;
                a ^= b;
                q[qc1] = a;
                q[qt] = b;
            } break;
            case 6: { // X
                q[qt] ^= cond;
            } break;
            case 14: { // CCZ
                phase ^= cond & q[qt] & q[qc1] & q[qc2];
            } break;
            case 9: { // CZ
                phase ^= cond & q[qt] & q[qc1];
            } break;
            case 7: { // Z
                phase ^= cond & q[qt];
            } break;
            case 0: { // Neg
                phase ^= cond;
            } break;
            case 12: { // Hmr
                u64 rv = rng[rng_idx]; rng_idx++;
                s[st] = (s[st] & ~cond) ^ (rv & cond);
                phase ^= q[qt] & rv & cond;
                q[qt] &= ~cond;
            } break;
            case 11: { // R
                u64 rv = rng[rng_idx]; rng_idx++;
                phase ^= q[qt] & rv & cond;
                q[qt] &= ~cond;
            } break;
            case 3: { // BitInvert
                s[st] ^= cond;
            } break;
            case 4: { // BitStore0
                s[st] &= ~cond;
            } break;
            case 5: { // BitStore1
                s[st] |= cond;
            } break;
            case 2: case 1: case 17: // Append / Register / Debug
                break;
            case 15: { // PushCondition
                cond_stack[sp++] = base_cond;
                base_cond &= s[sc];
            } break;
            case 16: { // PopCondition
                if (sp > 0) base_cond = cond_stack[--sp];
            } break;
            default: break;
        }
    }
    (void)n_rng;

    if (!write_out) {
        // bench mode: prevent dead-code elimination by writing a cheap reduction
        if (tid == 0xFFFFFFFFu) { out_phase[0] = phase ^ q[0] ^ s[0]; }
        return;
    }
    if (tid != 0) return; // validation: only thread 0 writes golden-comparable output

    u64 cond_mask = (bs >= 64) ? ~0ULL : ((1ULL << bs) - 1);

    // per-shot reg0/reg1 readout
    for (int sh = 0; sh < 64; sh++) {
        u64 r0[4] = {0,0,0,0};
        u64 r1[4] = {0,0,0,0};
        for (int i = 0; i < 256; i++) {
            if ((q[reg_q[i]] >> sh) & 1ULL) r0[i >> 6] |= (1ULL << (i & 63));
        }
        for (int i = 0; i < 256; i++) {
            if ((q[reg_q[256 + i]] >> sh) & 1ULL) r1[i >> 6] |= (1ULL << (i & 63));
        }
        for (int i = 0; i < 4; i++) { out_reg0[sh*4+i] = r0[i]; out_reg1[sh*4+i] = r1[i]; }
    }

    out_phase[0] = phase;

    // ancilla: zero the 512 register qubits, OR remaining qubits under cond_mask
    for (int i = 0; i < 512; i++) q[reg_q[i]] = 0;
    u64 dirty = 0;
    for (int i = 0; i < NQ; i++) dirty |= q[i];
    out_anc[0] = dirty & cond_mask;
}
