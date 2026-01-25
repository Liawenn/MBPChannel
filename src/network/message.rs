use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum NetworkMessage {
    // [User -> Operator]
    JoinRequest {
        user_name: String,
        user_addr_str: String,
        pk_hex: String,
        initial_balance_comm_hex: String,
        initial_balance_proof_b64: String,
        initial_balance: u64, 
    },
    
    JoinResponse { status: String, message: String, channel_id_hex: String },

    ChannelFinalized {
        channel_id_hex: String,
        participants: Vec<(String, String, String, u64)>, 
    },

    // [User <-> User] P2P (这里必须保留 amount，因为接收方需要知道收了多少钱)
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

    // [User -> Operator]
    // ⚠️ 隐私修改点：删除了 amount 字段
    UpdateProposal {
        user_name: String,
        counterparty_name: String,
        tx_id: u64,
        prev_tx_id: u64,
        // amount: u64,  <-- 已删除，Operator 不再知道金额
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

    UpdateResponse { status: String, message: String, aggregated_signature_hex: Option<String> },

    VoteRequest {
        tx_id: u64,
        proposer_name: String,
        tx_amount_comm_hex: String,
        range_proof_b64: String,
        proof_comm_value_b64: String,
        sender_new_comm_hex: String,
        sender_old_comm_hex: String,
        receiver_new_comm_hex: String,
        receiver_old_comm_hex: String,
        prev_tx_id: u64,
        prev_aggregated_sig_hex: Option<String>,
    },

    VoteResponse {
        tx_id: u64,
        voter_name: String,
        status: String,
        sig_share_hex: Option<String>,
    },

    // [Sequencer -> All]
    // ⚠️ 隐私修改点：删除了 amount 字段
    ConsensusReached {
        tx_id: u64,
        status: String,
        all_signatures: Vec<String>,
        sender_name: String,
        sender_new_comm_hex: String,
        receiver_name: String,
        receiver_new_comm_hex: String,
        // amount: u64, <-- 已删除，全网广播不再包含金额
    },

    CloseRequest {
        user_name: String,
        channel_id_hex: String,
        final_tx_id: u64,
        signature_b64: String,
        final_balances: HashMap<String, u64>, 
    },

    CloseConsensus {
        status: String,
        final_tx_id: u64,
        close_token: String,
    },

    ChannelClosed {
        channel_id_hex: String,
        closer: String,
        final_tx_id: u64,
    }
}