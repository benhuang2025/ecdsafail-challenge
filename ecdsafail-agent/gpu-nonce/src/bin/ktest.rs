use sha3::{Shake256 as RShake, digest::{Update, ExtendableOutput, XofReader}};
#[path="../keccak.rs"] mod keccak;
fn mine(msg:&[u8], outlen:usize)->Vec<u8>{ let mut s=keccak::Shake256::new(); s.absorb(msg); let mut x=s.finalize(); let mut o=vec![0u8;outlen]; x.read(&mut o); o }
fn theirs(msg:&[u8], outlen:usize)->Vec<u8>{ let mut h=RShake::default(); h.update(msg); let mut r=h.finalize_xof(); let mut o=vec![0u8;outlen]; r.read(&mut o); o }
fn main(){
    for (name,msg) in [("empty",&b""[..]),("abc",&b"abc"[..]),("long",&[0x5au8;500][..]),("135",&[7u8;135][..]),("136",&[9u8;136][..]),("137",&[3u8;137][..])]{
        let a=mine(msg,200); let b=theirs(msg,200);
        println!("{}: {}", name, if a==b {"OK"} else {"MISMATCH"});
    }
}
