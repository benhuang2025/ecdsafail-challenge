// Jacobian point ops + double-and-add scalar mult on secp256k1 (a=0).
// Uses field ops from field.cuh (_ModMult/_ModSqr/_ModAdd256/_ModSub256/_ModInv).
__device__ __constant__ uint64_t GX[4] = {0x59F2815B16F81798ULL,0x029BFCDB2DCE28D9ULL,0x55A06295CE870B07ULL,0x79BE667EF9DCBBACULL};
__device__ __constant__ uint64_t GY[4] = {0x9C47D08FFB10D4B8ULL,0xFD17B448A6855419ULL,0x5DA4FBFC0E1108A8ULL,0x483ADA7726A3C465ULL};
__device__ __constant__ uint64_t PP[4] = {0xFFFFFFFEFFFFFC2FULL,0xFFFFFFFFFFFFFFFFULL,0xFFFFFFFFFFFFFFFFULL,0xFFFFFFFFFFFFFFFFULL};

__device__ void cp4(uint64_t*d,const uint64_t*s){d[0]=s[0];d[1]=s[1];d[2]=s[2];d[3]=s[3];}
__device__ int ge_p(const uint64_t*r){for(int i=3;i>=0;i--){if(r[i]<PP[i])return 0;if(r[i]>PP[i])return 1;}return 1;}
__device__ void canon(uint64_t*r){ // reduce to [0,p): result of _ModMult can exceed p by a little
  for(int k=0;k<4 && ge_p(r);k++){ USUBO1(r[0],PP[0]); USUBC1(r[1],PP[1]); USUBC1(r[2],PP[2]); USUB1(r[3],PP[3]); }
}

__device__ void jdouble(uint64_t*X,uint64_t*Y,uint64_t*Z){
  uint64_t A[4],B[4],C[4],D[4],E[4],F[4],t[4],t2[4],X3[4],Y3[4],Z3[4];
  _ModSqr(A,X); _ModSqr(B,Y); _ModSqr(C,B);
  _ModAdd256(t,X,B); _ModSqr(t,t); _ModSub256(t,t,A); _ModSub256(t,t,C); _ModAdd256(D,t,t);
  _ModAdd256(E,A,A); _ModAdd256(E,E,A);
  _ModSqr(F,E);
  _ModAdd256(t,D,D); _ModSub256(X3,F,t);
  _ModSub256(t,D,X3); _ModMult(t,E,t);
  _ModAdd256(t2,C,C); _ModAdd256(t2,t2,t2); _ModAdd256(t2,t2,t2);
  _ModSub256(Y3,t,t2);
  _ModMult(t,Y,Z); _ModAdd256(Z3,t,t);
  cp4(X,X3);cp4(Y,Y3);cp4(Z,Z3);
}

__device__ void jadd_aff(uint64_t*X1,uint64_t*Y1,uint64_t*Z1,const uint64_t*X2,const uint64_t*Y2,int*inf){
  if(*inf){ cp4(X1,X2);cp4(Y1,Y2);Z1[0]=1;Z1[1]=0;Z1[2]=0;Z1[3]=0; *inf=0; return; }
  uint64_t Z1Z1[4],U2[4],S2[4],H[4],HH[4],I[4],J[4],r[4],V[4],t[4],t2[4],X3[4],Y3[4],Z3[4];
  _ModSqr(Z1Z1,Z1);
  _ModMult(U2,(uint64_t*)X2,Z1Z1);
  _ModMult(S2,(uint64_t*)Y2,Z1); _ModMult(S2,S2,Z1Z1);
  _ModSub256(H,U2,X1);
  _ModSqr(HH,H);
  _ModAdd256(I,HH,HH); _ModAdd256(I,I,I);
  _ModMult(J,H,I);
  _ModSub256(r,S2,Y1); _ModAdd256(r,r,r);
  _ModMult(V,X1,I);
  _ModSqr(t,r); _ModSub256(t,t,J); _ModAdd256(t2,V,V); _ModSub256(X3,t,t2);
  _ModSub256(t,V,X3); _ModMult(t,r,t); _ModMult(t2,Y1,J); _ModAdd256(t2,t2,t2); _ModSub256(Y3,t,t2);
  _ModAdd256(t,Z1,H); _ModSqr(t,t); _ModSub256(t,t,Z1Z1); _ModSub256(Z3,t,HH);
  cp4(X1,X3);cp4(Y1,Y3);cp4(Z1,Z3);
}

