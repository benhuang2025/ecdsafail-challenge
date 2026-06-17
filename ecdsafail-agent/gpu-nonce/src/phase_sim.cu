// M1 GPU phase-sim op-loop kernel (scalar, 1 thread = 1 shot).
//
// Reproduces sim.rs apply_iter for a single shot (one classical assignment).
// Bits/qubits are scalar (0/1) bytes per shot, stored in global memory.
//
// Compact op layout (matches phase_ref.rs CompactOp dump, 24 bytes each):
//   u8 kind; u8 pad[3]; i32 qc2; i32 qc1; i32 qt; i32 ct; i32 cc;   (-1 == NONE)
//
// Kinds: Neg0 Register1 Append2 BitInvert3 BitStore0=4 BitStore1=5 X6 Z7 CX8
//        CZ9 Swap10 R11 Hmr12 CCX13 CCZ14 Push15 Pop16 Debug17.
//
// RNG: Hmr/R consume next8 from the per-batch RNG buffer IN ORDER. Each shot
// reads the SAME sequence of u64 rng words (the bit-sliced sim uses one u64 per
// Hmr/R op shared across all 64 shots); for a single shot s we use bit s of that
// u64. We track a shared rng cursor by op index: since every shot walks the ops
// in the same order, rng word k corresponds to the k-th Hmr/R op encountered.
// So each thread maintains its own rng_idx incrementing on Hmr/R.

extern "C" __global__ void phase_sim(
    const unsigned char* ops_blob, // compact ops, 24 bytes/op
    unsigned long long    n_ops,
    const unsigned long long* init_q,  // num_qubits u64 (bit s = shot s)
    const unsigned long long* init_b,  // num_bits   u64
    unsigned long long    num_qubits,
    unsigned long long    num_bits,
    const unsigned long long* rng,     // n_rng u64 (bit s = shot s)
    unsigned long long    n_rng,
    unsigned long long    bs,          // live shots
    const unsigned int*   reg_q,       // 512 qubit indices: reg0[0..256], reg1[0..256]
    unsigned char*        q_state,     // scratch: num_qubits bytes per shot (64 shots)
    unsigned char*        b_state,     // scratch: num_bits   bytes per shot
    unsigned long long*   out_reg0,    // 4 u64 per shot
    unsigned long long*   out_reg1,    // 4 u64 per shot
    unsigned char*        out_phase,   // 1 byte per shot
    unsigned char*        out_ancilla) // 1 byte per shot (1 = clean)
{
    unsigned int shot = blockIdx.x * blockDim.x + threadIdx.x;
    if (shot >= 64) return;

    unsigned char* q = q_state + (size_t)shot * num_qubits;
    unsigned char* b = b_state + (size_t)shot * num_bits;

    if (shot >= bs) {
        // dead shot: emit zeros + clean
        for (int i = 0; i < 4; i++) { out_reg0[shot*4+i]=0; out_reg1[shot*4+i]=0; }
        out_phase[shot] = 0;
        out_ancilla[shot] = 1;
        return;
    }

    // load initial scalar state (bit `shot` of each u64)
    for (unsigned long long i = 0; i < num_qubits; i++)
        q[i] = (unsigned char)((init_q[i] >> shot) & 1ULL);
    for (unsigned long long i = 0; i < num_bits; i++)
        b[i] = (unsigned char)((init_b[i] >> shot) & 1ULL);

    unsigned char phase = 0;
    unsigned long long rng_idx = 0;

    // condition stack (scalar). base_cond starts = 1 (all-ones for this shot).
    unsigned char cond_stack[256];
    int sp = 0;
    unsigned char base_cond = 1;

    const unsigned char* p = ops_blob;
    for (unsigned long long oi = 0; oi < n_ops; oi++, p += 24) {
        unsigned char kind = p[0];
        int qc2 = *(const int*)(p + 4);
        int qc1 = *(const int*)(p + 8);
        int qt  = *(const int*)(p + 12);
        int ct  = *(const int*)(p + 16);
        int cc  = *(const int*)(p + 20);

        unsigned char cond = base_cond;
        if (cc != -1) cond &= b[cc];

        switch (kind) {
            case 13: { // CCX
                unsigned char v = cond & q[qc1] & q[qc2];
                q[qt] ^= v;
            } break;
            case 8: { // CX
                unsigned char v = cond & q[qc1];
                q[qt] ^= v;
            } break;
            case 10: { // Swap
                unsigned char a = q[qc1];
                unsigned char bb = q[qt];
                a ^= bb;
                bb ^= cond & a;
                a ^= bb;
                q[qc1] = a;
                q[qt] = bb;
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
                unsigned char rv = (unsigned char)((rng[rng_idx] >> shot) & 1ULL);
                rng_idx++;
                b[ct] = (b[ct] & (unsigned char)~cond) ^ (rv & cond);
                phase ^= q[qt] & rv & cond;
                q[qt] &= (unsigned char)~cond;
            } break;
            case 11: { // R
                unsigned char rv = (unsigned char)((rng[rng_idx] >> shot) & 1ULL);
                rng_idx++;
                phase ^= q[qt] & rv & cond;
                q[qt] &= (unsigned char)~cond;
            } break;
            case 3: { // BitInvert
                b[ct] ^= cond;
            } break;
            case 4: { // BitStore0
                b[ct] &= (unsigned char)~cond;
            } break;
            case 5: { // BitStore1
                b[ct] |= cond;
            } break;
            case 2: case 1: case 17: // Append / Register / Debug
                break;
            case 15: { // PushCondition
                cond_stack[sp++] = base_cond;
                base_cond &= b[cc];
            } break;
            case 16: { // PopCondition
                if (sp > 0) base_cond = cond_stack[--sp];
            } break;
            default: break;
        }
    }
    (void)n_rng;

    // read reg0 / reg1 (256 qubits each, little endian into 4 u64)
    unsigned long long r0[4] = {0,0,0,0};
    unsigned long long r1[4] = {0,0,0,0};
    for (int i = 0; i < 256; i++) {
        unsigned int qi = reg_q[i];
        if (q[qi]) r0[i >> 6] |= (1ULL << (i & 63));
    }
    for (int i = 0; i < 256; i++) {
        unsigned int qi = reg_q[256 + i];
        if (q[qi]) r1[i >> 6] |= (1ULL << (i & 63));
    }
    for (int i = 0; i < 4; i++) { out_reg0[shot*4+i] = r0[i]; out_reg1[shot*4+i] = r1[i]; }
    out_phase[shot] = phase & 1;

    // ancilla: zero the 512 register qubits, then any nonzero qubit => dirty
    for (int i = 0; i < 512; i++) q[reg_q[i]] = 0;
    unsigned char clean = 1;
    for (unsigned long long i = 0; i < num_qubits; i++) {
        if (q[i]) { clean = 0; break; }
    }
    out_ancilla[shot] = clean;
}
