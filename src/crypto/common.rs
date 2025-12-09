use crate::crypto::wrapper::{G1, G2, Fr};
use sha2::{Sha256, Digest};
use hex;

#[derive(Clone, Debug)]
pub struct PP {
    pub g1: G1, // Generator g
    pub g2: G2, // Generator g2 (for multisig PK)
    pub h: G1,  // Blinding generator h (for commitment)
}

impl PP {
    pub fn new() -> Self {
        let g1 = G1::generator();
        let g2 = G2::generator();
        
        // [关键修复] 生成确定性的 H
        // 1. 计算确定性的 Hash
        let mut hasher = Sha256::new();
        hasher.update(b"MBPChannel_H_Generator_Seed_v1"); // 固定种子
        let hash = hasher.finalize();
        
        // 2. 将 Hash 转换为 Hex 字符串
        let hex_hash = hex::encode(hash);
        
        // 3. 使用 wrapper 的 from_hex 生成确定的标量 r
        // 只要种子不变，r 就永远不变，h 也就永远不变
        let r = match Fr::from_hex(&hex_hash) {
            Ok(s) => s,
            Err(_) => {
                // 如果 Hash 转换失败（极低概率），回退到一个固定值
                Fr::from_u64(123456789) 
            }
        };

        let h = g1 * r; 

        PP { g1, g2, h }
    }
}