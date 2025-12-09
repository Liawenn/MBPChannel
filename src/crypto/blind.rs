use crate::crypto::wrapper::{Fr, G1}; // 注意：main 逻辑不需要 G2，只有 test 需要
use crate::crypto::multisig; // 需要确保 multisig::hash_to_g1 是 pub 的

/// 盲化结果结构体
pub struct BlindedMessage {
    pub point: G1,      // 盲化后的消息点 M'
    pub r: Fr,          // 盲化因子 r (去盲时需要)
}

// 1. [User] 盲化消息
// 输入: 原始消息 msg
// 输出: 盲化点 M', 盲化因子 r
pub fn blind(msg: &[u8]) -> BlindedMessage {
    // 1. H(m)
    // [重要提示] 请确保 src/crypto/multisig.rs 中的 hash_to_g1 函数已改为 pub
    let h_m = multisig::hash_to_g1(msg); 
    
    // 2. 生成随机盲化因子 r
    let r = Fr::random();
    
    // 3. M' = H(m) * r
    let blinded_point = h_m * r;
    
    BlindedMessage {
        point: blinded_point,
        r,
    }
}

// 2. [Signer] 对盲化消息签名
// 输入: 盲化点 M', 私钥 sk
// 输出: 盲化签名 sigma'
pub fn sign_blinded(sk: Fr, blinded_point: G1) -> G1 {
    // sigma' = M' * sk
    blinded_point * sk
}

// 3. [User] 去盲
// 输入: 盲化签名 sigma', 盲化因子 r
// 输出: 标准签名 sigma
pub fn unblind(blinded_sig: G1, r: Fr) -> G1 {
    // sigma = sigma' * r^-1
    let r_inv = r.inverse();
    blinded_sig * r_inv
}

// 验证函数直接复用 multisig::verify_aggregate 即可
// 因为去盲后的签名就是标准的 BLS 签名

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::multisig::{KeyPair, verify_aggregate};
    use crate::crypto::common::PP;

    #[test]
    fn test_blind_signature_flow() {
        let pp = PP::new();
        let msg = b"Secret Transaction Content";

        // 1. Signer (User B) 拥有密钥
        let signer_keys = KeyPair::generate(&pp);

        // 2. User A 想获得签名，但不想暴露消息
        // A 进行盲化
        let blinded = blind(msg);
        println!("🔒 消息已盲化");
        
        // --- 网络传输: A -> B (发送 blinded.point) ---

        // 3. Signer (B) 签名盲化点
        // B 根本不知道 msg 是什么，只能看到一个随机点
        let blinded_sig = sign_blinded(signer_keys.sk, blinded.point);
        println!("✍️  Signer 已盲签");

        // --- 网络传输: B -> A (发送 blinded_sig) ---

        // 4. User A 去盲
        let real_sig = unblind(blinded_sig, blinded.r);
        println!("🔓 签名已去盲");

        // 5. 验证 (Leader)
        // Leader 验证的是：real_sig 是否由 signer_pk 对 原始 msg 的签名
        let is_valid = verify_aggregate(real_sig, signer_keys.pk, msg, &pp);
        
        assert!(is_valid, "Blind signature verification failed!");
        println!("✅ 盲签名流程验证成功！Signer 在不知情的情况下完成了签名。");
    }
}