// Portable Keccak-f[1600] / SHAKE256, mirrored bit-for-bit in keccak.cu (device).
// Validated against the sha3 crate so the GPU Fiat-Shamir derivation matches the harness.
const RC: [u64;24] = [
 0x0000000000000001,0x0000000000008082,0x800000000000808a,0x8000000080008000,
 0x000000000000808b,0x0000000080000001,0x8000000080008081,0x8000000000008009,
 0x000000000000008a,0x0000000000000088,0x0000000080008009,0x000000008000000a,
 0x000000008000808b,0x800000000000008b,0x8000000000008089,0x8000000000008003,
 0x8000000000008002,0x8000000000000080,0x000000000000800a,0x800000008000000a,
 0x8000000080008081,0x8000000000008080,0x0000000080000001,0x8000000080008008];
const ROT: [u32;25] = [0,1,62,28,27, 36,44,6,55,20, 3,10,43,25,39, 41,45,15,21,8, 18,2,61,56,14];

pub fn keccak_f1600(s: &mut [u64;25]) {
    for round in 0..24 {
        let mut c=[0u64;5];
        for x in 0..5 { c[x]=s[x]^s[x+5]^s[x+10]^s[x+15]^s[x+20]; }
        let mut d=[0u64;5];
        for x in 0..5 { d[x]=c[(x+4)%5]^c[(x+1)%5].rotate_left(1); }
        for x in 0..5 { for y in 0..5 { s[x+5*y]^=d[x]; } }
        let mut b=[0u64;25];
        for x in 0..5 { for y in 0..5 { b[y+5*((2*x+3*y)%5)]=s[x+5*y].rotate_left(ROT[x+5*y]); } }
        for x in 0..5 { for y in 0..5 { s[x+5*y]=b[x+5*y]^((!b[(x+1)%5+5*y])&b[(x+2)%5+5*y]); } }
        s[0]^=RC[round];
    }
}

pub struct Shake256 { pub st:[u64;25], pub buf:[u8;136], pub pos:usize }
impl Shake256 {
    pub fn new()->Self{ Shake256{st:[0;25],buf:[0;136],pos:0} }
    pub fn absorb(&mut self,data:&[u8]){
        for &byte in data {
            self.buf[self.pos]=byte; self.pos+=1;
            if self.pos==136 { self.absorb_block(); self.pos=0; }
        }
    }
    fn absorb_block(&mut self){
        for i in 0..17 { let mut w=[0u8;8]; w.copy_from_slice(&self.buf[i*8..i*8+8]); self.st[i]^=u64::from_le_bytes(w); }
        keccak_f1600(&mut self.st);
    }
    // finalize absorbing (SHAKE 0x1F pad) -> ready to squeeze. Returns the squeeze state.
    pub fn finalize(mut self)->ShakeXof{
        // pad: buf[pos]^=0x1F ; buf[135]^=0x80 (within current rate block)
        for i in self.pos..136 { self.buf[i]=0; }
        self.buf[self.pos]^=0x1F; self.buf[135]^=0x80;
        self.absorb_block();
        ShakeXof{ st:self.st, out:[0u8;136], opos:136 }
    }
}
pub struct ShakeXof{ pub st:[u64;25], pub out:[u8;136], pub opos:usize }
impl ShakeXof{
    pub fn read(&mut self,dst:&mut [u8]){
        for d in dst.iter_mut(){
            if self.opos==136 { for i in 0..17 { let b=self.st[i].to_le_bytes(); self.out[i*8..i*8+8].copy_from_slice(&b);} keccak_f1600(&mut self.st); self.opos=0; }
            *d=self.out[self.opos]; self.opos+=1;
        }
    }
}
