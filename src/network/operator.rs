use crate::config::{AppConfig, ActorConfig};
use crate::crypto::common::PP;
use crate::crypto::{range_proof, multisig};
use crate::crypto::wrapper::{G1, G2};
use crate::network::message::NetworkMessage;
use crate::blockchain;
use std::error::Error;
use std::collections::{HashMap, HashSet, VecDeque};
use zeromq::{Socket, SocketRecv, SocketSend, RepSocket, PubSocket, ZmqMessage};
use alloy::primitives::{FixedBytes, keccak256, Address, U256};
use std::str::FromStr;
use uuid::Uuid;
use hex;
use base64::{Engine as _, engine::general_purpose};
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use bytes::Bytes;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::sync::broadcast; // [新增]

// ==========================================
// Core State Structure
// ==========================================
struct ChannelState {
    channel_alias: String,
    channel_id: FixedBytes<32>,
    pp: PP,
    multisig_keys: HashMap<String, G2>,      
    user_addresses: HashMap<String, Address>, 
    balance_commitments: HashMap<String, G1>, 
    finalized_txs: HashSet<u64>,       
    last_committed_sig: Option<String>,
    last_committed_tx_id: u64,
}

struct SharedContext {
    state: Mutex<ChannelState>,
    mempool: Mutex<VecDeque<NetworkMessage>>,
    notify: Notify,
}

// ==========================================
// Helper Functions
// ==========================================
fn get_actor_config(config: &AppConfig) -> ActorConfig {
    config.operator.clone()
}

async fn request_vote_from_user(host: &str, port: u16, req: &NetworkMessage) -> Option<String> {
    match TcpStream::connect(format!("{}:{}", host, port)).await {
        Ok(mut stream) => {
            let json = serde_json::to_string(req).unwrap();
            if let Err(_) = stream.write_all(json.as_bytes()).await { return None; }
            if let Err(_) = stream.shutdown().await { return None; }
            let mut buf = Vec::new();
            if let Ok(_) = stream.read_to_end(&mut buf).await {
                if let Ok(NetworkMessage::VoteResponse { status, sig_share_hex, .. }) = serde_json::from_slice(&buf) {
                    if status == "OK" { return sig_share_hex; }
                }
            }
        },
        Err(e) => println!("    ⚠️  Cannot connect Voter {}:{} ({})", host, port, e),
    }
    None
}

