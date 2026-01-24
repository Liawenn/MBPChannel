use crate::config::ActorConfig;
use alloy::{
    network::EthereumWallet,
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
    primitives::{Address, FixedBytes, U256, Bytes},
    transports::http::reqwest::Url,
};
use std::error::Error;
use std::str::FromStr;

// =========================================================================
// 1. 合约接口定义 (ABI 已更新以匹配新合约)
// =========================================================================
sol! {
    #[sol(rpc)]
    contract MBPChannel {
        // [修改] 对应 Solidity 的 deposit(bytes32 channelId)
        function deposit(bytes32 channelId) external payable;
        
        // [修改] Solidity 只有 channelId 参数，金额通过 value 传递
        function createChannel(bytes32 channelId) external payable;
        
        // [修改] Solidity 只有 channelId 参数
        function joinChannel(bytes32 channelId) external payable;
        
        function initiateClose(
            bytes32 channelId,
            uint256 nonce,
            address[] calldata recipients,
            uint256[] calldata amounts,
            bytes calldata signature
        ) external;

        function disputeClose(
            bytes32 channelId,
            uint256 nonce,
            address[] calldata recipients,
            uint256[] calldata amounts,
            bytes calldata signature
        ) external;

        function finalizeClose(bytes32 channelId) external;
        
        function withdraw() external; // [修改] withdraw 通常不需要参数，或者根据你的合约看，如果是 withdraw(amount) 则保持原样
        
        function channels(bytes32 channelId) external view returns (
            address leader,
            uint256 totalBalance,
            uint8 status,
            uint256 closingTime,
            uint256 bestNonce
        );
        
        function pendingWithdrawals(address user) external view returns (uint256);
    }
}

// =========================================================================
// 2. 辅助函数
// =========================================================================
async fn get_contract_instance(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
) -> Result<MBPChannel::MBPChannelInstance<impl Provider>, Box<dyn Error>> {
    let signer: PrivateKeySigner = actor.private_key.parse().map_err(|_| "Invalid private key")?;
    let wallet = EthereumWallet::from(signer);
    let url = Url::parse(rpc_url).map_err(|_| "Invalid RPC URL")?;

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .on_http(url);

    Ok(MBPChannel::new(contract_addr, provider))
}

// =========================================================================
// 3. 业务逻辑
// =========================================================================

// 1. 充值/锁仓 (Step 1)
// [修改] 增加了 channel_id 参数，因为 deposit 需要它
pub async fn lock_deposit(
    actor: &ActorConfig, 
    rpc_url: &str, 
    contract_address: Address,
    channel_id: FixedBytes<32>, 
    amount_wei: u128 
) -> Result<String, Box<dyn Error>> {
    println!("    💰 [Chain] 正在调用 deposit (Lock Deposit)...");
    
    let contract = get_contract_instance(actor, rpc_url, contract_address).await?;
    let amount_u256 = U256::from(amount_wei);

    // [关键] 调用 deposit，并附带 ETH
    let pending_tx = contract
        .deposit(channel_id)
        .value(amount_u256) 
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash().to_string();
    println!("    📡 充值已广播: {} (等待确认...)", tx_hash);

    let receipt = pending_tx.get_receipt().await?;
    
    if receipt.status() {
        println!("    ✅ 充值交易已确认!");
        Ok(tx_hash)
    } else {
        Err(format!("❌ 交易失败 (Reverted): {}", tx_hash).into())
    }
}

// 2. 创建通道
pub async fn create_channel(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>,
    initial_deposit: u128 
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播创建通道交易...");
    
    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;
    let amount_u256 = U256::from(initial_deposit);

    // [修改] createChannel 参数仅为 ID，金额通过 .value() 传入
    let pending_tx = contract
        .createChannel(channel_id)
        .value(amount_u256)
        .send()
        .await?; 

    let tx_hash = pending_tx.tx_hash().to_string();
    println!("    📡 创建已广播: {} (等待确认...)", tx_hash);

    let receipt = pending_tx.get_receipt().await?;

    if receipt.status() {
        println!("    ✅ 通道创建已确认!");
        Ok(tx_hash)
    } else {
        Err(format!("❌ 创建失败 (Reverted): {}", tx_hash).into())
    }
}

