use crate::config::{AppConfig};
use crate::crypto::common::PP;
use crate::crypto::{range_proof, multisig, commitment}; 
use crate::crypto::wrapper::{G1, G2};
use crate::network::message::NetworkMessage;
use crate::blockchain; 
use std::error::Error;
use std::collections::{HashMap, HashSet};
use zeromq::{Socket, SocketRecv, SocketSend, RepSocket, PubSocket, ZmqMessage}; 
use alloy::primitives::{FixedBytes, keccak256, Address, U256}; 
use std::str::FromStr;
use uuid::Uuid;
use hex;
use base64::{Engine as _, engine::general_purpose};
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use bytes::Bytes; 
use tokio::time::{self, Duration, Instant as TokioInstant, Interval}; 

// ==========================================
// 核心状态结构体
// ==========================================
struct ChannelState {
    channel_alias: String,
    channel_id: FixedBytes<32>,
    pp: PP,
    
    multisig_keys: HashMap<String, G2>,      
    user_addresses: HashMap<String, Address>, 
    balance_commitments: HashMap<String, G1>, 
    
    vote_pool: HashMap<u64, Vec<(String, G2, G1)>>, 
    dependency_map: HashMap<u64, u64>, 
    tx_prev_map: HashMap<u64, u64>,    
    finalized_txs: HashSet<u64>,       

    // ==========================================
    // [新增] 流水线共识缓存字段
    // ==========================================
    /// 上一笔交易的聚合签名 (Commit 证明)
    last_committed_sig: Option<String>,
    
    /// 上一笔成功 Commit 的交易 ID
    last_committed_tx_id: u64,
}

impl ChannelState {
    fn is_prev_finalized(&self, tx_id: u64) -> bool {
        if let Some(&prev_id) = self.tx_prev_map.get(&tx_id) {
            if prev_id == 0 { return true; }
            return self.finalized_txs.contains(&prev_id);
        }
        false
    }
}

// 辅助函数：向指定用户发起投票请求 (P2P TCP)
async fn request_vote_from_user(host: &str, port: u16, req: &NetworkMessage) -> Option<String> {
    match TcpStream::connect(format!("{}:{}", host, port)).await {
        Ok(mut stream) => {
            let json = serde_json::to_string(req).unwrap();
            if let Err(_) = stream.write_all(json.as_bytes()).await { return None; }
            if let Err(_) = stream.shutdown().await { return None; }
            
            let mut buf = Vec::new();
            if let Ok(_) = stream.read_to_end(&mut buf).await {
                if let Ok(NetworkMessage::VoteResponse { status, sig_share_hex, .. }) = serde_json::from_slice(&buf) {
                    if status == "OK" {
                        return sig_share_hex;
                    }
                }
            }
        },
        Err(e) => println!("    ⚠️  无法连接 Voter {}:{} ({})", host, port, e),
    }
    None
}

