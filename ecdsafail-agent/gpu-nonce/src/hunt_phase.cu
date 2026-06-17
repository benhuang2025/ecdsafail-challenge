// M3 stage-2 fused phase kernel. 1 thread = 1 candidate nonce. Processes all
// compacted batches SEQUENTIALLY so the single Fiat-Shamir XOF flows naturally
// (no seeking). Mirrors eval_circuit's order EXACTLY:
//   1. rebuild FS state + absorb nonce (same convention as hunt.cu) -> finalized XOF
//   2. ALL 9024 raw-shot input reads happen first (k1,k2 = 64 bytes each),
//      WITH compaction (skip degenerate shots; skipped shots still consume xof_in)
//   3. THEN measured-gate RNG (8 bytes each) flows from the SAME XOF continuation,
//      in op order across batches.
//
// This file reuses field.cuh/points.cu/keccak.cu (included by the harness before it)
// and the phase_sim_v2 op-loop semantics verbatim.
//
// Two kernels:
//   derive_check : de-risk step 1. Derive batch 0's 64 inputs (reg-packed) and the
//                  first N_RNG measured-gate u64s on-GPU; write them out for a
//                  byte-exact comparison against the CPU phase_ref dump.
//   hunt_phase   : full pipeline. Run the op-loop per batch, early-exit on the first
//                  classical or phase failure, emit a verdict per candidate.

#ifndef NQ
#define NQ 1170
#endif
#ifndef NS
#define NS 767
#endif
#ifndef CONDSTACK
#define CONDSTACK 64
#endif
#ifndef TARGET_SHOTS
#define TARGET_SHOTS 9024
#endif

typedef unsigned long long u64;

// ---- 8-byte-granular XOF reader, continues from a finalized Xof (keccak.cu) ----
// keccak.cu's squeeze_scalars reads 64 bytes (8 words) at a time and keeps opos a
// multiple of 8. The input region is 9024*64 = 577536 bytes = a multiple of 8, so
// after the inputs the Xof is 8-byte aligned and we can read 8 bytes at a time.
__device__ __forceinline__ u64 squeeze_u64(Xof* x) {
    if (x->opos == 136) {
        #pragma unroll
        for (int i = 0; i < 17; i++) x->out.words[i] = x->st[i];
        keccak_f(x->st);
        x->opos = 0;
    }
    u64 v = x->out.words[x->opos >> 3];
    x->opos += 8;
    return v;
}

// Rebuild the finalized XOF for a given nonce: FS prefix (st0,buf0,pos0) + nonce tail
// (2*nonce_bits identity X ops on qubit (bit?1:0), all other fields NO_QUBIT/NO_BIT/NO_REG).
// Identical convention to hunt.cu and to a CPU build with DIALOG_TAIL_NONCE=nonce.
__device__ __forceinline__ void build_xof(
    Xof* x, const u64* st0, const unsigned char* buf0, int pos0,
    int nonce_bits, int xkind, unsigned long long nonce)
{
    Shake s; shake_init_from(&s, st0, buf0, pos0);
    for (int i = 0; i < nonce_bits; i++) {
        unsigned long long q = ((nonce >> i) & 1ULL) ? 1ULL : 0ULL;
        for (int r = 0; r < 2; r++) {
            unsigned char op[49]; op[0] = (unsigned char)xkind;
            u64 f[6] = {~0ULL, ~0ULL, q, ~0ULL, ~0ULL, ~0ULL};
            for (int kk = 0; kk < 6; kk++)
                for (int b = 0; b < 8; b++) op[1 + kk*8 + b] = (unsigned char)(f[kk] >> (8*b));
            shake_absorb(&s, op, 49);
        }
    }
    shake_finalize(&s, x);
}

// less-than for 4-word LE (local copy; hunt.cu has its own g_lt4)
__device__ __forceinline__ int hp_lt4(const u64* a, const u64* b) {
    for (int i = 3; i >= 0; i--) { if (a[i] < b[i]) return 1; if (a[i] > b[i]) return 0; }
    return 0;
}
// affine sub mod p (xo - xt etc.), matches hunt.cu submodp
__device__ __forceinline__ void hp_submodp(u64* r, const u64* a, const u64* b) {
    if (!hp_lt4(a, b)) { g_sub(r, a, b); }
    else { u64 t[4]; g_sub(t, b, a); g_sub(r, PP, t); }
}

