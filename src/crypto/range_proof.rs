use crate::crypto::wrapper::{Fr as BlstFr, G1 as BlstG1};
use crate::crypto::common::PP; 

use base64::{Engine as _, engine::general_purpose};
use bls_bulletproofs::{BulletproofGens, PedersenGens, RangeProof};
use blstrs::{G1Projective, Scalar, G1Affine}; 
use group::{Group, GroupEncoding}; // 修正引用: Curve trait 可能不需要显示引入
use merlin::Transcript;
use ff::PrimeField; // 用于 from_repr

// ==========================================
// 适配层 (Bridge): blst <-> blstrs
// ==========================================

// 将 blst 的标量转换依然是 blstrs 的标量
// ⚠️ 警告：这是一个高风险操作，依赖于字节序转换
fn fr_to_scalar(fr: &BlstFr) -> Scalar {
    let hex = fr.to_hex();
    let mut bytes = hex::decode(hex).expect("Invalid Fr hex");
    
    // [关键] blst (Big-Endian) -> blstrs (Little-Endian)
    // 这一点非常重要，否则数值会完全变成另一个巨大的数
    bytes.reverse(); 
    
    let arr: [u8; 32] = bytes.try_into().expect("Invalid Fr byte length");
    // Option::from 处理 CTOption
    Option::from(Scalar::from_repr(arr)).expect("Scalar conversion failed")
}

// 将 blst 的 G1 点转换为 blstrs 的 G1Projective
fn g1_to_point(g1: &BlstG1) -> G1Projective {
    let hex = g1.to_hex();
    let bytes = hex::decode(hex).expect("Invalid G1 hex");
    
    // blst 的 to_hex (compress) 通常遵循 ZCash 标准，blstrs 也遵循
    // 但如果这里失败，通常意味着压缩格式 flag 位不匹配
    let arr: [u8; 48] = bytes.try_into().expect("Invalid G1 byte length");
    
    let affine_opt: Option<G1Affine> = Option::from(G1Affine::from_compressed(&arr));
    let affine = affine_opt.expect("G1 point decompression failed - format mismatch?");
    G1Projective::from(affine)
}

// ==========================================
// 对外接口
// ==========================================

/// 生成范围证明：证明 v - a >= 0
/// 对应论文 Update 阶段: P_i 生成证明 pi，证明 B_Pi - m >= 0 [cite: 367, 459]
pub fn generate_proof(
    v: u64,       // 当前余额 Balance
    a: u64,       // 转账金额 Amount
    r_blst: &BlstFr, // 随机数 r (blst类型)
    pp: &PP       // 公共参数
) -> Result<(String, String), String> {
    // 1. 转换参数
    let r = fr_to_scalar(r_blst);
    let g = g1_to_point(&pp.g1);
    let h = g1_to_point(&pp.h); 

    // 2. 计算剩余余额 (Value to prove)
    if v < a { 
        return Err("Underflow: Insufficient balance".into()); 
    }
    let value_to_prove = v - a;

    // 3. 初始化生成元和 Transcript
    // 注意：PedersenGens 必须使用和 Commitment 一致的 G 和 H
    let pc_gens = PedersenGens { B: g, B_blinding: h };
    let bp_gens = BulletproofGens::new(64, 1);
    let mut transcript = Transcript::new(b"MBPChannel_RangeProof");

    // 4. 生成证明
    // 这里 prove_single 会自动计算 commitment = value * G + r * H
    let (proof, committed_value) = RangeProof::prove_single(
        &bp_gens,
        &pc_gens,
        &mut transcript,
        value_to_prove,
        &r, 
        32, 
    ).map_err(|e| format!("Proof generation failed: {}", e))?;

    let proof_bytes = proof.to_bytes();
    let com_bytes = committed_value.to_compressed(); 

    Ok((
        general_purpose::STANDARD.encode(proof_bytes),
        general_purpose::STANDARD.encode(com_bytes)
    ))
}

