use rand::{Rng, RngCore};
use std::ops::{Add, Mul, Sub};
use std::fmt;
use blst::*;

// --- 标量域 (Fr) ---
#[derive(Clone, Copy, PartialEq)]
pub struct Fr(pub blst_fr);

impl fmt::Debug for Fr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fr({})", self.to_hex())
    }
}

impl Fr {
    pub fn from_u64(val: u64) -> Self {
        let mut bytes = [0u8; 32];
        let val_bytes = val.to_le_bytes(); 
        for i in 0..8 { bytes[i] = val_bytes[i]; }
        let mut scalar = unsafe { blst_scalar::default() };
        unsafe { blst_scalar_from_le_bytes(&mut scalar, bytes.as_ptr(), 32) };
        let mut ret = unsafe { blst_fr::default() };
        unsafe { blst_fr_from_scalar(&mut ret, &scalar) };
        Fr(ret)
    }

    pub fn to_hex(&self) -> String {
        let mut scalar = unsafe { blst_scalar::default() };
        unsafe { blst_scalar_from_fr(&mut scalar, &self.0) };
        let mut bytes = [0u8; 32];
        unsafe { blst_bendian_from_scalar(bytes.as_mut_ptr(), &scalar) };
        hex::encode(bytes)
    }

    pub fn from_hex(s: &str) -> Result<Self, String> {
        let bytes = hex::decode(s).map_err(|e| e.to_string())?;
        if bytes.len() != 32 { return Err("Invalid Fr hex length".into()); }
        let mut scalar = unsafe { blst_scalar::default() };
        unsafe { blst_scalar_from_bendian(&mut scalar, bytes.as_ptr()) };
        let mut ret = unsafe { blst_fr::default() };
        unsafe { blst_fr_from_scalar(&mut ret, &scalar) };
        Ok(Fr(ret))
    }

    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        let mut ikm = [0u8; 32];
        rng.fill_bytes(&mut ikm);
        let mut scalar = unsafe { blst_scalar::default() };
        unsafe { blst_scalar_from_le_bytes(&mut scalar, ikm.as_ptr(), 32) };
        let mut ret = unsafe { blst_fr::default() };
        unsafe { blst_fr_from_scalar(&mut ret, &scalar) };
        Fr(ret)
    }

    pub fn zero() -> Self { Fr(unsafe { blst_fr::default() }) }
    
    pub fn inverse(&self) -> Self {
        let mut ret = unsafe { blst_fr::default() };
        unsafe { blst_fr_eucl_inverse(&mut ret, &self.0) };
        Fr(ret)
    }
}

// --- G1 群 ---
#[derive(Clone, Copy)]
pub struct G1(pub blst_p1);

impl fmt::Debug for G1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "G1({})", self.to_hex())
    }
}

impl G1 {
    pub fn generator() -> Self { unsafe { G1(*blst_p1_generator()) } }
    
    pub fn to_hex(&self) -> String {
        let mut bytes = [0u8; 48];
        unsafe { blst_p1_compress(bytes.as_mut_ptr(), &self.0) };
        hex::encode(bytes)
    }
    
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let bytes = hex::decode(s).map_err(|e| e.to_string())?;
        if bytes.len() != 48 { return Err("Invalid G1 hex length".into()); }
        let mut p1 = unsafe { blst_p1::default() };
        let mut p1_aff = unsafe { blst_p1_affine::default() };
        let err = unsafe { blst_p1_uncompress(&mut p1_aff, bytes.as_ptr()) };
        if err != BLST_ERROR::BLST_SUCCESS { return Err("G1 uncompress failed".into()); }
        unsafe { blst_p1_from_affine(&mut p1, &p1_aff) };
        Ok(G1(p1))
    }

    // [新增] 安全的 Hash-to-Curve 绑定
    pub fn hash_to_curve(msg: &[u8], dst: &[u8]) -> Self {
        let mut p1 = unsafe { blst_p1::default() };
        unsafe {
            blst_hash_to_g1(
                &mut p1,            // 输出点
                msg.as_ptr(),       // 消息指针
                msg.len(),          // 消息长度
                dst.as_ptr(),       // 域分离标签(DST)指针
                dst.len(),          // DST长度
                std::ptr::null(),   // aug (可选信息，通常为空)
                0                   // aug长度
            );
        }
        G1(p1)
    }
}

// --- G2 群 ---
#[derive(Clone, Copy)]
pub struct G2(pub blst_p2);

impl fmt::Debug for G2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "G2({})", self.to_hex())
    }
}

impl G2 {
    pub fn generator() -> Self { unsafe { G2(*blst_p2_generator()) } }
    
