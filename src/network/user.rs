use crate::config::AppConfig;
use crate::crypto::common::PP;
use crate::crypto::{range_proof, multisig, commitment, blind}; 
use crate::crypto::multisig::KeyPair;
use crate::crypto::wrapper::{Fr, G1}; 
use crate::network::message::NetworkMessage;
use crate::blockchain; 
use std::error::Error;
use zeromq::{Socket, SocketRecv, SocketSend, ReqSocket, SubSocket, ZmqMessage}; 
use base64::{Engine as _, engine::general_purpose};
use crate::crypto::wrapper::G2;
use hex;
use std::sync::{Arc, Mutex}; 
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, AsyncBufReadExt}; 
use std::io::Write; 
use std::collections::{HashMap, HashSet}; 
use alloy::primitives::{FixedBytes, U256}; 

// ==========================================
// Local State Structure
// ==========================================
struct LocalState {
    balance: u64,
    r: Fr,
    comm: G1,
    current_tx_id: u64, 
    all_pks: HashMap<String, G2>,
    all_commitments: HashMap<String, G1>,
    all_balances: HashMap<String, u64>,
    participant_order: Vec<String>,
    finalized_txs: HashSet<u64>,
    tx_history_data: HashMap<u64, Vec<u8>>,
}

// ==========================================
// Helper Functions
// ==========================================
async fn send_p2p_msg(target_host: &str, target_port: u16, msg: &NetworkMessage) -> Result<NetworkMessage, Box<dyn Error>> {
    let addr = format!("{}:{}", target_host, target_port);
    let mut stream = TcpStream::connect(addr).await?;
    let json = serde_json::to_string(msg)?;
    stream.write_all(json.as_bytes()).await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let resp: NetworkMessage = serde_json::from_slice(&buf)?;
    Ok(resp)
}