// ==========================================
// Main Logic
// ==========================================
pub async fn run(config: AppConfig) -> Result<(), Box<dyn Error>> {
    let op_config = &config.operator;
    let actor_cfg = get_actor_config(&config);

    // [新增] 创建停机信号通道
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);

    println!("==== OPERATOR STARTUP SEQUENCE (Sequencer Mode) ====");
    println!("👤 Identity: Operator");

    let contracts = config.contracts.as_ref().ok_or("config error: contracts missing")?;
    let rpc_url = &config.rpc_url;
    let payment_contract = contracts.payment_channel;

    let uuid = Uuid::new_v4();
    let channel_alias = format!("ch-{}", &uuid.to_string()[0..8]);
    let channel_id_bytes = keccak256(channel_alias.as_bytes());
    let channel_id_hex = hex::encode(channel_id_bytes);
    let initial_deposit = 20u128;

    println!("[1] Channel Generated: {} (ID: 0x{})", channel_alias, channel_id_hex);
    println!("[2] Registering Channel On-chain & Depositing...");
    
    match blockchain::create_channel(&actor_cfg, rpc_url, payment_contract, channel_id_bytes, initial_deposit).await {
        Ok(tx) => println!("✅ [Operator] Channel Create Success! Tx: {}", tx),
        Err(e) => return Err(e),
    }

    let pp = PP::new();
    let initial_state = ChannelState {
        channel_alias: channel_alias.clone(),
        channel_id: channel_id_bytes,
        pp: pp.clone(),
        multisig_keys: HashMap::new(),
        user_addresses: HashMap::new(), 
        balance_commitments: HashMap::new(), 
        finalized_txs: HashSet::from([0]), 
        last_committed_sig: None,
        last_committed_tx_id: 0,
    };

    let context = Arc::new(SharedContext {
        state: Mutex::new(initial_state),
        mempool: Mutex::new(VecDeque::new()),
        notify: Notify::new(),
    });

    let rep_port = op_config.port.unwrap_or(5555);
    let pub_port = rep_port + 1;

    let ctx_ingress = context.clone();
    let ctx_sequencer = context.clone();
    let config_sequencer = config.clone();
    let config_ingress = config.clone();
    
    let close_actor_cfg = actor_cfg.clone();
    let close_rpc_url = rpc_url.clone();
    let close_contract = payment_contract;

    // ---------------------------------------------------------
    // Task A: Ingress (Receiver)
    // ---------------------------------------------------------
    let shutdown_tx_ingress = shutdown_tx.clone();
    
    let task_ingress = tokio::spawn(async move {
        let mut rep_socket = RepSocket::new();
        rep_socket.bind(&format!("tcp://0.0.0.0:{}", rep_port)).await.expect("Bind Rep failed");
        println!("👂 [Ingress] Listening on port {} ...", rep_port);

        // [修复] 在 loop 外面创建 Receiver，保证生命周期
        let mut shutdown_rx_ingress = shutdown_tx_ingress.subscribe();

        loop {
            // [关键修改] 使用 tokio::select! 监听停机信号
            let msg: ZmqMessage = tokio::select! {
                res = rep_socket.recv() => match res {
                    Ok(m) => m,
                    Err(_) => continue,
                },
                _ = shutdown_rx_ingress.recv() => break,
            };

            let payload = msg.get(0).expect("Empty msg");
            let msg_str = String::from_utf8(payload.to_vec()).unwrap_or_default();
            
            let request: NetworkMessage = match serde_json::from_str(&msg_str) {
                Ok(m) => m, Err(_) => continue, 
            };

            let response = match request {
                NetworkMessage::JoinRequest { user_name, user_addr_str, pk_hex, initial_balance_comm_hex, initial_balance_proof_b64, initial_balance } => {
                    let mut state = ctx_ingress.state.lock().unwrap();
                    println!(">>> [JOIN] User Request: {} (Init: {})", user_name, initial_balance);
                    
                    if let (Ok(user_addr), Ok(pk), Ok(comm_g1)) = (Address::from_str(&user_addr_str), G2::from_hex(&pk_hex), G1::from_hex(&initial_balance_comm_hex)) {
                        let comm_bytes = hex::decode(&initial_balance_comm_hex).unwrap_or_default();
                        let comm_b64 = general_purpose::STANDARD.encode(comm_bytes);
                        
                        if range_proof::verify_proof(&initial_balance_proof_b64, &comm_b64, 0, &state.pp) {
                            state.multisig_keys.insert(user_name.clone(), pk);
                            state.user_addresses.insert(user_name.clone(), user_addr);
                            state.balance_commitments.insert(user_name.clone(), comm_g1);
                            
                            println!("    Status: {}/{} Users Ready", state.multisig_keys.len(), config_ingress.users.len());

                            if state.multisig_keys.len() == config_ingress.users.len() {
                                println!("🎉 [Ingress] All participants ready. Waking Sequencer.");
                                ctx_ingress.notify.notify_one(); 
                            }
                            NetworkMessage::JoinResponse { status: "OK".to_string(), message: format!("Welcome"), channel_id_hex: hex::encode(state.channel_id) }
                        } else {
                            NetworkMessage::JoinResponse { status: "ERR".to_string(), message: "Invalid Proof".to_string(), channel_id_hex: "".to_string() }
                        }
                    } else {
                        NetworkMessage::JoinResponse { status: "ERR".to_string(), message: "Bad Format".to_string(), channel_id_hex: "".to_string() }
                    }
                },

                NetworkMessage::UpdateProposal { 
                    user_name, counterparty_name, tx_id, prev_tx_id, amount,
                    tx_amount_comm_hex, range_proof_b64, proof_comm_value_b64, 
                    sender_new_comm_hex, receiver_new_comm_hex, 
                    proposer_ephemeral_pk_hex, proposer_signature_b64, 
                    counterparty_ephemeral_pk_hex, counterparty_signature_b64 
                } => {
                    let is_valid = {
                        let state = ctx_ingress.state.lock().unwrap();
                        if state.finalized_txs.contains(&tx_id) { false } 
                        else { state.balance_commitments.contains_key(&user_name) }
                    };

                    if is_valid {
                        let proposal = NetworkMessage::UpdateProposal {
                            user_name, counterparty_name, tx_id, prev_tx_id, amount,
                            tx_amount_comm_hex, range_proof_b64, proof_comm_value_b64,
                            sender_new_comm_hex, receiver_new_comm_hex,
                            proposer_ephemeral_pk_hex, proposer_signature_b64,
                            counterparty_ephemeral_pk_hex, counterparty_signature_b64
                        };

                        {
                            let mut mempool = ctx_ingress.mempool.lock().unwrap();
                            mempool.push_back(proposal);
                            println!("📥 [Ingress] Proposal Tx_{} queued (Size: {})", tx_id, mempool.len());
                        }

                        ctx_ingress.notify.notify_one();
                        NetworkMessage::UpdateResponse { status: "QUEUED".to_string(), message: "Queued".to_string(), aggregated_signature_hex: None }
                    } else {
                        NetworkMessage::UpdateResponse { status: "REJECT".to_string(), message: "Duplicate/Invalid".to_string(), aggregated_signature_hex: None }
                    }
                },

                NetworkMessage::CloseRequest { user_name, channel_id_hex, final_tx_id, signature_b64, final_balances } => {
                    println!("🛑 [Close] Request from {}", user_name);
                    
                    let (is_valid, recipients, amounts, channel_id) = {
                        let state = ctx_ingress.state.lock().unwrap();
                        let mut recps = Vec::new();
                        let mut amts = Vec::new();
                        
                        let valid = if hex::encode(state.channel_id) == channel_id_hex.trim_start_matches("0x") {
                             let mut sorted_users: Vec<String> = state.user_addresses.keys().cloned().collect();
                             sorted_users.sort(); 

                             for u in sorted_users {
                                 if let Some(addr) = state.user_addresses.get(&u) {
                                     recps.push(*addr);
                                     let val = final_balances.get(&u).copied().unwrap_or(0);
                                     amts.push(U256::from(val));
                                 }
                             }
                             true
                        } else { false };
                        (valid, recps, amts, state.channel_id)
                    };

                    if is_valid {
                        match blockchain::initiate_close(
                            &close_actor_cfg, &close_rpc_url, close_contract, 
                            channel_id, final_tx_id, recipients, amounts, 
                            general_purpose::STANDARD.decode(&signature_b64).unwrap_or_default()
                        ).await {
                            Ok(tx) => {
                                // 1. 将关闭通知推入 Mempool 供 Sequencer 广播
                                let broadcast = NetworkMessage::ChannelClosed {
                                    channel_id_hex: channel_id_hex.clone(),
                                    closer: user_name.clone(),
                                    final_tx_id,
                                };
                                {
                                    let mut mempool = ctx_ingress.mempool.lock().unwrap();
                                    mempool.push_back(broadcast);
                                }
                                ctx_ingress.notify.notify_one();

                                // 2. [关键修改] 触发 Operator 倒计时关闭
                                println!("⏳ [System] 收到关闭信号，Operator 将在 10 秒后自动退出...");
                                let _ = shutdown_tx_ingress.send(());

                                NetworkMessage::CloseConsensus { status: "OK".to_string(), final_tx_id, close_token: tx }
                            },
                            Err(e) => NetworkMessage::CloseConsensus { status: "ERROR".to_string(), final_tx_id: 0, close_token: e.to_string() }
                        }
                    } else {
                        NetworkMessage::CloseConsensus { status: "REJECT".to_string(), final_tx_id: 0, close_token: "Invalid ID".to_string() }
                    }
                },
                _ => NetworkMessage::UpdateResponse { status: "ERR".to_string(), message: "Unknown".to_string(), aggregated_signature_hex: None }
            };

            let resp_str = serde_json::to_string(&response).unwrap();
            let _ = rep_socket.send(ZmqMessage::from(resp_str)).await;
        }
    });

    // ---------------------------------------------------------
    // Task B: Sequencer (Ordering & Consensus)
    // ---------------------------------------------------------
    let mut shutdown_rx_sequencer = shutdown_tx.subscribe();

    let task_sequencer = tokio::spawn(async move {
        let mut pub_socket = PubSocket::new();
        pub_socket.bind(&format!("tcp://0.0.0.0:{}", pub_port)).await.expect("Bind Pub failed");
        println!("📢 [Sequencer] Broadcast Service started: {}", pub_port);

        let mut initial_broadcast_done = false;

        loop {
            // 0. Initial Broadcast
            let init_msg_opt = {
                let state = ctx_sequencer.state.lock().unwrap();
                let total_users = config_sequencer.users.len();
                
                if !initial_broadcast_done && state.multisig_keys.len() == total_users && total_users > 0 {
                    println!("📢 [Sequencer] Participants ready, preparing ST_0 broadcast...");
                    let mut participant_list = Vec::new();
                    
                    for u in &config_sequencer.users {
                        if let (Some(pk), Some(comm)) = (state.multisig_keys.get(&u.name), state.balance_commitments.get(&u.name)) {
                             let init_bal = match u.name.as_str() {
                                 "alice" => 10, "bob" => 11, "carol" => 12, _ => 10
                             };
                             participant_list.push((u.name.clone(), pk.to_hex(), comm.to_hex(), init_bal));
                        }
                    }
                    let final_msg = NetworkMessage::ChannelFinalized {
                        channel_id_hex: hex::encode(state.channel_id), 
                        participants: participant_list,
                    };
                    
                    if let Ok(json) = serde_json::to_string(&final_msg) {
                        let mut b_msg = ZmqMessage::from(Bytes::from("BROADCAST"));
                        b_msg.push_back(Bytes::from(json));
                        Some(b_msg)
                    } else { None }
                } else { None }
            };

            if let Some(msg) = init_msg_opt {
                let _ = pub_socket.send(msg).await;
                initial_broadcast_done = true;
            }

            // 1. Fetch
            let tx_opt = {
                let mut mempool = ctx_sequencer.mempool.lock().unwrap();
                mempool.pop_front()
            };

            // 使用 tokio::select! 监听通知或停机信号
            let proposal = match tx_opt {
                Some(tx) => tx,
                None => {
                    tokio::select! {
                        _ = ctx_sequencer.notify.notified() => continue, // 有新消息
                        _ = shutdown_rx_sequencer.recv() => break, // 收到停机信号
                    }
                }
            };

            // 2. Process based on type
            match proposal {
                NetworkMessage::ChannelClosed { channel_id_hex, closer, final_tx_id } => {
                    println!("📢 [Sequencer] Broadcasting ChannelClosed from {} (Tx #{})", closer, final_tx_id);
                    let msg = NetworkMessage::ChannelClosed { channel_id_hex, closer, final_tx_id };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        let mut b_msg = ZmqMessage::from(Bytes::from("BROADCAST"));
                        b_msg.push_back(Bytes::from(json));
                        let _ = pub_socket.send(b_msg).await;
                    }
                },

                NetworkMessage::UpdateProposal { 
                    user_name, counterparty_name, tx_id, amount,
                    tx_amount_comm_hex, range_proof_b64, proof_comm_value_b64, 
                    sender_new_comm_hex, receiver_new_comm_hex, ..
                } => {
                    let (current_nonce, last_sig, sender_old_hex, receiver_old_hex) = {
                        let state = ctx_sequencer.state.lock().unwrap();
                        let s_old = state.balance_commitments.get(&user_name).unwrap().to_hex();
                        let r_old = state.balance_commitments.get(&counterparty_name).unwrap().to_hex();
                        (state.last_committed_tx_id, state.last_committed_sig.clone(), s_old, r_old)
                    };

                    println!("▶️ [Sequencer] Processing Tx_{} (Linking to Prev: Tx_{})", tx_id, current_nonce);

                    let vote_req = NetworkMessage::VoteRequest {
                        tx_id,
                        proposer_name: user_name.clone(),
                        tx_amount_comm_hex: tx_amount_comm_hex.clone(),
                        range_proof_b64: range_proof_b64.clone(),
                        proof_comm_value_b64: proof_comm_value_b64.clone(),
                        sender_new_comm_hex: sender_new_comm_hex.clone(),
                        sender_old_comm_hex: sender_old_hex,
                        receiver_new_comm_hex: receiver_new_comm_hex.clone(),
                        receiver_old_comm_hex: receiver_old_hex,
                        prev_tx_id: current_nonce,
                        prev_aggregated_sig_hex: last_sig,
                    };

                    let mut collected_sigs = Vec::new();
                    for user_cfg in &config_sequencer.users {
                        if let (Some(host), Some(port)) = (&user_cfg.host, user_cfg.port) {
                            if let Some(sig_hex) = request_vote_from_user(host, port, &vote_req).await {
                                if let Ok(sig_g1) = G1::from_hex(&sig_hex) {
                                    collected_sigs.push(sig_g1);
                                }
                            }
                        }
                    }

                    let total_users = config_sequencer.users.len();
                    let threshold = (total_users * 2 + 2) / 3;

                    if collected_sigs.len() >= threshold {
                        if let Ok(agg_sig) = multisig::aggregate_signatures(collected_sigs) {
                            let agg_sig_hex = agg_sig.to_hex();
                            let new_s = G1::from_hex(&sender_new_comm_hex).unwrap();
                            let new_r = G1::from_hex(&receiver_new_comm_hex).unwrap();
                            
                            {
                                let mut state = ctx_sequencer.state.lock().unwrap();
                                state.finalized_txs.insert(tx_id);
                                state.last_committed_tx_id = tx_id;
                                state.last_committed_sig = Some(agg_sig_hex.clone());
                                state.balance_commitments.insert(user_name.clone(), new_s);
                                state.balance_commitments.insert(counterparty_name.clone(), new_r);
                            }

                            let consensus_msg = NetworkMessage::ConsensusReached {
                                tx_id,
                                status: "COMMITTED".to_string(),
                                all_signatures: vec![agg_sig_hex], 
                                sender_name: user_name.clone(),
                                sender_new_comm_hex,
                                receiver_name: counterparty_name.clone(),
                                receiver_new_comm_hex,
                                amount, 
                            };
                            
                            if let Ok(json) = serde_json::to_string(&consensus_msg) {
                                let mut b_msg = ZmqMessage::from(Bytes::from("BROADCAST"));
                                b_msg.push_back(Bytes::from(json));
                                let _ = pub_socket.send(b_msg).await;
                            }
                            println!("✅ [Sequencer] Tx_{} Committed & Broadcasted.", tx_id);
                        } else {
                            println!("❌ [Sequencer] Tx_{} Aggregation failed.", tx_id);
                        }
                    } else {
                        println!("❌ [Sequencer] Tx_{} Insufficient Votes ({}/{}).", tx_id, collected_sigs.len(), threshold);
                    }
                },
                _ => {}
            }
        }
    });

    // ---------------------------------------------------------
    // Main Wait Loop
    // ---------------------------------------------------------
    // [关键修改] 等待 shutdown 信号或任务 Panic
    tokio::select! {
        _ = shutdown_rx.recv() => {
            println!("🛑 [System] 正在等待广播发送完成 (Sleep 10s)...");
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            println!("👋 Operator 关闭。");
        },
        _ = task_ingress => {
            eprintln!("❌ Ingress task panicked!");
        },
        _ = task_sequencer => {
            eprintln!("❌ Sequencer task panicked!");
        }
    }

    Ok(())
}