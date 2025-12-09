use crate::crypto::wrapper::{Fr, G1};
use crate::crypto::common::PP;
use std::ops::{Add, Sub};

// 对应论文 HomoCommit(pp, m)
// C(m) = g^m * h^r
pub fn commit(value: u64, r: Fr, pp: &PP) -> G1 {
    let m = Fr::from_u64(value);
    (pp.g1 * m) + (pp.h * r)
}

// [新增] 同态加法: C(A) + C(B) = C(A + B)
// 对应论文: C'(BP_j) = C(BP_j) + C(m) [cite: 523, 569]
pub fn homomorphic_add(c1: G1, c2: G1) -> G1 {
    c1 + c2 // 依赖 wrapper::G1 实现了 Add trait
}

// [新增] 同态减法: C(A) - C(B) = C(A - B)
// 对应论文: C'(BP_i) = C(BP_i) - C(m) 
pub fn homomorphic_sub(c1: G1, c2: G1) -> G1 {
    c1 - c2 // 依赖 wrapper::G1 实现了 Sub trait
}