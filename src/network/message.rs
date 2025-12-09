use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum NetworkMessage {
    // [Phase 1] Create / Join (保持不变)
    JoinRequest { 
        user_name: String, 
        user_addr_str: String, 
        pk_hex: String,
        initial_balance_comm_hex: String, 
        initial_balance_proof_b64: String 
    },
    JoinResponse { status: String, message: String, channel_id_hex: String },

    // ==========================================
    // [Phase 1] 通道确立广播消息
    // 当 Operator 确认人齐后，通过 PUB Socket 广播此消息
    // ==========================================
    ChannelFinalized {
        channel_id_hex: String,
        // 列表结构: (用户名, 公钥PK_Hex, 初始承诺Comm_Hex)
        participants: Vec<(String, String, String)>,
    },

    // [Phase 2] Update (Pipeline Consensus - Proposer <-> Operator)
    UpdateProposal {
        user_name: String,        
        counterparty_name: String, 
        
        tx_id: u64,
        prev_tx_id: u64, 
        
        tx_amount_comm_hex: String, 
        range_proof_b64: String,    
        proof_comm_value_b64: String, 

        sender_new_comm_hex: String,
        receiver_new_comm_hex: String,
        
        proposer_ephemeral_pk_hex: String,      
        proposer_signature_b64: String, 
        
        counterparty_ephemeral_pk_hex: Option<String>, 
        counterparty_signature_b64: Option<String>, 
    },
    
    // 注意：在流水线模式下，中间交易的共识结果通常通过下一笔 VoteRequest 捎带。
    // 但此消息仍保留，用于：
    // 1. 广播最后一笔交易的共识（没有下一笔来捎带时）。
    // 2. 超时强制提交。
    ConsensusReached { 
        tx_id: u64, 
        status: String, 
        // 聚合签名 (用于验证 Leader 确实收集了足够的票)
        all_signatures: Vec<String>,
        // 状态更新信息
        sender_name: String,
        sender_new_comm_hex: String,
        receiver_name: String,
        receiver_new_comm_hex: String,
    },
    
    UpdateResponse { status: String, message: String, aggregated_signature_hex: Option<String> },

    // [Phase 3] Close (保持不变)
    CloseRequest { user_name: String, channel_id_hex: String, final_tx_id: u64, signature_b64: String },
    CloseConsensus { status: String, final_tx_id: u64, close_token: String },
    PunishmentTriggered { cheater_name: String, submitted_tx_id: u64, actual_latest_tx_id: u64, proof: String },

    // --- P2P 消息 (Proposer <-> Counterparty) ---
    P2PUpdateReq {
        tx_id: u64,
        amount: u64, 
        commitment_hex: String, 
        range_proof_b64: String,
        comm_value_b64: String,
        blinded_tx_point_hex: String, 
    },

    P2PUpdateResp {
        tx_id: u64,
        status: String, 
        ephemeral_pk_hex: Option<String>, 
        blinded_signature_hex: Option<String>,
        receiver_new_comm_hex: Option<String>, 
    },

    // ==========================================
    // 全员共识投票消息 (Operator <-> All Users)
    // 用于 UPDATE-STATEVER 阶段的广播与收集
    // ==========================================
    VoteRequest {
        // --- 当前交易 (Tx_k) 的 Prepare 数据 ---
        tx_id: u64,
        proposer_name: String,
        
        // 验证所需的密码学数据 (Proof & Commitment)
        tx_amount_comm_hex: String, // C(m)
        range_proof_b64: String,    // pi
        proof_comm_value_b64: String, 
        
        // 状态转换验证数据
        sender_new_comm_hex: String,   // C'(B_i)
        sender_old_comm_hex: String,   // C(B_i)
        receiver_new_comm_hex: String, // C'(B_j)
        receiver_old_comm_hex: String, // C(B_j)

        // ==========================================
        // [新增] 流水线共识关键字段 (Pipeline Piggybacking)
        // 对应论文 Fig.7: Leader Broadcasts (..., Sigma_{k-1}, ...)
        // ==========================================
        
        /// 上一笔交易的 ID (Tx_{k-1})
        prev_tx_id: u64,
        
        /// 上一笔交易的聚合签名 (Commit Proof)
        /// 如果 prev_tx_id > 0，此字段必须存在且有效，用户据此 Finalize 上一笔交易
        prev_aggregated_sig_hex: Option<String>,
    },

    VoteResponse {
        tx_id: u64,
        voter_name: String,
        status: String, // "OK" or "REJECT"
        // 用户的多签份额 (使用长期私钥签名)
        sig_share_hex: Option<String>, 
    }
}