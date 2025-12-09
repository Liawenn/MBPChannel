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

struct LocalState {
    balance: u64,
    r: Fr,
    comm: G1,
    current_tx_id: u64, 
    all_pks: HashMap<String, G2>,
    all_commitments: HashMap<String, G1>,
    
    // 记录已 Finalized 的交易 ID
    finalized_txs: HashSet<u64>,

    // [新增] 缓存交易的历史数据 (Range Proof Bytes)，用于验证聚合签名
    // Key: TxID, Value: Message Bytes (Range Proof)
    tx_history_data: HashMap<u64, Vec<u8>>,
}

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
    
    let local_state = Arc::new(Mutex::new(LocalState {
        balance: deposit_amount,
        r: r_init,
        comm: initial_comm,
        current_tx_id: 0, 
        all_pks: HashMap::new(),
        all_commitments: HashMap::new(),
        finalized_txs: HashSet::from([0]), // Genesis Tx 0 is finalized
        tx_history_data: HashMap::new(),     // 初始化历史数据缓存
    }));

    let op_host = config.operator.host.clone().unwrap_or("127.0.0.1".to_string());
    let op_rep_port = config.operator.port.unwrap_or(5555);
    
    // 1. REQ Socket (用于主动请求 Operator)
    let mut zmq_socket = ReqSocket::new();
    zmq_socket.connect(&format!("tcp://{}:{}", op_host, op_rep_port)).await?;

    // 2. SUB Socket (用于接收广播)
    let op_pub_port = op_rep_port + 1; // 默认 5556
    let mut sub_socket = SubSocket::new();
    sub_socket.connect(&format!("tcp://{}:{}", op_host, op_pub_port)).await?;
    sub_socket.subscribe("BROADCAST").await?; // 订阅 Topic
    
    // 3. 启动后台广播监听任务
    let state_for_sub = local_state.clone(); 
    
    tokio::spawn(async move {
        println!("👂 正在监听 Operator 广播 (Topic: BROADCAST)...");
        loop {
            match sub_socket.recv().await {
                Ok(msg) => {
                    if let Some(payload) = msg.get(1) {
                        if let Ok(net_msg) = serde_json::from_slice::<NetworkMessage>(payload) {
                            match net_msg {
                                // 处理 Create 阶段的 ST_0
                                NetworkMessage::ChannelFinalized { channel_id_hex, participants } => {
                                    println!("\n📢 [Broadcast] 收到通道最终状态！");
                                    println!("    🆔 Channel ID: {}", channel_id_hex);
                                    
                                    let mut state = state_for_sub.lock().unwrap();
                                    for (name, pk_hex, comm_hex) in participants {
                                        if let (Ok(pk), Ok(comm)) = (G2::from_hex(&pk_hex), G1::from_hex(&comm_hex)) {
                                            state.all_pks.insert(name.clone(), pk);
                                            state.all_commitments.insert(name, comm);
                                        }
                                    }
                                    println!("✅ ST_0 状态已同步到本地存储。\n");
                                },

                                // 处理 Update 阶段的共识结果 (更新本地全局视图)
                                // 注意：在流水线模式下，这个消息主要用于最后一笔交易或超时补发
                                NetworkMessage::ConsensusReached { 
                                    tx_id, sender_name, sender_new_comm_hex, 
                                    receiver_name, receiver_new_comm_hex, .. 
                                } => {
                                    println!("\n📢 [Broadcast] Tx_{} 共识达成 (Final Commit)，正在更新全网视图...", tx_id);
                                    let mut state = state_for_sub.lock().unwrap();
                                    
                                    if let (Ok(c_s), Ok(c_r)) = (G1::from_hex(&sender_new_comm_hex), G1::from_hex(&receiver_new_comm_hex)) {
                                        state.all_commitments.insert(sender_name.clone(), c_s);
                                        state.all_commitments.insert(receiver_name.clone(), c_r);
                                        state.finalized_txs.insert(tx_id);
                                        println!("✅ 本地 ST 已更新 ({} & {})", sender_name, receiver_name);
                                    }
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

    // --- Phase 1: P2P Listener (Receiver & Voter) ---
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
                            // [P2P Update] 接收方处理
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
                                        // 接收方同态加法: C'(B) = C(B) + C(m)
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

                            // ==========================================
                            // [Vote Request] 投票方处理 (Pipeline Consensus)
                            // ==========================================
                            NetworkMessage::VoteRequest { 
                                tx_id, proposer_name, tx_amount_comm_hex, range_proof_b64, proof_comm_value_b64,
                                sender_new_comm_hex, sender_old_comm_hex, receiver_new_comm_hex, receiver_old_comm_hex,
                                prev_tx_id, prev_aggregated_sig_hex // [Pipeline] 新增字段
                            } => {
                                println!("\n🗳️  [Voter] 收到 Operator 投票请求 Tx_{} (捎带 Commit Tx_{})", tx_id, prev_tx_id);
                                
                                // --- Step 1: 处理 Pipeline 捎带确认 (Commit Phase) ---
                                // 用户必须验证 Operator 发来的聚合签名，确认上一笔交易确实达成了全网共识
                                if let Some(agg_sig_hex) = prev_aggregated_sig_hex {
                                    if prev_tx_id > 0 {
                                        // 1. 获取上一笔交易的原始消息（Range Proof Bytes）
                                        let prev_msg_opt = {
                                            let state = state_ref.lock().unwrap();
                                            if state.finalized_txs.contains(&prev_tx_id) {
                                                None // 已经 Finalized 过了，无需重复验证
                                            } else {
                                                state.tx_history_data.get(&prev_tx_id).cloned()
                                            }
                                        };

                                        if let Some(msg_bytes) = prev_msg_opt {
                                            // 2. 准备聚合公钥 (cPK = sum(pk_i))
                                            let all_pks: Vec<G2> = {
                                                let state = state_ref.lock().unwrap();
                                                state.all_pks.values().cloned().collect()
                                            };
                                            
                                            // 3. 执行密码学验证
                                            // verify(sigma, cpk, msg)
                                            let mut verified = false;
                                            if !all_pks.is_empty() {
                                                if let (Ok(agg_sig), Ok(agg_pk)) = (G1::from_hex(&agg_sig_hex), multisig::aggregate_public_keys(all_pks)) {
                                                    if multisig::verify_aggregate(agg_sig, agg_pk, &msg_bytes, &pp_ref) {
                                                        verified = true;
                                                    }
                                                }
                                            }

                                            if verified {
                                                let mut state = state_ref.lock().unwrap();
                                                state.finalized_txs.insert(prev_tx_id);
                                                println!("    🔗 [Pipeline] 聚合签名验证通过! 正式 Commit Tx_{}", prev_tx_id);
                                            } else {
                                                println!("    ❌ [Critical] Tx_{} 聚合签名验证失败！Operator 可能作恶！", prev_tx_id);
                                                // 在生产环境中，这里应该触发 Alert 或 View-Change
                                            }
                                        }
                                    }
                                }

                                // --- Step 2: 处理当前交易验证 (Prepare Phase) ---
                                // 1. 先将当前交易的 Proof 缓存，以便下一轮验证使用
                                if let Ok(proof_bytes) = general_purpose::STANDARD.decode(&range_proof_b64) {
                                    let mut state = state_ref.lock().unwrap();
                                    state.tx_history_data.insert(tx_id, proof_bytes);
                                }

                                let mut vote_granted = false;
                                let mut validation_failed_reason = "";

                                // 2. [核心安全校验] 检查 Sender 的旧状态是否与本地记录一致
                                let state_matches = {
                                    let state_guard = state_ref.lock().unwrap();
                                    if let Some(local_old_comm) = state_guard.all_commitments.get(&proposer_name) {
                                        if local_old_comm.to_hex() == sender_old_comm_hex {
                                            true
                                        } else {
                                            println!("    ❌ [Security] 旧状态不匹配! Local: {}... vs Req: {}...", 
                                                &local_old_comm.to_hex()[0..6], &sender_old_comm_hex[0..6]);
                                            false
                                        }
                                    } else {
                                        println!("    ⚠️ 未找到 Proposer 的本地记录，可能是新加入节点");
                                        false
                                    }
                                };

                                if !state_matches {
                                    validation_failed_reason = "State Mismatch";
                                } else if !range_proof::verify_proof(&range_proof_b64, &proof_comm_value_b64, 0, &pp_ref) {
                                    validation_failed_reason = "Invalid NIZK";
                                    println!("    ❌ NIZK 验证失败");
                                } else {
                                    // 3. 验证同态状态转换
                                    if let (Ok(c_m), Ok(c_s_new), Ok(c_s_old), Ok(c_r_new), Ok(c_r_old)) = (
                                        G1::from_hex(&tx_amount_comm_hex),
                                        G1::from_hex(&sender_new_comm_hex), G1::from_hex(&sender_old_comm_hex),
                                        G1::from_hex(&receiver_new_comm_hex), G1::from_hex(&receiver_old_comm_hex)
                                    ) {
                                        let expected_s_new = commitment::homomorphic_sub(c_s_old, c_m);
                                        let expected_r_new = commitment::homomorphic_add(c_r_old, c_m);
                                        
                                        if c_s_new == expected_s_new && c_r_new == expected_r_new {
                                            println!("    ✅ 验证通过：NIZK 有效，状态转换合法，旧状态匹配");
                                            vote_granted = true;
                                        } else {
                                            validation_failed_reason = "Homomorphic Mismatch";
                                            println!("    ❌ 验证失败：状态转换计算不成立");
                                        }
                                    } else {
                                        validation_failed_reason = "Parse Error";
                                    }
                                }

                                // 4. 发送投票响应
                                let resp = if vote_granted {
                                    let msg_bytes = general_purpose::STANDARD.decode(&range_proof_b64).unwrap_or_default();
                                    let sig = multisig::sign(sk_ref, &msg_bytes);
                                    
                                    NetworkMessage::VoteResponse {
                                        tx_id,
                                        voter_name: "me".to_string(),
                                        status: "OK".to_string(),
                                        sig_share_hex: Some(sig.to_hex()),
                                    }
                                } else {
                                    NetworkMessage::VoteResponse {
                                        tx_id, 
                                        voter_name: "me".to_string(), 
                                        status: "REJECT".to_string(), 
                                        sig_share_hex: None
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

    // --- Phase 2: Create (发送 Join) ---
    {
        let proof_b64 = range_proof::generate_proof(deposit_amount, 0, &r_init, &pp).unwrap().0;
        let join_msg = NetworkMessage::JoinRequest {
            user_name: user_name.clone(),
            user_addr_str: format!("{}", my_config.address),
            pk_hex: my_long_term_keys.pk.to_hex(),
            initial_balance_comm_hex: initial_comm.to_hex(),
            initial_balance_proof_b64: proof_b64,
        };
        zmq_socket.send(ZmqMessage::from(serde_json::to_string(&join_msg)?)).await?;
        let _ = zmq_socket.recv().await?;
        println!("✅ [Join] 已加入通道");
    }

    // --- CLI Sender Logic ---
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    println!("\n✨ Ready. Cmd: pay <target> <tx_id> | close");
    print!("> "); std::io::stdout().flush()?;

    while let Ok(Some(line)) = reader.next_line().await {
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.is_empty() { continue; }

        match parts[0] {
            "close" => {
                let (final_id, my_sig_b64) = {
                    let state = local_state.lock().unwrap();
                    println!("🛑 [Close] 正在请求关闭通道，提交最新 Nonce: {}", state.current_tx_id);
                    let msg = b"CLOSE_REQUEST"; 
                    let my_keys = KeyPair::generate(&pp_clone_for_cli); 
                    let sig = multisig::sign(my_keys.sk, msg);
                    let sig_b64 = general_purpose::STANDARD.encode(hex::decode(sig.to_hex()).unwrap());
                    (state.current_tx_id, sig_b64)
                };
                let close_req = NetworkMessage::CloseRequest {
                    user_name: user_name.clone(),
                    channel_id_hex: "mock_id".to_string(), 
                    final_tx_id: final_id,
                    signature_b64: my_sig_b64,
                };
                zmq_socket.send(ZmqMessage::from(serde_json::to_string(&close_req)?)).await?;
                let raw = zmq_socket.recv().await?;
                let resp_str = String::from_utf8(raw.get(0).unwrap().to_vec())?;
                if resp_str.contains("OK") {
                    println!("✅ 通道关闭成功！最终结算 TxID: {}", final_id);
                    break; 
                } else {
                    println!("❌ 关闭请求被拒绝: {}", resp_str);
                }
            },

            "pay" => {
                if parts.len() < 3 { println!("Usage: pay <target> <tx_id>"); }
                else {
                    let target_name = parts[1];
                    let tx_id: u64 = parts[2].parse().unwrap_or(1);
                    let amount = 10u64; 

                    println!("🌊 [Sender] 正在向 {} 转账 {}...", target_name, amount);
                    
                    // Sender Update-StateGen
                    let (c_m_hex, proof_b64, comm_val_b64, my_new_comm_hex, blinded_point_hex, r_blind) = {
                        let mut state = local_state.lock().unwrap();
                        let r_m = Fr::random(); 
                        let c_m = commitment::commit(amount, r_m, &pp);
                        
                        let (proof, comm_val) = range_proof::generate_proof(
                            state.balance, amount, &state.r, &pp
                        ).expect("余额不足或证明失败");
                        
                        let new_comm = commitment::homomorphic_sub(state.comm, c_m);
                        state.balance -= amount;
                        state.comm = new_comm;
                        state.current_tx_id = tx_id; 
                        println!("    💰 余额更新: -> {}", state.balance);

                        let msg_content_bytes = general_purpose::STANDARD.decode(&proof).unwrap();
                        let blinded_data = blind::blind(&msg_content_bytes);
                        
                        (c_m.to_hex(), proof, comm_val, new_comm.to_hex(), blinded_data.point.to_hex(), blinded_data.r)
                    };

                    if let (Some(host), Some(port)) = (config.get_user_host(target_name), config.get_user_port(target_name)) {
                        let p2p_req = NetworkMessage::P2PUpdateReq {
                            tx_id, amount, 
                            commitment_hex: c_m_hex.clone(), 
                            range_proof_b64: proof_b64.clone(),
                            comm_value_b64: comm_val_b64.clone(),
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

                                    // 检查上一笔交易 ID
                                    let prev_tx_id = if tx_id > 1 { tx_id - 1 } else { 0 };

                                    let proposal = NetworkMessage::UpdateProposal {
                                        user_name: user_name.clone(),
                                        counterparty_name: target_name.to_string(),
                                        tx_id,
                                        prev_tx_id, // [Pipeline] 指向上一个 ID
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