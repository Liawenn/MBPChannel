use crate::crypto::wrapper::{Fr, G1, G2, pairing, fp12_eq};
use crate::crypto::common::PP;

// =========================================================================
// Fig. 1: (n,t)-Aggregable Multi-Signature Scheme Implementation
// =========================================================================

pub struct KeyPair {
    pub sk: Fr,
    pub pk: G2, // BLS 公钥通常在 G2 (假设签名在 G1)
}

impl KeyPair {
    pub fn generate(pp: &PP) -> Self {
        let sk = Fr::random();
        // pk_l <- g2^sk_l
        let pk = pp.g2 * sk; 
        KeyPair { sk, pk }
    }
}

// [cite: 243] MulSig: 生成签名份额
// sigma_i = H0(Tx)^sk_i
pub fn sign(sk: Fr, msg: &[u8]) -> G1 {
    // 1. 将消息安全地映射到 G1 曲线上的点 (Hash-to-Curve)
    let h_tx = hash_to_g1(msg);
    // 2. 进行标量乘法: H(m)^sk
    h_tx * sk
}

// MulAgg (Part 1): 聚合签名
// Sigma <- Prod(sigma_i)
// Leader 使用此函数将收集到的部分签名聚合成一个主签名
pub fn aggregate_signatures(sigs: Vec<G1>) -> Result<G1, String> {
    if sigs.is_empty() {
        return Err("Cannot aggregate empty signatures".to_string());
    }
    // 假设 wrapper::G1 实现了 Add Trait
    let mut agg = sigs[0];
    for i in 1..sigs.len() {
        agg = agg + sigs[i];
    }
    Ok(agg)
}

// MulAgg (Part 2): 聚合公钥
// cpk <- Prod(pk_i)
// 验证者需要将所有签名者的公钥聚合，才能验证聚合签名
pub fn aggregate_public_keys(pks: Vec<G2>) -> Result<G2, String> {
    if pks.is_empty() {
        return Err("Cannot aggregate empty public keys".to_string());
    }
    // 假设 wrapper::G2 实现了 Add Trait (请确保 wrapper.rs 中添加了 impl Add for G2)
    let mut agg_pk = pks[0];
    for i in 1..pks.len() {
        agg_pk = agg_pk + pks[i];
    }
    Ok(agg_pk)
}

// [cite: 245] MulVer: 验证聚合签名
// e(Sigma, g2) = e(H0(Tx), cpk)
pub fn verify_aggregate(
    sigma: G1,  // 聚合签名 Sigma
    cpk: G2,    // 聚合公钥 cpk (必须是参与签名的所有 pk 的和)
    msg: &[u8], // 交易内容 Tx
    pp: &PP     // 公共参数 (包含 g2)
) -> bool {
    let h_tx = hash_to_g1(msg);
    
    // 验证双线性对等式:
    // LHS: e(sigma, g2) -> 这里的 pairing 顺序依赖 wrapper 实现，通常是 pairing(G2, G1)
    let lhs = pairing(pp.g2, sigma);
    
    // RHS: e(H(m), cpk) -> e(H(m), Sum(pk_i))
    // 因为双线性性质 e(a, b) = e(b, a)^(-1) 或对称，这里对应 G2 位置放 cpk
    let rhs = pairing(cpk, h_tx);
    
    fp12_eq(&lhs, &rhs)
}

// [核心修复] 安全的 Hash-to-G1 实现
// H0: {0,1}* -> G1
// 使用 blst 底层的 hash_to_curve (SSWU Map) 算法
pub fn hash_to_g1(msg: &[u8]) -> G1 {
    // Domain Separation Tag (DST)
    // 必须是唯一的，用于隔离不同协议的哈希空间
    let dst = b"MBPChannel-V1-SIG-G1"; 
    
    // 调用 wrapper::G1::hash_to_curve
    // 确保你的 wrapper.rs 中已经添加了此方法
    G1::hash_to_curve(msg, dst)
}