// ==========================================
// 主逻辑
// ==========================================
pub async fn run(config: AppConfig) -> Result<(), Box<dyn Error>> {
    let op_config = &config.operator;
    
    println!("\n==================================================");
    println!("   🚀 MBPChannel Operator (Pipeline Mode)        ");
    println!("   👤 Name:    {}", op_config.name);
    println!("==================================================\n");

    let contracts = config.contracts.as_ref().ok_or("config error: contracts missing")?;
    let rpc_url = &config.rpc_url;
    let payment_contract = contracts.payment_channel;

    // --- Phase 1: 准备通道参数 ---
    println!("==== [Phase 1] 准备通道参数 ====");
    let t_init_start = Instant::now(); 

    let uuid = Uuid::new_v4();
    let channel_alias = format!("ch-{}", &uuid.to_string()[0..8]);
    let channel_id_bytes = keccak256(channel_alias.as_bytes());

    println!("🆔 拟定 Channel ID: {}", channel_alias);
    let pp = PP::new();
    println!("⚙️  公共参数 (PP) 初始化完成 ({:.2?})", t_init_start.elapsed()); 

    // --- Phase 2: 链上注册 (Create) ---
    println!("\n==== [Phase 2] 链上注册 (Create) ====");
    let t_chain_start = Instant::now(); 
    let initial_deposit = 100u128; 
    
    match blockchain::create_channel(op_config, rpc_url, payment_contract, channel_id_bytes, initial_deposit).await {
        Ok(tx_hash) => println!("✅ 通道注册成功！Tx: {} ({:.2?})", tx_hash, t_chain_start.elapsed()),
        Err(e) => eprintln!("❌ 注册失败: {}", e),
    }

    // --- Phase 3: 启动服务 ---
    println!("\n==== [Phase 3] 启动共识服务 ====");
    
    let mut state = ChannelState {
        channel_alias: channel_alias.clone(),
        channel_id: channel_id_bytes,
        pp: pp.clone(),
        multisig_keys: HashMap::new(),
        user_addresses: HashMap::new(), 
        balance_commitments: HashMap::new(), 
        vote_pool: HashMap::new(),
        dependency_map: HashMap::new(),
        tx_prev_map: HashMap::new(),
        finalized_txs: HashSet::new(),
        // [Init] 流水线初始状态
        last_committed_sig: None,
        last_committed_tx_id: 0,
    };
    state.finalized_txs.insert(0); 

    let rep_port = op_config.port.unwrap_or(5555);
    
    // 1. 初始化 REQ-REP Socket (用于接收 Join/Update)
    let mut rep_socket = RepSocket::new();
    let rep_addr = format!("tcp://0.0.0.0:{}", rep_port);
    rep_socket.bind(&rep_addr).await?;
    println!("🌊 服务监听中 (REP): {}", rep_addr);

    // 2. 初始化 PUB Socket (用于广播 ST_0 和 Update结果)
    let pub_port = rep_port + 1; // 默认 5556
    let mut pub_socket = PubSocket::new();
    let pub_addr = format!("tcp://0.0.0.0:{}", pub_port);
    pub_socket.bind(&pub_addr).await?;
    println!("📡 广播服务已启动 (PUB): {}", pub_addr);
    
    // [Liveness 机制新增] 初始化定时器，用于强制提交最后一笔交易
    const LIVENESS_TIMEOUT_MS: u64 = 120_000; // 2分钟
    // 显式声明类型，避免歧义
    let mut liveness_timer: Interval = time::interval_at(
        TokioInstant::now() + Duration::from_millis(LIVENESS_TIMEOUT_MS), 
        Duration::from_millis(LIVENESS_TIMEOUT_MS)
    );
    println!("⏳ 活性检查定时器启动，间隔: 2分钟");


    // 使用 tokio::select! 同时监听网络请求和定时器事件
    loop {
        tokio::select! {
            // Case 1: 接收到网络请求 (优先级高)
            rep_result = rep_socket.recv() => {
                let msg: ZmqMessage = match rep_result {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let payload = msg.get(0).ok_or("Empty msg")?;
                let msg_str = String::from_utf8(payload.to_vec())?;
                
                let request: NetworkMessage = match serde_json::from_str(&msg_str) {
                    Ok(m) => m,
                    Err(_) => continue, 
                };

                let response = match request {
                    // ... (JoinRequest, UpdateProposal, CloseRequest 的处理逻辑不变) ...
                    
                    // [JoinRequest]
                    NetworkMessage::JoinRequest { user_name, user_addr_str, pk_hex, initial_balance_comm_hex, initial_balance_proof_b64 } => {
                        let t_join_start = Instant::now();
                        println!(">>> [JOIN] 用户申请: {}", user_name);
                        
                        if let (Ok(user_addr), Ok(pk), Ok(comm_g1)) = (Address::from_str(&user_addr_str), G2::from_hex(&pk_hex), G1::from_hex(&initial_balance_comm_hex)) {
                            let comm_bytes = hex::decode(&initial_balance_comm_hex).unwrap_or_default();
                            let comm_b64 = general_purpose::STANDARD.encode(comm_bytes);
                            
                            if range_proof::verify_proof(&initial_balance_proof_b64, &comm_b64, 0, &state.pp) {
                                println!("   ✅ 隐私证明验证通过");
                                let _ = blockchain::join_channel(op_config, rpc_url, payment_contract, state.channel_id, 100).await;
                                
                                state.multisig_keys.insert(user_name.clone(), pk);
                                state.user_addresses.insert(user_name.clone(), user_addr);
                                state.balance_commitments.insert(user_name.clone(), comm_g1);
                                
                                println!("⏱️  [Time] 用户 {} 加入耗时: {:.2?}", user_name, t_join_start.elapsed());
                                
                                // [检查是否人齐]
                                let expected_count = config.users.len();
                                let current_count = state.multisig_keys.len();
                                
                                if current_count == expected_count {
                                    println!("\n🎉 [System] 所有参与者已就位 ({}/{})。", current_count, expected_count);
                                    println!("⏳ 正在等待网络同步，准备广播...");

                                    // 1. 构建 ST_0
                                    let mut participant_list = Vec::new();
                                    for u in &config.users {
                                        if let (Some(pk), Some(comm)) = (state.multisig_keys.get(&u.name), state.balance_commitments.get(&u.name)) {
                                            participant_list.push((u.name.clone(), pk.to_hex(), comm.to_hex()));
                                        }
                                    }

                                    // 2. 构建 Finalized 消息
                                    let final_msg = NetworkMessage::ChannelFinalized {
                                        channel_id_hex: hex::encode(state.channel_id.0), 
                                        participants: participant_list,
                                    };

                                    // 3. 广播逻辑 (Create Phase)
                                    if let Ok(json_payload) = serde_json::to_string(&final_msg) {
                                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                                        let mut broadcast_msg = ZmqMessage::from(Bytes::from("BROADCAST"));
                                        broadcast_msg.push_back(Bytes::from(json_payload));

                                        let _ = pub_socket.send(broadcast_msg).await;
                                        println!("📡 [Broadcast] 全员状态已推送！");
                                    }
                                }

                                NetworkMessage::JoinResponse { status: "OK".to_string(), message: format!("Welcome to {}", state.channel_alias), channel_id_hex: format!("{}", state.channel_id) }
                            } else {
                                NetworkMessage::JoinResponse { status: "ERR".to_string(), message: "Invalid Proof".to_string(), channel_id_hex: "".to_string() }
                            }
                        } else {
                            NetworkMessage::JoinResponse { status: "ERR".to_string(), message: "Bad Format".to_string(), channel_id_hex: "".to_string() }
                        }
                    },
                    
                    // [UpdateProposal]
                    NetworkMessage::UpdateProposal { 
                        user_name, counterparty_name, tx_id, prev_tx_id, tx_amount_comm_hex, 
                        range_proof_b64, proof_comm_value_b64, sender_new_comm_hex, receiver_new_comm_hex, 
                        proposer_ephemeral_pk_hex, proposer_signature_b64, 
                        counterparty_ephemeral_pk_hex, counterparty_signature_b64,
                    } => {
                        if state.finalized_txs.contains(&tx_id) {
                            NetworkMessage::UpdateResponse { status: "DONE".to_string(), message: "Finalized".to_string(), aggregated_signature_hex: None }
                        } else {
                            println!("\n📨 [Pipeline Leader] 收到提案 Tx_{} (Prev: Tx_{})", tx_id, prev_tx_id);
                            
                            // --- 0. 验证 Pipeline 顺序性 ---
                            if prev_tx_id != state.last_committed_tx_id {
                                println!("    ⚠️  [Warn] 提案的 PrevTx ({}) 与本地 Commit 记录 ({}) 不符，可能存在分叉或延迟。", prev_tx_id, state.last_committed_tx_id);
                            }

                            let mut is_local_valid = false;
                            let sender_old_opt = state.balance_commitments.get(&user_name);
                            let receiver_old_opt = state.balance_commitments.get(&counterparty_name);

                            if let (Some(&c_s_old), Some(&c_r_old)) = (sender_old_opt, receiver_old_opt) {
                                if let (Ok(c_m), Ok(c_s_new), Ok(c_r_new)) = (
                                    G1::from_hex(&tx_amount_comm_hex), G1::from_hex(&sender_new_comm_hex), G1::from_hex(&receiver_new_comm_hex)
                                ) {
                                    let expected_s_new = commitment::homomorphic_sub(c_s_old, c_m);
                                    let expected_r_new = commitment::homomorphic_add(c_r_old, c_m);
                                    
                                    if c_s_new == expected_s_new && c_r_new == expected_r_new {
                                        if range_proof::verify_proof(&range_proof_b64, &proof_comm_value_b64, 0, &state.pp) {
                                            let msg_to_sign = general_purpose::STANDARD.decode(&range_proof_b64).unwrap_or_default();
                                            
                                            let mut sigs_ok = true;
                                            // 验证 Proposer
                                            if let (Ok(pk), Ok(sig_bytes)) = (G2::from_hex(&proposer_ephemeral_pk_hex), general_purpose::STANDARD.decode(&proposer_signature_b64)) {
                                                if let Ok(sig) = G1::from_hex(&hex::encode(sig_bytes)) {
                                                    if !multisig::verify_aggregate(sig, pk, &msg_to_sign, &state.pp) { sigs_ok = false; }
                                                }
                                            }
                                            // 验证 Counterparty
                                            if let (Some(pk_hex), Some(sig_b64)) = (counterparty_ephemeral_pk_hex, counterparty_signature_b64) {
                                                if let (Ok(pk), Ok(sig_bytes)) = (G2::from_hex(&pk_hex), general_purpose::STANDARD.decode(&sig_b64)) {
                                                    if let Ok(sig) = G1::from_hex(&hex::encode(sig_bytes)) {
                                                        if !multisig::verify_aggregate(sig, pk, &msg_to_sign, &state.pp) { sigs_ok = false; }
                                                    }
                                                }
                                            }

                                            if sigs_ok {
                                                is_local_valid = true;
                                                println!("    ✅ [Verify] 本地预验证通过");
                                            } else { println!("    ❌ 签名验证失败"); }
                                        } else { println!("    ❌ NIZK 验证失败"); }
                                    } else { println!("    ❌ 同态状态计算不匹配"); }
                                } else { println!("    ❌ 承诺格式解析错误"); }
                            } else { println!("    ❌ 未找到交易方状态 (Sender/Receiver 不存在)"); }

                            if is_local_valid {
                                let sender_old = state.balance_commitments.get(&user_name).unwrap();
                                let receiver_old = state.balance_commitments.get(&counterparty_name).unwrap();

                                // --- [核心修改] 构建 Pipeline VoteRequest ---
                                let vote_req = NetworkMessage::VoteRequest {
                                    // 当前交易 Verify 数据
                                    tx_id,
                                    proposer_name: user_name.clone(),
                                    tx_amount_comm_hex: tx_amount_comm_hex.clone(),
                                    range_proof_b64: range_proof_b64.clone(),
                                    proof_comm_value_b64: proof_comm_value_b64.clone(),
                                    sender_new_comm_hex: sender_new_comm_hex.clone(),
                                    sender_old_comm_hex: sender_old.to_hex(),
                                    receiver_new_comm_hex: receiver_new_comm_hex.clone(),
                                    receiver_old_comm_hex: receiver_old.to_hex(),
                                    
                                    // [捎带] 上一笔交易 Commit 数据
                                    prev_tx_id: state.last_committed_tx_id, 
                                    prev_aggregated_sig_hex: state.last_committed_sig.clone(),
                                };

                                let mut collected_sigs = Vec::new();
                                
                                // 广播 P2P 投票请求
                                for user_cfg in &config.users {
                                    if let (Some(host), Some(port)) = (&user_cfg.host, user_cfg.port) {
                                        if let Some(sig_hex) = request_vote_from_user(host, port, &vote_req).await {
                                            if let Ok(sig_g1) = G1::from_hex(&sig_hex) {
                                                collected_sigs.push(sig_g1);
                                            }
                                        }
                                    }
                                }

                                let total_users = config.users.len();
                                let threshold = if total_users == 0 { 0 } else { (total_users * 2 + 2) / 3 }; 

                                println!("    📊 [Consensus] 收集签名: {}/{} (阈值: {})", collected_sigs.len(), total_users, threshold);

                                if collected_sigs.len() >= threshold {
                                    if let Ok(agg_sig) = multisig::aggregate_signatures(collected_sigs) {
                                        let agg_sig_hex = agg_sig.to_hex();
                                        
                                        state.finalized_txs.insert(tx_id);
                                        state.tx_prev_map.insert(tx_id, prev_tx_id);
                                        
                                        // 更新最新状态
                                        state.balance_commitments.insert(user_name.clone(), G1::from_hex(&sender_new_comm_hex).unwrap());
                                        state.balance_commitments.insert(counterparty_name.clone(), G1::from_hex(&receiver_new_comm_hex).unwrap());
                                        
                                        // [关键] 更新 Pipeline 缓存，重置定时器
                                        state.last_committed_sig = Some(agg_sig_hex.clone());
                                        state.last_committed_tx_id = tx_id;
                                        
                                        println!("    🎉 [Pipeline] Tx_{} 本地聚合成功！等待随下一笔交易广播。", tx_id);
                                        
                                        // [最终修复] 避免使用 reset() 导致编译报错
                                        // 使用新的 time::interval_at 替换掉旧的计时器，从当前时间开始计时 2 分钟
                                        liveness_timer = time::interval_at(
                                            TokioInstant::now() + Duration::from_millis(LIVENESS_TIMEOUT_MS),
                                            Duration::from_millis(LIVENESS_TIMEOUT_MS)
                                        );

                                        NetworkMessage::UpdateResponse { 
                                            status: "OK".to_string(), 
                                            message: "Queued for pipeline commit".to_string(), 
                                            aggregated_signature_hex: None 
                                        }
                                    } else {
                                        NetworkMessage::UpdateResponse { status: "ERR".to_string(), message: "Agg Failed".to_string(), aggregated_signature_hex: None }
                                    }
                                } else {
                                    NetworkMessage::UpdateResponse { status: "REJECT".to_string(), message: "Consensus Failed".to_string(), aggregated_signature_hex: None }
                                }
                            } else {
                                NetworkMessage::UpdateResponse { status: "REJECT".to_string(), message: "Local Verify Failed".to_string(), aggregated_signature_hex: None }
                            }
                        }
                    },

                    // [Close]
                    NetworkMessage::CloseRequest { user_name, final_tx_id, signature_b64, .. } => {
                        let t_close_start = Instant::now();
                        println!("🛑 [Close] 收到用户 {} 的关闭请求 (用户声称 Tx: {})", user_name, final_tx_id);

                        let mut is_sig_valid = false;
                        if let Some(pk) = state.multisig_keys.get(&user_name) {
                            if let Ok(sig_bytes) = general_purpose::STANDARD.decode(&signature_b64) {
                                if let Ok(sig_g1) = G1::from_hex(&hex::encode(sig_bytes)) {
                                    if multisig::verify_aggregate(sig_g1, *pk, b"CLOSE_REQUEST", &state.pp) {
                                        println!("   ✅ 用户签名验证通过");
                                        is_sig_valid = true;
                                    }
                                }
                            }
                        }

                        if !is_sig_valid {
                            NetworkMessage::UpdateResponse { status: "REJECT".to_string(), message: "Invalid Signature".to_string(), aggregated_signature_hex: None }
                        } else {
                            let actual_latest_id = state.finalized_txs.iter().max().copied().unwrap_or(0);

                            if final_tx_id == actual_latest_id {
                                println!("   ✅ 状态验证通过 (Nonce: {} 是最新的)", final_tx_id);
                                let mut recipients = Vec::new();
                                let mut final_commitments = Vec::new();
                                for (user, addr) in &state.user_addresses {
                                    if let Some(comm) = state.balance_commitments.get(user) {
                                        recipients.push(*addr);
                                        let comm_bytes = hex::decode(comm.to_hex()).unwrap_or_default();
                                        let comm_hash = keccak256(&comm_bytes);
                                        final_commitments.push(U256::from_be_bytes(comm_hash.0));
                                    }
                                }
                                let _ = blockchain::initiate_close(op_config, rpc_url, payment_contract, state.channel_id, final_tx_id, recipients, final_commitments, general_purpose::STANDARD.decode(&signature_b64).unwrap_or_default()).await;
                                println!("   ⛓️  关闭交易已提交至区块链");
                                println!("⏱️  [Time] Close 阶段总耗时: {:.2?}", t_close_start.elapsed());
                                NetworkMessage::CloseConsensus { status: "OK".to_string(), final_tx_id, close_token: format!("settled_at_{}", final_tx_id) }
                            } else if final_tx_id < actual_latest_id {
                                println!("   🚨 [Fraud] 欺诈检测: 用户提交过旧状态!");
                                let _ = blockchain::dispute_close(op_config, rpc_url, payment_contract, state.channel_id, actual_latest_id, vec![], vec![], vec![]).await;
                                NetworkMessage::PunishmentTriggered { cheater_name: user_name, submitted_tx_id: final_tx_id, actual_latest_tx_id: actual_latest_id, proof: "fraud_proof".to_string() }
                            } else {
                                NetworkMessage::UpdateResponse { status: "REJECT".to_string(), message: "Invalid Future State".to_string(), aggregated_signature_hex: None }
                            }
                        }
                    },

                    _ => NetworkMessage::UpdateResponse { status: "ERR".to_string(), message: "Unknown".to_string(), aggregated_signature_hex: None }
                };

                let resp_str = serde_json::to_string(&response)?;
                rep_socket.send(ZmqMessage::from(resp_str)).await?;
            }, // rep_socket.recv() 结束

            // Case 2: 定时器超时 (Liveness 检查)
            _ = liveness_timer.tick() => {
                if state.last_committed_sig.is_some() {
                    // 只有当有聚合签名被缓存 (即 Tx_k 已 Prepare 但未 Commit) 且没有新的 Proposal 时才需要强制提交
                    println!("\n⏰ [Liveness Timeout] 2分钟未收到新提案。强制提交 Tx_{}...", state.last_committed_tx_id);
                    
                    // 构建一个 ConsensusReached 消息，将其视作最终 Commit 的广播
                    let final_commit_msg = NetworkMessage::ConsensusReached {
                        tx_id: state.last_committed_tx_id,
                        status: "TIMEOUT_COMMIT".to_string(),
                        all_signatures: state.last_committed_sig.clone().into_iter().collect(), // 传输缓存的签名
                        // 注意: 这里需要从 state 中重新获取当前的余额信息来广播
                        sender_name: "N/A".to_string(), // 简化处理
                        sender_new_comm_hex: "N/A".to_string(), // 简化处理
                        receiver_name: "N/A".to_string(), // 简化处理
                        receiver_new_comm_hex: "N/A".to_string(), // 简化处理
                    };
                    
                    if let Ok(json_payload) = serde_json::to_string(&final_commit_msg) {
                        let mut broadcast_msg = ZmqMessage::from(Bytes::from("BROADCAST"));
                        broadcast_msg.push_back(Bytes::from(json_payload));
                        
                        let _ = pub_socket.send(broadcast_msg).await;
                        println!("📡 [Broadcast] 强制 Commit 消息已推送。");
                        
                        // 清理缓存，防止重复提交
                        state.finalized_txs.insert(state.last_committed_tx_id);
                        state.last_committed_sig = None;
                        state.last_committed_tx_id = 0;
                    }
                }
            } // liveness_timer.tick() 结束
        } // tokio::select! 结束
    } // loop 结束
}