// ===========================================================================
// derive_check : 1 thread total (idx 0). Derive batch 0's 64 reg-packed inputs
// (target.x, target.y, offset.x, offset.y per shot) + the first n_rng_out
// measured-gate u64s. Writes:
//   out_inputs : per shot s in [0,64): tx[4],ty[4],ox[4],oy[4],ex[4]  (20 u64/shot)
//   out_rng    : n_rng_out u64
//   out_n      : [n_survivors]
// ===========================================================================
extern "C" __global__ void derive_check(
    const u64* __restrict__ st0, const unsigned char* __restrict__ buf0, int pos0,
    int nonce_bits, int xkind, unsigned long long nonce,
    const u64* __restrict__ TBL,
    u64* __restrict__ out_inputs,  // 20*64 u64
    u64* __restrict__ out_rng,     // n_rng_out u64
    u64 n_rng_out,
    u64* __restrict__ out_n)       // 1 u64
{
    if (blockIdx.x*blockDim.x + threadIdx.x != 0) return;

    Xof x;
    build_xof(&x, st0, buf0, pos0, nonce_bits, xkind, nonce);

    // ---- input derivation with compaction; capture first 64 survivors ----
    u64 sv_tx[64][4], sv_ty[64][4], sv_ox[64][4], sv_oy[64][4], sv_ex[64][4];
    int nsv = 0;
    for (int i = 0; i < TARGET_SHOTS; i++) {
        u64 k1[4], k2[4];
        squeeze_scalars(&x, k1, k2);
        u64 JXt[4],JYt[4],JZt[4], JXo[4],JYo[4],JZo[4];
        scalarmul_jac(k1, TBL, JXt, JYt, JZt);
        scalarmul_jac(k2, TBL, JXo, JYo, JZo);
        // skip if t inf or o inf
        if (((JZt[0]|JZt[1]|JZt[2]|JZt[3])==0ULL) || ((JZo[0]|JZo[1]|JZo[2]|JZo[3])==0ULL)) continue;
        // affine
        u64 zt[5],zo[5]; cp4(zt,JZt); zt[4]=0; _ModInv(zt); cp4(zo,JZo); zo[4]=0; _ModInv(zo);
        u64 xt[4],yt[4],xo[4],yo[4];
        affine_from_zinv(JXt,JYt,zt,xt,yt);
        affine_from_zinv(JXo,JYo,zo,xo,yo);
        if (xt[0]==xo[0]&&xt[1]==xo[1]&&xt[2]==xo[2]&&xt[3]==xo[3]) continue;
        // e = t + o (affine add); lambda=(yo-yt)/(xo-xt)
        u64 den[4]; hp_submodp(den, xo, xt);
        u64 di[5]; cp4(di,den); di[4]=0; _ModInv(di);
        u64 num[4],lam[4],t[4],ex[4],ey[4];
        _ModSub256(num,yo,yt); _ModMult(lam,num,di);
        _ModSqr(t,lam); _ModSub256(t,t,xt); _ModSub256(ex,t,xo); canon(ex);
        u64 t2[4]; _ModSub256(t2,xt,ex); _ModMult(t2,lam,t2); _ModSub256(ey,t2,yt); canon(ey);
        if (nsv < 64) {
            cp4(sv_tx[nsv],xt); cp4(sv_ty[nsv],yt);
            cp4(sv_ox[nsv],xo); cp4(sv_oy[nsv],yo);
            cp4(sv_ex[nsv],ex);
        }
        nsv++;
    }
    out_n[0] = (u64)nsv;
    for (int s = 0; s < 64 && s < nsv; s++) {
        for (int j=0;j<4;j++){
            out_inputs[s*20+0+j]=sv_tx[s][j];
            out_inputs[s*20+4+j]=sv_ty[s][j];
            out_inputs[s*20+8+j]=sv_ox[s][j];
            out_inputs[s*20+12+j]=sv_oy[s][j];
            out_inputs[s*20+16+j]=sv_ex[s][j];
        }
    }
    // ---- measured-gate RNG: continue the same XOF, 8 bytes each ----
    for (u64 k = 0; k < n_rng_out; k++) out_rng[k] = squeeze_u64(&x);
}

