use crate::config::ActorConfig;
use alloy::primitives::{Address, FixedBytes, U256}; 
use std::error::Error;
use uuid::Uuid;
use hex;

fn mock_tx_hash() -> String {
    format!("0x{}", hex::encode(Uuid::new_v4().as_bytes()))
}

// 1. 锁币/存款
pub async fn lock_deposit(
    actor: &ActorConfig, 
    _rpc_url: &str, 
    _contract_address: Address,
    amount_wei: u128 
) -> Result<(), Box<dyn Error>> {
    println!("    🎭 [MockChain] 存款: 用户={} 金额={} (模拟成功)", actor.name, amount_wei);
    Ok(())
}

// 2. 创建通道
pub async fn create_channel(
    _actor: &ActorConfig,
    _rpc_url: &str,
    _contract_address: Address,
    channel_id: FixedBytes<32>,
    initial_deposit: u128 
) -> Result<String, Box<dyn Error>> {
    println!("    🎭 [MockChain] createChannel: ID={} Deposit={} (模拟成功)", channel_id, initial_deposit);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Ok(mock_tx_hash())
}

// 3. 加入通道
pub async fn join_channel(
    actor: &ActorConfig,
    _rpc_url: &str,
    _contract_addr: Address,
    _channel_id: FixedBytes<32>,
    amount_wei: u128 
) -> Result<String, Box<dyn Error>> {
    println!("    🎭 [MockChain] joinChannel: 用户={} Deposit={} (模拟成功)", actor.name, amount_wei);
    Ok(mock_tx_hash())
}

// 4. [修改] 发起关闭 (initiateClose)
// 增加了 nonce, recipients, amounts, signature
pub async fn initiate_close(
    actor: &ActorConfig,
    _rpc_url: &str,
    _contract_addr: Address,
    _channel_id: FixedBytes<32>,
    nonce: u64,               // [新增] 对应合约的 nonce
    recipients: Vec<Address>, // [新增]
    amounts: Vec<U256>,       // [新增]
    _signature: Vec<u8>       
) -> Result<String, Box<dyn Error>> {
    println!("    🎭 [MockChain] initiateClose: 发起者={} Nonce={} 接收方数量={} (模拟成功)", 
        actor.name, nonce, recipients.len());
    
    // 简单的模拟校验
    if recipients.len() != amounts.len() {
        return Err("Mock Error: Recipients and amounts length mismatch".into());
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Ok(mock_tx_hash())
}

// 5. [新增] 提交争议 (disputeClose)
// 对应合约的 disputeClose
pub async fn dispute_close(
    actor: &ActorConfig,
    _rpc_url: &str,
    _contract_addr: Address,
    _channel_id: FixedBytes<32>,
    nonce: u64,
    recipients: Vec<Address>,
    amounts: Vec<U256>,
    _signature: Vec<u8>
) -> Result<String, Box<dyn Error>> {
    println!("    ⚔️ [MockChain] disputeClose: 挑战者={} NewNonce={} (欺诈证明提交成功)", actor.name, nonce);
    println!("       -> 状态已回滚至 Nonce {}", nonce);
    
    if recipients.len() != amounts.len() {
        return Err("Mock Error: Data mismatch".into());
    }

    Ok(mock_tx_hash())
}

// 6. 最终结算
pub async fn finalize_close(
    actor: &ActorConfig,
    _rpc_url: &str,
    _contract_addr: Address,
    _channel_id: FixedBytes<32>
) -> Result<String, Box<dyn Error>> {
    println!("    🎭 [MockChain] finalizeClose: 执行者={} (模拟成功)", actor.name);
    Ok(mock_tx_hash())
}

// 7. 提现
pub async fn withdraw_funds(
    actor: &ActorConfig,
    _rpc_url: &str,
    _contract_addr: Address
) -> Result<String, Box<dyn Error>> {
    println!("    🎭 [MockChain] withdraw: 用户={} (模拟成功)", actor.name);
    Ok(mock_tx_hash())
}

// 8. 检查通道
pub async fn check_channel_ready(
    _rpc_url: &str,
    _contract_addr: Address,
    _channel_id: FixedBytes<32>
) -> Result<bool, Box<dyn Error>> {
    Ok(true)
}