__device__ void scalarmul(const uint64_t*k,uint64_t*ox,uint64_t*oy){
  uint64_t X[4],Y[4],Z[4]; int inf=1;
  for(int bit=255;bit>=0;bit--){
    if(!inf) jdouble(X,Y,Z);
    uint64_t kb=(k[bit>>6]>>(bit&63))&1ULL;
    if(kb) jadd_aff(X,Y,Z,GX,GY,&inf);
  }
  if(inf){ for(int i=0;i<4;i++){ox[i]=0;oy[i]=0;} return; }
  uint64_t zi[5]; cp4(zi,Z); zi[4]=0; _ModInv(zi);
  uint64_t z2[4],t[4]; _ModSqr(z2,zi); _ModMult(ox,X,z2); _ModMult(t,z2,zi); _ModMult(oy,Y,t);
  canon(ox); canon(oy);
}



// Windowed fixed-base scalar mult returning JACOBIAN (X,Y,Z), no inversion (for batched inverse).
__device__ void scalarmul_jac(const uint64_t* k, const uint64_t* TBL, uint64_t* oX, uint64_t* oY, uint64_t* oZ){
  uint64_t X[4],Y[4],Z[4]; int inf=1;
  for(int i=0;i<16;i++){
    int d=(int)((k[i>>2]>>(16*(i&3)))&0xffffULL);
    if(d){ const uint64_t* e=TBL+(((unsigned long long)i*65536ULL+d)*8ULL); jadd_aff(X,Y,Z,e,e+4,&inf); }
  }
  if(inf){ for(int j=0;j<4;j++){oX[j]=0;oY[j]=0;oZ[j]=0;} return; }
  cp4(oX,X); cp4(oY,Y); cp4(oZ,Z);
}
// affine (x,y) from Jacobian (X,Y,Z) given Zinv = 1/Z:  x=X*Zinv^2, y=Y*Zinv^3.
__device__ void affine_from_zinv(const uint64_t* X,const uint64_t* Y,const uint64_t* Zinv,uint64_t* x,uint64_t* y){
  uint64_t z2[4],z3[4]; _ModSqr(z2,(uint64_t*)Zinv); _ModMult(z3,z2,(uint64_t*)Zinv);
  _ModMult(x,(uint64_t*)X,z2); _ModMult(y,(uint64_t*)Y,z3); canon(x); canon(y);
}

__device__ void scalarmul_win(const uint64_t* k, const uint64_t* TBL, uint64_t* ox, uint64_t* oy){
  uint64_t X[4],Y[4],Z[4]; int inf=1;
  for(int i=0;i<32;i++){
    int d=(int)((k[i>>3]>>(8*(i&7)))&0xffULL);
    if(d){ const uint64_t* e=TBL+((i*256+d)*8); jadd_aff(X,Y,Z,e,e+4,&inf); }
  }
  if(inf){ for(int j=0;j<4;j++){ox[j]=0;oy[j]=0;} return; }
  uint64_t zi[5]; cp4(zi,Z); zi[4]=0; _ModInv(zi);
  uint64_t z2[4],t[4]; _ModSqr(z2,zi); _ModMult(ox,X,z2); _ModMult(t,z2,zi); _ModMult(oy,Y,t); canon(ox); canon(oy);
}

extern "C" __global__ void test_kg(const uint64_t*ks,const uint64_t*TBL,uint64_t*outxy,int n){
  int i=blockIdx.x*blockDim.x+threadIdx.x; if(i>=n)return;
  uint64_t k[4]; for(int j=0;j<4;j++)k[j]=ks[i*4+j];
  uint64_t ox[4],oy[4]; scalarmul_win(k,TBL,ox,oy);
  for(int j=0;j<4;j++){outxy[i*8+j]=ox[j];outxy[i*8+4+j]=oy[j];}
}