// ==========================================
// Main Logic (User Node)
// ==========================================
pub async fn run(config: AppConfig, user_name: String, init_deposit: Option<u128>) -> Result<(), Box<dyn Error>> {
    println!("=== 👤 User 节点启动: {} ===", user_name);

    let my_config = config.users.iter().find(|u| u.name == user_name).ok_or("Config user not found")?;
    let my_port = my_config.port.unwrap_or(6000);
    
    let pp = Arc::new(PP::new());
    let pp_clone_for_cli = pp.clone(); 
    let my_long_term_keys = KeyPair::generate(&pp); 
    
    let deposit_amount = init_deposit.unwrap_or(10) as u64;
    let r_init = Fr::random(); 
    let initial_comm = commitment::commit(deposit_amount, r_init, &pp);
    
    // 初始化本地状态
    let local_state = Arc::new(Mutex::new(LocalState {
        balance: deposit_amount,
        r: r_init,
        comm: initial_comm,
        current_tx_id: 0, 
        all_pks: HashMap::new(),
        all_commitments: HashMap::new(),
        all_balances: HashMap::new(), 
        participant_order: Vec::new(), 
        finalized_txs: HashSet::from([0]), 
        tx_history_data: HashMap::new(),     
    }));

    // [Phase 0] 输入 Channel ID
    println!("\n⚠️  [Wait] 请输入 Operator 生成的 Channel ID (Hex format, e.g., 2b37...):");
    print!("> "); 
    std::io::stdout().flush()?;
    
    let mut input_buf = String::new();
    std::io::stdin().read_line(&mut input_buf)?;
    let clean_hex = input_buf.trim().trim_start_matches("0x");
    let channel_id_bytes = FixedBytes::<32>::from_slice(&hex::decode(clean_hex).map_err(|_| "无效的 Hex ID")?);
    let channel_id_hex = hex::encode(channel_id_bytes);

    // [Phase 1] 链上交互
    let rpc_url = &config.rpc_url;
    let payment_contract = config.contracts.as_ref().unwrap().payment_channel;
    let deposit_wei = deposit_amount as u128;

    println!("\n==== [Phase 1] 正在执行链上交互 ====");
    
    // Step 1: 锁仓
    println!("[1] 正在执行资金锁仓 ({} wei)...", deposit_wei);
    match blockchain::lock_deposit(my_config, rpc_url, payment_contract, channel_id_bytes, deposit_wei).await {
        Ok(tx) => println!("✅ [User] 锁仓成功! Tx: {}", tx),
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("already known") || err_msg.contains("nonce too low") {
                println!("⚠️ [提示] 充值交易已存在 (Already Known)，继续下一步...");
            } else {
                eprintln!("⚠️ 锁仓警告: {}", e);
            }
        }
    }

    println!("⏳ 正在等待交易确认 (Sleep 2s)...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Step 2: 加入通道
    println!("[2] 正在调用 joinChannel (上链)...");
    match blockchain::join_channel(my_config, rpc_url, payment_contract, channel_id_bytes).await {
        Ok(tx) => println!("✅ [User] 加入通道成功! Tx: {}", tx),
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("Already joined") || err_str.contains("execution reverted") {
                println!("⚠️ [提示] 检测到链上已存在身份 (Already Joined)。");
                println!("   -> 跳过注册步骤，直接进入状态同步阶段...");
            } else {
                eprintln!("❌ 加入失败: {}", e);
                return Err(e);
            }
        }
    }

    // [Phase 2] 网络连接
    let op_host = config.operator.host.clone().unwrap_or("127.0.0.1".to_string());
    let op_rep_port = config.operator.port.unwrap_or(5555);
    
    println!("\n==== [Phase 2] 连接 Operator ({}:{}) ====", op_host, op_rep_port);
    
    let mut zmq_socket = ReqSocket::new();
    if let Err(e) = zmq_socket.connect(&format!("tcp://{}:{}", op_host, op_rep_port)).await {
        return Err(format!("❌ 无法连接 Operator REQ: {}", e).into());
    }

    let op_pub_port = op_rep_port + 1; 
    let mut sub_socket = SubSocket::new();
    sub_socket.connect(&format!("tcp://{}:{}", op_host, op_pub_port)).await?;
    sub_socket.subscribe("BROADCAST").await?; 
    
    let state_for_sub = local_state.clone(); 
    
    // Task 1: 广播监听
    tokio::spawn(async move {
        println!("👂 正在监听 Operator 广播 (Topic: BROADCAST)...");
        loop {
            match sub_socket.recv().await {
                Ok(msg) => {
                    if let Some(payload) = msg.get(1) {
                        if let Ok(net_msg) = serde_json::from_slice::<NetworkMessage>(payload) {
                            match net_msg {
                                NetworkMessage::ChannelFinalized { channel_id_hex, participants } => {
                                    println!("\n📢 [Broadcast] 收到通道最终状态！");
                                    println!("    🆔 Channel ID: {}", channel_id_hex);
                                    
                                    let mut state = state_for_sub.lock().unwrap();
                                    state.participant_order.clear(); 
                                    
                                    for (name, pk_hex, comm_hex, balance) in participants {
                                        state.participant_order.push(name.clone());
                                        state.all_balances.insert(name.clone(), balance);
                                        if let (Ok(pk), Ok(comm)) = (G2::from_hex(&pk_hex), G1::from_hex(&comm_hex)) {
                                            state.all_pks.insert(name.clone(), pk);
                                            state.all_commitments.insert(name, comm);
                                        }
                                    }
                                    println!("✅ ST_0 状态已同步 (余额: {:?})", state.all_balances);
                                },
                                NetworkMessage::ConsensusReached { 
                                    tx_id, sender_name, sender_new_comm_hex, 
                                    receiver_name, receiver_new_comm_hex, amount, .. 
                                } => {
                                    println!("\n📢 [Broadcast] Tx_{} 共识达成 (Final Commit)", tx_id);
                                    let mut state = state_for_sub.lock().unwrap();
                                    if let (Ok(c_s), Ok(c_r)) = (G1::from_hex(&sender_new_comm_hex), G1::from_hex(&receiver_new_comm_hex)) {
                                        state.all_commitments.insert(sender_name.clone(), c_s);
                                        state.all_commitments.insert(receiver_name.clone(), c_r);
                                        if let Some(bal) = state.all_balances.get_mut(&sender_name) { 
                                            *bal = bal.saturating_sub(amount); 
                                        }
                                        if let Some(bal) = state.all_balances.get_mut(&receiver_name) { 
                                            *bal += amount; 
                                        }
                                        state.finalized_txs.insert(tx_id);
                                        if tx_id > state.current_tx_id {
                                            state.current_tx_id = tx_id;
                                        }
                                        println!("✅ 本地 ST 已更新。当前余额: {:?}", state.all_balances);
                                    }
                                },
                                // [新增] 处理通道关闭通知，并打印最终余额
                                NetworkMessage::ChannelClosed { closer, final_tx_id, .. } => {
                                    println!("\n🛑 [Broadcast] !!! 通道已关闭 !!!");
                                    println!("    发起人: {}", closer);
                                    println!("    最终交易 TxID: {}", final_tx_id);
                                    
                                    let state = state_for_sub.lock().unwrap();
                                    println!("💰 [Final] 最终结算状态:");
                                    println!("    我的余额: {}", state.balance);
                                    println!("    全网视图: {:?}", state.all_balances);
                                    
                                    println!("    ⚠️  程序将自动退出...");
                                    std::process::exit(0);
                                },
                                _ => {}
                            }
                        }
                    }
                },
                Err(e) => eprintln!("❌ 广播接收错误: {}", e),
            }
        }
    });

    // Task 2: P2P 监听
    let p2p_server_addr = format!("0.0.0.0:{}", my_port);
    let pp_server = pp.clone();
    let state_server = local_state.clone(); 
    let long_term_sk = my_long_term_keys.sk.clone(); 

    tokio::spawn(async move {
        let listener = TcpListener::bind(&p2p_server_addr).await.expect("Bind failed");
        loop {
            let (mut socket, _) = match listener.accept().await { Ok(v) => v, Err(_) => continue };
            let pp_ref = pp_server.clone();
            let state_ref = state_server.clone();
            let sk_ref = long_term_sk.clone(); 
            
            tokio::spawn(async move {
                let mut buf = Vec::new();
                if socket.read_to_end(&mut buf).await.is_ok() {
                    if let Ok(msg) = serde_json::from_slice::<NetworkMessage>(&buf) {
                        match msg {
                            NetworkMessage::P2PUpdateReq { 
                                tx_id, amount, range_proof_b64, comm_value_b64, commitment_hex, 
                                blinded_tx_point_hex 
                            } => {
                                println!("\n📩 [P2P Receiver] 收到转账请求 Tx_{} (Amount: {})", tx_id, amount);
                                let is_valid = range_proof::verify_proof(&range_proof_b64, &comm_value_b64, 0, &pp_ref);
                                if !is_valid {
                                    println!("❌ P2P 验证失败: Range Proof 无效");
                                } else {
                                    let c_m = G1::from_hex(&commitment_hex).unwrap();
                                    let new_comm_hex: String;
                                    let ephemeral_pk_hex: String;
                                    let blinded_sig_hex: String;

                                    {
                                        let mut state = state_ref.lock().unwrap();
                                        let new_comm = commitment::homomorphic_add(state.comm, c_m);
                                        state.balance += amount; 
                                        state.comm = new_comm;
                                        state.current_tx_id = tx_id; 
                                        println!("    💰 余额更新: -> {}", state.balance);
                                        new_comm_hex = new_comm.to_hex();
                                        
                                        let ekp = KeyPair::generate(&pp_ref);
                                        ephemeral_pk_hex = ekp.pk.to_hex();
                                        let blinded_point = G1::from_hex(&blinded_tx_point_hex).unwrap();
                                        let blinded_sig = blind::sign_blinded(ekp.sk, blinded_point);
                                        blinded_sig_hex = blinded_sig.to_hex();
                                    }

                                    let resp = NetworkMessage::P2PUpdateResp {
                                        tx_id, status: "OK".to_string(),
                                        ephemeral_pk_hex: Some(ephemeral_pk_hex),
                                        blinded_signature_hex: Some(blinded_sig_hex),
                                        receiver_new_comm_hex: Some(new_comm_hex),
                                    };
                                    let _ = socket.write_all(serde_json::to_string(&resp).unwrap().as_bytes()).await;
                                }
                            },
                            NetworkMessage::VoteRequest { 
                                tx_id, proposer_name, tx_amount_comm_hex, range_proof_b64, proof_comm_value_b64,
                                sender_new_comm_hex, sender_old_comm_hex, receiver_new_comm_hex, receiver_old_comm_hex,
                                prev_tx_id, prev_aggregated_sig_hex
                            } => {
                                println!("\n🗳️  [Voter] 收到 Operator 投票请求 Tx_{} (捎带 Commit Tx_{})", tx_id, prev_tx_id);
                                if let Some(agg_sig_hex) = prev_aggregated_sig_hex {
                                    if prev_tx_id > 0 {
                                        let prev_msg_opt = {
                                            let state = state_ref.lock().unwrap();
                                            if state.finalized_txs.contains(&prev_tx_id) { None } 
                                            else { state.tx_history_data.get(&prev_tx_id).cloned() }
                                        };
                                        if let Some(msg_bytes) = prev_msg_opt {
                                            let all_pks: Vec<G2> = {
                                                let state = state_ref.lock().unwrap();
                                                state.all_pks.values().cloned().collect()
                                            };
                                            if !all_pks.is_empty() {
                                                if let (Ok(agg_sig), Ok(agg_pk)) = (G1::from_hex(&agg_sig_hex), multisig::aggregate_public_keys(all_pks)) {
                                                    if multisig::verify_aggregate(agg_sig, agg_pk, &msg_bytes, &pp_ref) {
                                                        let mut state = state_ref.lock().unwrap();
                                                        state.finalized_txs.insert(prev_tx_id);
                                                        println!("    🔗 [Pipeline] 聚合签名验证通过! 正式 Commit Tx_{}", prev_tx_id);
                                                    } 
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Ok(proof_bytes) = general_purpose::STANDARD.decode(&range_proof_b64) {
                                    let mut state = state_ref.lock().unwrap();
                                    state.tx_history_data.insert(tx_id, proof_bytes);
                                }
                                
                                let mut vote_granted = false;
                                let state_matches = {
                                    let state_guard = state_ref.lock().unwrap();
                                    if let Some(local_old_comm) = state_guard.all_commitments.get(&proposer_name) {
                                        local_old_comm.to_hex() == sender_old_comm_hex
                                    } else {
                                        false
                                    }
                                };
                                if state_matches {
                                    if !range_proof::verify_proof(&range_proof_b64, &proof_comm_value_b64, 0, &pp_ref) {
                                        println!("    ❌ NIZK 验证失败");
                                    } else {
                                        if let (Ok(c_m), Ok(c_s_new), Ok(c_s_old), Ok(c_r_new), Ok(c_r_old)) = (
                                            G1::from_hex(&tx_amount_comm_hex),
                                            G1::from_hex(&sender_new_comm_hex), G1::from_hex(&sender_old_comm_hex),
                                            G1::from_hex(&receiver_new_comm_hex), G1::from_hex(&receiver_old_comm_hex)
                                        ) {
                                            let expected_s_new = commitment::homomorphic_sub(c_s_old, c_m);
                                            let expected_r_new = commitment::homomorphic_add(c_r_old, c_m);
                                            if c_s_new == expected_s_new && c_r_new == expected_r_new {
                                                println!("    ✅ 验证通过：NIZK 有效，状态转换合法");
                                                vote_granted = true;
                                            }
                                        }
                                    }
                                }
                                let resp = if vote_granted {
                                    let msg_bytes = general_purpose::STANDARD.decode(&range_proof_b64).unwrap_or_default();
                                    let sig = multisig::sign(sk_ref, &msg_bytes);
                                    NetworkMessage::VoteResponse {
                                        tx_id, voter_name: "me".to_string(), status: "OK".to_string(), sig_share_hex: Some(sig.to_hex()),
                                    }
                                } else {
                                    NetworkMessage::VoteResponse {
                                        tx_id, voter_name: "me".to_string(), status: "REJECT".to_string(), sig_share_hex: None
                                    }
                                };
                                let _ = socket.write_all(serde_json::to_string(&resp).unwrap().as_bytes()).await;
                            },
                            _ => {}
                        }
                    }
                }
            });
        }
    });

    // [Phase 2] 握手同步
    println!("\n==== [Phase 2] 向 Operator 同步状态 ====");
    {
        let proof_b64 = range_proof::generate_proof(deposit_amount, 0, &r_init, &pp).unwrap().0;
        let join_msg = NetworkMessage::JoinRequest {
            user_name: user_name.clone(),
            user_addr_str: format!("{}", my_config.address),
            pk_hex: my_long_term_keys.pk.to_hex(),
            initial_balance_comm_hex: initial_comm.to_hex(),
            initial_balance_proof_b64: proof_b64,
            initial_balance: deposit_amount, 
        };
        
        println!("    ⏳ 正在发送握手请求...");
        zmq_socket.send(ZmqMessage::from(serde_json::to_string(&join_msg)?)).await?;
        
        println!("    ⏳ 等待 Operator 响应...");
        match zmq_socket.recv().await {
            Ok(_) => println!("✅ [Sync] 已完成 Operator 握手，通道准备就绪"),
            Err(e) => {
                return Err(format!("❌ Operator 握手失败: {}. (请检查 Operator 是否运行)", e).into());
            }
        }
    }

    // CLI Loop
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    println!("\n✨ Ready. Cmd: pay <target> <amount> <tx_id> | balance | close");
    print!("> "); std::io::stdout().flush()?;

    let long_term_sk_for_close = my_long_term_keys.sk.clone();
    let channel_id_hex_for_close = channel_id_hex.clone();
    let channel_id_bytes_for_close = channel_id_bytes; 
    let user_name_clone = user_name.clone();

    while let Ok(Some(line)) = reader.next_line().await {
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.is_empty() { continue; }

        match parts[0] {
            "balance" => {
                let state = local_state.lock().unwrap();
                println!("=== 💰 账户状态 ===");
                println!("   我的可用余额 (Private): {}", state.balance);
                println!("   当前 Nonce: {}", state.current_tx_id);
                println!("   全网账本视图 (Public): {:?}", state.all_balances);
                println!("===================");
            },
            "close" => {
                // [修改点] 返回 tuple 为 4 个元素
                let (final_id, my_sig_b64, final_balances, my_final_balance) = {
                    let state = local_state.lock().unwrap();
                    let my_bal = state.balance; // 保存当前余额
                    println!("🛑 [Close] 正在请求关闭通道，提交最新 Nonce: {}", state.current_tx_id);
                    
                    let mut msg = Vec::new();
                    msg.extend_from_slice(&channel_id_bytes_for_close.0);
                    let nonce_u256 = U256::from(state.current_tx_id);
                    msg.extend_from_slice(nonce_u256.to_be_bytes::<32>().as_slice());
                    
                    for name in &state.participant_order {
                        if let Some(comm) = state.all_commitments.get(name) {
                            let comm_hex = comm.to_hex(); 
                            let comm_bytes = hex::decode(&comm_hex).unwrap();
                            msg.extend_from_slice(&comm_bytes);
                        }
                    }
                    println!("    ✍️  签署完整消息...");
                    let sig = multisig::sign(long_term_sk_for_close, &msg);
                    let sig_b64 = general_purpose::STANDARD.encode(hex::decode(sig.to_hex()).unwrap());
                    
                    // 返回 4 个值
                    (state.current_tx_id, sig_b64, state.all_balances.clone(), my_bal)
                };
                
                let close_req = NetworkMessage::CloseRequest {
                    user_name: user_name_clone.clone(),
                    channel_id_hex: channel_id_hex_for_close.clone(),
                    final_tx_id: final_id,
                    signature_b64: my_sig_b64,
                    final_balances,
                };
                
                zmq_socket.send(ZmqMessage::from(serde_json::to_string(&close_req)?)).await?;
                let raw = zmq_socket.recv().await?;
                let resp_str = String::from_utf8(raw.get(0).unwrap().to_vec())?;
                println!("    📩 {}", resp_str);
                
                if resp_str.contains("\"status\":\"OK\"") {
                    println!("🎉 通道关闭流程启动成功！");
                    // [新增] 主动退出打印余额
                    println!("💰 [Final] 我的最终余额: {}", my_final_balance);
                    println!("    程序即将退出...");
                    break;
                }
            },
            "pay" => {
                if parts.len() < 4 { println!("Usage: pay <target> <amount> <tx_id>"); }
                else {
                    let target_name = parts[1];
                    let amount: u64 = parts[2].parse().unwrap_or(10);
                    let tx_id: u64 = parts[3].parse().unwrap_or(1);

                    println!("🌊 [Sender] 正在向 {} 转账 {}...", target_name, amount);
                    
                    let (c_m_hex, proof_b64, comm_val_b64, my_new_comm_hex, blinded_point_hex, r_blind) = {
                        let mut state = local_state.lock().unwrap();
                        
                        if state.balance < amount {
                            println!("❌ 余额不足 (当前: {}, 需要: {})", state.balance, amount);
                            continue;
                        }

                        let r_m = Fr::random(); 
                        let c_m = commitment::commit(amount, r_m, &pp);
                        
                        let (proof, comm_val) = range_proof::generate_proof(
                            state.balance, amount, &state.r, &pp
                        ).expect("余额不足或证明失败");
                        
                        let new_comm = commitment::homomorphic_sub(state.comm, c_m);
                        state.balance -= amount;
                        state.comm = new_comm;
                        state.current_tx_id = tx_id; 
                        println!("    💰 本地余额预扣: -> {}", state.balance);

                        let msg_content_bytes = general_purpose::STANDARD.decode(&proof).unwrap();
                        let blinded_data = blind::blind(&msg_content_bytes);
                        
                        (c_m.to_hex(), proof, comm_val, new_comm.to_hex(), blinded_data.point.to_hex(), blinded_data.r)
                    };

                    if let (Some(host), Some(port)) = (config.get_user_host(target_name), config.get_user_port(target_name)) {
                        let p2p_req = NetworkMessage::P2PUpdateReq {
                            tx_id, amount, commitment_hex: c_m_hex.clone(), 
                            range_proof_b64: proof_b64.clone(), comm_value_b64: comm_val_b64.clone(),
                            blinded_tx_point_hex: blinded_point_hex,
                        };
                        
                        if let Ok(NetworkMessage::P2PUpdateResp { status, blinded_signature_hex, ephemeral_pk_hex, receiver_new_comm_hex, .. }) = send_p2p_msg(&host, port, &p2p_req).await {
                            if status == "OK" {
                                if let (Some(b_sig_hex), Some(peer_pk)) = (blinded_signature_hex, ephemeral_pk_hex) {
                                    println!("✅ [P2P] 收到 Bob 的盲签名，正在去盲...");
                                    
                                    let b_sig = G1::from_hex(&b_sig_hex).unwrap();
                                    let real_sig = blind::unblind(b_sig, r_blind);
                                    let real_sig_b64 = general_purpose::STANDARD.encode(hex::decode(real_sig.to_hex()).unwrap());

                                    let my_e_kp = KeyPair::generate(&pp_clone_for_cli);
                                    let proof_bytes = general_purpose::STANDARD.decode(&proof_b64).unwrap();
                                    let my_sig = multisig::sign(my_e_kp.sk, &proof_bytes);
                                    let my_sig_b64 = general_purpose::STANDARD.encode(hex::decode(my_sig.to_hex()).unwrap());
                                    let prev_tx_id = if tx_id > 1 { tx_id - 1 } else { 0 };

                                    let proposal = NetworkMessage::UpdateProposal {
                                        user_name: user_name.clone(),
                                        counterparty_name: target_name.to_string(),
                                        tx_id,
                                        prev_tx_id, 
                                        amount, 
                                        tx_amount_comm_hex: c_m_hex,
                                        range_proof_b64: proof_b64,
                                        proof_comm_value_b64: comm_val_b64,
                                        sender_new_comm_hex: my_new_comm_hex,
                                        receiver_new_comm_hex: receiver_new_comm_hex.unwrap(),
                                        proposer_ephemeral_pk_hex: my_e_kp.pk.to_hex(),
                                        proposer_signature_b64: my_sig_b64,
                                        counterparty_ephemeral_pk_hex: Some(peer_pk),
                                        counterparty_signature_b64: Some(real_sig_b64),
                                    };
                                    zmq_socket.send(ZmqMessage::from(serde_json::to_string(&proposal)?)).await?;
                                    let _ = zmq_socket.recv().await?; 
                                    println!("✅ 提案已提交给 Operator");
                                }
                            }
                        }
                    }
                }
            },
            _ => println!("❌ 未知命令"),
        }
        print!("> "); std::io::stdout().flush()?;
    }
    Ok(())
}