// 3. 加入通道 (Step 2)
// [修改] 移除了 amount 参数，因为这里不应该再发钱了
pub async fn join_channel(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播加入交易 (仅注册身份)...");

    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;

    // [关键] value 设为 0，因为资金已经在 lock_deposit 步骤中进入了
    let pending_tx = contract
        .joinChannel(channel_id)
        .value(U256::ZERO)
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash().to_string();
    println!("    📡 加入已广播: {} (等待确认...)", tx_hash);

    let receipt = pending_tx.get_receipt().await?;
    
    if receipt.status() {
        println!("    ✅ 加入通道已确认!");
        Ok(tx_hash)
    } else {
        Err(format!("❌ 加入失败 (Reverted): {}", tx_hash).into())
    }
}

// 4. 发起关闭
pub async fn initiate_close(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>,
    nonce: u64,
    recipients: Vec<Address>,
    amounts: Vec<U256>,
    signature: Vec<u8>       
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播关闭请求 (Nonce: {})...", nonce);

    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;
    let nonce_u256 = U256::from(nonce);
    let sig_bytes = Bytes::from(signature);

    let pending_tx = contract
        .initiateClose(channel_id, nonce_u256, recipients, amounts, sig_bytes)
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash().to_string();
    println!("    📡 关闭已广播: {}", tx_hash);

    let _ = pending_tx.get_receipt().await?;
    println!("    ✅ 关闭请求已确认!");

    Ok(tx_hash)
}

// 5. 提交争议
pub async fn dispute_close(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>,
    nonce: u64,
    recipients: Vec<Address>,
    amounts: Vec<U256>,
    signature: Vec<u8>
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播欺诈证明 (Nonce: {})...", nonce);

    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;
    let nonce_u256 = U256::from(nonce);
    let sig_bytes = Bytes::from(signature);

    let pending_tx = contract
        .disputeClose(channel_id, nonce_u256, recipients, amounts, sig_bytes)
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash().to_string();
    println!("    ⚔️  争议交易已广播: {}", tx_hash);
    let _ = pending_tx.get_receipt().await?;

    Ok(tx_hash)
}

// 6. 最终结算
pub async fn finalize_close(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播结算请求...");

    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;

    let pending_tx = contract
        .finalizeClose(channel_id)
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash().to_string();
    println!("    📡 结算请求已广播: {}", tx_hash);
    let _ = pending_tx.get_receipt().await?;
    println!("    ✅ 结算已完成!");

    Ok(tx_hash)
}

// 7. 提现
// [注意] 这里我把 withdraw 改为了无参数调用，如果你的合约 withdraw() 不需要参数请用这个
// 如果合约是 withdraw(uint256 amount)，请把下面的注释代码恢复
pub async fn withdraw_funds(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    // amount_wei: u128 // 如果全额提现可能不需要 amount
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播提现请求...");

    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;
    
    // 假设合约是 withdraw() 提取所有 pending
    let pending_tx = contract
        .withdraw() 
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash().to_string();
    println!("    📡 提现请求已广播: {}", tx_hash);
    let _ = pending_tx.get_receipt().await?;
    println!("    ✅ 提现已确认!");

    Ok(tx_hash)
}

// 8. 检查通道
pub async fn check_channel_ready(
    actor: &ActorConfig, // 增加 actor 参数以便复用 get_contract_instance
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>
) -> Result<bool, Box<dyn Error>> {
    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;

    let result = contract.channels(channel_id).call().await?;
    let is_open = result.status == 0; // 0 = OPEN (Enum)
    
    if is_open {
        println!("    🔍 [View] 链上状态: OPEN (Total Balance: {})", result.totalBalance);
    } else {
        println!("    🔍 [View] 链上状态: Status Code {} (可能尚未挖矿或未创建)", result.status);
    }

    Ok(is_open)
}