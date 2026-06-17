// Fixed-base comb table for secp256k1: TBL[(i*256+d)*8 .. +8] = affine (x[4],y[4]) of
// (d * 256^i) * G. 32 windows (one per byte of k) x 256 digits. d=0 entry = (0,0).
use alloy_primitives::U256;
use quantum_ecc::weierstrass_elliptic_curve::WeierstrassEllipticCurve;
pub fn curve()->WeierstrassEllipticCurve{ WeierstrassEllipticCurve{
    modulus:U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F",16).unwrap(),
    a:U256::from(0u64),b:U256::from(7u64),
    gx:U256::from_str_radix("79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798",16).unwrap(),
    gy:U256::from_str_radix("483ADA7726A3C4655DA4FBFC0E1108A8FD17B448A68554199C47D08FFB10D4B8",16).unwrap(),
    order:U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141",16).unwrap()}}
pub fn build_gtable(cv:&WeierstrassEllipticCurve)->Vec<u64>{
    // 16-bit window comb: 16 windows x 65536 digits; TBL[(i*65536+d)*8..]=affine(d*65536^i*G)
    let mut tbl=vec![0u64; 16*65536*8];
    let mut base=(cv.gx,cv.gy); // 65536^0 * G
    for i in 0..16usize {
        let mut acc=(U256::ZERO,U256::ZERO);
        for d in 0..65536usize {
            let off=(i*65536+d)*8; let lx=acc.0.as_limbs(); let ly=acc.1.as_limbs();
            for j in 0..4 { tbl[off+j]=lx[j]; tbl[off+4+j]=ly[j]; }
            acc=cv.add(acc.0,acc.1,base.0,base.1);
        }
        for _ in 0..16 { base=cv.add(base.0,base.1,base.0,base.1); }
    }
    tbl
}
