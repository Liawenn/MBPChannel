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
// 1. 合约接口定义 (保持不变)
// =========================================================================
sol! {
    #[sol(rpc)]
    contract MBPChannel {
        function createChannel(bytes32 channelId) external payable;
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
        function withdraw() external;
        
        function channels(bytes32 channelId) external view returns (
            address leader,
            uint256 totalBalance,
            uint8 status,
            uint256 closingTime,
            uint256 bestNonce
        );
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

    // 仅使用 wallet，不阻塞，不自动填充复杂 Gas 策略，追求最快构造速度
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .on_http(url);

    Ok(MBPChannel::new(contract_addr, provider))
}

// =========================================================================
// 3. 业务逻辑 (极速版：只发送，不等待)
// =========================================================================

pub async fn lock_deposit(
    _actor: &ActorConfig, 
    _rpc_url: &str, 
    _contract_address: Address,
    _amount_wei: u128 
) -> Result<(), Box<dyn Error>> {
    Ok(())
}

// 2. 创建通道
pub async fn create_channel(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>,
    initial_deposit: u128 
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播创建交易 (不等待挖矿)...");
    
    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;
    let amount_u256 = U256::from(initial_deposit);

    // [Async] 仅发送到 Mempool
    let pending_tx = contract
        .createChannel(channel_id)
        .value(amount_u256)
        .send()
        .await?; // 这里只等待 RPC 确认收到交易

    let tx_hash = pending_tx.tx_hash();
    println!("    📡 交易已广播! Hash: {}", tx_hash);
    
    Ok(tx_hash.to_string())
}

// 3. 加入通道
pub async fn join_channel(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>,
    amount_wei: u128 
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播加入交易...");

    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;
    let amount_u256 = U256::from(amount_wei);

    let pending_tx = contract
        .joinChannel(channel_id)
        .value(amount_u256)
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash();
    println!("    📡 加入请求已广播! Hash: {}", tx_hash);
    
    Ok(tx_hash.to_string())
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

    let tx_hash = pending_tx.tx_hash();
    println!("    📡 关闭请求已广播! Hash: {}", tx_hash);

    Ok(tx_hash.to_string())
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

    let tx_hash = pending_tx.tx_hash();
    println!("    ⚔️  争议交易已广播! Hash: {}", tx_hash);

    Ok(tx_hash.to_string())
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

    let tx_hash = pending_tx.tx_hash();
    println!("    📡 结算请求已广播! Hash: {}", tx_hash);

    Ok(tx_hash.to_string())
}

// 7. 提现
pub async fn withdraw_funds(
    actor: &ActorConfig,
    rpc_url: &str,
    contract_addr: Address
) -> Result<String, Box<dyn Error>> {
    println!("    🚀 [Chain] 正在广播提现请求...");

    let contract = get_contract_instance(actor, rpc_url, contract_addr).await?;

    let pending_tx = contract
        .withdraw()
        .send()
        .await?;

    let tx_hash = pending_tx.tx_hash();
    println!("    📡 提现请求已广播! Hash: {}", tx_hash);

    Ok(tx_hash.to_string())
}

// 8. 检查通道 (View 函数，本身就是只读不等待挖矿，但需要等待节点返回数据)
// ⚠️ 注意：如果你刚发完 create_channel 就调这个，因为不等待挖矿，
// 这个函数可能会告诉你通道还不存在 (NOT OPEN)，这是预期行为。
pub async fn check_channel_ready(
    rpc_url: &str,
    contract_addr: Address,
    channel_id: FixedBytes<32>
) -> Result<bool, Box<dyn Error>> {
    let url = Url::parse(rpc_url)?;
    let provider = ProviderBuilder::new().on_http(url);
    let contract = MBPChannel::new(contract_addr, provider);

    // Call 只是查询本地节点状态，不发交易，速度也很快
    // 但在不等待挖矿模式下，查询到的可能是旧状态
    let result = contract.channels(channel_id).call().await?;
    let is_open = result.status == 0; 
    
    if is_open {
        println!("    🔍 [View] 链上状态: OPEN (Balance: {})", result.totalBalance);
    } else {
        println!("    🔍 [View] 链上状态: Status Code {} (可能尚未挖矿)", result.status);
    }

    Ok(is_open)
}