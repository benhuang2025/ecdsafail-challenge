// Device Keccak-f[1600] / SHAKE256, mirrors keccak.rs bit-for-bit.
__device__ __constant__ uint64_t KRC[24] = {
 0x0000000000000001ULL,0x0000000000008082ULL,0x800000000000808aULL,0x8000000080008000ULL,
 0x000000000000808bULL,0x0000000080000001ULL,0x8000000080008081ULL,0x8000000000008009ULL,
 0x000000000000008aULL,0x0000000000000088ULL,0x0000000080008009ULL,0x000000008000000aULL,
 0x000000008000808bULL,0x800000000000008bULL,0x8000000000008089ULL,0x8000000000008003ULL,
 0x8000000000008002ULL,0x8000000000000080ULL,0x000000000000800aULL,0x800000008000000aULL,
 0x8000000080008081ULL,0x8000000000008080ULL,0x0000000080000001ULL,0x8000000080008008ULL};
__device__ __constant__ int KROT[25] = {0,1,62,28,27,36,44,6,55,20,3,10,43,25,39,41,45,15,21,8,18,2,61,56,14};
__device__ inline uint64_t rotl64(uint64_t x,int n){ return n? ((x<<n)|(x>>(64-n))) : x; }
// __noinline__ keeps keccak_f's 60 registers out of the hunt kernel's budget.
// Local st[25] copy lets NVCC register-allocate all 25 state words (no pointer aliasing).
// #pragma unroll inlines round constants and rotation amounts as immediates.
__device__ __noinline__ void keccak_f(uint64_t* s){
  uint64_t st[25];
  #pragma unroll
  for(int i=0;i<25;i++) st[i]=s[i];
  for(int round=0; round<24; round++){
    uint64_t c[5];
    #pragma unroll
    for(int x=0;x<5;x++) c[x]=st[x]^st[x+5]^st[x+10]^st[x+15]^st[x+20];
    uint64_t d[5];
    #pragma unroll
    for(int x=0;x<5;x++) d[x]=c[(x+4)%5]^rotl64(c[(x+1)%5],1);
    #pragma unroll
    for(int x=0;x<5;x++) for(int y=0;y<5;y++) st[x+5*y]^=d[x];
    uint64_t b[25];
    #pragma unroll
    for(int x=0;x<5;x++) for(int y=0;y<5;y++) b[y+5*((2*x+3*y)%5)]=rotl64(st[x+5*y],KROT[x+5*y]);
    #pragma unroll
    for(int x=0;x<5;x++) for(int y=0;y<5;y++) st[x+5*y]=b[x+5*y]^((~b[(x+1)%5+5*y])&b[(x+2)%5+5*y]);
    st[0]^=KRC[round];
  }
  #pragma unroll
  for(int i=0;i<25;i++) s[i]=st[i];
}
// Incremental SHAKE256 state (rate=136).
struct Shake { uint64_t st[25]; uint8_t buf[136]; int pos; };
__device__ void shake_init_from(Shake* s, const uint64_t* st0, const uint8_t* buf0, int pos0){
  for(int i=0;i<25;i++) s->st[i]=st0[i];
  for(int i=0;i<136;i++) s->buf[i]=buf0[i];
  s->pos=pos0;
}
__device__ void shake_absorb_block(Shake* s){
  for(int i=0;i<17;i++){ uint64_t w=0; for(int j=0;j<8;j++) w|=((uint64_t)s->buf[i*8+j])<<(8*j); s->st[i]^=w; }
  keccak_f(s->st);
}
__device__ void shake_absorb(Shake* s, const uint8_t* data, int len){
  for(int k=0;k<len;k++){ s->buf[s->pos++]=data[k]; if(s->pos==136){ shake_absorb_block(s); s->pos=0; } }
}
// Finalize + squeeze setup. After this, call squeeze_scalars or shake_squeeze.
// out union allows word-level access (little-endian == byte-level on all supported platforms).
struct Xof {
  uint64_t st[25];
  union { uint8_t bytes[136]; uint64_t words[17]; } out;
  int opos; // always a multiple of 8 when squeeze_scalars is used
};
__device__ void shake_finalize(Shake* s, Xof* x){
  for(int i=s->pos;i<136;i++) s->buf[i]=0;
  s->buf[s->pos]^=0x1F; s->buf[135]^=0x80;
  shake_absorb_block(s);
  for(int i=0;i<25;i++) x->st[i]=s->st[i];
  x->opos=136;
}
// Hot-path squeeze: read 64 bytes as two 4-word scalars directly, no byte serialize/deserialize.
// opos is always a multiple of 8 (starts at 136=17*8, decrements by 64=8*8).
// On little-endian GPUs, out.words[i] == little-endian u64 assembled from out.bytes[i*8..i*8+7].
__device__ void squeeze_scalars(Xof* x, uint64_t* k1, uint64_t* k2){
  if(x->opos==136){
    #pragma unroll
    for(int i=0;i<17;i++) x->out.words[i]=x->st[i];
    keccak_f(x->st);
    x->opos=0;
  }
  int ow=x->opos>>3;
  if(ow+8<=17){
    k1[0]=x->out.words[ow];   k1[1]=x->out.words[ow+1];
    k1[2]=x->out.words[ow+2]; k1[3]=x->out.words[ow+3];
    k2[0]=x->out.words[ow+4]; k2[1]=x->out.words[ow+5];
    k2[2]=x->out.words[ow+6]; k2[3]=x->out.words[ow+7];
    x->opos+=64;
  } else {
    // Block crossing: n words from current block, refill, 8-n from new.
    // n = 17-ow, range 1..7. Both n and 8-n are whole words (opos multiple of 8).
    int n=17-ow;
    uint64_t tmp[8];
    for(int i=0;i<n;i++) tmp[i]=x->out.words[ow+i];
    #pragma unroll
    for(int i=0;i<17;i++) x->out.words[i]=x->st[i];
    keccak_f(x->st);
    for(int i=n;i<8;i++) tmp[i]=x->out.words[i-n];
    x->opos=(8-n)<<3;
    k1[0]=tmp[0]; k1[1]=tmp[1]; k1[2]=tmp[2]; k1[3]=tmp[3];
    k2[0]=tmp[4]; k2[1]=tmp[5]; k2[2]=tmp[6]; k2[3]=tmp[7];
  }
}
// Byte-level squeeze kept for test_shake kernel.
__device__ void shake_squeeze(Xof* x, uint8_t* dst, int len){
  for(int k=0;k<len;k++){
    if(x->opos==136){ for(int i=0;i<17;i++) x->out.words[i]=x->st[i]; keccak_f(x->st); x->opos=0; }
    dst[k]=x->out.bytes[x->opos++];
  }
}
// test kernel: from prefix state, absorb `tail` (taillen bytes), squeeze outlen bytes
extern "C" __global__ void test_shake(const uint64_t* st0,const uint8_t* buf0,int pos0,const uint8_t* tail,int taillen,uint8_t* out,int outlen,int n){
  int i=blockIdx.x*blockDim.x+threadIdx.x; if(i>=n) return;
  Shake s; shake_init_from(&s, st0, buf0, pos0);
  shake_absorb(&s, tail, taillen);
  Xof x; shake_finalize(&s, &x);
  shake_squeeze(&x, out + i*outlen, outlen);
}