    pub fn to_hex(&self) -> String {
        let mut bytes = [0u8; 96];
        unsafe { blst_p2_compress(bytes.as_mut_ptr(), &self.0) };
        hex::encode(bytes)
    }
    
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let bytes = hex::decode(s).map_err(|e| e.to_string())?;
        if bytes.len() != 96 { return Err("Invalid G2 hex length".into()); }
        let mut p2 = unsafe { blst_p2::default() };
        let mut p2_aff = unsafe { blst_p2_affine::default() };
        let err = unsafe { blst_p2_uncompress(&mut p2_aff, bytes.as_ptr()) };
        if err != BLST_ERROR::BLST_SUCCESS { return Err("G2 uncompress failed".into()); }
        unsafe { blst_p2_from_affine(&mut p2, &p2_aff) };
        Ok(G2(p2))
    }
}

// --- 运算符重载 ---
impl Add for Fr { type Output = Fr; fn add(self, rhs: Self) -> Self::Output { let mut ret = unsafe { blst_fr::default() }; unsafe { blst_fr_add(&mut ret, &self.0, &rhs.0) }; Fr(ret) } }
impl Mul for Fr { type Output = Fr; fn mul(self, rhs: Self) -> Self::Output { let mut ret = unsafe { blst_fr::default() }; unsafe { blst_fr_mul(&mut ret, &self.0, &rhs.0) }; Fr(ret) } }
impl Sub for Fr { type Output = Fr; fn sub(self, rhs: Self) -> Self::Output { let mut ret = unsafe { blst_fr::default() }; unsafe { blst_fr_sub(&mut ret, &self.0, &rhs.0) }; Fr(ret) } }

// 补充 G2 的加法实现 (调用 blst_p2_add)
impl Add for G2 {
    type Output = G2;
    fn add(self, rhs: Self) -> Self::Output {
        let mut ret = unsafe { blst_p2::default() };
        unsafe { blst_p2_add(&mut ret, &self.0, &rhs.0) };
        G2(ret)
    }
}

// (可选) 建议顺便把减法也加上，以备不时之需
impl Sub for G2 {
    type Output = G2;
    fn sub(self, rhs: Self) -> Self::Output {
        // G2 减法: a - b = a + (-b)
        let mut rhs_neg = rhs.0;
        unsafe { blst_p2_cneg(&mut rhs_neg, true) }; // 取逆元
        let mut ret = unsafe { blst_p2::default() };
        unsafe { blst_p2_add(&mut ret, &self.0, &rhs_neg) };
        G2(ret)
    }
}


impl Add for G1 { type Output = G1; fn add(self, rhs: Self) -> Self::Output { let mut ret = unsafe { blst_p1::default() }; unsafe { blst_p1_add(&mut ret, &self.0, &rhs.0) }; G1(ret) } }
impl Sub for G1 { type Output = G1; fn sub(self, rhs: Self) -> Self::Output { let mut rhs_neg = rhs.0; unsafe { blst_p1_cneg(&mut rhs_neg, true) }; let mut ret = unsafe { blst_p1::default() }; unsafe { blst_p1_add(&mut ret, &self.0, &rhs_neg) }; G1(ret) } }
impl Mul<Fr> for G1 { type Output = G1; fn mul(self, rhs: Fr) -> Self::Output { let mut ret = unsafe { blst_p1::default() }; let mut scalar = unsafe { blst_scalar::default() }; unsafe { blst_scalar_from_fr(&mut scalar, &rhs.0) }; unsafe { blst_p1_mult(&mut ret, &self.0, scalar.b.as_ptr(), 255) }; G1(ret) } }
impl PartialEq for G1 { fn eq(&self, other: &Self) -> bool { unsafe { blst_p1_is_equal(&self.0, &other.0) } } }

impl Mul<Fr> for G2 { type Output = G2; fn mul(self, rhs: Fr) -> Self::Output { let mut ret = unsafe { blst_p2::default() }; let mut scalar = unsafe { blst_scalar::default() }; unsafe { blst_scalar_from_fr(&mut scalar, &rhs.0) }; unsafe { blst_p2_mult(&mut ret, &self.0, scalar.b.as_ptr(), 255) }; G2(ret) } }

// --- 配对函数 ---
pub fn pairing(p2: G2, p1: G1) -> blst_fp12 {
    let mut p1_aff = unsafe { blst_p1_affine::default() };
    let mut p2_aff = unsafe { blst_p2_affine::default() };
    unsafe { blst_p1_to_affine(&mut p1_aff, &p1.0) };
    unsafe { blst_p2_to_affine(&mut p2_aff, &p2.0) };
    let mut res = unsafe { blst_fp12::default() };
    unsafe { blst_miller_loop(&mut res, &p2_aff, &p1_aff) };
    let mut final_res = unsafe { blst_fp12::default() };
    unsafe { blst_final_exp(&mut final_res, &res) };
    final_res
}
pub fn fp12_eq(a: &blst_fp12, b: &blst_fp12) -> bool { unsafe { blst_fp12_is_equal(a, b) } }