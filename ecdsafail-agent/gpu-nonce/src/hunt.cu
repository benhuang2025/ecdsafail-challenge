// Chunk-batched per-nonce kernel (montgomery_batch_inverse pattern): C shots/chunk,
// 2 reciprocals per chunk instead of 3 per shot. SHAKE->kG(Jacobian)->batch-inv->affine
// ->affine-add x->factors->GCD filter. count_mode=1 counts all; 0 early-exits at first hard.
#define HC 8
__device__ const uint64_t ONE4[4]={1,0,0,0};
__device__ int eq4(const uint64_t*a,const uint64_t*b){return a[0]==b[0]&&a[1]==b[1]&&a[2]==b[2]&&a[3]==b[3];}
__device__ int isz4(const uint64_t*a){return (a[0]|a[1]|a[2]|a[3])==0ULL;}

// in-place batch inverse of n elements (each 4 u64). scratch pref[n][4]. 1 _ModInv total.
__device__ void batch_inv(uint64_t v[][4], int n, uint64_t pref[][4]){
  uint64_t acc[4]={1,0,0,0};
  for(int i=0;i<n;i++){ cp4(pref[i],acc); _ModMult(acc,v[i]); }
  uint64_t zi[5]; cp4(zi,acc); zi[4]=0; _ModInv(zi);
  for(int i=n-1;i>=0;i--){ uint64_t orig[4]; cp4(orig,v[i]); _ModMult(v[i],pref[i],zi); _ModMult(zi,orig); }
}

__device__ void parse_scalar(const uint8_t* rb, int off, uint64_t* k){
  for(int j=0;j<4;j++){ uint64_t w=0; for(int b=0;b<8;b++) w|=((uint64_t)rb[off+j*8+b])<<(8*b); k[j]=w; }
}

__device__ int g_lt4(const uint64_t*a,const uint64_t*b){ for(int i=3;i>=0;i--){if(a[i]<b[i])return 1;if(a[i]>b[i])return 0;} return 0; }
__device__ void submodp(uint64_t*r,const uint64_t*a,const uint64_t*b){
  if(!g_lt4(a,b)){ g_sub(r,a,b); } else { uint64_t t[4]; g_sub(t,b,a); g_sub(r,PP,t); }
}

extern "C" __global__ __launch_bounds__(64,5) void hunt(
    const uint64_t* st0, const uint8_t* buf0, int pos0, int nonce_bits, int xkind,
    const int* AW, const int* CB, const int* BW, int iters, int k2flag, int odd_u,
    unsigned long long nonce_start, int n, int target_shots, int count_mode, const uint64_t* TBL, int* out_hard)
{
  int idx=blockIdx.x*blockDim.x+threadIdx.x; if(idx>=n) return;
  unsigned long long nonce = nonce_start + (unsigned long long)idx;
  Shake s; shake_init_from(&s, st0, buf0, pos0);
  for(int i=0;i<nonce_bits;i++){
    unsigned long long q = ((nonce>>i)&1ULL)?1ULL:0ULL;
    for(int r=0;r<2;r++){
      uint8_t op[49]; op[0]=(uint8_t)xkind;
      uint64_t f[6]={~0ULL,~0ULL,q,~0ULL,~0ULL,~0ULL};
      for(int kk=0;kk<6;kk++) for(int b=0;b<8;b++) op[1+kk*8+b]=(uint8_t)(f[kk]>>(8*b));
      shake_absorb(&s, op, 49);
    }
  }
  Xof x; shake_finalize(&s,&x);
  int hard=0;
  uint64_t JXt[HC][4],JYt[HC][4],JZt[HC][4],JXo[HC][4],JYo[HC][4],JZo[HC][4];
  uint64_t xt[HC][4],yt[HC][4],xo[HC][4],yo[HC][4],den[HC][4];
  uint64_t zb[2*HC][4], pref[2*HC][4]; int skip[HC];
  for(int cs=0; cs<target_shots; cs+=HC){
    int fill = target_shots-cs; if(fill>HC) fill=HC;
    for(int j=0;j<fill;j++){
      uint64_t k1[4],k2[4]; squeeze_scalars(&x,k1,k2);
      scalarmul_jac(k1,TBL,JXt[j],JYt[j],JZt[j]);
      scalarmul_jac(k2,TBL,JXo[j],JYo[j],JZo[j]);
    }
    for(int j=0;j<fill;j++){
      skip[j] = (isz4(JZt[j])||isz4(JZo[j]))?1:0;
      cp4(zb[2*j],   skip[j]?ONE4:JZt[j]);
      cp4(zb[2*j+1], skip[j]?ONE4:JZo[j]);
    }
    batch_inv(zb, 2*fill, pref);
    for(int j=0;j<fill;j++){
      if(skip[j]){ cp4(den[j],ONE4); continue; }
      affine_from_zinv(JXt[j],JYt[j],zb[2*j],   xt[j],yt[j]);
      affine_from_zinv(JXo[j],JYo[j],zb[2*j+1], xo[j],yo[j]);
      if(eq4(xt[j],xo[j])){ skip[j]=1; cp4(den[j],ONE4); continue; }
      submodp(den[j], xo[j], xt[j]);
    }
    batch_inv(den, fill, pref);
    for(int j=0;j<fill;j++){
      if(skip[j]) continue;
      uint64_t num[4],lam[4],t[4],ex[4];
      _ModSub256(num,yo[j],yt[j]); _ModMult(lam,num,den[j]);
      _ModSqr(t,lam); _ModSub256(t,t,xt[j]); _ModSub256(ex,t,xo[j]); canon(ex);
      uint64_t dx[4],cc[4]; submodp(dx,xt[j],xo[j]); submodp(cc,xo[j],ex);
      int h = check_gcd_factor(dx,AW,CB,BW,iters,k2flag,odd_u) || check_gcd_factor(cc,AW,CB,BW,iters,k2flag,odd_u);
      if(h){ hard++; if(!count_mode){ out_hard[idx]=hard; return; } }
    }
  }
  out_hard[idx]=hard;
}