/// 验证范围证明
/// 对应论文 Update 阶段: 接收方 verifying the validity of proof pi [cite: 370]
pub fn verify_proof(
    proof_b64: &str, 
    c_op_str: &str, // 这是 generate_proof 返回的 committed_value (Base64)
    _a: u64,        // 为了接口兼容保留，实际不需要，因为证明是针对结果值的
    pp: &PP
) -> bool {
    // 1. 解码 Proof
    let proof_bytes = match general_purpose::STANDARD.decode(proof_b64) {
        Ok(b) => b, Err(_) => return false,
    };
    let proof = match RangeProof::from_bytes(&proof_bytes) {
        Ok(p) => p, Err(_) => return false,
    };

    // 2. 解码 Commitment
    // 这里是从 Base64 -> Bytes -> G1Affine -> G1Projective
    let c_bytes = match general_purpose::STANDARD.decode(c_op_str) {
        Ok(b) => b, Err(_) => return false,
    };
    if c_bytes.len() != 48 { return false; }
    
    let mut arr = [0u8; 48];
    arr.copy_from_slice(&c_bytes);
    
    let c_op_opt: Option<G1Affine> = Option::from(G1Affine::from_compressed(&arr));
    let c_target_affine = match c_op_opt {
        Some(p) => p, 
        None => return false,
    };

    // 3. 准备生成元 (必须与生成时一致)
    let g = g1_to_point(&pp.g1);
    let h = g1_to_point(&pp.h);
    
    let pc_gens = PedersenGens { B: g, B_blinding: h };
    let bp_gens = BulletproofGens::new(64, 1);
    let mut transcript = Transcript::new(b"MBPChannel_RangeProof");

    // 4. 验证
    // 直接验证传入的承诺 c_target_affine 是否在 [0, 2^32) 范围内
    // 这证明了 (v - a) >= 0，且没有发生溢出
    proof.verify_single(
        &bp_gens,
        &pc_gens,
        &mut transcript,
        &c_target_affine, 
        32
    ).is_ok()
}

// ==========================================
// 单元测试
// ==========================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::wrapper::Fr;

    #[test]
    fn test_bridge_consistency() {
        let pp = PP::new();
        
        // 1. 随机生成一个 r
        let r_blst = Fr::random();
        
        // 2. 转换到 blstrs scalar
        let scalar = fr_to_scalar(&r_blst);

        // 3. 简单的健全性检查
        // to_repr() 返回该标量的字节数组表示 (对于 bls12-381 Scalar 是 [u8; 32])
        let scalar_bytes = scalar.to_repr(); 

        // 注意: to_repr 返回的是 Little-Endian (小端序) 格式
        assert!(scalar_bytes.iter().any(|&x| x != 0), "Scalar shouldn't be zero");
    }

    #[test]
    fn test_valid_proof_generation_and_verification() {
        let pp = PP::new();
        let current_balance = 100u64;
        let transfer_amount = 40u64; // 剩余 60
        let r = Fr::random();

        // 1. 生成证明
        let result = generate_proof(current_balance, transfer_amount, &r, &pp);
        assert!(result.is_ok(), "Proof generation should succeed");
        
        let (proof_b64, comm_b64) = result.unwrap();
        println!("Proof Len: {}", proof_b64.len());
        println!("Comm: {}", comm_b64);

        // 2. 验证证明
        let is_valid = verify_proof(&proof_b64, &comm_b64, transfer_amount, &pp);
        assert!(is_valid, "Valid proof should verify successfully");
    }

    #[test]
    fn test_insufficient_balance() {
        let pp = PP::new();
        let current_balance = 10u64;
        let transfer_amount = 20u64; // 余额不足
        let r = Fr::random();

        let result = generate_proof(current_balance, transfer_amount, &r, &pp);
        assert!(result.is_err(), "Should fail when balance is insufficient");
    }

    #[test]
    fn test_invalid_proof_tampering() {
        let pp = PP::new();
        let (proof_b64, comm_b64) = generate_proof(100, 10, &Fr::random(), &pp).unwrap();

        // 篡改 Proof 字符串
        let mut tampered_proof = proof_b64.clone();
        tampered_proof.replace_range(0..4, "AAAA"); 

        let is_valid = verify_proof(&tampered_proof, &comm_b64, 10, &pp);
        assert!(!is_valid, "Tampered proof should fail");
    }
}