// ===========================================================================
// hunt_phase : 1 thread = 1 candidate nonce. Full stage-2 phase filter.
//
// Inputs:
//   st0/buf0/pos0  FS prefix; nonce_bits/xkind  nonce-tail convention.
//   nonce_start    candidate nonces are nonce_start+idx for idx in [0,n)
//   ops_blob       v2 compact ops (24 B/op, slot-remapped, zflag), n_ops of them
//   reg_q[512]     reg0|reg1 qubit indices (for set/get of t.x/t.y and e.x/e.y)
//   reg_s[512]     reg2|reg3 SLOT indices (for set of o.x/o.y classical bits)
//   TBL            comb table
// Output:
//   out_verdict[idx] : 0 = dual-clean ; 1 = classical-fail ; 2 = phase-fail ;
//                      3 = degenerate (n survivors not a clean multiple? still run)
//   out_failbatch[idx] : first failing batch index (or -1)
//
// Verdict computed by running compacted batches in order, early-exiting on the
// first classical or phase failure. Ancilla is NOT checked here (CPU re-certifies
// the handful of dual-clean survivors); classical+phase is the stage-2 gate.
// ===========================================================================
extern "C" __global__ void hunt_phase(
    const u64* __restrict__ st0, const unsigned char* __restrict__ buf0, int pos0,
    int nonce_bits, int xkind, unsigned long long nonce_start, int n,
    const unsigned char* __restrict__ ops_blob, u64 n_ops,
    const unsigned int* __restrict__ reg_q,   // 512
    const unsigned int* __restrict__ reg_s,   // 512
    const u64* __restrict__ TBL,
    signed char* __restrict__ out_verdict,
    int* __restrict__ out_failbatch,
    const unsigned long long* __restrict__ nlist)
{
    unsigned int idx = blockIdx.x*blockDim.x + threadIdx.x;
    if (idx >= (unsigned)n) return;
    unsigned long long nonce = nlist[idx]; (void)nonce_start;

    // two XOF readers from the same finalized state.
    Xof xin; build_xof(&xin, st0, buf0, pos0, nonce_bits, xkind, nonce);
    Xof xmeas = xin; // copy of finalized state (opos=136)
    // position xmeas at the measured region: discard 9024 input reads (= 9024*64 bytes).
    {
        u64 d1[4], d2[4];
        for (int i = 0; i < TARGET_SHOTS; i++) squeeze_scalars(&xmeas, d1, d2);
    }

    // per-thread state
    u64 q[NQ];
    u64 s[NS];

    int verdict = 0;       // default dual-clean
    int failbatch = -1;

    int produced = 0;      // survivors emitted into batches so far
    int consumed = 0;      // raw shots consumed from xin
    // process survivors in batches of 64, deriving inputs lazily from xin.
    while (failbatch < 0) {
        // zero state, then derive+scatter inputs lane-by-lane (no 64-wide input buffer)
        #pragma unroll 1
        for (int i = 0; i < NQ; i++) q[i] = 0;
        #pragma unroll 1
        for (int i = 0; i < NS; i++) s[i] = 0;
        u64 b_ex[64][4], b_ey[64][4];
        int bs = 0;
        while (bs < 64 && consumed < TARGET_SHOTS) {
            consumed++;
            u64 k1[4], k2[4];
            squeeze_scalars(&xin, k1, k2);
            u64 JXt[4],JYt[4],JZt[4], JXo[4],JYo[4],JZo[4];
            scalarmul_jac(k1, TBL, JXt, JYt, JZt);
            scalarmul_jac(k2, TBL, JXo, JYo, JZo);
            if (((JZt[0]|JZt[1]|JZt[2]|JZt[3])==0ULL) || ((JZo[0]|JZo[1]|JZo[2]|JZo[3])==0ULL)) continue;
            u64 zt[5],zo[5]; cp4(zt,JZt); zt[4]=0; _ModInv(zt); cp4(zo,JZo); zo[4]=0; _ModInv(zo);
            u64 xt[4],yt[4],xo[4],yo[4];
            affine_from_zinv(JXt,JYt,zt,xt,yt);
            affine_from_zinv(JXo,JYo,zo,xo,yo);
            if (xt[0]==xo[0]&&xt[1]==xo[1]&&xt[2]==xo[2]&&xt[3]==xo[3]) continue;
            u64 den[4]; hp_submodp(den, xo, xt);
            u64 di[5]; cp4(di,den); di[4]=0; _ModInv(di);
            u64 num[4],lam[4],t[4],ex[4],ey[4];
            _ModSub256(num,yo,yt); _ModMult(lam,num,di);
            _ModSqr(t,lam); _ModSub256(t,t,xt); _ModSub256(ex,t,xo); canon(ex);
            u64 t2[4]; _ModSub256(t2,xt,ex); _ModMult(t2,lam,t2); _ModSub256(ey,t2,yt); canon(ey);
            { u64 m = 1ULL << bs;
              for (int i = 0; i < 256; i++) {
                if ((xt[i>>6] >> (i&63)) & 1ULL) q[reg_q[i]]     |= m;
                if ((yt[i>>6] >> (i&63)) & 1ULL) q[reg_q[256+i]] |= m;
                if ((xo[i>>6] >> (i&63)) & 1ULL) s[reg_s[i]]     |= m;
                if ((yo[i>>6] >> (i&63)) & 1ULL) s[reg_s[256+i]] |= m;
              } }
            cp4(b_ex[bs],ex); cp4(b_ey[bs],ey);
            bs++;
        }
        if (bs == 0) break; // no more survivors

        int batch = produced / 64;

        // ---- op-loop (phase_sim_v2 verbatim) ----
        u64 phase = 0, rng_idx_unused = 0; (void)rng_idx_unused;
        u64 cond_stack[CONDSTACK]; int sp = 0; u64 base_cond = ~0ULL;
        const unsigned char* p = ops_blob;
        for (u64 oi = 0; oi < n_ops; oi++, p += 24) {
            unsigned char kind  = p[0];
            unsigned char zflag = p[1];
            int qc2 = *(const int*)(p + 4);
            int qc1 = *(const int*)(p + 8);
            int qt  = *(const int*)(p + 12);
            int st  = *(const int*)(p + 16);
            int sc  = *(const int*)(p + 20);
            if (zflag & 1) s[st] = 0;
            if (zflag & 2) s[sc] = 0;
            u64 cond = base_cond;
            if (sc != -1) cond &= s[sc];
            switch (kind) {
                case 13: q[qt] ^= cond & q[qc1] & q[qc2]; break;          // CCX
                case 8:  q[qt] ^= cond & q[qc1]; break;                    // CX
                case 10: { u64 a=q[qc1], b=q[qt]; a^=b; b^=cond&a; a^=b; q[qc1]=a; q[qt]=b; } break; // Swap
                case 6:  q[qt] ^= cond; break;                             // X
                case 14: phase ^= cond & q[qt] & q[qc1] & q[qc2]; break;   // CCZ
                case 9:  phase ^= cond & q[qt] & q[qc1]; break;            // CZ
                case 7:  phase ^= cond & q[qt]; break;                     // Z
                case 0:  phase ^= cond; break;                             // Neg
                case 12: { u64 rv = squeeze_u64(&xmeas);                   // Hmr
                           s[st] = (s[st] & ~cond) ^ (rv & cond);
                           phase ^= q[qt] & rv & cond; q[qt] &= ~cond; } break;
                case 11: { u64 rv = squeeze_u64(&xmeas);                   // R
                           phase ^= q[qt] & rv & cond; q[qt] &= ~cond; } break;
                case 3:  s[st] ^= cond; break;                            // BitInvert
                case 4:  s[st] &= ~cond; break;                           // BitStore0
                case 5:  s[st] |= cond; break;                            // BitStore1
                case 2: case 1: case 17: break;                          // Append/Register/Debug
                case 15: cond_stack[sp++] = base_cond; base_cond &= s[sc]; break; // Push
                case 16: if (sp > 0) base_cond = cond_stack[--sp]; break; // Pop
                default: break;
            }
        }

        u64 cond_mask = (bs >= 64) ? ~0ULL : ((1ULL << bs) - 1);

        // ---- classical check: reg0/reg1 == e.x/e.y per shot ----
        int cfail = 0;
        for (int sh = 0; sh < bs && !cfail; sh++) {
            for (int i = 0; i < 256; i++) {
                u64 got = (q[reg_q[i]] >> sh) & 1ULL;
                u64 want = (b_ex[sh][i>>6] >> (i&63)) & 1ULL;
                if (got != want) { cfail = 1; break; }
            }
            if (cfail) break;
            for (int i = 0; i < 256; i++) {
                u64 got = (q[reg_q[256+i]] >> sh) & 1ULL;
                u64 want = (b_ey[sh][i>>6] >> (i&63)) & 1ULL;
                if (got != want) { cfail = 1; break; }
            }
        }
        if (cfail) { verdict = 1; failbatch = batch; break; }

        // ---- phase check ----
        if ((phase & cond_mask) != 0) { verdict = 2; failbatch = batch; break; }

        produced += bs;
        if (consumed >= TARGET_SHOTS && bs < 64) break; // last partial batch done
    }

    out_verdict[idx] = (signed char)verdict;
    out_failbatch[idx] = failbatch;
}
